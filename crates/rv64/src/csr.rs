extern crate alloc;
use alloc::boxed::Box;

use crate::exception::Exception;

pub const SSTATUS: u16 = 0x100;
pub const SIE: u16 = 0x104;
pub const STVEC: u16 = 0x105;
pub const SSCRATCH: u16 = 0x140;
pub const SEPC: u16 = 0x141;
pub const SCAUSE: u16 = 0x142;
pub const STVAL: u16 = 0x143;
pub const SIP: u16 = 0x144;
pub const SATP: u16 = 0x180;

pub const MSTATUS: u16 = 0x300;
pub const MISA: u16 = 0x301;
pub const MEDELEG: u16 = 0x302;
pub const MIDELEG: u16 = 0x303;
pub const MIE: u16 = 0x304;
pub const MTVEC: u16 = 0x305;
pub const MSCRATCH: u16 = 0x340;
pub const MEPC: u16 = 0x341;
pub const MCAUSE: u16 = 0x342;
pub const MTVAL: u16 = 0x343;
pub const MIP: u16 = 0x344;

pub const MHARTID: u16 = 0xF14;

/// The unprivileged counter/timer CSRs. These are *not* backed by the
/// register file: they are answered in the SYSTEM decoder
/// (`insn/rv64i.rs`), the one place that has both the CSR address and the
/// `bus` — and therefore the CLINT — in scope. `Csrs::read` would otherwise
/// hand back a permanently-zero array slot, which for `time` livelocks
/// Linux's timer (see that decoder arm for the full reasoning).
pub const CYCLE: u16 = 0xC00;
pub const TIME: u16 = 0xC01;
pub const INSTRET: u16 = 0xC02;

/// Sdtrig (debug trigger) registers. This machine implements no triggers,
/// so the whole block is read-only zero — see `Csrs::write`.
pub const TSELECT: u16 = 0x7A0;
pub const TCONTROL: u16 = 0x7A5;

/// mstatus.UXL (bits 33:32) and mstatus.SXL (35:34).
const MSTATUS_XLEN_FIELDS: u64 = 0xF_0000_0000;
/// Both fields set to 2, the encoding for XLEN=64.
const MSTATUS_XLEN_RV64: u64 = (2 << 32) | (2 << 34);

/// Bits of mstatus visible through sstatus: SIE, SPIE, SPP, plus the
/// memory-privilege and extension-state fields.
const SSTATUS_MASK: u64 = 0x8000_0003_000D_E162;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priv {
    U = 0,
    S = 1,
    M = 3,
}

/// The whole 12-bit CSR address space as a flat array.
///
/// The flatness is a deliberate, documented choice rather than an oversight:
/// a sparse map would ship the same behaviour, because what actually decides
/// whether a CSR is honest is *which addresses have an arm in
/// [`Csrs::write`]*, not how the backing store is shaped. Every field this
/// machine does not implement gets an explicit arm there (see `MTVEC`,
/// `SATP`, `MISA`, the Sdtrig block) precisely because the default —
/// accepting and returning any value at any address — tells probing software
/// that every capability exists.
///
/// It lives behind a `Box` for one reason, and it is not the 32 KiB itself:
/// built as a local and moved, `Csrs::default` and `Cpu::new` each
/// materialize the array in their own frame, and on `riscv32imac` (debug)
/// that measured 65,744 and 34,112 bytes respectively — a peak of ~98 KiB
/// of stack in one call chain, against the 128 KiB a Xous thread typically
/// gets. Release inlines it to about a kilobyte, but whether the emulator
/// fits on its target's stack must not be a property of the optimizer.
pub struct Csrs {
    regs: Box<[u64; 4096]>,
}

impl Default for Csrs {
    /// This emulator plays the M-mode firmware role directly (see
    /// `sbi.rs`) rather than executing an actual OpenSBI image, so nothing
    /// ever runs the M-mode boot code that would normally program
    /// `mideleg`/`medeleg` before jumping to the S-mode kernel. Left at
    /// their architectural reset value of 0, both failures are silent
    /// hangs rather than crashes: with `mideleg == 0`, a guest's write to
    /// `sie`/`sip` (masked by `mideleg` — see `read`/`write` below) is
    /// discarded entirely, so it can never actually enable the supervisor
    /// timer interrupt Task 14 forwards to it; with `medeleg == 0`, every
    /// exception the guest raises — including ordinary page faults —
    /// vectors through `mtvec` (0 for this guest) instead of `stvec`. Real
    /// firmware sets both during init, so this reset does too.
    fn default() -> Self {
        // Filled on the heap, never on the stack: `vec![0u64; N]` goes
        // straight to `alloc_zeroed`, so no 32 KiB temporary is ever
        // materialized in this frame. `[0u64; 4096]` boxed afterwards would
        // build it as a local first and defeat the point.
        let mut regs: Box<[u64; 4096]> = alloc::vec![0u64; 4096]
            .into_boxed_slice()
            .try_into()
            .expect("allocated exactly 4096 elements");
        // MISA: RV64 with I, M, A, C, S and U modes.
        let mxl = 2u64 << 62;
        let ext = (1 << 0) | (1 << 2) | (1 << 8) | (1 << 12) | (1 << 18) | (1 << 20);
        regs[MISA as usize] = mxl | ext;

        // Delegable interrupts: SSIP, STIP, SEIP (bits 1, 5, 9) — the only
        // three that are architecturally delegable. The machine-level bits
        // (MSIP/MTIP/MEIP, 3/7/11) are read-only zero in mideleg on real
        // hardware, which is also why `Cpu::check_interrupts` hard-codes
        // the timer's M-mode and S-mode targets directly instead of
        // consulting this register for the timer.
        const MIDELEG_SSIP: u64 = 1 << 1;
        const MIDELEG_STIP: u64 = 1 << 5;
        const MIDELEG_SEIP: u64 = 1 << 9;
        regs[MIDELEG as usize] = MIDELEG_SSIP | MIDELEG_STIP | MIDELEG_SEIP;

        // Delegable exceptions: the standard S-mode set, built from
        // `Exception::cause()` — the single source of truth for cause
        // numbers — rather than copied in as a bare hex literal. This is
        // exactly OpenSBI's usual delegation set: the misaligned/access
        // faults, the breakpoint, ECALL-from-U, and the three page faults.
        //
        // EnvironmentCallFromSMode (cause 9) is deliberately excluded: that
        // is the SBI call itself. `Cpu::step_trapping` already intercepts
        // it ahead of `trap()`, so `medeleg` does not currently reroute it
        // regardless — but delegating it here would be wrong the moment
        // that interception ever moved, so it is left undelegated on
        // purpose rather than by omission.
        // mstatus.SXL/UXL: this machine's S and U modes are RV64, and XLEN
        // is not configurable, so both fields are read-only 2. Their reset
        // value of 0 is a *reserved* encoding, not "unknown" — software
        // that reads sstatus to learn the user XLEN (as `rv64si-p-csr` and
        // `rv64mi-p-csr` do) is entitled to see 2 here.
        regs[MSTATUS as usize] = MSTATUS_XLEN_RV64;

        let medeleg = (1 << Exception::InstructionAddressMisaligned(0).cause())
            | (1 << Exception::InstructionAccessFault(0).cause())
            | (1 << Exception::Breakpoint.cause())
            | (1 << Exception::LoadAddressMisaligned(0).cause())
            | (1 << Exception::LoadAccessFault(0).cause())
            | (1 << Exception::StoreAddressMisaligned(0).cause())
            | (1 << Exception::StoreAccessFault(0).cause())
            | (1 << Exception::EnvironmentCallFromUMode.cause())
            | (1 << Exception::InstructionPageFault(0).cause())
            | (1 << Exception::LoadPageFault(0).cause())
            | (1 << Exception::StorePageFault(0).cause());
        regs[MEDELEG as usize] = medeleg;

        Self { regs }
    }
}

impl Csrs {
    pub fn read(&self, addr: u16) -> u64 {
        match addr {
            SSTATUS => self.regs[MSTATUS as usize] & SSTATUS_MASK,
            SIE => self.regs[MIE as usize] & self.regs[MIDELEG as usize],
            SIP => self.regs[MIP as usize] & self.regs[MIDELEG as usize],
            _ => self.regs[addr as usize],
        }
    }

    pub fn write(&mut self, addr: u16, v: u64) {
        match addr {
            // SXL/UXL are read-only (XLEN is fixed at 64), so they are
            // subtracted from the writable set here and re-imposed on the
            // full-mstatus path below.
            SSTATUS => {
                let m = self.regs[MSTATUS as usize];
                let w = SSTATUS_MASK & !MSTATUS_XLEN_FIELDS;
                self.regs[MSTATUS as usize] = (m & !w) | (v & w);
            }
            MSTATUS => {
                self.regs[MSTATUS as usize] = (v & !MSTATUS_XLEN_FIELDS) | MSTATUS_XLEN_RV64;
            }
            // xtvec MODE (bits 1:0) is WARL. Only Direct mode (0) is
            // implemented — `Cpu::enter_trap` always vectors to the base
            // address — so a write of Vectored (1) or a reserved mode must
            // read back as Direct rather than being stored verbatim.
            // Storing it would advertise a trap-delivery mode that never
            // happens, and software that probes for vectored mode by
            // writing it and reading it back then waits forever for an
            // interrupt that is delivered to the base address instead.
            MTVEC | STVEC => self.regs[addr as usize] = v & !3,
            // satp.MODE (bits 63:60) is WARL and this machine implements
            // only Bare (0) and Sv39 (8) — `Mmu::translate` treats every
            // other value as Bare. Same shape as the xtvec clamp above, and
            // for the same reason one level up: a register file that stores
            // MODE=9/10 verbatim advertises a paging mode that never
            // happens. Linux's `set_satp_mode()` probes exactly that way —
            // it writes Sv57, reads satp back, and *believes the readback*.
            // Because that code runs identity-mapped, "translation silently
            // off" is indistinguishable from "translation works", so the
            // probe succeeds, five-level page tables get built, and the
            // first jump to a kernel virtual address lands outside RAM. The
            // guest dies before printing a character.
            //
            // The spec is specific about the remedy for satp (unlike xtvec,
            // where any legal MODE may be substituted): "if satp is written
            // with an unsupported MODE, the entire write has no effect; no
            // fields in satp are modified". So an unsupported MODE is
            // discarded outright rather than coerced to Bare, and the probe
            // reads back the previous value — which is what makes Linux fall
            // through to Sv48 and then to Sv39, the mode `guest.dts`
            // already advertises.
            //
            // ASID (bits 59:44) is likewise forced to zero: no ASIDs are
            // implemented. `asids_init()` sizes that field by writing all
            // ones and counting what sticks, and a flat register file
            // answers "16 bits" — after which Linux stops flushing the TLB
            // on context switch. (Today the whole TLB is flushed on every
            // satp write by the SYSTEM decoder, which is why that lie has
            // been harmless so far; this makes the register honest so the
            // flush is no longer load-bearing for correctness.)
            SATP => {
                let mode = v >> 60;
                if mode == 0 || mode == 8 {
                    self.regs[SATP as usize] = v & !(0xFFFFu64 << 44);
                }
            }
            // Sdtrig: no debug triggers are implemented, so this block is
            // read-only zero. It must not act as scratch storage — the
            // standard probe for "does this machine support trigger type
            // X?" is to write X to `tdata1` and read it back, and a machine
            // that echoes the write claims every trigger type exists and
            // then never fires one.
            TSELECT..=TCONTROL => {}
            SIE => {
                let d = self.regs[MIDELEG as usize];
                let m = self.regs[MIE as usize];
                self.regs[MIE as usize] = (m & !d) | (v & d);
            }
            SIP => {
                let d = self.regs[MIDELEG as usize];
                let m = self.regs[MIP as usize];
                self.regs[MIP as usize] = (m & !d) | (v & d);
            }
            // MISA is WARL, not read-only: csr_addr[11:10] for 0x301 is
            // 0b00, so a guest write to it is architecturally legal and the
            // SYSTEM decoder lets it through. Our ISA is fixed, so the
            // write has no effect.
            //
            // MHARTID (0xF14) has csr_addr[11:10] == 0b11, so the SYSTEM
            // decoder now rejects any guest-instruction write to it as
            // IllegalInstruction before this method is ever called with
            // that address — see insn/rv64i.rs. This layer therefore no
            // longer needs its own guard for MHARTID; leaving one here
            // would only block a legitimate direct write from
            // emulator-internal code (e.g. a future multi-hart `Cpu::new`
            // setting the hart ID at reset), which is not a guest access
            // and must not be treated as read-only.
            MISA => {}
            _ => self.regs[addr as usize] = v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mtvec`/`stvec` MODE (bits 1:0) is WARL, and this implementation
    /// only ever vectors to the base address (`Cpu::enter_trap`). A machine
    /// that accepts a MODE it does not implement tells probing software the
    /// feature is there and then never delivers a vectored trap — which is
    /// exactly how `rv64mi-p-illegal` used to hang forever.
    #[test]
    fn tvec_mode_is_warl_clamped_to_direct() {
        let mut c = Csrs::default();
        c.write(MTVEC, 0x8000_1234 | 1);
        assert_eq!(c.read(MTVEC), 0x8000_1234);
        c.write(STVEC, 0x8000_5670 | 3);
        assert_eq!(c.read(STVEC), 0x8000_5670);
    }

    /// mstatus.SXL/UXL must read 2 (XLEN=64) on a machine whose S and U
    /// modes are RV64. Reset leaves them 0, which is a reserved encoding.
    #[test]
    fn mstatus_reports_xlen_64_for_s_and_u_mode() {
        let c = Csrs::default();
        assert_eq!((c.read(MSTATUS) >> 32) & 3, 2, "UXL");
        assert_eq!((c.read(MSTATUS) >> 34) & 3, 2, "SXL");
        assert_eq!((c.read(SSTATUS) >> 32) & 3, 2, "UXL through sstatus");
    }

    /// The XLEN fields are read-only when XLEN is fixed, so neither a full
    /// mstatus write nor an sstatus write may clear them.
    #[test]
    fn mstatus_xlen_fields_are_read_only() {
        let mut c = Csrs::default();
        c.write(MSTATUS, 0);
        assert_eq!(c.read(MSTATUS) & MSTATUS_XLEN_FIELDS, MSTATUS_XLEN_RV64);
        c.write(MSTATUS, u64::MAX);
        assert_eq!(c.read(MSTATUS) & MSTATUS_XLEN_FIELDS, MSTATUS_XLEN_RV64);
        c.write(SSTATUS, 0);
        assert_eq!(c.read(MSTATUS) & MSTATUS_XLEN_FIELDS, MSTATUS_XLEN_RV64);
    }

    /// This emulator implements no debug triggers, so the Sdtrig CSRs are
    /// read-only zero. They must not behave like scratch registers: the
    /// standard way software probes for a trigger type is to write it to
    /// `tdata1` and read it back, and a flat register file answers "yes" to
    /// every such probe.
    #[test]
    fn debug_trigger_csrs_read_as_zero_after_a_write() {
        let mut c = Csrs::default();
        for addr in TSELECT..=TCONTROL {
            c.write(addr, u64::MAX);
            assert_eq!(c.read(addr), 0, "CSR {addr:#x} must be read-only zero");
        }
    }

    /// `satp.MODE` is WARL and this machine implements only Bare (0) and
    /// Sv39 (8). Linux's `set_satp_mode()` probes for Sv57/Sv48 by writing
    /// the mode and reading it back (`csr_swap`), and *believes the
    /// readback*: a register file that stores MODE=10 verbatim tells the
    /// kernel five-level paging exists, at which point it builds five-level
    /// tables, jumps to a kernel virtual address, and `Mmu::translate` —
    /// which treats every MODE except 8 as Bare — hands back the untranslated
    /// address. The guest dies before printing a character.
    #[test]
    fn satp_mode_is_warl_and_an_unsupported_mode_write_is_discarded() {
        let mut c = Csrs::default();

        // Sv39 and Bare are implemented, so both must take effect.
        c.write(SATP, (8u64 << 60) | 0x8_0123);
        assert_eq!(c.read(SATP), (8u64 << 60) | 0x8_0123);

        // Sv48 (9), Sv57 (10) and the reserved encodings are not. Per the
        // privileged spec, such a write has *no effect at all* — no field of
        // satp is modified — so the previous, supported value survives and
        // the probe reads back something other than what it wrote.
        for mode in [1u64, 9, 10, 15] {
            c.write(SATP, (mode << 60) | 0x9_9999);
            assert_eq!(
                c.read(SATP),
                (8u64 << 60) | 0x8_0123,
                "a satp write with MODE={mode} must not modify any field"
            );
        }

        c.write(SATP, 0);
        assert_eq!(c.read(SATP), 0, "Bare is implemented and must take effect");
    }

    /// `satp.ASID` reads back zero because this machine implements no ASIDs.
    /// Linux's `asids_init()` sizes the ASID field by writing all ones and
    /// counting the bits that stick; a flat register file answers "16 bits",
    /// and Linux then stops flushing the TLB on context switch because it
    /// believes each address space has its own tag.
    #[test]
    fn satp_asid_reads_back_zero_because_no_asids_are_implemented() {
        let mut c = Csrs::default();
        c.write(SATP, (8u64 << 60) | (0xFFFF << 44) | 0x1234);
        assert_eq!((c.read(SATP) >> 44) & 0xFFFF, 0, "ASID is not implemented");
        assert_eq!(c.read(SATP) & 0xFFF_FFFF_FFFF, 0x1234, "PPN must still be written");
    }

    /// `Csrs` must stay a handle, not an inline 32 KiB array. Built as a
    /// local and moved, the array is materialized once in `Csrs::default`'s
    /// frame and again in `Cpu::new`'s — measured at 65,744 and 34,112 bytes
    /// on `riscv32imac` in debug, a ~98 KiB peak against the 128 KiB a Xous
    /// thread typically gets. Release optimizes it away, which is exactly
    /// why this needs a test: the fit must not depend on the optimizer.
    #[test]
    fn csrs_is_a_handle_so_the_register_file_never_lands_on_the_stack() {
        assert!(
            core::mem::size_of::<Csrs>() <= 16,
            "Csrs is {} bytes; the register file must stay behind a pointer",
            core::mem::size_of::<Csrs>()
        );
    }

    /// Guards the rest of the register file: an ordinary read/write CSR
    /// still round-trips, so the special cases above are special cases.
    #[test]
    fn an_ordinary_csr_still_round_trips() {
        let mut c = Csrs::default();
        c.write(MSCRATCH, 0xDEAD_BEEF);
        assert_eq!(c.read(MSCRATCH), 0xDEAD_BEEF);
    }
}
