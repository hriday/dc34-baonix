//! Badge probe: the numbers only hardware can answer, over USB serial.
//!
//! 1. USB-serial throughput for 4 KiB transfers, transmit-only;
//! 2. **sustained receive**, swept: bursts of 4 KiB pages streamed back to back at
//!    doubling sizes, reporting both a rate and -- the more important number -- the
//!    largest burst the badge can absorb before the USB server's message queue
//!    overruns. The emulator's transfer design turns on both, because it reads
//!    ahead and pipelines rather than stopping to wait for each page, and the
//!    ceiling is how far ahead it is allowed to read;
//! 3. **per-request round-trip latency**, printed next to the noise floor of the
//!    instrument that measured it;
//! 4. free physical pages (with its measurement bias stated in the line itself);
//! 5. how far a demand-paged `map_memory` region can be touched, and how far a
//!    *heap* allocation can climb -- the latter being the shape a Rust page cache
//!    would really take.
//!
//! ORDER IS DELIBERATE -- do not "tidy" it. All three throughput legs run before
//! the memory climbs, because they are the numbers the design hangs on and the
//! climbs are the part that takes the system down. Within throughput, TX runs
//! first because it needs no host cooperation, so it still produces a number when
//! `echo-host.py` was never started; the sweep runs before the latency rounds
//! because it is the figure the design turns on and the latency leg is the one
//! that can time out. The heap ceiling is now raised *before* all three, so the
//! throughput legs and the memory legs are measured on the same machine -- in
//! round 4 they were not. Within the climbs, the mapping climb runs first *because it
//! can be given back* and the heap climb runs last *because it cannot* -- see the
//! long comment at stage 5a. Both are capped so that only one of them is ever
//! outstanding, and the reclaim between them is measured rather than assumed.
//!
//! # Why receive is measured by a dedicated reader thread
//!
//! Round 3 reported `120 ms/rt` and it was **an artifact of this program**, not a
//! property of the badge. The old round-trip leg sent `REQ` and only then called
//! `serial_wait_binary()`. `Opcode::SerialHookBinary`
//! (`services/usb-bao1x/src/main.rs:683-686`) parks the listener and does **not**
//! drain `serial_buf`, so a reply that lands before the park is not delivered by
//! the park: it waits for the next event that inspects the buffer. With nothing
//! else arriving, that event is the watchdog flush. The host answers in
//! microseconds while the probe still has a syscall to make, so the reply
//! routinely lost that race and the flush period became the floor -- 100 ms, and
//! `120 ms/rt` is that floor plus the honest part.
//!
//! Both receive legs are therefore driven by a **reader thread that keeps a
//! listener parked essentially all the time** and stamps each delivery's arrival
//! from inside the delivery path. The main thread never waits on the wire; it
//! reads counters. Concretely:
//!
//! * the request is not sent until the reader has confirmed it is parked
//!   (`RX_PARKED`) plus a settle delay, so the reply cannot beat the park;
//! * each burst of the sweep sidesteps the race entirely rather than papering over
//!   it -- bytes keep arriving, so every delivery is driven by an IRQ that finds a
//!   listener already waiting, and the watchdog is never in the path;
//! * a listener lend that fails is *recorded*, not fatal. Round 4 died on one,
//!   reporting only the `.expect()` string that had replaced the real error. See
//!   `wait_binary()` for what the error actually was, why the fault is in the
//!   kernel rather than in `usb-bao1x`, and why the failed buffer must be forgotten
//!   rather than dropped;
//! * the latency leg reports the floor it could not remove: the 1 ms clock, the
//!   measured cost of one blocking IPC, and the residual watchdog period, together
//!   with how many rounds landed at or above it. A latency number that cannot
//!   resolve below its own instrument is worse than one that admits it.
//!
//! # How the host tells "panicked" from "wedged" from "never started"
//!
//! Every failure mode on this badge otherwise presents identically -- output stops
//! -- so three mechanisms are built in, and they are the reason this file is longer
//! than the measurements need:
//!
//! * **Never started.** Nothing at all on the CDC port, not even the `=== probe
//!   start` banner. The image did not boot, or the loader refused it, or the probe
//!   is not in the image.
//! * **Panicked.** The probe hooks the log server's USB mirror (`TryHookUsbMirror`)
//!   itself and **checks the answer** -- see `try_hook_panic_mirror`, and the
//!   `mirror:` line it prints. With the mirror up, the std panic path -- log-server
//!   opcodes 1000 and 1101..=1132 -- mirrors `PANIC in PID n:` plus the panic text
//!   to that same CDC port, so a userspace panic anywhere, *including the swapper's*
//!   `panic!("Ran out of swap space, hard OOM!")`, prints. If the transcript says
//!   `mirror: NOT HOOKED`, panics are invisible and only the heartbeat separates a
//!   panic from a wedge. The probe must never call `serial_clear_input_hooks()`,
//!   which unhooks the mirror; an earlier revision did, which is why every failure
//!   used to look like silence.
//!   (Kernel panics still go only to the physical debug UART, which is not brought
//!   out to CDC. A kernel panic looks like "wedged, heartbeat stopped".)
//! * **Wedged.** A background thread prints `..hb` every ~500 ms. Heartbeat still
//!   ticking with no new `##` stage line means a blocking call never returned;
//!   heartbeat stopped means the *process* is gone.
//!
//! Each risky step prints `## <stage>` *before* it runs. If the transcript ends on
//! a `##` line, that stage is what killed the run. This matters because neither
//! climb can report its own boundary as an error: `map_memory` only reserves PTEs,
//! the physical allocation happens on the touching store, and there failure is a
//! kernel `.expect("Couldn't allocate new page")` or a swapper `panic!`, never an
//! `Err` this process can print.
//!
//! The probe is an init process that ends in `terminate_process`, so it re-runs on
//! **every boot**. A garbled or missed capture costs a power cycle, not a flash.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const PAGE: usize = 4096;
const XFERS: usize = 2048; // 8 MiB of 4 KiB transfers
/// The byte `echo-host.py` writes at offset `i` of its 4096-byte answer page.
///
/// **Not a constant fill any more, and that change is the point.**
/// `echo-host.py` used to answer with `b"\xa5" * PAGE`, and a page of identical
/// bytes made a *duplicated packet byte-for-byte indistinguishable from a
/// correct one*. `usb-bao1x` was re-reading one shared 512-byte staging buffer
/// for several packets' worth of counts, and this probe's edge check --
/// `d[0] != FILL || d[last] != FILL` -- passed anyway, every time. "The probe
/// receives real data over this exact mechanism" was therefore never the
/// reassurance it looked like, and it pointed five rounds of debugging away
/// from the driver.
///
/// Mirrors `echo-host.py`'s `FILL`, which must stay in step with this.
fn fill_byte(i: usize) -> u8 { ((i * 7 + (i >> 8) * 131) & 0xFF) as u8 }

const PAGE_LEN: usize = 4096; // echo-host.py's answer page, repeated
const TX_FILL: u8 = 0xa5; // unused by the receive check; kept for reference

/// The receive sweep, in 4 KiB pages per burst. Each entry is one `STREAM <n>`
/// request, streamed back to back by the host and drained completely before the
/// next is asked for. This replaces round 4's single 1 MiB stream, which did not
/// measure a rate -- it killed the probe. See `wait_binary()` for why.
///
/// **The point of the sweep is the largest entry that completes with zero listener
/// errors.** That number, not the KiB/s beside it, is what the emulator's transfer
/// design has to be built around: it is how much the badge can absorb in one
/// uninterrupted push before the USB server's message queue overruns.
///
/// Where the ceiling should land, derived from source rather than guessed. The USB
/// interrupt handler posts one `IrqSerialRx` scalar per CDC packet
/// (`services/usb-bao1x/src/hw.rs:333-337`) of at most `SERIAL_MAX_PACKET_SIZE` =
/// 512 B (`hw.rs:29`). A Xous server's message queue is exactly one page
/// (`kernel/src/services.rs:1841-1844`) of `QueuedMessage`, whose largest variant
/// is 4 + 6*4 = 28 B plus a 4 B tag = 32 B, so 4096/32 = **128 slots**. If the
/// server drained nothing at all while a burst arrived, 128 * 512 = **64 KiB**
/// would fill the queue -- see `CDC_PACKET` and `SERVER_QUEUE_SLOTS`. It does
/// drain, concurrently, so the real ceiling is higher; how much higher is exactly
/// what no reading of the source can settle, and what this sweep is for.
///
/// Capped at 128 pages (512 KiB) deliberately. A burst that overruns is a burst
/// the probe stops draining for a few milliseconds at a time, and every undrained
/// byte sits in `serial_buf`, which is an unbounded `Vec` in the USB server
/// (`services/usb-bao1x/src/main.rs:154`). Round 4 already proved ~1 MiB of it is
/// survivable on this badge -- the probe died partway into a 1 MiB stream, nothing
/// drained the rest, and `usb-bao1x` was still serving its CDC node afterwards --
/// so 512 KiB has a measured margin under it rather than an assumed one.
const SWEEP_PAGES: [usize; 8] = [1, 2, 4, 8, 16, 32, 64, 128];
const STREAM_DEADLINE_MS: u64 = 20_000; // per burst
const STREAM_STALL_MS: u64 = 2_000; // no progress for this long => the host stopped

/// How long the reader waits after a failed listener lend before trying again.
/// Long enough for the USB server's main loop to pop queue slots, short enough
/// that `serial_buf` does not grow by much at 5.7 MiB/s (~59 KiB per 10 ms).
const RX_ERR_BACKOFF_MS: u64 = 10;
/// Ceiling on listener-lend failures for the whole run. Each failure costs one
/// 4 KiB page permanently -- the kernel lends the page to the USB server before it
/// discovers the queue is full, and nothing ever gives it back (see
/// `wait_binary()`) -- so on a machine with ~91 free pages this is a budget, not a
/// counter. Exceeding it abandons the sweep rather than the badge.
const RX_ERR_BUDGET: usize = 24;

/// `SERIAL_MAX_PACKET_SIZE` in `services/usb-bao1x/src/hw.rs:29` -- the most one
/// CDC read can hand the interrupt handler, and therefore the most one
/// `IrqSerialRx` message can carry.
const CDC_PACKET: usize = 512;
/// Slots in a Xous server's message queue. The kernel allocates it one page
/// (`kernel/src/services.rs:1841-1844`) of `QueuedMessage`, whose largest variant
/// is `u16 + u8 + u8 + 6 * usize` = 28 B plus a 4 B tag = 32 B on riscv32.
const SERVER_QUEUE_SLOTS: usize = 4096 / 32;

const ROUNDS: usize = 64; // 256 KiB of 4 KiB round trips
const RT_TIMEOUT_MS: u64 = 2000; // per round trip; a run with no host costs this once
const IPC_PROBE_N: u64 = 1000; // blocking-IPC calls timed to size the latency floor
const PARK_SETTLE_MS: u64 = 2; // head start the reader's park gets over the request

/// Watchdog period while nothing is being timed. `serial_wait_binary()` blocks
/// forever with no sender, so a flush on a period is the only thing that bounds a
/// blocked read; 100 ms is cheap and the legs that care set their own.
const FLUSH_IDLE_MS: usize = 100;
/// Watchdog period during the latency leg. This is the **residual floor** of that
/// measurement and it is printed in the transcript: a reply that lands in the
/// microseconds while the reader is between deliveries waits for a flush, and this
/// is how long that wait can be. It cannot go to zero -- with no watchdog at all a
/// reply lost that way would hang the round instead of costing it 5 ms -- so it is
/// reported rather than hidden.
const FLUSH_RT_MS: usize = 5;
const HB_EVERY: u32 = 5; // heartbeat once per 5 idle flushes, i.e. ~500 ms

const HEAP_MAX: usize = 8 * 1024 * 1024;
const HEAP_STEP: usize = 128 * 1024;
const MAP_STEP: usize = 256 * 1024;
const SRAM_KIB: usize = 2048; // HW_SRAM_MEM_LEN == 2097152

/// Usable swap, *derived* rather than guessed: `SWAP_RAM_LEN` is 8 MiB
/// (`libs/bao1x-api/src/offsets/baosec.rs:16`), and `derive_usable_swap`
/// (`loader/src/swap.rs:78`) subtracts the MAC table -- one 16-byte `Tag` per
/// 4096-byte page, rounded up to a page boundary: 8388608 - 32768 = 8355840 B.
/// This is the ceiling the climbs below actually run into, and it is a *system*
/// ceiling: it is shared with every other process's evicted pages.
const SWAP_USABLE_KIB: usize = 8160;

/// Where each climb stops. **This number is arbitrary.** Nothing in the source
/// derives it: `SWAP_USABLE_KIB` above is the only hard bound in sight, and it is
/// system-wide, so how much of it the kernel and the ten services that boot ahead
/// of us are already holding is exactly the thing no number here can know until a
/// transcript comes back. 6 MiB is a guess with headroom -- one climb at a time,
/// leaving ~2 MiB of the swap ceiling for everyone else. Because the climbs are
/// ordered and the first is reclaimed before the second starts, this cap is also
/// the *peak*, not half of it. If the transcript shows the boot set holding more
/// than that headroom, lower this; it costs a power cycle, not a flash.
const CLIMB_CAP: usize = 6 * 1024 * 1024;

/// Gates the heartbeat off during the three throughput legs, whose whole point is
/// to measure bytes-per-second on a channel nothing else is writing to -- and, for
/// the two receive legs, with nothing else competing for the USB server either.
static HB_ON: AtomicBool = AtomicBool::new(false);

/// The watchdog's period, in milliseconds, read fresh on every iteration so a leg
/// can tighten it for the duration of a measurement and hand it back afterwards.
static FLUSH_PERIOD_MS: AtomicUsize = AtomicUsize::new(FLUSH_IDLE_MS);

// ---- what the reader thread publishes, and the main thread only reads ----
//
// There is exactly one writer (the reader thread) and one reader (main), and main
// only ever looks while it is waiting on `RX_BYTES`, so `RX_BYTES` is the release
// point: every other field is stored *before* it and read *after* it.
//
// `AtomicUsize` and not `AtomicU64` for the millisecond stamps: riscv32imac has no
// 64-bit atomics, and 32 bits of milliseconds is 49 days of uptime on a badge that
// runs this probe once per boot.
static RX_BYTES: AtomicUsize = AtomicUsize::new(0);
static RX_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
/// Deliveries that did not match `echo-host.py`'s position-dependent page.
///
/// This used to be an O(1) check of the first and last byte against a constant
/// fill, on the reasoning that checking every byte would cost more than the
/// measurement could spare. That reasoning was wrong in the way that matters:
/// against a constant fill the check could not fail, so it bought nothing at
/// all, and it certified a receive path that was in fact duplicating packets.
///
/// The check is now every byte against `fill_byte(stream_offset + k)`. It costs
/// a few hundred nanoseconds per kilobyte and it can actually fail -- and a
/// check that cannot fail is not cheaper than one that can, it is worthless.
static RX_BAD: AtomicUsize = AtomicUsize::new(0);
static RX_FIRST_MS: AtomicUsize = AtomicUsize::new(0);
static RX_FIRST_LEN: AtomicUsize = AtomicUsize::new(0);
static RX_LAST_MS: AtomicUsize = AtomicUsize::new(0);
/// True while the reader is blocked in `serial_wait_binary()` -- or, for the few
/// microseconds between the store and the syscall, about to be. Requests are not
/// sent until this reads true, which is what keeps a reply from beating the park.
static RX_PARKED: AtomicBool = AtomicBool::new(false);
/// Listener lends that came back `Err` instead of a delivery, and the raw
/// `xous::Error` discriminant of the most recent one. Round 4 could report
/// neither: `serial_wait_binary()` throws the error away and panics on it. See
/// `wait_binary()`.
static RX_ERRS: AtomicUsize = AtomicUsize::new(0);
static RX_LAST_ERR: AtomicUsize = AtomicUsize::new(0);

/// Zero the receive counters. Safe to call only when nothing is arriving, which is
/// true at the head of each leg: the host says nothing unless it is asked.
fn arm_rx() {
    RX_DELIVERIES.store(0, Ordering::Relaxed);
    RX_BAD.store(0, Ordering::Relaxed);
    RX_FIRST_MS.store(0, Ordering::Relaxed);
    RX_FIRST_LEN.store(0, Ordering::Relaxed);
    RX_LAST_MS.store(0, Ordering::Relaxed);
    RX_BYTES.store(0, Ordering::Release);
}

/// Block until the reader thread is parked, then give the park a `PARK_SETTLE_MS`
/// head start over the request that follows. Returns false if the reader never
/// parked within a second, which the transcript counts rather than hides -- a round
/// sent without a confirmed park is a round that can hit the watchdog floor.
fn wait_parked(tt: &ticktimer::Ticktimer) -> bool {
    let t0 = tt.elapsed_ms();
    while !RX_PARKED.load(Ordering::Acquire) {
        if tt.elapsed_ms() - t0 > 1000 {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::thread::sleep(std::time::Duration::from_millis(PARK_SETTLE_MS));
    RX_PARKED.load(Ordering::Acquire)
}

/// The name of a `xous::Error` discriminant, for the few this path can produce.
///
/// `xous::Error` is an explicit-discriminant enum
/// (`xous-rs/src/definitions.rs:117-158`), so the reader thread can stash
/// `e as usize` in an atomic and the number can be turned back into a name here.
fn err_name(code: usize) -> &'static str {
    match code {
        0 => "NoError",
        1 => "BadAlignment",
        2 => "BadAddress",
        3 => "OutOfMemory",
        9 => "ServerNotFound",
        14 => "InternalError",
        15 => "ServerQueueFull",
        19 => "ShareViolation",
        _ => "other -- look it up in xous-rs/src/definitions.rs",
    }
}

/// The three lines of `usb_bao1x::UsbHid::serial_wait_binary()`, inlined, with the
/// error kept instead of discarded -- and with the failed `Buffer` defused instead
/// of dropped.
///
/// # Why this exists at all
///
/// Round 4 ended here, on hardware, with the badge showing
/// `usb-bao1x/src/lib.rs:243:14: Internal error`. That line is
/// `.expect("Internal error")` on `lend_mut`, and the line above it is
/// `.or(Err(xous::Error::InternalError))` -- so the real error was thrown away one
/// line before the panic that reported it. The message named the *mask*, not the
/// fault. These twelve lines are the same call with the mask removed.
///
/// # What the mask was hiding, and why it is not fixable in the USB server
///
/// A blocking memory message is a two-step operation in the kernel, and the steps
/// are not atomic. `send_message` lends the page into the server first
/// (`kernel/src/syscall.rs:117-131`), and only then tries to queue the message
/// (`:288`). If the server's queue is full, `queue_message` returns
/// `ServerQueueFull` (`kernel/src/server.rs:908-931`), which
/// `SysCall::SendMessage` turns into `retry_syscall` -- *re-executing the whole
/// instruction* (`kernel/src/syscall.rs:1013-1014`). **The lend is never undone.**
/// So the retry runs `lend_memory` on a page whose PTE now has `VALID` cleared and
/// `SHARED` set, and `ensure_page_exists_inner` rejects exactly that shape:
/// `flags == 0 || (flags & S) != 0` => `Err(BadAddress)`
/// (`kernel/src/arch/riscv/mem.rs:1047-1049`). That is the error under the mask,
/// and it is permanent for this buffer -- retrying the same one can only reproduce
/// it.
///
/// Two consequences this function has to handle, neither of which the library call
/// can:
///
/// * **The page is gone.** It is mapped into the USB server's message window and
///   no queued message references it, so nothing will ever return it. `Buffer`'s
///   `Drop` would call `unmap_memory(...).expect("Buffer: failed to drop memory")`
///   on it (`xous-ipc/src/buffer.rs:359-364`) -- a *second* panic on top of the
///   first. `core::mem::forget` is therefore not a leak being tolerated, it is the
///   only correct move: the page is already lost, and the choice is between losing
///   it quietly and losing it plus the process.
/// * **It is not the USB server's doing and not the USB server's to fix.**
///   `usb-bao1x` stays alive throughout -- after round 4's crash its CDC node was
///   still enumerated on the host. The defect is in the kernel. The only thing an
///   application can do about it is not fill the queue, and how much traffic that
///   allows is a measurement, which is what `SWEEP_PAGES` is.
fn wait_binary(conn: xous::CID) -> Result<Vec<u8>, xous::Error> {
    use num_traits::ToPrimitive;
    let req = usb_bao1x::UsbSerialBinary { d: Vec::new() };
    let mut buf = xous_ipc::Buffer::into_buf(req).or(Err(xous::Error::InternalError))?;
    if let Err(e) = buf.lend_mut(conn, usb_bao1x::Opcode::SerialHookBinary.to_u32().unwrap()) {
        core::mem::forget(buf);
        return Err(e);
    }
    buf.to_original::<usb_bao1x::UsbSerialBinary, _>().map(|r| r.d)
}

/// KiB/s from a byte count and a millisecond window, with the window floored at one
/// tick so a sub-millisecond window cannot divide by zero. A window of 0 means the
/// figure is below the clock's resolution, and every caller says so in its line.
fn kib_s(bytes: usize, ms: u64) -> u64 { (bytes as u64) * 1000 / (ms.max(1) * 1024) }

/// Wait until nothing has arrived for half a second, and report what was swallowed.
///
/// This runs between the two receive legs and it is not tidiness. If the streaming
/// leg gives up on its deadline, the host is still writing the pages it was asked
/// for; those bytes would satisfy the latency leg's first requests the instant they
/// were sent, and the latency figure would be a fiction built out of the previous
/// leg's backlog. Returns the byte count and whether quiet was actually reached
/// within `cap_ms` -- a false is printed rather than swallowed, because the leg
/// that follows is then measuring something other than what it says.
fn drain_quiet(tt: &ticktimer::Ticktimer, cap_ms: u64) -> (usize, bool) {
    let start = tt.elapsed_ms();
    arm_rx();
    let (mut seen, mut last_change) = (0usize, start);
    loop {
        let got = RX_BYTES.load(Ordering::Acquire);
        let now = tt.elapsed_ms();
        if got != seen {
            seen = got;
            last_change = now;
        }
        if now - last_change > 500 {
            return (seen, true);
        }
        if now - start > cap_ms {
            return (seen, false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `serial_send` truncates at SERIAL_BINARY_BUFLEN (3840) and returns only the
/// prefix it accepted, so every send loops. `Ok(0)` means USB is not configured --
/// the service replies rather than dropping the message, so this cannot block.
fn send_all(usb: &usb_bao1x::UsbHid, b: &[u8]) -> usize {
    let mut sent = 0;
    while sent < b.len() {
        match usb.serial_send(&b[sent..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => sent += n,
        }
    }
    sent
}

/// Ask the log server to mirror console output -- panics included -- to the USB
/// CDC port, and *return whether it took*.
///
/// This is the single point of failure for diagnosability. The std panic path
/// (log-server opcodes 1000 and 1101..=1132) reaches the operator only through
/// this mirror; without it every panic is the silence a previous round was spent
/// eliminating. So it is checked.
///
/// It is deliberately **not** requested through `serial_console_input_injection()`,
/// which an earlier revision called. That path cannot report: the client side is a
/// *non-blocking* scalar (`services/usb-bao1x/src/lib.rs:256`), and the usb-bao1x
/// handler that does the real work discards the log server's answer into
/// `log::error!` (`services/usb-bao1x/src/main.rs:686-710`), which goes to the
/// physical debug UART that is not brought out on this badge. The answer exists;
/// it is only unreachable that way.
///
/// Asking the log server directly reaches it. `TryHookUsbMirror` (opcode 4,
/// `api/xous-api-log/src/api.rs:44`) is a *blocking* scalar returning 1 when the
/// mirror is established and 0 when it could not connect to the USB driver
/// (`services/xous-log/src/main.rs:250-288`). It is the identical request
/// usb-bao1x forwards, minus the discarded answer and minus the `ConsoleListener`
/// mode flip -- which was never wanted here: in *both* `ConsoleListener` and
/// `NoListener` the IRQ path clears `serial_buf` (`main.rs:596`, `:606`), so
/// nothing about arriving bytes changes before the priming `serial_wait_binary()`
/// in `main`.
///
/// `None` means the log server did not answer in the expected shape at all.
fn try_hook_panic_mirror() -> Option<usize> {
    let sid = xous::SID::from_bytes(b"xous-log-server ")?;
    let conn = xous::connect(sid).ok()?;
    match xous::send_message(
        conn,
        xous::Message::new_blocking_scalar(
            log_server::api::Opcode::TryHookUsbMirror as usize,
            0,
            0,
            0,
            0,
        ),
    ) {
        Ok(xous::Result::Scalar1(v)) => Some(v),
        _ => None,
    }
}

/// Force a garbage collection in the swapper and read back the free physical page
/// count -- **out of the field the swapper actually answers in**.
///
/// `xous_swapper::Swapper::garbage_collect_pages()` cannot return this number to
/// anyone, on any kernel, and the defect is one destructure. The swapper's handler
/// answers by mutating the scalar body in place -- `scalar.arg1 = free_pages`
/// (`services/xous-swapper/src/main.rs:795`) -- and `reply_and_receive_next` sends
/// that body back whole, as `Result::Scalar5(id, arg1, arg2, arg3, arg4)`: the
/// client library packs `[id, arg1, arg2, arg3, arg4]`
/// (`xous-rs/src/definitions/messages/mod.rs:106`, `xous-rs/src/syscall.rs:1856`)
/// and the kernel returns them in that order (`kernel/src/syscall.rs:562`). The
/// client then reads the **first** field (`services/xous-swapper/src/lib.rs:106`),
/// which is the message id -- and `Opcode::GarbageCollect` is `0`. So the helper
/// returns a constant 0 to every caller, always, which is exactly what round 3's
/// transcript showed while the swapper's own mirrored log said 91 and 265.
///
/// Reading `arg1` instead is the whole fix, and it is an app-side one: the swapper
/// on the badge is already answering correctly. This is worth reporting upstream
/// (`services/xous-swapper/src/lib.rs:100-110`) but needs no patch here, and no
/// reflash.
///
/// `None` -- rather than the old `0` -- for an IPC failure, which retires the
/// other half of the previous round's caveat: a printed `0` now means zero free
/// pages and nothing else.
///
/// The bias is unchanged and is still stated in every line that prints this: there
/// is no app-callable pure query (the kernel's `GetFreePages` is SWAPPER_PID-gated),
/// and the swapper sets `pages_to_free = n.max(HARD_OOM_PAGE_TARGET * 2)`, so asking
/// for 0 still *forces 48 pages out to SPI RAM* before counting.
fn gc_free_pages(conn: xous::CID, pages: usize) -> Option<usize> {
    match xous::send_message(
        conn,
        xous::Message::new_blocking_scalar(
            xous_swapper::Opcode::GarbageCollect as usize,
            pages,
            0,
            0,
            0,
        ),
    ) {
        Ok(xous::Result::Scalar5(_id, free_pages, _, _, _)) => Some(free_pages),
        _ => None,
    }
}

/// Render a free-page count for a report line, keeping "no answer" distinct from
/// "no memory" -- the distinction the old `0` return could not make.
fn pages_str(v: Option<usize>) -> String {
    match v {
        Some(n) => format!("{} ({} KiB)", n, n * 4),
        None => "IPC-ERROR (no answer from the swapper)".to_string(),
    }
}

fn main() -> ! {
    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    std::thread::sleep(std::time::Duration::from_secs(5)); // let a console attach

    let usb = usb_bao1x::UsbHid::new();
    let say = |s: String| {
        send_all(&usb, s.as_bytes());
    };

    // The banner goes out BEFORE the mirror hook, on purpose: `serial_send` does
    // not depend on the mirror, and the hook is the one blocking call this early.
    // A transcript with a banner and nothing after it therefore points at the hook.
    say(format!("\r\n=== probe start pid={:?} ===\r\n", xous::process::id()));

    // Hook the log server's USB mirror, and say out loud whether it took. Three
    // attempts because a 0 can mean the USB driver simply was not ready yet.
    let mut mirror = None;
    for _ in 0..3 {
        mirror = try_hook_panic_mirror();
        if mirror == Some(1) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    say(match mirror {
        Some(1) => "mirror: HOOKED -- panics will print on this port as 'PANIC in PID n:'.\r\n"
            .to_string(),
        Some(v) => format!(
            "mirror: NOT HOOKED (log server answered {}) -- FLYING BLIND: a userspace panic \
             will NOT print here. Everything below still runs; tell a panic from a wedge by \
             the heartbeat instead (a panic takes the process, so '..hb' stops too).\r\n",
            v
        ),
        None => "mirror: NOT HOOKED (no answer from the log server) -- FLYING BLIND: a \
                 userspace panic will NOT print here. Tell a panic from a wedge by the \
                 heartbeat instead (a panic takes the process, so '..hb' stops too).\r\n"
            .to_string(),
    });
    say("legend: '## x' = entered stage x. A transcript ending on a '##' line means \
         that stage killed the run.\r\n        '..hb' every ~500 ms = process alive. \
         No hb and no progress = process gone.\r\n        'PANIC in PID n:' = a \
         userspace panic -- but only if the 'mirror:' line above says HOOKED.\r\n        \
         The three throughput legs run with the heartbeat OFF on purpose -- they are \
         timing this channel -- so expect silence for the length of each one, up to \
         20 s per burst of the rx sweep.\r\n"
        .to_string());

    // The watchdog serves two jobs. (a) `serial_wait_binary()` blocks forever with
    // no sender; `serial_flush()` on a period turns each blocked read into a
    // bounded one returning whatever has arrived. (b) The heartbeat. Both need a
    // UsbHid of their own because UsbHid is not Sync; `UsbHid::new()` is just a
    // name-server lookup, so a second one is cheap.
    //
    // The period is read fresh each time round, because it is not a constant of the
    // program any more: it is the residual noise floor of the latency leg, and that
    // leg tightens it for its own duration. Sleeping the *current* period rather
    // than a fixed one means a change takes effect after at most one old period.
    //
    // This is safe only against a `usb-bao1x` carrying
    // `badge/usb-bao1x-serialflush-repair.patch`.
    // Stock, the flush handler's binary branch does `copy_from_slice` into the
    // client's empty Vec -- it panics whenever there is anything to deliver -- and
    // `continue`s past the listener release when the device is not Configured, so
    // it cannot break a blocked read out on a disconnect either.
    std::thread::spawn(|| {
        let flusher = usb_bao1x::UsbHid::new();
        let mut n: u32 = 0;
        loop {
            let period = FLUSH_PERIOD_MS.load(Ordering::Relaxed) as u64;
            std::thread::sleep(std::time::Duration::from_millis(period));
            flusher.serial_flush().ok();
            n = n.wrapping_add(1);
            if n % HB_EVERY == 0 && HB_ON.load(Ordering::Relaxed) {
                send_all(&flusher, b"..hb\r\n");
            }
        }
    });

    // The reader. This thread exists so that a listener is parked before any
    // request leaves the badge and stays parked for as long as bytes keep coming --
    // see the header comment. It also owns the arrival clock: the stamp is taken
    // the instant the delivery returns, inside the delivery path, so no polling
    // interval on the main thread can get into a receive number.
    //
    // Priming the binary listen mode is this thread's first act rather than a
    // separate call on `usb`. Hooking the console above put the service in
    // `ConsoleListener`, and in that mode the IRQ path injects arriving bytes as
    // keystrokes and then does `serial_buf.clear()` -- they are gone. The first
    // `wait_binary()` switches the mode to `BinaryListener` (`SerialHookBinary`
    // sets the mode; only the *listener* is consumed by a delivery), and from then
    // on bytes queue even with no listener parked.
    //
    // It calls `wait_binary()` and not `UsbHid::serial_wait_binary()`, and it takes
    // a raw CID rather than a `UsbHid`, for the reason set out at length on
    // `wait_binary`: the library call panics on a `lend_mut` error after discarding
    // which error it was, and that panic is what ended round 4. Here an error is a
    // *reading*. The thread counts it, records its discriminant, backs off long
    // enough for the USB server to pop queue slots, and re-parks. Nothing else in
    // the probe changes behaviour, and every leg below still runs.
    //
    // The back-off is the one thing that has to be gentle in both directions. Too
    // short and it burns pages (one per failure, permanently) against a queue that
    // has not drained yet; too long and `serial_buf` in the USB server grows by
    // ~59 KiB per 10 ms of not draining. `RX_ERR_BACKOFF_MS` is set where those two
    // costs are both small, and `RX_ERR_BUDGET` stops the run before either becomes
    // large.
    //
    // The server name is spelled out because `SERVER_NAME_USB_DEVICE` is
    // `pub(crate)` in `services/usb-bao1x/src/api.rs:4` -- reachable by
    // `UsbHid::new()`, which is what it is for, and by nobody else. This connection
    // is the same one `UsbHid::new()` would have made.
    const USB_SERVER_NAME: &str = "_Xous USB device driver_";
    let rx_conn = xous_api_names::XousNames::new()
        .unwrap()
        .request_connection_blocking(USB_SERVER_NAME)
        .expect("couldn't connect to the USB device server");
    std::thread::spawn(move || {
        let rtt = ticktimer::Ticktimer::new().unwrap();
        loop {
            RX_PARKED.store(true, Ordering::Release);
            let d = match wait_binary(rx_conn) {
                Ok(d) => d,
                Err(e) => {
                    RX_PARKED.store(false, Ordering::Release);
                    RX_LAST_ERR.store(e as usize, Ordering::Relaxed);
                    // Release, and last: `RX_ERRS` is what the sweep reads to decide
                    // a burst overran, so the discriminant above must already be
                    // visible when the new count is.
                    RX_ERRS.fetch_add(1, Ordering::Release);
                    std::thread::sleep(std::time::Duration::from_millis(RX_ERR_BACKOFF_MS));
                    continue;
                }
            };
            RX_PARKED.store(false, Ordering::Release);
            if d.is_empty() {
                continue; // a watchdog flush with nothing buffered; re-park at once
            }
            let now = rtt.elapsed_ms() as usize;
            // The absolute offset of this delivery within the stream, which is
            // `echo-host.py`'s page repeated. Read before the counter is
            // advanced below.
            let off = RX_BYTES.load(Ordering::Relaxed);
            if d.iter().enumerate().any(|(k, &b)| b != fill_byte((off + k) % PAGE_LEN)) {
                RX_BAD.fetch_add(1, Ordering::Relaxed);
            }
            RX_DELIVERIES.fetch_add(1, Ordering::Relaxed);
            RX_LAST_MS.store(now, Ordering::Relaxed);
            if RX_BYTES.load(Ordering::Relaxed) == 0 {
                RX_FIRST_MS.store(now, Ordering::Relaxed);
                RX_FIRST_LEN.store(d.len(), Ordering::Relaxed);
            }
            // Last, and with Release: this is what main waits on, so everything
            // above must already be visible when the new count is.
            RX_BYTES.fetch_add(d.len(), Ordering::Release);
        }
    });

    // Do not proceed until that first hook has actually happened; the mode flip is
    // what makes arriving bytes queue instead of being eaten by the console.
    let tt = ticktimer::Ticktimer::new().unwrap();
    if !wait_parked(&tt) {
        say("rx: WARNING -- the reader thread never reported itself parked. Both \
             receive legs below will run anyway, but a listener that is not parked \
             is the exact condition that produced round 3's 120 ms artifact.\r\n"
            .to_string());
    }

    // Raise the heap ceiling HERE, before anything is measured, and not in the heap
    // stage where it used to live.
    //
    // It is not what killed round 4 -- the receive path allocates its IPC pages with
    // `map_memory`, which draws on `MemoryType::Default`, a separate 256 MiB window
    // that `HeapMaximum` does not govern (`kernel/src/mem.rs:449-467`), and the
    // reader drops each delivery as soon as it has counted it, so nothing
    // accumulates. But round 4 ran its whole receive leg with the process ceiling
    // still at its 512 KiB default and only raised it afterwards, which meant the
    // receive numbers and the memory numbers were taken on two different machines.
    // Raising it first costs nothing (`AdjustProcessLimit` sets a limit; it
    // allocates no pages) and removes the difference.
    //
    // AdjustProcessLimit is compare-and-set: the first call is a deliberate no-op
    // read, the second one writes. The default is 512 KiB unless the kernel carries
    // `big-heap`, and this read is the only way to know which.
    say("## heap: raise ceiling (moved ahead of the throughput legs in fix round 5)\r\n".to_string());
    let hm = xous::Limits::HeapMaximum as usize;
    if let Ok(xous::Result::Scalar2(_, cur)) =
        xous::rsyscall(xous::SysCall::AdjustProcessLimit(hm, 0, HEAP_MAX))
    {
        let after = match xous::rsyscall(xous::SysCall::AdjustProcessLimit(hm, cur, HEAP_MAX)) {
            Ok(xous::Result::Scalar2(_, a)) => a,
            _ => cur,
        };
        say(format!("heap_max: {} -> {} bytes (requested {})\r\n", cur, after, HEAP_MAX));
    } else {
        say("heap_max: AdjustProcessLimit read FAILED; ceiling unknown\r\n".to_string());
    }

    // ---- 1. transmit-only throughput. `accepted` proves a host was draining. ----
    say("## tx: 8 MiB of 4 KiB writes\r\n".to_string());
    let page = [0x5au8; PAGE];
    let start = tt.elapsed_ms();
    let mut accepted = 0usize;
    for _ in 0..XFERS {
        accepted += send_all(&usb, &page);
    }
    let ms = (tt.elapsed_ms() - start).max(1);
    say(format!(
        "\r\nxfer: {} x {}B in {} ms, {} B accepted => {} KiB/s\r\n",
        XFERS,
        PAGE,
        ms,
        accepted,
        (accepted as u64) * 1000 / (ms * 1024)
    ));

    // Both receive legs are entered only if the transmit leg proved a host is
    // draining the port. With nothing attached, `accepted` is 0 and there is no one
    // to answer, so the waits below would buy nothing but their own deadlines.
    if accepted == 0 {
        say("rx: SKIPPED (sweep and latency both) -- transmit accepted 0 bytes, so \
             nothing is draining the port. Plug the badge into a host and start \
             badge/echo-host.py (or badge/reattach.sh).\r\n"
            .to_string());
    } else {
        // ---- 2. sustained receive, swept for the burst the badge can absorb. ----
        //
        // Round 4 asked for 1 MiB in one push and the probe died partway through it.
        // The autopsy is on `wait_binary()`; the short form is that the USB
        // interrupt handler posts one message per 512 B CDC packet into a server
        // message queue that is 128 slots long, and when a client's blocking memory
        // send finds that queue full the kernel lends the client's page away, fails
        // to queue, and then retries the whole instruction against a page it has
        // already lent -- which can only fail. Sustained receive on this badge is
        // therefore not one number, it is two: a rate, and **the largest burst that
        // can be pushed before the queue overruns**. The second one is the one the
        // emulator's transfer design has to be built around, and it is the one round
        // 4 could not report because it was the thing that killed the run.
        //
        // So: a sweep. One `STREAM <n>` per entry in `SWEEP_PAGES`, each burst
        // streamed back to back by the host (this is still genuinely sustained
        // receive -- within a burst nothing waits for anything) and drained fully
        // before the next is asked for. A burst that completes with `RX_ERRS`
        // unchanged is a burst the badge absorbed. The first one that does not is
        // the ceiling, and the sweep stops there rather than climbing past it.
        //
        // The clock on each burst starts at its FIRST byte and stops at its LAST,
        // and the first delivery's bytes are subtracted from the numerator, so what
        // is divided is exactly the bytes that arrived *during* the window. That
        // excludes the request's turnaround by construction -- which is the point:
        // the turnaround is measured on its own, one leg further down. The wall
        // figure that does include it is printed beside it rather than instead of it.
        say(format!(
            "## rx-sweep: bursts of {:?} pages of {} B, each drained before the next \
             (silent for up to {} s per burst)\r\n",
            SWEEP_PAGES,
            PAGE,
            STREAM_DEADLINE_MS / 1000,
        ));
        say(format!(
            "rx-sweep: what this is looking for is the LARGEST burst with 0 lend \
             errors, not the KiB/s. Derived ceiling if the USB server drained nothing \
             while a burst arrived: {} queue slots x {} B per CDC packet = {} KiB. It \
             does drain concurrently, so the real figure should be higher; how much \
             higher is what no reading of the source could settle.\r\n",
            SERVER_QUEUE_SLOTS,
            CDC_PACKET,
            SERVER_QUEUE_SLOTS * CDC_PACKET / 1024,
        ));
        // The largest burst that completed whole with no listener error. Stays 0 if
        // even one page could not be received, which is a finding and not a blank.
        let mut absorbed = 0usize;
        let mut absorbed_kib_s = 0u64;
        let mut ceiling: Option<String> = None;
        for &pages in SWEEP_PAGES.iter() {
            if RX_ERRS.load(Ordering::Acquire) >= RX_ERR_BUDGET {
                ceiling = Some(format!(
                    "abandoned at {} KiB: the {}-failure budget was spent. Each failure \
                     costs a page that never comes back, so the sweep stops rather than \
                     the badge",
                    pages * PAGE / 1024,
                    RX_ERR_BUDGET,
                ));
                break;
            }
            let want = pages * PAGE;
            arm_rx();
            let err0 = RX_ERRS.load(Ordering::Acquire);
            let parked = wait_parked(&tt);
            let t_req = tt.elapsed_ms();
            send_all(&usb, format!("STREAM {}\n", pages).as_bytes());
            let (mut seen, mut last_change) = (0usize, t_req);
            loop {
                let got = RX_BYTES.load(Ordering::Acquire);
                if got >= want {
                    break;
                }
                let now = tt.elapsed_ms();
                if got != seen {
                    seen = got;
                    last_change = now;
                }
                // Three exits, and they mean different things in the line below. The
                // deadline means the burst is slower than its own 20 s cap. The stall
                // means the host stopped talking (an echo-host.py older than fix round
                // 4 does not know the word STREAM and never starts). A new lend error
                // means the queue overran, which is the thing being measured, and
                // waiting out the remaining deadline after it would only burn more
                // pages against a queue that is already full.
                if RX_ERRS.load(Ordering::Acquire) != err0 {
                    break;
                }
                if now - t_req > STREAM_DEADLINE_MS || now - last_change > STREAM_STALL_MS {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let got = RX_BYTES.load(Ordering::Acquire);
            let errs = RX_ERRS.load(Ordering::Acquire) - err0;
            let first_ms = RX_FIRST_MS.load(Ordering::Relaxed) as u64;
            let last_ms = RX_LAST_MS.load(Ordering::Relaxed) as u64;
            let first_len = RX_FIRST_LEN.load(Ordering::Relaxed);
            let deliveries = RX_DELIVERIES.load(Ordering::Relaxed);
            let bad = RX_BAD.load(Ordering::Relaxed);
            let window = last_ms.saturating_sub(first_ms);
            let steady = got - first_len.min(got);
            say(format!(
                "rx-burst {} KiB: {}/{} B, {} deliveries ({} B mean, the service hands \
                 over at most 3840 B at a time), {} ms steady window => {} KiB/s{}; wall \
                 {} ms request-to-last-byte => {} KiB/s including the turnaround; {} \
                 lend errors{}; {} bad edge bytes; parked before the request: {}\r\n",
                want / 1024,
                got,
                want,
                deliveries,
                got / deliveries.max(1),
                window,
                kib_s(steady, window),
                // The small end of the sweep is below the instrument. A 4 KiB burst
                // is two deliveries and lands inside one tick of a 1 ms clock, so its
                // steady-state rate is a floor, not a measurement -- the sweep wants
                // those sizes for the error column, not the rate column.
                if window == 0 {
                    " -- WINDOW BELOW THE 1 ms CLOCK, this rate is a lower bound only"
                } else {
                    ""
                },
                last_ms.saturating_sub(t_req),
                kib_s(got, last_ms.saturating_sub(t_req)),
                errs,
                if errs > 0 {
                    format!(
                        " (last: {})",
                        err_name(RX_LAST_ERR.load(Ordering::Relaxed)),
                    )
                } else {
                    String::new()
                },
                bad,
                parked,
            ));
            if errs == 0 && got >= want {
                absorbed = want;
                absorbed_kib_s = kib_s(steady, window);
                continue;
            }
            ceiling = Some(if errs > 0 {
                format!(
                    "{} KiB overran the USB server's message queue after {} of {} B \
                     arrived -- {} lend failure(s), last one {}. This is the boundary",
                    want / 1024,
                    got,
                    want,
                    errs,
                    err_name(RX_LAST_ERR.load(Ordering::Relaxed)),
                )
            } else if got == 0 {
                format!(
                    "{} KiB got NO DATA -- the host never answered `STREAM`. An \
                     echo-host.py older than fix round 4 does not know the word; update \
                     it. This is not a badge finding",
                    want / 1024,
                )
            } else {
                format!(
                    "{} KiB truncated at {} B with no lend error -- the host stopped or \
                     the deadline ran out, which is a host or link finding, not a queue \
                     one",
                    want / 1024,
                    got,
                )
            });
            break;
        }
        say(match &ceiling {
            Some(why) if absorbed > 0 => format!(
                "rx-sweep: LARGEST BURST ABSORBED = {} KiB at {} KiB/s sustained. Next \
                 step up {}. Treat {} KiB as the emulator's receive window: a transfer \
                 design that pushes more than that without waiting for the badge to \
                 drain will hit the same fault, and the fault takes the receiving \
                 process, not the USB server.\r\n",
                absorbed / 1024,
                absorbed_kib_s,
                why,
                absorbed / 1024,
            ),
            Some(why) => format!(
                "rx-sweep: NOTHING ABSORBED -- the smallest burst already failed: {}.\r\n",
                why,
            ),
            None => format!(
                "rx-sweep: LARGEST BURST ABSORBED = {} KiB at {} KiB/s sustained, and \
                 that is the top of the sweep, NOT a boundary -- every size tried \
                 succeeded. The ceiling is above {} KiB and this run did not find it; \
                 extend SWEEP_PAGES to go looking.\r\n",
                absorbed / 1024,
                absorbed_kib_s,
                absorbed / 1024,
            ),
        });

        // Let the wire go quiet before timing anything per-request. After a sweep
        // that stopped on a ceiling the host is still writing the burst it was
        // asked for, and those bytes would satisfy the latency leg's first requests
        // the instant they were sent -- the latency figure would be a fiction built
        // out of this leg's backlog. The watchdog is tightened for the duration
        // because a backlog with no host traffic behind it is delivered only by a
        // flush, and at the idle period that is 3840 B per 100 ms.
        FLUSH_PERIOD_MS.store(FLUSH_RT_MS, Ordering::Relaxed);
        let (swallowed, quiet) = drain_quiet(&tt, 30_000);
        FLUSH_PERIOD_MS.store(FLUSH_IDLE_MS, Ordering::Relaxed);
        if swallowed > 0 || !quiet {
            say(format!(
                "rx-drain: swallowed {} B of trailing burst before the latency \
                 leg{}\r\n",
                swallowed,
                if quiet {
                    ""
                } else {
                    " -- STILL ARRIVING after 30 s, so every rt figure below is \
                     measuring the backlog, not a round trip"
                },
            ));
        }

        // ---- 3. per-request round-trip latency, with its own noise floor. ----
        //
        // What round 3 got wrong and this leg fixes: the request now goes out only
        // after the reader has confirmed it is parked, so the reply cannot land
        // before there is a listener to take it. The watchdog period is tightened
        // for the duration, because it is what a reply pays if it lands anyway --
        // and both the tightened period and the count of rounds that could have
        // paid it are printed, so the figure can be read against its own floor.
        FLUSH_PERIOD_MS.store(FLUSH_RT_MS, Ordering::Relaxed);

        // The instrument's own cost, measured rather than asserted. `elapsed_ms()`
        // is itself a blocking scalar to the ticktimer server, so timing a run of
        // them prices one blocking IPC round trip -- the irreducible unit every
        // figure below is built out of. A round trip contains at least four: the
        // send, the delivery to the reader, and the two clock reads that bracket it.
        let ipc0 = tt.elapsed_ms();
        for _ in 0..IPC_PROBE_N {
            core::hint::black_box(tt.elapsed_ms());
        }
        let ipc_us = (tt.elapsed_ms() - ipc0) * 1000 / IPC_PROBE_N;

        say(format!("## rt: {} x 4 KiB round trips, one request at a time\r\n", ROUNDS));
        // One 4 KiB reply is 8 CDC packets, i.e. 8 of the 128 queue slots the sweep
        // above just found the edge of, so this leg is nowhere near it and never was
        // -- which is why round 4's 64 round trips completed while its 1 MiB stream
        // did not. Counted anyway: an unexpected error here would mean the ceiling is
        // far lower than the sweep said, and that is worth knowing.
        let rt_err0 = RX_ERRS.load(Ordering::Acquire);
        let (mut done, mut sum, mut min, mut max) = (0usize, 0u64, u64::MAX, 0u64);
        let (mut at_floor, mut unparked, mut rt_bad) = (0usize, 0usize, 0usize);
        for _ in 0..ROUNDS {
            arm_rx();
            if !wait_parked(&tt) {
                unparked += 1;
            }
            let t0 = tt.elapsed_ms();
            send_all(&usb, b"REQ\n");
            let mut ok = false;
            loop {
                if RX_BYTES.load(Ordering::Acquire) >= PAGE {
                    ok = true;
                    break;
                }
                if tt.elapsed_ms() - t0 > RT_TIMEOUT_MS {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if !ok {
                break; // short or silent: reported as a timeout below, never as a rate
            }
            // The arrival stamp comes from inside the delivery path, so the 1 ms
            // poll above sets how soon this loop *notices*, not what it measures.
            let lat = (RX_LAST_MS.load(Ordering::Relaxed) as u64).saturating_sub(t0);
            rt_bad += RX_BAD.load(Ordering::Relaxed);
            sum += lat;
            min = min.min(lat);
            max = max.max(lat);
            if lat >= FLUSH_RT_MS as u64 {
                at_floor += 1;
            }
            done += 1;
        }
        FLUSH_PERIOD_MS.store(FLUSH_IDLE_MS, Ordering::Relaxed);

        if done == 0 {
            say(format!(
                "rt: TIMEOUT -- no host answer to a single request in {} ms; start \
                 badge/echo-host.py\r\n",
                RT_TIMEOUT_MS,
            ));
        } else {
            let mean10 = sum * 10 / done as u64;
            say(format!(
                "rt: {}/{} x {}B, min {} / mean {}.{} / max {} ms per round trip => {} \
                 KiB/s at the mean{}\r\n",
                done,
                ROUNDS,
                PAGE,
                min,
                mean10 / 10,
                mean10 % 10,
                max,
                kib_s(PAGE * done, sum),
                if done < ROUNDS { " TIMEOUT-EARLY" } else { "" },
            ));
            say(format!(
                "rt-floor: clock {} ms; one blocking IPC {} us measured over {} calls, \
                 and a round trip contains at least four; watchdog residual {} ms with \
                 {}/{} rounds at or above it, {} rounds sent without a confirmed park, \
                 {} deliveries with a bad edge byte, {} listener lend errors{}. The \
                 host's own service time is inside every figure above and cannot be \
                 separated on this end -- echo-host.py prints its own turnaround.\r\n",
                1,
                ipc_us,
                IPC_PROBE_N,
                FLUSH_RT_MS,
                at_floor,
                done,
                unparked,
                rt_bad,
                RX_ERRS.load(Ordering::Acquire) - rt_err0,
                if RX_ERRS.load(Ordering::Acquire) > rt_err0 {
                    format!(" (last: {})", err_name(RX_LAST_ERR.load(Ordering::Relaxed)))
                } else {
                    String::new()
                },
            ));
        }
    }
    HB_ON.store(true, Ordering::Relaxed);

    // ---- 4. free physical pages, with its bias in the line. ----
    // There is no app-callable pure query: the kernel's GetFreePages is
    // SWAPPER_PID-gated. The swapper's GarbageCollect opcode is the only reachable
    // path and it is not a query -- the swapper sets
    // `pages_to_free = n.max(HARD_OOM_PAGE_TARGET * 2)`, so asking for 0 *forces 48
    // pages (192 KiB) out to SPI RAM* with interrupts masked, and only then reads
    // the free count. On a 2 MiB machine that is a large fraction, and the eviction
    // filter exempts only PID 1 and PID 2, so it can steal this process's own pages.
    //
    // The message is sent here rather than through
    // `xous_swapper::Swapper::garbage_collect_pages()`, which reads the wrong field
    // and can only ever return 0 -- see `gc_free_pages` for the whole derivation.
    // Round 3's `free_pages=0` was that bug, not this badge.
    //
    // It is called exactly twice, and the second call is not a "trend": it is the
    // *same call with the same bias* after the mapping climb has been reclaimed, so
    // the pair brackets the reclaim. Comparing two identically-biased readings is
    // the one thing this call can honestly support. Nothing between them allocates
    // except the climb being measured.
    say("## mem: baseline free-page read (forces a 48-page eviction)\r\n".to_string());
    let xns = xous_api_names::XousNames::new().unwrap();
    let swapper = xns.request_connection_blocking(xous_swapper::SWAPPER_PUBLIC_NAME).unwrap();
    let base_free = gc_free_pages(swapper, 0);
    say(format!(
        "free_pages={} of {} KiB SRAM, BASELINE -- BIASED: measured immediately after \
         a GarbageCollect(0) forced 48 pages out to swap; treat it as an upper bound \
         on what was free before the call, not a steady-state figure. A 0 here now \
         means zero free pages and nothing else -- an IPC failure prints as \
         IPC-ERROR.\r\n",
        pages_str(base_free),
        SRAM_KIB,
    ));

    // ---- 5a. demand-paged map_memory, touched, then RECLAIMED. ----
    //
    // WHY THIS RUNS FIRST, and why the heap climb runs last. An earlier revision had
    // it the other way round, and the two climbs together could touch 12 MiB against
    // 2048 KiB of SRAM and 8160 KiB of swap -- so the second climb frequently could
    // not happen at all, and the mapping numbers were being lost to an ordering
    // artifact rather than to any hardware limit. Two facts fix it:
    //
    //   * A `map_memory` region can be given back. A heap allocation cannot: dropping
    //     a `Vec` returns its pages to this process's allocator, not to the kernel,
    //     and nothing in this program can make the heap shrink again. So whatever the
    //     heap climb takes, it holds until the process exits -- which is why it has
    //     to be the LAST thing this probe does.
    //   * Giving a mapping back takes two steps, in this order and only this order.
    //     `unmap_page` (`kernel/src/mem.rs:774`) releases a physical page only if
    //     `virt_to_phys` resolves, and a swapped-out page does not resolve
    //     (`kernel/src/arch/riscv/mem.rs:647`): the PTE is simply zeroed, and nothing
    //     tells the swapper, so the swap slot stays marked used. `FLG_SWAP_USED` is
    //     cleared in exactly one place -- the swapper's read-back path
    //     (`services/xous-swapper/src/main.rs:517`). So each step below is *read back
    //     first*, which forces RetrievePage and frees the swap slot, and *unmapped
    //     second*, which frees the physical page. Unmapping a swapped-out region
    //     without reading it back leaks the swap it is sitting on.
    //
    // Reading back page by page and unmapping behind the read also keeps the reclaim
    // monotonic: a page that has been unmapped cannot be evicted again, so the
    // footprint only falls.
    //
    // The reclaim is then *verified* rather than trusted -- the free-page count is
    // read again with the same call and the same bias, and the heap climb prints both
    // numbers in its header. If the second is far below the first, the heap figure is
    // depressed by whatever this climb left behind, and the transcript says so rather
    // than leaving a reader to guess.
    //
    // What the number means: `MemoryFlags::RESERVE` is *ignored* on RISC-V (it is
    // read only in the ARM backend), so every `map_memory(None, None, ..)` here is
    // demand-paged whether or not the flag is passed -- there is no way to ask this
    // API for eager backing. `map_memory` reserves PTEs only; the touching store is
    // what forces the physical allocation, and there OOM arrives as a kernel
    // `.expect("Couldn't allocate new page")` or, on a swap kernel like this one, a
    // swapper `panic!("Ran out of swap space, hard OOM!")`. Neither is an `Err`,
    // which is why this climb prints per step instead of relying on a `FAIL` arm.
    say(format!(
        "## map: reserve {} KiB, touch it in {} KiB steps -- measured against BASELINE \
         free_pages={} above, i.e. the machine as the boot set left it, with nothing \
         allocated by this probe. RAM-limited only up to {} KiB of SRAM; past that this \
         is a SWAP-limited figure, served out of {} KiB of usable swap shared with every \
         other process.\r\n",
        CLIMB_CAP / 1024,
        MAP_STEP / 1024,
        pages_str(base_free),
        SRAM_KIB,
        SWAP_USABLE_KIB,
    ));
    match xous::map_memory(None, None, CLIMB_CAP, xous::MemoryFlags::R | xous::MemoryFlags::W) {
        Ok(range) => {
            // Raw pointer, not `as_slice_mut`: the reclaim below hands pieces of this
            // region back while the loop is still walking it, and volatile accesses
            // also keep the optimizer from eliding the touches that ARE the
            // measurement.
            let base = range.as_mut_ptr();
            say(format!("map: reserved {} KiB of VA (no physical pages yet)\r\n", CLIMB_CAP / 1024));
            let m0 = tt.elapsed_ms();
            let mut touched = 0usize;
            while touched < CLIMB_CAP {
                let end = touched + MAP_STEP;
                let mut i = touched;
                while i < end {
                    unsafe { core::ptr::write_volatile(base.add(i), 0xa5) };
                    i += PAGE;
                }
                touched = end;
                say(format!(
                    "map: {} KiB touched, {} ms elapsed (next step may panic the kernel or \
                     the swapper; if this is the last map line, {} KiB is the boundary)\r\n",
                    touched / 1024,
                    tt.elapsed_ms() - m0,
                    touched / 1024,
                ));
            }
            say(format!(
                "map: reached the {} KiB cap without failing. The cap is a CHOICE, not a \
                 boundary -- the derived ceiling is {} KiB of usable swap plus {} KiB of \
                 SRAM, shared with the whole system.\r\n",
                CLIMB_CAP / 1024,
                SWAP_USABLE_KIB,
                SRAM_KIB,
            ));

            // Reclaim: read back (frees the swap slot), then unmap (frees the page).
            say("## map: reclaim -- read each page back, then unmap it\r\n".to_string());
            let r0 = tt.elapsed_ms();
            let (mut sum, mut off, mut reclaimed) = (0u64, 0usize, 0usize);
            while off < CLIMB_CAP {
                let end = off + MAP_STEP;
                let mut i = off;
                while i < end {
                    sum += unsafe { core::ptr::read_volatile(base.add(i)) } as u64;
                    i += PAGE;
                }
                // Sub-range unmap is legitimate: UnmapMemory just walks the range a
                // page at a time (`kernel/src/syscall.rs:793`); there is no
                // whole-allocation bookkeeping to violate.
                if let Ok(sub) = unsafe { xous::MemoryRange::new(base as usize + off, MAP_STEP) } {
                    if xous::unmap_memory(sub).is_ok() {
                        reclaimed += MAP_STEP;
                    }
                }
                off = end;
            }
            // Every page was stamped 0xa5 on the way up and most of them made a round
            // trip through encrypted swap to get here, so this is also the only
            // integrity check we get on that path -- for free.
            let want = (CLIMB_CAP / PAGE) as u64 * 0xa5;
            say(format!(
                "map: reclaimed {} KiB of {} KiB in {} ms; readback checksum {} of {} -- {}\r\n",
                reclaimed / 1024,
                CLIMB_CAP / 1024,
                tt.elapsed_ms() - r0,
                sum,
                want,
                if sum == want {
                    "the swap round trip preserved every marker byte"
                } else {
                    "MISMATCH: swap did not return what was written"
                },
            ));
        }
        // Reachable only for a VA-reservation refusal -- out of the 256 MiB Default
        // window, or a bad size. Physical exhaustion never lands here.
        Err(e) => say(format!("map: reserve of {} KiB FAILED {:?}\r\n", CLIMB_CAP / 1024, e)),
    }

    // ---- 5b. the heap ceiling, which is what a Rust page cache would hit. ----
    // Last, because it is the one climb whose memory cannot be given back.
    say("## mem: post-reclaim free-page read (same call, same bias, so it is comparable)\r\n".to_string());
    let after_free = gc_free_pages(swapper, 0);
    say(format!(
        "free_pages={} POST-RECLAIM vs {} BASELINE -- a figure at or above the baseline \
         means the mapping climb was fully given back and the heap climb below starts \
         from the same machine the map climb did. Well below it means the reclaim did \
         not take, and every heap number below is depressed by the difference.\r\n",
        pages_str(after_free),
        pages_str(base_free),
    ));

    // The ceiling was raised at the top of the run, before the throughput legs --
    // see the `## heap: raise ceiling` stage there for the numbers. Nothing between
    // there and here changes it, and raising it allocates nothing, so the climb
    // below starts from the same machine the mapping climb did.

    // The heap ceiling governs `MemoryType::Heap` only -- `IncreaseHeap` and the Heap
    // arm of `find_virtual_address`. It does NOT govern `map_memory`, which allocates
    // from `MemoryType::Default` (a separate 256 MiB virtual window). So this climb,
    // not the mapping climb above, is the one that measures the limit an emulator
    // page cache built out of `Vec`/`Box` would actually meet.
    //
    // Heap exhaustion is an allocation-error abort, not an `Err`, so the last `heap:`
    // line printed is the boundary.
    say(format!(
        "## heap: climb to {} KiB in {} KiB steps -- measured against POST-RECLAIM \
         free_pages={} (baseline was {}). RAM-limited only up to {} KiB of SRAM; past \
         that this is a SWAP-limited figure, out of {} KiB of usable swap shared with \
         every other process.\r\n",
        CLIMB_CAP / 1024,
        HEAP_STEP / 1024,
        pages_str(after_free),
        pages_str(base_free),
        SRAM_KIB,
        SWAP_USABLE_KIB,
    ));
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut heap_total = 0usize;
    let h0 = tt.elapsed_ms();
    while heap_total + HEAP_STEP <= CLIMB_CAP {
        let mut v = vec![0u8; HEAP_STEP];
        for i in (0..HEAP_STEP).step_by(PAGE) {
            v[i] = 0xa5;
        }
        held.push(v);
        heap_total += HEAP_STEP;
        say(format!(
            "heap: {} KiB held, +{} KiB step, {} ms elapsed (next step may abort; if this \
             is the last heap line, {} KiB is the boundary)\r\n",
            heap_total / 1024,
            HEAP_STEP / 1024,
            tt.elapsed_ms() - h0,
            heap_total / 1024,
        ));
    }
    say(format!(
        "heap: reached the {} KiB cap without failing. The cap is a CHOICE, not a \
         boundary -- the derived ceiling is {} KiB of usable swap plus {} KiB of SRAM, \
         shared with the whole system. Nothing is freed after this line: heap pages \
         cannot be returned to the kernel, which is why this climb runs last.\r\n",
        CLIMB_CAP / 1024,
        SWAP_USABLE_KIB,
        SRAM_KIB,
    ));
    drop(held);

    say("=== probe done ===\r\n".to_string());
    HB_ON.store(false, Ordering::Relaxed);
    xous::terminate_process(0)
}
