//! `UsbHost` — guest memory that lives on the laptop and arrives a page at a
//! time over USB-CDC.
//!
//! This is the third implementation of [`rv64::backing::MemBacking`], after
//! `FakeBacking` and `rv64_host::HostFile`, and it passes the same conformance
//! suite. `read_page` becomes a `ReadReq`/`ReadResp` exchange with
//! `rv64-host serve` at the other end of the cable; `write_page` becomes
//! `WriteReq`/`WriteAck`.
//!
//! # Park before you send. This is the whole ballgame.
//!
//! `Opcode::SerialHookBinary` (`services/usb-bao1x/src/main.rs:683-686`) does
//! exactly two things: it sets `serial_listen_mode = BinaryListener` and takes
//! the listener message. **It does not drain `serial_buf`.** Bytes that arrived
//! before the park sit in the buffer and are handed over only by the next event
//! that inspects it — an `IrqSerialRx` (`:592-670`, whose no-listener branch is
//! literally `// do nothing, keep queuing data...`) or a `SerialFlush`
//! (`:731`).
//!
//! Against a strictly synchronous peer there is no next event. The host says
//! nothing until it is asked again, and we are the ones who would ask. So a
//! reply that beats the park is not late, it is *lost*, and the read blocks
//! forever.
//!
//! This project already paid for that lesson. The probe's round 3 sent `REQ`
//! and only then called `serial_wait_binary()`; it reported `120 ms/rt`, which
//! was not the link but the 100 ms watchdog-flush period — the reply lost the
//! race essentially every round. Round 5 fixed it by (a) a reader thread that
//! keeps a listener parked, (b) confirming the park before the request goes
//! out, and (c) a short flush watchdog as a backstop, and the number went to
//! **2 ms**. That is the figure this module's design is justified by, so this
//! module has to earn it the same way.
//!
//! Hence [`Transport::arm`], which every [`UsbHost::exchange`] calls **before**
//! a single byte of the request leaves the badge, and which on hardware blocks
//! until the reader thread has confirmed itself parked plus a settle delay.
//! `park_is_confirmed_before_any_request_byte_is_sent` in the tests fails if
//! that ordering is ever inverted, because the inverted version does not fail
//! anywhere else — it hangs, silently, with no console left to say so.
//!
//! The listen mode must also be *primed*. It starts at `NoListener`, whose IRQ
//! branch logs the bytes and calls `serial_buf.clear()` — they are gone
//! (`main.rs:597-604`) — and `serial_console_input_injection()` leaves it at
//! `ConsoleListener`, which injects arriving bytes into the keyboard server and
//! then clears (`:606-618`). Only a `wait_binary()` flips it to
//! `BinaryListener`. [`UsbTransport::new`] therefore does not return until its
//! reader thread has parked once.
//!
//! # The exchange is strictly synchronous — for requests
//!
//! One request outstanding at a time. No pipelining, no outstanding-request
//! table, no read-ahead, no batching. [`rv64_proto::Mux::take_matching`] matches
//! a held frame by **type byte**, because the protocol carries no request id, so
//! two reads in flight have nothing to correlate them; the page-number check in
//! `exchange` turns that into a detected [`Error::Medium`] instead of silent
//! guest-memory corruption, but detecting it is not supporting it.
//!
//! The page check **rejects** a response for another page; it does not fault on
//! one. Those are different things and the difference is a boot: because only
//! one request is ever outstanding, a response for a page nobody asked for is
//! provably an answer to an exchange that has already finished, so `exchange`
//! drops it and keeps waiting rather than ending the run. See
//! `LinkInner::late_dropped`, and §36 of the task report for the run that made
//! the distinction expensive.
//!
//! That doctrine governs **requests**, and it is worth being precise about the
//! distinction, because an earlier draft of this module conflated the two and
//! the conflation is what produced the hang above. The *receive park* is not a
//! request. A parked listener is one message occupying one queue slot; it adds
//! no outstanding request and no queue-depth exposure. Keeping a listener
//! parked from a reader thread is therefore not a violation of the doctrine —
//! it is what the doctrine requires in order to work at all.
//!
//! # The badge's USB stack, as measured on hardware
//!
//! Numbers from `../probe`, run on a real badge; API facts quoted verbatim with
//! file and line in `../../docs/xous-api-notes.md`.
//!
//! * **The wire round trip costs 2 ms** (min 2 / mean 2.0 / max 3 over 64
//!   samples), with a parked-ahead reader.
//!
//!   Read that figure carefully, because this module is easy to build in a way
//!   that invalidates it. The probe takes `t0` *after* `wait_parked` and stamps
//!   arrival from inside the reader's delivery path, so the park settle and the
//!   1 ms notice poll are both deliberately measured **out** of the 2 ms. They
//!   are not free; they were excluded. Put either of them on the critical path
//!   of a page fault — a fixed 2 ms settle before every request, a 1 ms sleep
//!   before noticing every delivery — and a page fault costs 4–5 ms, which is
//!   *at or past* the badge's own swap at ~4 ms/page, and the reason for having
//!   this module at all evaporates.
//!
//!   So neither is on the path. [`confirm_park`] spins on the parked flag and
//!   pays only a scheduler turn when the reader is already parked, and `recv`
//!   spins before it sleeps. What a page fault costs in the best case is
//!   therefore the wire round trip plus a few scheduler turns — but **that has
//!   not been measured on hardware**, and until it has, no tethered-vs-swap
//!   ratio should be quoted from this file.
//! * **`serial_send` truncates at [`SERIAL_BINARY_BUFLEN`] (3840)** and returns
//!   the accepted prefix, so every send loops — see [`send_all`].
//! * **`serial_wait_binary()` hands over at most 3840 bytes per delivery**, so a
//!   page response always takes several deliveries. [`UsbHost`] owns one [`Mux`]
//!   for the whole link, for the same reason `rv64_host::serve` owns one per
//!   connection.
//! * **The USB server's message queue is 128 slots and overrunning it is
//!   fatal.** See below.
//!
//! # The 128-slot budget
//!
//! `usb-bao1x` posts one message per 512-byte CDC packet into a server queue
//! that is a single 4096-byte page of 32-byte slots — [`SERVER_QUEUE_SLOTS`] =
//! 128. Overrunning it is not a dropped packet: the kernel's blocking send lends
//! the client page *before* queuing and does not undo the lend when
//! `queue_message` returns `ServerQueueFull`, so the automatic retry re-runs
//! `lend_memory` on a page whose PTE now has `VALID` cleared and `SHARED` set
//! and fails permanently with `BadAddress`. An unbounded 1 MiB stream killed the
//! probe this way.
//!
//! What bounds it is the synchronous exchange: one outstanding request means one
//! page-sized frame in flight, and [`MAX_FRAME`] (4109 bytes) is
//! [`MAX_FRAME_PACKETS`] (9) slots of 128. The `static_assertions` below make
//! that a compile-time fact; `MemoryLoopback` in the tests tracks the peak queue
//! depth across a whole conformance run and asserts it is *exactly* nine.
//!
//! # The USB panic mirror and this transport can coexist — hook it directly
//!
//! An earlier revision of this file claimed the opposite, and said so in the
//! docs and the README. **It was wrong**, it was quoted into another task's
//! brief, and it nearly cost a hardware cycle. This section is the correction,
//! checked against the source rather than reasoned from the listen-mode state
//! machine.
//!
//! **The mirror is orthogonal to the listen mode.** `TryHookUsbMirror` is log
//! server opcode 4 (`services/xous-log/src/main.rs:250-288`). It connects to
//! `_Xous USB device driver_` by name and stores the resulting CID in the *log
//! server's own* `usb_serial` slot. It never reads or writes `usb-bao1x`'s
//! `serial_listen_mode`. Only `UnhookUsbMirror` (opcode 5, `:289-293`) clears
//! it, by `usb_serial.take()`. So a hooked mirror survives any number of
//! `SerialHookBinary` re-parks, and this transport's reader cannot disturb it.
//!
//! **What must be avoided is `serial_console_input_injection()`, not the
//! mirror.** That call reaches `usb-bao1x`'s `SerialHookConsole` handler
//! (`services/usb-bao1x/src/main.rs:687-712`), which does *two* things: it
//! forwards `TryHookUsbMirror` to the log server — discarding the answer into a
//! `log::error!` that goes to a physical debug UART not brought out on this
//! badge — **and** it sets `serial_listen_mode = ConsoleListener`. The mode flip
//! is the harm. In `ConsoleListener` the IRQ path injects arriving bytes into
//! the keyboard server as keystrokes and then `serial_buf.clear()`s them, which
//! destroys page traffic. The mirror it also establishes is not the problem and
//! is worth having on its own.
//!
//! **So hook it directly, the way `../probe/src/main.rs` does** — a blocking
//! scalar to `xous-log-server ` opcode 4 — and *check the answer*: it returns 1
//! when the mirror is established and 0 when it could not reach the USB driver.
//! `try_hook_panic_mirror` there is the reference implementation, and the reason
//! it goes direct is exactly this: it wants the mirror without the mode flip.
//!
//! This is proven on hardware, not inferred. The probe printed
//! `mirror: HOOKED`, then parked binary listeners for the whole session, and the
//! swapper's `INFO:xous_swapper: Free pages after GC: 77` still arrived over CDC
//! *after* the round-trip leg. Both worked at once on the real badge.
//!
//! **The one real interaction is TX interleaving, and the transport absorbs
//! it.** Mirrored text goes out of the same CDC *transmit* endpoint as protocol
//! frames — badge to host — so what it corrupts is **requests**, not responses.
//! A request is [`MAX_FRAME`] (4109) bytes and therefore always takes at least
//! two `serial_send` calls, so a mirrored line can land between them and split
//! it. That is not a mis-decode: the host's `rv64_proto::Decoder` scans for
//! SYNC, rejects an implausible LEN or a bad CRC, drains two bytes and rescans,
//! and only ever returns a frame on a CRC32 match — a silent wrong-page delivery
//! would need a CRC32 collision. So the frame is **dropped**, and the host
//! simply has no request to answer.
//!
//! An earlier revision concluded from this that the fix was to "keep
//! steady-state log volume near zero". **That was not achievable and it was the
//! wrong answer.** The mirror lives in the log server, so it carries every
//! process's output, and the process that logs most is the swapper — which logs
//! under memory pressure, which is exactly and continuously what an emulator
//! paging 32 MiB of guest RAM through a small cache creates. The drop rate
//! therefore tracks the workload rather than being rare.
//!
//! The right answer is that a dropped frame is *transient*, so the exchange
//! retries it: see [`RETRY_BUDGET`] for the bounds and for why re-sending is
//! safe in a protocol with no request ids. What was a dead boot at a rate
//! proportional to paging is now a re-send, counted in [`Link::retries`].
//!
//! So: **hook the mirror.** Quieting `usb-bao1x` itself with
//! `UsbHid::set_log_level(LogLevel::Err)` is still worth doing — every avoided
//! line is an avoided round trip — but it is an optimisation now, not the thing
//! standing between the badge and a mystery crash. And at panic time the
//! interleaving costs nothing that matters, because the run is over, which is
//! precisely the moment the mirror is worth having.
//!
//! # What is reachable after `PageCache` takes the backing
//!
//! `rv64::PageCache` owns its backing privately and exposes no accessor, so once
//! a `UsbHost` is handed to `PageCache::new` it can never be borrowed again.
//! Three things still need to be reachable from the run loop: console bytes from
//! the guest's shell, the diagnostic behind an [`Error::Medium`], and a
//! non-blocking pump so keystrokes arrive when the page cache is warm and
//! nothing is faulting. All three hang off [`Link`], a cloneable handle taken
//! *before* the cache consumes the backing.
//!
//! ```no_run
//! # use badge_app::usbhost::{UsbHost, Transport};
//! # fn build<T: Transport>(transport: T) {
//! let backing = UsbHost::new(transport);
//! let link = backing.link();               // keep this; `backing` is about to vanish
//! let cache = rv64::PageCache::new(backing, 512);
//! // ... in the run loop, once per instruction slice:
//! link.pump().ok();                        // non-blocking; console arrives without a fault
//! for _b in link.take_console() { /* bus.uart.push_input(b) */ }
//! if let Some(f) = link.take_fault() { /* report it: this is why Medium happened */ }
//! # }
//! ```

use rv64::backing::{Error, MemBacking};
use rv64::PAGE;
use rv64_proto::{encode, Frame, Mux};
use std::cell::RefCell;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------------

/// The low byte of `rv64_proto::SYNC`, which is the **first byte of every frame
/// on the wire** (the sync word is little-endian). The receive fingerprint
/// checks against it: a delivery that begins a frame must start with this.
pub const SYNC_LO: u8 = rv64_proto::SYNC.to_le_bytes()[0];

/// `Frame::ReadResp`. `rv64_proto::Frame::type_byte` is `pub(crate)` over there,
/// so the type bytes are spelled out here rather than borrowed.
const TY_READ_RESP: u8 = 0x02;
/// `Frame::WriteAck`.
const TY_WRITE_ACK: u8 = 0x04;
/// `Frame::Err`.
const TY_ERR: u8 = 0x07;

/// Error code `rv64_host::serve` sends when it cannot read the requested page.
const ERR_READ: u16 = 1;
/// Error code `rv64_host::serve` sends when it cannot write the requested page.
const ERR_WRITE: u16 = 2;

/// The longest frame that can cross this link: `SYNC(2) + TYPE(1) + LEN(2) +
/// page(4) + PAGE(4096) + CRC32(4)`. A `ReadResp` and a `WriteReq` are both
/// exactly this size.
pub const MAX_FRAME: usize = 5 + 4 + PAGE + 4;

// ---------------------------------------------------------------------------
// Badge USB limits, measured on hardware
// ---------------------------------------------------------------------------

/// `usb_bao1x::api::SERIAL_BINARY_BUFLEN`, verbatim from
/// `services/usb-bao1x/src/api.rs:168`: *"save 256 bytes on the page for Rkyv
/// overhead"*. Both `serial_send` paths silently truncate to
/// `data.len().min(SERIAL_BINARY_BUFLEN)`, and `serial_wait_binary` delivers at
/// most this much per call.
pub const SERIAL_BINARY_BUFLEN: usize = 3840;

/// USB CDC bulk packet size. `usb-bao1x` posts one IPC message per packet.
pub const CDC_PACKET: usize = 512;

/// Slots in the USB server's message queue: one 4096-byte page of 32-byte slots.
/// Overrunning it is fatal to the sender — see the module docs.
pub const SERVER_QUEUE_SLOTS: usize = 4096 / 32;

/// Queue slots one maximum-size frame occupies while it is in flight.
pub const MAX_FRAME_PACKETS: usize = MAX_FRAME.div_ceil(CDC_PACKET);

/// How long an exchange has in total — across every retry — before it gives up
/// and reports the link dead.
///
/// The probe's `RT_TIMEOUT_MS`, and unchanged in meaning by the retry logic: it
/// is still the point at which this module stops believing in the link. A round
/// trip measured 2 ms, so this is three orders of magnitude of headroom — not a
/// performance knob, but the difference between a diagnosable fault and a badge
/// that stops with no output and no way to tell a dropped byte from a wedged
/// server.
pub const RECV_DEADLINE_MS: u64 = 2000;

/// How long one *attempt* waits before re-sending its request.
///
/// A frame that is going to arrive arrives in about 2 ms, so 250 ms is still two
/// orders of magnitude of headroom for a healthy link — but it is what decides
/// how long the guest is stalled by a **dropped request**, which is a routine
/// event rather than a broken link. See [`RETRY_BUDGET`].
pub const ATTEMPT_DEADLINE_MS: u64 = 250;

/// Re-sends allowed before an exchange gives up. Total attempts are this plus
/// one, and the whole sequence is capped by [`RECV_DEADLINE_MS`] as well.
///
/// # Why a timed-out exchange is retried rather than reported
///
/// A dropped frame is transient; the link is fine. And frames *will* drop, for a
/// reason this module creates itself: mirrored log output shares the CDC
/// transmit endpoint with protocol frames (see the panic-mirror section above),
/// and a request is [`MAX_FRAME`]-sized, so a mirrored line landing between its
/// two `serial_send` calls splits it. The host's decoder resyncs and drops the
/// split frame — correctly, it never mis-reads one — and then simply has no
/// request to answer.
///
/// The process that logs most is the swapper, and it logs under memory
/// pressure, which is precisely what an emulator paging 32 MiB of guest RAM
/// through a small cache generates continuously. Without a retry the first such
/// drop is an [`Error::Medium`], which is a guest load/store fault, which in
/// Linux is a dead boot — at a rate that tracks the workload, presenting as an
/// emulator bug rather than a dropped frame.
///
/// # Why re-sending is safe without request ids
///
/// This is the one place where the protocol's lack of a request id could bite,
/// and the reason it does not is the synchronous-request doctrine doing work:
///
/// 1. **Every attempt carries the same request.** One request is outstanding at
///    a time, so a retry sequence is not two requests — it is one request sent
///    more than once, with the same type and the same page number.
/// 2. **The page check therefore cannot be fooled.** `exchange` accepts a
///    response only when its page equals the page asked; all attempts asked the
///    same page, so any response that passes answers all of them.
/// 3. **A duplicate answer is byte-identical, not merely acceptable.** For a
///    read, the host re-reads the same offset of the same file, and nothing can
///    have changed it in between: the only writer is us, and we cannot have a
///    write outstanding while a read is. For a write, `WriteAck` carries no data
///    and the host applied the same bytes to the same offset twice — idempotent.
/// 4. **The leftover duplicate cannot satisfy a *later* exchange.** That is the
///    real hazard — a stale `ReadResp` for page 7 answering a *subsequent* read
///    of page 7 after an intervening write would return pre-write data — and it
///    is closed twice over. `discard_stale_responses` purges held responses
///    before every attempt sends, because with nothing outstanding at that
///    moment any held response is stale by construction. And the CDC stream is
///    ordered, so a duplicate written before a later exchange's answer must have
///    been *delivered* before it too; a completed exchange is proof that
///    everything queued ahead of its answer has already been drained.
///
/// Point 4's second half is the load-bearing one and it depends on stream
/// ordering. If this transport ever grows a path that reorders deliveries, this
/// argument must be rebuilt — and the honest fix at that point is a request id
/// in `rv64-proto`, not a cleverer check here.
pub const RETRY_BUDGET: usize = 3;

/// Discarded bytes the badge keeps as a sample, for [`Fault::discarded`].
///
/// # Why the badge carries noise capture at all
///
/// `rv64_proto::Decoder::capturing_noise`'s own docs say the badge must not
/// have it, and that was right when the buffer was [`rv64_proto::MAX_NOISE`]
/// (64 KiB) on a machine the probe measured at 308 KiB free. It is wrong at 64
/// bytes.
///
/// The fourth hardware run produced: four complete replies received
/// (16,436 bytes = 4 x 4109 exactly), delivery order preserved, reader healthy
/// on every counter, and **no frame decoded**. Those cannot all be true, and
/// every remaining hypothesis -- rejected on CRC, mangled upstream, never
/// reaching the decoder, or our own request looping back -- is settled by
/// looking at what the decoder threw away. Reasoning about it had already cost
/// a cycle.
///
/// Sixty-four bytes is a frame header plus enough payload to recognise, and the
/// *count* of everything discarded stays exact whatever the cap is. That is a
/// permanent 64-byte cost for a permanent answer to "the decoder dropped
/// something and will not say what", which three cycles of invisible failures
/// have more than paid for.
pub const NOISE_SAMPLE: usize = 64;

/// Consecutive `Ok(0)` results from the send sink that [`send_all`] will absorb
/// before declaring the link stalled.
///
/// `Ok(0)` is *not* only "USB not configured".
/// `Opcode::SerialSendDataBlocking` (`services/usb-bao1x/src/main.rs:805-840`)
/// writes in 512-byte chunks and breaks at the first short write, returning
/// `total_sent`; `serial_write_irq_safe` is `usbd_serial::SerialPort::write`,
/// whose TX buffer is 1024 bytes (`hw.rs:78`) and which returns
/// `Err(WouldBlock)` when full. A full buffer on the *first* chunk gives
/// `total_sent == 0`. That is the host pausing for a moment — and
/// `rv64-host serve` does real file I/O between reads, so it will pause — not
/// the device being unplugged.
///
/// The API cannot distinguish the two, so this bounds the retry instead of
/// betting on either extreme. Each attempt is a blocking IPC round trip to the
/// USB server, so the retries are self-pacing at roughly the cost of one IPC
/// each; no sleep is needed and none is taken, which is what keeps [`send_all`]
/// platform-free and testable.
pub const SEND_RETRY_BUDGET: usize = 64;

/// Wall-clock milliseconds one [`Transport::send`] may spend before it gives up
/// and names itself.
///
/// # Why an attempt count was not a bound
///
/// [`SEND_RETRY_BUDGET`] counts **consecutive** `Ok(0)`s, and any accepted byte
/// resets it. That is an attempt count, not a clock, and the two come apart
/// exactly where this link fails:
///
/// * a request is 13 bytes — **one** `write_some` call at [`TX_PACKET`], which
///   cannot loop at all, so the read path never exercises the budget;
/// * a writeback is 4109 bytes — **nine** calls, each of which may be refused
///   up to 64 times and each acceptance of which resets the counter. A sink
///   that dribbles one byte per call, or that alternates accept/refuse, spends
///   an unbounded number of blocking IPC round trips with no clock ever
///   consulted.
///
/// So §31b's "no attempt begins, and no request is transmitted, after the
/// deadline has passed" had a hole underneath it precisely one frame kind wide,
/// and it is the frame kind that had never worked. This closes it: the send
/// loop itself gives up, with `sent`/`len` attached, and the exchange reports a
/// named fault instead of running on.
///
/// 500 ms is two orders of magnitude above a healthy paced writeback (nine
/// packets, ~8 ms at the badge's `TX_PACE_MS == 1`) and well inside
/// [`RECV_DEADLINE_MS`], so it can only fire on a transmit path that has
/// genuinely stopped moving. The worst case of one exchange is now
/// `RECV_DEADLINE_MS` plus one `arm` plus one send — bounded, and every term
/// named.
///
/// What it still cannot bound is a `serial_send` that never *returns*: that is
/// a blocking syscall, and §31a's rule applies unchanged — a thread inside the
/// kernel cannot time itself out. See [`Transport::send`].
pub const SEND_DEADLINE_MS: u64 = 500;

/// Compile-time proof that one outstanding page response fits inside the USB
/// server's queue with room to spare. If someone later grows `PAGE` or adds
/// pipelining depth, this is where it stops.
mod static_assertions {
    use super::{MAX_FRAME_PACKETS, SERVER_QUEUE_SLOTS};
    const _: () = assert!(
        MAX_FRAME_PACKETS <= SERVER_QUEUE_SLOTS,
        "one frame must fit in the USB server's 128-slot queue"
    );
    /// Eight-to-one headroom, so console frames and a retry cannot creep the
    /// steady-state depth up to the cliff.
    const _: () = assert!(
        MAX_FRAME_PACKETS * 8 <= SERVER_QUEUE_SLOTS,
        "the in-flight budget has lost its safety margin: no pipelining here"
    );
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The byte pipe underneath. Split out so the logic is testable off-hardware.
///
/// The error type really is `()`. Clippy objects, and it is right in general —
/// but the one error a caller could act on is a `xous::Error`, which does not
/// exist off-hardware and cannot appear in a trait the laptop tests implement.
/// [`Transport::take_platform_fault`] is how the real error escapes, as text,
/// and [`Link::take_fault`] is where the run loop reads it.
#[allow(clippy::result_unit_err)]
pub trait Transport {
    /// Park a receive listener and do not return until it is **confirmed**
    /// parked.
    ///
    /// [`UsbHost::exchange`] calls this before every send, and the ordering is
    /// load-bearing rather than tidy: see the module docs. An implementation
    /// with nothing to park may return `Ok(())`, but it must never return `Ok`
    /// on the strength of having *started* a park.
    fn arm(&mut self) -> Result<(), ()>;

    /// Deliver all of `bytes`. The 3840-byte truncation is the implementation's
    /// problem, not the caller's — see [`send_all`].
    ///
    /// The error says *which* failure, because the two are diagnosed
    /// differently and the likelier one is the one a human will hit: an
    /// unplugged cable is 65 consecutive `Ok(0)`s, i.e. [`SendError::Stalled`],
    /// and reporting that as a generic failure is the diagnostic gap this trait
    /// exists to close.
    ///
    /// **An implementation must return.** It is allowed to be slow and it is
    /// not allowed to be unbounded: the badge's answer is
    /// [`SEND_DEADLINE_MS`], checked by [`send_paced`] between packets, and it
    /// exists because this is the one step of an exchange that used to have an
    /// attempt budget and no clock. The residue — a platform `send` that never
    /// returns because the thread is inside a blocking syscall — cannot be
    /// bounded here or anywhere else in this crate; see §31a.
    fn send(&mut self, bytes: &[u8]) -> Result<(), SendError>;

    /// Wait for the next delivery and return it, which on the badge is at most
    /// [`SERIAL_BINARY_BUFLEN`] bytes.
    ///
    /// May return empty: a flush that finds nothing buffered delivers nothing,
    /// and that means "not yet", not "the cable is gone". Implementations must
    /// block or sleep — `exchange`'s loop is otherwise a busy-wait — and must
    /// return within a bound comfortably under [`RECV_DEADLINE_MS`].
    fn recv(&mut self) -> Result<Vec<u8>, ()>;

    /// Whatever has already arrived, without waiting at all.
    ///
    /// This is what lets console input reach the guest while the page cache is
    /// warm and nothing is faulting — see [`Link::pump`]. Defaults to nothing,
    /// for transports with no asynchronous arrival.
    fn poll(&mut self) -> Result<Vec<u8>, ()> {
        Ok(Vec::new())
    }

    /// Monotonic milliseconds, for [`RECV_DEADLINE_MS`].
    fn now_ms(&mut self) -> u64;

    /// The last platform-level error, as text, cleared by the call.
    ///
    /// On the badge this is the real `xous::Error` that
    /// `usb_bao1x::UsbHid::serial_wait_binary` would have flattened to
    /// `InternalError` and `.expect()`ed. Defaults to nothing.
    fn take_platform_fault(&mut self) -> Option<String> {
        None
    }

    /// The receive path's current state, **whether or not anything is wrong**.
    ///
    /// [`Transport::take_platform_fault`] reports errors, and reports each one
    /// once. That is right for an error and wrong for a diagnosis: after the
    /// fourth hardware run the question was "is the reader parked, has it died,
    /// how often is the watchdog un-parking it" — none of which is an error, and
    /// all of which decide what a timeout means. A healthy answer here is as
    /// useful as an unhealthy one, because it rules three things out.
    ///
    /// Called on every recorded fault, and never cleared.
    fn status(&mut self) -> Option<String> {
        None
    }
}

/// Why a [`Transport::send`] failed.
///
/// The platform-free half of [`SendFault`]: same distinction, with the
/// platform's own error dropped, because the trait's callers cannot name a
/// `xous::Error` and do not need to — [`Transport::take_platform_fault`]
/// carries the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The sink accepted nothing for [`SEND_RETRY_BUDGET`] consecutive
    /// attempts. On the badge this is the cable, or a host that has stopped
    /// draining for longer than this link waits.
    Stalled,
    /// The sink returned an error.
    Failed,
    /// The send did not finish inside [`SEND_DEADLINE_MS`], carrying how far it
    /// got. Distinct from [`SendError::Stalled`] because they are different
    /// bugs: `Stalled` is a sink that refuses everything, this is a sink that
    /// keeps accepting and never drains — the shape a multi-packet writeback
    /// fails in, and the one an attempt count cannot see.
    TimedOut { sent: usize, len: usize },
}

/// Why a [`send_all`] call gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFault<E> {
    /// The sink accepted zero bytes [`SEND_RETRY_BUDGET`] times in a row. Either
    /// USB is not configured or the host has stopped draining for longer than
    /// this link is willing to wait.
    Stalled,
    /// The sink returned an error, carried out whole rather than flattened.
    Link(E),
    /// The caller's `out_of_time` predicate said so before every byte was
    /// handed over. See [`SEND_DEADLINE_MS`] for why an attempt count was not
    /// enough.
    TimedOut { sent: usize, len: usize },
}

/// One CDC bulk packet, and the most this link hands the USB server in one
/// call. See [`send_paced`].
///
/// The same number `rv64_host::serve::CDC_PACKET` uses on the host, and for the
/// same reason: the packet is the unit the badge's USB hardware moves, so it is
/// the unit a sender has to think in.
pub const TX_PACKET: usize = 512;

/// Drives a truncating, partial-accepting writer until every byte of `bytes` has
/// been handed over.
///
/// A free function taking a closure, rather than a method on [`UsbTransport`],
/// because it is the piece of the hardware path most likely to be wrong and the
/// piece a laptop can test. The tests drive it against a sink that truncates at
/// exactly 3840 bytes the way `serial_send` does, and against one that stalls
/// transiently the way a full TX buffer does.
///
/// Equivalent to [`send_paced`] with no cap, no gap and no clock.
pub fn send_all<E, F>(write_some: F, bytes: &[u8]) -> Result<(), SendFault<E>>
where
    F: FnMut(&[u8]) -> Result<usize, E>,
{
    send_paced(write_some, || {}, || false, usize::MAX, bytes)
}

/// [`send_all`], with the transmit side paced: at most `chunk` bytes per call,
/// and `gap` run between calls.
///
/// # Why the badge cannot hand over a page in one go
///
/// The badge→host direction has the mirror of the receive defect that cost
/// §23–§25. `CorigineWrapper::write` copies each IN packet into a *single*
/// 512-byte hardware buffer — `get_app_buf_ptr` computes
/// `new_index = enq + mps` and resets to 0 whenever `new_index + mps` exceeds
/// `CRG_UDC_APP_BUF_LEN`, and with `mps == CRG_UDC_APP_BUF_LEN == 512` that is
/// *every* call — and then enqueues a transfer pointing at it **without any
/// check that the previous transfer has completed**. `usbd-serial`'s `flush`
/// emits one packet per `SerialPort::write`, and `usb-bao1x`'s
/// `SerialSendDataBlocking` calls `SerialPort::write` once per 512-byte chunk
/// in a tight loop. So a 4109-byte frame handed over in one call is nine
/// packets queued back-to-back into one buffer, each overwriting the last.
///
/// Everything this link has ever successfully sent fits in **one** packet: a
/// 13-byte `ReadReq`, and console lines short enough not to split. The first
/// transmit that does not is the first writeback — which is why `writebacks=0`
/// in every hardware run to date, and why the boot dies at its first dirty
/// eviction with the page's own bytes arriving at the host as unframed noise.
///
/// This is the transmit-side twin of `rv64_host::serve::Pace`, which tests the
/// same property in the other direction, and it is deliberately shaped the same
/// way: hand over one packet, wait, hand over the next.
///
/// # Why the policy is here and not in the platform leaf
///
/// Because the leaf is `#[cfg(target_os = "xous")]` and three rounds of this
/// task put a hardware-shaped defect somewhere no laptop could see it. The leaf
/// keeps only the two things a laptop has no version of — the syscall and the
/// sleep.
///
/// `gap` runs *between* calls, never before the first and never after the last,
/// so a frame that fits in one packet — every request this link sends — pays
/// nothing at all.
///
/// # Why it also takes a clock
///
/// `out_of_time` is checked at the top of every iteration, and it is the only
/// bound on this loop that is a *clock*. See [`SEND_DEADLINE_MS`]: the retry
/// budget counts consecutive refusals and is reset by any accepted byte, so it
/// bounds a sink that refuses everything and does not bound a sink that keeps
/// accepting and never drains. A `ReadReq` is one call and cannot loop; a
/// writeback is nine, which is exactly why the write path could outrun the
/// exchange deadline while the read path never did.
pub fn send_paced<E, F, G, D>(
    mut write_some: F,
    mut gap: G,
    mut out_of_time: D,
    chunk: usize,
    bytes: &[u8],
) -> Result<(), SendFault<E>>
where
    F: FnMut(&[u8]) -> Result<usize, E>,
    G: FnMut(),
    D: FnMut() -> bool,
{
    let mut sent = 0;
    let mut idle = 0;
    let mut first = true;
    while sent < bytes.len() {
        // Before the gap, so a deadline that has already passed is not paid a
        // pacing interval first, and before the write, so no further bytes go
        // on the wire once the send has been given up on.
        if out_of_time() {
            return Err(SendFault::TimedOut { sent, len: bytes.len() });
        }
        if !first {
            gap();
        }
        first = false;
        let end = bytes.len().min(sent.saturating_add(chunk.max(1)));
        match write_some(&bytes[sent..end]) {
            Ok(0) => {
                // Transient (TX buffer full) or permanent (unconfigured); the
                // API cannot say which, so bound the wait rather than guess.
                idle += 1;
                if idle > SEND_RETRY_BUDGET {
                    return Err(SendFault::Stalled);
                }
            }
            Ok(n) => {
                sent += n;
                idle = 0;
            }
            Err(e) => return Err(SendFault::Link(e)),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The transmit instrument
// ---------------------------------------------------------------------------

/// What one [`Transport::send`] did, counted at the only place the bytes
/// actually change hands: the return value of the platform sink.
///
/// # Why this exists
///
/// The twenty-third hardware run produced a writeback failure with a shape no
/// existing counter could explain. The host discarded **no** `WriteReq` — its
/// frame-shaped-noise reporter (§27f) logged one `ConOut` and nothing else —
/// and its memory image's mtime never moved across a ten-minute run, so no
/// `WriteReq` was accepted either. The frame was therefore neither corrupted
/// nor rejected: it did not arrive. Meanwhile the badge's own `send` returned
/// `Ok(())` and the exchange failed as `Timeout`, not [`LinkFault::Stalled`]
/// or [`LinkFault::SendTimedOut`].
///
/// Every reading of that transcript needs a number nobody was recording: what
/// the sink *said* it took, call by call, for the frame that vanished.
///
/// * `calls == 9`, `accepted == 4109`, `refusals == 0`, a few milliseconds —
///   the app layer handed over the whole frame without back-pressure, so
///   everything from `send_paced` up is exonerated and the loss is below
///   `usbd-serial`: `CorigineWrapper::write`, `bulk_xfer`, or the wire.
/// * `accepted < 4109` with `send` still returning `Ok` — impossible by
///   [`send_paced`]'s loop condition, and if it is ever seen, that loop is the
///   bug.
/// * `refusals` climbing with a long `ms` — the transmit pipeline is stalling
///   on completions, which is the `ep_in_busy` path of
///   `badge/bao1x-hal-usb-in-completion.patch` failing to clear.
///
/// It is a plain value type above the `cfg` for the reason everything else in
/// this module is: the leaf holds the syscall, the policy is testable on a
/// laptop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxTally {
    /// Bytes the caller wanted transmitted.
    pub asked: usize,
    /// Calls into the platform sink.
    pub calls: usize,
    /// Bytes the sink reported taking, summed.
    pub accepted: usize,
    /// Calls that returned `Ok(0)`: the sink took nothing at all.
    pub refusals: usize,
    /// Calls that returned `Ok(n)` with `0 < n <` the length offered. A short
    /// accept is not an error — it is the transmit buffer filling — but a
    /// writeback made entirely of them is a pipeline that is barely moving.
    pub shorts: usize,
    /// Explicit flushes issued for this frame. See [`TxTally::record_flush`].
    pub flushes: usize,
    /// Milliseconds the whole send spent.
    pub ms: u64,
}

impl TxTally {
    /// A tally for a send of `asked` bytes, before any call has been made.
    pub fn new(asked: usize) -> Self {
        Self { asked, ..Self::default() }
    }

    /// Folds in one platform-sink result. `offered` is the length of the slice
    /// handed to it, which is what makes a short accept distinguishable from a
    /// full one.
    pub fn record<E>(&mut self, offered: usize, r: &Result<usize, E>) {
        self.calls += 1;
        match r {
            Ok(0) => self.refusals += 1,
            Ok(n) => {
                self.accepted += n;
                if *n < offered {
                    self.shorts += 1;
                }
            }
            Err(_) => {}
        }
    }

    /// Notes that an explicit flush was issued.
    pub fn record_flush(&mut self) {
        self.flushes += 1;
    }

    /// True when the frame needed more than one packet, i.e. it is a
    /// `WriteReq`. Every other frame this link sends is one call.
    pub fn multi_packet(&self) -> bool {
        self.asked > TX_PACKET
    }
}

/// [`TxTally`] for the last send, plus the totals since boot.
///
/// The last send is the one a fault is about; the totals say whether the link
/// has ever transmitted a multi-packet frame at all — which through twenty-two
/// hardware rounds has been the single most valuable number about this port and
/// the one nobody could read off a transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxLog {
    /// The most recent send, whatever became of it.
    pub last: TxTally,
    /// Frames handed to `send`.
    pub frames: usize,
    /// Of those, the ones needing more than one packet.
    pub multi_frames: usize,
    /// Bytes accepted over the life of the link.
    pub accepted: usize,
    /// `Ok(0)` results over the life of the link.
    pub refusals: usize,
}

impl TxLog {
    /// Folds a finished send in.
    pub fn finish(&mut self, t: TxTally) {
        self.frames += 1;
        if t.multi_packet() {
            self.multi_frames += 1;
        }
        self.accepted = self.accepted.saturating_add(t.accepted);
        self.refusals = self.refusals.saturating_add(t.refusals);
        self.last = t;
    }

    /// One line for [`Transport::status`], which is attached to every recorded
    /// fault.
    pub fn describe(&self) -> String {
        let l = self.last;
        format!(
            "tx: last {} B asked / {} accepted in {} call(s), {} refused, {} short, \
             {} flush(es), {} ms | life {} frames ({} multi-packet), {} bytes accepted, \
             {} refusals",
            l.asked,
            l.accepted,
            l.calls,
            l.refusals,
            l.shorts,
            l.flushes,
            l.ms,
            self.frames,
            self.multi_frames,
            self.accepted,
            self.refusals,
        )
    }
}

// ---------------------------------------------------------------------------
// Park confirmation
// ---------------------------------------------------------------------------

/// Confirms that a receive listener is parked, tolerating the un-parks the
/// flush watchdog creates **by design**.
///
/// Lifted out of [`UsbTransport`] and expressed over closures for the same
/// reason [`send_all`] is: the two previous rounds of this task both put a
/// hardware-shaped defect in a place no laptop test could see, and both times
/// the defect was policy rather than platform. This is the policy.
///
/// # Why a lost sample is not a failure
///
/// `Opcode::SerialFlush` under `BinaryListener`
/// (`services/usb-bao1x/src/main.rs:740-750`) calls `serial_listener.take()`
/// **unconditionally** — whether or not there is anything buffered to deliver.
/// The watchdog runs every `FLUSH_MS` (5), so on an idle link the reader is
/// un-parked and re-parks roughly 200 times a second. That is not a
/// malfunction; it is the mechanism that bounds a blocked read, and this module
/// depends on it.
///
/// So a sample that catches the reader mid-re-park is the flush escape working.
/// Treating it as fatal — which an earlier revision did, returning `Err` on one
/// bad post-settle sample — converts a routine event into
/// [`LinkFault::NotParked`], an [`Error::Medium`], and a guest load/store
/// fault, at whatever fraction of ~16,000 page operations per boot happens to
/// land in the window. The probe got this right: `wait_parked` returns false
/// there too, and the probe *counts* it and runs the round anyway, because with
/// a watchdog running a reply is late, not lost.
///
/// This therefore loops back and waits for the re-park, bounded by
/// `deadline_ms` so a genuinely dead link still terminates.
///
/// # Why there is no fixed settle
///
/// The flag is published just before `lend_mut`, so a `true` can be a few
/// microseconds early. The earlier revision covered that with an unconditional
/// 2 ms sleep — which bought a rare few microseconds of safety at the price of
/// 2 ms on *every* page fault, and 2 ms per fault is most of the round trip
/// this module is justified by. `settle` is therefore one scheduler turn, not a
/// sleep, and the caller pays it only twice on the fast path.
///
/// Returns the number of un-parks absorbed — a latency statistic worth
/// counting, exactly as the probe counts it, and never an error.
pub fn confirm_park(
    mut parked: impl FnMut() -> bool,
    mut elapsed_ms: impl FnMut() -> u64,
    mut settle: impl FnMut(),
    deadline_ms: u64,
) -> Result<usize, usize> {
    let mut unparks = 0usize;
    loop {
        if parked() {
            // One turn for a just-published park to reach the server, then
            // re-check. Cheap when the reader has been parked for milliseconds,
            // which is the common case.
            settle();
            if parked() {
                return Ok(unparks);
            }
            // The watchdog took the listener. Wait for the re-park.
            unparks += 1;
        }
        if elapsed_ms() > deadline_ms {
            return Err(unparks);
        }
        settle();
    }
}

// ---------------------------------------------------------------------------
// Faults
// ---------------------------------------------------------------------------

/// Why the link returned [`Error::Medium`].
///
/// Readable through [`Link::take_fault`]. Without it the badge's only failure
/// report is an undifferentiated load/store fault at a random guest address —
/// the exact silence the probe spent a hardware round eliminating, reintroduced
/// one layer up.
///
/// Every `Medium` that reaches the guest carries one of these **except two**,
/// both of which are unreachable rather than merely unlikely and are named at
/// their sites: a failed `RefCell` borrow in `UsbHost::with`, which could only
/// record a fault by taking the borrow whose failure it is reporting; and a
/// request frame with no page number, which neither caller can construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkFault {
    /// No receive listener could be confirmed parked, so the request was never
    /// sent. Sending anyway is the module's cardinal sin — see the docs.
    NotParked,
    /// [`send_all`] gave up: the sink accepted nothing for
    /// [`SEND_RETRY_BUDGET`] consecutive attempts.
    Stalled,
    /// The transport's `send` spent [`SEND_DEADLINE_MS`] without handing over
    /// the whole frame.
    ///
    /// A distinct fault from [`LinkFault::Stalled`] on purpose: `sent` against
    /// `len` says whether the transmit path stopped dead or was crawling, and a
    /// partial `sent` is also the badge-side signature of the truncated frames
    /// the host reports as "discarded N bytes; frame-shaped ... WriteReq".
    /// Seeing the same truncation named from both ends is what tells a
    /// transcript reader the bytes were never handed over, rather than handed
    /// over and lost.
    SendTimedOut { sent: usize, len: usize },
    /// The transport's `send` failed outright.
    SendFailed,
    /// The transport's `recv` failed outright.
    RecvFailed,
    /// The request went unanswered.
    ///
    /// # Why this carries five numbers and not one
    ///
    /// Because "no answer" was covering at least four different bugs with one
    /// sentence, and the fourth hardware run could not be told apart from the
    /// third by reading it. The old text also quoted the *policy constants*
    /// (`over 2000 ms`) rather than measured time, so a run that gave up in
    /// 15 ms read as one that had waited two seconds.
    ///
    /// | what the numbers say | what it means |
    /// |---|---|
    /// | `elapsed` far below `RECV_DEADLINE_MS`, `waited` below `ATTEMPT_DEADLINE_MS` | something returned early. The wait is not being waited |
    /// | `deliveries == 0` | the receive path produced nothing at all: dead reader, listener never parked, or nothing on the wire |
    /// | `deliveries > 0`, `bytes_in > 0`, no frame | bytes are arriving and not decoding -- framing, CRC or truncation |
    /// | `elapsed` at the deadline with `deliveries == 0` | the honest case: we waited and the peer said nothing |
    Timeout {
        attempts: usize,
        /// Measured milliseconds across the whole exchange.
        elapsed_ms: u64,
        /// Measured milliseconds this attempt waited before giving up.
        waited_ms: u64,
        /// Non-empty deliveries the transport produced over the life of the link.
        deliveries: usize,
        /// Bytes in those deliveries.
        bytes_in: usize,
    },
    /// A response arrived for a page nobody asked for: the link is
    /// desynchronised, or someone added the concurrency the docs forbid.
    WrongPage { asked: u32, got: u32 },
    /// The host answered the right page with the wrong frame type.
    WrongType,
}

impl LinkFault {
    /// A one-line description for a transcript, with the platform's own error
    /// appended when there is one.
    pub fn describe(&self, platform: Option<&str>) -> String {
        let base = match self {
            LinkFault::NotParked => "no receive listener confirmed parked before send".into(),
            LinkFault::Stalled => {
                format!("send sink accepted nothing {SEND_RETRY_BUDGET} times running")
            }
            LinkFault::SendTimedOut { sent, len } => format!(
                "send gave up after {SEND_DEADLINE_MS} ms with {sent} of {len} bytes handed \
                 over: the transmit path is accepting bytes and not draining them"
            ),
            LinkFault::SendFailed => "transport send failed".into(),
            LinkFault::RecvFailed => "transport recv failed".into(),
            LinkFault::Timeout { attempts, elapsed_ms, waited_ms, deliveries, bytes_in } => {
                // Measured first, policy second, and then the reading. The
                // policy constants are still printed because a measurement is
                // only interpretable next to the bound it was supposed to hit.
                let reading = if *deliveries == 0 {
                    if *waited_ms < ATTEMPT_DEADLINE_MS / 2 {
                        "RETURNED EARLY and heard nothing: the attempt gave up well short \
                         of its own deadline, so the receive path is failing fast rather \
                         than waiting -- look at arm/recv, not at the peer"
                    } else {
                        "waited and heard nothing: no delivery ever reached the decoder, \
                         so either nothing arrived or the listener never took it"
                    }
                } else if *waited_ms < ATTEMPT_DEADLINE_MS / 2 {
                    "RETURNED EARLY though bytes were arriving: the wait is not being \
                     waited"
                } else {
                    "bytes arrived but never formed the frame asked for: framing, CRC, \
                     truncation, or an answer for another page"
                };
                format!(
                    "no answer after {attempts} attempt(s); measured {elapsed_ms} ms total, \
                     {waited_ms} ms on the last attempt, {deliveries} deliveries \
                     ({bytes_in} bytes) ever received. Policy: {RECV_DEADLINE_MS} ms total, \
                     {ATTEMPT_DEADLINE_MS} ms per attempt, {RETRY_BUDGET} re-sends. \
                     Reading: {reading}"
                )
            }
            LinkFault::WrongPage { asked, got } => {
                format!("response for page {got} while waiting for page {asked}")
            }
            LinkFault::WrongType => "response frame was the wrong type".into(),
        };
        match platform {
            Some(p) => format!("{base} ({p})"),
            None => base,
        }
    }
}

/// What the link was doing when it faulted.
///
/// # Why a fault has to say this
///
/// §27b cost a hardware round to establish that a `MEM FAULT 0x81fc800c` names
/// the guest *load* that missed, not the operation that failed —
/// `PageCache::resident` reads the incoming page first and writes the victim
/// back second, so a failed writeback surfaces at the address of an unrelated
/// read, with `writebacks` and `evictions` both still at their old values. The
/// transcript said nothing about which of the two had died, and the wrong one
/// was investigated first.
///
/// One clause of text ends that permanently: a fault now names the frame it was
/// exchanging and the page it was about, so "reading page 8136" and "writing
/// page 1275" are told apart in the line itself rather than reconstructed from
/// cache counters afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read(u32),
    Write(u32),
}

impl Op {
    fn describe(&self) -> String {
        match self {
            Op::Read(p) => format!("reading page {p}"),
            Op::Write(p) => format!("writing page {p}"),
        }
    }
}

/// A fault together with whatever the platform said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub kind: LinkFault,
    /// The page operation in flight, when there was one. `None` for the
    /// non-request paths — [`Link::pump`] and [`Link::send_console`] — which
    /// are not exchanges and have no page.
    pub op: Option<Op>,
    /// The platform's own error text, if the transport had one.
    pub platform: Option<String>,
    /// The receive path's state at the moment of the fault, healthy or not.
    /// See [`Transport::status`].
    pub status: Option<String>,
    /// What the frame decoder threw away, as an exact count plus a hex sample.
    /// See [`LinkInner::noise_report`] and [`NOISE_SAMPLE`].
    pub discarded: Option<String>,
}

impl Fault {
    pub fn describe(&self) -> String {
        let mut out = match self.op {
            Some(op) => format!("{}: {}", op.describe(), self.kind.describe(self.platform.as_deref())),
            None => self.kind.describe(self.platform.as_deref()),
        };
        if let Some(s) = &self.status {
            out.push_str(&format!(" [{s}]"));
        }
        if let Some(d) = &self.discarded {
            out.push_str(&format!(" [{d}]"));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The link and its shared half
// ---------------------------------------------------------------------------

struct LinkInner<T: Transport> {
    t: T,
    /// The page operation currently in flight, for [`Fault::op`]. Set by
    /// `exchange_inner` from the request frame and cleared by the paths that
    /// are not exchanges, so a fault can never inherit a stale one.
    op: Option<Op>,
    /// One `Mux` for the whole link, never one per delivery — the badge hands
    /// over at most 3840 bytes at a time and a page frame is 4109, so a `Mux`
    /// scoped to a single `recv` would buffer the first delivery's prefix and
    /// then throw it away, leaving the tail with nothing to complete against.
    /// `rv64_host::serve` carries the mirror image of this comment.
    mux: Mux,
    console: Vec<u8>,
    fault: Option<Fault>,
    /// Requests re-sent after an attempt went unanswered. Cumulative, and
    /// readable through [`Link::retries`] — if this climbs on hardware we want
    /// it in a transcript, not inferred from a slow boot.
    retries: usize,
    /// Held responses discarded as stale before an attempt sent. Cumulative.
    /// Non-zero means duplicates are arriving, which is the retry logic working
    /// rather than a fault.
    stale_dropped: usize,
    /// Responses for another page dropped *during* a wait, rather than found
    /// held at the start of one. Cumulative, and the counter the twenty-fourth
    /// run had no way to report: a duplicate that arrives after
    /// [`LinkInner::discard_stale_responses`] has already run is invisible to
    /// `stale_dropped`, which is why that run's fault line read `stale=0` while
    /// the response stream was one frame ahead.
    ///
    /// Non-zero and the boot still finished means exactly one thing: the wire
    /// carried more answers than this badge asked questions. Compare it with the
    /// host's own duplicate-request line (`rv64_host::serve`) to say which
    /// direction duplicated.
    late_dropped: usize,
    /// Reused across exchanges rather than reallocated. One 4109-byte `Vec` per
    /// page operation against ~16,000 page operations per boot, on a machine the
    /// probe measured at 308 KiB free, is a cost worth not paying.
    scratch: Vec<u8>,
    /// The first [`NOISE_SAMPLE`] bytes the decoder threw away, and how many it
    /// threw away in total. See [`LinkInner::noise_report`].
    noise_sample: Vec<u8>,
    noise_dropped: usize,
    /// Non-empty deliveries the transport has ever handed over, and their total
    /// size.
    ///
    /// These exist to answer one question a timeout cannot otherwise answer:
    /// **did anything at all ever arrive on this link?** Zero deliveries after a
    /// send means the receive path never produced a byte -- a dead reader, a
    /// listener that never parked, a cable in backwards -- while a non-zero
    /// count with no matching frame means bytes are arriving and not decoding.
    /// Those are different bugs and the message used to name neither.
    deliveries: usize,
    bytes_in: usize,
    /// Milliseconds spent inside [`LinkInner::exchange`], cumulative.
    ///
    /// This is the *whole* of the emulator's I/O wait: `read_page` and
    /// `write_page` are the only two things that block, they are synchronous,
    /// and they both go through `exchange`. Subtracting it from the run loop's
    /// wall clock is what turns "the badge took forty minutes" into an
    /// interpreter throughput figure -- see `run::Report::insn_per_sec`.
    ///
    /// Measured with [`Transport::now_ms`], which on the badge is the
    /// ticktimer, so the resolution is a millisecond and the cost is two extra
    /// clock reads per page operation -- ~3,000 per boot against ~173 million
    /// guest instructions.
    blocked_ms: u64,
    /// Completed calls to [`LinkInner::exchange`], successful or not. The
    /// denominator for [`LinkInner::blocked_ms`]: it is what makes
    /// "53 s of link time" readable as "18 ms per page operation".
    exchanges: usize,
}

impl<T: Transport> LinkInner<T> {
    /// Records a fault, pulling the platform's own error text along with it, and
    /// returns the `MemBacking` error to propagate.
    fn note(&mut self, kind: LinkFault) -> Error {
        let platform = self.t.take_platform_fault();
        let status = self.t.status();
        // `pump` and the read/write wrappers call `note` without going through
        // `resync`, so take whatever the decoder is holding here too.
        self.stash_noise();
        let discarded = self.noise_report();
        self.fault = Some(Fault { kind, op: self.op, platform, status, discarded });
        Error::Medium
    }

    /// Decodes `bytes`, moving console payloads aside.
    fn absorb(&mut self, bytes: &[u8]) {
        self.deliveries += 1;
        self.bytes_in += bytes.len();
        self.mux.push(bytes);
        let con = self.mux.take_console();
        if !con.is_empty() {
            self.console.extend_from_slice(&con);
        }
    }

    /// Drops every frame and partial frame the decoder is holding, after
    /// rescuing console bytes.
    ///
    /// Called when the link is known to be desynchronised. Without it a stale
    /// response sits in `Mux.held` and is discarded one exchange at a time by
    /// the page check — self-healing, but slowly.
    ///
    /// It used to cost a spurious guest fault per stale frame as well, and on
    /// hardware a guest fault ends the boot: one duplicate answer anywhere in
    /// 971 page reads was fatal. That is fixed where it belongs, in
    /// `exchange_inner`'s receive loop — a response for a page nobody is waiting
    /// for is now dropped and the wait continues — so this is a bulk discard for
    /// speed, not the thing standing between a duplicate and a dead run.
    fn resync(&mut self) {
        let con = self.mux.take_console();
        if !con.is_empty() {
            self.console.extend_from_slice(&con);
        }
        // Rescue the discarded-byte sample before the decoder holding it is
        // dropped. `resync` runs on the failure exit path, immediately before
        // `note` records the fault -- so without this the one thing worth
        // knowing about a failed exchange would be destroyed a line before it
        // was asked for.
        self.stash_noise();
        self.mux = Mux::capturing_noise_capped(NOISE_SAMPLE);
    }

    /// Moves whatever the decoder discarded into [`LinkInner::noise`], where
    /// [`LinkInner::note`] can put it in the fault.
    fn stash_noise(&mut self) {
        let (sample, dropped) = self.mux.take_noise();
        if sample.is_empty() && dropped == 0 {
            return;
        }
        self.noise_sample = sample;
        self.noise_dropped += dropped;
    }

    /// `<n> bytes discarded; first <k>: aa bb cc ...`, or `None` when the
    /// decoder threw nothing away.
    ///
    /// Hex rather than an interpretation, deliberately: the four hypotheses
    /// still open are told apart by what the first few bytes *are* — a
    /// `c1 b0 02 04 10` prefix is an intact frame rejected on CRC, a shifted
    /// copy is upstream mangling, a 13-byte `c1 b0 01 04 00` is our own request
    /// looping back, and nothing at all means the bytes never reached the
    /// decoder. Any of those summarised into a sentence here would be this
    /// module guessing again.
    /// **Drains** the stash, so a later fault that discarded nothing reports
    /// nothing rather than inheriting an older fault's bytes. A diagnostic that
    /// can be stale is worse than none: it would send a reader after a frame
    /// that had already been explained.
    fn noise_report(&mut self) -> Option<String> {
        let sample = core::mem::take(&mut self.noise_sample);
        let dropped = core::mem::take(&mut self.noise_dropped);
        let total = sample.len() + dropped;
        if total == 0 {
            return None;
        }
        let hex: Vec<String> = sample.iter().map(|b| format!("{b:02x}")).collect();
        Some(format!(
            "decoder discarded {total} bytes; first {}: {}",
            sample.len(),
            hex.join(" ")
        ))
    }

    /// Response frame types. At the moment an attempt is about to send, nothing
    /// is outstanding — requests are strictly synchronous — so a held frame of
    /// any of these types answers a request that has already completed or
    /// already timed out. It is stale by construction.
    const RESPONSE_TYPES: [u8; 3] = [TY_READ_RESP, TY_WRITE_ACK, TY_ERR];

    /// Drops held responses left over from an earlier exchange or an earlier
    /// attempt, returning how many.
    ///
    /// Without this a duplicate answer produced by a retry would fail the *next*
    /// exchange's page check and cost a spurious guest fault — and, worse, a
    /// duplicate for the same page could satisfy a later read of that page after
    /// an intervening write, returning pre-write data. See [`RETRY_BUDGET`].
    fn discard_stale_responses(&mut self) -> usize {
        let mut n = 0;
        for ty in Self::RESPONSE_TYPES {
            while self.mux.take_matching(ty).is_some() {
                n += 1;
            }
        }
        n
    }

    /// Sends `f` and waits for a frame of type `want` for the page `f` names,
    /// re-sending up to [`RETRY_BUDGET`] times if an attempt goes unanswered.
    ///
    /// A dropped frame is transient and a re-send is safe; both claims are
    /// argued at [`RETRY_BUDGET`], and the second is the one worth reading
    /// before touching this.
    ///
    /// This wrapper exists only to time [`LinkInner::exchange_inner`], which has
    /// three exit points; bracketing here rather than at each of them is what
    /// keeps the accounting from drifting the next time one is added.
    fn exchange(&mut self, f: Frame, want: u8) -> Result<Frame, Error> {
        let t0 = self.t.now_ms();
        let r = self.exchange_inner(f, want);
        let t1 = self.t.now_ms();
        self.blocked_ms = self.blocked_ms.saturating_add(t1.saturating_sub(t0));
        self.exchanges += 1;
        r
    }

    fn exchange_inner(&mut self, f: Frame, want: u8) -> Result<Frame, Error> {
        // Both callers pass a page-bearing frame, so this cannot fail today. It
        // is `?` rather than `expect` anyway: a panic on the badge is a dead
        // process with no console left to say why.
        let asked = frame_page(&f).ok_or(Error::Medium)?;
        // Recorded before anything can fail, so every fault out of this
        // function names the operation rather than the guest address the
        // failure will eventually surface at. See [`Op`].
        self.op = Some(match f {
            Frame::WriteReq { .. } => Op::Write(asked),
            _ => Op::Read(asked),
        });

        // The request buffer is moved out, filled and moved back, so the same
        // allocation serves every exchange — and every retry — for the life of
        // the link.
        let mut out = core::mem::take(&mut self.scratch);
        out.clear();
        encode(&f, &mut out);

        let began = self.t.now_ms();
        let mut attempts = 0usize;
        // Responses for some *other* page seen while waiting for this one, and
        // the last such page. Kept across attempts so a link that answers every
        // request with the wrong page still ends as `WrongPage` and not as an
        // undifferentiated timeout.
        let mut mismatched = 0usize;
        let mut last_mismatch: Option<u32> = None;

        let fault = 'attempts: loop {
            attempts += 1;
            let stale = self.discard_stale_responses();
            self.stale_dropped += stale;

            // ---- the deadline, checked where it can actually be evaded ----
            // `RECV_DEADLINE_MS` used to be checked in exactly one place: the
            // receive loop below. Everything *above* that loop — `arm`, which
            // is allowed `PARK_WAIT_MS` (1000) of its own, and `send`, which on
            // the badge is a blocking syscall with no time bound at all — could
            // therefore spend arbitrary time without the deadline ever being
            // consulted. The bound was not `RECV_DEADLINE_MS`; it was
            // `(RETRY_BUDGET + 1) x (arm + send)`, with one term unbounded.
            //
            // These two checks make it unconditional in the sense the module's
            // whole diagnostic design rests on: **no attempt begins, and no
            // request is transmitted, after the deadline has passed.** A link
            // that is merely slow now reports `Timeout` with its numbers
            // attached instead of running on until something else notices.
            //
            // What this still cannot bound, and it is worth being exact about:
            // a `send` that never *returns*. `Transport::send` on the badge is
            // `xous::send_message`, and a blocking syscall cannot be timed out
            // by the thread that is inside it — no check placed here or
            // anywhere else in this crate can fire while the main thread is
            // parked in the kernel. That case is a wedged USB server, and it is
            // fixed where it lives (see `badge/README.md`, "the log server must
            // not block"), not with a deadline here.
            let elapsed = self.t.now_ms().saturating_sub(began);
            if elapsed > RECV_DEADLINE_MS {
                break 'attempts LinkFault::Timeout {
                    attempts,
                    elapsed_ms: elapsed,
                    waited_ms: elapsed,
                    deliveries: self.deliveries,
                    bytes_in: self.bytes_in,
                };
            }

            // ---- the ordering the module exists to get right ----
            // A listener must be parked and CONFIRMED before the request leaves,
            // on every attempt. `SerialHookBinary` does not drain `serial_buf`,
            // and against a synchronous peer nothing else will: a reply that
            // beats the park is lost, not delayed. Inverting these two does not
            // fail a test anywhere else — it hangs the badge with no output.
            if self.t.arm().is_err() {
                break 'attempts LinkFault::NotParked;
            }
            // The second half of the check above. `arm` is allowed
            // `PARK_WAIT_MS` and a slow re-park can spend most of it, so the
            // deadline is re-read *after* it: transmitting a request whose
            // answer can no longer be waited for is 4109 bytes of wire time
            // spent to produce a timeout that is already decided.
            let elapsed = self.t.now_ms().saturating_sub(began);
            if elapsed > RECV_DEADLINE_MS {
                break 'attempts LinkFault::Timeout {
                    attempts,
                    elapsed_ms: elapsed,
                    waited_ms: elapsed,
                    deliveries: self.deliveries,
                    bytes_in: self.bytes_in,
                };
            }
            if let Err(e) = self.t.send(&out) {
                break 'attempts match e {
                    SendError::Stalled => LinkFault::Stalled,
                    SendError::Failed => LinkFault::SendFailed,
                    SendError::TimedOut { sent, len } => LinkFault::SendTimedOut { sent, len },
                };
            }

            let sent_at = self.t.now_ms();
            loop {
                if let Some(r) = self.mux.take_matching(want) {
                    match frame_page(&r) {
                        Some(p) if p == asked => {
                            self.scratch = out;
                            return Ok(r);
                        }
                        // Not this caller's bytes. It is an answer to an
                        // exchange that has already completed — requests are
                        // strictly synchronous, so nothing else can have asked
                        // for it — which is to say it is exactly what
                        // `discard_stale_responses` would have thrown away, had
                        // it arrived a few microseconds earlier.
                        //
                        // So it is dropped and the wait continues, the same way
                        // a stray `Err` is a few lines below. **Ending the wait
                        // here was the bug**: one duplicate response anywhere in
                        // a boot cost a `MEM FAULT` and the whole run, and
                        // `resync`'s own doc comment already said as much ("a
                        // spurious guest fault for each stale frame"). The
                        // answer this caller asked for is the very next frame in
                        // an ordered stream, so the cost of absorbing the
                        // duplicate is one more turn of this loop.
                        //
                        // Finite: each turn removes one held frame, and the two
                        // deadlines below still bound the wait.
                        //
                        // A link that is *genuinely* desynchronised — every
                        // answer for the wrong page, never the right one — still
                        // reports `WrongPage` rather than a bare timeout: the
                        // mismatch is remembered and re-raised at the deadline.
                        // See `mismatched` / `last_mismatch`.
                        got => {
                            mismatched += 1;
                            self.late_dropped += 1;
                            last_mismatch = got;
                            continue;
                        }
                    }
                }
                if let Some(Frame::Err { code, page }) = self.mux.take_matching(TY_ERR) {
                    if page == asked {
                        self.scratch = out;
                        return Err(err_code(code));
                    }
                    // An `Err` for some other page is not this caller's answer,
                    // and ending the wait on it would report a fault for the
                    // wrong page. Drop it and keep waiting. Finite: each turn
                    // removes one held frame.
                    continue;
                }

                let now = self.t.now_ms();
                let timeout = |i: &Self, attempts| LinkFault::Timeout {
                    attempts,
                    elapsed_ms: now.saturating_sub(began),
                    waited_ms: now.saturating_sub(sent_at),
                    deliveries: i.deliveries,
                    bytes_in: i.bytes_in,
                };
                if now.saturating_sub(began) > RECV_DEADLINE_MS {
                    break 'attempts timeout(self, attempts);
                }
                if now.saturating_sub(sent_at) > ATTEMPT_DEADLINE_MS {
                    if attempts > RETRY_BUDGET {
                        break 'attempts timeout(self, attempts);
                    }
                    // Transient: a dropped request, most likely split by
                    // mirrored log output on the shared transmit endpoint.
                    // Re-arm and re-send.
                    self.retries += 1;
                    continue 'attempts;
                }

                let bytes = match self.t.recv() {
                    Ok(b) => b,
                    Err(()) => break 'attempts LinkFault::RecvFailed,
                };
                // An empty delivery is a flush that found nothing buffered,
                // which is routine with a watchdog running. It means "not yet",
                // never "the cable is gone" — the deadlines above bound the wait.
                if !bytes.is_empty() {
                    self.absorb(&bytes);
                }
            }
        };

        self.scratch = out;
        // A wait that ran out of time *having seen answers for other pages* is
        // not a silent link; it is a desynchronised one, and saying so is worth
        // more than the timing numbers. This is what keeps `WrongPage`
        // reachable now that a single mismatch no longer ends the wait: it is
        // raised when the mismatch was the whole story, never when one stray
        // duplicate was absorbed and the right answer followed.
        let fault = match fault {
            LinkFault::Timeout { .. } if mismatched > 0 => {
                LinkFault::WrongPage { asked, got: last_mismatch.unwrap_or(u32::MAX) }
            }
            other => other,
        };
        // Held frames from a failed exchange are worthless and would cost the
        // next exchange a spurious fault apiece.
        self.resync();
        Err(self.note(fault))
    }
}

/// The half of the link that survives `PageCache` taking ownership of the
/// backing.
///
/// Cheap to clone; every clone views the same link. `Rc`, not `Arc`, on purpose:
/// the emulator's run loop is single-threaded, and the reader thread that
/// [`UsbTransport`] spawns lives entirely *below* this boundary — it publishes
/// into the transport, never into here.
pub struct Link<T: Transport> {
    inner: Rc<RefCell<LinkInner<T>>>,
}

impl<T: Transport> Clone for Link<T> {
    fn clone(&self) -> Self {
        Self { inner: Rc::clone(&self.inner) }
    }
}

impl<T: Transport> Link<T> {
    /// Drains console bytes received so far.
    pub fn take_console(&self) -> Vec<u8> {
        match self.inner.try_borrow_mut() {
            Ok(mut i) => core::mem::take(&mut i.console),
            Err(_) => Vec::new(),
        }
    }

    /// Requests re-sent because an attempt went unanswered, over the life of the
    /// link.
    ///
    /// Expected to be small but non-zero once the USB panic mirror is hooked:
    /// mirrored log output shares the transmit endpoint with protocol frames and
    /// splits one occasionally. A count that climbs with the guest's paging rate
    /// is that, working. A count that climbs without it is something else, and
    /// worth putting in a transcript rather than inferring from a slow boot.
    pub fn retries(&self) -> usize {
        self.inner.try_borrow().map(|i| i.retries).unwrap_or(0)
    }

    /// Held responses discarded as stale before an attempt sent. Non-zero means
    /// a retry produced a duplicate answer and it was dropped rather than
    /// allowed to satisfy a later exchange.
    pub fn stale_dropped(&self) -> usize {
        self.inner.try_borrow().map(|i| i.stale_dropped).unwrap_or(0)
    }

    /// Responses for another page dropped while waiting for this one. See
    /// [`LinkInner::late_dropped`]: non-zero means the stream carried a
    /// duplicate answer that arrived too late for the pre-send purge to catch,
    /// and the link absorbed it instead of faulting the guest.
    pub fn late_dropped(&self) -> usize {
        self.inner.try_borrow().map(|i| i.late_dropped).unwrap_or(0)
    }

    /// Milliseconds this link has spent blocked on a page exchange, and the
    /// number of exchanges that produced them. See [`LinkInner::blocked_ms`].
    pub fn blocked(&self) -> (u64, usize) {
        self.inner.try_borrow().map(|i| (i.blocked_ms, i.exchanges)).unwrap_or((0, 0))
    }

    /// The transport's clock, for the run loop's own timing.
    ///
    /// `None` means the borrow failed, i.e. an exchange is in progress on this
    /// link -- which nothing in the run loop does, since it only reads the clock
    /// between slices. A dropped sample is better than an invented one: this is
    /// a measuring instrument, and a zero here would be indistinguishable from
    /// a real timestamp and would silently corrupt a throughput figure.
    pub fn now_ms(&self) -> Option<u64> {
        self.inner.try_borrow_mut().map(|mut i| i.t.now_ms()).ok()
    }

    /// Takes the reason for the most recent [`Error::Medium`], if one has not
    /// been read yet.
    pub fn take_fault(&self) -> Option<Fault> {
        self.inner.try_borrow_mut().ok().and_then(|mut i| i.fault.take())
    }

    /// Non-blocking. Decodes whatever has already arrived on the link.
    ///
    /// Call this once per instruction slice. Without it, console input reaches
    /// the guest only as a side effect of a page fault — which is to say, never,
    /// once the working set is resident and the guest is sitting at a shell
    /// prompt, which is exactly when a human is typing.
    ///
    /// Any page frame this happens to decode is held by the `Mux`; since no
    /// request is outstanding while the run loop is executing, such a frame is
    /// by definition stale, and the next exchange's page check discards it.
    pub fn pump(&self) -> Result<(), Error> {
        // A failed borrow means an exchange is in progress on the same link,
        // which nothing in the run loop does. Skipping is correct and a panic
        // would be fatal.
        let Ok(mut i) = self.inner.try_borrow_mut() else {
            return Ok(());
        };
        // Not an exchange: there is no page in flight, and a fault from here
        // must not inherit the last one's.
        i.op = None;
        match i.t.poll() {
            Ok(b) => {
                if !b.is_empty() {
                    i.absorb(&b);
                }
                Ok(())
            }
            Err(()) => Err(i.note(LinkFault::RecvFailed)),
        }
    }

    /// Mirrors guest console output to the host as a `ConOut` frame.
    ///
    /// The badge's guest console goes to the OLED, which is eight rows. A
    /// kernel oops is longer than that, and after the fact the rows it scrolled
    /// off are gone -- so the serial transcript, which is the other half of
    /// "one photograph plus a log", carried nothing the guest said. This is
    /// what puts it there. `rv64_host::serve` already decodes `ConOut` and
    /// echoes it to stdout; that half was written and tested before anything
    /// sent one.
    ///
    /// # Why this does not violate the synchronous-request doctrine
    ///
    /// A `ConOut` is not a request: nothing is expected back, so it adds no
    /// outstanding request, no correlation problem and no queue depth beyond
    /// the frame itself. And it cannot split a request the way mirrored log
    /// text can, because that hazard comes from a *second writer* landing
    /// between the two `serial_send` calls a 4109-byte request takes. This
    /// writer is the run loop, on the same thread as `exchange`, between
    /// slices -- there is never a request half-sent when it runs.
    ///
    /// It is called with whatever the guest printed during one slice, so on a
    /// quiet link it is called with nothing and costs nothing.
    ///
    /// A failure is the caller's to survive: losing a transcript line is not
    /// worth ending a boot over. The fault is recorded either way, so
    /// [`Link::take_fault`] explains it.
    pub fn send_console(&self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        // Same reasoning as `pump`: a failed borrow means an exchange is in
        // progress, which cannot happen from the run loop's service point.
        let Ok(mut i) = self.inner.try_borrow_mut() else {
            return Ok(());
        };
        // A fresh buffer rather than `scratch`. `scratch` is sized once at
        // `MAX_FRAME` and reused for the life of the link precisely so page
        // exchanges never allocate; `encode` chunks a `ConOut` at `MAX_PAYLOAD`
        // per frame, so a long burst would grow that buffer permanently and
        // quietly undo the invariant its comment claims. Console output is a
        // few kilobytes across a whole boot, against ~16,000 page operations.
        let mut out = Vec::new();
        encode(&Frame::ConOut(bytes.to_vec()), &mut out);
        // Same reason as `pump`: a console mirror is not a page operation.
        i.op = None;
        match i.t.send(&out) {
            Ok(()) => Ok(()),
            Err(e) => Err(i.note(match e {
                SendError::Stalled => LinkFault::Stalled,
                SendError::Failed => LinkFault::SendFailed,
                SendError::TimedOut { sent, len } => LinkFault::SendTimedOut { sent, len },
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// UsbHost
// ---------------------------------------------------------------------------

pub struct UsbHost<T: Transport> {
    link: Link<T>,
}

impl<T: Transport> UsbHost<T> {
    pub fn new(t: T) -> Self {
        Self {
            link: Link {
                inner: Rc::new(RefCell::new(LinkInner {
                    op: None,
                    t,
                    mux: Mux::capturing_noise_capped(NOISE_SAMPLE),
                    console: Vec::new(),
                    fault: None,
                    retries: 0,
                    stale_dropped: 0,
                    late_dropped: 0,
                    scratch: Vec::with_capacity(MAX_FRAME),
                    deliveries: 0,
                    bytes_in: 0,
                    noise_sample: Vec::new(),
                    noise_dropped: 0,
                    blocked_ms: 0,
                    exchanges: 0,
                })),
            },
        }
    }

    /// A handle to the console, the fault channel and the non-blocking pump.
    ///
    /// Take it *before* handing this `UsbHost` to `rv64::PageCache::new`, which
    /// owns its backing privately and never gives it back.
    pub fn link(&self) -> Link<T> {
        self.link.clone()
    }

    /// Drains console bytes received so far. Equivalent to
    /// `self.link().take_console()`, and usable only while this `UsbHost` is
    /// still reachable — which, once `PageCache` owns it, it is not. Prefer
    /// [`UsbHost::link`].
    pub fn take_console(&mut self) -> Vec<u8> {
        self.link.take_console()
    }

    fn with<R>(&mut self, f: impl FnOnce(&mut LinkInner<T>) -> Result<R, Error>) -> Result<R, Error> {
        let Ok(mut i) = self.link.inner.try_borrow_mut() else {
            return Err(Error::Medium);
        };
        f(&mut i)
    }
}

/// The page number a frame is about, for the frame kinds that have one.
fn frame_page(f: &Frame) -> Option<u32> {
    match f {
        Frame::ReadReq { page }
        | Frame::WriteAck { page }
        | Frame::ReadResp { page, .. }
        | Frame::WriteReq { page, .. }
        | Frame::Err { page, .. } => Some(*page),
        Frame::ConIn(_) | Frame::ConOut(_) => None,
    }
}

/// Translates the host's error codes into `MemBacking` errors.
///
/// `rv64_host::serve` sends code 1 when it cannot read the requested page and
/// code 2 when it cannot write it, and the only cause it actually produces is
/// its `out_of_range` check against the image length. So mapping both to
/// [`Error::OutOfRange`] is right for every case the host generates today, and
/// it is what the conformance suite requires.
///
/// It is nonetheless lossy, knowingly: a genuine host-side I/O fault would
/// arrive under the same code and be reported to the guest as an out-of-range
/// access. Fixing that means a distinct error code on the wire and a matching
/// arm in `serve` — an `rv64-proto` change, not this task's.
fn err_code(code: u16) -> Error {
    match code {
        ERR_READ | ERR_WRITE => Error::OutOfRange,
        _ => Error::Medium,
    }
}

impl<T: Transport> MemBacking for UsbHost<T> {
    fn read_page(&mut self, page: u32, buf: &mut [u8; PAGE]) -> Result<(), Error> {
        self.with(|i| match i.exchange(Frame::ReadReq { page }, TY_READ_RESP)? {
            Frame::ReadResp { data, .. } => {
                buf.copy_from_slice(&data[..]);
                Ok(())
            }
            _ => Err(i.note(LinkFault::WrongType)),
        })
    }

    fn write_page(&mut self, page: u32, buf: &[u8; PAGE]) -> Result<(), Error> {
        let data = Box::new(*buf);
        self.with(|i| match i.exchange(Frame::WriteReq { page, data }, TY_WRITE_ACK)? {
            Frame::WriteAck { .. } => Ok(()),
            _ => Err(i.note(LinkFault::WrongType)),
        })
    }

    /// Nothing to flush. Every `write_page` has already been acknowledged by the
    /// host before it returned — the exchange is synchronous, so there is no
    /// such thing as an unacknowledged write sitting in a buffer here.
    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The real badge transport
// ---------------------------------------------------------------------------

#[cfg(target_os = "xous")]
pub use badge::UsbTransport;

/// The hardware [`Transport`]: `usb_bao1x` USB-CDC, shaped exactly like the
/// probe's proven receive path.
///
/// Compiled only for the badge, but type-checked on the laptop by
/// `cargo check --target riscv32imac-unknown-xous-elf`.
///
/// **This module requires a badge image carrying
/// `badge/usb-bao1x-serialflush-repair.patch`.** The flush watchdog below is
/// what bounds a blocked read; stock, the flush handler's binary branch does a
/// `copy_from_slice` into the client's empty `Vec` and panics whenever there is
/// anything to deliver.
#[cfg(target_os = "xous")]
mod badge {
    use super::{
        confirm_park, send_paced, SendError, SendFault, Transport, TxLog, TxTally,
        SEND_DEADLINE_MS, SERIAL_BINARY_BUFLEN, SYNC_LO, TX_PACKET,
    };
    use num_traits::ToPrimitive;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// How long [`UsbTransport::arm`] will wait for a confirmed park. The reader
    /// re-parks in microseconds, so exceeding this means the USB server is
    /// wedged, not busy.
    ///
    /// There is deliberately no `PARK_SETTLE_MS` any more. See [`confirm_park`]:
    /// a fixed 2 ms sleep before every request put most of a round trip on the
    /// critical path of every page fault, to buy a few microseconds of cover for
    /// a window the flush watchdog already covers.
    const PARK_WAIT_MS: u64 = 1000;
    /// Watchdog flush period. The probe's `FLUSH_RT_MS` — the value its latency
    /// leg ran at when it measured 2 ms per round trip. It is the residual floor
    /// for a reply that lands while the reader is between deliveries.
    const FLUSH_MS: u64 = 5;
    /// How long the reader waits after a failed listener lend before re-parking.
    /// Long enough for the USB server's main loop to pop queue slots, short
    /// enough that `serial_buf` does not grow much at 5.8 MiB/s. The probe's
    /// `RX_ERR_BACKOFF_MS`.
    const RX_ERR_BACKOFF_MS: u64 = 10;
    /// Poll granularity once the spin below has given up. This is what an idle
    /// or dead link costs; it is never on the critical path of a page fault.
    const POLL_MS: u64 = 1;
    /// Wait between one transmitted CDC packet and the next. See
    /// [`send_paced`] for the defect this exists for.
    ///
    /// **One millisecond, because the hardware said so twice.** §28d set this
    /// to 0 on the strength of `badge/bao1x-hal-usb-in-completion.patch` making
    /// `CorigineWrapper::write` return `WouldBlock` while a previous IN
    /// transfer is in flight, and left the instruction that if a run came back
    /// with `writebacks=0` it should go back to 1. It did, and this is that:
    ///
    /// ```text
    /// [rv64-host: discarded 2061 bytes; frame-shaped at offset 0: WriteReq,
    ///  declared len 4100, page 1275; 4 full 512-byte blocks, REPEATED: 2=3]
    /// ```
    ///
    /// A 512-byte block on the wire three times over is the clobbered transmit
    /// buffer, named from the transcript exactly as §27f said it would be. The
    /// completion check is not carrying the pipeline on its own — either the
    /// completion event is late often enough that `IN_REFUSAL_LIMIT`'s escape
    /// hatch fires and enqueues over a live buffer, or `endpoint_in_complete`
    /// is not re-entering `flush` on this stack. Both are firmware questions;
    /// this millisecond is the app-side answer that has demonstrably worked,
    /// and it is what the run that reached a shell was built with.
    ///
    /// **The gap is what a writeback pays, and nothing else does.**
    /// [`send_paced`] runs it *between* calls, and every frame this link sends
    /// except a page is one call: a `ReadReq` is 13 bytes, a `ConOut` line is
    /// short. A 4109-byte `WriteReq` is nine calls and eight gaps, so ~8 ms per
    /// writeback and zero on everything else. At **816** writebacks to a shell
    /// — the measured figure at the current `run::FRAMES` of 1400, which see —
    /// that is ~6.5 s across a boot, against a transmit path that otherwise
    /// does not work at all.
    ///
    /// Chunking at [`TX_PACKET`] stays either way: it is what keeps a short
    /// accept meaningful, and `send_paced`'s retry budget (64) absorbs the
    /// refusals. Going back to 0 needs a hardware run whose `writebacks` climb
    /// with it at 0 — not an argument from the driver source, which has now
    /// been wrong about this once.
    const TX_PACE_MS: u64 = 1;
    /// Scheduler turns to spin — yielding, so the reader runs — before falling
    /// back to `POLL_MS` sleeps.
    ///
    /// The wire round trip is ~2 ms and the main thread has nothing else to do
    /// while a page fault is outstanding, so yielding until the reply lands
    /// costs no useful work and saves up to `POLL_MS` of notice latency per
    /// delivery, of which a page frame needs several. The number itself is a
    /// laptop-side guess: the *shape* (spin, then sleep) is what matters, and
    /// the value wants one hardware run to tune.
    ///
    /// For `recv` this is a budget **per exchange**, reset by `arm`, not per
    /// call. Per call it was a spin of this size on every one of the ~2000
    /// iterations a timeout takes — half a million context switches to discover
    /// that a cable is unplugged. Per exchange it covers the expected round trip
    /// once and then degrades to pure `POLL_MS` polling, which is the behaviour
    /// wanted in both cases: prompt when the reply is coming, cheap when it is
    /// not.
    const SPIN_TURNS: usize = 256;
    /// Listener lends that may fail before the reader gives up.
    ///
    /// The probe's `RX_ERR_BUDGET`, and a budget rather than a counter for the
    /// reason given there: each failure `core::mem::forget`s a 4 KiB page
    /// permanently — the kernel lends the page to the USB server before it
    /// discovers the queue is full, and nothing gives it back — so against the
    /// ~308 KiB the probe measured free, an unbounded retry at one per
    /// `RX_ERR_BACKOFF_MS` leaks the machine in about a second. Exceeding it
    /// stops the reader, which makes every subsequent `arm` time out with the
    /// reason attached: loud, and bounded.
    const RX_ERR_BUDGET: usize = 24;
    /// Process heap ceiling to ask for. The default is 512 KiB unless the kernel
    /// carries `big-heap`.
    const HEAP_MAX: usize = 8 * 1024 * 1024;

    /// What the reader thread publishes and the main thread reads.
    struct Rx {
        /// True while the reader is blocked in `wait_binary` — or, for the few
        /// microseconds between the store and the syscall, about to be.
        /// Requests are not sent until this reads true, which is what keeps a
        /// reply from beating the park.
        parked: AtomicBool,
        q: Mutex<Vec<Vec<u8>>>,
        errs: AtomicUsize,
        last_err: AtomicUsize,
        /// Set when the reader has spent `RX_ERR_BUDGET` and stopped. Nothing
        /// will ever park again, so every `arm` from here on times out with the
        /// reason attached.
        gave_up: AtomicBool,
        /// The first [`RX_FINGERPRINT`] bytes of the first non-empty delivery,
        /// captured **the instant `wait_binary` returns** — before the `Vec`
        /// is queued, moved, or touched by anything else.
        ///
        /// # What this is for
        ///
        /// The fifth hardware run showed the decoder discarding 16,436 bytes of
        /// RISC-V `nop` padding while the host had demonstrably put four
        /// correct `ReadResp` frames on the wire. So the bytes are wrong, the
        /// *count* is exactly right, and the corruption is somewhere in the
        /// handoff rather than in framing, CRC or the decoder — all of which
        /// are now eliminated.
        ///
        /// This splits the remaining space in half, which is the one thing a
        /// laptop cannot do here:
        ///
        /// * fingerprint shows `c1 b0 02 04 10 …` while the decoder saw nops →
        ///   the delivery was **correct when it arrived** and is being corrupted
        ///   below this line: the queue, `drain`, or the `Vec` itself.
        /// * fingerprint shows nops too → it was **already wrong** when
        ///   `wait_binary` returned: rkyv's `to_original`, the lent page, or the
        ///   server's write into it.
        ///
        /// This is exactly the check `../probe/src/main.rs` had — it compared
        /// `d[0]` and `d[d.len() - 1]` against its fill byte and counted
        /// mismatches in `RX_BAD` — and it is the reason the probe could claim
        /// its *content* was good and not merely its byte count. This crate's
        /// reader dropped that check when it took the probe's code, which is
        /// why five runs could not tell these two cases apart.
        first: Mutex<Vec<u8>>,
        /// Bytes the reader has handed over, counted here rather than inferred
        /// from `absorb` — so the two can be compared.
        rx_bytes: AtomicUsize,
        /// Non-empty deliveries whose first byte is not the low half of `SYNC`.
        /// The first delivery of a frame must start with it; a mid-frame
        /// continuation need not, so this is a smell rather than a verdict.
        not_sync: AtomicUsize,
    }

    /// How many bytes of the first delivery to fingerprint. Enough for a frame
    /// header (`SYNC(2) TYPE(1) LEN(2)`) and a recognisable start of payload.
    const RX_FINGERPRINT: usize = 16;

    pub struct UsbTransport {
        usb: usb_bao1x::UsbHid,
        tt: ticktimer::Ticktimer,
        rx: Arc<Rx>,
        /// Errors already reported through `take_platform_fault`, so a repeat
        /// call does not re-report an old one.
        reported_errs: usize,
        send_err: Option<xous::Error>,
        /// Yields `recv` may still spend on this exchange. Reset by `arm`.
        recv_spins_left: usize,
        /// Watchdog un-parks absorbed by [`confirm_park`]. A latency statistic,
        /// never an error — but read on the way out through
        /// [`UsbTransport::unparks`] and reported on an `arm` failure, because a
        /// rate far above `1 / FLUSH_MS` means the reader is not keeping up.
        unparks: usize,
        /// Un-parks absorbed by the `arm` that failed, if one has. Reported once
        /// and cleared: it is the difference between "the reader is gone" and
        /// "the reader is thrashing".
        arm_fail: Option<usize>,
        /// False if construction never saw the reader park, i.e. the listen mode
        /// may never have flipped to `BinaryListener`. Reported once.
        primed: bool,
        /// What the transmit path has actually done. Reported by `status`, so
        /// it lands in every `MEM FAULT` line. See [`TxLog`].
        tx: TxLog,
    }

    impl UsbTransport {
        /// Starts the reader and the flush watchdog, primes `BinaryListener`,
        /// and raises the process heap ceiling.
        ///
        /// Does not return until the reader has parked once. That first park is
        /// what flips `serial_listen_mode` from `NoListener` (whose IRQ branch
        /// does `serial_buf.clear()` — the bytes are gone) or `ConsoleListener`
        /// (which injects them as keystrokes and then clears) to
        /// `BinaryListener`, after which arriving bytes queue even with no
        /// listener parked. Sending a request before that flip means the first
        /// response is destroyed rather than merely stranded.
        pub fn new(usb: usb_bao1x::UsbHid) -> Self {
            raise_heap_ceiling();

            // The watchdog goes FIRST now, because the main-thread probe below
            // parks a listener and `wait_binary` blocks forever with no sender:
            // the flush on a period is what bounds it. It was spawned after the
            // reader before; the order matters only for the probe.
            std::thread::spawn(|| {
                let flusher = usb_bao1x::UsbHid::new();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(FLUSH_MS));
                    flusher.serial_flush().ok();
                }
            });

            let tt = ticktimer::Ticktimer::new().expect("no ticktimer");

            let rx = Arc::new(Rx {
                parked: AtomicBool::new(false),
                q: Mutex::new(Vec::new()),
                errs: AtomicUsize::new(0),
                last_err: AtomicUsize::new(0),
                gave_up: AtomicBool::new(false),
                first: Mutex::new(Vec::new()),
                rx_bytes: AtomicUsize::new(0),
                not_sync: AtomicUsize::new(0),
            });

            // The reader. It exists so a listener is parked before any request
            // leaves the badge and stays parked for as long as bytes keep
            // coming. A dedicated `UsbHid` because `UsbHid` is not `Sync`;
            // `UsbHid::new()` is just a name-server lookup, so a second one is
            // cheap.
            //
            // It calls `wait_binary()` and not `UsbHid::serial_wait_binary()`
            // for the reason set out on `wait_binary`: the library call panics
            // on a `lend_mut` error after discarding which error it was, and
            // that panic is what ended the probe's round 4. Here an error is a
            // reading — counted, recorded, backed off, re-parked.
            {
                let rx = Arc::clone(&rx);
                std::thread::spawn(move || {
                    let reader_usb = usb_bao1x::UsbHid::new();
                    let conn = reader_usb.cid();
                    loop {
                        rx.parked.store(true, Ordering::Release);
                        match wait_binary(conn) {
                            Ok(d) => {
                                rx.parked.store(false, Ordering::Release);
                                if !d.is_empty() {
                                    // Fingerprint FIRST, before the `Vec` is
                                    // queued or moved anywhere. The whole value
                                    // of this reading is that it is taken at the
                                    // instant `wait_binary` returned; taking it
                                    // any later would measure the thing under
                                    // suspicion. See `Rx::first`.
                                    rx.rx_bytes.fetch_add(d.len(), Ordering::Relaxed);
                                    if d[0] != SYNC_LO {
                                        rx.not_sync.fetch_add(1, Ordering::Relaxed);
                                    }
                                    if let Ok(mut f) = rx.first.lock() {
                                        if f.is_empty() {
                                            f.extend_from_slice(
                                                &d[..d.len().min(RX_FINGERPRINT)],
                                            );
                                        }
                                    }
                                    if let Ok(mut q) = rx.q.lock() {
                                        q.push(d);
                                    }
                                }
                            }
                            Err(e) => {
                                rx.parked.store(false, Ordering::Release);
                                rx.last_err.store(e as usize, Ordering::Relaxed);
                                // Release, and last: the discriminant above must
                                // already be visible when the new count is.
                                let n = rx.errs.fetch_add(1, Ordering::Release) + 1;
                                if n >= RX_ERR_BUDGET {
                                    // Every failure has cost a page permanently.
                                    // Stop rather than leak the machine; `arm`
                                    // now times out and says why.
                                    rx.gave_up.store(true, Ordering::Release);
                                    return;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(
                                    RX_ERR_BACKOFF_MS,
                                ));
                            }
                        }
                    }
                });
            }

            // (The watchdog is spawned at the top of this function; it has to
            // precede the main-thread probe, which is what bounds that probe's
            // blocking `wait_binary`. It is also what delivers the stranded
            // tail of every page response — see the note on `BaoLink` in the
            // tests: a 4109-byte reply exceeds the 3840-byte delivery cap and
            // no second IRQ carries the remainder.)
            let mut t = Self {
                usb,
                tt,
                rx,
                reported_errs: 0,
                send_err: None,
                recv_spins_left: 0,
                unparks: 0,
                arm_fail: None,
                primed: false,
                tx: TxLog::default(),
            };
            // Prime: do not return until the reader has parked, because that
            // first park is the mode flip. A failure here is recorded rather
            // than discarded — an unprimed transport is one whose first response
            // gets `serial_buf.clear()`ed, and that is worth saying out loud
            // instead of presenting as a mystery timeout later.
            t.primed = t.arm().is_ok();
            t
        }

        /// Watchdog un-parks absorbed over the life of this transport.
        ///
        /// Expected to sit near `elapsed_seconds / FLUSH_MS * 1000` on an idle
        /// link, because the flush takes the listener whether or not it has
        /// anything to deliver. Far above that means the reader is losing the
        /// race to re-park, which would be worth knowing before it becomes a
        /// latency figure nobody can explain.
        pub fn unparks(&self) -> usize {
            self.unparks
        }

        fn drain(&mut self) -> Vec<u8> {
            let mut out = Vec::new();
            if let Ok(mut q) = self.rx.q.lock() {
                for d in q.drain(..) {
                    out.extend_from_slice(&d);
                }
            }
            out
        }
    }

    impl Transport for UsbTransport {
        /// Blocks until the reader is confirmed parked.
        ///
        /// The policy — including why an un-park is absorbed rather than
        /// reported, and why there is no fixed settle — is [`confirm_park`],
        /// which lives above the `cfg` so a laptop test can hold it. This is
        /// only the platform half: a flag, a clock, and a scheduler turn.
        fn arm(&mut self) -> Result<(), ()> {
            let t0 = self.tt.elapsed_ms();
            let rx = Arc::clone(&self.rx);
            let tt = &self.tt;
            let mut turns = 0usize;
            let r = confirm_park(
                || rx.parked.load(Ordering::Acquire),
                || tt.elapsed_ms().saturating_sub(t0),
                || {
                    // Yield first: on the fast path the reader is already parked
                    // and one turn is enough. Only a link that is actually stuck
                    // pays a sleep, and it pays it off the critical path.
                    turns += 1;
                    if turns <= SPIN_TURNS {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                    }
                },
                PARK_WAIT_MS,
            );
            match r {
                Ok(n) => {
                    self.unparks += n;
                    // A new exchange is about to start: give `recv` its spin
                    // budget back. This is the only place that knows an
                    // exchange boundary has been crossed.
                    self.recv_spins_left = SPIN_TURNS;
                    Ok(())
                }
                Err(n) => {
                    self.unparks += n;
                    self.arm_fail = Some(n);
                    Err(())
                }
            }
        }

        /// Loops, because `serial_send` truncates at `SERIAL_BINARY_BUFLEN`
        /// (3840) and returns only the contiguous prefix it accepted. A page
        /// frame is 4109 bytes, so this always takes at least two calls.
        ///
        /// **One packet per call, with a gap between them.** The policy, and
        /// the hardware defect it exists for, are [`send_paced`]; this leaf
        /// holds only the syscall and the gap. `TX_PACKET` (512) is far below
        /// `SERIAL_BINARY_BUFLEN`, so the truncation above no longer decides
        /// anything — but the loop still has to handle a short accept, because
        /// a full transmit buffer can still take less than a packet.
        ///
        /// The gap is a scheduler turn at [`TX_PACE_MS`] `== 0` and a sleep
        /// otherwise; see that constant for why zero is now the right value and
        /// what would make it wrong again. `yield_slice` rather than
        /// `yield_now`, because the thing that has to run is the USB server's
        /// interrupt handler, not another thread of ours.
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            debug_assert!(SERIAL_BINARY_BUFLEN >= TX_PACKET);
            let usb = &self.usb;
            let tt = &self.tt;
            // The clock the retry budget is not. See [`SEND_DEADLINE_MS`]: a
            // 13-byte request is one call and cannot loop, so only a page frame
            // can spend time in here, and only a page frame ever has.
            let t0 = tt.elapsed_ms();
            // See [`TxTally`]: what the sink *said* it took, call by call. The
            // twenty-third run failed with a writeback the host neither
            // rejected nor received, and no counter anywhere could say whether
            // the badge had handed the bytes over.
            let mut tally = TxTally::new(bytes.len());
            let paced = send_paced(
                |b| {
                    let r = usb.serial_send(b);
                    tally.record(b.len(), &r);
                    r
                },
                || {
                    if TX_PACE_MS == 0 {
                        xous::yield_slice();
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(TX_PACE_MS));
                    }
                },
                || tt.elapsed_ms().saturating_sub(t0) > SEND_DEADLINE_MS,
                TX_PACKET,
                bytes,
            );
            // ---- push the tail, rather than waiting for the watchdog ----
            // `serial_send` reports what `usbd-serial` *buffered*, not what
            // went on the wire: `SerialPort::write` returns
            // `write_buf.write(data)` and its inner `flush` is allowed to fail
            // with `WouldBlock`. Since
            // `badge/bao1x-hal-usb-in-completion.patch` that failure is routine
            // -- a bulk IN endpoint refuses while a transfer is in flight -- so
            // when the last packet of a frame is handed over there can be up to
            // a full 1024-byte transmit ring still sitting in the class, and
            // the only things that drive it out are `endpoint_in_complete` and
            // the 5 ms flush watchdog.
            //
            // A stranded tail is invisible from both ends, which is exactly the
            // failure this round is about: the host's decoder holds an
            // incomplete frame silently (`Decoder::next_frame` returns `None`
            // and captures no noise), so the transcript shows the frame neither
            // accepted nor discarded. One scalar IPC after the last packet
            // removes that from the list.
            //
            // Multi-packet frames only. A request is one packet, has never
            // failed, and the flush also un-parks the reader (`SerialFlush`
            // calls `serial_listener.take()` unconditionally) -- routine,
            // absorbed by [`confirm_park`], and still not worth paying on the
            // path that works.
            if tally.multi_packet() {
                usb.serial_flush().ok();
                tally.record_flush();
            }
            tally.ms = tt.elapsed_ms().saturating_sub(t0);
            self.tx.finish(tally);
            match paced {
                Ok(()) => Ok(()),
                Err(SendFault::Link(e)) => {
                    self.send_err = Some(e);
                    Err(SendError::Failed)
                }
                // 65 consecutive `Ok(0)`s. On the badge that is overwhelmingly
                // the cable, and reporting it as itself is what makes a
                // flash-and-capture cycle readable.
                Err(SendFault::Stalled) => Err(SendError::Stalled),
                // The sink kept accepting and never drained. A multi-packet
                // frame is the only thing that can reach this.
                Err(SendFault::TimedOut { sent, len }) => Err(SendError::TimedOut { sent, len }),
            }
        }

        /// Waits up to one poll tick for the reader to publish something. The
        /// caller's deadline is what bounds the overall wait; returning empty is
        /// how this reports "nothing yet".
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            // Spin first, yielding so the reader runs. A page fault is
            // outstanding and the main thread has nothing else to do, so this
            // costs no useful work — and it keeps `POLL_MS` off the critical
            // path, which is the difference between a page fault costing the
            // wire round trip and costing the round trip plus a sleep per
            // delivery. See the round-trip note in the module docs.
            while self.recv_spins_left > 0 {
                let got = self.drain();
                if !got.is_empty() {
                    return Ok(got);
                }
                self.recv_spins_left -= 1;
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            Ok(self.drain())
        }

        /// Whatever the reader has already published, with no wait at all.
        fn poll(&mut self) -> Result<Vec<u8>, ()> {
            Ok(self.drain())
        }

        fn now_ms(&mut self) -> u64 {
            self.tt.elapsed_ms()
        }

        /// Always answerable, healthy or not — see [`Transport::status`].
        ///
        /// Every field here decides how to read a timeout: whether the reader
        /// is parked *right now*, whether it has stopped, how many listener
        /// lends have failed, and how many watchdog un-parks have been
        /// absorbed. An un-park rate far above `1000 / FLUSH_MS` means the
        /// reader is losing the race to re-park, which is a receive path that
        /// is present but not working — the case a bare "no answer" hides.
        fn status(&mut self) -> Option<String> {
            // The fingerprint is the load-bearing part: compared against the
            // decoder's discarded sample it says whether the bytes were already
            // wrong when the reader got them. See `Rx::first`.
            let first = self
                .rx
                .first
                .lock()
                .map(|f| {
                    f.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
                })
                .unwrap_or_else(|_| "<poisoned>".into());
            Some(format!(
                "reader: parked={} stopped={} lend_errs={} unparks={} primed={} \
                 rx_bytes={} not_sync={} first_delivery=[{}] | {} | {}",
                self.rx.parked.load(Ordering::Acquire),
                self.rx.gave_up.load(Ordering::Acquire),
                self.rx.errs.load(Ordering::Acquire),
                self.unparks,
                self.primed,
                self.rx.rx_bytes.load(Ordering::Relaxed),
                self.rx.not_sync.load(Ordering::Relaxed),
                first,
                self.tx.describe(),
                address_report(),
            ))
        }

        fn take_platform_fault(&mut self) -> Option<String> {
            // First, and ungated by any "reported once" counter. This is the one
            // line someone will be staring at a transcript hoping to find, and
            // once the reader has stopped every subsequent exchange fails for
            // exactly this reason — so every one of them should say so.
            if self.rx.gave_up.load(Ordering::Acquire) {
                let n = self.rx.errs.load(Ordering::Acquire);
                return Some(format!(
                    "reader STOPPED after {n} listener lends failed (budget {RX_ERR_BUDGET}), \
                     last {}; nothing will park again and every exchange from here fails",
                    err_name(self.rx.last_err.load(Ordering::Relaxed)),
                ));
            }
            if let Some(e) = self.send_err.take() {
                return Some(format!("serial_send: {}", err_name(e as usize)));
            }
            if let Some(n) = self.arm_fail.take() {
                return Some(format!(
                    "no park confirmed in {PARK_WAIT_MS} ms after absorbing {n} watchdog un-parks"
                ));
            }
            if !self.primed {
                self.primed = true; // report once
                return Some(
                    "the reader never parked during construction: the listen mode may \
                     still be NoListener or ConsoleListener, in which case the first \
                     response was cleared rather than queued"
                        .into(),
                );
            }
            let n = self.rx.errs.load(Ordering::Acquire);
            if n > self.reported_errs {
                self.reported_errs = n;
                let code = self.rx.last_err.load(Ordering::Relaxed);
                return Some(format!(
                    "listener lend failed {n}x, last {}; each costs one page permanently",
                    err_name(code),
                ));
            }
            None
        }
    }

    /// Ask for a bigger process heap before anything allocates.
    ///
    /// The default ceiling is 512 KiB unless the kernel carries `big-heap`, and
    /// the probe measured 308 KiB free. This link allocates a `Box<[u8; 4096]>`
    /// per response — `rv64_proto`'s decoder builds it — against roughly 16,000
    /// page operations per boot, so the ceiling is worth raising even though the
    /// request buffer is now reused rather than reallocated.
    ///
    /// `AdjustProcessLimit` is compare-and-set: the first call is a deliberate
    /// no-op read, the second one writes.
    fn raise_heap_ceiling() {
        let hm = xous::Limits::HeapMaximum as usize;
        if let Ok(xous::Result::Scalar2(_, cur)) =
            xous::rsyscall(xous::SysCall::AdjustProcessLimit(hm, 0, HEAP_MAX))
        {
            xous::rsyscall(xous::SysCall::AdjustProcessLimit(hm, cur, HEAP_MAX)).ok();
        }
    }

    /// The name of a `xous::Error` discriminant, for the few this path can
    /// produce. `xous::Error` is an explicit-discriminant enum
    /// (`xous-rs/src/definitions.rs:117-158`).
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

    // -----------------------------------------------------------------
    // The address report
    // -----------------------------------------------------------------

    /// The archived form of the struct the USB server writes into the lent
    /// page. Named through `rkyv` rather than hardcoded, so its size and
    /// alignment below are *computed* — the point of this diagnostic is that it
    /// must not encode my guess about the layout.
    type ArchivedBinary = <usb_bao1x::UsbSerialBinary as rkyv::Archive>::Archived;

    /// Captured once, on the first delivery. `AcqRel` swap makes "once" mean
    /// once even though the main-thread probe and the reader thread both call
    /// `wait_binary`.
    static ADDR_DONE: AtomicBool = AtomicBool::new(false);
    static A_BASE: AtomicUsize = AtomicUsize::new(0);
    static A_PAGELEN: AtomicUsize = AtomicUsize::new(0);
    static A_USED: AtomicUsize = AtomicUsize::new(0);
    static A_ROOT: AtomicUsize = AtomicUsize::new(0);
    static A_RELOFF: AtomicUsize = AtomicUsize::new(0);
    static A_RESOLVED: AtomicUsize = AtomicUsize::new(0);
    static A_VECLEN: AtomicUsize = AtomicUsize::new(0);
    static A_TEXT: AtomicUsize = AtomicUsize::new(0);
    static A_HEAP: AtomicUsize = AtomicUsize::new(0);
    static A_STACK: AtomicUsize = AtomicUsize::new(0);
    /// The first 32 bytes **at `base`** -- the page start, where rkyv lays the
    /// payload down before the root.
    static A_HEAD: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    /// 16 bytes at `base + 512` and at `base + 1024`, i.e. at the boundaries of
    /// the driver's [`SERIAL_MAX_PACKET_SIZE`]-byte staging buffer.
    ///
    /// **This is the experiment.** `usb-bao1x`'s IRQ handler reads each CDC
    /// packet into a *single shared* `serial_rx: [u8; 512]` and posts a scalar
    /// carrying only the byte count (`hw.rs:333-339`); the main loop appends
    /// `&cu.serial_rx[..valid_bytes]` later (`main.rs:595`), and
    /// `claim_interrupt` hands the IRQ the same `Box<Bao1xUsb>` the main loop
    /// holds (`hw.rs:137-140`), so the two are the same 512 bytes. If a second
    /// packet lands before the main loop drains the first message, the older
    /// count is satisfied from the newer bytes.
    ///
    /// A 4109-byte reply is nine such packets back to back. If this is what is
    /// happening, the page will contain **the same 512-byte block repeated** --
    /// so `base`, `base + 512` and `base + 1024` will be identical. That is a
    /// yes/no answer to a defect I can point at in the source, and it is
    /// cheaper than changing anything.
    static A_PKT1: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    static A_PKT2: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    /// Same value as [`A_VECLEN`]; a separate cell only so the byte-window
    /// block below can read it without threading a local through the `unsafe`.
    static A_VECLEN_LOCAL: AtomicUsize = AtomicUsize::new(0);
    /// The 8 bytes at the root position, i.e. what is being interpreted as the
    /// archived header.
    static A_ROOTB: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    /// What `used` should be for a payload of `n` bytes.
    ///
    /// rkyv lays the payload down first and the root last, padding the payload
    /// up to the root's alignment, so `used = round_up(n, 4) + 8`. Verified
    /// against rkyv 0.8.18 on the host rather than derived by eye -- it returns
    /// 8, 12, 24, 280, 392 and 3848 for payloads of 0, 1, 13, 269, 384 and
    /// 3840, and the badge's own empty-delivery capture reported exactly
    /// `used=8, veclen=0`, which is this formula's answer for `n = 0`.
    ///
    /// Printed next to the observed `used` so the badge compares them itself.
    /// A mismatch is the bug, named on the device.
    fn expected_used(veclen: usize) -> usize {
        let align = core::mem::align_of::<ArchivedBinary>();
        veclen.next_multiple_of(align) + core::mem::size_of::<ArchivedBinary>()
    }

    /// **Records where the deserialiser is actually pointing.**
    ///
    /// Eight hardware runs have narrowed this to: the delivery is already wrong
    /// when `wait_binary` returns; the *lengths* are always exactly right and
    /// track reality; the payload is stable and byte-identical across two
    /// binaries with different `.text` sizes and layouts. That last fact is what
    /// killed the remaining content-based theories — including that we were
    /// reading our own code, which cannot be true if the bytes do not move when
    /// the code does.
    ///
    /// So stop looking at bytes. `ArchivedVec` is a relative pointer plus a
    /// length; the length is demonstrably fine, so the pointer is what is wrong,
    /// and an address names the region outright where six theories could not.
    ///
    /// What is captured, all from the same delivery:
    ///
    /// * `base`/`pagelen` — the lent page, from `Buffer::to_raw_parts`.
    /// * `used` — the offset the **server** returned, which `lend_mut` copied
    ///   into the client `Buffer` and which decides where the root is read from.
    /// * `arch` — `size_of`/`align_of` of the archived root. If this app and the
    ///   flashed `xous.uf2` resolved rkyv's additive `pointer_width_*` features
    ///   differently, this is where it shows: 8 bytes means 32-bit
    ///   `ArchivedUsize`, 16 means 64-bit, and the two ends must agree.
    /// * `root` — `base + used - size_of::<Archived>()`, exactly what
    ///   `rkyv::access_unchecked` computes, and its alignment — which is the
    ///   check rkyv itself makes only under `debug_assertions`.
    /// * `reloff`/`resolved` — the relative pointer as stored, and the address
    ///   it resolves to. `RawRelPtr::as_ptr_raw` is
    ///   `base_raw(this).offset(offset_raw(this))` and `base_raw` is the
    ///   pointer's *own* address, so `resolved = root + reloff`.
    /// * `veclen` — the length beside it, expected correct, as a self-check that
    ///   the root was found at all.
    /// * `text`/`heap`/`stack` — one known address from each region of this
    ///   process, so `resolved` can be placed without a map. Anything far from
    ///   all three is somebody else's memory.
    ///
    /// Reads are `read_unaligned` throughout: a misaligned root is one of the
    /// things being measured, so the instrument must not fault on it.
    fn capture_addresses(buf: &xous_ipc::Buffer) {
        if ADDR_DONE.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: read-only inspection of the buffer's own bookkeeping.
        let (base, pagelen, used) = unsafe { buf.to_raw_parts() };

        // **Only a delivery that carried something.** The first capture caught
        // `used=8, veclen=0` -- a correct, empty archive, which is routine with
        // the flush watchdog running and says nothing about the data path. It
        // did prove the boundary works for the empty case, which is worth
        // having; it is simply not the case under investigation. Keep looking
        // until a non-empty one arrives.
        if used <= core::mem::size_of::<ArchivedBinary>() || used > pagelen {
            return;
        }
        if ADDR_DONE.swap(true, Ordering::AcqRel) {
            return;
        }
        A_BASE.store(base, Ordering::Relaxed);
        A_PAGELEN.store(pagelen, Ordering::Relaxed);
        A_USED.store(used, Ordering::Relaxed);

        let asize = core::mem::size_of::<ArchivedBinary>();
        {
            let root = base + used - asize;
            A_ROOT.store(root, Ordering::Relaxed);
            // `ArchivedVec<u8>` is `{ ptr: RelPtr<u8>, len: ArchivedUsize }`,
            // the pointer first at offset 0. Widths follow the archived size so
            // this is right for either `pointer_width` resolution.
            let half = asize / 2;
            // SAFETY: `root .. root + asize` is inside the mapped page by the
            // bounds check above, and every read is unaligned-safe.
            unsafe {
                let p = root as *const u8;
                let (reloff, veclen): (isize, usize) = if half == 8 {
                    (
                        core::ptr::read_unaligned(p as *const i64) as isize,
                        core::ptr::read_unaligned(p.add(half) as *const u64) as usize,
                    )
                } else {
                    (
                        core::ptr::read_unaligned(p as *const i32) as isize,
                        core::ptr::read_unaligned(p.add(half) as *const u32) as usize,
                    )
                };
                A_RELOFF.store(reloff as usize, Ordering::Relaxed);
                A_VECLEN.store(veclen, Ordering::Relaxed);
                A_VECLEN_LOCAL.store(veclen, Ordering::Relaxed);
                A_RESOLVED.store((root as isize).wrapping_add(reloff) as usize, Ordering::Relaxed);
            }
        }

        // The two byte windows that separate "correct data, wrong root" from
        // "wrong data". rkyv writes the payload first and the root last, so on
        // a correct archive the frame starts at `base`:
        //   `head` = c1 b0 02 04 10 ...  -> the server wrote our frame and only
        //                                   the root offset is wrong
        //   `head` = 13 00 00 00 ...     -> the page itself holds the wrong
        //                                   data and the fault is upstream of
        //                                   the archive entirely
        // SAFETY: both windows are inside the mapped page -- `used <= pagelen`
        // was checked above and `root = base + used - asize`.
        unsafe {
            let head = core::slice::from_raw_parts(base as *const u8, 32.min(pagelen));
            if let Ok(mut h) = A_HEAD.lock() {
                h.extend_from_slice(head);
            }
            // The staging-buffer test: same offset within the next two
            // 512-byte packets. Identical to `head` means the driver re-read
            // one buffer for several packets' worth of counts.
            for (off, slot) in [(512usize, &A_PKT1), (1024usize, &A_PKT2)] {
                if off + 16 <= A_VECLEN_LOCAL.load(Ordering::Relaxed).min(pagelen) {
                    let w = core::slice::from_raw_parts((base + off) as *const u8, 16);
                    if let Ok(mut v) = slot.lock() {
                        v.extend_from_slice(w);
                    }
                }
            }
            let rootb = core::slice::from_raw_parts((base + used - asize) as *const u8, asize);
            if let Ok(mut r) = A_ROOTB.lock() {
                r.extend_from_slice(rootb);
            }
        }

        // One known address per region, so the resolved pointer can be placed
        // without a memory map.
        A_TEXT.store(capture_addresses as *const () as usize, Ordering::Relaxed);
        let heap = std::boxed::Box::new(0u8);
        A_HEAP.store(&*heap as *const u8 as usize, Ordering::Relaxed);
        let stack_probe = 0u8;
        A_STACK.store(&stack_probe as *const u8 as usize, Ordering::Relaxed);
    }

    /// The captured addresses, as one line of hex for the wire.
    fn address_report() -> String {
        if !ADDR_DONE.load(Ordering::Acquire) {
            return "addrs: no non-empty delivery captured yet".into();
        }
        let hex = |m: &Mutex<Vec<u8>>| {
            m.lock()
                .map(|v| v.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|_| "<poisoned>".into())
        };
        let asize = core::mem::size_of::<ArchivedBinary>();
        let aalign = core::mem::align_of::<ArchivedBinary>();
        let root = A_ROOT.load(Ordering::Relaxed);
        format!(
            "addrs: base={:#010x} pagelen={} used={} (expected {} for veclen) \
             arch=size{}/align{} root={:#010x} root_align={} reloff={} \
             resolved={:#010x} veclen={} | head@base=[{}] +512=[{}] +1024=[{}] \
             pkt_repeat={} rootbytes=[{}] \
             | text={:#010x} heap={:#010x} stack={:#010x}",
            A_BASE.load(Ordering::Relaxed),
            A_PAGELEN.load(Ordering::Relaxed),
            A_USED.load(Ordering::Relaxed),
            expected_used(A_VECLEN.load(Ordering::Relaxed)),
            asize,
            aalign,
            root,
            root % aalign,
            A_RELOFF.load(Ordering::Relaxed) as isize,
            A_RESOLVED.load(Ordering::Relaxed),
            A_VECLEN.load(Ordering::Relaxed),
            hex(&A_HEAD),
            hex(&A_PKT1),
            hex(&A_PKT2),
            // The verdict, computed on the badge: are the 512-byte packet
            // windows the same bytes?
            {
                let h = A_HEAD.lock().map(|v| v[..16.min(v.len())].to_vec()).unwrap_or_default();
                let p1 = A_PKT1.lock().map(|v| v.clone()).unwrap_or_default();
                let p2 = A_PKT2.lock().map(|v| v.clone()).unwrap_or_default();
                if p1.is_empty() {
                    "n/a".to_string()
                } else {
                    format!("{}", h == p1 && (p2.is_empty() || h == p2))
                }
            },
            hex(&A_ROOTB),
            A_TEXT.load(Ordering::Relaxed),
            A_HEAP.load(Ordering::Relaxed),
            A_STACK.load(Ordering::Relaxed),
        )
    }

    /// The three lines of `usb_bao1x::UsbHid::serial_wait_binary()`, inlined,
    /// with the error kept instead of discarded — and with the failed `Buffer`
    /// defused instead of dropped.
    ///
    /// The library call does `.or(Err(xous::Error::InternalError))` on the
    /// `lend_mut` and then `.expect("Internal error")`s it
    /// (`services/usb-bao1x/src/lib.rs:243`), so a real fault panics with a
    /// substituted message: the probe's round 4 died on hardware showing
    /// `Internal error` when the actual error was `BadAddress`.
    ///
    /// `core::mem::forget` on the failure path is not a tolerated leak. When
    /// `lend_mut` fails after the kernel has already lent the page, the page is
    /// mapped into the USB server's window with no queued message referencing it
    /// and nothing will ever return it. `Buffer`'s `Drop` would call
    /// `unmap_memory(...).expect("Buffer: failed to drop memory")` on it — a
    /// second panic on top of the first. The choice is between losing the page
    /// quietly and losing the page plus the process. See
    /// `../probe/src/main.rs`'s `wait_binary` for the full autopsy.
    fn wait_binary(conn: xous::CID) -> Result<Vec<u8>, xous::Error> {
        let req = usb_bao1x::UsbSerialBinary { d: Vec::new() };
        let mut buf = xous_ipc::Buffer::into_buf(req).or(Err(xous::Error::InternalError))?;
        if let Err(e) = buf.lend_mut(conn, usb_bao1x::Opcode::SerialHookBinary.to_u32().unwrap()) {
            core::mem::forget(buf);
            return Err(e);
        }
        // Between the lend returning and the deserialise: this is the only
        // moment the buffer's own bookkeeping and the server's returned offset
        // are both true and nothing has interpreted them yet. Captured once.
        capture_addresses(&buf);
        buf.to_original::<usb_bao1x::UsbSerialBinary, _>().map(|r| r.d)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rv64::backing::MemBacking;

    /// A transport that answers every READ with a page full of `fill`.
    ///
    /// This is the brief's Step 1 fake, with the two methods the trait gained in
    /// fix round 1: `arm`, which it has nothing to park for, and `now_ms`, whose
    /// frozen clock is correct here because this fake always answers before the
    /// first poll.
    struct Loopback {
        fill: u8,
        pending: Vec<u8>,
    }
    impl Transport for Loopback {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = rv64_proto::Mux::new();
            m.push(bytes);
            if let Some(rv64_proto::Frame::ReadReq { page }) = m.take_matching(0x01) {
                let mut out = Vec::new();
                rv64_proto::encode(
                    &rv64_proto::Frame::ReadResp { page, data: Box::new([self.fill; 4096]) },
                    &mut out,
                );
                self.pending = out;
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            Ok(core::mem::take(&mut self.pending))
        }
        fn now_ms(&mut self) -> u64 {
            0
        }
    }

    #[test]
    fn a_read_returns_the_page_the_host_sent() {
        let mut h = UsbHost::new(Loopback { fill: 0x42, pending: Vec::new() });
        let mut buf = [0u8; 4096];
        h.read_page(9, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x42));
    }

    // -- F1: park before send ----------------------------------------------

    /// Records the order of `arm` and `send`, and refuses to answer a request
    /// that was sent without a confirmed park — which is what the badge's USB
    /// service does, except that on the badge it does it by hanging forever with
    /// no output rather than by failing a test.
    struct ParkOrderLink {
        armed: bool,
        pending: Vec<u8>,
        /// Requests that left before a park was confirmed. On hardware each of
        /// these is a permanent silent hang.
        sent_unparked: usize,
        arms: usize,
        clock: u64,
    }

    impl ParkOrderLink {
        fn new() -> Self {
            Self { armed: false, pending: Vec::new(), sent_unparked: 0, arms: 0, clock: 0 }
        }
    }

    impl Transport for ParkOrderLink {
        fn arm(&mut self) -> Result<(), ()> {
            self.arms += 1;
            self.armed = true;
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            if !self.armed {
                // No listener parked: `SerialHookBinary` will not drain
                // `serial_buf`, and against a synchronous peer nothing else
                // ever will. The reply is lost, so this fake loses it too.
                self.sent_unparked += 1;
                return Ok(());
            }
            // A delivery consumes the parked listener, exactly as the service
            // does: `serial_listener.take()`.
            self.armed = false;
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                encode(&Frame::ReadResp { page, data: Box::new([0x7c; PAGE]) }, &mut self.pending);
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.pending.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.pending.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    /// **The regression test for the defect that hangs the badge.**
    ///
    /// If `exchange` ever sends before `arm()` has returned, this read gets no
    /// reply at all and times out. Nothing else in the suite would notice: the
    /// other fakes answer regardless of park state, because a `Vec` has no
    /// listener to lose the race with.
    #[test]
    fn park_is_confirmed_before_any_request_byte_is_sent() {
        let mut h = UsbHost::new(ParkOrderLink::new());
        let link = h.link();
        let mut buf = [0u8; PAGE];
        h.read_page(11, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x7c));

        let i = h.link.inner.borrow();
        assert_eq!(i.t.sent_unparked, 0, "a request left the badge with no listener parked");
        assert_eq!(i.t.arms, 1, "exactly one park per exchange");
        drop(i);
        assert_eq!(link.take_fault(), None);
    }

    /// The same link, driven the wrong way round, to prove the fake can actually
    /// see the defect rather than passing vacuously: a request sent while
    /// unarmed is never answered.
    #[test]
    fn the_park_order_fake_really_does_lose_an_unparked_request() {
        let mut t = ParkOrderLink::new();
        let mut req = Vec::new();
        encode(&Frame::ReadReq { page: 1 }, &mut req);
        t.send(&req).unwrap(); // no arm() first
        assert_eq!(t.sent_unparked, 1);
        assert_eq!(t.recv().unwrap(), Vec::<u8>::new(), "the reply was lost, as on hardware");
    }

    #[test]
    fn every_exchange_parks_again_because_a_delivery_consumes_the_listener() {
        let mut h = UsbHost::new(ParkOrderLink::new());
        let mut buf = [0u8; PAGE];
        for p in 0..5 {
            h.read_page(p, &mut buf).unwrap();
        }
        let i = h.link.inner.borrow();
        assert_eq!(i.t.arms, 5);
        assert_eq!(i.t.sent_unparked, 0);
    }

    // -- N1: a watchdog un-park is routine, not a fault --------------------

    /// Drives [`confirm_park`] the way the badge's own flush watchdog drives it:
    /// the listener is taken unconditionally every `FLUSH_MS`, so a sample
    /// sometimes lands mid-re-park.
    ///
    /// **This is the regression test for N1.** If `confirm_park` goes back to
    /// returning an error on one bad post-settle sample, this fails. It cannot
    /// be written against `UsbTransport` — that is `cfg(target_os = "xous")` —
    /// which is exactly why the policy was lifted out of it.
    #[test]
    fn a_watchdog_unpark_is_absorbed_rather_than_reported_as_a_fault() {
        // parked, parked, then the flush takes the listener (false), then the
        // reader re-parks. The second `true` in each pair is the post-settle
        // re-check, so this sequence makes the *confirm* lose, not the wait.
        let samples = [true, false, true, false, true, true];
        let i = std::cell::Cell::new(0usize);
        let clock = std::cell::Cell::new(0u64);
        let r = confirm_park(
            || {
                let v = samples[i.get().min(samples.len() - 1)];
                i.set(i.get() + 1);
                v
            },
            || clock.get(),
            || clock.set(clock.get() + 1),
            1000,
        );
        assert_eq!(r, Ok(2), "two un-parks absorbed, and neither is an error");
    }

    /// The fast path — the reader has been parked for milliseconds — must not
    /// pay a fixed sleep. `settle` is called exactly once: the confirm turn.
    ///
    /// This is N2's half of the same change. A regression to
    /// `sleep(PARK_SETTLE_MS)` before the check would put 2 ms on every page
    /// fault, which is most of the round trip the module is justified by.
    #[test]
    fn confirming_an_already_parked_listener_costs_one_scheduler_turn() {
        let settles = std::cell::Cell::new(0usize);
        let clock = std::cell::Cell::new(0u64);
        let r = confirm_park(
            || true,
            || clock.get(),
            || {
                settles.set(settles.get() + 1);
                clock.set(clock.get() + 1);
            },
            1000,
        );
        let settles = settles.get();
        assert_eq!(r, Ok(0));
        assert_eq!(settles, 1, "the fast path must not sleep, only yield once");
    }

    /// A link where nothing ever parks still terminates, on the deadline.
    #[test]
    fn a_listener_that_never_parks_gives_up_on_the_deadline() {
        let clock = std::cell::Cell::new(0u64);
        let r = confirm_park(|| false, || clock.get(), || clock.set(clock.get() + 1), 10);
        assert_eq!(r, Err(0));
    }

    /// A link that un-parks forever — the reader wedged mid-cycle — must also
    /// terminate rather than absorb un-parks until the heat death.
    #[test]
    fn perpetual_unparking_still_terminates_on_the_deadline() {
        let flip = std::cell::Cell::new(false);
        let clock = std::cell::Cell::new(0u64);
        let r = confirm_park(
            || {
                flip.set(!flip.get());
                flip.get()
            },
            || clock.get(),
            || clock.set(clock.get() + 1),
            50,
        );
        assert!(r.is_err(), "an un-park storm must hit the deadline, not spin");
        assert!(matches!(r, Err(n) if n > 0), "and it must have counted them");
    }

    // -- the badge-shaped loopback ------------------------------------------

    /// A `Transport` that behaves the way the badge's USB stack was measured to
    /// behave, backed by real storage so reads and writes persist.
    ///
    /// It truncates every send at [`SERIAL_BINARY_BUFLEN`] and reports the
    /// accepted prefix; hands back at most that much per `recv`; keeps **one**
    /// `Mux` for the whole connection as `rv64_host::serve` does; and tracks the
    /// peak number of CDC packets queued toward the badge, so the 128-slot
    /// budget is a checked property of a whole conformance run.
    struct MemoryLoopback {
        pages: Vec<[u8; PAGE]>,
        host_mux: Mux,
        to_badge: Vec<u8>,
        /// Console bytes to interleave ahead of the next response.
        inject_console: Vec<u8>,
        /// Console bytes deliverable only through `poll`, i.e. arriving with no
        /// page request to ride along with.
        async_console: Vec<u8>,
        send_calls: usize,
        deliveries: usize,
        peak_packets: usize,
        clock: u64,
    }

    impl MemoryLoopback {
        fn new(pages: u32) -> Self {
            Self {
                pages: vec![[0u8; PAGE]; pages as usize],
                host_mux: Mux::new(),
                to_badge: Vec::new(),
                inject_console: Vec::new(),
                async_console: Vec::new(),
                send_calls: 0,
                deliveries: 0,
                peak_packets: 0,
                clock: 0,
            }
        }

        fn peak_queue_slots(&self) -> usize {
            self.peak_packets
        }
    }

    impl Transport for MemoryLoopback {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }

        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            // Push through the same `send_all` the badge uses, against a sink
            // that truncates exactly like `serial_send`.
            let mut accepted: Vec<u8> = Vec::new();
            let mut calls = 0usize;
            send_all(
                |b| {
                    calls += 1;
                    let n = b.len().min(SERIAL_BINARY_BUFLEN);
                    accepted.extend_from_slice(&b[..n]);
                    Ok::<usize, ()>(n)
                },
                bytes,
            )
            .map_err(|_| SendError::Failed)?;
            self.send_calls += calls;

            self.host_mux.push(&accepted);

            if !self.inject_console.is_empty() {
                let c = std::mem::take(&mut self.inject_console);
                encode(&Frame::ConIn(c), &mut self.to_badge);
            }

            while let Some(f) =
                self.host_mux.take_matching(0x01).or_else(|| self.host_mux.take_matching(0x03))
            {
                let reply = match f {
                    Frame::ReadReq { page } => match self.pages.get(page as usize) {
                        Some(p) => Frame::ReadResp { page, data: Box::new(*p) },
                        None => Frame::Err { code: ERR_READ, page },
                    },
                    Frame::WriteReq { page, data } => match self.pages.get_mut(page as usize) {
                        Some(p) => {
                            p.copy_from_slice(&data[..]);
                            Frame::WriteAck { page }
                        }
                        None => Frame::Err { code: ERR_WRITE, page },
                    },
                    _ => unreachable!("take_matching only yields the types we asked for"),
                };
                encode(&reply, &mut self.to_badge);
            }

            // Everything queued and not yet drained is occupying slots.
            self.peak_packets = self.peak_packets.max(self.to_badge.len().div_ceil(CDC_PACKET));
            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.to_badge.len().min(SERIAL_BINARY_BUFLEN);
            self.deliveries += 1;
            Ok(self.to_badge.drain(..n).collect())
        }

        /// Console bytes that arrived with no page request to ride along with —
        /// a human typing at a warm cache.
        fn poll(&mut self) -> Result<Vec<u8>, ()> {
            if self.async_console.is_empty() {
                return Ok(Vec::new());
            }
            let c = std::mem::take(&mut self.async_console);
            let mut out = Vec::new();
            encode(&Frame::ConIn(c), &mut out);
            Ok(out)
        }

        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn usbhost_passes_conformance() {
        let mut h = UsbHost::new(MemoryLoopback::new(64));
        rv64::backing::conformance(&mut h, 64);
    }

    #[test]
    fn a_page_read_needs_several_deliveries_and_the_mux_accumulates_them() {
        let mut h = UsbHost::new(MemoryLoopback::new(4));
        let mut w = [0u8; PAGE];
        for (i, b) in w.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        h.write_page(2, &w).unwrap();
        let mut r = [0u8; PAGE];
        h.read_page(2, &mut r).unwrap();
        assert_eq!(r, w);

        let i = h.link.inner.borrow();
        assert!(i.t.deliveries > 2, "a page must arrive in several deliveries");
        assert!(i.t.send_calls > 2, "a WriteReq must take several sends");
    }

    // -- the transmit instrument -------------------------------------------

    /// The reading that the twenty-third run needed and could not produce: a
    /// healthy page frame, counted where the bytes change hands.
    ///
    /// Nine calls, 4109 accepted, nothing refused. If a hardware transcript
    /// ever shows this line next to a `WriteReq` the host never saw, then
    /// everything from `send_paced` upwards handed the frame over and the loss
    /// is below `usbd-serial` -- `CorigineWrapper::write`, `bulk_xfer`, or the
    /// wire -- which is a firmware question and not an app one.
    #[test]
    fn a_healthy_page_frame_tallies_nine_calls_and_every_byte() {
        let frame = vec![0xa5u8; MAX_FRAME];
        let mut tally = TxTally::new(frame.len());
        let r: Result<(), SendFault<()>> = send_paced(
            |b| {
                let r: Result<usize, ()> = Ok(b.len());
                tally.record(b.len(), &r);
                r
            },
            || {},
            || false,
            TX_PACKET,
            &frame,
        );
        assert!(r.is_ok());
        assert_eq!(tally.calls, MAX_FRAME.div_ceil(TX_PACKET));
        assert_eq!(tally.accepted, MAX_FRAME);
        assert_eq!(tally.asked, MAX_FRAME);
        assert_eq!(tally.refusals, 0);
        assert_eq!(tally.shorts, 0);
        assert!(tally.multi_packet(), "a page frame is the only multi-packet frame");
    }

    /// A request is one call and is not a multi-packet frame, so it does not
    /// take the extra flush the leaf issues for a page. The distinction is what
    /// keeps the instrument -- and the flush -- off the path that works.
    #[test]
    fn a_request_is_one_call_and_not_multi_packet() {
        let mut out = Vec::new();
        encode(&Frame::ReadReq { page: 7 }, &mut out);
        let mut tally = TxTally::new(out.len());
        let r: Result<(), SendFault<()>> = send_paced(
            |b| {
                let r: Result<usize, ()> = Ok(b.len());
                tally.record(b.len(), &r);
                r
            },
            || {},
            || false,
            TX_PACKET,
            &out,
        );
        assert!(r.is_ok());
        assert_eq!(tally.calls, 1);
        assert_eq!(tally.accepted, out.len());
        assert!(!tally.multi_packet());
    }

    /// A sink that takes a prefix and a sink that takes nothing are different
    /// readings, and the tally has to tell them apart: `shorts` is
    /// back-pressure that is still moving, `refusals` is a transmit path that
    /// is not.
    #[test]
    fn short_accepts_and_refusals_are_counted_apart() {
        let frame = vec![0u8; 3 * TX_PACKET];
        let mut tally = TxTally::new(frame.len());
        let mut turn = 0usize;
        let r: Result<(), SendFault<()>> = send_paced(
            |b| {
                turn += 1;
                // refuse, then take a quarter, then take everything
                let n = match turn % 3 {
                    1 => 0,
                    2 => b.len() / 4,
                    _ => b.len(),
                };
                let r: Result<usize, ()> = Ok(n);
                tally.record(b.len(), &r);
                r
            },
            || {},
            || false,
            TX_PACKET,
            &frame,
        );
        assert!(r.is_ok());
        assert_eq!(tally.accepted, frame.len(), "every byte still gets handed over");
        assert!(tally.refusals > 0, "the Ok(0)s are visible");
        assert!(tally.shorts > 0, "the partial accepts are visible, and separately");
    }

    /// The lifetime totals answer the one question twenty-two hardware rounds
    /// could not read off a transcript: has this link ever transmitted a
    /// multi-packet frame at all?
    #[test]
    fn the_log_separates_multi_packet_frames_from_the_rest() {
        let mut log = TxLog::default();
        let mut req = TxTally::new(13);
        req.record::<()>(13, &Ok(13));
        log.finish(req);
        let mut page = TxTally::new(MAX_FRAME);
        for _ in 0..MAX_FRAME.div_ceil(TX_PACKET) {
            page.record::<()>(TX_PACKET, &Ok(TX_PACKET));
        }
        log.finish(page);

        assert_eq!(log.frames, 2);
        assert_eq!(log.multi_frames, 1);
        assert_eq!(log.last.asked, MAX_FRAME);
        let line = log.describe();
        assert!(line.contains("life 2 frames (1 multi-packet)"), "{line}");
    }

    /// `describe` is read off a hardware transcript, so the numbers a reader
    /// needs first have to be in it.
    #[test]
    fn the_tx_line_carries_asked_accepted_and_calls() {
        let mut log = TxLog::default();
        let mut t = TxTally::new(MAX_FRAME);
        t.record::<()>(TX_PACKET, &Ok(TX_PACKET));
        t.record::<()>(TX_PACKET, &Ok(0));
        t.record_flush();
        t.ms = 11;
        log.finish(t);
        let line = log.describe();
        for want in ["4109 B asked", "512 accepted", "2 call(s)", "1 refused", "1 flush(es)", "11 ms"] {
            assert!(line.contains(want), "{want:?} missing from {line:?}");
        }
    }

    // -- send_all ----------------------------------------------------------

    #[test]
    fn send_all_loops_past_the_3840_byte_truncation() {
        let mut chunks: Vec<usize> = Vec::new();
        let payload = vec![0xa5u8; MAX_FRAME];
        let mut got: Vec<u8> = Vec::new();
        send_all(
            |b| {
                let n = b.len().min(SERIAL_BINARY_BUFLEN);
                chunks.push(n);
                got.extend_from_slice(&b[..n]);
                Ok::<usize, ()>(n)
            },
            &payload,
        )
        .unwrap();
        assert_eq!(chunks, vec![SERIAL_BINARY_BUFLEN, MAX_FRAME - SERIAL_BINARY_BUFLEN]);
        assert_eq!(got, payload);
    }

    /// A full TX buffer is transient. `SerialSendDataBlocking` breaks at the
    /// first short write, so a `WouldBlock` on the first 512-byte chunk yields
    /// `Ok(0)` — and `rv64-host serve` does file I/O between reads, so it will
    /// happen. Treating it as fatal would kill a boot mid-flight.
    #[test]
    fn send_all_rides_out_a_transient_full_tx_buffer() {
        let mut stalls_left = SEND_RETRY_BUDGET - 1;
        let mut got: Vec<u8> = Vec::new();
        let payload = vec![0x11u8; 100];
        send_all(
            |b| {
                if stalls_left > 0 {
                    stalls_left -= 1;
                    return Ok::<usize, ()>(0);
                }
                got.extend_from_slice(b);
                Ok(b.len())
            },
            &payload,
        )
        .unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn send_all_gives_up_once_the_retry_budget_is_spent() {
        let mut calls = 0usize;
        let r = send_all(
            |_: &[u8]| {
                calls += 1;
                Ok::<usize, ()>(0)
            },
            &[1u8, 2, 3],
        );
        assert_eq!(r, Err(SendFault::Stalled));
        assert_eq!(calls, SEND_RETRY_BUDGET + 1, "bounded, and bounded where it says");
    }

    #[test]
    fn send_all_reports_the_underlying_error_whole() {
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        struct BadAddress;
        let r = send_all(|_: &[u8]| Err::<usize, _>(BadAddress), &[1u8]);
        assert_eq!(r, Err(SendFault::Link(BadAddress)));
    }

    // -- send_paced ---------------------------------------------------------

    /// The property the badge's transmit side now depends on: **no call ever
    /// hands over more than one CDC packet.** The hardware buffer behind
    /// `serial_send` holds exactly one packet and nothing checks whether the
    /// previous one has left, so a call carrying nine packets' worth is nine
    /// overwrites of one buffer. See [`send_paced`].
    #[test]
    fn a_page_frame_leaves_one_packet_at_a_time_with_a_wait_between() {
        let mut chunks: Vec<usize> = Vec::new();
        let mut gaps = 0usize;
        let payload: Vec<u8> = (0..MAX_FRAME as u32).map(|i| (i % 251) as u8).collect();
        let mut got: Vec<u8> = Vec::new();
        send_paced(
            |b| {
                chunks.push(b.len());
                got.extend_from_slice(b);
                Ok::<usize, ()>(b.len())
            },
            || gaps += 1,
            || false,
            TX_PACKET,
            &payload,
        )
        .unwrap();

        assert!(
            chunks.iter().all(|&n| n <= TX_PACKET),
            "a call handed over more than one packet: {chunks:?}"
        );
        assert_eq!(chunks.len(), MAX_FRAME.div_ceil(TX_PACKET));
        assert_eq!(gaps, chunks.len() - 1, "a gap between packets, never before or after");
        assert_eq!(got, payload, "pacing must not alter or reorder a single byte");
    }

    /// The other half of the bargain: **a request pays nothing.** Every frame
    /// this link sends except a writeback fits in one packet, so if pacing cost
    /// them anything it would be a millisecond on each of ~16,000 page
    /// operations.
    #[test]
    fn a_frame_that_fits_in_one_packet_never_waits() {
        let mut gaps = 0usize;
        let mut calls = 0usize;
        let req = {
            let mut v = Vec::new();
            encode(&Frame::ReadReq { page: 7 }, &mut v);
            v
        };
        assert!(req.len() <= TX_PACKET, "a ReadReq is one packet by construction");
        send_paced(
            |b| {
                calls += 1;
                Ok::<usize, ()>(b.len())
            },
            || gaps += 1,
            || false,
            TX_PACKET,
            &req,
        )
        .unwrap();
        assert_eq!((calls, gaps), (1, 0));
    }

    /// A short accept still costs a wait before the retry, because the reason a
    /// packet-sized write was short is that the transmit buffer had not drained
    /// — which is the same thing the gap is for. Retrying immediately would
    /// spend the retry budget in microseconds.
    #[test]
    fn a_short_accept_waits_before_the_remainder_goes_out() {
        let mut gaps = 0usize;
        let mut chunks: Vec<usize> = Vec::new();
        let mut first = true;
        send_paced(
            |b| {
                chunks.push(b.len());
                if core::mem::take(&mut first) {
                    return Ok::<usize, ()>(10);
                }
                Ok(b.len())
            },
            || gaps += 1,
            || false,
            TX_PACKET,
            &[0u8; 100],
        )
        .unwrap();
        assert_eq!(chunks, vec![100, 90]);
        assert_eq!(gaps, 1);
    }

    /// `send_all` is `send_paced` with the pacing off, and the existing
    /// truncation test above is what proves it still behaves that way. This
    /// pins the one thing that test cannot see: no gap is ever taken.
    #[test]
    fn send_all_is_send_paced_with_nothing_paced() {
        let mut chunks: Vec<usize> = Vec::new();
        let mut gaps = 0usize;
        send_paced(
            |b| {
                chunks.push(b.len());
                Ok::<usize, ()>(b.len())
            },
            || gaps += 1,
            || false,
            usize::MAX,
            &[0u8; MAX_FRAME],
        )
        .unwrap();
        assert_eq!(chunks, vec![MAX_FRAME]);
        assert_eq!(gaps, 0);
    }

    // -- the send deadline --------------------------------------------------

    /// The hole [`SEND_DEADLINE_MS`] exists to close, stated as a measurement
    /// rather than an argument: **a sink that keeps accepting bytes and never
    /// drains never trips the retry budget**, because any accepted byte resets
    /// it. Without a clock this loop is unbounded on a nine-packet frame.
    #[test]
    fn a_dribbling_sink_never_trips_the_retry_budget() {
        let mut calls = 0usize;
        // One byte per call: legal, non-zero, and `idle` is reset every time.
        let r = send_paced(
            |_: &[u8]| {
                calls += 1;
                Ok::<usize, ()>(1)
            },
            || {},
            || false,
            TX_PACKET,
            &[0u8; MAX_FRAME],
        );
        assert_eq!(r, Ok(()));
        assert_eq!(calls, MAX_FRAME, "one call per byte, and not one of them counted as idle");
    }

    /// The clock, doing what the count cannot. Same dribbling sink, but the
    /// caller is out of time part way through: the send gives up, says how far
    /// it got, and hands over nothing more.
    #[test]
    fn a_send_that_cannot_finish_gives_up_on_the_clock() {
        const BUDGET: usize = 100;
        let mut calls = 0usize;
        let mut clock = 0usize;
        let r = send_paced(
            |_: &[u8]| {
                calls += 1;
                Ok::<usize, ()>(1)
            },
            || {},
            || {
                clock += 1;
                clock > BUDGET
            },
            TX_PACKET,
            &[0u8; MAX_FRAME],
        );
        assert_eq!(r, Err(SendFault::TimedOut { sent: BUDGET, len: MAX_FRAME }));
        assert_eq!(calls, BUDGET, "no byte went out after the deadline passed");
    }

    /// The deadline is checked *before* the pacing gap, so a send that is
    /// already out of time does not first pay a millisecond per remaining
    /// packet on the way out.
    #[test]
    fn an_expired_send_does_not_pay_the_gap_on_its_way_out() {
        let mut gaps = 0usize;
        let r = send_paced(
            |b: &[u8]| Ok::<usize, ()>(b.len()),
            || gaps += 1,
            || true,
            TX_PACKET,
            &[0u8; MAX_FRAME],
        );
        assert_eq!(r, Err(SendFault::TimedOut { sent: 0, len: MAX_FRAME }));
        assert_eq!(gaps, 0);
    }

    /// A frame that fits in one packet cannot reach the deadline at all: the
    /// loop runs once. This is why the read path never exercised the hole and
    /// the write path always did.
    #[test]
    fn a_one_packet_frame_is_one_call_and_cannot_loop() {
        let mut calls = 0usize;
        let mut checks = 0usize;
        let mut req = Vec::new();
        encode(&Frame::ReadReq { page: 7 }, &mut req);
        send_paced(
            |b: &[u8]| {
                calls += 1;
                Ok::<usize, ()>(b.len())
            },
            || {},
            || {
                checks += 1;
                false
            },
            TX_PACKET,
            &req,
        )
        .unwrap();
        assert_eq!((calls, checks), (1, 1));
    }

    /// A transmit path that has stopped moving mid-frame. The end-to-end
    /// property this round exists for: **a writeback that cannot be
    /// transmitted faults, with numbers, instead of looping.**
    struct TruncatingLink {
        clock: u64,
    }
    impl Transport for TruncatingLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            // Four packets went out and the fifth never will -- the badge-side
            // shape of the host's "discarded 2061 bytes; frame-shaped ...
            // WriteReq" line.
            self.clock += SEND_DEADLINE_MS + 1;
            Err(SendError::TimedOut { sent: 4 * TX_PACKET, len: bytes.len() })
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            Ok(Vec::new())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn a_writeback_that_cannot_be_transmitted_faults_rather_than_looping() {
        let mut h = UsbHost::new(TruncatingLink { clock: 0 });
        let handle = h.link();
        assert_eq!(h.write_page(1275, &[0u8; PAGE]), Err(Error::Medium));
        let f = handle.take_fault().unwrap();
        assert_eq!(f.kind, LinkFault::SendTimedOut { sent: 4 * TX_PACKET, len: MAX_FRAME });
        let d = f.describe();
        assert!(d.contains("2048 of 4109"), "{d}");
        assert!(d.contains("not draining"), "{d}");
    }

    // -- naming the operation -----------------------------------------------

    /// §27b's finding, made unnecessary to rediscover: a fault says whether it
    /// was reading or writing, and which page, so a `MEM FAULT` at the address
    /// of an unrelated load cannot be read as a failed read.
    #[test]
    fn a_fault_names_the_page_operation_that_failed() {
        let mut h = UsbHost::new(StalledLink);
        let handle = h.link();

        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(8136, &mut buf), Err(Error::Medium));
        let f = handle.take_fault().unwrap();
        assert_eq!(f.op, Some(Op::Read(8136)));
        assert!(f.describe().starts_with("reading page 8136: "), "{}", f.describe());

        assert_eq!(h.write_page(1275, &[0u8; PAGE]), Err(Error::Medium));
        let f = handle.take_fault().unwrap();
        assert_eq!(f.op, Some(Op::Write(1275)));
        assert!(f.describe().starts_with("writing page 1275: "), "{}", f.describe());
    }

    /// The console mirror and the pump are not page operations, and a fault
    /// from either must not inherit the last exchange's page.
    #[test]
    fn a_console_fault_claims_no_page() {
        let mut h = UsbHost::new(StalledLink);
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(8136, &mut buf), Err(Error::Medium));
        assert_eq!(handle.take_fault().map(|f| f.op), Some(Some(Op::Read(8136))));

        assert_eq!(handle.send_console(b"hello"), Err(Error::Medium));
        let f = handle.take_fault().unwrap();
        assert_eq!(f.op, None);
        assert!(!f.describe().contains("page 8136"), "{}", f.describe());
    }

    // -- queue budget -------------------------------------------------------

    #[test]
    fn one_outstanding_page_stays_far_inside_the_128_slot_queue() {
        let mut h = UsbHost::new(MemoryLoopback::new(64));
        rv64::backing::conformance(&mut h, 64);
        let peak = h.link.inner.borrow().t.peak_queue_slots();
        assert_eq!(
            peak, MAX_FRAME_PACKETS,
            "peak depth must be exactly one page frame; anything more means \
             something started pipelining"
        );
        assert!(peak * 8 <= SERVER_QUEUE_SLOTS, "the safety margin is gone");
    }

    // -- console ------------------------------------------------------------

    #[test]
    fn a_console_frame_arriving_mid_exchange_is_kept_not_mistaken_for_the_response() {
        let mut link = MemoryLoopback::new(8);
        link.inject_console = b"ls -l\r".to_vec();
        let mut h = UsbHost::new(link);
        let handle = h.link();

        let mut buf = [0xffu8; PAGE];
        h.read_page(3, &mut buf).unwrap();

        assert!(buf.iter().all(|&b| b == 0), "the response, not the keystrokes");
        assert_eq!(handle.take_console(), b"ls -l\r".to_vec());
        assert!(handle.take_console().is_empty(), "take drains");
    }

    /// F5: a human typing at a shell prompt with a warm page cache generates no
    /// page faults, so console input that only arrives inside `exchange` never
    /// arrives at all. `pump` is the non-blocking path, and it must work through
    /// the handle, because by then the backing belongs to `PageCache`.
    #[test]
    fn console_input_arrives_without_a_page_fault_to_carry_it() {
        let mut link = MemoryLoopback::new(8);
        link.async_console = b"whoami\r".to_vec();
        let h = UsbHost::new(link);
        let handle = h.link();

        // No page operation at all: exactly the warm-cache case.
        let cache = rv64::PageCache::new(h, 4);
        assert!(handle.take_console().is_empty(), "nothing yet, nothing pumped");
        handle.pump().unwrap();
        assert_eq!(handle.take_console(), b"whoami\r".to_vec());
        drop(cache);
    }

    #[test]
    fn the_handle_outlives_the_backing() {
        let mut link = MemoryLoopback::new(8);
        link.inject_console = b"hi".to_vec();
        let mut h = UsbHost::new(link);
        let handle = h.link();

        let mut buf = [0u8; PAGE];
        h.read_page(0, &mut buf).unwrap();
        let cache = rv64::PageCache::new(h, 4);
        drop(cache); // the backing is gone; the console bytes are not.

        assert_eq!(handle.take_console(), b"hi".to_vec());
    }

    // -- error paths --------------------------------------------------------

    #[test]
    fn an_error_frame_from_the_host_becomes_out_of_range() {
        let mut h = UsbHost::new(MemoryLoopback::new(4));
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(4, &mut buf), Err(Error::OutOfRange));
        assert_eq!(h.write_page(9, &[0u8; PAGE]), Err(Error::OutOfRange));
    }

    #[test]
    fn an_unrecognised_error_code_is_a_medium_failure() {
        assert_eq!(err_code(ERR_READ), Error::OutOfRange);
        assert_eq!(err_code(ERR_WRITE), Error::OutOfRange);
        assert_eq!(err_code(0), Error::Medium);
        assert_eq!(err_code(99), Error::Medium);
    }

    /// A transport that parks and sends but never delivers: a dropped byte, a
    /// CRC-rejected frame, a missed IRQ. Without a deadline this is a permanent
    /// silent hang, which is the one failure mode the badge cannot report.
    struct SilentLink {
        clock: u64,
    }
    impl Transport for SilentLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            Ok(Vec::new()) // a flush that found nothing buffered
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }


    // -----------------------------------------------------------------
    // A faithful model of `usb-bao1x`'s listener semantics
    // -----------------------------------------------------------------

    /// The watchdog flush period, in virtual milliseconds.
    ///
    /// The same 5 ms as `badge::FLUSH_MS`, which is `#[cfg(target_os = "xous")]`
    /// and so invisible here. Duplicated rather than hoisted: hoisting it would
    /// put a hardware timing constant in the platform-free half of the module
    /// for the sake of one test, and the value that matters to this model is
    /// "the watchdog fires far more often than a round trip", not the number.
    const MODEL_FLUSH_MS: u64 = 5;

    /// The device-side state machine the badge actually talks to, on a virtual
    /// clock.
    ///
    /// Every other fake in this file models *bytes*: you send, and the answer
    /// is there. That is why they all pass while the hardware does not. The one
    /// exception is `ParkOrderLink`, which models the **listener** rather than
    /// the bytes, and it is the only fake that ever caught a hardware bug. This
    /// is that idea taken as far as the sources allow.
    ///
    /// What is modelled, each from `services/usb-bao1x/src/main.rs`:
    ///
    /// * **`serial_buf` accumulates and is not drained by parking.**
    ///   `SerialHookBinary` (`:683-686`) sets the listen mode and takes the
    ///   listener message. It does not touch the buffer. Bytes that arrive with
    ///   no listener parked stay queued (`:592-670`, whose no-listener branch is
    ///   "do nothing, keep queuing data").
    /// * **A delivery consumes the listener.** `serial_listener.take()`.
    /// * **The flush watchdog un-parks unconditionally**, whether or not there
    ///   is anything to deliver (`:731`, `:740-750`), roughly every
    ///   [`FLUSH_MS`]. On hardware this happens ~200 times a second, constantly,
    ///   and in no other test here at all.
    /// * **A delivery caps at [`SERIAL_BINARY_BUFLEN`]**, so a 4109-byte page
    ///   response always takes more than one.
    /// * **The reader re-parks after a gap.** The real reader loops
    ///   `parked = true; wait_binary()`, so there is a window after every
    ///   delivery — including every watchdog un-park — in which nothing is
    ///   parked.
    ///
    /// The peer answers after [`Self::rt_ms`], the measured 2 ms round trip.
    struct BaoLink {
        clock: u64,
        /// Bytes the host has put on the wire that the device has buffered.
        serial_buf: Vec<u8>,
        /// Is a binary listener parked right now?
        parked: bool,
        /// When the reader will re-park, if it is currently un-parked.
        repark_at: Option<u64>,
        /// Deliveries handed to the reader thread, awaiting `recv`.
        delivered: std::collections::VecDeque<Vec<u8>>,
        /// The host's reply and when it lands on the wire.
        reply_at: Option<u64>,
        pending: Vec<u8>,
        next_flush: u64,
        rt_ms: u64,
        repark_ms: u64,
        /// Watchdog deliveries that actually took a parked listener. Counted so
        /// a test can assert the watchdog really did interfere.
        unparks: usize,
        /// Requests the host decoded, for the same reason.
        served: usize,
    }

    impl BaoLink {
        fn new() -> Self {
            Self {
                clock: 0,
                serial_buf: Vec::new(),
                // The listen mode starts at `NoListener`; `UsbTransport::new`
                // primes it by parking once before it returns, which is what
                // `arm` models here.
                parked: false,
                repark_at: Some(0),
                delivered: std::collections::VecDeque::new(),
                reply_at: None,
                pending: Vec::new(),
                next_flush: MODEL_FLUSH_MS,
                rt_ms: 2,
                repark_ms: 1,
                unparks: 0,
                served: 0,
            }
        }

        /// `serial_listener.take()` plus the delivery it carries. Empty
        /// deliveries are real and routine: a flush that finds nothing buffered
        /// still consumes the listener.
        fn deliver(&mut self, watchdog: bool) {
            if !self.parked {
                return;
            }
            let n = self.serial_buf.len().min(SERIAL_BINARY_BUFLEN);
            let chunk: Vec<u8> = self.serial_buf.drain(..n).collect();
            if watchdog {
                // Every watchdog delivery, not only the idle ones. Modelling
                // this turned up something worth knowing: a 4109-byte page
                // response exceeds `SERIAL_BINARY_BUFLEN`, so its first chunk
                // is delivered by the IRQ and **its tail is delivered by the
                // flush watchdog** — there is no second IRQ to carry it. The
                // watchdog is therefore not a backstop for idle links, it is on
                // the critical path of every page response. That is the
                // strongest argument yet for why the badge image must carry
                // `usb-bao1x-serialflush-repair.patch`: without a working
                // flush, every page read strands its own tail.
                self.unparks += 1;
            }
            self.delivered.push_back(chunk);
            self.parked = false;
            self.repark_at = Some(self.clock + self.repark_ms);
        }

        /// One millisecond of the world.
        fn tick(&mut self) {
            self.clock += 1;

            if let Some(t) = self.repark_at {
                if self.clock >= t {
                    self.parked = true;
                    self.repark_at = None;
                }
            }

            // The host's answer lands on the wire. This is the IRQ path: if a
            // listener is parked it is delivered immediately, and if not the
            // bytes simply queue.
            if let Some(t) = self.reply_at {
                if self.clock >= t {
                    let bytes = core::mem::take(&mut self.pending);
                    self.serial_buf.extend_from_slice(&bytes);
                    self.reply_at = None;
                    self.deliver(false);
                }
            }

            // The flush watchdog, which takes the listener whether or not there
            // is anything to hand over.
            if self.clock >= self.next_flush {
                self.next_flush = self.clock + MODEL_FLUSH_MS;
                self.deliver(true);
            }
        }
    }

    impl Transport for BaoLink {
        /// The reader is parked, or we wait for it — which is what
        /// `confirm_park` does on hardware, absorbing the watchdog's un-parks
        /// rather than treating them as failures.
        fn arm(&mut self) -> Result<(), ()> {
            for _ in 0..2000 {
                if self.parked {
                    return Ok(());
                }
                self.tick();
            }
            Err(())
        }

        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                self.served += 1;
                let mut out = Vec::new();
                encode(&Frame::ReadResp { page, data: Box::new([0xb0; PAGE]) }, &mut out);
                self.pending = out;
                self.reply_at = Some(self.clock + self.rt_ms);
            }
            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.tick();
            Ok(self.delivered.pop_front().unwrap_or_default())
        }

        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    /// **The end-to-end claim, against the real listener semantics.**
    ///
    /// A page read completes even though the watchdog is un-parking the reader
    /// every [`MODEL_FLUSH_MS`] throughout, the answer arrives in more than one
    /// delivery, and there are windows in which the answer is sitting in
    /// `serial_buf` with nothing parked to take it.
    ///
    /// If this ever fails, the receive path is broken in a way no byte-level
    /// fake can see.
    #[test]
    fn a_read_completes_against_the_real_listener_semantics() {
        const READS: u32 = 40;
        let mut h = UsbHost::new(BaoLink::new());
        let link = h.link();
        let mut buf = [0u8; PAGE];
        // A sequence, not one read. A single exchange finishes inside 5 ms and
        // never meets the watchdog at all — which is exactly why every earlier
        // fake here looked healthy. A boot is ~16,000 exchanges against a
        // watchdog firing 200 times a second, so the interesting state is the
        // one *between* exchanges.
        for page in 0..READS {
            h.read_page(page, &mut buf).unwrap_or_else(|e| {
                let f = link.take_fault();
                panic!("read {page} failed: {e:?} ({})", f.map(|f| f.describe()).unwrap_or_default())
            });
            assert!(buf.iter().all(|&b| b == 0xb0), "read {page} returned the wrong bytes");
        }

        let i = h.link.inner.borrow();
        assert_eq!(i.t.served, READS as usize, "some request needed re-sending");
        // The point of the fake: the watchdog really was interfering.
        assert!(i.t.unparks > 0, "the watchdog never un-parked; the model is not modelling");
        drop(i);
        assert_eq!(link.take_fault(), None);
        assert_eq!(link.retries(), 0, "no exchange should have needed a retry");
    }

    /// The answer arrives while nothing is parked, so only the watchdog can
    /// deliver it. This is the case the flush watchdog exists for, and the one that
    /// silently hangs if the flush watchdog is ever removed or the badge image
    /// lacks `usb-bao1x-serialflush-repair.patch`.
    #[test]
    fn an_answer_that_lands_while_unparked_is_still_delivered() {
        let mut l = BaoLink::new();
        // Re-parking takes longer than the round trip, so the reply always
        // lands in `serial_buf` with no listener to take it.
        l.repark_ms = 20;
        l.rt_ms = 2;
        let mut h = UsbHost::new(l);
        let link = h.link();
        let mut buf = [0u8; PAGE];
        h.read_page(7, &mut buf).expect("the watchdog must rescue a stranded reply");
        assert!(buf.iter().all(|&b| b == 0xb0));
        assert_eq!(link.take_fault(), None);
    }

    /// A peer that never answers still terminates, and the fault now says
    /// *which* kind of silence it was: measured time short of the deadline
    /// would mean the wait is not being waited, and zero deliveries means
    /// nothing ever arrived.
    ///
    /// This is the regression for the message that could not tell the third
    /// hardware run from the fourth.
    #[test]
    fn a_peer_that_never_answers_reports_measured_time_and_zero_deliveries() {
        let mut l = BaoLink::new();
        l.rt_ms = u64::MAX / 4; // the answer never lands
        let mut h = UsbHost::new(l);
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(3, &mut buf), Err(Error::Medium));

        let fault = link.take_fault().expect("a timeout must record a fault");
        match fault.kind {
            LinkFault::Timeout { attempts, elapsed_ms, waited_ms, deliveries, bytes_in } => {
                assert_eq!(attempts, RETRY_BUDGET + 1);
                // Measured, not quoted: each attempt really did wait out its
                // own deadline.
                assert!(
                    waited_ms > ATTEMPT_DEADLINE_MS,
                    "the last attempt gave up after {waited_ms} ms, short of its \
                     {ATTEMPT_DEADLINE_MS} ms deadline -- the wait is not being waited"
                );
                assert!(elapsed_ms >= waited_ms);
                // The watchdog delivers empty results constantly, and an empty
                // delivery is not a delivery: it must not look like bytes
                // arriving.
                assert_eq!(deliveries, 0, "empty watchdog flushes were counted as data");
                assert_eq!(bytes_in, 0);
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        let text = fault.describe();
        assert!(text.contains("waited and heard nothing"), "{text}");
        assert!(!text.contains("RETURNED EARLY"), "{text}");
    }

    /// And the other reading: a transport that gives up early is reported as
    /// giving up early, not as "no answer after 4 attempts".
    ///
    /// **This is the message the fourth hardware run needed and did not have.**
    /// The old text quoted `RECV_DEADLINE_MS` regardless of how long anything
    /// actually waited, so a link that burned its whole budget in 15 ms read as
    /// one that had waited two seconds.
    #[test]
    fn a_transport_that_returns_early_says_so_rather_than_blaming_the_peer() {
        /// A transport whose clock races: every reading jumps a full attempt
        /// deadline, so the exchange concludes "no answer" almost immediately.
        struct FastClock {
            clock: u64,
        }
        impl Transport for FastClock {
            fn arm(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
                Ok(())
            }
            fn recv(&mut self) -> Result<Vec<u8>, ()> {
                Ok(Vec::new())
            }
            fn now_ms(&mut self) -> u64 {
                self.clock += ATTEMPT_DEADLINE_MS + 1;
                self.clock
            }
        }

        let mut h = UsbHost::new(FastClock { clock: 0 });
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(1, &mut buf), Err(Error::Medium));
        let text = link.take_fault().unwrap().describe();
        // The numbers are measured, so they are the ones a human compares
        // against the host's timestamps.
        assert!(text.contains("measured"), "{text}");
        assert!(text.contains("0 deliveries"), "{text}");
    }

    /// **The deadline must bound `arm` and `send`, not only the receive wait.**
    ///
    /// The nineteenth hardware run stopped with no `MEM FAULT` at all, and the
    /// structural half of the reason was here: `RECV_DEADLINE_MS` was consulted
    /// in exactly one place — the receive loop — so everything above it was
    /// outside the deadline. `arm` may spend `PARK_WAIT_MS` (1000) per attempt,
    /// so with a slow re-park an exchange transmitted requests, and kept
    /// transmitting them, well past the point at which it had already decided
    /// to fail.
    ///
    /// This models that: a park that costs 1500 ms and a peer that never
    /// answers. Attempt 1 arms at 1500, sends, and gives up on its own
    /// `ATTEMPT_DEADLINE_MS`. Attempt 2 then arms to 3250 — past the two-second
    /// deadline — and must **not** put a second 4109-byte request on a link it
    /// has already given up on.
    ///
    /// Deleting either of the two checks in `exchange_inner` makes `sends` 2.
    #[test]
    fn a_slow_park_cannot_smuggle_a_request_out_past_the_deadline() {
        struct SlowPark {
            clock: u64,
            sends: usize,
        }
        impl Transport for SlowPark {
            fn arm(&mut self) -> Result<(), ()> {
                // A re-park inside `PARK_WAIT_MS`, so this is a *success*:
                // the point is that a legal `arm` can still consume most of
                // the exchange's whole budget.
                self.clock += 1500;
                Ok(())
            }
            fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
                self.sends += 1;
                Ok(())
            }
            fn recv(&mut self) -> Result<Vec<u8>, ()> {
                self.clock += 10;
                Ok(Vec::new())
            }
            fn now_ms(&mut self) -> u64 {
                self.clock
            }
        }

        let mut h = UsbHost::new(SlowPark { clock: 0, sends: 0 });
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(11, &mut buf), Err(Error::Medium));

        let sends = h.link.inner.borrow().t.sends;
        assert_eq!(
            sends, 1,
            "a request was transmitted after the exchange had already passed \
             RECV_DEADLINE_MS -- the deadline is not bounding `arm`"
        );
        // And it still reports itself, which is the property the whole
        // diagnostic design rests on: a fault names itself, a hang does not.
        assert!(matches!(link.take_fault().map(|f| f.kind), Some(LinkFault::Timeout { .. })));
    }

    /// **The diagnostic the fifth hardware run is for.**
    ///
    /// A peer whose replies are byte-perfect except for the CRC produces
    /// exactly the fourth run's signature -- every byte received, order
    /// preserved, reader healthy, no frame -- and the fault must now show the
    /// discarded bytes so that signature is readable rather than inferred.
    ///
    /// The expected prefix is the frame header itself: `c1 b0` (SYNC, little
    /// endian), `02` (ReadResp), `04 10` (LEN = 4100). Seeing that in the hex
    /// means the frame arrived intact and was rejected on its CRC.
    #[test]
    fn a_reply_rejected_on_crc_shows_up_as_discarded_bytes_in_hex() {
        struct BadCrc {
            pending: Vec<u8>,
            clock: u64,
        }
        impl Transport for BadCrc {
            fn arm(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
                let mut m = Mux::new();
                m.push(bytes);
                if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                    let mut out = Vec::new();
                    encode(&Frame::ReadResp { page, data: Box::new([0x5a; PAGE]) }, &mut out);
                    // One bit of the CRC trailer, and nothing else.
                    let last = out.len() - 1;
                    out[last] ^= 0x01;
                    self.pending = out;
                }
                Ok(())
            }
            fn recv(&mut self) -> Result<Vec<u8>, ()> {
                self.clock += 1;
                Ok(core::mem::take(&mut self.pending))
            }
            fn now_ms(&mut self) -> u64 {
                self.clock
            }
        }

        let mut h = UsbHost::new(BadCrc { pending: Vec::new(), clock: 0 });
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(9, &mut buf), Err(Error::Medium));

        let fault = link.take_fault().expect("a fault must be recorded");
        let text = fault.describe();
        // The count is exact even though only NOISE_SAMPLE bytes are kept.
        assert!(
            text.contains(&format!("discarded {}", 4 * (5 + 4 + PAGE + 4))),
            "the discarded count is wrong or missing: {text}"
        );
        // And the sample identifies the frame that was thrown away.
        assert!(
            text.contains("c1 b0 02 04 10"),
            "the hex sample does not show the rejected frame header: {text}"
        );
        // This is the reading the fourth run produced, so the test pins that
        // the two now appear together and can be told apart.
        assert!(text.contains("bytes arrived but never formed the frame asked for"), "{text}");

        // And the stash is drained: a second failure that discards nothing must
        // not inherit this one's bytes. A stale diagnostic would send a reader
        // after a frame that had already been explained.
        let mut h2 = UsbHost::new(SilentLink { clock: 0 });
        let l2 = h2.link();
        assert_eq!(h2.read_page(0, &mut buf), Err(Error::Medium));
        let second = l2.take_fault().unwrap().describe();
        assert!(!second.contains("discarded"), "a stale noise sample leaked: {second}");
    }

    /// **The two halves of the fifth run's open question, in one line.**
    ///
    /// The decoder's discarded sample says what reached the *decoder*; the
    /// transport's `status()` fingerprint says what reached the *reader*. The
    /// corruption is between them or before them, and only having both in the
    /// same message decides which — so this pins that they are both there and
    /// are distinguishable, which is the part a laptop can guarantee.
    ///
    /// The bug itself is below `Transport`, at the rkyv/lent-page boundary, and
    /// no laptop test can reach it: there is no analogue of a page lent to
    /// another process. What the laptop *has* established is that everything
    /// above `Transport` is sound — `BaoLink` drives the real listener
    /// semantics and completes — which is what localised the fault here.
    #[test]
    fn a_fault_carries_both_what_the_reader_saw_and_what_the_decoder_discarded() {
        /// Delivers plausible-looking rubbish, and reports a fingerprint of it
        /// exactly as `UsbTransport::status` does on hardware.
        struct Rubbish {
            clock: u64,
            pending: Vec<u8>,
        }
        impl Transport for Rubbish {
            fn arm(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
                // RISC-V `nop` padding: the fifth hardware run's signature.
                self.pending = [0x13u8, 0x00, 0x00, 0x00].repeat(64);
                Ok(())
            }
            fn recv(&mut self) -> Result<Vec<u8>, ()> {
                self.clock += 1;
                Ok(core::mem::take(&mut self.pending))
            }
            fn now_ms(&mut self) -> u64 {
                self.clock
            }
            fn status(&mut self) -> Option<String> {
                Some("reader: parked=true first_delivery=[13 00 00 00]".into())
            }
        }

        let mut h = UsbHost::new(Rubbish { clock: 0, pending: Vec::new() });
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(2, &mut buf), Err(Error::Medium));
        let text = link.take_fault().unwrap().describe();

        // What the reader saw...
        assert!(text.contains("first_delivery=[13 00 00 00]"), "{text}");
        // ...and what the decoder threw away, in the same message.
        assert!(text.contains("decoder discarded"), "{text}");
        assert!(text.contains("13 00 00 00"), "{text}");
        // Both present means the comparison can be made from one line, which is
        // the whole point: agreeing pins the fault above the reader, differing
        // pins it below.
    }

    /// The other hypothesis the hex settles: the CDC endpoint looping our own
    /// request back would also give an exact multiple of a frame size, and it
    /// reads completely differently in hex -- `01` for `ReadReq`, and a 13-byte
    /// frame rather than a 4109-byte one.
    #[test]
    fn a_request_looped_back_is_distinguishable_in_the_hex_from_a_bad_reply() {
        struct Loopback {
            pending: Vec<u8>,
            clock: u64,
        }
        impl Transport for Loopback {
            fn arm(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
                // Echo the request back verbatim. It decodes as a `ReadReq`,
                // which `Mux` holds and no `take_matching(0x02)` ever wants —
                // so it is *held*, not discarded, and the hex stays empty.
                // That difference is the point.
                self.pending = bytes.to_vec();
                Ok(())
            }
            fn recv(&mut self) -> Result<Vec<u8>, ()> {
                self.clock += 1;
                Ok(core::mem::take(&mut self.pending))
            }
            fn now_ms(&mut self) -> u64 {
                self.clock
            }
        }

        let mut h = UsbHost::new(Loopback { pending: Vec::new(), clock: 0 });
        let link = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(4, &mut buf), Err(Error::Medium));
        let text = link.take_fault().unwrap().describe();
        // A well-formed frame of an unwanted type is held, never discarded, so
        // a loopback shows *no* discarded bytes while a CRC rejection shows
        // thousands. The two are unambiguous.
        assert!(!text.contains("discarded"), "a decodable echo must not read as noise: {text}");
        assert!(text.contains("deliveries"), "{text}");
    }

    #[test]
    fn a_silent_link_times_out_instead_of_hanging() {
        let mut h = UsbHost::new(SilentLink { clock: 0 });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(0, &mut buf), Err(Error::Medium));
        match handle.take_fault().map(|f| f.kind) {
            Some(LinkFault::Timeout { attempts, deliveries, bytes_in, .. }) => {
                assert_eq!(
                    attempts,
                    RETRY_BUDGET + 1,
                    "a silent link must exhaust the budget, not give up on the first try"
                );
                // A link that says nothing must report that nothing arrived —
                // which is what tells a reader "the peer is silent" apart from
                // "bytes arrived and did not decode".
                assert_eq!(deliveries, 0);
                assert_eq!(bytes_in, 0);
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// F6: an empty delivery means "not yet", not "the cable is gone". A flush
    /// watchdog produces these routinely.
    struct SlowLink {
        pending: Vec<u8>,
        empties_left: usize,
        clock: u64,
    }
    impl Transport for SlowLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                encode(&Frame::ReadResp { page, data: Box::new([0x3b; PAGE]) }, &mut self.pending);
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            if self.empties_left > 0 {
                self.empties_left -= 1;
                return Ok(Vec::new());
            }
            let n = self.pending.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.pending.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn empty_deliveries_are_waited_through_not_treated_as_a_dead_cable() {
        let mut h = UsbHost::new(SlowLink { pending: Vec::new(), empties_left: 50, clock: 0 });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        h.read_page(2, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x3b));
        assert_eq!(handle.take_fault(), None);
    }

    /// A transport that refuses to park. On hardware this is the USB server
    /// wedged; the one thing that must not happen is sending anyway.
    struct UnparkableLink {
        sends: usize,
    }
    impl Transport for UnparkableLink {
        fn arm(&mut self) -> Result<(), ()> {
            Err(())
        }
        fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
            self.sends += 1;
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            Ok(Vec::new())
        }
        fn now_ms(&mut self) -> u64 {
            0
        }
        fn take_platform_fault(&mut self) -> Option<String> {
            Some("lend_mut: ServerQueueFull".into())
        }
    }

    #[test]
    fn a_link_that_cannot_park_never_sends() {
        let mut h = UsbHost::new(UnparkableLink { sends: 0 });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(0, &mut buf), Err(Error::Medium));
        assert_eq!(h.link.inner.borrow().t.sends, 0, "a request left with no listener parked");
        let f = handle.take_fault().unwrap();
        assert_eq!(f.kind, LinkFault::NotParked);
        // F3: the platform's own error reaches the run loop, as text, through a
        // handle that survives `PageCache`.
        assert_eq!(f.platform.as_deref(), Some("lend_mut: ServerQueueFull"));
        assert!(f.describe().contains("ServerQueueFull"));
    }

    /// A cable that has come out: `serial_send` accepts nothing, forever.
    struct StalledLink;
    impl Transport for StalledLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
            Err(SendError::Stalled)
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            Ok(Vec::new())
        }
        fn now_ms(&mut self) -> u64 {
            0
        }
        fn take_platform_fault(&mut self) -> Option<String> {
            Some("serial_send: NoError".into())
        }
    }

    /// N3: the failure a human will actually hit must report as itself, not as
    /// a generic transport error.
    #[test]
    fn an_unplugged_cable_reports_as_stalled_not_as_a_generic_failure() {
        let mut h = UsbHost::new(StalledLink);
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(0, &mut buf), Err(Error::Medium));
        let f = handle.take_fault().unwrap();
        assert_eq!(f.kind, LinkFault::Stalled);
        assert!(f.describe().contains("accepted nothing"), "{}", f.describe());
    }

    struct RefusingLink;
    impl Transport for RefusingLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, _: &[u8]) -> Result<(), SendError> {
            Err(SendError::Failed)
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            Ok(Vec::new())
        }
        fn now_ms(&mut self) -> u64 {
            0
        }
    }

    #[test]
    fn a_failed_send_is_a_medium_failure_with_a_reason() {
        let mut h = UsbHost::new(RefusingLink);
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(0, &mut buf), Err(Error::Medium));
        assert_eq!(handle.take_fault().map(|f| f.kind), Some(LinkFault::SendFailed));
        assert_eq!(h.write_page(0, &[0u8; PAGE]), Err(Error::Medium));
        assert_eq!(handle.take_fault().map(|f| f.kind), Some(LinkFault::SendFailed));
    }

    /// Answers every read with a response for the *wrong* page — a
    /// desynchronised link, or the pipelining the module docs forbid.
    struct WrongPageLink {
        out: Vec<u8>,
        clock: u64,
    }
    impl Transport for WrongPageLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                encode(
                    &Frame::ReadResp { page: page.wrapping_add(1), data: Box::new([0xee; PAGE]) },
                    &mut self.out,
                );
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            Ok(std::mem::take(&mut self.out))
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn a_response_for_the_wrong_page_is_refused_rather_than_returned() {
        let mut h = UsbHost::new(WrongPageLink { out: Vec::new(), clock: 0 });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        assert_eq!(h.read_page(7, &mut buf), Err(Error::Medium));
        assert!(buf.iter().all(|&b| b == 0), "nothing may be copied out of a mismatched frame");
        assert_eq!(
            handle.take_fault().map(|f| f.kind),
            Some(LinkFault::WrongPage { asked: 7, got: 8 })
        );
    }

    /// A host that answers the *previous* request a second time, with the
    /// duplicate ahead of this request's own answer in the stream.
    ///
    /// This is the twenty-fourth hardware run reproduced on a laptop. That run
    /// received 972 x 4109 bytes for 971 requests — one whole surplus
    /// `ReadResp`, arriving after `discard_stale_responses` had already run and
    /// found nothing, which is why the transcript could say `stale=0` and
    /// `retries=0` and still die with `response for page 1513 while waiting for
    /// page 1514`.
    ///
    /// The duplicate is emitted *before* the real answer on purpose: the CDC
    /// stream is ordered, so no amount of purging before the send can get ahead
    /// of it. The only place it can be dealt with is the receive loop.
    struct EchoPreviousLink {
        out: Vec<u8>,
        clock: u64,
        /// The page whose answer will be repeated at the head of the next
        /// exchange.
        last: Option<u32>,
    }
    /// A byte pattern that is distinct per page, so a test can tell whose data
    /// came back.
    fn dup_fill(page: u32) -> u8 {
        (page as u8).wrapping_mul(7).wrapping_add(0x21)
    }
    impl Transport for EchoPreviousLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                if let Some(prev) = self.last.take() {
                    encode(
                        &Frame::ReadResp { page: prev, data: Box::new([dup_fill(prev); PAGE]) },
                        &mut self.out,
                    );
                }
                encode(
                    &Frame::ReadResp { page, data: Box::new([dup_fill(page); PAGE]) },
                    &mut self.out,
                );
                self.last = Some(page);
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            // The delivery cap, so the duplicate and the answer arrive in
            // fragments and the decoder has to accumulate across turns — the
            // shape the wire actually has.
            let n = self.out.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.out.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    /// **The regression for the twenty-fourth run.** A duplicate answer that
    /// arrives mid-wait is dropped and the wait continues; the guest never sees
    /// a fault, and the count says it happened.
    #[test]
    fn a_duplicate_answer_arriving_mid_wait_is_absorbed_not_faulted() {
        let mut h = UsbHost::new(EchoPreviousLink { out: Vec::new(), clock: 0, last: None });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        for page in [3u32, 4, 5, 6] {
            h.read_page(page, &mut buf).unwrap_or_else(|e| {
                panic!("page {page} faulted on a duplicate that is provably stale: {e:?}")
            });
            assert!(
                buf.iter().all(|&b| b == dup_fill(page)),
                "page {page} got another page's bytes"
            );
        }
        assert_eq!(handle.take_fault(), None, "a duplicate must not reach the guest");
        // One per exchange after the first.
        assert_eq!(handle.late_dropped(), 3, "the duplicates must be counted, not merely survived");
        // The evidence signature of the hardware run: the pre-send purge cannot
        // see a frame that has not arrived yet, so `stale` stays at zero while
        // the stream carries a surplus answer. A test that let `stale_dropped`
        // catch these would be testing the wrong window.
        assert_eq!(handle.stale_dropped(), 0, "the purge cannot be what catches these");
    }

    /// An `Err` frame for a page nobody asked about must not end this caller's
    /// wait: doing so reports a fault for the wrong page.
    struct StrayErrLink {
        out: Vec<u8>,
        clock: u64,
    }
    impl Transport for StrayErrLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                encode(&Frame::Err { code: ERR_READ, page: page + 1000 }, &mut self.out);
                encode(&Frame::ReadResp { page, data: Box::new([0x2d; PAGE]) }, &mut self.out);
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.out.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.out.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn an_error_frame_for_another_page_does_not_end_this_wait() {
        let mut h = UsbHost::new(StrayErrLink { out: Vec::new(), clock: 0 });
        let handle = h.link();
        let mut buf = [0u8; PAGE];
        h.read_page(5, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x2d));
        assert_eq!(handle.take_fault(), None);
    }

    // -- retries ------------------------------------------------------------

    /// Swallows the first request whole, then behaves normally.
    ///
    /// That is what a mirrored log line does to a request frame: it lands
    /// between the two `serial_send` calls a 4109-byte frame needs, the host's
    /// decoder finds a bad CRC, drains two bytes and rescans, and the request is
    /// simply gone. The host never answers because it never saw a request.
    struct DropOnceLink {
        pages: Vec<[u8; PAGE]>,
        out: Vec<u8>,
        dropped: bool,
        clock: u64,
    }

    impl DropOnceLink {
        fn new(pages: u32) -> Self {
            Self {
                pages: vec![[0u8; PAGE]; pages as usize],
                out: Vec::new(),
                dropped: false,
                clock: 0,
            }
        }
    }

    impl Transport for DropOnceLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            let Some(f) = m.take_matching(0x01).or_else(|| m.take_matching(0x03)) else {
                return Ok(());
            };
            if !self.dropped {
                self.dropped = true;
                return Ok(()); // split on the wire; the host never saw it
            }
            let reply = match f {
                Frame::ReadReq { page } => match self.pages.get(page as usize) {
                    Some(p) => Frame::ReadResp { page, data: Box::new(*p) },
                    None => Frame::Err { code: ERR_READ, page },
                },
                Frame::WriteReq { page, data } => match self.pages.get_mut(page as usize) {
                    Some(p) => {
                        p.copy_from_slice(&data[..]);
                        Frame::WriteAck { page }
                    }
                    None => Frame::Err { code: ERR_WRITE, page },
                },
                _ => unreachable!(),
            };
            encode(&reply, &mut self.out);
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.out.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.out.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    /// A dropped request must cost a re-send, not a guest fault. Before the
    /// retry existed this was `Error::Medium`, which in Linux is a dead boot —
    /// under memory pressure, which is exactly when the swapper is logging into
    /// the same transmit endpoint.
    #[test]
    fn a_dropped_request_is_retried_and_succeeds() {
        let mut h = UsbHost::new(DropOnceLink::new(16));
        let handle = h.link();
        let mut buf = [0xffu8; PAGE];
        h.read_page(5, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0), "the retry must deliver the real page");
        assert_eq!(handle.retries(), 1, "exactly one re-send");
        assert_eq!(handle.take_fault(), None, "a recovered drop is not a fault");
    }

    /// Answers every request **twice** after dropping the first one: the late
    /// answer to attempt 1 plus the answer to attempt 2.
    ///
    /// The duplicate is the hazard the retry introduces, and the dangerous shape
    /// is specific: a stale `ReadResp` for page 7 satisfying a *later* read of
    /// page 7 after an intervening write, returning pre-write data. Nothing
    /// about the page check catches that — both frames say page 7.
    struct DuplicatingLink {
        pages: Vec<[u8; PAGE]>,
        out: Vec<u8>,
        dropped: bool,
        clock: u64,
    }

    impl DuplicatingLink {
        fn new(pages: u32) -> Self {
            Self {
                pages: vec![[0u8; PAGE]; pages as usize],
                out: Vec::new(),
                dropped: false,
                clock: 0,
            }
        }
    }

    impl Transport for DuplicatingLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut m = Mux::new();
            m.push(bytes);
            let Some(f) = m.take_matching(0x01).or_else(|| m.take_matching(0x03)) else {
                return Ok(());
            };
            if !self.dropped {
                self.dropped = true;
                return Ok(());
            }
            let reply = match f {
                Frame::ReadReq { page } => match self.pages.get(page as usize) {
                    Some(p) => Frame::ReadResp { page, data: Box::new(*p) },
                    None => Frame::Err { code: ERR_READ, page },
                },
                Frame::WriteReq { page, data } => match self.pages.get_mut(page as usize) {
                    Some(p) => {
                        p.copy_from_slice(&data[..]);
                        Frame::WriteAck { page }
                    }
                    None => Frame::Err { code: ERR_WRITE, page },
                },
                _ => unreachable!(),
            };
            encode(&reply, &mut self.out);
            encode(&reply, &mut self.out); // the duplicate
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.out.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.out.drain(..n).collect())
        }
        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn a_duplicate_answer_from_a_retry_cannot_satisfy_a_later_exchange() {
        let mut h = UsbHost::new(DuplicatingLink::new(16));
        let handle = h.link();

        let mut buf = [0xffu8; PAGE];
        h.read_page(7, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(handle.retries(), 1);

        // Change page 7. The duplicate still in flight carries its *old*
        // contents and says page 7, so only the stale purge stands between that
        // and a read returning pre-write data.
        h.write_page(7, &[0xd7u8; PAGE]).unwrap();

        let mut r = [0u8; PAGE];
        h.read_page(7, &mut r).unwrap();
        assert!(
            r.iter().all(|&b| b == 0xd7),
            "a stale duplicate answered a later read of the same page"
        );
        assert!(handle.stale_dropped() > 0, "the duplicate must have been discarded");
        assert_eq!(handle.take_fault(), None);
    }

    /// "We re-sent this four times and never heard back" and "the cable is out"
    /// are different problems and must read differently in a transcript — that
    /// is the whole reason the fault channel exists.
    #[test]
    fn an_exhausted_retry_budget_reads_differently_from_a_dead_link() {
        let mut buf = [0u8; PAGE];

        let mut silent = UsbHost::new(SilentLink { clock: 0 });
        let sh = silent.link();
        assert_eq!(silent.read_page(0, &mut buf), Err(Error::Medium));
        let timed_out = sh.take_fault().unwrap();

        let mut dead = UsbHost::new(StalledLink);
        let dh = dead.link();
        assert_eq!(dead.read_page(0, &mut buf), Err(Error::Medium));
        let stalled = dh.take_fault().unwrap();

        assert!(
            matches!(timed_out.kind, LinkFault::Timeout { attempts, .. } if attempts == RETRY_BUDGET + 1)
        );
        assert_eq!(stalled.kind, LinkFault::Stalled);
        assert_ne!(timed_out.describe(), stalled.describe());
        assert!(
            timed_out.describe().contains(&format!("{} attempt", RETRY_BUDGET + 1)),
            "the timeout must name its attempt count: {}",
            timed_out.describe()
        );
        assert!(stalled.describe().contains("accepted nothing"), "{}", stalled.describe());

        // The counters separate them too: a dead link never gets as far as
        // waiting, so it is never retried.
        assert_eq!(sh.retries(), RETRY_BUDGET);
        assert_eq!(dh.retries(), 0);
    }

    /// The retry must not multiply the queue depth: attempts are sequential, so
    /// only one request is ever in flight.
    #[test]
    fn retries_do_not_raise_the_number_of_frames_in_flight() {
        let mut h = UsbHost::new(DropOnceLink::new(16));
        let mut buf = [0u8; PAGE];
        h.read_page(1, &mut buf).unwrap();
        // One frame queued at a time is what `one_outstanding_page_stays_far_
        // inside_the_128_slot_queue` measures on the loopback; here the point is
        // simply that a retry re-sends rather than pipelines.
        assert!(h.link.inner.borrow().t.out.len() <= MAX_FRAME);
    }

    // -- against the real laptop server ------------------------------------

    /// The badge client wired to the *actual* `rv64-host serve` code path, with
    /// the badge's USB behaviour in between: sends truncated at 3840, receives
    /// delivered at most 3840 at a time, and — fix round 1 — `serve_once` fed in
    /// 3840-byte slices so the host's own accumulate-across-reads discipline is
    /// re-exercised from this side too.
    ///
    /// `MemoryLoopback` is a model of the host; this is the host.
    struct RealHostLink {
        img: std::io::Cursor<Vec<u8>>,
        len: u64,
        host_mux: Mux,
        to_badge: Vec<u8>,
        clock: u64,
    }

    impl RealHostLink {
        fn new(pages: u32) -> Self {
            let len = pages as u64 * PAGE as u64;
            Self {
                img: std::io::Cursor::new(vec![0u8; len as usize]),
                len,
                host_mux: Mux::new(),
                to_badge: Vec::new(),
                clock: 0,
            }
        }
    }

    impl Transport for RealHostLink {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }

        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            let mut accepted: Vec<u8> = Vec::new();
            send_all(
                |b| {
                    let n = b.len().min(SERIAL_BINARY_BUFLEN);
                    accepted.extend_from_slice(&b[..n]);
                    Ok::<usize, ()>(n)
                },
                bytes,
            )
            .map_err(|_| SendError::Failed)?;
            // One `Mux` for the connection, fed a slice at a time — exactly what
            // `rv64_host::serve` does per `read()`, and the reason it does it: a
            // 4109-byte WriteReq spans reads.
            for slice in accepted.chunks(SERIAL_BINARY_BUFLEN) {
                rv64_host::serve::serve_once(
                    &mut self.img,
                    self.len,
                    &mut self.host_mux,
                    slice,
                    &mut self.to_badge,
                    &mut Vec::new(),
                )
                .map_err(|_| SendError::Failed)?;
            }
            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            let n = self.to_badge.len().min(SERIAL_BINARY_BUFLEN);
            Ok(self.to_badge.drain(..n).collect())
        }

        fn now_ms(&mut self) -> u64 {
            self.clock
        }
    }

    #[test]
    fn usbhost_passes_conformance_against_the_real_host_server() {
        let mut h = UsbHost::new(RealHostLink::new(64));
        rv64::backing::conformance(&mut h, 64);
    }

    #[test]
    fn the_frame_that_fits_the_queue_is_the_one_the_probe_measured() {
        assert_eq!(MAX_FRAME, 4109);
        assert_eq!(MAX_FRAME_PACKETS, 9);
        assert_eq!(SERVER_QUEUE_SLOTS, 128);
        assert_eq!(SERIAL_BINARY_BUFLEN, 3840);
    }

    /// F7: the request buffer is reused rather than reallocated per exchange.
    #[test]
    fn the_request_buffer_is_reused_across_exchanges() {
        let mut h = UsbHost::new(MemoryLoopback::new(8));
        let mut buf = [0u8; PAGE];
        h.read_page(0, &mut buf).unwrap();
        let first = h.link.inner.borrow().scratch.as_ptr();
        h.write_page(1, &[0x99u8; PAGE]).unwrap();
        h.read_page(1, &mut buf).unwrap();
        let last = h.link.inner.borrow().scratch.as_ptr();
        assert_eq!(first, last, "the scratch buffer was reallocated between exchanges");
        assert!(h.link.inner.borrow().scratch.capacity() >= MAX_FRAME);
    }
}
