use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::csr::{self, Csrs, Priv};
use crate::exception::{Exception, Interrupt};
use crate::insn;
use crate::mmu::{Access, Mmu};
use crate::uart::ConsoleSink;

pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    /// Reservation set by LR and cleared by SC/AMO/plain stores. Always a
    /// *physical* (translated) address — see `invalidate_reservation`.
    pub reservation: Option<u64>,
    /// Length in bytes of the instruction currently being executed — 2 for a
    /// compressed instruction, 4 otherwise. Set before dispatch so JAL/JALR
    /// can link `pc + insn_len` rather than assuming a fixed width.
    pub insn_len: u64,
    /// Instructions retired since reset. Backs the `cycle` and `instret`
    /// counter CSRs (`insn/rv64i.rs`), which are read-only to the guest and
    /// so have no other way to advance. This machine executes exactly one
    /// instruction per cycle, so the two report the same number — a
    /// distinction only a pipelined implementation could make.
    pub instret: u64,
    pub csrs: Csrs,
    pub priv_: Priv,
    pub mmu: Mmu,
    next_pc: Option<u64>,
}

impl Cpu {
    pub fn new(pc: u64) -> Self {
        Self {
            regs: [0; 32],
            pc,
            reservation: None,
            insn_len: 4,
            instret: 0,
            csrs: Csrs::default(),
            priv_: Priv::M,
            mmu: Mmu::default(),
            next_pc: None,
        }
    }

    #[inline]
    pub fn reg(&self, i: usize) -> u64 {
        self.regs[i]
    }

    /// Writes to x0 are discarded — it is hardwired to zero.
    #[inline]
    pub fn set_reg(&mut self, i: usize, v: u64) {
        if i != 0 {
            self.regs[i] = v;
        }
    }

    /// Reads `size` (1..=8) bytes starting at `vaddr`, translating with
    /// `access`. `Bus::load` bounds-checks and reads `size` *physically
    /// contiguous* bytes — true by construction before this task, when
    /// virtual and physical addresses were identical, but not any more. An
    /// access whose bytes straddle a 4 KiB virtual page boundary can land
    /// its two halves on two unrelated physical pages, so `Bus::load`
    /// cannot be asked for the whole thing in one call; each half must be
    /// translated (and permission-checked) independently.
    ///
    /// The common case (no straddle) costs exactly one `translate` call.
    /// The straddling case is deliberately the "obviously correct" byte-at-
    /// a-time form rather than a two-chunk split, since `Bus::load` also
    /// only accepts sizes in {1,2,4,8} and a straddle can produce an
    /// arbitrary chunk split (e.g. 3 + 5 bytes) that isn't one of those.
    ///
    /// A fault raised while translating one of the straddling bytes is
    /// reported against *that byte's* address, not `vaddr` (the start of
    /// the whole access) — per the privileged spec, `stval`/`mtval` holds
    /// the address of the portion of a misaligned access that actually
    /// faulted, not the access's nominal start. Reporting `vaddr` instead
    /// would make the faulting address already-mapped from the kernel's
    /// point of view, so a page-fault handler using it to resolve the fault
    /// would resolve nothing, return, and fault identically forever.
    fn read_translated<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        vaddr: u64,
        size: u8,
        access: Access,
    ) -> Result<u64, Exception> {
        if (vaddr & 0xFFF) + size as u64 <= crate::PAGE as u64 {
            let pa = self.mmu.translate(bus, &self.csrs, self.priv_, vaddr, access)?;
            return bus.load(pa, size);
        }
        let mut buf = [0u8; 8];
        for i in 0..size as u64 {
            let a = vaddr.wrapping_add(i);
            let pa = self.mmu.translate(bus, &self.csrs, self.priv_, a, access)?;
            buf[i as usize] = bus.load(pa, 1)? as u8;
        }
        Ok(u64::from_le_bytes(buf))
    }

    /// Mirror of `read_translated` for stores. See that method's doc for why
    /// a straddling access can't be issued as a single `Bus::store` call,
    /// and for why a fault is reported against the faulting byte's own
    /// address rather than `vaddr`.
    fn write_translated<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        vaddr: u64,
        size: u8,
        v: u64,
    ) -> Result<(), Exception> {
        if (vaddr & 0xFFF) + size as u64 <= crate::PAGE as u64 {
            let pa = self.mmu.translate(bus, &self.csrs, self.priv_, vaddr, Access::Store)?;
            bus.store(pa, size, v)?;
            self.invalidate_reservation(pa, size);
            return Ok(());
        }
        let bytes = v.to_le_bytes();
        for i in 0..size as u64 {
            let a = vaddr.wrapping_add(i);
            let pa = self.mmu.translate(bus, &self.csrs, self.priv_, a, Access::Store)?;
            bus.store(pa, 1, bytes[i as usize] as u64)?;
            self.invalidate_reservation(pa, 1);
        }
        Ok(())
    }

    /// Reads a fetch of `size` bytes at `pc` (via `read_translated`),
    /// remapping any non-`BackingFailure`, non-page-fault error (e.g. the
    /// translated physical address landing outside RAM) to
    /// `InstructionAccessFault(pc)` — mirroring the pre-MMU behavior where a
    /// raw `bus.load` failure at this stage was reported against the
    /// instruction's own address rather than whatever `Bus::load` embedded.
    fn fetch<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        size: u8,
    ) -> Result<u64, Exception> {
        match self.read_translated(bus, self.pc, size, Access::Fetch) {
            Ok(w) => Ok(w),
            Err(e @ Exception::BackingFailure(_)) => Err(e),
            Err(e @ Exception::InstructionPageFault(_)) => Err(e),
            Err(_) => Err(Exception::InstructionAccessFault(self.pc)),
        }
    }

    /// Translates `vaddr` for a load and reads `size` bytes from it.
    pub fn vload<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        vaddr: u64,
        size: u8,
    ) -> Result<u64, Exception> {
        self.read_translated(bus, vaddr, size, Access::Load)
    }

    /// Translates `vaddr` for a store and writes `size` bytes to it. Any
    /// store — atomic or not — invalidates a reservation over the same
    /// granule, so this is the single point plain STORE goes through and
    /// also invalidates on the translated (physical) address.
    pub fn vstore<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        vaddr: u64,
        size: u8,
        v: u64,
    ) -> Result<(), Exception> {
        self.write_translated(bus, vaddr, size, v)
    }

    /// Redirects control flow. The pending target overrides the default
    /// pc + insn_len at the end of `step`.
    #[inline]
    pub fn jump(&mut self, target: u64) {
        self.next_pc = Some(target);
    }

    /// An ordinary store overlapping the reserved granule breaks the reservation,
    /// so a later SC must fail. Real hardware tracks this per cache line; an
    /// 8-byte granule is the conservative equivalent here.
    pub fn invalidate_reservation(&mut self, addr: u64, size: u8) {
        if let Some(r) = self.reservation {
            let store_end = addr.saturating_add(size as u64);
            let granule_end = r.saturating_add(8);
            if addr < granule_end && r < store_end {
                self.reservation = None;
            }
        }
    }

    pub fn step<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
    ) -> Result<(), Exception> {
        // A pc ending in an odd multiple of 2 (…FFE) never straddles for
        // this 2-byte probe (its last byte is at offset 0xFFF, still inside
        // the page), so this always costs exactly one `translate` call.
        let half = self.fetch(bus, 2)? as u16;

        let (insn, len) = if half & 0x3 == 0x3 {
            // Unlike the probe above, this CAN straddle a page boundary:
            // rv64c allows an uncompressed (4-byte) instruction to start at
            // any 2-byte-aligned pc, including …FFE. `fetch` (via
            // `read_translated`) handles that case correctly; the common,
            // non-straddling case still costs only one more `translate`
            // call, which hits the TLB entry the probe above just warmed —
            // not a second page walk.
            (self.fetch(bus, 4)? as u32, 4u64)
        } else {
            let expanded = insn::rvc::expand(half)
                .ok_or(Exception::IllegalInstruction(half as u64))?;
            (expanded, 2u64)
        };

        self.insn_len = len;
        self.next_pc = None;
        let handled = insn::rv64m::execute(self, bus, insn)?
            || insn::rv64a::execute(self, bus, insn)?
            || insn::rv64i::execute(self, bus, insn)?;
        if !handled {
            return Err(Exception::IllegalInstruction(insn as u64));
        }

        self.pc = self.next_pc.take().unwrap_or(self.pc.wrapping_add(len));
        // Retired, not attempted: every `?` above returns before this point,
        // so an instruction that faults does not advance the counter — which
        // is what `instret` means architecturally.
        self.instret = self.instret.wrapping_add(1);
        Ok(())
    }

    /// Steps once, delivering any resulting exception as a trap rather than
    /// propagating it.
    ///
    /// A `BackingFailure` is not a RISC-V exception — the emulator's own
    /// storage medium failed, and there is nothing to tell the guest about —
    /// so it is returned as `Err(addr)` for the caller to handle rather than
    /// delivered as a trap. It is deliberately *not* a panic: this crate is
    /// destined for a Xous app where a panic aborts the host process with no
    /// recovery, and a bad backing file should produce a diagnostic and a
    /// non-zero exit, not a Rust backtrace. This is the only place a guest
    /// could otherwise force one.
    ///
    /// The `BackingFailure` arm must stay ahead of the catch-all `trap(e)`
    /// arm below it. `Exception::BackingFailure(_).cause()` returns 5, which
    /// aliases `LoadAccessFault` — a cause `Csrs::default`'s `medeleg`
    /// delegates to S-mode — so a backing failure that reached `trap` would
    /// be silently handed to the guest as a spurious load access fault and
    /// the run would continue against memory that no longer works.
    ///
    /// `EnvironmentCallFromSMode` is routed to the SBI stub (`sbi::handle`)
    /// instead of being trapped: this emulator plays the M-mode firmware
    /// role directly in host code rather than hosting OpenSBI, so an
    /// `ecall` from the S-mode guest is serviced right here and the guest
    /// resumes at `pc + insn_len` — never a literal `4`, per the rule
    /// carried from Task 9 that nothing may assume a fixed instruction
    /// width (`ecall` has no compressed form, so `4` happens to be correct
    /// today, which is exactly why it's worth spelling out rather than
    /// hard-coding).
    pub fn step_trapping<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
    ) -> Result<crate::sbi::SbiOutcome, u64> {
        match self.step(bus) {
            Ok(()) => Ok(crate::sbi::SbiOutcome::Handled),
            Err(Exception::EnvironmentCallFromSMode) => {
                let outcome = crate::sbi::handle(self, bus);
                self.pc = self.pc.wrapping_add(self.insn_len);
                Ok(outcome)
            }
            Err(Exception::BackingFailure(addr)) => Err(addr),
            Err(e) => {
                self.trap(e);
                Ok(crate::sbi::SbiOutcome::Handled)
            }
        }
    }

    /// Enters a trap, delegating to S-mode when medeleg says so.
    pub fn trap(&mut self, e: Exception) {
        let cause = e.cause();
        let deleg = self.csrs.read(csr::MEDELEG);
        let to_s = self.priv_ != Priv::M && (deleg >> cause) & 1 == 1;
        self.enter_trap(cause, e.tval(), to_s);
    }

    /// The trap-entry sequence shared by exceptions (`trap`) and interrupts
    /// (`check_interrupts`): writes xEPC/xCAUSE/xTVAL, moves xIE into xPIE
    /// and clears xIE, records the pre-trap privilege into xPP, switches
    /// `priv_`, and vectors to xTVEC. `cause` must already carry the
    /// interrupt bit (bit 63) when delivering an interrupt.
    ///
    /// This must not be duplicated: an interrupt differs from an exception
    /// only in the cause value, the delegation register consulted, and
    /// `tval` being 0 — not in how the trap is entered.
    ///
    /// xTVEC is used as-is, with no MODE mask: `Csrs::write` WARL-clamps
    /// the MODE field to Direct on the way in, which is the single place
    /// that decision is made. Masking again here would let the register
    /// hold a mode this function silently ignores — the exact bug that made
    /// `rv64mi-p-illegal` spin forever waiting for a vectored interrupt.
    fn enter_trap(&mut self, cause: u64, tval: u64, to_s: bool) {
        if to_s {
            self.csrs.write(csr::SEPC, self.pc);
            self.csrs.write(csr::SCAUSE, cause);
            self.csrs.write(csr::STVAL, tval);
            let status = self.csrs.read(csr::MSTATUS);
            let sie = (status >> 1) & 1;
            let spp = (self.priv_ as u64) & 1;
            let new = (status & !((1 << 1) | (1 << 5) | (1 << 8))) | (sie << 5) | (spp << 8);
            self.csrs.write(csr::MSTATUS, new);
            self.priv_ = Priv::S;
            self.pc = self.csrs.read(csr::STVEC);
        } else {
            self.csrs.write(csr::MEPC, self.pc);
            self.csrs.write(csr::MCAUSE, cause);
            self.csrs.write(csr::MTVAL, tval);
            let status = self.csrs.read(csr::MSTATUS);
            let mie = (status >> 3) & 1;
            let mpp = self.priv_ as u64;
            let new = (status & !((1 << 3) | (1 << 7) | (0b11 << 11))) | (mie << 7) | (mpp << 11);
            self.csrs.write(csr::MSTATUS, new);
            self.priv_ = Priv::M;
            self.pc = self.csrs.read(csr::MTVEC);
        }
    }

    /// True if an interrupt/trap targeting `to_s` (S-mode) rather than
    /// M-mode would be taken from the current privilege: taken whenever
    /// `priv_` is strictly below the target level, taken at the target
    /// level only if that level's global interrupt-enable bit is set, and
    /// never taken above the target level.
    fn interrupt_enabled(&self, to_s: bool) -> bool {
        let status = self.csrs.read(csr::MSTATUS);
        if to_s {
            match self.priv_ {
                Priv::M => false,
                Priv::S => (status >> 1) & 1 == 1, // SIE
                Priv::U => true,
            }
        } else {
            self.priv_ != Priv::M || (status >> 3) & 1 == 1 // MIE
        }
    }

    /// Injects a pending timer interrupt if enabled. Called once per
    /// instruction from the run loop, *before* `step` — never from inside
    /// it. `step` clears its private `next_pc` before dispatch and applies
    /// it at the end via `next_pc.take()`, so a `pc` written here mid-`step`
    /// would be silently overwritten by that instruction's own fallthrough
    /// pc.
    ///
    /// `mideleg` bit 7 (MTIP) is read-only zero on real hardware: a machine
    /// timer interrupt can never be hardware-delegated to S-mode. Real
    /// firmware instead gives an S-mode guest a timer tick by having M-mode
    /// software raise `mip.STIP` itself after fielding MTIP — a *forward*,
    /// not a delegation — and this is what the CLINT does here directly:
    /// `MTIP` is the wire straight from the comparator (`clint.pending()`),
    /// recomputed every call in both directions exactly like real hardware;
    /// `STIP` is the software-controlled forward of that same signal, so it
    /// is only ever *raised* here — it is cleared exclusively by
    /// `sbi::EXT_SET_TIMER`'s explicit acknowledgment (see `sbi.rs`), never
    /// auto-cleared by this recompute. Both bits are driven by the same
    /// `clint.pending()` unconditionally: a guest that happens to run
    /// bare-metal in M-mode can still take MTIP (gated by `MIE`, which no
    /// S-mode guest can ever set — it lives at CSR 0x304, an M-mode-only
    /// address), while the S-mode Linux guest this emulator targets takes
    /// only STIP, through `stvec`, cause 5 — the path that actually makes
    /// the scheduler tick.
    pub fn check_interrupts<B: MemBacking, S: ConsoleSink>(&mut self, bus: &mut Bus<B, S>) {
        const MTIP: u64 = 1 << 7;
        const STIP: u64 = 1 << 5;
        let mut mip = self.csrs.read(csr::MIP);
        if bus.clint.pending() {
            mip |= MTIP | STIP;
        } else {
            mip &= !MTIP;
        }
        self.csrs.write(csr::MIP, mip);

        // Machine timer: real priority order puts M-level interrupts ahead
        // of S-level ones, and this is the higher-privilege path. It
        // `return`s before the STIP forward below is ever reached, so it
        // depends on `mie.MTIE` staying clear for the S-mode guest this
        // emulator targets — `Csrs::default` (the firmware-init reset this
        // task added) deliberately never sets it, precisely so this branch
        // stays permanently inert and the STIP forward below is always
        // reached. If firmware init ever needs to set `mie.MTIE` for a
        // bare-metal M-mode guest, this branch would then need to itself
        // forward to STIP (as real OpenSBI's trap handler does) rather than
        // just `return`ing after entering the M-mode trap, or a guest with
        // both bits enabled would never see its supervisor timer interrupt.
        if mip & MTIP != 0 && self.csrs.read(csr::MIE) & MTIP != 0 && self.interrupt_enabled(false)
        {
            self.enter_trap(Interrupt::MachineTimer.cause(), 0, false);
            return;
        }

        // Supervisor timer: the forwarded path a Linux guest actually takes.
        if mip & STIP != 0 && self.csrs.read(csr::MIE) & STIP != 0 && self.interrupt_enabled(true)
        {
            self.enter_trap(Interrupt::SupervisorTimer.cause(), 0, true);
        }
    }

    pub fn mret(&mut self) {
        let status = self.csrs.read(csr::MSTATUS);
        let mpp = (status >> 11) & 0b11;
        let mpie = (status >> 7) & 1;
        let mut new = (status & !((1 << 3) | (1 << 7) | (0b11 << 11))) | (mpie << 3) | (1 << 7);
        // Per the privileged spec, MRET clears MPRV whenever the new
        // privilege mode (MPP) is below M — an M-mode-only memory-access
        // override cannot apply once execution leaves M-mode. This MMU does
        // not currently honor MPRV for translation (see `mmu::translate`'s
        // doc comment), so the bit has no behavioral effect yet, but it
        // must still read back correctly from mstatus.
        if mpp != Priv::M as u64 {
            new &= !(1 << 17);
        }
        self.csrs.write(csr::MSTATUS, new);
        self.priv_ = match mpp {
            0 => Priv::U,
            1 => Priv::S,
            _ => Priv::M,
        };
        self.jump(self.csrs.read(csr::MEPC));
    }

    pub fn sret(&mut self) {
        let status = self.csrs.read(csr::MSTATUS);
        let spp = (status >> 8) & 1;
        let spie = (status >> 5) & 1;
        let new = (status & !((1 << 1) | (1 << 5) | (1 << 8))) | (spie << 1) | (1 << 5);
        self.csrs.write(csr::MSTATUS, new);
        self.priv_ = if spp == 1 { Priv::S } else { Priv::U };
        self.jump(self.csrs.read(csr::SEPC));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backing::FakeBacking;
    use crate::cache::PageCache;
    use crate::uart::VecSink;
    use crate::RAM_BASE;

    pub fn run(words: &[u32]) -> (Cpu, Bus<FakeBacking, VecSink>) {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        for (i, w) in words.iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(RAM_BASE);
        for _ in 0..words.len() {
            cpu.step(&mut bus).unwrap();
        }
        (cpu, bus)
    }

    /// addi x1, x0, 5
    #[test]
    fn addi_sets_register() {
        let (cpu, _) = run(&[0x0050_0093]);
        assert_eq!(cpu.reg(1), 5);
    }

    /// addi x1, x0, -1  — immediate must sign-extend to 64 bits
    #[test]
    fn addi_sign_extends_immediate() {
        let (cpu, _) = run(&[0xFFF0_0093]);
        assert_eq!(cpu.reg(1), u64::MAX);
    }

    /// addi x0, x0, 7 — x0 is hardwired zero
    #[test]
    fn x0_is_immutable() {
        let (cpu, _) = run(&[0x0070_0013]);
        assert_eq!(cpu.reg(0), 0);
    }

    /// lui x1, 0x12345
    #[test]
    fn lui_loads_upper_immediate() {
        let (cpu, _) = run(&[0x1234_50B7]);
        assert_eq!(cpu.reg(1), 0x1234_5000);
    }

    /// addi x1, x0, -1 ; slti x2, x1, 0  — signed comparison
    #[test]
    fn slti_is_signed() {
        let (cpu, _) = run(&[0xFFF0_0093, 0x0000_A113]);
        assert_eq!(cpu.reg(2), 1);
    }

    /// addi x1, x0, -1 ; sltiu x2, x1, 0 — unsigned comparison
    #[test]
    fn sltiu_is_unsigned() {
        let (cpu, _) = run(&[0xFFF0_0093, 0x0000_B113]);
        assert_eq!(cpu.reg(2), 0);
    }

    /// addiw sign-extends the 32-bit result
    /// lui x1, 0x80000 ; addiw x2, x1, 0
    #[test]
    fn addiw_sign_extends_word_result() {
        let (cpu, _) = run(&[0x8000_00B7, 0x0000_811B]);
        assert_eq!(cpu.reg(2), 0xFFFF_FFFF_8000_0000);
    }

    #[test]
    fn pc_advances_by_four() {
        let (cpu, _) = run(&[0x0050_0093]);
        assert_eq!(cpu.pc, RAM_BASE + 4);
    }

    /// addi x1,x0,5 ; slli x1,x1,4  => 80
    #[test]
    fn slli_shifts_left() {
        let (cpu, _) = run(&[0x0050_0093, 0x0040_9093]);
        assert_eq!(cpu.reg(1), 80);
    }

    /// addi x1,x0,80 ; srli x2,x1,2  => 20
    #[test]
    fn srli_shifts_right_logical() {
        let (cpu, _) = run(&[0x0500_0093, 0x0020_D113]);
        assert_eq!(cpu.reg(2), 20);
    }

    /// addi x1,x0,-8 ; srai x2,x1,2  => -2 (sign preserved)
    #[test]
    fn srai_shifts_right_arithmetic() {
        let (cpu, _) = run(&[0xFF80_0093, 0x4020_D113]);
        assert_eq!(cpu.reg(2) as i64, -2);
    }

    /// addi x1,x0,5 ; xori x2,x1,0xF  => 10
    #[test]
    fn xori_exclusive_ors() {
        let (cpu, _) = run(&[0x0050_0093, 0x00F0_C113]);
        assert_eq!(cpu.reg(2), 10);
    }

    /// addi x1,x0,5 ; ori x2,x1,0xF  => 15
    #[test]
    fn ori_ors() {
        let (cpu, _) = run(&[0x0050_0093, 0x00F0_E113]);
        assert_eq!(cpu.reg(2), 15);
    }

    /// addi x1,x0,5 ; andi x2,x1,0xC  => 4
    #[test]
    fn andi_ands() {
        let (cpu, _) = run(&[0x0050_0093, 0x00C0_F113]);
        assert_eq!(cpu.reg(2), 4);
    }

    /// auipc x1, 0x1  => pc + 0x1000
    #[test]
    fn auipc_adds_shifted_immediate_to_pc() {
        let (cpu, _) = run(&[0x0000_1097]);
        assert_eq!(cpu.reg(1), RAM_BASE + 0x1000);
    }

    /// lui x1,0x08000 ; slliw x2,x1,4
    /// 0x0800_0000 << 4 = 0x8000_0000, which sign-extends as a 32-bit result
    #[test]
    fn slliw_computes_in_32_bits_and_sign_extends() {
        let (cpu, _) = run(&[0x0800_00B7, 0x0040_911B]);
        assert_eq!(cpu.reg(2), 0xFFFF_FFFF_8000_0000);
    }

    /// lui x1,0x80000 ; srliw x2,x1,4
    /// shifts the UNSIGNED 32-bit value: 0x8000_0000 >> 4 = 0x0800_0000
    #[test]
    fn srliw_shifts_the_unsigned_word() {
        let (cpu, _) = run(&[0x8000_00B7, 0x0040_D11B]);
        assert_eq!(cpu.reg(2), 0x0800_0000);
    }

    /// lui x1,0x80000 ; sraiw x2,x1,4
    /// shifts the SIGNED 32-bit value: 0x8000_0000 >> 4 = 0xF800_0000, sign-extended
    #[test]
    fn sraiw_shifts_the_signed_word() {
        let (cpu, _) = run(&[0x8000_00B7, 0x4040_D11B]);
        assert_eq!(cpu.reg(2), 0xFFFF_FFFF_F800_0000);
    }

    struct FailingBacking;
    impl crate::backing::MemBacking for FailingBacking {
        fn read_page(
            &mut self,
            _p: u32,
            _b: &mut [u8; crate::PAGE],
        ) -> Result<(), crate::backing::Error> {
            Err(crate::backing::Error::Medium)
        }
        fn write_page(
            &mut self,
            _p: u32,
            _b: &[u8; crate::PAGE],
        ) -> Result<(), crate::backing::Error> {
            Err(crate::backing::Error::Medium)
        }
        fn flush(&mut self) -> Result<(), crate::backing::Error> {
            Ok(())
        }
    }

    #[test]
    fn fetch_propagates_backing_failure_rather_than_masking_it() {
        let mut bus = Bus::new(PageCache::new(FailingBacking, 4), VecSink::default());
        let mut cpu = Cpu::new(RAM_BASE);
        assert!(matches!(cpu.step(&mut bus), Err(Exception::BackingFailure(_))));
    }

    /// addi x1,x0,7 ; addi x2,x0,9 ; add x3,x1,x2
    #[test]
    fn add_register_register() {
        let (cpu, _) = run(&[0x0070_0093, 0x0090_0113, 0x0020_81B3]);
        assert_eq!(cpu.reg(3), 16);
    }

    /// addi x1,x0,5 ; addi x2,x0,9 ; sub x3,x1,x2  => -4
    #[test]
    fn sub_wraps_signed() {
        let (cpu, _) = run(&[0x0050_0093, 0x0090_0113, 0x4020_81B3]);
        assert_eq!(cpu.reg(3) as i64, -4);
    }

    /// addi x1,x0,0x123 ; sd x1,0(x2 with x2=RAM_BASE) ; ld x3,0(x2)
    ///
    /// x2 is seeded via `auipc x2, 0` rather than `lui x2, 0x80000`: LUI
    /// sign-extends its 32-bit result to 64 bits per the RV64I spec (already
    /// verified in Task 5's `imm_u`), so `lui x2, 0x80000` actually produces
    /// `0xFFFF_FFFF_8000_0000`, not `RAM_BASE` (0x8000_0000) — that address
    /// is far outside RAM and faults. `auipc x2, 0` sets x2 = pc, which is
    /// already a valid, correctly-formed address with no sign-extension risk.
    #[test]
    fn store_then_load_doubleword() {
        // auipc x2, 0 ; addi x1,x0,0x123 ; sd x1,0(x2) ; ld x3,0(x2)
        let auipc_x2 = 0x0000_0117u32; // auipc x2, 0 => x2 = pc == RAM_BASE
        let (cpu, _) = run(&[auipc_x2, 0x1230_0093, 0x0011_3023, 0x0001_3183]);
        assert_eq!(cpu.reg(3), 0x123);
    }

    /// lb sign-extends, lbu does not
    ///
    /// See `store_then_load_doubleword` for why `auipc x2, 0` is used
    /// instead of `lui x2, 0x80000` to seed the base address.
    #[test]
    fn byte_loads_differ_in_sign_extension() {
        let auipc_x2 = 0x0000_0117u32;
        // auipc x2, 0 ; addi x1,x0,-1 ; sb x1,0(x2) ; lb x3,0(x2) ; lbu x4,0(x2)
        let (cpu, _) = run(&[auipc_x2, 0xFFF0_0093, 0x0011_0023, 0x0001_0183, 0x0001_4203]);
        assert_eq!(cpu.reg(3) as i64, -1);
        assert_eq!(cpu.reg(4), 0xFF);
    }

    /// beq taken skips the next instruction
    #[test]
    fn beq_taken_branches() {
        // beq x0,x0,+8 ; addi x1,x0,1 ; addi x2,x0,2
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        for (i, w) in [0x0000_0463u32, 0x0010_0093, 0x0020_0113].iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.step(&mut bus).unwrap(); // beq
        cpu.step(&mut bus).unwrap(); // should be the addi x2
        assert_eq!(cpu.reg(1), 0, "skipped instruction must not execute");
        assert_eq!(cpu.reg(2), 2);
    }

    /// bne not taken falls through
    #[test]
    fn bne_not_taken_falls_through() {
        let (cpu, _) = run(&[0x0000_1463, 0x0010_0093]);
        assert_eq!(cpu.reg(1), 1);
    }

    /// jal x1, +8 — link register holds pc+4
    #[test]
    fn jal_links_return_address() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x0080_00EFu64).unwrap(); // jal x1, +8
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.reg(1), RAM_BASE + 4);
        assert_eq!(cpu.pc, RAM_BASE + 8);
    }

    /// auipc x1, 0 — must use the pc of the auipc itself
    #[test]
    fn auipc_uses_its_own_pc() {
        let (cpu, _) = run(&[0x0000_0097]);
        assert_eq!(cpu.reg(1), RAM_BASE);
    }

    /// auipc x2,0 ; jalr x1,5(x2)
    /// Target is x2+5 = RAM_BASE+5, and the spec requires clearing the low bit,
    /// so the pc must land on RAM_BASE+4, not RAM_BASE+5.
    #[test]
    fn jalr_masks_the_low_bit_of_its_target() {
        let (cpu, _) = run(&[0x0000_0117, 0x0051_00E7]);
        assert_eq!(cpu.pc, RAM_BASE + 4, "low bit of the target must be cleared");
        assert_eq!(cpu.reg(1), RAM_BASE + 8, "link is the address after the jalr");
    }

    /// auipc x2,0 ; jalr x2,16(x2)
    /// rd and rs1 are the same register: rs1 must be read before rd is written,
    /// or the computed target is garbage.
    #[test]
    fn jalr_reads_rs1_before_writing_rd() {
        let (cpu, _) = run(&[0x0000_0117, 0x0101_0167]);
        assert_eq!(cpu.pc, RAM_BASE + 16, "target computed from the ORIGINAL x2");
        assert_eq!(cpu.reg(2), RAM_BASE + 8, "link value overwrote x2");
    }

    /// auipc x2,0 ; addi x1,x0,-1 ; sh x1,0(x2) ; lh x3,0(x2) ; lhu x4,0(x2)
    #[test]
    fn halfword_loads_differ_in_sign_extension() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_1023, 0x0001_1183, 0x0001_5203,
        ]);
        assert_eq!(cpu.reg(3) as i64, -1, "lh sign-extends");
        assert_eq!(cpu.reg(4), 0xFFFF, "lhu zero-extends");
    }

    /// auipc x2,0 ; addi x1,x0,-1 ; sw x1,0(x2) ; lw x3,0(x2) ; lwu x4,0(x2)
    #[test]
    fn word_loads_differ_in_sign_extension() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0001_2183, 0x0001_6203,
        ]);
        assert_eq!(cpu.reg(3) as i64, -1, "lw sign-extends");
        assert_eq!(cpu.reg(4), 0xFFFF_FFFF, "lwu zero-extends, RV64 only");
    }

    /// addi x1,x0,6 ; addi x2,x0,7 ; mul x3,x1,x2
    #[test]
    fn mul_multiplies() {
        let (cpu, _) = run(&[0x0060_0093, 0x0070_0113, 0x0220_81B3]);
        assert_eq!(cpu.reg(3), 42);
    }

    /// Division by zero returns all ones, per the spec — it must not trap.
    /// addi x1,x0,5 ; div x3,x1,x0
    #[test]
    fn div_by_zero_returns_all_ones() {
        let (cpu, _) = run(&[0x0050_0093, 0x0200_C1B3]);
        assert_eq!(cpu.reg(3), u64::MAX);
    }

    /// Remainder by zero returns the dividend.
    /// addi x1,x0,5 ; rem x3,x1,x0
    #[test]
    fn rem_by_zero_returns_dividend() {
        let (cpu, _) = run(&[0x0050_0093, 0x0200_E1B3]);
        assert_eq!(cpu.reg(3), 5);
    }

    #[test]
    fn signed_division_overflow_is_defined() {
        // i64::MIN / -1 must yield i64::MIN, not panic.
        assert_eq!(crate::insn::rv64m::div_signed(i64::MIN, -1), i64::MIN);
        assert_eq!(crate::insn::rv64m::rem_signed(i64::MIN, -1), 0);
    }

    /// addi x1,x0,-1 ; addi x2,x0,-1 ; mulh x3,x1,x2
    /// (-1) * (-1) = 1, so the high 64 bits are 0.
    #[test]
    fn mulh_is_signed_times_signed() {
        let (cpu, _) = run(&[0xFFF0_0093, 0xFFF0_0113, 0x0220_91B3]);
        assert_eq!(cpu.reg(3), 0);
    }

    /// addi x1,x0,-1 ; addi x2,x0,-1 ; mulhsu x3,x1,x2
    /// x1 signed = -1, x2 unsigned = 2^64-1, product = -(2^64-1),
    /// whose high 64 bits are all ones.
    #[test]
    fn mulhsu_is_signed_times_unsigned() {
        let (cpu, _) = run(&[0xFFF0_0093, 0xFFF0_0113, 0x0220_A1B3]);
        assert_eq!(cpu.reg(3), 0xFFFF_FFFF_FFFF_FFFF);
    }

    /// addi x1,x0,-1 ; addi x2,x0,-1 ; mulhu x3,x1,x2
    /// Both unsigned: (2^64-1)^2 has high 64 bits 0xFFFF_FFFF_FFFF_FFFE.
    #[test]
    fn mulhu_is_unsigned_times_unsigned() {
        let (cpu, _) = run(&[0xFFF0_0093, 0xFFF0_0113, 0x0220_B1B3]);
        assert_eq!(cpu.reg(3), 0xFFFF_FFFF_FFFF_FFFE);
    }

    /// addi x1,x0,5 ; divu x3,x1,x0  => all ones, must not trap
    #[test]
    fn divu_by_zero_returns_all_ones() {
        let (cpu, _) = run(&[0x0050_0093, 0x0200_D1B3]);
        assert_eq!(cpu.reg(3), u64::MAX);
    }

    /// addi x1,x0,5 ; remu x3,x1,x0  => dividend, must not trap
    #[test]
    fn remu_by_zero_returns_dividend() {
        let (cpu, _) = run(&[0x0050_0093, 0x0200_F1B3]);
        assert_eq!(cpu.reg(3), 5);
    }

    /// lui x1,0x10 ; lui x2,0x8 ; mulw x3,x1,x2
    /// 2^16 * 2^15 = 2^31, which is negative as a 32-bit result and sign-extends.
    #[test]
    fn mulw_sign_extends_its_32_bit_result() {
        let (cpu, _) = run(&[0x0001_00B7, 0x0000_8137, 0x0220_81BB]);
        assert_eq!(cpu.reg(3), 0xFFFF_FFFF_8000_0000);
    }

    /// lui x1,0x80000 ; addi x2,x0,-1 ; divw x3,x1,x2
    /// i32::MIN / -1 overflows; the spec says return i32::MIN, sign-extended.
    #[test]
    fn divw_overflow_returns_i32_min() {
        let (cpu, _) = run(&[0x8000_00B7, 0xFFF0_0113, 0x0220_C1BB]);
        assert_eq!(cpu.reg(3), 0xFFFF_FFFF_8000_0000);
    }

    /// lui x1,0x80000 ; addi x2,x0,-1 ; remw x3,x1,x2  => 0
    #[test]
    fn remw_overflow_returns_zero() {
        let (cpu, _) = run(&[0x8000_00B7, 0xFFF0_0113, 0x0220_E1BB]);
        assert_eq!(cpu.reg(3), 0);
    }

    /// lui x1,0x80000 ; addi x2,x0,-1 ; divuw x3,x1,x2
    /// Unsigned: 0x8000_0000 / 0xFFFF_FFFF = 0. Contrast with divw above,
    /// which returns i32::MIN for the same operands.
    #[test]
    fn divuw_treats_both_words_as_unsigned() {
        let (cpu, _) = run(&[0x8000_00B7, 0xFFF0_0113, 0x0220_D1BB]);
        assert_eq!(cpu.reg(3), 0);
    }

    /// lui x1,0x80000 ; addi x2,x0,-1 ; remuw x3,x1,x2
    /// 0x8000_0000 % 0xFFFF_FFFF = 0x8000_0000, sign-extended as a word result.
    #[test]
    fn remuw_treats_both_words_as_unsigned() {
        let (cpu, _) = run(&[0x8000_00B7, 0xFFF0_0113, 0x0220_F1BB]);
        assert_eq!(cpu.reg(3), 0xFFFF_FFFF_8000_0000);
    }

    #[test]
    fn word_division_overflow_is_defined() {
        assert_eq!(crate::insn::rv64m::div_signed_w(i32::MIN, -1), i32::MIN);
        assert_eq!(crate::insn::rv64m::rem_signed_w(i32::MIN, -1), 0);
        assert_eq!(crate::insn::rv64m::div_signed_w(5, 0), -1);
        assert_eq!(crate::insn::rv64m::rem_signed_w(5, 0), 5);
    }

    /// auipc x2,0 ; addi x1,x0,5 ; sd x1,0(x2) ; addi x3,x0,3 ;
    /// amoadd.d x4,x3,(x2) ; ld x5,0(x2)
    #[test]
    fn amoadd_returns_old_and_stores_sum() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0x0030_0193,
            0x0031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(4), 5, "amo returns the old value");
        assert_eq!(cpu.reg(5), 8, "memory holds the sum");
    }

    /// auipc x2,0 ; lr.d x1,(x2) ; addi x3,x0,7 ; sc.d x4,x3,(x2)
    #[test]
    fn sc_succeeds_after_lr_on_same_address() {
        let (cpu, _) = run(&[0x0000_0117, 0x1001_30AF, 0x0070_0193, 0x1831_322F]);
        assert_eq!(cpu.reg(4), 0, "sc must report success");
    }

    /// auipc x2,0 ; addi x3,x0,7 ; sc.d x4,x3,(x2)  — no preceding lr.d
    #[test]
    fn sc_fails_without_reservation() {
        let (cpu, _) = run(&[0x0000_0117, 0x0070_0193, 0x1831_322F]);
        assert_eq!(cpu.reg(4), 1, "sc must report failure");
    }

    /// auipc x2,0 ; addi x1,x0,-1 ; sw x1,0(x2) ; addi x3,x0,1 ;
    /// amomin.w x4,x3,(x2) ; lw x5,0(x2)
    /// Signed: min(-1, 1) = -1, so memory is unchanged.
    #[test]
    fn amomin_w_compares_as_signed_32_bit() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0010_0193,
            0x8031_222F, 0x0001_2283,
        ]);
        assert_eq!(cpu.reg(4) as i64, -1, "rd is the sign-extended old word");
        assert_eq!(cpu.reg(5) as i64, -1, "memory keeps -1, the signed minimum");
    }

    /// Same operands, AMOMAX.W: max(-1, 1) = 1, so memory becomes 1.
    #[test]
    fn amomax_w_compares_as_signed_32_bit() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0010_0193,
            0xA031_222F, 0x0001_2283,
        ]);
        assert_eq!(cpu.reg(4) as i64, -1);
        assert_eq!(cpu.reg(5), 1, "memory takes 1, the signed maximum");
    }

    /// Same operands, AMOMINU.W: unsigned min(0xFFFF_FFFF, 1) = 1.
    /// Contrast with amomin.w above, which keeps -1.
    #[test]
    fn amominu_w_compares_as_unsigned_32_bit() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0010_0193,
            0xC031_222F, 0x0001_2283,
        ]);
        assert_eq!(cpu.reg(5), 1);
    }

    /// Same operands, AMOMAXU.W: unsigned max = 0xFFFF_FFFF, memory unchanged.
    /// Contrast with amomax.w above, which writes 1.
    #[test]
    fn amomaxu_w_compares_as_unsigned_32_bit() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0010_0193,
            0xE031_222F, 0x0001_2283,
        ]);
        assert_eq!(cpu.reg(5) as i64, -1);
    }

    /// amoadd.w on 0xFFFF_FFFF + 1 wraps the word to 0; rd is the sign-extended old.
    #[test]
    fn amoadd_w_sign_extends_rd_and_wraps_the_word() {
        let (cpu, _) = run(&[
            0x0000_0117, 0xFFF0_0093, 0x0011_2023, 0x0010_0193,
            0x0031_222F, 0x0001_2283,
        ]);
        assert_eq!(cpu.reg(4) as i64, -1);
        assert_eq!(cpu.reg(5), 0);
    }

    /// auipc x2,0 ; addi x1,x0,5 ; sd x1,0(x2) ; addi x3,x0,3 ; <amo> ; ld x5,0(x2)
    #[test]
    fn amoswap_d_replaces_memory_and_returns_old() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0x0030_0193,
            0x0831_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(4), 5);
        assert_eq!(cpu.reg(5), 3);
    }

    #[test]
    fn amoxor_d_exclusive_ors() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0x0030_0193,
            0x2031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(5), 6);
    }

    #[test]
    fn amoor_d_ors() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0x0030_0193,
            0x4031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(5), 7);
    }

    #[test]
    fn amoand_d_ands() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0x0030_0193,
            0x6031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(5), 1);
    }

    /// memory 5, src -1. Signed min is -1.
    #[test]
    fn amomin_d_compares_as_signed() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0xFFF0_0193,
            0x8031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(5) as i64, -1);
    }

    /// Same operands unsigned: -1 is 2^64-1, so the minimum is 5 and memory holds.
    #[test]
    fn amominu_d_compares_as_unsigned() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x0050_0093, 0x0011_3023, 0xFFF0_0193,
            0xC031_322F, 0x0001_3283,
        ]);
        assert_eq!(cpu.reg(5), 5);
    }

    /// auipc x2,0 ; lr.d x1,(x2) ; sd x3,0(x2) ; addi x3,x0,7 ; sc.d x4,x3,(x2)
    /// An ordinary store between LR and SC must break the reservation.
    #[test]
    fn plain_store_invalidates_the_reservation() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x1001_30AF, 0x0031_3023, 0x0070_0193, 0x1831_322F,
        ]);
        assert_eq!(cpu.reg(4), 1, "sc must fail after an intervening store");
    }

    /// auipc x2,0 ; lr.d x1,(x2) ; addi x3,x0,3 ; amoadd.d x4,x3,(x2) ;
    /// sc.d x4,x3,(x2)
    /// An AMO is a write, so it must break the reservation just as a plain
    /// store does.
    #[test]
    fn amo_invalidates_the_reservation() {
        let (cpu, _) = run(&[
            0x0000_0117, 0x1001_30AF, 0x0030_0193, 0x0031_322F, 0x1831_322F,
        ]);
        assert_eq!(cpu.reg(4), 1, "sc must fail after an intervening AMO");
    }

    /// c.nop at pc 0 must advance the pc by 2, not 4. `run()` loads 32-bit
    /// words and is unsuitable here, so the halfword is stored directly.
    #[test]
    fn compressed_instruction_advances_pc_by_two() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 2, 0x0001).unwrap(); // c.nop
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, RAM_BASE + 2);
    }

    /// auipc x1,0 ; addi x1,x1,8 ; c.jalr x1  (link register is x1 itself)
    ///
    /// The c.jalr is 2 bytes at RAM_BASE+8, so its link value must be
    /// (RAM_BASE+8) + 2, not +4. Getting `insn_len` wrong here would return
    /// to the wrong address after a compressed call — exactly the bug the
    /// brief calls out.
    #[test]
    fn compressed_jalr_links_pc_plus_insn_len_not_four() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        // auipc x1, 0
        bus.store(RAM_BASE, 4, 0x0000_0097u64).unwrap();
        // addi x1, x1, 8
        bus.store(RAM_BASE + 4, 4, 0x0080_8093u64).unwrap();
        // c.jalr x1  (funct4=1001, rs1=x1, rs2=0, op=10)
        bus.store(RAM_BASE + 8, 2, 0x9082u64).unwrap();

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.step(&mut bus).unwrap(); // auipc x1, 0  => x1 = RAM_BASE
        cpu.step(&mut bus).unwrap(); // addi x1,x1,8 => x1 = RAM_BASE + 8
        assert_eq!(cpu.reg(1), RAM_BASE + 8);
        cpu.step(&mut bus).unwrap(); // c.jalr x1
        assert_eq!(cpu.pc, RAM_BASE + 8, "target is the pre-link value of x1");
        assert_eq!(
            cpu.reg(1),
            RAM_BASE + 10,
            "link must be pc + insn_len (2 for a compressed instruction), not pc + 4"
        );
    }

    use crate::csr::{self, Priv};

    /// csrrw x1, mtvec, x2 writes and returns the old value
    #[test]
    fn csrrw_swaps_value() {
        // addi x2,x0,0x100 ; csrrw x1,mtvec,x2 ; csrrw x3,mtvec,x0
        let (cpu, _) = run(&[0x1000_0113, 0x3051_10F3, 0x3050_11F3]);
        assert_eq!(cpu.reg(1), 0, "old mtvec was zero");
        assert_eq!(cpu.reg(3), 0x100, "mtvec now holds the written value");
    }

    /// ECALL from M-mode traps to mtvec with cause 11
    #[test]
    fn ecall_traps_to_mtvec() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        for (i, w) in [0x1000_0113u32, 0x3051_10F3, 0x0000_0073].iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        let ecall_pc = cpu.pc;
        let _ = cpu.step_trapping(&mut bus);
        assert_eq!(cpu.pc, 0x100, "must vector to mtvec");
        assert_eq!(cpu.csrs.read(csr::MCAUSE), 11);
        assert_eq!(cpu.csrs.read(csr::MEPC), ecall_pc);
    }

    #[test]
    fn sstatus_is_a_masked_view_of_mstatus() {
        let mut c = crate::csr::Csrs::default();
        c.write(csr::MSTATUS, 0xFFFF_FFFF_FFFF_FFFF);
        let s = c.read(csr::SSTATUS);
        assert_eq!(s & (1 << 3), 0, "MIE must not be visible in sstatus");
        assert_ne!(s & (1 << 1), 0, "SIE must be visible in sstatus");
    }

    #[test]
    fn cpu_starts_in_machine_mode() {
        let cpu = Cpu::new(RAM_BASE);
        assert_eq!(cpu.priv_, Priv::M);
    }

    // --- Fix round 1: mret/sret, S-mode delegation, and remaining SYSTEM
    // decode paths (CSRRS/CSRRC, the immediate forms, EBREAK/WFI/SFENCE.VMA)
    // ---

    /// Full round trip: M-mode delegates ECALL-from-U to S-mode via
    /// medeleg, exercising `trap`'s to-S branch, `mret`'s M->U transition,
    /// and `sret`'s S->U return together.
    #[test]
    fn ecall_from_u_mode_delegates_to_s_mode_and_sret_returns() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        let mret = 0x3020_0073u32;
        let ecall = 0x0000_0073u32;
        let sret = 0x1020_0073u32;
        bus.store(RAM_BASE, 4, mret as u64).unwrap();
        bus.store(RAM_BASE + 4, 4, ecall as u64).unwrap();
        let stvec_target = RAM_BASE + 0x200;
        bus.store(stvec_target, 4, sret as u64).unwrap();

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.csrs.write(csr::MEDELEG, 1 << 8); // delegate ECALL-from-U-mode
        cpu.csrs.write(csr::MEPC, RAM_BASE + 4); // mret lands on the ECALL
        cpu.csrs.write(csr::STVEC, stvec_target);

        cpu.step(&mut bus).unwrap(); // MRET: M -> U
        assert_eq!(cpu.priv_, Priv::U, "mret with mpp=U drops to user mode");
        assert_eq!(cpu.pc, RAM_BASE + 4, "mret jumps to mepc");

        // Set SIE so the trap's SIE->SPIE move is observable, not vacuous.
        let mstatus = cpu.csrs.read(csr::MSTATUS);
        cpu.csrs.write(csr::MSTATUS, mstatus | (1 << 1));

        let _ = cpu.step_trapping(&mut bus); // ECALL from U, delegated to S
        assert_eq!(cpu.priv_, Priv::S, "medeleg bit 8 delegates to S-mode");
        assert_eq!(cpu.pc, stvec_target, "must vector to stvec, not mtvec");
        assert_eq!(cpu.csrs.read(csr::SCAUSE), 8, "cause is ECALL-from-U-mode");
        assert_eq!(cpu.csrs.read(csr::SEPC), RAM_BASE + 4);
        let sstatus = cpu.csrs.read(csr::SSTATUS);
        assert_eq!(sstatus & (1 << 8), 0, "SPP=U: trap originated in U-mode");
        assert_eq!(sstatus & (1 << 5), 1 << 5, "SIE moved into SPIE");
        assert_eq!(sstatus & (1 << 1), 0, "SIE cleared on trap entry");

        cpu.step(&mut bus).unwrap(); // SRET: S -> U (SPP recorded U)
        assert_eq!(cpu.priv_, Priv::U, "sret restores the pre-trap privilege");
        assert_eq!(cpu.pc, RAM_BASE + 4, "sret returns to sepc");
        let sstatus = cpu.csrs.read(csr::SSTATUS);
        assert_ne!(sstatus & (1 << 1), 0, "SIE restored from SPIE");
        assert_ne!(sstatus & (1 << 5), 0, "SPIE is always set to 1 by sret");
    }

    /// addi x1,x0,0xF0 ; csrrw x0,mscratch,x1 ; addi x2,x0,0xF ; csrrs x3,mscratch,x2
    #[test]
    fn csrrs_sets_bits_without_clobbering_others() {
        let (cpu, _) = run(&[0x0F00_0093, 0x3400_9073, 0x00F0_0113, 0x3401_21F3]);
        assert_eq!(cpu.reg(3), 0xF0, "csrrs returns the old value");
        assert_eq!(cpu.csrs.read(csr::MSCRATCH), 0xFF, "0xF0 | 0xF");
    }

    /// addi x1,x0,0xFF ; csrrw x0,mscratch,x1 ; addi x2,x0,0xF ; csrrc x3,mscratch,x2
    #[test]
    fn csrrc_clears_bits() {
        let (cpu, _) = run(&[0x0FF0_0093, 0x3400_9073, 0x00F0_0113, 0x3401_31F3]);
        assert_eq!(cpu.reg(3), 0xFF, "csrrc returns the old value");
        assert_eq!(cpu.csrs.read(csr::MSCRATCH), 0xF0, "0xFF & !0xF");
    }

    /// csrrwi x0,mscratch,5 ; csrrsi x1,mscratch,2 ; csrrci x2,mscratch,1
    #[test]
    fn csrrwi_csrrsi_csrrci_use_the_five_bit_immediate_source() {
        let (cpu, _) = run(&[0x3402_D073, 0x3401_60F3, 0x3400_F173]);
        assert_eq!(cpu.reg(1), 5, "csrrsi returns mscratch as csrrwi left it");
        assert_eq!(cpu.reg(2), 7, "csrrci returns mscratch as csrrsi left it");
        assert_eq!(cpu.csrs.read(csr::MSCRATCH), 6, "(5 | 2) & !1");
    }

    /// ebreak
    #[test]
    fn ebreak_raises_breakpoint() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x0010_0073u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        assert!(matches!(cpu.step(&mut bus), Err(Exception::Breakpoint)));
    }

    /// wfi ; addi x1,x0,1 — wfi must not block execution or alter state
    #[test]
    fn wfi_is_a_noop() {
        let (cpu, _) = run(&[0x1050_0073, 0x0010_0093]);
        assert_eq!(cpu.reg(1), 1);
        assert_eq!(cpu.pc, RAM_BASE + 8);
    }

    /// sfence.vma x0,x0 ; addi x1,x0,1
    #[test]
    fn sfence_vma_is_a_noop() {
        let (cpu, _) = run(&[0x1200_0073, 0x0010_0093]);
        assert_eq!(cpu.reg(1), 1);
        assert_eq!(cpu.pc, RAM_BASE + 8);
    }

    // --- Finding 1: privilege and read-only enforcement ---

    /// csrrs x1,mstatus,x0 from U-mode — mstatus requires M-level access,
    /// so even a pure read must trap.
    #[test]
    fn u_mode_csr_read_of_m_mode_csr_traps() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x3000_20F3u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::U;
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// csrrw x1,mhartid,x0 — mhartid is read-only (csr_addr[11:10] == 0b11);
    /// M-mode has full privilege here, isolating the read-only check from
    /// the privilege check.
    #[test]
    fn write_to_read_only_csr_traps() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0xF140_10F3u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// mret executed from U-mode must trap rather than silently escalate
    /// privilege to whatever mstatus.MPP holds.
    #[test]
    fn u_mode_mret_traps() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x3020_0073u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::U;
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// sret executed from U-mode must trap; only S-mode and above may sret.
    #[test]
    fn u_mode_sret_traps() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x1020_0073u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::U;
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// csrrs x1,mhartid,x0 — rs1 == 0 means CSRRS performs no write, so it
    /// must NOT trip the read-only check even though mhartid is read-only.
    #[test]
    fn csrrs_with_rs1_zero_against_readonly_csr_does_not_trap() {
        let (cpu, _) = run(&[0xF140_20F3]);
        assert_eq!(cpu.reg(1), 0, "mhartid is 0 on a single-hart emulator");
    }

    // --- Task 11: Sv39 MMU wiring ---

    const PTE_V: u64 = 1;
    const PTE_R: u64 = 1 << 1;
    const PTE_W: u64 = 1 << 2;
    const PTE_X: u64 = 1 << 3;
    const PTE_AD: u64 = (1 << 6) | (1 << 7);

    /// Sets up a shared 3-level Sv39 root/mid/leaf table (all mappings share
    /// vpn2=vpn1=0, so the leaf table has room for 512 independent vaddr ->
    /// paddr entries keyed by vpn0 alone). Returns the bus and the physical
    /// root table address to write into satp.
    fn sv39_root() -> (Bus<FakeBacking, VecSink>, u64) {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mid = RAM_BASE + 0x11_0000;
        let leaf = RAM_BASE + 0x12_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        bus.store(root, 8, ppn(mid) | PTE_V).unwrap();
        bus.store(mid, 8, ppn(leaf) | PTE_V).unwrap();
        (bus, root)
    }

    /// Installs one leaf mapping. `vaddr` must satisfy vaddr < 0x20_0000 so
    /// it lands in the shared leaf table `sv39_root` built above.
    fn map(bus: &mut Bus<FakeBacking, VecSink>, vaddr: u64, paddr: u64, perms: u64) {
        let leaf = RAM_BASE + 0x12_0000;
        let vpn0 = (vaddr >> 12) & 0x1FF;
        let ppn = |a: u64| (a >> 12) << 10;
        bus.store(leaf + vpn0 * 8, 8, ppn(paddr) | perms).unwrap();
    }

    /// End-to-end: with satp active and the hart in S-mode, `Cpu::step` must
    /// translate the instruction fetch *and* the LOAD/STORE data accesses —
    /// not just the standalone `Mmu::translate` calls covered in mmu.rs.
    /// Code lives at vaddr 0x4000 (mapped to RAM_BASE); the data word lives
    /// at a separate vaddr/paddr pair (0x1000 -> RAM_BASE+0x2000).
    #[test]
    fn step_translates_fetch_and_data_through_sv39() {
        let (mut bus, root) = sv39_root();
        let code_pa = RAM_BASE;
        let data_pa = RAM_BASE + 0x2000;
        map(&mut bus, 0x4000, code_pa, PTE_V | PTE_R | PTE_W | PTE_X | PTE_AD);
        map(&mut bus, 0x1000, data_pa, PTE_V | PTE_R | PTE_W | PTE_AD);

        // lui x2,1 ; addi x1,x0,0x123 ; sd x1,0(x2) ; ld x3,0(x2)
        let words = [0x0000_1137u32, 0x1230_0093, 0x0011_3023, 0x0001_3183];
        for (i, w) in words.iter().enumerate() {
            bus.store(code_pa + 4 * i as u64, 4, *w as u64).unwrap();
        }

        let mut cpu = Cpu::new(0x4000);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        for _ in 0..words.len() {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.reg(2), 0x1000, "lui produced the data vaddr");
        assert_eq!(cpu.reg(3), 0x123, "ld must read back what sd wrote, via translation");
        // Confirm the store actually landed at the translated physical
        // address, not at the untranslated vaddr (which is far outside RAM).
        assert_eq!(bus.load(data_pa, 8).unwrap(), 0x123);
    }

    /// Ruling: AMOs are classed as store accesses for fault reporting, even
    /// though every AMO also reads memory. A read-only page must raise a
    /// StorePageFault, not a LoadPageFault, from an AMO's read half.
    #[test]
    fn amo_on_read_only_page_faults_as_store_not_load() {
        let (mut bus, root) = sv39_root();
        map(&mut bus, 0x1000, RAM_BASE + 0x3000, PTE_V | PTE_R | PTE_AD); // no W

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));
        cpu.set_reg(2, 0x1000);
        cpu.set_reg(3, 5);

        // amoadd.d x4, x3, (x2)
        let r = crate::insn::rv64a::execute(&mut cpu, &mut bus, 0x0031_322F);
        assert!(
            matches!(r, Err(Exception::StorePageFault(0x1000))),
            "expected StorePageFault, got {r:?}"
        );
    }

    /// Interface note: LR/SC reservations must be keyed on the translated
    /// *physical* address, not the virtual one, now that translation
    /// exists. Two different virtual addresses aliased to the same physical
    /// page must let an SC through, because real hardware's reservation is
    /// over the physical granule.
    #[test]
    fn reservation_is_keyed_on_physical_address_across_virtual_aliases() {
        let (mut bus, root) = sv39_root();
        let paddr = RAM_BASE + 0x3000;
        map(&mut bus, 0x1000, paddr, PTE_V | PTE_R | PTE_W | PTE_AD);
        map(&mut bus, 0x2000, paddr, PTE_V | PTE_R | PTE_W | PTE_AD);

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        // lr.d x1, (x2) with x2 = 0x1000
        cpu.set_reg(2, 0x1000);
        crate::insn::rv64a::execute(&mut cpu, &mut bus, 0x1001_30AF).unwrap();

        // sc.d x4, x3, (x2) with x2 = 0x2000 — a different vaddr, same paddr
        cpu.set_reg(2, 0x2000);
        cpu.set_reg(3, 7);
        crate::insn::rv64a::execute(&mut cpu, &mut bus, 0x1831_322F).unwrap();
        assert_eq!(cpu.reg(4), 0, "sc must succeed: same physical granule as the lr");
    }

    /// Task 10 defect fix: mret must clear mstatus.MPRV when the new
    /// privilege level (MPP) drops below M, per the privileged spec.
    #[test]
    fn mret_clears_mprv_when_dropping_below_m() {
        let mut cpu = Cpu::new(RAM_BASE);
        let mprv = 1u64 << 17;
        let mpp_s = 1u64 << 11; // MPP = 01 (S)
        cpu.csrs.write(csr::MSTATUS, mprv | mpp_s);
        cpu.mret();
        assert_eq!(cpu.priv_, Priv::S);
        assert_eq!(
            cpu.csrs.read(csr::MSTATUS) & mprv,
            0,
            "MPRV must be cleared once execution leaves M-mode"
        );
    }

    /// SFENCE.VMA requires at least S-mode privilege; a U-mode guest must
    /// trap rather than silently flushing (or no-op'ing on) the TLB.
    #[test]
    fn sfence_vma_traps_from_u_mode() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x1200_0073u64).unwrap(); // sfence.vma x0,x0
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::U;
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// A write to satp must flush the TLB — a stale translation from the
    /// old address space must not survive an address-space switch.
    #[test]
    fn satp_write_flushes_the_tlb() {
        let (mut bus, root) = sv39_root();
        map(&mut bus, 0x1000, RAM_BASE + 0x3000, PTE_V | PTE_R | PTE_AD);

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));
        cpu.mmu
            .translate(&mut bus, &cpu.csrs, cpu.priv_, 0x1000, Access::Load)
            .unwrap();
        assert_eq!(cpu.mmu.walks, 1);

        // Switch to M-mode just to fetch and execute the csrrw below — an
        // M-mode fetch bypasses translation entirely, and satp is still a
        // legal write target from M. This isolates the thing under test
        // (does the CSR write itself flush the TLB) from needing the
        // instruction stream mapped too.
        cpu.priv_ = Priv::M;
        // x1 already holds the same satp value that's active — even a
        // same-value write must flush, since Cpu can't know whether the
        // underlying page tables changed.
        cpu.set_reg(1, (8u64 << 60) | (root >> 12));
        bus.store(RAM_BASE, 4, 0x1800_9073u64).unwrap(); // csrrw x0, satp, x1
        cpu.step(&mut bus).unwrap();
        cpu.priv_ = Priv::S;

        cpu.mmu
            .translate(&mut bus, &cpu.csrs, cpu.priv_, 0x1000, Access::Load)
            .unwrap();
        assert_eq!(cpu.mmu.walks, 2, "satp write must force a re-walk");
    }

    /// `csrrw x0, satp, x1` — `csrw satp, x1`, how a guest installs a
    /// page table.
    const CSRW_SATP_X1: u32 = 0x1800_9073;
    /// `csrrs x2, satp, x0` — `csrr x2, satp`, a pure read with no write
    /// side effect (rs1 == x0 on a CSRRS).
    const CSRR_X2_SATP: u32 = 0x1800_2173;
    /// `csrrw x2, satp, x0` — `csr_swap(satp, 0)`: returns the old value
    /// *and* writes zero. CSRRW always writes, even from x0.
    const CSRSWAP_X2_SATP_ZERO: u32 = 0x1800_9173;

    /// The `satp` MODE clamp, driven through the SYSTEM decoder rather than
    /// by calling `Csrs::write` directly.
    ///
    /// Every other `satp` test reaches the register file straight from Rust.
    /// This one goes the way a guest does — real `csrrw`/`csrrs`
    /// instructions, through the decoder's privilege and read-only gates —
    /// because that is the path Linux takes and the path where a gate
    /// ordering mistake would hide.
    ///
    /// What it pins is the difference between *discarding* and *coercing*.
    /// The privileged spec says a `satp` write with an unsupported MODE has
    /// no effect at all — "no fields in satp are modified" — so a previously
    /// installed Sv39 translation must survive an Sv57 probe completely
    /// intact. Coercing the write to Bare would also defeat the probe, but
    /// it would silently tear down the address space the guest was already
    /// running on.
    #[test]
    fn an_unsupported_satp_mode_written_by_a_real_csrrw_changes_nothing() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, CSRW_SATP_X1 as u64).unwrap();
        bus.store(RAM_BASE + 4, 4, CSRR_X2_SATP as u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);

        // Install / read back one satp value through the two instructions.
        let probe = |cpu: &mut Cpu, bus: &mut Bus<_, _>, v: u64| -> u64 {
            cpu.pc = RAM_BASE;
            cpu.set_reg(1, v);
            cpu.step(bus).unwrap();
            cpu.step(bus).unwrap();
            cpu.reg(2)
        };

        // Sv39 is implemented: the write takes effect.
        let sv39 = (8u64 << 60) | 0x8_0201;
        assert_eq!(probe(&mut cpu, &mut bus, sv39), sv39, "Sv39 must be accepted");

        // Sv48 and Sv57 are not. The whole write is discarded, so the live
        // Sv39 translation is still installed afterwards — MODE, ASID and
        // PPN all untouched.
        for mode in [9u64, 10] {
            let unsupported = (mode << 60) | 0x9_9999;
            let read_back = probe(&mut cpu, &mut bus, unsupported);
            assert_ne!(
                read_back, unsupported,
                "MODE={mode} read back intact; the guest would believe it is supported"
            );
            assert_eq!(
                read_back, sv39,
                "MODE={mode} must be discarded entirely, not coerced — the running \
                 Sv39 address space must survive the probe"
            );
        }

        // Bare is implemented too, and disabling translation must work.
        assert_eq!(probe(&mut cpu, &mut bus, 0), 0, "Bare must be accepted");
        assert_eq!(probe(&mut cpu, &mut bus, sv39), sv39, "…and Sv39 re-installable");
    }

    /// `arch/riscv/mm/init.c:set_satp_mode()`, transcribed:
    ///
    /// ```c
    /// identity_satp = PFN_DOWN((uintptr_t)&early_pg_dir) | satp_mode;
    /// csr_write(CSR_SATP, identity_satp);
    /// hw_satp = csr_swap(CSR_SATP, 0ULL);
    /// if (hw_satp != identity_satp) { /* try the next mode down */ }
    /// ```
    ///
    /// It runs on every 64-bit non-XIP kernel, before `setup_vm` finishes,
    /// and it starts at Sv57. The loop below is that code: same two CSR
    /// instructions, same comparison, same descending order — and the
    /// `csr_swap` writing 0 is what resets `satp` between rounds, exactly as
    /// it does in the kernel.
    ///
    /// Before the clamp this settled on Sv57 on the first round, because the
    /// register file echoed MODE=10 back. The kernel would then build
    /// five-level page tables against an MMU that implements three, and die
    /// on the first jump to a kernel virtual address with no console output
    /// at all. It must settle on Sv39, which is what `guest.dts` advertises.
    #[test]
    fn the_kernels_satp_mode_probe_falls_through_sv57_and_sv48_to_sv39() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, CSRW_SATP_X1 as u64).unwrap();
        bus.store(RAM_BASE + 4, 4, CSRSWAP_X2_SATP_ZERO as u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);

        // PFN_DOWN(&early_pg_dir): any plausible page-table root.
        const ROOT_PFN: u64 = 0x8_0201;
        let mut settled = None;
        for satp_mode in [10u64, 9, 8] {
            let identity_satp = (satp_mode << 60) | ROOT_PFN;
            cpu.pc = RAM_BASE;
            cpu.set_reg(1, identity_satp);
            cpu.step(&mut bus).unwrap(); // csr_write(CSR_SATP, identity_satp)
            cpu.step(&mut bus).unwrap(); // hw_satp = csr_swap(CSR_SATP, 0)
            if cpu.reg(2) == identity_satp {
                settled = Some(satp_mode);
                break;
            }
        }

        assert_eq!(settled, Some(8), "set_satp_mode must fall through to Sv39");
    }

    // --- Fix round 1, Finding 3: accesses straddling a virtual page
    // boundary must translate each half independently, since the two
    // halves can land on unrelated physical pages. ---

    /// A load whose bytes straddle a page boundary must assemble the right
    /// value even when the two virtual pages map to two *non-adjacent*
    /// physical pages — proving the read isn't just forwarding a single
    /// `Bus::load(pa, size)` call across the translated base address.
    #[test]
    fn straddling_load_reads_across_a_page_boundary() {
        let (mut bus, root) = sv39_root();
        let p1 = RAM_BASE + 0x5000;
        let p2 = RAM_BASE + 0x9000; // deliberately not p1 + 0x1000
        map(&mut bus, 0x1000, p1, PTE_V | PTE_R | PTE_W | PTE_AD);
        map(&mut bus, 0x2000, p2, PTE_V | PTE_R | PTE_W | PTE_AD);

        // vaddr 0x1FFE..0x2006: 2 bytes at the tail of page 1, 6 at the
        // head of page 2. Distinct byte values per offset make a wrong
        // assembly (wrong order, wrong page, or a naive contiguous
        // physical read) detectable.
        let bytes = [0x10u8, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17];
        for (i, b) in bytes.iter().enumerate() {
            let pa = if i < 2 { p1 + 0xFFE + i as u64 } else { p2 + (i as u64 - 2) };
            bus.store(pa, 1, *b as u64).unwrap();
        }

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let v = cpu.vload(&mut bus, 0x1FFE, 8).unwrap();
        assert_eq!(v, u64::from_le_bytes(bytes));
    }

    /// Mirror of the load test: a store whose bytes straddle a page
    /// boundary must write each byte to its own translated physical page.
    #[test]
    fn straddling_store_writes_across_a_page_boundary() {
        let (mut bus, root) = sv39_root();
        let p1 = RAM_BASE + 0x5000;
        let p2 = RAM_BASE + 0x9000;
        map(&mut bus, 0x1000, p1, PTE_V | PTE_R | PTE_W | PTE_AD);
        map(&mut bus, 0x2000, p2, PTE_V | PTE_R | PTE_W | PTE_AD);

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let value = 0x0011_2233_4455_6677u64;
        cpu.vstore(&mut bus, 0x1FFE, 8, value).unwrap();

        let mut bytes = [0u8; 8];
        for (i, b) in bytes.iter_mut().enumerate() {
            let pa = if i < 2 { p1 + 0xFFE + i as u64 } else { p2 + (i as u64 - 2) };
            *b = bus.load(pa, 1).unwrap() as u8;
        }
        assert_eq!(u64::from_le_bytes(bytes), value);
    }

    /// A fault in the *second* half of a straddling load must be reported
    /// against the address of the faulting byte itself (0x2000, the second
    /// page's base, since that's the first byte of the unmapped page this
    /// particular access touches) — per the privileged spec, `stval`
    /// carries the address of the portion of a misaligned access that
    /// faulted, not the access's nominal start (0x1FFE). Reporting the
    /// start instead would make the kernel's fault handler look up an
    /// address that's already mapped, resolve nothing, and re-fault
    /// forever.
    #[test]
    fn straddling_load_fault_reports_the_faulting_bytes_address() {
        let (mut bus, root) = sv39_root();
        // Only the first page is mapped; the second (0x2000) is not.
        map(&mut bus, 0x1000, RAM_BASE + 0x5000, PTE_V | PTE_R | PTE_AD);

        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let r = cpu.vload(&mut bus, 0x1FFE, 8);
        assert!(
            matches!(r, Err(Exception::LoadPageFault(0x2000))),
            "fault must carry the faulting byte's address (0x2000, the start \
             of the unmapped second page), not the access's nominal start \
             (0x1FFE): got {r:?}"
        );
    }

    /// rv64c allows a 4-byte (uncompressed) instruction to start at any
    /// 2-byte-aligned pc, including the last two bytes of a page — a real
    /// kernel will do this. The fetch must still decode correctly even
    /// though its two halves live on non-adjacent physical pages.
    #[test]
    fn straddling_instruction_fetch_at_a_page_boundary() {
        let (mut bus, root) = sv39_root();
        let p1 = RAM_BASE + 0x5000;
        let p2 = RAM_BASE + 0x9000;
        map(&mut bus, 0x1000, p1, PTE_V | PTE_R | PTE_X | PTE_AD);
        map(&mut bus, 0x2000, p2, PTE_V | PTE_R | PTE_X | PTE_AD);

        // addi x1, x0, 5 — a full 32-bit instruction, split 2+2 across the
        // page boundary at vaddr 0x1FFE.
        let insn: u32 = 0x0050_0093;
        bus.store(p1 + 0xFFE, 2, (insn & 0xFFFF) as u64).unwrap();
        bus.store(p2, 2, (insn >> 16) as u64).unwrap();

        let mut cpu = Cpu::new(0x1FFE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.reg(1), 5, "the straddling fetch must decode to addi x1,x0,5");
        assert_eq!(cpu.pc, 0x1FFE + 4, "pc must advance past the full straddling instruction");
    }

    /// Mirror of the load-fault test, for fetch: a 4-byte instruction
    /// straddling the page boundary whose *second* half is unmapped must
    /// raise `InstructionPageFault(0x2000)` — the faulting byte's address —
    /// while `pc` (what `mepc`/`sepc` will hold on trap entry) is left
    /// unchanged at 0x1FFE, the instruction's own start. A real trap
    /// handler needs exactly that pair: `mepc` says which instruction to
    /// retry, `mtval` says which page to map first.
    #[test]
    fn straddling_fetch_fault_reports_the_faulting_bytes_address() {
        let (mut bus, root) = sv39_root();
        // Only the first page is mapped; the second (0x2000) is not.
        let p1 = RAM_BASE + 0x5000;
        map(&mut bus, 0x1000, p1, PTE_V | PTE_R | PTE_X | PTE_AD);
        // The 2-byte compressed-vs-full probe reads this (mapped) half, and
        // must see an uncompressed opcode's low bits (…11) to attempt the
        // full 4-byte fetch that then hits the unmapped second page — the
        // low halfword of `addi x1,x0,5`, matching the other straddle-fetch
        // test above.
        bus.store(p1 + 0xFFE, 2, 0x0093).unwrap();

        let mut cpu = Cpu::new(0x1FFE);
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let r = cpu.step(&mut bus);
        assert!(
            matches!(r, Err(Exception::InstructionPageFault(0x2000))),
            "fault must carry the faulting byte's address (0x2000), not the \
             instruction's start (0x1FFE): got {r:?}"
        );
        assert_eq!(cpu.pc, 0x1FFE, "pc (mepc/sepc on trap) must still be the instruction's own start");
    }

    // --- Task 12: CLINT and timer interrupts ---

    /// Sets up a bus with a pending machine timer interrupt (mtimecmp
    /// already reached) and enables it in mie/mstatus.
    fn bus_with_pending_timer() -> Bus<FakeBacking, VecSink> {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.clint.mtimecmp = 10;
        bus.clint.tick(10);
        bus
    }

    /// Nothing asserted `mip` at all, including the branch that *clears*
    /// MTIP — and that branch is what lets a guest acknowledge a timer.
    ///
    /// The acknowledgment protocol is "write a larger `mtimecmp`", not
    /// "clear a flag", so MTIP has to be recomputed from the comparator in
    /// *both* directions on every call, exactly like the wire it models. If
    /// this ever regressed to `mip |= MTIP` — dropping the `else` arm — the
    /// interrupt would refire forever after the first tick and a real kernel
    /// would hang in its timer handler, with the whole suite still green.
    ///
    /// STIP is the other half and is deliberately *not* symmetric: it is the
    /// M-mode firmware's software forward of MTIP, so it is only ever raised
    /// here and is cleared exclusively by `sbi::EXT_SET_TIMER`'s explicit
    /// acknowledgment. Asserting that asymmetry is the point — making STIP
    /// follow the comparator down too would silently swallow a tick the
    /// guest had not yet handled.
    #[test]
    fn check_interrupts_drives_mip_mtip_from_the_comparator_in_both_directions() {
        const MTIP: u64 = 1 << 7;
        const STIP: u64 = 1 << 5;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        let mut cpu = Cpu::new(RAM_BASE);

        // No deadline programmed (mtimecmp resets to u64::MAX).
        cpu.check_interrupts(&mut bus);
        assert_eq!(cpu.csrs.read(csr::MIP) & (MTIP | STIP), 0, "no timer is pending yet");

        bus.clint.mtimecmp = 10;
        bus.clint.tick(10);
        cpu.check_interrupts(&mut bus);
        assert_eq!(cpu.csrs.read(csr::MIP) & MTIP, MTIP, "MTIP is the wire from the comparator");
        assert_eq!(cpu.csrs.read(csr::MIP) & STIP, STIP, "STIP is the forward of that wire");

        // The guest acknowledges by raising mtimecmp.
        bus.clint.mtimecmp = 1000;
        cpu.check_interrupts(&mut bus);
        assert_eq!(
            cpu.csrs.read(csr::MIP) & MTIP,
            0,
            "MTIP must follow the comparator back down, or the timer refires forever"
        );
        assert_eq!(
            cpu.csrs.read(csr::MIP) & STIP,
            STIP,
            "STIP is cleared only by the SBI set_timer acknowledgment, never by this recompute"
        );
    }

    /// `check_interrupts` read-modify-writes the whole of `mip`, so it must
    /// leave every bit it does not own alone. SSIP (bit 1) is guest-writable
    /// through `sip`, and a self-IPI silently dropped by the timer recompute
    /// is a deadlock in whatever was waiting on it.
    #[test]
    fn check_interrupts_preserves_mip_bits_it_does_not_own() {
        const SSIP: u64 = 1 << 1;
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.csrs.write(csr::MIP, SSIP);

        cpu.check_interrupts(&mut bus);

        assert_eq!(cpu.csrs.read(csr::MIP) & SSIP, SSIP);
    }

    /// Taking a timer interrupt must run the *full* trap-entry sequence,
    /// not a stripped-down copy: xIE cleared (or the handler is retaken on
    /// its very first instruction, forever) and xPP recorded (or mret
    /// returns to the wrong privilege). This is the brief's defect A.
    #[test]
    fn timer_interrupt_clears_mie_and_records_mpp() {
        let mut bus = bus_with_pending_timer();
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.csrs.write(csr::MTVEC, RAM_BASE + 0x400);
        cpu.csrs.write(csr::MIE, 1 << 7); // MTIE
        let mstatus = cpu.csrs.read(csr::MSTATUS) | (1 << 3); // MIE=1
        cpu.csrs.write(csr::MSTATUS, mstatus);
        cpu.priv_ = Priv::S; // previous privilege, must land in MPP

        cpu.check_interrupts(&mut bus);

        assert_eq!(cpu.priv_, Priv::M, "must vector into M-mode");
        assert_eq!(cpu.pc, RAM_BASE + 0x400, "must vector to mtvec");
        assert_eq!(cpu.csrs.read(csr::MCAUSE), Interrupt::MachineTimer.cause());
        assert_eq!(cpu.csrs.read(csr::MEPC), RAM_BASE);
        assert_eq!(cpu.csrs.read(csr::MTVAL), 0, "tval is 0 for an interrupt");

        let status = cpu.csrs.read(csr::MSTATUS);
        assert_eq!(status & (1 << 3), 0, "MIE must be cleared on trap entry");
        assert_eq!(status & (1 << 7), 1 << 7, "old MIE must be preserved in MPIE");
        assert_eq!((status >> 11) & 0b11, Priv::S as u64, "MPP must record the interrupted privilege");
    }

    /// `rdtime` (CSR 0xC01) must report the CLINT's `mtime`, not a
    /// permanently-zero slot in the flat CSR array.
    ///
    /// Linux's `riscv_clock_next_event` computes `get_cycles64() + delta`
    /// and hands that to SBI `set_timer`. With `time` stuck at zero, every
    /// re-arm programs `mtimecmp` to a small number that `mtime` has already
    /// passed, so `Clint::pending()` is true again on the very next
    /// instruction and the kernel spins in its timer handler forever — a
    /// livelock, with the CLINT and the whole STIP-forwarding path inert.
    #[test]
    fn rdtime_reads_the_clints_mtime_and_advances_with_it() {
        // rdtime a0 (csrrs a0, time, x0); rdtime a1
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        for (i, w) in [0xC010_2573u32, 0xC010_25F3].iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(RAM_BASE);

        bus.clint.tick(1234);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.reg(10), 1234, "rdtime must read the CLINT's mtime");
        assert_eq!(cpu.reg(10), bus.clint.mtime, "…the same value the CLINT reports");

        bus.clint.tick(1000);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.reg(11), 2234, "rdtime must advance as the CLINT ticks");
        assert_eq!(cpu.reg(11), bus.clint.mtime);
    }

    /// `cycle` (0xC00) and `instret` (0xC02) are driven by the retired-
    /// instruction counter. Linux reads both (`get_cycles`, perf, and the
    /// vDSO), and a constant zero makes any interval computed from them
    /// zero-length.
    #[test]
    fn rdcycle_and_rdinstret_count_retired_instructions() {
        // nop; nop; rdinstret a0; rdcycle a1
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        let prog = [0x0000_0013u32, 0x0000_0013, 0xC020_2573, 0xC000_25F3];
        for (i, w) in prog.iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        let mut cpu = Cpu::new(RAM_BASE);
        for _ in 0..prog.len() {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.reg(10), 2, "two instructions had retired before the rdinstret");
        assert_eq!(cpu.reg(11), 3, "…and three before the rdcycle");
    }

    /// The counters live at `csr_addr[11:10] == 0b11`, so they are
    /// architecturally read-only and a write must trap rather than silently
    /// desynchronise the guest's clock from the CLINT's.
    #[test]
    fn writing_the_time_csr_traps_instead_of_desynchronising_the_clock() {
        // csrrw x0, time, x1 — a write, even with rd == x0.
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0xC010_9073u64).unwrap();
        let mut cpu = Cpu::new(RAM_BASE);
        assert!(matches!(
            cpu.step(&mut bus),
            Err(Exception::IllegalInstruction(_))
        ));
    }

    /// Correction B: `interrupt_enabled` must apply "priv_ < target always
    /// taken, priv_ == target only if that level's xIE bit is set, priv_ >
    /// target never taken" for *both* delegation targets, not just M-mode.
    /// The brief's original condition only ever returned early when
    /// `priv_ == Priv::M`, so an S-mode-targeted interrupt was taken even
    /// with SIE == 0 whenever the CPU happened to be in S-mode.
    #[test]
    fn interrupt_enabled_respects_target_level_ie_bit() {
        let mut cpu = Cpu::new(RAM_BASE);

        // Targeting S-mode (to_s = true):
        cpu.priv_ = Priv::S;
        cpu.csrs.write(csr::MSTATUS, 0); // SIE = 0
        assert!(
            !cpu.interrupt_enabled(true),
            "must NOT be taken: priv_ == target level and SIE is clear"
        );

        cpu.csrs.write(csr::MSTATUS, 1 << 1); // SIE = 1
        assert!(cpu.interrupt_enabled(true), "must be taken: priv_ == target level and SIE is set");

        cpu.priv_ = Priv::U;
        cpu.csrs.write(csr::MSTATUS, 0); // SIE = 0, irrelevant from U
        assert!(
            cpu.interrupt_enabled(true),
            "must be taken: priv_ (U) is strictly below the S-mode target, regardless of SIE"
        );

        cpu.priv_ = Priv::M;
        cpu.csrs.write(csr::MSTATUS, 1 << 1); // SIE = 1, irrelevant from M
        assert!(
            !cpu.interrupt_enabled(true),
            "must NOT be taken: priv_ (M) is above the S-mode target"
        );

        // Targeting M-mode (to_s = false), the path check_interrupts
        // actually uses since MTIP is not delegatable:
        cpu.priv_ = Priv::M;
        cpu.csrs.write(csr::MSTATUS, 0); // MIE = 0
        assert!(!cpu.interrupt_enabled(false), "must NOT be taken: priv_ == M and MIE is clear");

        cpu.priv_ = Priv::S;
        assert!(
            cpu.interrupt_enabled(false),
            "must be taken: priv_ (S) is strictly below the M-mode target, regardless of MIE"
        );
    }

    /// End-to-end version of the previous test through the real delivery
    /// path: from M-mode with MIE clear the pending timer must not be
    /// taken; from S-mode (a lower privilege than the timer's only
    /// possible target, M) it must be taken even though mstatus.MIE (an
    /// M-mode-only bit) was never set.
    #[test]
    fn check_interrupts_honors_target_level_enable_rule() {
        let mut bus = bus_with_pending_timer();
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.csrs.write(csr::MTVEC, RAM_BASE + 0x400);
        cpu.csrs.write(csr::MIE, 1 << 7); // MTIE

        // M-mode, MIE clear: must not be taken.
        cpu.priv_ = Priv::M;
        cpu.check_interrupts(&mut bus);
        assert_eq!(cpu.pc, RAM_BASE, "must not be taken: M-mode with MIE clear");
        assert_eq!(cpu.priv_, Priv::M);

        // S-mode: below the timer's M-mode target, so taken unconditionally.
        cpu.priv_ = Priv::S;
        cpu.check_interrupts(&mut bus);
        assert_eq!(cpu.priv_, Priv::M, "must be taken from S-mode regardless of mstatus.MIE");
        assert_eq!(cpu.pc, RAM_BASE + 0x400);
    }

    /// Guards against reintroducing defect C: the CLINT dispatch in `Bus`
    /// must keep the size-inclusive bound, not just check the starting
    /// address. Exercised here through the CPU/MMU path (physical == virtual
    /// with no SATP set) to confirm the wiring holds up end to end, not just
    /// at the `Bus` unit level (see `bus::tests::oversized_access_at_end_of_clint_window_faults`).
    #[test]
    fn vstore_past_end_of_clint_window_faults_not_silently_truncated() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        let mut cpu = Cpu::new(RAM_BASE);
        let addr = crate::bus::CLINT_BASE + crate::bus::CLINT_SIZE - 4;
        let r = cpu.vstore(&mut bus, addr, 8, 0);
        assert!(matches!(r, Err(Exception::StoreAccessFault(_))));
    }

    // --- Task 14: SBI stub ---

    /// Defect B: `step_trapping` must resume an `ecall` at `pc + insn_len`,
    /// never a hard-coded `4`. ECALL has no compressed form, so both give
    /// the same numeric answer here — the point is that the code path goes
    /// through `insn_len`, matching the rule carried from Task 9 that
    /// nothing may assume a fixed instruction width.
    #[test]
    fn sbi_ecall_resumes_at_pc_plus_insn_len_not_a_literal_four() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x0000_0073u64).unwrap(); // ecall
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.set_reg(17, 0xDEAD_BEEF); // unknown extension: handled, no side effects

        let outcome = cpu.step_trapping(&mut bus);

        assert_eq!(outcome, Ok(crate::sbi::SbiOutcome::Handled));
        assert_eq!(cpu.pc, RAM_BASE + cpu.insn_len, "must resume via insn_len, not a literal 4");
        assert_eq!(cpu.pc, RAM_BASE + 4);
    }

    /// A backing-store failure is not a RISC-V exception, so it must not be
    /// delivered to the guest — and it must not panic either. This crate is
    /// destined for a Xous app where a panic aborts the host process with no
    /// recovery, so `step_trapping` reports it as `Err(addr)` and the caller
    /// decides.
    ///
    /// The negative half is the point: `Exception::BackingFailure(_).cause()`
    /// is 5, which aliases `LoadAccessFault` — a cause `Csrs::default`'s
    /// `medeleg` delegates to S-mode. If this arm ever fell through to the
    /// catch-all `cpu.trap(e)` below it, the guest would be handed a
    /// spurious load access fault and the run would continue on a machine
    /// whose memory is broken. `priv_` is unchanged here precisely because
    /// `trap` was never reached.
    #[test]
    fn step_trapping_reports_a_backing_failure_instead_of_panicking_or_trapping_it() {
        let mut bus = Bus::new(PageCache::new(FailingBacking, 4), VecSink::default());
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;

        let r = cpu.step_trapping(&mut bus);

        assert!(r.is_err(), "expected Err(addr) for a backing failure, got {r:?}");
        assert_eq!(cpu.priv_, Priv::S, "must not have been delivered as a guest trap");
        assert_eq!(cpu.csrs.read(csr::SCAUSE), 0, "no trap was entered");
    }

    /// `EnvironmentCallFromSMode` must be routed to the SBI stub instead of
    /// being delivered as an ordinary trap: `SCAUSE` must stay untouched
    /// and the guest must simply resume past the `ecall`, in S-mode.
    #[test]
    fn ecall_from_s_mode_is_routed_to_sbi_not_trapped() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 32), VecSink::default());
        bus.store(RAM_BASE, 4, 0x0000_0073u64).unwrap(); // ecall
        let mut cpu = Cpu::new(RAM_BASE);
        cpu.priv_ = Priv::S;
        cpu.set_reg(17, crate::sbi::EXT_SHUTDOWN);

        let outcome = cpu.step_trapping(&mut bus);

        assert_eq!(outcome, Ok(crate::sbi::SbiOutcome::Shutdown));
        assert_eq!(cpu.priv_, Priv::S, "SBI handling must not enter a trap / change privilege");
        assert_eq!(cpu.csrs.read(csr::SCAUSE), 0, "SBI handling must not write scause");
    }

    // --- Task 14 fix round 1: firmware CSR init (Finding 1) ---

    /// This emulator plays the M-mode firmware role directly rather than
    /// executing an actual OpenSBI image (see `sbi.rs`), so nothing ever
    /// runs the M-mode boot code that would normally program `mideleg` and
    /// `medeleg` before jumping to the S-mode kernel. Without them, a
    /// guest's write to `sie`/`sip` (masked by `mideleg`) is silently
    /// discarded and every exception it raises — including ordinary page
    /// faults — vectors through `mtvec` (0 for this guest) instead of
    /// `stvec`. Both are silent hangs, not crashes, so `Cpu::new` must set
    /// these at reset, and this test exists so that a later change which
    /// drops that init fails loudly here instead of silently un-booting the
    /// guest.
    #[test]
    fn reset_delegates_the_standard_s_mode_interrupts_and_exceptions() {
        let cpu = Cpu::new(RAM_BASE);

        let mideleg = cpu.csrs.read(csr::MIDELEG);
        assert_eq!(
            mideleg,
            (1 << 1) | (1 << 5) | (1 << 9),
            "mideleg must delegate exactly SSIP(1)/STIP(5)/SEIP(9) — the only \
             interrupts that are architecturally delegable"
        );

        let medeleg = cpu.csrs.read(csr::MEDELEG);
        let expected = (1 << Exception::InstructionAddressMisaligned(0).cause())
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
        assert_eq!(medeleg, expected, "medeleg must delegate the standard S-mode exception set");
        assert_eq!(
            medeleg & (1 << Exception::EnvironmentCallFromSMode.cause()),
            0,
            "ECALL-from-S-mode (cause 9) must NOT be delegated — that is the SBI \
             call itself and must stay addressed to M-mode"
        );
    }
}
