//! The badge side of the tethered emulator.
//!
//! The laptop holds the guest's 32 MiB of RAM in a flat file and answers page
//! requests over USB-CDC (`rv64-host serve`); this crate is what asks. See
//! [`usbhost`] for the transport and the hardware constraints it is shaped by,
//! and [`oled`] for the other end of the guest -- its console, on the badge's
//! 128x128 display.
//!
//! [`run`] is what joins them: the emulator, the transport and the console in
//! one loop, with nothing platform-specific in it. `src/main.rs` is the badge's
//! platform leaf and `tests/dry_run.rs` is the laptop's, and they run the same
//! [`run`].

pub mod oled;
pub mod run;
pub mod startup;
pub mod usbhost;
