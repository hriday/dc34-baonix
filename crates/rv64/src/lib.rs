#![cfg_attr(not(test), no_std)]

/// Guest page size, fixed everywhere in this crate.
pub const PAGE: usize = 4096;

/// Guest physical address of the first byte of RAM.
pub const RAM_BASE: u64 = 0x8000_0000;

/// Guest RAM size in bytes (32 MiB).
pub const RAM_SIZE: u64 = 0x0200_0000;

pub mod backing;
pub use backing::{Error, MemBacking};

pub mod cache;
pub use cache::{PageCache, Stats};

pub mod bus;
pub mod clint;
pub use clint::Clint;
pub mod exception;
pub use bus::Bus;
pub use exception::{Exception, Interrupt};

pub mod cpu;
pub mod csr;
pub mod insn;
pub use cpu::Cpu;

pub mod mmu;
pub use mmu::{Access, Mmu};

pub mod sbi;

pub mod uart;
pub use uart::{ConsoleSink, Uart};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_consistent() {
        assert_eq!(PAGE, 4096);
        assert_eq!(RAM_SIZE % PAGE as u64, 0);
    }
}
