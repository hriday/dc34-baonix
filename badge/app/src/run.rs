//! The run loop: the emulator, the USB page transport and the OLED console
//! wired into one machine, with nothing platform-specific in it.
//!
//! This is the whole of Task 8's logic. `main.rs` is the badge's platform leaf
//! — it builds a [`UsbTransport`](crate::usbhost::UsbTransport) and a
//! `GfxScreen` and hands them here — and `tests/dry_run.rs` is the laptop's,
//! handing in a socket transport and a recording screen instead. **Everything
//! between those two leaves is this file**, so the boot that runs on a laptop
//! and the boot that runs on the badge are the same code down to the slice
//! length.
//!
//! # Why that matters more than it sounds
//!
//! Every hardware-only defect in Task 6 was a policy decision that had been
//! written below a `#[cfg(target_os = "xous")]`, where no laptop test could
//! reach it, and each one cost a flash-and-photograph cycle to find. Task 7
//! answered that by putting all of the display's policy in `Grid` and only
//! `draw_textview` below the `cfg`. This module is the same answer applied to
//! the run loop, and it is what makes the dry run in `tests/dry_run.rs`
//! evidence rather than a rehearsal: that test boots the real nixpkgs guest
//! through *this* loop, *this* `PageCache`, *this* `Mux` and *this* console
//! pipeline. What it cannot cover is exactly two leaves — the `usb_bao1x`
//! syscalls and the `Gfx` syscalls — and nothing else.
//!
//! # The three orderings that are load-bearing
//!
//! 1. **`check_interrupts` runs before `step`, never inside it.** `Cpu::step`
//!    clears a private `next_pc` before dispatch and applies it at the end, so
//!    a trap vector installed from within is silently overwritten by the
//!    interrupted instruction's own fallthrough pc. Recorded in the phase-2
//!    handoff and not visible from the call site; `rv64_host::run_until`
//!    carries the same comment.
//!
//! 2. **The loop is sliced.** Console input and screen updates happen *between*
//!    slices, so without slicing a `ConIn` frame never reaches the guest and
//!    the display never refreshes. The slice length matches
//!    `rv64_host`'s (100 000), which matters for a reason beyond taste: the
//!    laptop's reference boot was measured with that slice, and a boot that
//!    diverges is diagnosed by comparing instruction counts.
//!
//! 3. **[`Link::take_fault`] is called on every `Error::Medium`.** Without it
//!    the badge's only report of a dead link is an undifferentiated guest
//!    load/store fault at a random address — which has already cost one
//!    hardware cycle. Every path here that can see a backing failure takes the
//!    fault and puts it on the screen.
//!
//! # What the badge does *not* do
//!
//! It does not load anything. `rv64-host serve` lays the kernel, the DTB and
//! the initramfs into the flat image with `load_boot_images`, and the badge
//! starts a CPU at [`KERNEL_LOAD_ADDR`] against that image. The one number the
//! badge still needs is `a1`, the DTB's guest address, which the loader
//! computed and the protocol has no field for — so [`dtb_address`] recovers it
//! from the kernel's own boot header, which is *in* the image. See that
//! function for why that is a derivation rather than a guess.

use rv64::backing::MemBacking;
use rv64::bus::Bus;
use rv64::cache::PageCache;
use rv64::csr::{self, Priv};
use rv64::sbi::SbiOutcome;
use rv64::uart::ConsoleSink;
use rv64::{Cpu, Stats};

use crate::oled::{OledSink, Screen};
use crate::usbhost::{Fault, Link, Transport, UsbHost};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Where `rv64_host::load_boot_images` puts the kernel, and therefore where the
/// CPU starts. `rv64_host::KERNEL_LOAD_ADDR`, spelled out here because this
/// crate cannot depend on `rv64-host` outside tests — it is full of `std::fs`.
pub const KERNEL_LOAD_ADDR: u64 = rv64::RAM_BASE + 0x20_0000;

/// Resident page-cache frames: 1400, which is ~5.5 MiB of guest RAM held on
/// the badge at a time.
///
/// # The number follows the cost of a miss, and that cost is not fixed
///
/// Three measured prices decide this constant, and none of them is a guess:
///
/// | where the page comes from | cost | measured by |
/// |---|---|---|
/// | badge swap (a resident-but-swapped-out frame) | **~4 ms** | `badge/probe-transcript.txt`: 256 KiB per ~260 ms mapping step = 64 pages, and the heap climb agrees |
/// | USB, unpaced | **~2 ms** | the probe's round-trip leg |
/// | USB, paced at 512 B/ms | **~18 ms** | a 4109-byte frame is nine packets, and `serve --pace-ms 1` runs a millisecond between each |
///
/// The badge has ~308 KiB free SRAM, so **a cache of any interesting size is
/// already swap-backed** — at 256 frames it was 1 MiB against 308 KiB, i.e.
/// three quarters swap. The question was never "SRAM or not", it is "which of
/// swap and USB is cheaper", and *pacing decides that*:
///
/// * **While the host paces** (today, and until the receive side of the USB
///   driver carries more than one packet at a time) a swap fault at 4 ms beats
///   a USB fetch at 18 ms by a factor of four. Every frame that turns a miss
///   into a swapped hit is a win, so the cache should be as large as the
///   machine can hold.
/// * **Once pacing is gone** the comparison inverts: 4 ms of swap against 2 ms
///   of USB. A frame that only ever lives in swap is then *worse* than not
///   caching the page at all, and the right size is roughly what fits in real
///   SRAM — **~64 frames**, 256 KiB against the 308 KiB measured free. That is
///   also the size `rv64::cache`'s own docs are written against ("a
///   badge-plausible 32-64 frames").
///
/// So this constant has two correct values and they differ by twenty times.
/// It is 1400 because the link is paced; **when `--pace-ms` and
/// `usbhost::TX_PACE_MS` are both retired, bring it back to 64** and expect the
/// boot to get faster, not slower.
///
/// # Why 1400: writebacks are the fragile transmit, and this is how few buy
///
/// The frame count is not only a speed dial. It is the **only** app-side lever
/// on how many times the boot takes the one path that has repeatedly killed a
/// run — a nine-packet `WriteReq`, the badge's only multi-packet transmit.
/// Every avoided writeback is a dice roll not thrown.
///
/// Measured exactly, by booting the real guest to a shell prompt through
/// `rv64_host::boot_capturing_frames` at each size (the whole 173.5 M
/// instructions; `writebacks` is the column that matters):
///
/// | frames | MiB | misses | evictions | writebacks | writebacks by 128 M insns |
/// |---|---|---|---|---|---|
/// | 1024 | 4.0 | 2 826 | 1 802 | **1 670** | 853 |
/// | 1152 | 4.5 | 2 671 | 1 519 | 1 448 | |
/// | 1280 | 5.0 | 2 277 | 997 | 979 | |
/// | 1344 | 5.3 | 2 166 | 822 | 819 | |
/// | **1400** | **5.5** | 2 216 | 816 | **816** | **162** |
/// | 1536 | 6.0 | 2 242 | 706 | 706 | 26 |
/// | 1792 | 7.0 | 2 113 | 321 | 321 | |
/// | 1978 | 7.7 | 1 978 | **0** | **0** | 0 |
///
/// Two things fall out of that table, and the second is why this number moved.
///
/// **Zero writebacks is real but unreachable.** The boot's whole working set to
/// a shell is 1 978 distinct guest pages, so at 1 978 frames nothing is ever
/// evicted and the badge never transmits a page at all — 1 977 frames costs
/// exactly one writeback, which is the cleanest possible confirmation that the
/// number is the working set and not an artefact. But 1 978 frames is 7 912 KiB
/// of frame data, and the system-wide ceiling is **8 160 KiB of usable swap**
/// (`SWAP_RAM_LEN` 8 MiB less a 16-byte MAC per page; `badge/README.md`,
/// *Memory*), shared with the kernel and the ten services that boot ahead of
/// this app. There is no arrangement of that budget in which the page cache
/// gets 7.7 MiB of it. Zero writebacks is off the table on this hardware, and
/// it is worth knowing that precisely rather than hoping.
///
/// **1400 puts the whole boot under what the badge has already survived.** The
/// twenty-fifth hardware run reached 128 M instructions at 1024 frames before
/// it wedged — which is **853 completed writebacks**, from the table's last
/// column. At 1400 frames the *entire* boot to a shell costs 816. The badge is
/// therefore being asked for fewer page transmits across the whole run than it
/// demonstrably completed tonight before stalling, and only 162 of them by the
/// point at which it stalled. That is not a marginal improvement in a rate; it
/// moves the total requirement below the observed survival.
///
/// # Why 1400 and not 1536
///
/// Task 5's heap climb reached its **6144 KiB cap without failing** in this
/// same app process, and the transcript says in as many words that the cap is a
/// choice rather than a boundary — but it is also the largest figure hardware
/// has ever actually demonstrated, so it is treated here as the budget rather
/// than as a floor to build on.
///
/// A `Frame` is 4104 bytes (a 4096-byte page, a `u32`, three flags, one byte of
/// padding), and the residency index is a flat 16 KiB whatever the frame count
/// is. So 1400 frames is `1400 x 4104 + 16 384` = **5 627 KiB**, leaving ~517
/// KiB of the demonstrated 6144 for the rest of the process: the decoder's
/// per-response page, the 8 KiB console mirror, the transport's buffers, the
/// OLED grid. 1536 frames would be 6 172 KiB — *past* the demonstrated figure
/// with nothing left over — for 110 fewer writebacks. That is the wrong side of
/// a trade on a night with one flash left: an allocation the machine cannot
/// serve does not cost writebacks, it costs the whole boot.
///
/// `UsbTransport::new` raises the process heap ceiling to 8 MiB before anything
/// allocates (`usbhost::HEAP_MAX`), which is what makes any of this possible;
/// `rv64::cache`'s residency index is a `u16` per guest page, so 8192 frames is
/// the type's ceiling and not a constraint here.
///
/// **Heap pages cannot be given back.** Task 5 found this the hard way: a `Vec`
/// drop returns pages to the Rust allocator, never to the kernel. An over-large
/// cache is permanent for the life of the process, which is the reason to leave
/// headroom rather than take the ceiling.
///
/// # What it costs to change, and what it moves
///
/// It is an argument to [`assemble`] rather than a hard-wired constant, and the
/// only badge-side thing that reads it is `main.rs`, so trying another value is
/// an app-only rebuild — `swap.uf2` alone.
///
/// Instruction count is frame-count-independent and `tests/dry_run.rs` asserts
/// that against `rv64_host::run_until` exactly (**173,500,000** instructions,
/// 2,169,838 MMU walks for the current `nix/guest`). The cache counters are
/// *not* independent of it, which is why the dry run's reconciliation boots its
/// reference through `boot_capturing_frames(.., FRAMES)`: change this number and
/// the misses on a hardware transcript should be compared against a dry run at
/// the same number, never against the phase-3 handoff's 256-frame figures.
pub const FRAMES: usize = 1400;

/// Instructions per slice. `rv64_host`'s `CONSOLE_POLL_INSNS`, deliberately the
/// same number: see the module docs.
pub const SLICE_INSNS: u64 = 100_000;

/// Slices between heartbeat ticks.
///
/// The spinner exists to distinguish a wedge from a panic in a photograph, so
/// it has to move — but every tick owes the display a frame, and a frame is a
/// whole-screen `draw_textview` plus a `flush` over IPC. At one tick per slice
/// the badge would repaint 128x128 pixels every 100 000 guest instructions
/// whether or not anything changed. Sixteen slices is 1.6 M instructions, which
/// is seconds on the badge and still fast enough that a stopped spinner is
/// obvious in a photograph.
///
/// Guest output is unaffected either way: `OledSink::flush` repaints whenever
/// the grid is dirty, so a line the guest prints appears at the end of the
/// slice that printed it, not at the next tick.
pub const HEARTBEAT_SLICES: u64 = 16;

/// Slices between throughput reports, once the first one has been made.
///
/// The report is one `log::info!` line, which on the badge rides the log
/// server's USB panic mirror -- the *transmit* stream a page request also uses.
/// Mirrored text can split a request and cost a re-send (`Link::retries`), so
/// the period is chosen to make that negligible rather than to make the
/// transcript pretty: 256 slices is 25.6 M guest instructions, which is about
/// seven lines across a whole boot.
pub const RATE_SLICES: u64 = 256;

/// The slice at which the *first* throughput report is made, before
/// [`RATE_SLICES`] takes over.
///
/// A hardware run costs a power cycle and forty minutes, and several have ended
/// before reaching a shell. 16 slices is 1.6 M instructions -- seconds, even at
/// the badge's rate -- so the number this whole exercise exists to obtain
/// survives a run that dies at page 300. It is deliberately the same divisor as
/// [`HEARTBEAT_SLICES`], so the first report lands on a tick rather than
/// between two.
pub const FIRST_RATE_SLICE: u64 = 16;

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// An accumulated span of wall-clock milliseconds, opened and closed against a
/// clock that is allowed to refuse.
///
/// [`Link::now_ms`] returns `None` when its borrow fails, which cannot happen
/// from the run loop's service points but is not worth a panic on a machine
/// whose only output device is a 16x8 screen. A refused sample is *dropped*,
/// never guessed: an invented timestamp is indistinguishable from a real one
/// and would corrupt the throughput figure silently, which is the one failure
/// mode a measuring instrument must not have. [`Span::dropped`] counts them so
/// a corrupted total announces itself.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    open_at: Option<u64>,
    /// Milliseconds accumulated across every closed interval.
    pub total_ms: u64,
    /// Intervals abandoned because the clock refused at one end or the other.
    pub dropped: u32,
}

impl Span {
    /// Starts an interval. An `open_at` of `None` makes the matching
    /// [`Span::close`] a dropped sample.
    pub fn open(&mut self, now: Option<u64>) {
        self.open_at = now;
    }

    /// Ends the interval opened by the last [`Span::open`] and adds it in.
    ///
    /// `saturating_sub` rather than a subtraction: the badge's ticktimer is
    /// monotonic, but the trait only promises milliseconds, and a run that
    /// silently accumulated a wrapped interval would report a throughput
    /// number nobody could tell was wrong.
    pub fn close(&mut self, now: Option<u64>) {
        match (self.open_at.take(), now) {
            (Some(a), Some(b)) => self.total_ms = self.total_ms.saturating_add(b.saturating_sub(a)),
            _ => self.dropped = self.dropped.saturating_add(1),
        }
    }
}

/// The one line this measurement exists to produce.
///
/// `cpu` is wall-clock time minus the link and minus the between-slice service
/// block, i.e. **time spent interpreting**, and `ips` is `executed / cpu`. The
/// other three terms are printed beside it so the subtraction can be checked
/// from the transcript rather than trusted.
///
/// `ips=?` rather than a zero when there is no interpreting time to divide by:
/// a printed `0 insn/s` is a measurement, and this is the absence of one.
pub fn rate_line(executed: u64, wall_ms: u64, link_ms: u64, service_ms: u64) -> String {
    let cpu_ms = wall_ms.saturating_sub(link_ms).saturating_sub(service_ms);
    let ips = match executed.saturating_mul(1000).checked_div(cpu_ms) {
        Some(n) => n.to_string(),
        None => "?".to_string(),
    };
    format!(
        "rv64 rate: insn={executed} wall={wall_ms}ms link={link_ms}ms svc={service_ms}ms \
         cpu={cpu_ms}ms ips={ips}"
    )
}

// ---------------------------------------------------------------------------
// The console seam
// ---------------------------------------------------------------------------

/// A [`ConsoleSink`] the run loop can also ask to repaint and to tick.
///
/// `rv64::Bus` is generic over `ConsoleSink`, which is `put` and nothing else —
/// correct for the core crate, which has no business knowing that a sink might
/// have a screen behind it. This trait adds the two calls the run loop makes
/// between slices, so the loop stays generic over the sink instead of being
/// nailed to `OledSink<GfxScreen>`.
///
/// The reason that genericity is worth a trait: `tests/dry_run.rs` runs the
/// real `OledSink` *and* keeps a transcript of every byte, by wrapping both in
/// one sink. Without this the loop could not be handed the wrapper.
pub trait Console: ConsoleSink {
    /// Repaint if anything changed. Cheap when nothing has.
    fn flush(&mut self);
    /// Advance the heartbeat.
    fn tick(&mut self);
    /// Guest bytes to mirror back to the host as a `ConOut` frame, drained.
    ///
    /// Empty by default, because most sinks are only a screen. [`Mirrored`]
    /// is the one that has something to say, and it is what the badge and the
    /// dry run both wrap their console in.
    fn take_output(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

impl<S: Screen> Console for OledSink<S> {
    fn flush(&mut self) {
        OledSink::flush(self)
    }
    fn tick(&mut self) {
        OledSink::tick(self)
    }
}

/// How many un-mirrored guest bytes to hold before dropping them.
///
/// A whole boot's console output is a few kilobytes and the run loop drains
/// this every slice, so it only grows when the link has stopped taking bytes —
/// and on a machine the probe measured at ~308 KiB free, an unbounded buffer
/// waiting for a link that is never coming back is a second failure stacked on
/// the first. Eight kilobytes is ~60 screens' worth: far more than any single
/// slice produces, far less than a problem.
pub const MIRROR_CAP: usize = 8 * 1024;

/// A [`Console`] that also keeps what the guest said, so the run loop can send
/// it to the host as `ConOut`.
///
/// # Why the badge mirrors its console at all
///
/// The OLED is eight rows of sixteen. That is enough for the photograph this
/// project is for, and nowhere near enough for a kernel oops — and once a line
/// has scrolled off the grid it is gone, because the grid is the only place it
/// was. So "one photograph plus a serial transcript" rested on the photograph
/// for everything the guest ever said.
///
/// `rv64_proto::Frame::ConOut`, `rv64_host::serve`'s decode of it and its echo
/// to stdout were all written and tested before anything sent one — the badge
/// simply never encoded the frame, and `serve --help` promised an echo that
/// could not happen. This closes that.
///
/// # Why the buffer is here and not in `OledSink`
///
/// `OledSink` is a grid and a screen; bytes reach it and become cells. Giving
/// it a byte buffer would put a second responsibility inside the one module
/// whose whole design is that it holds only display policy. This wraps it
/// instead, so the badge composes `Mirrored<OledSink<GfxScreen>>` and every
/// existing test that wants a bare `OledSink` keeps working unchanged.
pub struct Mirrored<C: Console> {
    inner: C,
    buf: Vec<u8>,
    /// Bytes dropped because [`MIRROR_CAP`] was reached. Non-zero means the
    /// transcript has a hole in it and the link is why.
    dropped: usize,
}

impl<C: Console> Mirrored<C> {
    pub fn new(inner: C) -> Self {
        Self { inner, buf: Vec::new(), dropped: 0 }
    }

    /// The console underneath — the screen, on the badge.
    pub fn inner(&self) -> &C {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut C {
        &mut self.inner
    }

    /// Guest bytes dropped rather than mirrored.
    pub fn dropped(&self) -> usize {
        self.dropped
    }
}

impl<C: Console> ConsoleSink for Mirrored<C> {
    fn put(&mut self, byte: u8) {
        // The screen first, always. Mirroring is the secondary channel and must
        // never be able to cost a character of the primary one.
        self.inner.put(byte);
        if self.buf.len() < MIRROR_CAP {
            self.buf.push(byte);
        } else {
            self.dropped += 1;
        }
    }
}

impl<C: Console> Console for Mirrored<C> {
    fn flush(&mut self) {
        self.inner.flush()
    }
    fn tick(&mut self) {
        self.inner.tick()
    }
    fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.buf)
    }
}

/// Writes a line to the console and repaints immediately.
///
/// For progress notes from `main`, which has no other output device: on the
/// badge the only things that reach a human are this screen and the log
/// server's USB mirror, and a stage line on the screen is what localises a hang
/// in whichever blocking call never returned.
pub fn note<C: Console>(c: &mut C, line: &str) {
    for b in line.bytes() {
        c.put(b);
    }
    c.put(b'\n');
    c.flush();
}

// ---------------------------------------------------------------------------
// Recovering the DTB address
// ---------------------------------------------------------------------------

/// Why the DTB address could not be recovered from the loaded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootProbeError {
    /// A read of guest RAM failed. The address is the one that faulted; on the
    /// badge this is almost always a dead link rather than a bad address, and
    /// the [`Fault`] alongside it says which.
    ///
    /// Boxed because a `Fault` now carries the link's whole diagnosis -- the
    /// measured timeout figures, the reader's state, and a hex sample of what
    /// the decoder discarded -- which is worth well over a hundred bytes on
    /// every `Result` in this module's happy path if it is stored inline.
    Read { addr: u64, fault: Option<Box<Fault>> },
    /// No riscv64 boot header at [`KERNEL_LOAD_ADDR`]. Either the host never
    /// served the image, or it served an ELF kernel — see [`dtb_address`].
    NoImageHeader,
    /// The image declares a load offset this runner does not implement, which
    /// means the host loaded it somewhere other than where we are about to
    /// start executing.
    TextOffset(u64),
    /// `image_size` is corrupt: smaller than a header, or large enough to
    /// overflow the address space.
    ImageSize(u64),
    /// The derived address does not hold a flattened device tree. The layout
    /// rule and the host's disagree.
    NoFdt { at: u64, magic: u32 },
}

impl BootProbeError {
    /// Sixteen-column-friendly, because it goes on the badge's screen.
    pub fn describe(&self) -> String {
        match self {
            BootProbeError::Read { addr, fault } => match fault {
                Some(f) => format!("read {addr:#x} failed: {}", f.describe()),
                None => format!("read {addr:#x} failed"),
            },
            BootProbeError::NoImageHeader => {
                format!("no RSC\\x05 magic at {KERNEL_LOAD_ADDR:#x}: host served no image?")
            }
            BootProbeError::TextOffset(o) => {
                format!("kernel wants text_offset {o:#x}, we run at {KERNEL_LOAD_ADDR:#x}")
            }
            BootProbeError::ImageSize(s) => format!("bad image_size {s:#x}"),
            BootProbeError::NoFdt { at, magic } => {
                format!("no dtb at {at:#x} (magic {magic:#010x})")
            }
        }
    }
}

/// `RISCV_IMAGE_MAGIC2` at offset 56 of the riscv64 boot header, as a
/// little-endian `u32` — `b"RSC\x05"`.
const IMAGE_MAGIC2: u32 = u32::from_le_bytes(*b"RSC\x05");

/// The flattened-device-tree magic, `0xd00dfeed`, as the little-endian `u32` a
/// 4-byte load of a big-endian header reads back as.
const FDT_MAGIC_LE: u32 = 0xedfe_0dd0;

/// Recovers the DTB's guest physical address — the value the boot protocol
/// wants in `a1` — from the kernel image the host already laid into guest RAM.
///
/// # Why this is derived rather than sent
///
/// `rv64-host serve` computes this address with `rv64_host::boot_layout` during
/// its load phase and then throws it away: the wire protocol
/// (`rv64_proto::Frame`) carries pages, console bytes and errors, and has no
/// field for it. So the badge has three options — hard-code it, add a frame
/// type, or read it back out of the image. Hard-coding is a number that goes
/// silently wrong the next time the guest kernel is rebuilt, and it fails as a
/// hang with no output, which is the single worst failure mode in this project.
/// Adding a frame type means changing `rv64-proto` and both ends of a link that
/// is already proven. Reading it back is neither: the input to `boot_layout` is
/// the kernel's own `image_size`, and the kernel is *in the image we are about
/// to execute*.
///
/// # The derivation, and that it is the host's
///
/// `boot_layout` places the DTB at `kernel_end.next_multiple_of(8)` where
/// `kernel_end` is `KERNEL_LOAD_ADDR + image_size` — the *memory* footprint the
/// image declares in its own header (`image_size`, offset 16), never the file
/// length, because `objcopy -O binary` drops `.bss` and the kernel zeroes that
/// range in `clear_bss` before it reads the device tree. Those two lines are
/// the whole of the rule for a raw `Image`, and they are reproduced here rather
/// than shared because `rv64-host` is the laptop side and pulls in `std::fs`.
///
/// The `text_offset` check is the host's too, for the same reason it exists
/// there: a kernel linked for a different offset was loaded at an address it
/// was not linked for, and that is a hang with no output.
///
/// # Why it then checks
///
/// Reproducing a rule is how the two copies drift, so this does not trust its
/// own arithmetic: it reads four bytes at the address it derived and requires
/// the FDT magic. If the host's placement ever changes, this reports
/// [`BootProbeError::NoFdt`] on the screen at boot instead of handing the
/// kernel a pointer to nothing — which presents as a dark screen with no
/// output, indistinguishable from a dead link.
///
/// **ELF kernels are not supported here**, deliberately: `load_kernel` accepts
/// them, but their extent comes from program headers this function would have
/// to re-parse, and the guest this project boots is a raw `Image`. An ELF
/// kernel reports [`BootProbeError::NoImageHeader`] rather than being loaded
/// somewhere approximate.
pub fn dtb_address<B: MemBacking, S: ConsoleSink, T: Transport>(
    bus: &mut Bus<B, S>,
    link: &Link<T>,
) -> Result<u64, BootProbeError> {
    let mut load = |addr: u64, size: u8| -> Result<u64, BootProbeError> {
        bus.load(addr, size)
            .map_err(|_| BootProbeError::Read { addr, fault: link.take_fault().map(Box::new) })
    };

    if load(KERNEL_LOAD_ADDR + 56, 4)? != IMAGE_MAGIC2 as u64 {
        return Err(BootProbeError::NoImageHeader);
    }

    let text_offset = load(KERNEL_LOAD_ADDR + 8, 8)?;
    let ours = KERNEL_LOAD_ADDR - rv64::RAM_BASE;
    if text_offset != ours {
        return Err(BootProbeError::TextOffset(text_offset));
    }

    let image_size = load(KERNEL_LOAD_ADDR + 16, 8)?;
    // A footprint smaller than the header it was read out of is corrupt, and
    // `checked_add` catches a crafted 64-bit value that would wrap. Both are
    // `load_kernel`'s own guards, kept for the same reason.
    if image_size < rv64::PAGE as u64 {
        return Err(BootProbeError::ImageSize(image_size));
    }
    let kernel_end =
        KERNEL_LOAD_ADDR.checked_add(image_size).ok_or(BootProbeError::ImageSize(image_size))?;
    let dtb = kernel_end.next_multiple_of(8);

    let magic = load(dtb, 4)? as u32;
    if magic != FDT_MAGIC_LE {
        return Err(BootProbeError::NoFdt { at: dtb, magic });
    }
    Ok(dtb)
}

/// Builds the CPU the riscv64 boot protocol asks for: S-mode, at
/// [`KERNEL_LOAD_ADDR`], `a0 = 0` (hartid) and `a1 = ` the DTB's address.
///
/// The privilege level is not a detail. `Cpu::new` starts in M-mode, and
/// `Cpu::step_trapping` only intercepts `ecall` from S-mode — left in M-mode
/// the guest's very first SBI console write raises
/// `EnvironmentCallFromMMode`, which nothing services, and the run produces no
/// output and never sees a shutdown. `rv64_host::load_boot_images` carries the
/// same three lines and the same warning.
pub fn start_cpu<B: MemBacking, S: ConsoleSink, T: Transport>(
    bus: &mut Bus<B, S>,
    link: &Link<T>,
) -> Result<Cpu, BootProbeError> {
    let dtb = dtb_address(bus, link)?;
    let mut cpu = Cpu::new(KERNEL_LOAD_ADDR);
    cpu.priv_ = Priv::S;
    cpu.set_reg(10, 0);
    cpu.set_reg(11, dtb);
    Ok(cpu)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Knobs the run loop reads. Every one of them is the same on the badge and on
/// the laptop except [`Config::max_insns`], which exists so a dry run
/// terminates rather than hanging a CI job.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Instructions between console/display service points.
    pub slice_insns: u64,
    /// Instruction budget. `u64::MAX` on the badge: a real boot ends at a
    /// shell prompt and stays there.
    pub max_insns: u64,
    /// Slices between heartbeat ticks.
    pub heartbeat_slices: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            slice_insns: SLICE_INSNS,
            max_insns: u64::MAX,
            heartbeat_slices: HEARTBEAT_SLICES,
        }
    }
}

/// How a run ended. Mirrors `rv64_host::RunOutcome` with two additions the
/// badge needs: the link fault behind a backing failure, and a caller-requested
/// stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The guest asked to power off. The only clean ending.
    Shutdown,
    /// [`Config::max_insns`] ran out.
    Capped,
    /// Guest memory failed. On the badge this is the link, and `fault` says
    /// how — see [`Link::take_fault`], and note that without it this is an
    /// undifferentiated load/store fault at an arbitrary guest address.
    Backing { addr: u64, fault: Option<Fault> },
    /// A trap vectored to M-mode, where this emulator installs no handler, so
    /// `mtvec` is 0 and the guest would spin at address 0 forever with no
    /// output. Cause 2 (illegal instruction) is the one that actually happens:
    /// it is what an unimplemented opcode raises.
    MachineTrap { mcause: u64, mepc: u64, mtval: u64 },
    /// The caller's watch said stop. Only the dry run uses it.
    Stopped,
}

impl Ending {
    /// A compact line for the badge's screen. Sixteen columns, so this is
    /// terse on purpose; the long form goes to `log::` and rides the mirror.
    pub fn describe(&self) -> String {
        match self {
            Ending::Shutdown => "-- shutdown --".into(),
            Ending::Capped => "-- insn cap --".into(),
            Ending::Backing { addr, fault } => match fault {
                Some(f) => format!("MEM FAULT {addr:#x} {}", f.describe()),
                None => format!("MEM FAULT {addr:#x} (no link fault recorded)"),
            },
            Ending::MachineTrap { mcause, mepc, mtval } => {
                format!("M-TRAP c{mcause} pc {mepc:#x} tv {mtval:#x}")
            }
            Ending::Stopped => "-- stopped --".into(),
        }
    }
}

/// Everything one run produced. The counters travel with the ending because
/// they are what the plan's Step 5 compares against the laptop: the
/// instruction count must match **exactly** (same guest, same work), and a
/// difference means the emulation diverged rather than merely ran slower.
#[derive(Debug, Clone)]
pub struct Report {
    pub ending: Ending,
    /// Instructions retired.
    pub executed: u64,
    pub cache: Stats,
    pub mmu_walks: u64,
    /// Requests re-sent after an attempt went unanswered. Expected small but
    /// non-zero with the panic mirror hooked — mirrored log text shares the
    /// CDC transmit endpoint with request frames and splits one occasionally.
    pub retries: usize,
    /// Duplicate responses discarded before they could satisfy a later
    /// exchange.
    pub stale_dropped: usize,
    /// Duplicate responses that arrived *during* a wait, after the pre-send
    /// purge had already run, and were dropped so the wait could continue.
    ///
    /// This is the counter the twenty-fourth run needed and did not have. One
    /// such duplicate used to end the boot with a `MEM FAULT` naming the
    /// previous page; now it costs one turn of the receive loop and shows up
    /// here. Non-zero means the wire carried more answers than the badge asked
    /// questions -- read it next to `serve`'s own duplicate-request line to say
    /// which direction duplicated.
    pub late_dropped: usize,
    /// Console-pump failures. Non-fatal by design: a failed non-blocking poll
    /// costs keystrokes, not correctness, and killing a boot over one would
    /// be worse than the symptom.
    pub pump_faults: usize,
    /// Failures to mirror guest console output back to the host as `ConOut`.
    /// Non-fatal for the same reason, and it costs a transcript line rather
    /// than a keystroke. Non-zero means the serial log has holes in it.
    pub mirror_faults: usize,
    /// Wall-clock milliseconds from the first slice to the ending.
    pub wall_ms: u64,
    /// Of which, blocked on a page exchange. See [`Link::blocked`].
    pub link_ms: u64,
    /// Of which, spent in the between-slice service block -- the console pump,
    /// the `ConOut` mirror, the heartbeat and the repaint.
    ///
    /// Small on the laptop and *not* small on the badge: a heartbeat tick is a
    /// whole-screen `draw_textview` over IPC. It is subtracted rather than
    /// lumped into either of the other two because it is neither interpreting
    /// nor waiting for the host, and leaving it in `cpu` would understate the
    /// interpreter by however slow the OLED happens to be.
    pub service_ms: u64,
    /// Page exchanges behind [`Report::link_ms`], so a transcript reader can
    /// divide and get the per-page cost.
    pub exchanges: usize,
    /// Clock samples the run loop had to drop. Non-zero invalidates the timing
    /// (not the emulation) -- see [`Span`].
    pub clock_drops: u32,
}

impl Report {
    /// Milliseconds spent interpreting: wall clock less the link and less the
    /// service block. **This is the denominator that matters** — the badge is
    /// compute-bound, and a boot's wall clock is dominated by whatever the
    /// transport is doing that week.
    pub fn cpu_ms(&self) -> u64 {
        self.wall_ms.saturating_sub(self.link_ms).saturating_sub(self.service_ms)
    }

    /// Guest instructions retired per second of interpreting. `None` when there
    /// is no interpreting time to divide by — see [`rate_line`].
    pub fn insn_per_sec(&self) -> Option<u64> {
        let cpu = self.cpu_ms();
        (cpu != 0).then(|| self.executed.saturating_mul(1000) / cpu)
    }

    /// The one line. Same text the run loop logs periodically.
    pub fn rate_line(&self) -> String {
        rate_line(self.executed, self.wall_ms, self.link_ms, self.service_ms)
    }

    /// The line a transcript wants. Long; goes to `log::` and to the dry run's
    /// stderr, never to the screen.
    pub fn summary(&self) -> String {
        let clock = if self.clock_drops == 0 {
            String::new()
        } else {
            format!("\nWARNING: {} dropped clock samples; the timing above is short", self.clock_drops)
        };
        format!(
            "{}\ninstructions: {}\npage cache: hits={} misses={} evictions={} writebacks={} declined={}\n\
             mmu walks: {}\nlink: retries={} stale={} late={} pump_faults={} mirror_faults={}",
            self.ending.describe(),
            self.executed,
            self.cache.hits,
            self.cache.misses,
            self.cache.evictions,
            self.cache.writebacks,
            self.cache.declined,
            self.mmu_walks,
            self.retries,
            self.stale_dropped,
            self.late_dropped,
            self.pump_faults,
            self.mirror_faults,
        ) + &format!("\n{}\nexchanges: {}{}", self.rate_line(), self.exchanges, clock)
    }
}

/// Runs the guest.
///
/// `watch` is called once per slice with the bus and the instruction count so
/// far; returning `false` ends the run with [`Ending::Stopped`]. The badge
/// passes a closure that never stops. The dry run passes one that watches the
/// console for a shell prompt — which is the only reason this parameter exists,
/// and it is a parameter rather than a `stop_marker` field because a marker
/// field would have to look at the sink, and the sink is generic on purpose.
///
/// Ends by writing its own ending to the console and repainting, so a badge
/// with no debugger attached shows *why* it stopped rather than freezing on the
/// last thing the guest said.
pub fn run<T, C, W>(
    cpu: &mut Cpu,
    bus: &mut Bus<UsbHost<T>, C>,
    link: &Link<T>,
    cfg: &Config,
    mut watch: W,
) -> Report
where
    T: Transport,
    C: Console,
    W: FnMut(&mut Bus<UsbHost<T>, C>, u64) -> bool,
{
    let mut executed = 0u64;
    let mut slices = 0u64;
    let mut pump_faults = 0usize;
    let mut mirror_faults = 0usize;

    // --- the throughput instrument ---
    //
    // Three spans, and the whole point is what is *not* in the third:
    //
    // * `wall` is the run, opened here and closed after the loop, so a run that
    //   ends inside a slice is still accounted for.
    // * `link` is not measured here at all -- `LinkInner::exchange` accumulates
    //   it, because a page fault happens *inside* `Cpu::step` and the run loop
    //   cannot see one from the outside.
    // * `service` is the between-slice block: the pump, the `ConOut` mirror,
    //   the heartbeat and the repaint.
    //
    // `cpu = wall - link - service` is then time spent interpreting, and
    // `executed / cpu` is the number this instrument exists to produce. Two
    // clock reads per slice (100 000 instructions) and two per page exchange:
    // on the badge that is ~3,500 ticktimer calls across a whole boot.
    let mut wall = Span::default();
    let mut service = Span::default();
    wall.open(link.now_ms());

    let ending = 'outer: loop {
        if executed >= cfg.max_insns {
            break Ending::Capped;
        }
        let budget = cfg.slice_insns.min(cfg.max_insns - executed);

        for _ in 0..budget {
            // The undelegated-trap dead end. `Csrs::default` delegates the
            // standard S-mode set, but a cause outside it — cause 2, illegal
            // instruction, above all — vectors to `mtvec`, which nothing has
            // written and which is therefore 0. The CPU lands at 0, faults
            // fetching there, traps again, and spins forever with no output.
            //
            // This must fire on the *first* arrival, while mcause/mepc/mtval
            // still describe the instruction that caused it; one more
            // iteration and the re-trap from the fetch at 0 overwrites them
            // with mepc = 0. The two field comparisons are checked first so
            // the CSR read happens only when they hold — `rv64_host::run_until`
            // reads `mtvec` every instruction, which is affordable on a laptop
            // and not a habit worth carrying to a microcontroller.
            if cpu.pc == 0 && cpu.priv_ == Priv::M && cpu.csrs.read(csr::MTVEC) == 0 {
                break 'outer Ending::MachineTrap {
                    mcause: cpu.csrs.read(csr::MCAUSE),
                    mepc: cpu.csrs.read(csr::MEPC),
                    mtval: cpu.csrs.read(csr::MTVAL),
                };
            }

            // Before the step, never inside it. See the module docs.
            cpu.check_interrupts(bus);
            match cpu.step_trapping(bus) {
                Ok(SbiOutcome::Shutdown) => {
                    // The shutdown `ecall` itself retired.
                    executed += 1;
                    break 'outer Ending::Shutdown;
                }
                Ok(SbiOutcome::Handled) => executed += 1,
                // Guest memory failed, so the faulting instruction never
                // completed and is not counted. Take the fault *here*: the
                // next `Error::Medium` would overwrite it, and without it the
                // report is a load/store fault at an address that means
                // nothing.
                Err(addr) => {
                    break 'outer Ending::Backing { addr, fault: link.take_fault() }
                }
            }
            bus.clint.tick(1);
        }

        // --- between slices: everything that is not the guest executing ---

        // Everything from here to `flush()` is the service block, and it is
        // timed so it can be taken *out* of the interpreter's denominator.
        service.open(link.now_ms());

        // Non-blocking. Without it, console input reaches the guest only as a
        // side effect of a page fault — which is to say never, once the
        // working set is resident and the guest is sitting at a prompt, which
        // is exactly when a human is typing.
        if link.pump().is_err() {
            // Counted and reported, not fatal. A failed poll costs keystrokes;
            // ending a boot over one would be strictly worse than the symptom.
            // The fault is still drained so it cannot masquerade as the cause
            // of a later `Error::Medium`.
            pump_faults += 1;
            link.take_fault();
        }
        for b in link.take_console() {
            bus.uart.push_input(b);
        }

        // The other direction: whatever the guest printed during this slice
        // goes back to the host as `ConOut`, so the serial transcript carries
        // it and the eight-row screen is not the only record. Non-fatal by
        // design -- losing a transcript line is not worth ending a boot over --
        // but counted, and the fault drained so it cannot masquerade as the
        // cause of a later `Error::Medium`.
        let out = bus.uart.sink.take_output();
        if !out.is_empty() && link.send_console(&out).is_err() {
            mirror_faults += 1;
            link.take_fault();
        }

        slices += 1;
        if cfg.heartbeat_slices != 0 && slices.is_multiple_of(cfg.heartbeat_slices) {
            bus.uart.sink.tick();
        }
        // `pub sink: S`, not `sink_mut()`.
        bus.uart.sink.flush();

        service.close(link.now_ms());

        // The throughput report. `log::info!` and nothing else: on the badge
        // this rides the log server's USB mirror to the transcript's *stderr*
        // half, which is where badge diagnostics live. It deliberately does not
        // go through `note`, which writes into the guest's console sink -- that
        // stream is the guest's alone (§26), and a badge measurement in it would
        // be indistinguishable from something the kernel printed.
        if slices == FIRST_RATE_SLICE || slices.is_multiple_of(RATE_SLICES) {
            let (link_ms, _) = link.blocked();
            let so_far = match (wall.open_at, link.now_ms()) {
                (Some(a), Some(b)) => b.saturating_sub(a),
                _ => 0,
            };
            log::info!("{}", rate_line(executed, so_far, link_ms, service.total_ms));
        }

        if !watch(bus, executed) {
            break Ending::Stopped;
        }
    };

    wall.close(link.now_ms());

    // Say why, on the one output device the badge has. Guest text scrolls up
    // rather than being cleared, so the last thing the guest said is still on
    // screen above this.
    //
    // **Except [`Ending::Stopped`], which is never drawn.** That ending is the
    // caller asking to stop, not anything the guest or the link did, and the
    // badge's screen is the artifact this project exists to photograph — text
    // belonging to a test harness has no business appearing in it. It cannot
    // reach the badge anyway: the only site that produces `Stopped` is `watch`
    // returning false, and `main.rs` passes `|_, _| true`. But "cannot happen"
    // is a weaker guarantee than "is not drawn", and only one of the two
    // survives someone later giving the badge a stop condition of its own.
    let line = ending.describe();
    log::info!("rv64: {line}");
    if ending != Ending::Stopped {
        note(&mut bus.uart.sink, "");
        note(&mut bus.uart.sink, &line);
    }
    // One last drain, so the ending -- and whatever the guest said in its final
    // partial slice -- reaches the transcript instead of dying in a buffer three
    // lines above the place someone will be reading for it.
    let tail = bus.uart.sink.take_output();
    if !tail.is_empty() && link.send_console(&tail).is_err() {
        mirror_faults += 1;
        link.take_fault();
    }

    let (link_ms, exchanges) = link.blocked();
    let report = Report {
        ending,
        executed,
        cache: bus.cache_mut().stats(),
        mmu_walks: cpu.mmu.walks,
        retries: link.retries(),
        stale_dropped: link.stale_dropped(),
        late_dropped: link.late_dropped(),
        pump_faults,
        mirror_faults,
        wall_ms: wall.total_ms,
        link_ms,
        service_ms: service.total_ms,
        exchanges,
        clock_drops: wall.dropped.saturating_add(service.dropped),
    };
    // The last one, and the only one that covers the whole run. Logged rather
    // than left to the caller because `main.rs` parks after this returns and
    // the badge has no other way to say it.
    log::info!("{}", report.rate_line());
    report
}

/// The assembled machine: everything the run loop needs, and everything a
/// caller still wants afterwards.
///
/// The `link` is a field rather than a local because it is the *only* way back
/// to the transport once `PageCache` owns the backing — see [`assemble`].
pub struct Machine<T: Transport, C: Console> {
    pub cpu: Cpu,
    pub bus: Bus<UsbHost<T>, C>,
    pub link: Link<T>,
}

impl<T: Transport, C: Console> Machine<T, C> {
    /// Runs to an [`Ending`]. See [`run`].
    pub fn run<W>(&mut self, cfg: &Config, watch: W) -> Report
    where
        W: FnMut(&mut Bus<UsbHost<T>, C>, u64) -> bool,
    {
        run(&mut self.cpu, &mut self.bus, &self.link, cfg, watch)
    }
}

/// Assembles the machine: the four lines the plan's Step 2 shows, in the one
/// order that works.
///
/// **[`UsbHost::link`] is taken before `PageCache::new`**, and that ordering is
/// the correction the plan needed: `PageCache` owns its backing privately and
/// exposes no accessor, so the plan's `bus.backing_mut().take_console()` cannot
/// compile and there is no way to recover the backing once the cache has it.
/// [`Link`] is the cloneable handle that survives — console bytes, the fault
/// channel and the non-blocking pump all hang off it.
///
/// One function rather than five lines at each call site, because there are two
/// call sites — the badge's `main` and the laptop's dry run — and the whole
/// value of the dry run is that they are not two implementations.
pub fn assemble<T: Transport, C: Console>(
    transport: T,
    console: C,
    frames: usize,
) -> Result<Machine<T, C>, BootProbeError> {
    let backing = UsbHost::new(transport);
    // Before the cache consumes it. There is no second chance.
    let link = backing.link();
    let mut bus = Bus::new(PageCache::new(backing, frames), console);
    let cpu = start_cpu(&mut bus, &link)?;
    Ok(Machine { cpu, bus, link })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oled::FakeScreen;
    use crate::usbhost::SendError;
    use rv64_proto::{encode, Frame, Mux};

    /// A transport backed by a flat page array in this process. Not the dry
    /// run — that one talks to the real `rv64_host::serve` over a socket (see
    /// `tests/dry_run.rs`). This is for the small facts that want a hand-built
    /// image: the DTB derivation, the endings, the input path.
    struct Ram {
        pages: Vec<[u8; rv64::PAGE]>,
        pending: Vec<u8>,
        /// Bytes to hand the guest as console input on the next `poll`.
        inject: Vec<u8>,
        clock: u64,
        /// Milliseconds the clock advances on every `now_ms` call.
        ///
        /// Zero by default, which is what every test written before the
        /// throughput instrument assumes: with it zero the clock moves *only*
        /// inside `recv`, i.e. only while an exchange is blocked, which is what
        /// makes `the_link_gets_every_millisecond_it_actually_costs` an exact
        /// assertion rather than an approximate one. Set it non-zero to model a
        /// machine where interpreting also takes time.
        now_tick: u64,
        /// Set to fail every read, to exercise the `Error::Medium` path.
        broken: bool,
    }

    impl Ram {
        fn new(pages: usize) -> Self {
            Self {
                pages: vec![[0u8; rv64::PAGE]; pages],
                pending: Vec::new(),
                inject: Vec::new(),
                clock: 0,
                now_tick: 0,
                broken: false,
            }
        }

        fn write_at(&mut self, gpa: u64, bytes: &[u8]) {
            let base = (gpa - rv64::RAM_BASE) as usize;
            for (i, &b) in bytes.iter().enumerate() {
                let off = base + i;
                self.pages[off / rv64::PAGE][off % rv64::PAGE] = b;
            }
        }
    }

    impl Transport for Ram {
        fn arm(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send(&mut self, bytes: &[u8]) -> Result<(), SendError> {
            if self.broken {
                return Err(SendError::Failed);
            }
            let mut m = Mux::new();
            m.push(bytes);
            // Console output the badge mirrored back. `rv64_host::serve` writes
            // this to stdout; here it goes to MIRRORED, because once
            // `PageCache` owns the backing the transport is unreachable and
            // `usbhost` should not grow an accessor for a test's convenience.
            let con = m.take_console();
            if !con.is_empty() {
                MIRRORED.with(|v| v.borrow_mut().extend_from_slice(&con));
            }
            if let Some(Frame::ReadReq { page }) = m.take_matching(0x01) {
                let data = Box::new(self.pages[page as usize]);
                encode(&Frame::ReadResp { page, data }, &mut self.pending);
            }
            if let Some(Frame::WriteReq { page, data }) = m.take_matching(0x03) {
                self.pages[page as usize] = *data;
                encode(&Frame::WriteAck { page }, &mut self.pending);
            }
            Ok(())
        }
        fn recv(&mut self) -> Result<Vec<u8>, ()> {
            self.clock += 1;
            Ok(core::mem::take(&mut self.pending))
        }
        fn poll(&mut self) -> Result<Vec<u8>, ()> {
            if self.inject.is_empty() {
                return Ok(Vec::new());
            }
            let mut out = Vec::new();
            encode(&Frame::ConIn(core::mem::take(&mut self.inject)), &mut out);
            Ok(out)
        }
        fn now_ms(&mut self) -> u64 {
            let now = self.clock;
            self.clock += self.now_tick;
            now
        }
    }

    /// A minimal riscv64 boot header: magic, `text_offset`, `image_size`.
    fn image_header(image_size: u64) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[8..16].copy_from_slice(&(KERNEL_LOAD_ADDR - rv64::RAM_BASE).to_le_bytes());
        h[16..24].copy_from_slice(&image_size.to_le_bytes());
        h[56..60].copy_from_slice(b"RSC\x05");
        h
    }

    fn oled() -> OledSink<FakeScreen> {
        OledSink::with_screen(FakeScreen::default())
    }

    fn ram_with_image(image_size: u64, dtb_magic: bool) -> Ram {
        let mut r = Ram::new((rv64::RAM_SIZE / rv64::PAGE as u64) as usize);
        r.write_at(KERNEL_LOAD_ADDR, &image_header(image_size));
        if dtb_magic {
            let dtb = (KERNEL_LOAD_ADDR + image_size).next_multiple_of(8);
            r.write_at(dtb, &0xd00d_feedu32.to_be_bytes());
        }
        r
    }

    /// The rule this reproduces is `rv64_host::boot_layout`'s, and the number
    /// it must not use is the file length. An `image_size` that is *larger*
    /// than the kernel's file — which is the real case, because `objcopy -O
    /// binary` drops `.bss` — puts the DTB past the end of `.bss`, and using
    /// the file length instead would put it inside.
    #[test]
    fn the_dtb_address_comes_from_image_size_and_is_eight_aligned() {
        // Deliberately not a multiple of 8, so the alignment step is visible.
        let size = 0x30_0001u64;
        let backing = UsbHost::new(ram_with_image(size, true));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        assert_eq!(
            dtb_address(&mut bus, &link).unwrap(),
            (KERNEL_LOAD_ADDR + size).next_multiple_of(8)
        );
    }

    #[test]
    fn a_missing_boot_header_is_reported_rather_than_guessed_at() {
        let backing = UsbHost::new(Ram::new(1024));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        assert_eq!(dtb_address(&mut bus, &link), Err(BootProbeError::NoImageHeader));
    }

    /// The check that catches a drift between this derivation and the host's:
    /// the arithmetic succeeds and the address holds no device tree.
    #[test]
    fn a_derived_address_with_no_fdt_magic_is_refused() {
        let size = 0x30_0000u64;
        let backing = UsbHost::new(ram_with_image(size, false));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        assert!(matches!(
            dtb_address(&mut bus, &link),
            Err(BootProbeError::NoFdt { .. })
        ));
    }

    #[test]
    fn a_kernel_linked_for_another_offset_is_refused_rather_than_run() {
        let mut r = Ram::new((rv64::RAM_SIZE / rv64::PAGE as u64) as usize);
        let mut h = image_header(0x30_0000);
        h[8..16].copy_from_slice(&0x40_0000u64.to_le_bytes());
        r.write_at(KERNEL_LOAD_ADDR, &h);
        let backing = UsbHost::new(r);
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        assert_eq!(dtb_address(&mut bus, &link), Err(BootProbeError::TextOffset(0x40_0000)));
    }

    /// A dead link during the probe must arrive with the link's own diagnosis
    /// attached, not as a bare address. This is the `take_fault` rule at the
    /// earliest point it applies.
    #[test]
    fn a_probe_read_over_a_dead_link_carries_the_link_fault() {
        let mut r = Ram::new(1024);
        r.broken = true;
        let backing = UsbHost::new(r);
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        match dtb_address(&mut bus, &link) {
            Err(BootProbeError::Read { fault, .. }) => {
                assert!(fault.is_some(), "the link fault was lost behind a masked error")
            }
            other => panic!("expected a read failure, got {other:?}"),
        }
    }

    #[test]
    fn start_cpu_places_the_guest_where_the_boot_protocol_says() {
        let size = 0x30_0000u64;
        let backing = UsbHost::new(ram_with_image(size, true));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let cpu = start_cpu(&mut bus, &link).unwrap();
        assert_eq!(cpu.pc, KERNEL_LOAD_ADDR);
        assert_eq!(cpu.priv_, Priv::S);
        assert_eq!(cpu.reg(10), 0);
        assert_eq!(cpu.reg(11), (KERNEL_LOAD_ADDR + size).next_multiple_of(8));
    }

    /// `rv64_host`'s own `HI_AND_SHUTDOWN`, verbatim: two SBI `console_putchar`
    /// calls and an SBI shutdown. Copied rather than hand-assembled because a
    /// mis-encoded fixture fails as "the run loop is broken", which is the one
    /// diagnosis this test exists to make trustworthy.
    const HI_AND_SHUTDOWN: [u32; 8] = [
        0x00100893, // li a7, 1        (console_putchar)
        0x06800513, // li a0, 'h'
        0x00000073, // ecall
        0x00100893, // li a7, 1
        0x06900513, // li a0, 'i'
        0x00000073, // ecall
        0x00800893, // li a7, 8        (shutdown)
        0x00000073, // ecall
    ];

    /// `jal x0, 0` — a one-instruction infinite loop. A guest that never
    /// finishes, which is what the cap, the watch and the heartbeat are all
    /// about. Zeroed RAM is *not* a substitute: an all-zero word is an illegal
    /// instruction, which vectors to M-mode and ends the run as
    /// [`Ending::MachineTrap`] on the second instruction.
    const SPIN: u32 = 0x0000_006f;

    fn ram_running(program: &[u32], at: u64) -> Ram {
        let mut r = Ram::new((rv64::RAM_SIZE / rv64::PAGE as u64) as usize);
        let mut bytes = Vec::new();
        for w in program {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        r.write_at(at, &bytes);
        r
    }

    /// The guest's console reaches the screen and its shutdown ends the run —
    /// the two halves of the loop's contract, through the real `UsbHost`,
    /// `PageCache` and `OledSink`.
    #[test]
    fn a_guest_that_prints_and_shuts_down_reaches_the_screen() {
        let backing = UsbHost::new(ram_running(&HI_AND_SHUTDOWN, rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let report = run(&mut cpu, &mut bus, &link, &Config::default(), |_, _| true);
        assert_eq!(report.ending, Ending::Shutdown);
        assert_eq!(report.executed, 8, "the shutdown ecall itself must be counted");
        // Rows 0 and 1 are the banner; the guest starts on row 2.
        assert_eq!(bus.uart.sink.grid().line(2), "hi");
    }

    /// The same program, entered through [`boot`] rather than [`run`]: the
    /// assembly the badge's `main` performs — the link taken *before*
    /// `PageCache::new` consumes the backing, the DTB derived from the image,
    /// the CPU placed — exercised whole.
    ///
    /// The image is a real one in shape: a boot header at offset 0 whose first
    /// word is also its first instruction (which is exactly what a riscv64
    /// `Image` is — `head.S` starts with a jump), the program past the header
    /// fields the probe reads, and a device tree above `image_size`.
    #[test]
    fn boot_assembles_the_machine_and_runs_it() {
        const JUMP_PAST_THE_HEADER: u32 = 0x0400_006f; // jal x0, +64
        let size = 0x1_0000u64;
        let mut r = ram_with_image(size, true);
        r.write_at(KERNEL_LOAD_ADDR, &JUMP_PAST_THE_HEADER.to_le_bytes());
        let mut bytes = Vec::new();
        for w in &HI_AND_SHUTDOWN {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        r.write_at(KERNEL_LOAD_ADDR + 64, &bytes);

        let mut m = assemble(r, oled(), 16).expect("the machine must assemble");
        assert_eq!(m.cpu.reg(11), (KERNEL_LOAD_ADDR + size).next_multiple_of(8));
        let report = m.run(&Config::default(), |_, _| true);
        assert_eq!(report.ending, Ending::Shutdown);
        assert_eq!(report.executed, 9, "the jump plus the eight-instruction program");
        assert_eq!(m.bus.uart.sink.grid().line(2), "hi");
    }

    /// The other half of the console: bytes the host sends arrive as guest
    /// *input*, through `Link::pump` between slices. Nothing else in the tree
    /// covers that path end to end.
    #[test]
    fn console_input_from_the_link_reaches_the_guests_uart() {
        let mut r = ram_running(&[SPIN], rv64::RAM_BASE);
        r.inject = b"ls\r".to_vec();
        let backing = UsbHost::new(r);
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        // One slice of nothing: the guest reads no instructions we care about,
        // and the input arrives at the service point between slices.
        let cfg = Config { slice_insns: 1, max_insns: 1, heartbeat_slices: 0 };
        run(&mut cpu, &mut bus, &link, &cfg, |_, _| true);
        // LSR data-ready, then the bytes, in order.
        assert_eq!(bus.load(0x1000_0000 + rv64::uart::LSR, 1).unwrap() & 1, 1);
        assert_eq!(bus.load(0x1000_0000, 1).unwrap(), b'l' as u64);
        assert_eq!(bus.load(0x1000_0000, 1).unwrap(), b's' as u64);
        assert_eq!(bus.load(0x1000_0000, 1).unwrap(), b'\r' as u64);
    }

    /// The instruction cap terminates rather than hanging, and says so on the
    /// screen.
    #[test]
    fn the_instruction_cap_ends_the_run_and_names_itself() {
        let backing = UsbHost::new(ram_running(&[SPIN], rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let cfg = Config { slice_insns: 8, max_insns: 32, heartbeat_slices: 0 };
        let report = run(&mut cpu, &mut bus, &link, &cfg, |_, _| true);
        assert_eq!(report.ending, Ending::Capped);
        assert_eq!(report.executed, 32);
        let frame = bus.uart.sink.frame();
        assert!(frame.contains("insn cap"), "the ending is not on the screen:\n{frame}");
    }

    /// A dead link mid-run reports the link's own diagnosis, not a bare guest
    /// address. This is the rule that has already cost a hardware cycle once.
    #[test]
    fn a_backing_failure_carries_the_link_fault_to_the_screen() {
        let mut r = Ram::new(1024);
        r.broken = true;
        let backing = UsbHost::new(r);
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let report = run(&mut cpu, &mut bus, &link, &Config::default(), |_, _| true);
        match &report.ending {
            Ending::Backing { fault, .. } => {
                assert!(fault.is_some(), "the link fault was lost behind a masked error")
            }
            other => panic!("expected a backing failure, got {other:?}"),
        }
        assert!(bus.uart.sink.frame().contains("MEM FAULT"));
    }

    /// Guest console output reaches the host as `ConOut`, so the serial
    /// transcript is not limited to eight rows of OLED.
    ///
    /// The screen keeps every byte too: mirroring is a second channel, never a
    /// diversion.
    #[test]
    fn guest_console_output_is_mirrored_to_the_host() {
        let backing = UsbHost::new(ram_running(&HI_AND_SHUTDOWN, rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), Mirrored::new(oled()));
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let report = run(&mut cpu, &mut bus, &link, &Config::default(), |_, _| true);
        assert_eq!(report.ending, Ending::Shutdown);
        assert_eq!(report.mirror_faults, 0);
        // Still on the screen.
        assert_eq!(bus.uart.sink.inner().grid().line(2), "hi");
        // And on the wire. The ending line is mirrored too, by the final drain
        // — which is the point of that drain: it is the last thing anyone
        // reading a transcript wants and the easiest thing to leave in a
        // buffer.
        let mirrored = MIRRORED.with(|m| m.borrow().clone());
        let text = String::from_utf8_lossy(&mirrored).into_owned();
        assert!(text.starts_with("hi"), "guest output never reached the host: {text:?}");
        assert!(
            text.contains("shutdown"),
            "the ending never reached the transcript, so it died in the mirror \
             buffer three lines above where someone would read for it: {text:?}"
        );
    }

    thread_local! {
        /// Where `Ram` publishes the `ConOut` payloads it received.
        ///
        /// A thread-local rather than a field on `Ram`, because once
        /// `PageCache` owns the backing the transport is unreachable — and
        /// `usbhost` should not grow a public accessor to make one test
        /// convenient. Test threads are one per test, so this isolates.
        static MIRRORED: core::cell::RefCell<Vec<u8>> =
            const { core::cell::RefCell::new(Vec::new()) };
    }

    /// The mirror buffer is bounded, and says so rather than growing without
    /// limit on a machine with 2 MiB of RAM and a link that has stopped taking
    /// bytes.
    #[test]
    fn the_mirror_buffer_is_capped_and_counts_what_it_drops() {
        let mut m = Mirrored::new(oled());
        for _ in 0..(MIRROR_CAP + 100) {
            m.put(b'x');
        }
        assert_eq!(m.dropped(), 100);
        assert_eq!(m.take_output().len(), MIRROR_CAP);
        // Draining resets it: the cap is on what is *held*, not on a lifetime
        // total.
        m.put(b'y');
        assert_eq!(m.take_output(), b"y");
    }

    /// A sink with nothing to mirror mirrors nothing, and the run loop does not
    /// send an empty frame every slice for it.
    #[test]
    fn a_plain_console_mirrors_nothing() {
        let mut o = oled();
        o.put(b'z');
        assert!(o.take_output().is_empty());
    }

    /// A harness stop leaves **nothing** on the screen.
    ///
    /// The badge's display is the deliverable — a photograph of real store
    /// paths — and `-- stopped --` is a test harness talking. The other endings
    /// are drawn, and must be: they are the only report a badge with no
    /// debugger attached can make.
    #[test]
    fn a_harness_stop_writes_nothing_to_the_screen() {
        let backing = UsbHost::new(ram_running(&[SPIN], rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let cfg = Config { slice_insns: 1, max_insns: u64::MAX, heartbeat_slices: 0 };
        let report = run(&mut cpu, &mut bus, &link, &cfg, |_, n| n < 4);
        assert_eq!(report.ending, Ending::Stopped);
        let frame = bus.uart.sink.frame();
        assert!(
            !frame.contains("stopped"),
            "harness text reached the artifact being photographed:\n{frame}"
        );
        // Row 2 onward is still exactly as the guest left it: blank.
        assert_eq!(bus.uart.sink.grid().line(2), "");
    }

    /// The badge's own watch — `|_, _| true`, the literal `main.rs` passes —
    /// cannot produce [`Ending::Stopped`], whatever the guest does.
    #[test]
    fn the_badge_watch_can_never_stop_the_run() {
        for program in [&[SPIN][..], &HI_AND_SHUTDOWN[..], &[0u32][..]] {
            let backing = UsbHost::new(ram_running(program, rv64::RAM_BASE));
            let link = backing.link();
            let mut bus = Bus::new(PageCache::new(backing, 16), oled());
            let mut cpu = Cpu::new(rv64::RAM_BASE);
            cpu.priv_ = Priv::S;
            let cfg = Config { slice_insns: 4, max_insns: 64, heartbeat_slices: 0 };
            let report = run(&mut cpu, &mut bus, &link, &cfg, |_, _| true);
            assert_ne!(report.ending, Ending::Stopped, "program {program:?}");
        }
    }

    /// `watch` returning false stops the run — the hook the dry run boots
    /// through, proven here so its absence would fail a unit test rather than
    /// a forty-second boot.
    #[test]
    fn the_watch_can_stop_the_run_between_slices() {
        let backing = UsbHost::new(ram_running(&[SPIN], rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let cfg = Config { slice_insns: 4, max_insns: u64::MAX, heartbeat_slices: 0 };
        let mut seen = 0u64;
        let report = run(&mut cpu, &mut bus, &link, &cfg, |_, n| {
            seen = n;
            n < 12
        });
        assert_eq!(report.ending, Ending::Stopped);
        assert_eq!(report.executed, 12);
        assert_eq!(seen, 12);
    }

    /// The heartbeat is rate-limited on purpose: a tick per slice would repaint
    /// the whole display every 100 000 instructions. This pins the divisor,
    /// because getting it wrong is invisible on a laptop and expensive on the
    /// badge.
    #[test]
    fn the_heartbeat_ticks_once_per_configured_run_of_slices() {
        let backing = UsbHost::new(ram_running(&[SPIN], rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        // 40 slices of 1 instruction, ticking every 16: two ticks.
        let cfg = Config { slice_insns: 1, max_insns: 40, heartbeat_slices: 16 };
        run(&mut cpu, &mut bus, &link, &cfg, |_, _| true);
        let spins: usize = bus
            .uart
            .sink
            .screen_mut()
            .frames
            .iter()
            .filter_map(|f| f.last().cloned())
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0] != w[1])
            .count();
        // Two ticks can move the corner glyph at most twice; the point is that
        // it is not forty.
        assert!(spins <= 2, "the heartbeat ticked more often than configured: {spins}");
    }

    // -----------------------------------------------------------------------
    // The throughput instrument
    // -----------------------------------------------------------------------

    /// A refused clock sample must not be guessed at. This is the property the
    /// whole instrument rests on: a number that quietly absorbs a zero
    /// timestamp is worse than no number, because nothing in the transcript
    /// distinguishes it from a real measurement.
    #[test]
    fn a_span_drops_an_interval_it_cannot_time_rather_than_inventing_one() {
        let mut s = Span::default();
        s.open(Some(10));
        s.close(Some(25));
        assert_eq!(s.total_ms, 15);
        assert_eq!(s.dropped, 0);

        s.open(None);
        s.close(Some(99));
        assert_eq!(s.total_ms, 15, "an unopened interval must contribute nothing");
        assert_eq!(s.dropped, 1);

        s.open(Some(100));
        s.close(None);
        assert_eq!(s.total_ms, 15);
        assert_eq!(s.dropped, 2);

        // Intervals accumulate rather than replace, and a clock that went
        // backwards contributes zero instead of wrapping to eighteen
        // quintillion milliseconds.
        s.open(Some(200));
        s.close(Some(210));
        assert_eq!(s.total_ms, 25);
        s.open(Some(500));
        s.close(Some(400));
        assert_eq!(s.total_ms, 25);
    }

    /// The line is the deliverable, so its arithmetic is pinned: `cpu` is the
    /// subtraction, `ips` is the division, and there is exactly one token for a
    /// reader to look for.
    #[test]
    fn the_rate_line_reports_interpreting_time_not_wall_clock() {
        let l = rate_line(1_000_000, 10_000, 6_000, 1_000);
        assert!(l.contains("cpu=3000ms"), "{l}");
        // 1,000,000 instructions in 3 s of interpreting.
        assert!(l.contains("ips=333333"), "{l}");
        assert!(
            l.contains("wall=10000ms") && l.contains("link=6000ms") && l.contains("svc=1000ms"),
            "{l}"
        );
        assert_eq!(l.lines().count(), 1, "one line, or it is not cheap to read");
    }

    /// No interpreting time is the absence of a measurement, not a measurement
    /// of zero.
    #[test]
    fn a_rate_with_no_interpreting_time_says_so_instead_of_dividing() {
        assert!(rate_line(5, 100, 100, 0).contains("ips=?"));
        assert!(rate_line(5, 100, 90, 10).contains("ips=?"));
        // And the subtraction saturates rather than wrapping if the two
        // components somehow exceed the wall clock.
        assert!(rate_line(5, 100, 90, 90).contains("cpu=0ms"));
    }

    /// The exact assertion the fake clock buys: `Ram` advances time *only*
    /// inside `recv`, i.e. only while an exchange is blocked. So on this
    /// transport every millisecond the run consumed belongs to the link, and
    /// the interpreter's share must come out at exactly zero. If page-wait time
    /// ever leaked into `cpu_ms`, this is the test that fails.
    #[test]
    fn the_link_gets_every_millisecond_it_actually_costs() {
        let backing = UsbHost::new(ram_running(&HI_AND_SHUTDOWN, rv64::RAM_BASE));
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let report = run(&mut cpu, &mut bus, &link, &Config::default(), |_, _| true);

        assert_eq!(report.ending, Ending::Shutdown);
        assert!(report.exchanges > 0, "the guest must have faulted at least one page in");
        assert!(report.link_ms > 0, "a clock that only moves in `recv` must show link time");
        assert_eq!(report.wall_ms, report.link_ms, "nothing else moved this clock");
        assert_eq!(report.cpu_ms(), 0);
        assert_eq!(report.insn_per_sec(), None, "zero interpreting time is not a rate");
        assert_eq!(report.clock_drops, 0, "the run loop must never fail to read the clock");
    }

    /// With a clock that also advances outside the link, the three spans stay
    /// disjoint and inside the wall clock, and the interpreter gets a non-zero
    /// share. An inequality rather than an equality is deliberate: the exact
    /// split depends on how many times the loop happens to read the clock,
    /// which is an implementation detail, while "these do not overlap and do
    /// not exceed the whole" is the property.
    #[test]
    fn interpreting_service_and_link_are_three_disjoint_shares_of_the_wall_clock() {
        let mut ram = ram_running(&[SPIN], rv64::RAM_BASE);
        ram.now_tick = 1;
        let backing = UsbHost::new(ram);
        let link = backing.link();
        let mut bus = Bus::new(PageCache::new(backing, 16), oled());
        let mut cpu = Cpu::new(rv64::RAM_BASE);
        cpu.priv_ = Priv::S;
        let cfg = Config { slice_insns: 4, max_insns: 400, heartbeat_slices: 16 };
        let report = run(&mut cpu, &mut bus, &link, &cfg, |_, _| true);

        assert_eq!(report.ending, Ending::Capped);
        assert!(report.service_ms > 0, "the between-slice block took time and must be counted");
        assert!(report.cpu_ms() > 0, "the interpreter must get a non-zero share");
        assert!(
            report.wall_ms >= report.link_ms + report.service_ms,
            "spans overlapped: wall={} link={} svc={}",
            report.wall_ms,
            report.link_ms,
            report.service_ms
        );
        assert!(report.insn_per_sec().is_some());
        assert_eq!(report.clock_drops, 0);
        // The one line reaches the transcript through `summary`, which is what
        // the dry run prints and what the badge logs.
        assert!(report.summary().contains("rv64 rate: "), "{}", report.summary());
    }
}
