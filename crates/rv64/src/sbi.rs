//! SBI v0.2 stub. The emulator itself plays the M-mode firmware role that
//! OpenSBI would occupy on real hardware — there is no separate firmware
//! image and no M-mode trap round trip for an `ecall`; `Cpu::step_trapping`
//! intercepts `EnvironmentCallFromSMode` directly and calls [`handle`] from
//! host code, then resumes the guest past the `ecall`.
//!
//! Only what a Linux/riscv64 guest needs to boot to a shell is implemented:
//! the legacy (v0.1) timer/console/shutdown calls Linux still issues under
//! `CONFIG_RISCV_SBI_V01=y` and `earlycon=sbi`, plus the v0.2 base extension
//! a modern kernel probes before falling back to those.

use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::csr;
use crate::uart::ConsoleSink;

// Legacy (v0.1) extension IDs, still used by Linux for console and timer.
pub const EXT_SET_TIMER: u64 = 0x00;
pub const EXT_CONSOLE_PUTCHAR: u64 = 0x01;
pub const EXT_CONSOLE_GETCHAR: u64 = 0x02;
pub const EXT_SHUTDOWN: u64 = 0x08;

// v0.2 extensions.
pub const EXT_BASE: u64 = 0x10;
pub const EXT_TIMER_V02: u64 = 0x54494D45; // "TIME"
pub const EXT_SRST: u64 = 0x53525354;      // "SRST"

pub const SBI_SUCCESS: i64 = 0;
pub const SBI_ERR_NOT_SUPPORTED: i64 = -2;

/// `mip` bit 5: the supervisor timer interrupt-pending bit that a Linux
/// guest actually takes a trap on. See `set_timer` and
/// `Cpu::check_interrupts` for how it is raised and cleared.
const MIP_STIP: u64 = 1 << 5;

/// Whether the run loop should keep going. `Shutdown` (from the `SHUTDOWN`
/// and `SRST` extensions) is the only signal this crate gives that the
/// guest wants to stop; a caller that lets this value fall on the floor
/// silently ignores every shutdown request, so it is `#[must_use]` —
/// Task 17's run loop is the intended consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SbiOutcome {
    Handled,
    Shutdown,
}

/// Services an `ecall` from S-mode. Arguments are in a0..a5, extension in
/// a7, function in a6.
///
/// The legacy (v0.1) extensions and the v0.2 extensions use different
/// return conventions on real hardware, and this stub honors both rather
/// than flattening them to one: v0.2 calls return a pair — error in a0,
/// value in a1 — but the legacy extensions (SET_TIMER, CONSOLE_PUTCHAR,
/// CONSOLE_GETCHAR, SHUTDOWN) return a single value in a0 with no error
/// field at all. Getting this wrong is observable: CONSOLE_GETCHAR's "no
/// character available" result is -1 in a0 under the legacy convention, and
/// this guest's kernel enables `CONFIG_RISCV_SBI_V01` and boots with
/// `earlycon=sbi`, so legacy calls are genuinely exercised, not vestigial.
pub fn handle<B: MemBacking, S: ConsoleSink>(cpu: &mut Cpu, bus: &mut Bus<B, S>) -> SbiOutcome {
    let ext = cpu.reg(17);
    let func = cpu.reg(16);
    let a0 = cpu.reg(10);

    // Legacy (v0.1): a single return value in a0, no a1.
    match ext {
        EXT_SET_TIMER => {
            set_timer(cpu, bus, a0);
            cpu.set_reg(10, 0);
            return SbiOutcome::Handled;
        }
        EXT_CONSOLE_PUTCHAR => {
            bus.uart.sink.put(a0 as u8);
            cpu.set_reg(10, 0);
            return SbiOutcome::Handled;
        }
        EXT_CONSOLE_GETCHAR => {
            // No input source is modeled for the legacy console path (input
            // arrives through the 8250, not SBI) — always report "nothing
            // available", which per the v0.1 convention is -1 in a0 alone.
            cpu.set_reg(10, u64::MAX);
            return SbiOutcome::Handled;
        }
        EXT_SHUTDOWN => return SbiOutcome::Shutdown,
        _ => {}
    }

    // v0.2: error in a0, value in a1.
    let (err, val): (i64, u64) = match ext {
        EXT_TIMER_V02 if func == 0 => {
            set_timer(cpu, bus, a0);
            (SBI_SUCCESS, 0)
        }
        EXT_SRST if func == 0 => return SbiOutcome::Shutdown,
        EXT_BASE => match func {
            0 => (SBI_SUCCESS, 0x0000_0002), // spec version 0.2
            1 => (SBI_SUCCESS, 1),           // impl id
            2 => (SBI_SUCCESS, 1),           // impl version
            3 => {
                let supported = matches!(
                    a0,
                    EXT_SET_TIMER | EXT_CONSOLE_PUTCHAR | EXT_CONSOLE_GETCHAR
                        | EXT_SHUTDOWN | EXT_BASE | EXT_TIMER_V02 | EXT_SRST
                );
                (SBI_SUCCESS, supported as u64)
            }
            4 => (SBI_SUCCESS, 0), // mvendorid
            5 => (SBI_SUCCESS, 0), // marchid
            6 => (SBI_SUCCESS, 0), // mimpid
            _ => (SBI_ERR_NOT_SUPPORTED, 0),
        },
        _ => (SBI_ERR_NOT_SUPPORTED, 0),
    };

    cpu.set_reg(10, err as u64);
    cpu.set_reg(11, val);
    SbiOutcome::Handled
}

/// Schedules the next timer event and acknowledges the current one.
///
/// Per the SBI spec, `set_timer` does two things: program `mtimecmp` for
/// the next deadline, and clear the *currently pending* supervisor timer
/// interrupt — that is how the guest acknowledges a tick. Real OpenSBI does
/// this by clearing `mip.STIP` directly in its `set_timer` handler rather
/// than waiting for the next poll of the CLINT to notice `mtimecmp` moved;
/// this mirrors that. Without the explicit clear here, `Cpu::check_interrupts`
/// would only stop re-raising STIP once it next observes
/// `mtime < mtimecmp`, which happens to align in the common case (the guest
/// always reprograms `mtimecmp` to a future deadline) but is not the
/// property the SBI spec actually guarantees.
fn set_timer<B: MemBacking, S: ConsoleSink>(cpu: &mut Cpu, bus: &mut Bus<B, S>, deadline: u64) {
    bus.clint.mtimecmp = deadline;
    let mip = cpu.csrs.read(csr::MIP);
    cpu.csrs.write(csr::MIP, mip & !MIP_STIP);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backing::FakeBacking;
    use crate::cache::PageCache;
    use crate::uart::VecSink;
    use crate::{Bus, Cpu, RAM_BASE};

    fn setup() -> (Cpu, Bus<FakeBacking, VecSink>) {
        let bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        (Cpu::new(RAM_BASE), bus)
    }

    #[test]
    fn console_putchar_reaches_the_sink() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, EXT_CONSOLE_PUTCHAR); // a7
        cpu.set_reg(10, b'Z' as u64);          // a0
        assert_eq!(handle(&mut cpu, &mut bus), SbiOutcome::Handled);
        assert_eq!(bus.uart.sink.bytes, b"Z");
    }

    #[test]
    fn set_timer_programs_mtimecmp() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, EXT_SET_TIMER);
        cpu.set_reg(10, 12345);
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(bus.clint.mtimecmp, 12345);
    }

    #[test]
    fn shutdown_is_reported() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, EXT_SHUTDOWN);
        assert_eq!(handle(&mut cpu, &mut bus), SbiOutcome::Shutdown);
    }

    #[test]
    fn base_probe_reports_supported_extensions() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, EXT_BASE);
        cpu.set_reg(16, 3);                 // a6 = probe_extension
        cpu.set_reg(10, EXT_TIMER_V02);     // a0 = extension id
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(cpu.reg(10), 0, "error field must be SBI_SUCCESS");
        assert_eq!(cpu.reg(11), 1, "value field must report available");
    }

    #[test]
    fn unknown_extension_returns_not_supported() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, 0xDEAD_BEEF);
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(cpu.reg(10) as i64, -2, "SBI_ERR_NOT_SUPPORTED");
    }

    /// Judgment call: legacy (v0.1) extensions return a single value in a0,
    /// not the v0.2 (error, value) pair. CONSOLE_GETCHAR is where getting
    /// this wrong is observable — under the v0.2 convention "no character"
    /// would be (SBI_SUCCESS, u64::MAX), i.e. a0 = 0, which a legacy caller
    /// reads as character 0 rather than the -1 that means "nothing
    /// available". a1 must be untouched (still its reset value of 0),
    /// confirming this went through the single-value path, not the pair one.
    #[test]
    fn console_getchar_uses_the_legacy_single_value_convention() {
        let (mut cpu, mut bus) = setup();
        cpu.set_reg(17, EXT_CONSOLE_GETCHAR);
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(cpu.reg(10) as i64, -1, "a0 alone must carry -1, the legacy 'no input' value");
        assert_eq!(cpu.reg(11), 0, "a1 must be untouched by a legacy call");
    }

    /// Defect C, end to end: a Linux/S-mode guest never sets up an M-mode
    /// trap handler, so a machine timer interrupt (Task 12's original
    /// behavior) would vector to whatever mtvec holds — 0 for this guest —
    /// and hang. `sbi_set_timer` must instead leave the guest able to take
    /// an ordinary *supervisor* timer interrupt through stvec: this
    /// programs a deadline exactly as a guest would, ticks the CLINT past
    /// it, and confirms `check_interrupts` vectors to stvec with cause 5
    /// and mip.STIP set. A second `set_timer` (the guest's acknowledgment
    /// of the tick, per the SBI spec) must then clear mip.STIP — without
    /// that clear the interrupt refires forever and the guest never makes
    /// forward progress.
    ///
    /// The enable is written through `SIE` (CSR 0x104), exactly as Linux's
    /// riscv timer driver does (`csr_set(CSR_IE, IE_TIE)`) — not through the
    /// M-mode-only raw `MIE` (0x304), which no S-mode guest can ever touch.
    /// `SIE` writes are masked by `mideleg`, so this only works once
    /// `Cpu::new` has delegated STIP (fix round 2, "Finding 1"); against the
    /// unfixed CPU this test is RED, which is the point — a test that
    /// enables the timer a way no real guest can is a test that cannot
    /// catch this class of bug.
    #[test]
    fn set_timer_forwards_the_clint_to_a_supervisor_timer_interrupt() {
        use crate::csr::{self, Priv};
        use crate::Interrupt;

        let (mut cpu, mut bus) = setup();
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::STVEC, RAM_BASE + 0x800);
        cpu.csrs.write(csr::SIE, 1 << 5); // STIE, written the way Linux writes it
        let mstatus = cpu.csrs.read(csr::MSTATUS) | (1 << 1); // SIE
        cpu.csrs.write(csr::MSTATUS, mstatus);

        // The guest programs a deadline via SBI, exactly like Linux's
        // riscv timer driver does.
        cpu.set_reg(17, EXT_SET_TIMER);
        cpu.set_reg(10, 100);
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(bus.clint.mtimecmp, 100);

        bus.clint.tick(100); // mtime now >= mtimecmp: the CLINT fires.
        cpu.check_interrupts(&mut bus);

        assert_eq!(cpu.priv_, Priv::S, "must stay in S-mode, not vector through M");
        assert_eq!(cpu.pc, RAM_BASE + 0x800, "must vector to stvec");
        assert_eq!(
            cpu.csrs.read(csr::SCAUSE),
            Interrupt::SupervisorTimer.cause(),
            "scause must be 5 | (1 << 63)"
        );
        assert_ne!(cpu.csrs.read(csr::MIP) & (1 << 5), 0, "mip.STIP must be set");
        assert_ne!(
            cpu.csrs.read(csr::SIP) & (1 << 5),
            0,
            "the guest must be able to observe the pending timer through sip, \
             which is masked by mideleg exactly like sie"
        );

        // The guest's handler acknowledges the tick with another set_timer.
        cpu.set_reg(17, EXT_SET_TIMER);
        cpu.set_reg(10, 999_999);
        let _ = handle(&mut cpu, &mut bus);
        assert_eq!(
            cpu.csrs.read(csr::MIP) & (1 << 5),
            0,
            "a second set_timer must clear mip.STIP"
        );
    }
}
