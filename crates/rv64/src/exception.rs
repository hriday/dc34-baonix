#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    InstructionAddressMisaligned(u64),
    InstructionAccessFault(u64),
    IllegalInstruction(u64),
    Breakpoint,
    LoadAddressMisaligned(u64),
    LoadAccessFault(u64),
    StoreAddressMisaligned(u64),
    StoreAccessFault(u64),
    EnvironmentCallFromUMode,
    EnvironmentCallFromSMode,
    EnvironmentCallFromMMode,
    InstructionPageFault(u64),
    LoadPageFault(u64),
    StorePageFault(u64),
    /// The backing store failed. Not a RISC-V exception — the run loop must
    /// abort on this rather than delivering it to the guest. Carries the guest
    /// physical address of the failed access for diagnostics.
    BackingFailure(u64),
}

impl Exception {
    /// `scause` / `mcause` value, per the privileged spec.
    pub fn cause(&self) -> u64 {
        use Exception::*;
        match self {
            InstructionAddressMisaligned(_) => 0,
            InstructionAccessFault(_) => 1,
            IllegalInstruction(_) => 2,
            Breakpoint => 3,
            LoadAddressMisaligned(_) => 4,
            LoadAccessFault(_) => 5,
            StoreAddressMisaligned(_) => 6,
            StoreAccessFault(_) => 7,
            EnvironmentCallFromUMode => 8,
            EnvironmentCallFromSMode => 9,
            EnvironmentCallFromMMode => 11,
            InstructionPageFault(_) => 12,
            LoadPageFault(_) => 13,
            StorePageFault(_) => 15,
            // BackingFailure is a placeholder that must never reach guest trap delivery
            BackingFailure(_) => 5,
        }
    }

    /// `stval` / `mtval` value.
    pub fn tval(&self) -> u64 {
        use Exception::*;
        match self {
            InstructionAddressMisaligned(v)
            | InstructionAccessFault(v)
            | IllegalInstruction(v)
            | LoadAddressMisaligned(v)
            | LoadAccessFault(v)
            | StoreAddressMisaligned(v)
            | StoreAccessFault(v)
            | InstructionPageFault(v)
            | LoadPageFault(v)
            | StorePageFault(v)
            | BackingFailure(v) => *v,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    SupervisorSoftware,
    SupervisorTimer,
    SupervisorExternal,
    MachineSoftware,
    MachineTimer,
    MachineExternal,
}

impl Interrupt {
    pub fn cause(&self) -> u64 {
        use Interrupt::*;
        let code = match self {
            SupervisorSoftware => 1,
            MachineSoftware => 3,
            SupervisorTimer => 5,
            MachineTimer => 7,
            SupervisorExternal => 9,
            MachineExternal => 11,
        };
        code | (1u64 << 63)
    }
}
