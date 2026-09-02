//! `rv64-host --kernel <path> --dtb <path> --mem <path> [--initrd <path>]`
//!
//! Boots a riscv64 Linux kernel image under this emulator's SBI stub: loads
//! `--kernel` (either a raw `Image` or an ELF — see
//! `rv64_host::load_kernel`), places `--dtb` and any `--initrd` above the
//! kernel's *memory* footprint (`rv64_host::boot_layout` — not above its
//! file length, which would land them in the kernel's `.bss`), records the
//! initrd's address in the device tree, starts the CPU in S-mode at the
//! kernel's load address `0x8020_0000` with `a0 = 0`
//! (hartid) and `a1 = <dtb guest address>`, and runs until the guest issues
//! an SBI shutdown or `--max-insns` is exhausted. On exit it prints the
//! page-cache and MMU-walk counters to stderr — the numbers that decide
//! whether this stack is viable on the badge.
//!
//! `main.rs` is intentionally thin: argument parsing and wiring only. Both
//! halves of the work it appears to do live in the library and are shared
//! with `tests/boot.rs`, which is the only thing that exercises them
//! automatically — `rv64_host::load_boot_images` for everything from "read
//! the kernel bytes" to "the CPU is sitting at the entry point", and
//! `rv64_host::run_until` for the loop. See those two doc comments for why
//! duplicating either in a binary that no test invokes would be a hazard
//! rather than mere repetition.

use rv64::bus::Bus;
use rv64::cache::PageCache;
use rv64::{PAGE, RAM_SIZE};
use rv64_host::{
    has_riscv_image_header, load_boot_images, riscv_image_footprint, run_until, BootError,
    HostFile, StdoutSink, DEFAULT_FRAMES,
};
use std::process::ExitCode;

fn usage() -> &'static str {
    "\
rv64-host --kernel <path> --dtb <path> --mem <path> [--initrd <path>]
          [--frames <n>] [--max-insns <n>]
rv64-host serve --kernel <path> --dtb <path> --mem <path> [--initrd <path>]
                --port <path> [--frames <n>] [--input]

  `serve` lays the guest images into --mem with the same loader as above,
  then answers page and console requests read from --port instead of
  running the guest itself. Run `rv64-host serve --help` for its flags.

  --kernel <path>   kernel image (required). An ELF (identified by its
                     \\x7fELF magic) is loaded at its own PT_LOAD physical
                     addresses; anything else is treated as a raw riscv64
                     Image and written verbatim at 0x80200000.
  --dtb <path>      flattened device tree blob, placed 8-byte-aligned above
                     the kernel's memory footprint (required)
  --initrd <path>   initramfs cpio (optionally gzipped), placed on a page
                     boundary above the DTB. Its address and size are
                     written into the device tree's /chosen
                     linux,initrd-start and linux,initrd-end properties,
                     which must already exist there as two-cell values.
  --mem <path>      backing file for guest RAM (required). NOTE: opened
                     with truncate(true) — every run starts from a zeroed
                     image. This is not a resumable snapshot; --mem is
                     overwritten on every invocation.
  --frames <n>      resident page-cache frames, at least 1 (default 256)
  --max-insns <n>   instruction budget before the run is aborted as
                     non-terminating (default: unlimited)
"
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Reads a numeric flag, distinguishing "absent" (use `default`) from
/// "present but not a valid number" (an error, not a silent fallback to
/// `default`). The distinction matters most for `--max-insns`: it is the
/// one safety valve against a hung guest running forever, and a typo'd
/// value silently disarming it — rather than being reported — is worse
/// than the typo itself in a project whose central risk is exactly a
/// silent hang.
fn num_flag(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    match flag(args, name) {
        None => Ok(default),
        Some(v) => v.parse::<u64>().map_err(|_| format!("{name}: invalid number '{v}'")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("serve") {
        return serve_main(&args[1..]);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let (Some(kernel_path), Some(dtb_path), Some(mem_path)) =
        (flag(&args, "--kernel"), flag(&args, "--dtb"), flag(&args, "--mem"))
    else {
        eprintln!("error: --kernel, --dtb, and --mem are all required\n");
        eprint!("{}", usage());
        return ExitCode::FAILURE;
    };

    let frames = match num_flag(&args, "--frames", DEFAULT_FRAMES as u64) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if frames < 1 {
        eprintln!("error: --frames must be at least 1, got {frames}");
        return ExitCode::FAILURE;
    }
    let frames = frames as usize;

    let max_insns = match num_flag(&args, "--max-insns", u64::MAX) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let kernel_bytes = match std::fs::read(&kernel_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading kernel {kernel_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let dtb_bytes = match std::fs::read(&dtb_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading dtb {dtb_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let initrd_path = flag(&args, "--initrd");
    let initrd_bytes = match initrd_path.as_ref().map(std::fs::read).transpose() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading initrd {}: {e}", initrd_path.unwrap());
            return ExitCode::FAILURE;
        }
    };

    let pages = (RAM_SIZE / PAGE as u64) as u32;
    let backing = match HostFile::new(&mem_path, pages) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: opening mem file {mem_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut bus = Bus::new(PageCache::new(backing, frames), StdoutSink);

    // A raw Image whose footprint we could not read gets its file length
    // used instead, which is exactly the mistake that puts the DTB inside
    // .bss. Every real kernel declares one, so say something rather than
    // letting the weaker answer pass unremarked — and distinguish the two
    // ways it can happen, because they send the reader to different places.
    if !kernel_bytes.starts_with(b"\x7fELF") && riscv_image_footprint(&kernel_bytes).is_none() {
        let why = if has_riscv_image_header(&kernel_bytes) {
            "declares an image_size smaller than the file itself, which is a \
             corrupt boot header"
        } else {
            "has no riscv64 boot header"
        };
        eprintln!(
            "warning: {kernel_path} {why}, so its memory footprint is being taken \
             as its file length ({} bytes). If this is a real kernel, the DTB and \
             initrd may land inside its .bss.",
            kernel_bytes.len()
        );
    }

    // Loading, placement and the `/chosen` patch all live in
    // `rv64_host::load_boot_images`, shared with the boot test's
    // `boot_capturing`. Only the file names — which that function has no
    // reason to know — are added back here.
    let (mut cpu, _layout) =
        match load_boot_images(&mut bus, &kernel_bytes, &dtb_bytes, initrd_bytes.as_deref()) {
            Ok(v) => v,
            Err(e) => {
                match &e {
                    BootError::Kernel(_) => eprintln!("error: {e} ({kernel_path})"),
                    BootError::Dtb(_) => eprintln!("error: {e} ({dtb_path})"),
                    BootError::Initrd(_) => {
                        eprintln!("error: {e} ({})", initrd_path.as_deref().unwrap_or("?"))
                    }
                    _ => eprintln!("error: {e}"),
                }
                return ExitCode::FAILURE;
            }
        };

    // No stdin plumbing in *this* mode, deliberately. `serve --input` grew one
    // (`rv64_host::serve::pump_input`), but it forwards keystrokes as `ConIn`
    // frames to a badge that owns the guest; here the guest is this process's
    // own `Bus` and the equivalent would feed `bus.uart.push_input` directly.
    // Nothing needs it: this mode exists to boot a kernel and print the
    // counters, `tests/boot.rs` drives it, and no caller has ever wanted to
    // type at it. Adding a raw-mode terminal and a second thread to a loop that
    // is otherwise a pure function of its inputs would cost more than it buys.
    // If that changes, `rawtty::make_raw_console` and the escape handling in
    // `pump_input` are the two halves to reuse.

    let outcome = run_until(&mut cpu, &mut bus, max_insns);

    let stats = bus.cache_mut().stats();
    eprintln!(
        "instructions executed: {}\n\
         page cache ({frames} frames): hits={} misses={} evictions={} writebacks={} declined={}\n\
         mmu walks: {}",
        outcome.executed(),
        stats.hits,
        stats.misses,
        stats.evictions,
        stats.writebacks,
        stats.declined,
        cpu.mmu.walks,
    );

    match outcome.diagnostic() {
        // The only ending that is not a problem: the guest asked to stop.
        None => ExitCode::SUCCESS,
        // Everything else is a failure, `Capped` included — matching
        // `--max-insns`'s own description ("aborted as non-terminating"), a
        // guest that has not finished within its budget is reported as such
        // rather than as a quiet success with a warning nobody has to notice.
        Some(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn serve_usage() -> &'static str {
    "\
rv64-host serve --kernel <path> --dtb <path> --mem <path> [--initrd <path>]
                --port <path> [--frames <n>] [--input]

  Lays the guest images into --mem with the same loader `rv64-host` uses to
  boot (see its own --help for --kernel/--dtb/--initrd's exact rules), then
  reopens that file and answers page requests read off --port, writing
  replies back to it, echoing any guest console output to stdout. The badge
  loads nothing; it is the client that sends the requests this answers.

  --port is put into raw mode before any frame crosses it. A USB-CDC node is
  a tty, and a tty defaults to canonical mode, in which ICANON, ECHO, ICRNL,
  ISIG and IXON between them corrupt the first 4 KiB page that goes over it.
  This prints `raw mode set`, or refuses to serve if it cannot.

  Typing at the guest needs --input, and nothing turns it on for you. isatty
  would be the obvious auto-detector and it is the wrong one: `serve-wait.sh`
  runs under `nohup` in a job whose stdin is still the terminal, so autodetect
  would put that terminal into raw mode and have a background job read from it
  -- SIGTTIN, a stopped server, and a stolen keyboard, on the runs where nobody
  is watching. A flag makes an unattended run identical to today by
  construction rather than by inference.

  Two output streams, kept separate on purpose:
    stdout  the guest's console, from ConOut frames.
    stderr  everything arriving on --port that is not a frame, verbatim.
            That is the badge's USB panic mirror -- `PANIC in PID n:` and the
            text after it -- which shares the CDC endpoint with the protocol.
            `rv64-host serve >guest.log 2>badge.log` captures both.

  --kernel <path>   kernel image (required)
  --dtb <path>      flattened device tree blob (required)
  --initrd <path>   initramfs cpio, optionally gzipped (optional)
  --mem <path>      backing file for guest RAM (required). Built fresh on
                     every invocation, then reopened read/write to serve —
                     not a resumable snapshot.
  --port <path>     serial device to serve page/console requests over
                     (required)
  --frames <n>      resident page-cache frames used while *loading* the
                     images, at least 1 (default 256). Irrelevant once
                     serving starts: the serve phase reads and writes the
                     plain file directly, with no cache in front of it.
  --console <path>  guest console output (ConOut frames) is appended here
                     instead of stdout. Use it whenever the transcript merges
                     stdout and stderr: stderr carries every byte that was NOT
                     a frame, and telling the two apart is otherwise
                     impossible.
  --input           Forward this terminal's keystrokes to the guest as ConIn
                     frames, so you can type at the shell the badge is running.
                     OFF BY DEFAULT and never auto-enabled -- see below.
                     stdin is put into raw mode (no echo, no line buffering, no
                     ISIG) and restored when serve exits.
                       Ctrl-C      end serve and restore the terminal. It is
                                   NOT forwarded to the guest.
                       Ctrl-] <k>  send <k> to the guest verbatim, so Ctrl-]
                                   Ctrl-C interrupts the guest's process.
                     With --input, guest console output is ALSO written to
                     stdout so you can see what you are typing at; --console
                     still gets its own complete copy.
                     A non-tty stdin (a pipe, a file) is accepted and forwarded
                     with no termios call, which is how a script types.
  --pace-ms <n>     DIAGNOSTIC, off by default. Write each reply as 512-byte
                     chunks with n milliseconds between them, so at most one
                     USB packet is in flight. `usb-bao1x` takes one packet out
                     of the hardware per UsbDevice::poll(), and one interrupt
                     can cover several arrivals -- this tests whether the badge
                     simply cannot keep up with back-to-back packets, without
                     rebuilding firmware. Try 1 first; lower it to find the
                     threshold. Not a transport policy: at 1 ms a 4109-byte
                     page costs ~9 ms against a 2 ms round trip.
"
}

/// `rv64-host serve`: the laptop side of the badge link. Builds the flat
/// guest image with the same [`load_boot_images`] the default boot mode
/// uses — the DTB/initrd placement rules it encodes are exactly what makes
/// the badge's copy of the image bootable, so this must not re-derive them
/// — flushes it to disk, then serves page requests off it over `--port`
/// until the port closes.
fn serve_main(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", serve_usage());
        return ExitCode::SUCCESS;
    }

    let (Some(kernel_path), Some(dtb_path), Some(mem_path), Some(port_path)) = (
        flag(args, "--kernel"),
        flag(args, "--dtb"),
        flag(args, "--mem"),
        flag(args, "--port"),
    ) else {
        eprintln!("error: --kernel, --dtb, --mem, and --port are all required\n");
        eprint!("{}", serve_usage());
        return ExitCode::FAILURE;
    };

    let frames = match num_flag(args, "--frames", DEFAULT_FRAMES as u64) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if frames < 1 {
        eprintln!("error: --frames must be at least 1, got {frames}");
        return ExitCode::FAILURE;
    }
    let frames = frames as usize;

    let kernel_bytes = match std::fs::read(&kernel_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading kernel {kernel_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let dtb_bytes = match std::fs::read(&dtb_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading dtb {dtb_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let initrd_path = flag(args, "--initrd");
    let initrd_bytes = match initrd_path.as_ref().map(std::fs::read).transpose() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading initrd {}: {e}", initrd_path.unwrap());
            return ExitCode::FAILURE;
        }
    };

    // --- Load phase: unchanged from what the default boot mode does, down
    // to the same `HostFile` -> `PageCache` -> `Bus` -> `load_boot_images`
    // sequence. `HostFile::new` truncates, so this must be the only place
    // that opens `--mem` through it.
    let pages = (RAM_SIZE / PAGE as u64) as u32;
    let backing = match HostFile::new(&mem_path, pages) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: opening mem file {mem_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut bus = Bus::new(PageCache::new(backing, frames), StdoutSink);

    if let Err(e) = load_boot_images(&mut bus, &kernel_bytes, &dtb_bytes, initrd_bytes.as_deref())
    {
        match &e {
            BootError::Kernel(_) => eprintln!("error: {e} ({kernel_path})"),
            BootError::Dtb(_) => eprintln!("error: {e} ({dtb_path})"),
            BootError::Initrd(_) => {
                eprintln!("error: {e} ({})", initrd_path.as_deref().unwrap_or("?"))
            }
            _ => eprintln!("error: {e}"),
        }
        return ExitCode::FAILURE;
    }

    // Every byte the loader wrote goes through the page cache; without this
    // flush, whatever is still resident never reaches disk and the serve
    // phase below would answer some pages with stale (zero) data.
    if let Err(e) = bus.cache_mut().flush() {
        eprintln!("error: flushing the loaded image to {mem_path}: {e:?}");
        return ExitCode::FAILURE;
    }
    // `Bus`/`PageCache` expose no way to recover the backing store (no
    // `into_cache`/`into_backing` — the global constraint forbids adding
    // one to `crates/rv64`), so the serve phase drops the whole load-phase
    // stack and reopens `--mem` as a plain file instead.
    drop(bus);

    // --- Serve phase: a plain file, opened without `create`/`truncate` —
    // `HostFile::new` would zero out everything the load phase just wrote.
    let mut img = match std::fs::OpenOptions::new().read(true).write(true).open(&mem_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: reopening mem file {mem_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let port = match std::fs::OpenOptions::new().read(true).write(true).open(&port_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: opening port {port_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Raw mode, before a single byte crosses. `--port` is a USB-CDC tty and a
    // freshly opened tty is in canonical mode, where `ICANON` waits for a
    // newline, `ECHO` sends every request straight back at the badge, `ICRNL`
    // and `ONLCR` rewrite CR/LF *inside page data*, `ISIG` turns a `0x03` byte
    // in a page into SIGINT, and `IXON` eats `0x11`/`0x13`. A 4 KiB page of a
    // kernel image contains all of those, so the first exchange is corrupted
    // and the badge reports it as a fault at its own end of the cable. See
    // `rv64_host::rawtty` for the full reasoning and for why this is not an
    // `stty` in the README.
    //
    // Reported either way, and it is worth the two lines: "not a tty" against a
    // path the operator believes is the badge means they typed the wrong node,
    // and that is otherwise indistinguishable from a badge that never speaks.
    match rv64_host::rawtty::make_raw(&port) {
        Ok(true) => eprintln!("{port_path}: raw mode set"),
        Ok(false) => eprintln!(
            "note: {port_path} is not a terminal, so no line discipline was configured. \
             If this is meant to be the badge's USB-CDC node, it is the wrong path."
        ),
        Err(e) => {
            eprintln!(
                "error: cannot put {port_path} into raw mode: {e}\n\
                 Serving a canonical-mode tty cannot work -- every frame would be \
                 echoed, CR/LF-translated and line-buffered -- so this stops here \
                 rather than failing later as a phantom fault on the badge."
            );
            return ExitCode::FAILURE;
        }
    }
    let rx = match port.try_clone() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cloning port {port_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `--pace-ms`: an instrument, off unless asked for. See
    // `rv64_host::serve::Pace` for what it is testing and why the default has
    // to be off.
    let pace = match flag(args, "--pace-ms") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(ms) => {
                let p = rv64_host::serve::Pace {
                    chunk: rv64_host::serve::CDC_PACKET,
                    gap: std::time::Duration::from_millis(ms),
                };
                // Said out loud, because a transcript has to record what was
                // actually done -- a paced run and an unpaced one are different
                // experiments and they look identical afterwards otherwise.
                eprintln!(
                    "pacing replies: {}-byte chunks, {} ms between them \
                     (diagnostic; omit --pace-ms for normal operation)",
                    p.chunk, ms
                );
                Some(p)
            }
            Err(_) => {
                eprintln!("error: --pace-ms: invalid number '{v}'");
                return ExitCode::FAILURE;
            }
        },
    };

    // Where guest console output goes. Default stdout, as before; `--console`
    // sends it to its own file so it can be told apart from the *other* stream
    // this command produces -- bytes that were not a frame at all, which go to
    // stderr. Merging the two (`2>&1`) makes "is this the guest talking, or is
    // it unframed traffic on the wire?" unanswerable, and that question has
    // come up twice.
    let input = args.iter().any(|a| a == "--input");

    let mut con = match flag(args, "--console") {
        Some(path) => {
            let f = match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error: opening console log {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // With `--input` the operator is typing at the guest, and a reply
            // they cannot see is not an interactive session. So the file gets
            // its complete copy, unchanged — every existing use of `--console`
            // reads that file and must keep getting exactly what it got before
            // — and stdout gets one too, purely so there is something on the
            // screen to type against. Without `--input`, nothing is teed and
            // the behaviour is byte for byte what it was.
            if input {
                eprintln!("guest console -> {path} and this terminal (--input)");
                ConSink::Both(f, std::io::stdout())
            } else {
                eprintln!("guest console -> {path}");
                ConSink::File(f)
            }
        }
        None => ConSink::Stdout(std::io::stdout()),
    };

    // One writer for the port, shared: with `--input` the keyboard thread puts
    // `ConIn` frames on the same wire the page loop is answering requests on,
    // and `FrameTx`'s lock is what stops a keystroke landing inside a page.
    let tx = std::sync::Arc::new(rv64_host::serve::FrameTx::new(port));

    // Raw mode on the operator's terminal, and the means to undo it. `Arc`
    // because both this thread and the keyboard thread have to be able to put
    // the terminal back: whichever of them ends the run gets there first, and
    // `Restore::restore` is idempotent so the other one costs nothing.
    let restore = if input {
        match rv64_host::rawtty::make_raw_console(&std::io::stdin()) {
            Ok(r) => {
                if r.is_none() {
                    eprintln!(
                        "note: stdin is not a terminal, so it is forwarded as-is with no \
                         line-discipline change. Ctrl-C and Ctrl-] still apply."
                    );
                } else {
                    eprintln!(
                        "input: typing goes to the guest. Ctrl-C ends serve; \
                         Ctrl-] <key> sends <key> through (Ctrl-] Ctrl-C interrupts the guest)."
                    );
                }
                r.map(std::sync::Arc::new)
            }
            Err(e) => {
                // Not fatal to the link — the page loop below is unaffected —
                // but it must be said, because a terminal that quietly stayed
                // canonical looks exactly like a badge ignoring the keyboard.
                eprintln!(
                    "error: cannot put stdin into raw mode: {e}\n\
                     Serving continues without a keyboard; guest output is unaffected."
                );
                None
            }
        }
    } else {
        None
    };

    if input {
        let keys = std::sync::Arc::clone(&tx);
        let guard = restore.clone();
        // A thread, not a poll of stdin from the serve loop. The serve loop is
        // the only thing standing between the guest and its 2000 ms deadline,
        // and it must never be doing anything but waiting for a request and
        // answering it. See `rv64_host::serve::pump_input` for why polling
        // would also have to set `O_NONBLOCK` on a file description the
        // operator's shell shares.
        std::thread::spawn(move || {
            let end = rv64_host::serve::pump_input(&mut std::io::stdin(), &keys, pace);
            // Explicitly, before `exit` — which runs no destructors, so the
            // `Restore`'s `Drop` would never fire and the operator would be
            // left with a terminal that does not echo.
            if let Some(g) = guard.as_deref() {
                g.restore();
            }
            match end {
                Ok(rv64_host::serve::InputEnd::Quit) => {
                    eprintln!("\n[rv64-host: Ctrl-C, stopping]");
                    std::process::exit(0);
                }
                // stdin ran out. The badge is still being served, and that is
                // the interesting half — a script that piped in a command
                // should not tear down the link behind it. The terminal is
                // already back, so `serve` simply carries on without a
                // keyboard.
                Ok(rv64_host::serve::InputEnd::Eof) => {}
                Err(e) => eprintln!("\n[rv64-host: keyboard input stopped: {e}]"),
            }
        });
    }

    let result = rv64_host::serve::serve_shared(&mut img, rx, &*tx, pace, &mut con);
    // The other exit path: the port closed and `serve` returned. The keyboard
    // thread is still parked in `read`, so its copy of the guard will never be
    // dropped — this call is the whole reason `Restore` is not `Drop`-only.
    if let Some(g) = restore.as_deref() {
        g.restore();
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: serving over {port_path}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Where guest console output goes.
///
/// `Both` exists for `--input`: see the comment at its construction. It is an
/// enum rather than a `Vec<Box<dyn Write>>` because there are exactly three
/// cases, two of them predate this and must not change, and a `match` says so.
enum ConSink {
    Stdout(std::io::Stdout),
    File(std::fs::File),
    /// The `--console` file *and* the terminal. The file is written first: it
    /// is the durable record, and if stdout is a closed pipe the transcript
    /// must still be complete.
    Both(std::fs::File, std::io::Stdout),
}

impl std::io::Write for ConSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConSink::Stdout(o) => o.write_all(buf)?,
            ConSink::File(f) => f.write_all(buf)?,
            ConSink::Both(f, o) => {
                f.write_all(buf)?;
                o.write_all(buf)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConSink::Stdout(o) => o.flush(),
            ConSink::File(f) => f.flush(),
            ConSink::Both(f, o) => {
                f.flush()?;
                o.flush()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_finds_the_value_following_its_name() {
        let args = vec!["--kernel".to_string(), "x.elf".to_string()];
        assert_eq!(flag(&args, "--kernel"), Some("x.elf".to_string()));
        assert_eq!(flag(&args, "--dtb"), None);
    }

    #[test]
    fn num_flag_uses_the_default_when_absent() {
        let args: Vec<String> = vec![];
        assert_eq!(num_flag(&args, "--frames", 256), Ok(256));
    }

    #[test]
    fn num_flag_parses_a_present_value() {
        let args = vec!["--frames".to_string(), "42".to_string()];
        assert_eq!(num_flag(&args, "--frames", 256), Ok(42));
    }

    /// The regression this guards: a present-but-unparseable value must be
    /// reported, not silently treated the same as "absent". Before this
    /// fix, `--max-insns 1_000` (Rust digit-group syntax, not a valid `u64`
    /// literal for `str::parse`) silently produced the *unbounded* default
    /// instead of an error — turning a user's explicit safety cap into no
    /// cap at all, with no diagnostic.
    #[test]
    fn num_flag_reports_an_unparseable_value_instead_of_silently_defaulting() {
        let args = vec!["--max-insns".to_string(), "1_000".to_string()];
        assert_eq!(
            num_flag(&args, "--max-insns", u64::MAX),
            Err("--max-insns: invalid number '1_000'".to_string())
        );
    }
}
