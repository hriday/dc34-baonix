//! 8250-style UART register model and the `ConsoleSink` trait that decouples
//! it from any concrete output device.
//!
//! Models exactly enough of a real 8250/16450 for a Linux 8250 serial driver
//! to initialize cleanly and exchange bytes: RBR/THR, IER, IIR, LCR
//! (including the divisor-latch-access bit), and LSR. No FIFOs, no MCR/MSR,
//! no loopback — this emulator has exactly one guest driver to satisfy and
//! none of those affect whether it boots to a clean shell prompt.
//!
//! IIR reports a *pending cause* rather than a hard-wired "nothing pending",
//! and that distinction is load-bearing — see [`IIR_NO_INTERRUPT`].

extern crate alloc;
use alloc::collections::VecDeque;

pub const RBR: u64 = 0; // read, only when DLAB=0
pub const THR: u64 = 0; // write, only when DLAB=0
pub const IER: u64 = 1; // only when DLAB=0
pub const IIR: u64 = 2; // read-only
pub const LCR: u64 = 3;
pub const LSR: u64 = 5;

pub const LSR_DR: u64 = 1 << 0; // data ready
pub const LSR_THRE: u64 = 1 << 5; // transmit holding register empty
pub const LSR_TEMT: u64 = 1 << 6; // transmitter empty

/// LCR bit 7: Divisor Latch Access Bit. While set, offsets 0 and 1 stop
/// meaning RBR/THR and IER and instead address the baud-rate divisor latch
/// (DLL/DLM). Linux's 8250 driver sets this, writes the divisor, then clears
/// it again during ordinary `set_termios` initialization — not an edge case.
/// Without honoring it, that init sequence writes the divisor's low byte
/// straight to THR (a garbage byte on the only output device this project
/// has) and the high byte into IER.
pub const LCR_DLAB: u8 = 1 << 7;

/// IIR bit 0: 1 = no interrupt pending, 0 = an interrupt is pending, with the
/// cause in bits 3:1. Returning 0 for this offset (this crate's default for
/// unmodelled registers) would tell a probing driver that an interrupt is
/// permanently outstanding and never serviced.
///
/// **This value must not be returned unconditionally, and doing so was a
/// deliberate earlier decision that turned out to be wrong.** Task 13's
/// review recorded a hardwired `IIR_NO_INTERRUPT` as load-bearing for polled
/// transmission. That reasoning is superseded; anyone tempted to "restore" it
/// on the badge port should read this first.
///
/// What actually happens, against `drivers/tty/serial/8250/8250_port.c` in
/// the 6.12 tree this project builds:
///
///  - `autoconfig_irq()`'s THRE test (`:2305`) *is* gated on `port->irq`, so
///    it is skipped here — `guest.dts` gives the UART no `interrupts`
///    property and there is no PLIC, so Linux binds the port with `irq = 0`.
///  - The **TXEN test (`:2379`-`:2398`) is not gated on `port->irq`** and
///    runs anyway. Against the old model — LSR permanently `TEMT`, IIR
///    permanently "nothing pending" — it concludes the port cannot assert a
///    TX interrupt and sets `UART_BUG_TXEN`.
///  - `__start_tx()` (`:1522`-`:1528`) then takes the bug path: it pushes
///    exactly `tx_loadsz` bytes — 16 for the 16550A this DT declares — and
///    waits for an interrupt this machine has no way to deliver.
///
/// So the workaround exists for hardware that fails to *assert* a TX
/// interrupt, which is precisely the defect the old model had. It buys one
/// FIFO load and nothing more. Reporting a real cause instead disables
/// `UART_BUG_TXEN` and moves TX onto the poll-timer path — `serial8250_timeout()`
/// → `serial8250_default_handle_irq()` → `serial8250_handle_irq()` — which
/// drains the buffer for as long as the driver keeps `IER.THRI` set.
///
/// It looked sufficient for two years of this project only because kernel
/// `printk` never exercises it: `serial8250_console_write()` busy-polls
/// `LSR_THRE` itself and never consults IIR. The defect was invisible until
/// there was a userland to expose it. Measured exactly that way — `/init`'s
/// first line stopped dead after 16 characters and had not advanced 1.7
/// billion instructions later.
pub const IIR_NO_INTERRUPT: u64 = 0x01;

/// IIR cause 0b001: the transmit holding register is empty, so the driver may
/// load more bytes. Whether it is empty comes from [`Uart::thr_empty`], which
/// is also what fills in `LSR`'s `THRE`/`TEMT` — see that method for why the
/// two must not be written down separately.
///
/// IIR bits 7:6 ("FIFO enabled") are left at 0 and are *not* modelled. That
/// is safe only because `of_serial` sets a fixed port type from the DT
/// `compatible` string, so `autoconfig()` never probes them to decide what
/// this chip is. The phase-3 badge port does not change that: it runs this
/// same `rv64` core against this same `guest.dts`, so `compatible =
/// "ns16550a"` still makes `8250_of` set `UPF_FIXED_TYPE`,
/// `uart_configure_port` still skips `autoconfig()`, and `tx_loadsz` stays
/// 16 — `no-loopback-test` is a second, independent guard on that same
/// path. The risk belongs to a *future port that changes the guest device
/// tree* — drops `compatible = "ns16550a"`, or drops the DT binding
/// entirely — which would autoconfigure this chip as an 8250 with no FIFO
/// and drop `tx_loadsz` to 1. Worth checking there, not on the badge port
/// as such.
pub const IIR_THR_EMPTY: u64 = 0x02;

/// IIR cause 0b010: received data is available. Outranks
/// [`IIR_THR_EMPTY`], matching the 8250's fixed interrupt priority.
pub const IIR_RX_AVAILABLE: u64 = 0x04;

/// IER bit 0: raise an interrupt when received data is available.
pub const IER_RX_AVAILABLE: u8 = 1 << 0;
/// IER bit 1: raise an interrupt when the transmit holding register empties.
pub const IER_THR_EMPTY: u8 = 1 << 1;

/// Where guest console output goes. stdout on the host, an OLED on the
/// badge — the UART model itself never touches either; it only knows this
/// trait. That boundary is what keeps file/socket/display I/O entirely out
/// of this `no_std` core crate.
pub trait ConsoleSink {
    fn put(&mut self, byte: u8);
}

/// An in-memory sink for tests: bytes the guest "printed" land in `bytes`,
/// nothing else observes them.
#[derive(Default)]
pub struct VecSink {
    pub bytes: alloc::vec::Vec<u8>,
}

impl ConsoleSink for VecSink {
    fn put(&mut self, byte: u8) {
        self.bytes.push(byte);
    }
}

pub struct Uart<S: ConsoleSink> {
    pub sink: S,
    input: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    /// Baud-rate divisor latch, low/high byte. Nothing in this emulator
    /// consumes it (there is no simulated baud rate to program), but it must
    /// exist and absorb DLAB-shadowed writes to offsets 0/1 rather than
    /// letting them fall through to THR/IER.
    dll: u8,
    dlm: u8,
}

impl<S: ConsoleSink> Uart<S> {
    pub fn new(sink: S) -> Self {
        Self { sink, input: VecDeque::new(), ier: 0, lcr: 0, dll: 0, dlm: 0 }
    }

    pub fn push_input(&mut self, byte: u8) {
        self.input.push_back(byte);
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    /// Whether the transmit holding register can accept another byte.
    ///
    /// The single source of truth for two registers that a driver reads as
    /// one fact: `LSR`'s `THRE`/`TEMT` bits, and whether `IIR` may report
    /// [`IIR_THR_EMPTY`]. Writing "always empty" into both independently
    /// happens to be consistent *today* — `store` hands each byte straight to
    /// the sink and nothing can refuse it — but it encodes the same truth
    /// twice, and the moment a sink applies backpressure the two would
    /// disagree: IIR would invite the driver to load a byte that LSR says
    /// there is no room for, and TX would wedge. The badge's OLED sink is the
    /// obvious candidate. Deriving both from here makes that impossible by
    /// construction; a sink that can block only has to be reflected in this
    /// one method.
    fn thr_empty(&self) -> bool {
        true
    }

    pub fn load(&mut self, off: u64) -> u64 {
        match off {
            RBR if self.dlab() => self.dll as u64,
            RBR => self.input.pop_front().unwrap_or(0) as u64,
            IER if self.dlab() => self.dlm as u64,
            IER => self.ier as u64,
            // Only causes the guest has *enabled* in IER may be reported;
            // an 8250 does not signal a masked source. RX outranks TX.
            IIR if self.ier & IER_RX_AVAILABLE != 0 && !self.input.is_empty() => IIR_RX_AVAILABLE,
            IIR if self.ier & IER_THR_EMPTY != 0 && self.thr_empty() => IIR_THR_EMPTY,
            IIR => IIR_NO_INTERRUPT,
            LCR => self.lcr as u64,
            LSR => {
                let dr = if self.input.is_empty() { 0 } else { LSR_DR };
                let tx = if self.thr_empty() { LSR_THRE | LSR_TEMT } else { 0 };
                dr | tx
            }
            _ => 0,
        }
    }

    pub fn store(&mut self, off: u64, v: u64) {
        match off {
            THR if self.dlab() => self.dll = v as u8,
            THR => self.sink.put(v as u8),
            IER if self.dlab() => self.dlm = v as u8,
            IER => self.ier = v as u8,
            LCR => self.lcr = v as u8,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_thr_emits_a_byte() {
        let mut u = Uart::new(VecSink::default());
        u.store(THR, b'A' as u64);
        u.store(THR, b'B' as u64);
        assert_eq!(u.sink.bytes, b"AB");
    }

    #[test]
    fn lsr_reports_transmitter_always_ready() {
        let mut u = Uart::new(VecSink::default());
        assert_ne!(u.load(LSR) & LSR_THRE, 0, "guest polls this before every byte");
    }

    #[test]
    fn lsr_data_ready_reflects_pending_input() {
        let mut u = Uart::new(VecSink::default());
        assert_eq!(u.load(LSR) & LSR_DR, 0);
        u.push_input(b'x');
        assert_ne!(u.load(LSR) & LSR_DR, 0);
    }

    #[test]
    fn reading_rbr_consumes_input() {
        let mut u = Uart::new(VecSink::default());
        u.push_input(b'x');
        assert_eq!(u.load(RBR), b'x' as u64);
        assert_eq!(u.load(LSR) & LSR_DR, 0, "input must be consumed once");
    }

    /// Linux's 8250 driver sets LCR's DLAB bit, writes the baud divisor to
    /// offsets 0/1, then clears DLAB again — ordinary `set_termios` init, not
    /// an edge case. Without DLAB support, that sequence would write the
    /// divisor's low byte straight to THR (a garbage byte on the console,
    /// the one output this project exists to produce) and the high byte to
    /// IER.
    #[test]
    fn dlab_hides_thr_and_ier_behind_the_divisor_latch() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, 0x0F); // ordinary IER write while DLAB=0
        u.store(LCR, LCR_DLAB as u64); // set DLAB
        u.store(RBR, b'z' as u64); // offset 0 is now DLL, not THR
        u.store(IER, 0x00); // offset 1 is now DLM, not IER
        assert!(
            u.sink.bytes.is_empty(),
            "a divisor byte written while DLAB=1 must not reach the console"
        );
        u.store(LCR, 0); // clear DLAB
        assert_eq!(
            u.load(IER),
            0x0F,
            "IER must be unchanged by the DLAB-shadowed write to offset 1"
        );
    }

    /// DLAB shadows offsets 0 and 1 on the *read* side too, and only the
    /// write side was covered — so inverting the two load arms (returning
    /// RBR while DLAB=1 and DLL while DLAB=0) passed the entire suite.
    ///
    /// That inversion is not cosmetic. Linux's `serial8250_do_set_termios`
    /// reads the divisor back while DLAB is still set; with the arms
    /// inverted that read pops the input queue, so a keystroke that arrived
    /// during port setup is consumed as a baud-rate byte and never reaches
    /// the tty. Worse, the `dlab()` guard is the only thing separating "read
    /// the divisor" from "consume a character", and RBR is destructive.
    #[test]
    fn dlab_shadows_the_read_side_and_a_divisor_read_never_consumes_input() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, 0x0F); // ordinary IER write while DLAB=0
        u.store(LCR, LCR_DLAB as u64);
        u.store(RBR, 0xAB); // -> DLL
        u.store(IER, 0xCD); // -> DLM
        u.push_input(b'q'); // a byte arrives mid-configuration

        assert_eq!(u.load(RBR), 0xAB, "offset 0 must read DLL while DLAB=1, not RBR");
        assert_eq!(u.load(IER), 0xCD, "offset 1 must read DLM while DLAB=1, not IER");
        assert_ne!(u.load(LSR) & LSR_DR, 0, "the pending byte must still be pending");

        u.store(LCR, 0); // clear DLAB
        assert_eq!(u.load(RBR), b'q' as u64, "the input byte survived the divisor read-back");
        assert_eq!(u.load(IER), 0x0F, "IER was never touched by the DLAB-shadowed access");
    }

    /// With every source masked in IER, IIR bit 0 must read 1 ("nothing
    /// pending"). Returning 0 — this crate's default for unmodelled offsets
    /// — would tell a probing driver an interrupt is permanently,
    /// unservicably pending.
    #[test]
    fn iir_reports_no_interrupt_pending_while_every_source_is_masked() {
        let mut u = Uart::new(VecSink::default());
        assert_eq!(
            u.load(IIR) & 1,
            1,
            "bit 0 clear would signal a permanently pending, never-serviced interrupt"
        );
    }

    /// The regression that matters: with `irq = 0` Linux drives this port
    /// from a polling timer, and `serial8250_handle_irq()` gives up on
    /// `UART_IIR_NO_INT` before it can call `serial8250_tx_chars()`. So once
    /// the guest enables the TX-empty source, IIR must actually report it —
    /// otherwise userspace output stops after one FIFO load and the guest
    /// appears hung. See `IIR_NO_INTERRUPT`.
    #[test]
    fn iir_reports_thr_empty_once_the_guest_enables_that_source() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, IER_THR_EMPTY as u64);
        assert_eq!(
            u.load(IIR),
            IIR_THR_EMPTY,
            "THR is always empty in this model, so an enabled TX source is always pending"
        );
        assert_eq!(u.load(IIR) & 1, 0, "bit 0 must read 0 while a cause is pending");
    }

    /// An 8250 does not report a source the guest has masked off, and a
    /// driver that has finished transmitting clears IER.THRI precisely to
    /// stop being told about it.
    #[test]
    fn a_masked_tx_source_is_not_reported() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, IER_THR_EMPTY as u64);
        u.store(IER, 0);
        assert_eq!(u.load(IIR), IIR_NO_INTERRUPT);
    }

    /// The invariant `thr_empty` exists to hold: IIR and LSR must never
    /// disagree about whether there is room in the transmitter. They agree
    /// trivially today; this pins it so that teaching a sink to apply
    /// backpressure cannot silently break one of the two and wedge TX.
    #[test]
    fn iir_and_lsr_never_disagree_about_room_in_the_transmitter() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, IER_THR_EMPTY as u64);
        let lsr_has_room = u.load(LSR) & LSR_THRE != 0;
        let iir_has_room = u.load(IIR) == IIR_THR_EMPTY;
        assert_eq!(
            lsr_has_room, iir_has_room,
            "IIR must not invite a byte LSR says there is no room for, or vice versa"
        );
    }

    /// Received data outranks TX-empty in the 8250's fixed priority order,
    /// and only counts while a byte is actually waiting.
    #[test]
    fn pending_input_outranks_the_tx_source_and_only_while_it_is_queued() {
        let mut u = Uart::new(VecSink::default());
        u.store(IER, (IER_RX_AVAILABLE | IER_THR_EMPTY) as u64);
        assert_eq!(u.load(IIR), IIR_THR_EMPTY, "no input queued yet");

        u.push_input(b'x');
        assert_eq!(u.load(IIR), IIR_RX_AVAILABLE, "queued input takes priority");

        assert_eq!(u.load(RBR), b'x' as u64);
        assert_eq!(u.load(IIR), IIR_THR_EMPTY, "the RX cause clears once the byte is read");
    }
}
