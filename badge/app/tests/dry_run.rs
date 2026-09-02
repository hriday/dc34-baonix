//! **The laptop dry run: the whole badge, minus the two syscall leaves.**
//!
//! This boots the real nixpkgs-built riscv64 guest to a busybox shell through
//! the badge's own integrated run loop, on a laptop, against the real
//! `rv64_host::serve` over a real socket. Only two things are swapped for the
//! hardware ones:
//!
//! | badge | here |
//! |---|---|
//! | `usbhost::UsbTransport` — `usb_bao1x` syscalls | [`SocketTransport`] — a TCP loopback pair |
//! | `oled::GfxScreen` — `Gfx` syscalls | `oled::FakeScreen` — records the frames |
//!
//! Everything else is the same code the badge runs: the same
//! [`badge_app::run`] loop, the same `UsbHost` exchange with its park-then-send
//! ordering and its retry budget, the same `rv64_proto` frame codec, the same
//! `rv64::PageCache`, the same `OledSink` grid, the same DTB derivation, the
//! same slice length, the same frame count. There is no reimplementation of
//! anything anywhere in this file — the host side is
//! `rv64_host::serve::serve`, the function `rv64-host serve` itself calls, run
//! on a thread with the image `rv64_host::load_boot_images` laid down.
//!
//! # Why this exists
//!
//! Because the alternative is a flash-and-photograph cycle through a human for
//! every defect, and Task 6 spent three of those on bugs that were policy
//! decisions hidden below a `#[cfg(target_os = "xous")]`. If this test passes,
//! everything about the badge port has been tested except the `usb_bao1x` and
//! `Gfx` syscalls. If it fails, it fails in minutes.
//!
//! # What it deliberately does *not* prove
//!
//! * **Timing.** A loopback socket is ~50 µs per round trip; the badge's CDC
//!   link is 2 ms. This says the boot is *correct*, never that it is fast.
//! * **Memory.** The laptop has no 2 MiB SRAM ceiling, no swapper, and no
//!   process heap limit, so nothing here can fail the way the badge fails.
//! * **The park-before-send race.** [`SocketTransport::arm`] has nothing to
//!   park: the race is a property of `usb-bao1x`'s listen mode, and it is
//!   covered by `usbhost`'s `park_is_confirmed_before_any_request_byte_is_sent`
//!   against a fake that models it.
//! * **Frame corruption from the log mirror.** Nothing here interleaves
//!   mirrored log text into the transmit stream; the retry path that answers it
//!   is covered by `usbhost`'s own tests.
//!
//! # Running it
//!
//! ```text
//! nix develop
//! cd badge/app
//! cargo test --release --test dry_run -- --nocapture
//! ```
//!
//! **`--release` is not optional advice.** Booting Linux is 175 million
//! emulated instructions: ~40 seconds optimized, minutes not. It skips when
//! `GUEST_KERNEL`/`GUEST_DTB`/`GUEST_INITRAMFS` are absent, like the
//! workspace's other integration suites; the devShell sets `RV64_REQUIRE_SUITES`
//! so that inside `nix develop` the skip is a failure instead.

use badge_app::oled::{FakeScreen, OledSink, COLS, ROWS};
use badge_app::run::{self, Config, Console, Ending, FRAMES};
use badge_app::usbhost::{
    send_all, SendError, SendFault, Transport, ATTEMPT_DEADLINE_MS, SERIAL_BINARY_BUFLEN,
};
use rv64::uart::ConsoleSink;
use rv64_proto::{encode, Frame};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The busybox `ash` prompt. `/init` ends in `exec /bin/sh`, so this cannot
/// appear until the guest has unpacked the initramfs and run PID 1 to
/// completion.
const SHELL_PROMPT: &str = "~ #";

/// What the dry run types at the shell once it has one.
///
/// **The shell computes the marker; the command line does not contain it.**
/// That is deliberate and it is the difference between this test proving
/// something and passing on a coincidence. An earlier version typed
/// `echo BADGE-INPUT-OK` and waited for `BADGE-INPUT-OK`, which the tty's own
/// echo of the command line satisfies — so the test could not distinguish "the
/// guest ran the command" from "the guest echoed what it was sent", and it
/// stopped the run at whatever slice boundary the echo completed on, leaving
/// the command's actual output cut off mid-word. `$((6*7))` is expanded by
/// `ash`, so [`TYPED_MARKER`] can only appear in output the guest *produced*.
const TYPED: &str = "echo BADGE-INPUT-$((6*7))\n";

/// The complete line the guest must print, **terminated**.
///
/// The terminator is the load-bearing part. Waiting for a bare
/// `BADGE-INPUT-42` would stop the run at the slice boundary that happened to
/// contain the last character of the marker, which says nothing about whether
/// the console pipeline delivered the rest of the line — a pipeline that
/// silently ate the next three bytes would look identical. Requiring the CR
/// that busybox's ONLCR appends means the guest wrote the whole line and every
/// byte of it arrived. See `the_marker_is_only_satisfied_by_a_complete_line`.
const TYPED_MARKER: &str = "BADGE-INPUT-42\r";

/// Safety valve, matching `crates/rv64-host/tests/boot.rs`: ~5.7x the measured
/// 175 M instructions a real boot costs, so a guest that never reaches a prompt
/// fails rather than hanging.
const MAX_INSNS: u64 = 1_000_000_000;

/// Wall-clock guard, so a wedged *link* — as opposed to a wedged guest — also
/// terminates. The instruction cap cannot catch that: a link that never answers
/// stops retiring instructions altogether.
const WALL_CLOCK_LIMIT: Duration = Duration::from_secs(900);

/// The alphabet Nix store-path hashes are printed in: base32 without `e`, `o`,
/// `u` or `t`.
const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

// ---------------------------------------------------------------------------
// The laptop's transport leaf
// ---------------------------------------------------------------------------

/// A [`Transport`] over a TCP loopback socket, shaped like the badge's.
///
/// The shape is copied deliberately rather than simplified, because the shape
/// is what the protocol code above it is built around:
///
/// * a **reader thread** publishes deliveries into a queue, exactly as
///   `UsbTransport`'s does, so `recv` never owns the socket and `poll` can
///   return what has already arrived with no wait at all;
/// * deliveries are **capped at [`SERIAL_BINARY_BUFLEN`] (3840)**, so a
///   4109-byte page frame arrives in pieces here as it does on the badge and
///   the `Mux` has to accumulate across `recv` calls;
/// * `send` goes through the same [`send_all`] the badge uses, against a sink
///   that truncates at 3840 the way `serial_send` does.
///
/// What it does not model is the park: see the module docs.
struct SocketTransport {
    tx: TcpStream,
    rx: Receiver<Vec<u8>>,
    t0: Instant,
    /// The bytes of the previous request, for the re-send detector below.
    last_req: Vec<u8>,
    /// When the outstanding request went out, cleared by the first delivery
    /// that answers it.
    sent_at: Option<Instant>,
    diag: Arc<Mutex<Diag>>,
}

/// What the link actually did, as opposed to what its counters summarise.
///
/// This exists to answer one question that `Link::retries()` alone cannot:
/// **why** a retry fired. `retries` counts "an attempt went unanswered for
/// [`ATTEMPT_DEADLINE_MS`]", from any cause. On the badge the intended cause is
/// a request frame split by mirrored log text sharing the CDC transmit
/// endpoint, and that is the number a hardware transcript is meant to be read
/// for. A loopback socket has no such medium — so a non-zero `retries` here
/// means something *else* can produce one, and if that something also exists on
/// hardware then the hardware number means two things at once.
///
/// So the dry run measures the distribution rather than the total: how long
/// each request waited for its answer, which requests were re-sent, and when.
///
/// # What that measurement found, 2026-08-24
///
/// A boot is **15,558 requests**. Unloaded, on a 16-core laptop, four
/// consecutive runs:
///
/// ```text
/// 0 re-sends; longest wait 10 / 14 / 14 / 23 ms
/// waits: <1ms ~12,800   1-9ms ~2,700   10-99ms 7-10   >=100ms 0
/// ```
///
/// Eight of these boots run *concurrently* — 124,464 requests against 16 cores
/// with 24 busy threads — still **0 re-sends**, with the worst single wait at
/// **71 ms**. So the wait tracks scheduler latency, and even at 8x overload it
/// stays 3.5x short of [`ATTEMPT_DEADLINE_MS`].
///
/// That is what a `retries=1 stale=1` seen on a loaded machine is: one wait
/// that crossed 250 ms, one re-send, and then the original (merely late, never
/// lost) answer completing the exchange — leaving the *duplicate* held, to be
/// purged by the next exchange's `discard_stale_responses`. The counters
/// agreeing 1:1 is the signature of that path, not of a dropped frame; a
/// dropped frame would give retries without a matching stale.
///
/// **It matters for the hardware number.** The badge's peer is a laptop running
/// `rv64-host serve`, doing this same file I/O under this same scheduler, so
/// this cause does not disappear on hardware — `retries` there is
/// "frames split by the log mirror **plus** host stalls past 250 ms". What the
/// numbers above buy is a bound on the second term: on an unloaded host it is
/// zero with an order of magnitude to spare, so a non-zero `retries` in a
/// hardware transcript can be read as interleaving. The assertion below is what
/// keeps that bound honest rather than assumed.
#[derive(Default)]
struct Diag {
    sends: usize,
    /// Requests sent whose bytes were byte-identical to the request before
    /// them. The exchange is strictly synchronous and a page becomes resident
    /// once it is read, so the *only* way the same request can go out twice in
    /// a row is `UsbHost::exchange` re-sending it.
    resends: usize,
    /// `(ms since the link opened, ms the re-sent attempt had waited)` per
    /// re-send.
    resend_at: Vec<(u64, u64)>,
    /// Longest wait from a request going out to the first byte of its answer.
    max_reply_ms: u64,
    /// Every wait at or above [`STALL_MS`], as `(ms since the link opened,
    /// wait in ms)`. This is the evidence: a re-send is preceded by a wait past
    /// [`ATTEMPT_DEADLINE_MS`], and where those waits cluster says what caused
    /// them.
    stalls: Vec<(u64, u64)>,
    /// Waits, bucketed by order of magnitude, so a single outlier is visibly
    /// an outlier rather than a number next to an average.
    buckets: [usize; 5],
    /// `ConOut` frames the badge sent, and the guest bytes inside them.
    ///
    /// Counted here rather than at the host because `serve` echoes console
    /// output to *its* stdout, which is this test's stdout and not something a
    /// test can assert on. What this measures is the badge half of the mirror:
    /// every byte the guest printed was encoded into a `ConOut` frame and put
    /// on the wire. The host half — decode and echo — is covered by
    /// `crates/rv64-host/tests/serve.rs`; the two together are the chain.
    conout_frames: usize,
    conout_bytes: usize,
    /// Every `WriteReq` the badge sends, as `(requests sent before it, page)`.
    ///
    /// # Why the wire and not the cache
    ///
    /// A writeback is the only badge->host frame that carries a **page** of
    /// data (4109 bytes against a 13-byte `ReadReq`), and until this was
    /// recorded nothing on either side of the link could say *which* page the
    /// first one carries or *when* it goes out. That mattered because a
    /// hardware transcript showed device-tree content arriving from the badge
    /// as unframed bytes, and the badge has no device tree of its own — only
    /// the one it received as page data. Reading it off the transport is what
    /// makes "the badge is leaking a received buffer" and "the badge is sending
    /// a perfectly ordinary writeback of a page it received" distinguishable,
    /// and they are the same bytes on the wire.
    ///
    /// The ordinal is the request count rather than a timestamp because it is
    /// the number a hardware transcript can be compared against: the badge's
    /// `misses` counter is one `ReadReq` each.
    writes: Vec<(usize, u32)>,
}

impl Diag {
    fn bucket(ms: u64) -> usize {
        match ms {
            0 => 0,
            1..=9 => 1,
            10..=99 => 2,
            100..=999 => 3,
            _ => 4,
        }
    }

    fn report(&self) -> String {
        let names = ["<1ms", "1-9ms", "10-99ms", "100-999ms", ">=1s"];
        let hist: Vec<String> = names
            .iter()
            .zip(self.buckets.iter())
            .map(|(n, c)| format!("{n}={c}"))
            .collect();
        format!(
            "link diagnostics: {} requests, {} re-sends, longest wait {} ms\n  \
             waits: {}\n  \
             stalls >= {STALL_MS} ms: {:?}\n  \
             re-sends at (ms, waited ms): {:?}\n  \
             console mirrored to the host: {} ConOut frames, {} guest bytes\n  \
             writebacks: {} WriteReq frames; first ten (requests before it, page): {:?}",
            self.sends,
            self.resends,
            self.max_reply_ms,
            hist.join(" "),
            self.stalls,
            self.resend_at,
            self.conout_frames,
            self.conout_bytes,
            self.writes.len(),
            &self.writes[..self.writes.len().min(10)],
        )
    }
}

/// A wait worth recording individually. Two orders of magnitude above a healthy
/// loopback round trip and an order of magnitude below [`ATTEMPT_DEADLINE_MS`],
/// so the record shows a stall building rather than only the one that crossed
/// the line.
const STALL_MS: u64 = 20;

impl SocketTransport {
    fn new(sock: TcpStream, diag: Arc<Mutex<Diag>>) -> io::Result<Self> {
        // Loopback with Nagle on would add 40 ms to every round trip of a
        // request that leaves in two writes, which is most of them.
        sock.set_nodelay(true)?;
        let mut reader = sock.try_clone()?;
        let (send, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; SERIAL_BINARY_BUFLEN];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if send.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Ok(Self {
            tx: sock,
            rx,
            t0: Instant::now(),
            last_req: Vec::new(),
            sent_at: None,
            diag,
        })
    }

    /// Counts a `ConOut` frame on its way out, and the guest bytes in it.
    ///
    /// Console frames must not enter the re-send detector: they are not
    /// requests, and two identical `ConOut`s in a row are two identical lines
    /// from the guest, not a retry.
    fn note_console(&mut self, bytes: &[u8]) {
        let mut m = rv64_proto::Mux::new();
        m.push(bytes);
        let payload = m.take_console();
        let mut d = self.diag.lock().unwrap();
        d.conout_frames += 1;
        d.conout_bytes += payload.len();
    }

    /// Records that a request is going out, and whether it is a re-send.
    fn note_send(&mut self, bytes: &[u8]) {
        let now = self.t0.elapsed().as_millis() as u64;
        let waited = self.sent_at.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        {
            let mut d = self.diag.lock().unwrap();
            d.sends += 1;
            // `WriteReq`'s type byte is 0x03, at offset 2 after the two SYNC
            // bytes, and its page is the first four payload bytes (offset 5,
            // little-endian) — the layout `rv64_proto::encode` writes.
            if bytes.len() >= 9 && bytes[2] == 0x03 {
                let page = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
                let before = d.sends - 1;
                d.writes.push((before, page));
            }
            if bytes == self.last_req.as_slice() {
                d.resends += 1;
                d.resend_at.push((now, waited));
            }
        }
        self.last_req.clear();
        self.last_req.extend_from_slice(bytes);
        self.sent_at = Some(Instant::now());
    }

    /// Records that the outstanding request has been answered.
    fn note_reply(&mut self) {
        let Some(t) = self.sent_at.take() else { return };
        let ms = t.elapsed().as_millis() as u64;
        let mut d = self.diag.lock().unwrap();
        d.max_reply_ms = d.max_reply_ms.max(ms);
        d.buckets[Diag::bucket(ms)] += 1;
        if ms >= STALL_MS {
            let at = self.t0.elapsed().as_millis() as u64;
            d.stalls.push((at, ms));
        }
    }

    fn drain_ready(&mut self) -> Result<Vec<u8>, ()> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(b) => out.extend_from_slice(&b),
                Err(mpsc::TryRecvError::Empty) => return Ok(out),
                Err(mpsc::TryRecvError::Disconnected) => {
                    if out.is_empty() {
                        return Err(());
                    }
                    return Ok(out);
                }
            }
        }
    }
}

impl Transport for SocketTransport {
    /// Nothing to park. A socket buffers whatever arrives; the badge's USB
    /// server does not, which is the whole reason `arm` exists — see
    /// `usbhost`'s module docs and the fake that models it.
    fn arm(&mut self) -> Result<(), ()> {
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
        // `ConOut`'s type byte is 0x05, at offset 2 after the two SYNC bytes.
        // It is the one thing the badge sends that is not a request, so it is
        // counted separately and kept out of the re-send detector, whose whole
        // premise is that requests are synchronous.
        if bytes.len() > 2 && bytes[2] == 0x05 {
            self.note_console(bytes);
        } else {
            self.note_send(bytes);
        }
        let tx = &mut self.tx;
        let r = send_all(
            |b| {
                // `serial_send` truncates at 3840 and returns the accepted
                // prefix. Modelled here so the same `send_all` loop is exercised
                // on both sides of the `cfg`.
                let n = b.len().min(SERIAL_BINARY_BUFLEN);
                tx.write(&b[..n])
            },
            bytes,
        );
        match r {
            Ok(()) => Ok(()),
            Err(SendFault::Stalled) => Err(SendError::Stalled),
            Err(SendFault::Link(_)) => Err(SendError::Failed),
            // `send_all` passes no clock, so this is unreachable here; it is
            // matched rather than caught by a wildcard so a future deadline on
            // the dry run's transport has to be handled deliberately.
            Err(SendFault::TimedOut { sent, len }) => Err(SendError::TimedOut { sent, len }),
        }
    }

    fn recv(&mut self) -> Result<Vec<u8>, ()> {
        match self.rx.recv_timeout(Duration::from_millis(2)) {
            Ok(first) => {
                self.note_reply();
                let mut out = first;
                out.extend_from_slice(&self.drain_ready()?);
                Ok(out)
            }
            // "Not yet", never "the cable is gone" — the caller's deadlines
            // bound the wait.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(()),
        }
    }

    fn poll(&mut self) -> Result<Vec<u8>, ()> {
        self.drain_ready()
    }

    fn now_ms(&mut self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }
}

// ---------------------------------------------------------------------------
// The laptop's console leaf
// ---------------------------------------------------------------------------

/// The badge's real [`OledSink`] plus a transcript of every byte.
///
/// The screen is what the badge shows and what the project is judged on; the
/// transcript is what the *test* needs, because a 16x8 grid scrolls and the
/// shell prompt this run waits for would otherwise have to be caught in the one
/// frame it appeared in. Both halves see the identical byte stream, and the
/// screen half is not a stand-in for anything — it is `OledSink`, with
/// `FakeScreen` under it.
struct Tee {
    oled: OledSink<FakeScreen>,
    transcript: Vec<u8>,
}

impl ConsoleSink for Tee {
    fn put(&mut self, byte: u8) {
        self.oled.put(byte);
        self.transcript.push(byte);
    }
}

impl Console for Tee {
    fn flush(&mut self) {
        self.oled.flush()
    }
    fn tick(&mut self) {
        self.oled.tick()
    }
}

// ---------------------------------------------------------------------------
// The laptop's host side: the real `serve`, on a thread
// ---------------------------------------------------------------------------

/// A writer several threads share, whose `write_all` is atomic against the
/// others'.
///
/// `serve` writes each reply with one `write_all`, and the test injects
/// keystroke frames into the same socket. Without the lock — and without
/// overriding `write_all`, whose default implementation would release it
/// between chunks — an injected `ConIn` could land inside a `ReadResp` and the
/// badge's decoder would drop the page on its CRC. That is survivable (the
/// transport retries) but it would make the input test flaky for a reason that
/// has nothing to do with what it is testing.
#[derive(Clone)]
struct SharedTx(Arc<Mutex<TcpStream>>);

impl Write for SharedTx {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// Removes the guest image when the test ends, however it ends.
struct TempImage(PathBuf);

impl Drop for TempImage {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Lays the kernel, DTB and initramfs into a flat file with the *same* loader
/// `rv64-host serve` uses, then hands back the path.
///
/// This is `main.rs`'s `serve_main` load phase, function for function:
/// `HostFile::new` (which truncates, so it must open the file exactly once),
/// `PageCache`, `Bus`, `load_boot_images`, `flush`, drop. The placement rules
/// it encodes — the DTB above the kernel's declared *memory* footprint, the
/// initrd on a page of its own above that, the `/chosen` patch applied before
/// the bytes are written — are what make the image bootable, and re-deriving
/// any of them here would be re-deriving the thing under test.
fn lay_out_the_image(
    kernel: &PathBuf,
    dtb: &PathBuf,
    initramfs: &PathBuf,
) -> (TempImage, rv64::Stats) {
    let path = std::env::temp_dir()
        .join(format!("rv64-dry-run-{}-{:?}.img", std::process::id(), std::thread::current().id()));
    let guard = TempImage(path.clone());

    let kernel = std::fs::read(kernel).expect("kernel");
    let dtb = std::fs::read(dtb).expect("dtb");
    let initramfs = std::fs::read(initramfs).expect("initramfs");

    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let backing = rv64_host::HostFile::new(&path, pages).expect("cannot create the guest image");
    let mut bus = rv64::Bus::new(rv64::PageCache::new(backing, FRAMES), rv64::uart::VecSink::default());
    rv64_host::load_boot_images(&mut bus, &kernel, &dtb, Some(&initramfs))
        .expect("the guest images must load");
    // Every byte the loader wrote went through the cache; without this the
    // resident pages never reach disk and the serve phase answers with zeroes.
    bus.cache_mut().flush().expect("flushing the loaded image");
    // The load phase's own cache cost, returned rather than discarded: it is
    // the whole of the difference between the badge's counters and the
    // reference runner's, and measuring it is what turns "probably different
    // warming" into a number. See the divergence check below.
    let stats = bus.cache_mut().stats();
    drop(bus);

    (guard, stats)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn image(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var)?);
    p.exists().then_some(p)
}

/// Removes CSI escape sequences, the same rule `crates/rv64-host/tests/boot.rs`
/// and `oled::Grid` both use. busybox colourizes when its stdout is a tty, and
/// in the guest it is.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    out
}

/// Whether `marker` has appeared in `out`, resuming from `*scanned`.
///
/// `rv64_host`'s own `console_reached`, which is private there. The cursor is
/// left `marker.len() - 1` short of the end rather than at it, because the
/// marker can straddle two slices — `~ ` arriving in one and `#` in the next.
/// Getting that wrong does not fail loudly; the run simply never notices the
/// prompt and burns its whole budget.
fn console_reached(out: &[u8], marker: &[u8], scanned: &mut usize) -> bool {
    if marker.is_empty() || out.len() < marker.len() {
        return false;
    }
    let from = (*scanned).min(out.len().saturating_sub(marker.len()));
    let found = out[from..].windows(marker.len()).any(|w| w == marker);
    *scanned = out.len().saturating_sub(marker.len() - 1);
    found
}

/// A genuine store path as it lands on a 16-column screen: the 32-character
/// hash across two full rows, then the package name on the row under it.
/// `tests/oled_boot.rs`'s `store_path_on_screen`, which is private there.
fn store_path_on_screen(rows: &[String], i: usize, name: &str) -> bool {
    let (Some(top), Some(bottom)) = (rows.get(i), rows.get(i + 1)) else {
        return false;
    };
    let hash = format!("{}{}", top.trim_end(), bottom.trim_end());
    hash.len() == 32
        && hash.chars().all(|c| NIX_BASE32.contains(c))
        && rows.get(i + 2).is_some_and(|next| next.contains(name))
}

fn print_screen(rows: &[String]) {
    eprintln!("+{}+", "-".repeat(COLS));
    for row in rows {
        eprintln!("|{row}|");
    }
    eprintln!("+{}+", "-".repeat(COLS));
}

// ---------------------------------------------------------------------------
// The dry run
// ---------------------------------------------------------------------------

#[test]
fn the_badge_run_loop_boots_the_guest_to_a_shell_over_a_socket() {
    let (Some(kernel), Some(dtb), Some(initramfs)) =
        (image("GUEST_KERNEL"), image("GUEST_DTB"), image("GUEST_INITRAMFS"))
    else {
        rv64_host::suite_prerequisite_missing(
            "dry_run",
            "GUEST_KERNEL, GUEST_DTB and GUEST_INITRAMFS are not all set to files that \
             exist. `nix develop` sets all three from nix/guest; outside it, build them \
             with `nix build .#guest`",
        );
        return;
    };

    // --- the host: lay the image down, then serve it over a socket ---
    let (img_guard, load) = lay_out_the_image(&kernel, &dtb, &initramfs);
    let mut img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&img_guard.0)
        .expect("reopening the guest image");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let badge_side = TcpStream::connect(addr).expect("connect");
    let (host_side, _) = listener.accept().expect("accept");
    host_side.set_nodelay(true).expect("nodelay");

    let host_rx = host_side.try_clone().expect("clone");
    let host_tx = SharedTx(Arc::new(Mutex::new(host_side)));
    let inject = host_tx.clone();
    let host = std::thread::spawn(move || rv64_host::serve::serve(&mut img, host_rx, host_tx, None, &mut Vec::new()));

    // --- the badge: the same assembly `src/main.rs` performs ---
    //
    // Kept so the socket can be shut down at the end. Dropping the transport is
    // not enough: its reader thread holds a `try_clone` of the same socket, and
    // the host would wait for an EOF that never comes.
    let badge_shutdown = badge_side.try_clone().expect("clone");
    let diag = Arc::new(Mutex::new(Diag::default()));
    let transport = SocketTransport::new(badge_side, Arc::clone(&diag)).expect("transport");
    // `Mirrored<Tee>` is the badge's own composition, with `Tee` where
    // `OledSink<GfxScreen>` sits on hardware. Wrapping it here is what puts
    // `Mirrored` and the `ConOut` path under the dry run rather than leaving
    // them badge-only.
    let console = run::Mirrored::new(Tee {
        oled: OledSink::with_screen(FakeScreen::default()),
        transcript: Vec::new(),
    });
    let mut machine =
        run::assemble(transport, console, FRAMES).expect("the machine must assemble");

    // The DTB address is derived from the kernel's own boot header, in the
    // image the host just laid down. If that derivation ever drifts from
    // `rv64_host::boot_layout`'s, this is where it shows — the guest would
    // otherwise hang with no output, which is the worst failure mode here.
    eprintln!("dtb (a1) = {:#x}", machine.cpu.reg(11));

    let cfg = Config { max_insns: MAX_INSNS, ..Config::default() };
    let started = Instant::now();

    // --- phase 1: boot to a prompt ---
    let mut scanned = 0usize;
    let mut timed_out = false;
    let report = machine.run(&cfg, |bus, _| {
        if console_reached(&bus.uart.sink.inner().transcript, SHELL_PROMPT.as_bytes(), &mut scanned) {
            return false;
        }
        if started.elapsed() > WALL_CLOCK_LIMIT {
            timed_out = true;
            return false;
        }
        true
    });
    let boot_elapsed = started.elapsed();

    let transcript = strip_ansi(&String::from_utf8_lossy(&machine.bus.uart.sink.inner().transcript));
    eprintln!("{transcript}");
    eprintln!("{}", report.summary());
    eprintln!("wall clock to a shell prompt: {:.1}s", boot_elapsed.as_secs_f64());
    let boot_screen = machine.bus.uart.sink.inner().oled.frame();
    let boot_rows: Vec<String> = boot_screen.split('\n').map(str::to_string).collect();
    print_screen(&boot_rows);

    eprintln!("{}", diag.lock().unwrap().report());

    assert!(!timed_out, "the run gave up after {WALL_CLOCK_LIMIT:?} without a shell prompt");

    // **`retries` must be zero here, and that is the point of asserting it.**
    //
    // On the badge, `Link::retries()` is the number a hardware transcript is
    // read for: it counts request frames that went unanswered for
    // `ATTEMPT_DEADLINE_MS`, and the cause it exists to measure is a request
    // split by mirrored log text sharing the CDC transmit endpoint. A loopback
    // socket has no such medium and drops nothing, so anything above zero here
    // is a *second* cause — and a counter with two causes cannot be read as
    // either one.
    //
    // The distribution above is what makes this assertion diagnosable rather
    // than merely strict: a run that trips it prints where the waits were, so
    // the answer is in the failure rather than in a re-run with logging added.
    // The one wait long enough to matter would have to be `ATTEMPT_DEADLINE_MS`
    // (250 ms) of the host thread not being scheduled or of a `write` to the
    // image file blocking — both possible on a loaded machine, neither a
    // property of the port. If this ever fires in CI and the stall record shows
    // a single isolated outlier, that is what it is; a *pattern* is not.
    let d = diag.lock().unwrap();
    assert_eq!(
        report.retries, 0,
        "the link re-sent {} request(s) over a socket that cannot drop one. \
         `retries` is the number the first hardware transcript will be read for, \
         and it now means two things. {}\nThe re-send fires after \
         {ATTEMPT_DEADLINE_MS} ms with no answer, so look at the stall record: \
         an isolated outlier is the host thread losing the CPU or a write to the \
         image file blocking; a pattern is not.",
        report.retries,
        d.report(),
    );
    assert_eq!(report.stale_dropped, 0, "a duplicate answer was purged: {}", d.report());
    assert_eq!(report.pump_faults, 0, "the console pump failed: {}", d.report());
    assert_eq!(report.mirror_faults, 0, "console mirroring failed: {}", d.report());

    // The console mirror, end to end on the badge half: every byte the guest
    // printed left as a `ConOut` frame. Without this the serial transcript
    // carries nothing the guest said and the eight-row screen is the only
    // record -- which is fine for the photograph and useless for a kernel oops.
    assert!(d.conout_frames > 0, "the badge never sent a ConOut frame: {}", d.report());
    assert_eq!(
        d.conout_bytes,
        machine.bus.uart.sink.inner().transcript.len(),
        "the guest printed {} bytes but only {} reached the host as ConOut. \
         The transcript has holes in it. {}",
        machine.bus.uart.sink.inner().transcript.len(),
        d.conout_bytes,
        d.report(),
    );
    assert_eq!(
        machine.bus.uart.sink.dropped(),
        0,
        "the mirror buffer overflowed and dropped guest output"
    );
    drop(d);
    assert_eq!(
        report.ending,
        Ending::Stopped,
        "the boot did not reach a prompt; it ended as: {}",
        report.ending.describe()
    );
    assert!(
        transcript.contains(SHELL_PROMPT),
        "the guest never printed a `{SHELL_PROMPT}` prompt"
    );

    // The deliverable, in text: a genuine store path the guest read off its own
    // filesystem, laid out across the badge's grid. Not a fixture — every
    // character of it came out of the initramfs over the socket.
    let every_frame: Vec<Vec<String>> = machine
        .bus
        .uart
        .sink
        .inner_mut()
        .oled
        .screen_mut()
        .frames
        .iter()
        .map(|f| f.iter().map(|r| r.trim_end().to_string()).collect())
        .collect();
    assert!(
        every_frame
            .iter()
            .any(|f| (0..f.len()).any(|i| store_path_on_screen(f, i, "busybox"))),
        "no genuine store path was ever on the badge's screen"
    );
    // The frames really are the shape the display promises, all the way
    // through: a row that is not COLS wide would be the typesetter re-wrapping
    // rows the grid had already wrapped.
    for (n, f) in machine.bus.uart.sink.inner_mut().oled.screen_mut().frames.iter().enumerate() {
        assert_eq!(f.len(), ROWS, "frame {n} has {} rows", f.len());
        assert!(f.iter().all(|r| r.chars().count() == COLS), "frame {n}: {f:?}");
    }

    // --- the divergence check the plan's Step 5 asks for, made here ---
    //
    // Step 5 says to compare the badge's instruction count against the
    // laptop's, "because it is the same guest doing the same work. A
    // difference means the emulation diverged." That comparison is only worth
    // making on hardware if the *loop* has already been shown to agree with the
    // reference runner — otherwise a mismatch on the badge is ambiguous between
    // "the port diverged" and "these two loops were never going to agree".
    //
    // So it is made here first: the same guest, stopped at the same marker with
    // the same 100 000-instruction granularity, through `rv64_host::run_until`
    // instead of through `badge_app::run::run`.
    //
    // `executed` and `mmu_walks` must match **exactly**. The page-cache
    // counters must not, and the difference is *measured* below rather than
    // waved at, because these are the numbers a badge page-traffic estimate
    // will be built on and an unexplained 7% would undermine it.
    //
    // `boot_capturing` writes the boot images through the very same `PageCache`
    // it then runs the guest with, so its counters are the load phase **plus**
    // the boot. On the badge the host does the loading, so the badge's counters
    // are the boot alone, from a cold cache. `lay_out_the_image` performs
    // exactly that load, through a cache of exactly `FRAMES` frames, and hands
    // back what it cost — so the two sets of counters can be reconciled instead
    // of compared.
    // At `FRAMES`, not at `rv64_host::DEFAULT_FRAMES`: the frame count is the
    // one input that moves the cache counters without moving a single retired
    // instruction, so a reference booted at a different size would make the
    // reconciliation below measure the frame count rather than the guest.
    let reference = rv64_host::boot_capturing_frames(
        &kernel,
        &dtb,
        &initramfs,
        MAX_INSNS,
        SHELL_PROMPT,
        FRAMES,
    )
    .expect("the reference boot must load");
    eprintln!(
        "reference (rv64_host::run_until, warm cache): {} instructions, \
         misses={} writebacks={} mmu walks={}",
        reference.executed, reference.cache.misses, reference.cache.writebacks,
        reference.mmu_walks,
    );
    assert!(reference.reached_marker, "the reference boot never reached a prompt either");
    assert_eq!(
        report.executed, reference.executed,
        "the badge's run loop retired a different number of instructions than \
         `rv64_host::run_until` did for the same guest — the two loops disagree, \
         and any badge-vs-laptop comparison on hardware would be meaningless \
         until they do not"
    );
    assert_eq!(
        report.mmu_walks, reference.mmu_walks,
        "the same guest walked its page tables a different number of times"
    );

    // The reconciliation. `reference = badge + load - saved`, where `saved` is
    // the handful of pages that were still resident when the load finished and
    // that the guest read before they were evicted — hits for the reference,
    // misses for the badge. That term is bounded by `FRAMES` by construction:
    // at most `FRAMES` pages can be resident at the end of the load, and each
    // can save at most one miss.
    eprintln!(
        "load phase (what the host pays and the badge does not): \
         misses={} writebacks={} evictions={}",
        load.misses, load.writebacks, load.evictions,
    );
    let unexplained_misses = (report.cache.misses + load.misses) as i64 - reference.cache.misses as i64;
    eprintln!(
        "reconciliation: badge {} + load {} - reference {} = {} misses unaccounted for \
         (bounded by FRAMES = {FRAMES})",
        report.cache.misses, load.misses, reference.cache.misses, unexplained_misses,
    );
    assert!(
        (0..=FRAMES as i64).contains(&unexplained_misses),
        "the badge's page traffic is not the reference's minus the load phase: \
         badge={} load={} reference={}, leaving {unexplained_misses} misses that \
         nothing accounts for. Anything outside 0..={FRAMES} is not a warming \
         difference — the two runs are touching guest memory differently.",
        report.cache.misses, load.misses, reference.cache.misses,
    );

    // --- phase 2: type at the shell ---
    //
    // This is the one path nothing else in the tree covers end to end: a
    // `ConIn` frame on the wire -> `rv64_proto::Mux` -> `Link::take_console`
    // -> `Uart::push_input` -> the guest's 8250 driver. It is injected here
    // rather than by `rv64-host serve`, which has no stdin plumbing (a standing
    // TODO in `crates/rv64-host/src/main.rs`) — so what this proves is that the
    // badge half is complete and waiting for a host that sends the frames.
    let mut frame = Vec::new();
    encode(&Frame::ConIn(TYPED.as_bytes().to_vec()), &mut frame);
    inject.0.lock().unwrap().write_all(&frame).expect("injecting keystrokes");

    let before = machine.bus.uart.sink.inner().transcript.len();
    let typed_at = Instant::now();
    let mut scanned = before;
    let mut echoed = false;
    let mut typing_timed_out = false;
    let echo_cfg = Config { max_insns: MAX_INSNS, ..Config::default() };
    let echo_report = machine.run(&echo_cfg, |bus, _| {
        if console_reached(&bus.uart.sink.inner().transcript, TYPED_MARKER.as_bytes(), &mut scanned) {
            echoed = true;
            return false;
        }
        if typed_at.elapsed() >= Duration::from_secs(120) {
            typing_timed_out = true;
            return false;
        }
        true
    });

    let raw = &machine.bus.uart.sink.inner().transcript[before..];
    let after = strip_ansi(&String::from_utf8_lossy(raw));
    eprintln!("--- after typing {TYPED:?} ---\n{after}");
    print_screen(
        &machine.bus.uart.sink.inner().oled.frame().split('\n').map(str::to_string).collect::<Vec<_>>(),
    );

    // Three distinct failures, told apart, because they need different fixes
    // and the first two look the same from the transcript.
    assert!(
        !typing_timed_out,
        "the guest was still running 120 s after the keystrokes went out, having \
         printed {after:?}"
    );
    // **`echoed` is checked before `Capped`, and the order is the finding.**
    // A lost console byte means the marker never appears, so the run *always*
    // exhausts its budget and ends `Capped` -- which, with the asserts the other
    // way round, reported every byte-loss failure as "raise MAX_INSNS rather
    // than going looking for a console bug". That is the one failure this test
    // exists to catch, and the message sent the reader away from it.
    assert!(
        echoed,
        "the marker never arrived. Either the keystrokes never reached the guest \
         (the `ConIn` path), or the guest printed the line and bytes were lost \
         between its UART and the transcript. It printed {after:?}"
    );
    assert_ne!(
        echo_report.ending,
        Ending::Capped,
        "the instruction budget ran out. The marker *did* arrive, so this is a \
         budget problem and not a lost byte -- raise MAX_INSNS. It printed {after:?}"
    );

    // And the claim the marker's terminator buys: a *whole* line, exactly the
    // one the shell was asked to compute, with nothing eaten out of the middle
    // or the end of it. A pipeline dropping bytes fails here rather than
    // passing on a prefix.
    let lines: Vec<String> =
        after.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
    assert!(
        lines.iter().any(|l| l == "BADGE-INPUT-42"),
        "the guest never printed the computed line whole -- some of it was lost \
         between the UART and the transcript. It printed {lines:?}"
    );

    // --- shut the host down and confirm it had nothing to complain about ---
    drop(machine);
    badge_shutdown.shutdown(std::net::Shutdown::Both).ok();
    match host.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("the host side failed: {e}"),
        Err(_) => panic!("the host side panicked"),
    }
}
