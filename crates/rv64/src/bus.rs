use crate::backing::MemBacking;
use crate::cache::PageCache;
use crate::clint::Clint;
use crate::exception::Exception;
use crate::uart::{ConsoleSink, Uart};
use crate::{RAM_BASE, RAM_SIZE};

pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x0001_0000;
pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;

/// Routes guest physical accesses to RAM (via the page cache), the CLINT, or
/// the UART.
pub struct Bus<B: MemBacking, S: ConsoleSink> {
    cache: PageCache<B>,
    pub clint: Clint,
    pub uart: Uart<S>,
}

impl<B: MemBacking, S: ConsoleSink> Bus<B, S> {
    pub fn new(cache: PageCache<B>, sink: S) -> Self {
        Self { cache, clint: Clint::default(), uart: Uart::new(sink) }
    }

    pub fn cache_mut(&mut self) -> &mut PageCache<B> {
        &mut self.cache
    }

    fn in_ram(addr: u64, size: u8) -> bool {
        addr >= RAM_BASE && addr.saturating_add(size as u64) <= RAM_BASE + RAM_SIZE
    }

    /// Bounds-checks the *whole* access, not just its starting address — an
    /// 8-byte access starting in the last 4 bytes of the CLINT window must
    /// be rejected rather than silently truncated or read out of range.
    fn is_clint(addr: u64, size: u8) -> bool {
        let end = addr.saturating_add(size as u64);
        addr >= CLINT_BASE && end <= CLINT_BASE + CLINT_SIZE
    }

    /// Same whole-access bound as `is_clint`, applied to the UART window.
    fn is_uart(addr: u64, size: u8) -> bool {
        let end = addr.saturating_add(size as u64);
        addr >= UART_BASE && end <= UART_BASE + UART_SIZE
    }

    pub fn load(&mut self, addr: u64, size: u8) -> Result<u64, Exception> {
        if !matches!(size, 1 | 2 | 4 | 8) {
            return Err(Exception::LoadAccessFault(addr));
        }
        if Self::in_ram(addr, size) {
            // Dispatched to a const width so the cache's copy is a machine
            // load rather than a `memmove` call. `size` is one of 1, 2, 4, 8
            // by the guard above, which is what makes the last arm exact
            // rather than a default.
            let v = match size {
                1 => self.cache.read_le::<1>(addr),
                2 => self.cache.read_le::<2>(addr),
                4 => self.cache.read_le::<4>(addr),
                _ => self.cache.read_le::<8>(addr),
            };
            return v.map_err(|_| Exception::BackingFailure(addr));
        }
        if Self::is_clint(addr, size) {
            return Ok(self.clint.load(addr - CLINT_BASE));
        }
        if Self::is_uart(addr, size) {
            return Ok(self.uart.load(addr - UART_BASE));
        }
        Err(Exception::LoadAccessFault(addr))
    }

    pub fn store(&mut self, addr: u64, size: u8, value: u64) -> Result<(), Exception> {
        if !matches!(size, 1 | 2 | 4 | 8) {
            return Err(Exception::StoreAccessFault(addr));
        }
        if Self::in_ram(addr, size) {
            // Same const-width dispatch as `load`; see there.
            let r = match size {
                1 => self.cache.write_le::<1>(addr, value),
                2 => self.cache.write_le::<2>(addr, value),
                4 => self.cache.write_le::<4>(addr, value),
                _ => self.cache.write_le::<8>(addr, value),
            };
            return r.map_err(|_| Exception::BackingFailure(addr));
        }
        if Self::is_clint(addr, size) {
            self.clint.store(addr - CLINT_BASE, size, value);
            return Ok(());
        }
        if Self::is_uart(addr, size) {
            self.uart.store(addr - UART_BASE, value);
            return Ok(());
        }
        Err(Exception::StoreAccessFault(addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backing::FakeBacking;
    use crate::cache::PageCache;
    use crate::uart::VecSink;
    use crate::RAM_BASE;

    fn bus() -> Bus<FakeBacking, VecSink> {
        Bus::new(PageCache::new(FakeBacking::new(64), 8), VecSink::default())
    }

    #[test]
    fn stores_and_loads_are_little_endian() {
        let mut b = bus();
        b.store(RAM_BASE, 8, 0x0102_0304_0506_0708).unwrap();
        assert_eq!(b.load(RAM_BASE, 1).unwrap(), 0x08);
        assert_eq!(b.load(RAM_BASE, 2).unwrap(), 0x0708);
        assert_eq!(b.load(RAM_BASE, 4).unwrap(), 0x0506_0708);
        assert_eq!(b.load(RAM_BASE, 8).unwrap(), 0x0102_0304_0506_0708);
    }

    #[test]
    fn load_below_ram_faults() {
        let mut b = bus();
        assert!(matches!(b.load(0x1000, 4), Err(Exception::LoadAccessFault(_))));
    }

    #[test]
    fn store_above_ram_faults() {
        let mut b = bus();
        let past = RAM_BASE + crate::RAM_SIZE;
        assert!(matches!(b.store(past, 4, 0), Err(Exception::StoreAccessFault(_))));
    }

    #[test]
    fn exception_cause_codes_match_the_spec() {
        assert_eq!(Exception::InstructionAddressMisaligned(0).cause(), 0);
        assert_eq!(Exception::InstructionAccessFault(0).cause(), 1);
        assert_eq!(Exception::IllegalInstruction(0).cause(), 2);
        assert_eq!(Exception::Breakpoint.cause(), 3);
        assert_eq!(Exception::LoadAddressMisaligned(0).cause(), 4);
        assert_eq!(Exception::LoadAccessFault(0).cause(), 5);
        assert_eq!(Exception::StoreAddressMisaligned(0).cause(), 6);
        assert_eq!(Exception::StoreAccessFault(0).cause(), 7);
        assert_eq!(Exception::EnvironmentCallFromUMode.cause(), 8);
        assert_eq!(Exception::EnvironmentCallFromSMode.cause(), 9);
        assert_eq!(Exception::EnvironmentCallFromMMode.cause(), 11);
        assert_eq!(Exception::InstructionPageFault(0).cause(), 12);
        assert_eq!(Exception::LoadPageFault(0).cause(), 13);
        assert_eq!(Exception::StorePageFault(0).cause(), 15);
    }

    #[test]
    fn illegal_access_width_faults_instead_of_panicking() {
        let mut b = bus();
        assert!(matches!(b.load(RAM_BASE, 9), Err(Exception::LoadAccessFault(_))));
        assert!(matches!(b.load(RAM_BASE, 3), Err(Exception::LoadAccessFault(_))));
        assert!(matches!(b.store(RAM_BASE, 0, 0), Err(Exception::StoreAccessFault(_))));
    }

    #[test]
    fn backing_failure_preserves_the_faulting_address() {
        assert_eq!(Exception::BackingFailure(0x8000_1234).tval(), 0x8000_1234);
    }

    #[test]
    fn clint_mtimecmp_round_trips_through_the_bus() {
        let mut b = bus();
        b.store(CLINT_BASE + crate::clint::MTIMECMP_OFF, 8, 0x1234).unwrap();
        assert_eq!(b.load(CLINT_BASE + crate::clint::MTIMECMP_OFF, 8).unwrap(), 0x1234);
    }

    /// An 8-byte access starting in the last 4 bytes of the CLINT window
    /// spills past CLINT_BASE + CLINT_SIZE and must fault, not be silently
    /// accepted — Task 4 added this bound to what was then called `is_mmio`
    /// (renamed `is_uart` in Task 13) deliberately, and rewiring the CLINT
    /// branch must not lose it.
    #[test]
    fn oversized_access_at_end_of_clint_window_faults() {
        let mut b = bus();
        let addr = CLINT_BASE + CLINT_SIZE - 4;
        assert!(matches!(b.load(addr, 8), Err(Exception::LoadAccessFault(_))));
        assert!(matches!(b.store(addr, 8, 0), Err(Exception::StoreAccessFault(_))));
    }

    #[test]
    fn uart_thr_round_trips_through_the_bus() {
        let mut b = bus();
        b.store(UART_BASE + crate::uart::THR, 1, b'Q' as u64).unwrap();
        assert_eq!(b.uart.sink.bytes, b"Q");
    }

    /// Mirror of `oversized_access_at_end_of_clint_window_faults` for the
    /// UART window: Task 13's brief dropped the size-inclusive bound a third
    /// time (`addr >= UART_BASE && addr < UART_BASE + UART_SIZE`, no end
    /// check), which this test guards against regressing.
    #[test]
    fn oversized_access_at_end_of_uart_window_faults() {
        let mut b = bus();
        let addr = UART_BASE + UART_SIZE - 4;
        assert!(matches!(b.load(addr, 8), Err(Exception::LoadAccessFault(_))));
        assert!(matches!(b.store(addr, 8, 0), Err(Exception::StoreAccessFault(_))));
    }
}
