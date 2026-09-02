//! SiFive-style CLINT (core-local interruptor): `msip`, `mtimecmp`, `mtime`.
//! Only the machine timer is wired to interrupt delivery in this task —
//! `msip` (inter-hart software interrupts) is modeled for completeness but
//! nothing yet reads `Interrupt::MachineSoftware`.

pub const MSIP_OFF: u64 = 0x0000;
pub const MTIMECMP_OFF: u64 = 0x4000;
pub const MTIME_OFF: u64 = 0xBFF8;

pub struct Clint {
    pub mtime: u64,
    pub mtimecmp: u64,
    pub msip: u32,
}

impl Default for Clint {
    /// `mtimecmp` defaults to `u64::MAX`, not 0: real hardware resets it to
    /// an implementation-defined value, and 0 would make `pending()` true
    /// from the moment `mtime` starts advancing, before any guest has
    /// programmed a deadline. `u64::MAX` also lets a guest legitimately
    /// program `mtimecmp = 0` (fire immediately) without a special case in
    /// `pending()`.
    fn default() -> Self {
        Self { mtime: 0, mtimecmp: u64::MAX, msip: 0 }
    }
}

impl Clint {
    pub fn tick(&mut self, n: u64) {
        self.mtime = self.mtime.wrapping_add(n);
    }

    /// Machine timer interrupt is pending while mtime >= mtimecmp. The guest
    /// acknowledges it by writing a larger mtimecmp, not by clearing a flag.
    pub fn pending(&self) -> bool {
        self.mtime >= self.mtimecmp
    }

    pub fn load(&self, off: u64) -> u64 {
        match off {
            MSIP_OFF => self.msip as u64,
            MTIMECMP_OFF => self.mtimecmp,
            MTIME_OFF => self.mtime,
            _ => 0,
        }
    }

    /// `size` is accepted for interface symmetry with `Bus::store` but
    /// otherwise ignored: an RV64 guest (and the OpenSBI/Linux stack this
    /// emulator targets) always writes `mtimecmp` and `mtime` as a single
    /// aligned 8-byte store, and `msip` as a single 4-byte store. There is
    /// no guest in scope that performs a narrower, byte-level read-modify-
    /// write of these registers, so partial-width store semantics are not
    /// modeled.
    pub fn store(&mut self, off: u64, _size: u8, v: u64) {
        match off {
            MSIP_OFF => self.msip = v as u32,
            MTIMECMP_OFF => self.mtimecmp = v,
            MTIME_OFF => self.mtime = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtime_advances_with_ticks() {
        let mut c = Clint::default();
        assert_eq!(c.load(MTIME_OFF), 0);
        c.tick(100);
        assert_eq!(c.load(MTIME_OFF), 100);
    }

    #[test]
    fn timer_is_not_pending_before_mtimecmp() {
        let mut c = Clint::default();
        c.store(MTIMECMP_OFF, 8, 1000);
        c.tick(999);
        assert!(!c.pending());
    }

    #[test]
    fn timer_fires_when_mtime_reaches_mtimecmp() {
        let mut c = Clint::default();
        c.store(MTIMECMP_OFF, 8, 1000);
        c.tick(1000);
        assert!(c.pending());
    }

    #[test]
    fn raising_mtimecmp_clears_the_pending_timer() {
        let mut c = Clint::default();
        c.store(MTIMECMP_OFF, 8, 10);
        c.tick(20);
        assert!(c.pending());
        c.store(MTIMECMP_OFF, 8, 100);
        assert!(!c.pending(), "writing mtimecmp is how the guest acks the timer");
    }
}
