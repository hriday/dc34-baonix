//! Automated check for the guest device tree (Task 18).
//!
//! This is a data file, not code, so it does not fit the project's usual
//! "test fails before the fix, passes after" shape. What can still be
//! checked automatically, and is checked here:
//!
//!  1. `nix/guest/guest.dts` actually compiles with `dtc`.
//!  2. The result is a valid flattened device tree — the FDT magic
//!     (`0xd00d_feed`, big-endian) is the first four bytes.
//!  3. The CLI runner's own `--dtb` loader (`rv64_host::write_blob`, the
//!     function `main.rs` calls to place `--dtb` above the loaded kernel)
//!     accepts the compiled bytes without error.
//!  4. Every address and size the tree hands the guest matches the constant
//!     the emulator actually implements it at. `guest.dts` duplicates four
//!     addresses and three sizes from Rust by hand, in a data file the
//!     compiler cannot check. Change `RAM_SIZE` and forget the DTS and the
//!     guest gets a memory node describing RAM that does not exist — the
//!     kernel then fails while telling you about something else entirely,
//!     which is the hardest failure of all to diagnose. This is the guard
//!     that makes changing those constants safe.
//!
//! What this does *not* prove: that Linux parses the tree correctly, or
//! that any node's contents are semantically right for the kernel's
//! drivers. There is no kernel to boot yet (Tasks 19-21), so that is not
//! checkable here — a decompile a human reads once (the brief's Step 3) is
//! the strongest check available for the parts of the tree that have no
//! counterpart in Rust (`compatible` strings, `bootargs`, `no-loopback-test`).

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Whether `dtc` is on `PATH`. Matches how `rv64-difftest` skips its Spike
/// comparison and `riscv_tests.rs` skips the ISA suite when their external
/// dependency is unavailable — loud, and only outside `nix develop`, where
/// `dtc` lives.
fn dtc_available() -> bool {
    Command::new("dtc").arg("--version").output().is_ok()
}

/// `crates/rv64-host` -> `crates` -> repo root, where `nix/guest/guest.dts`
/// lives.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().expect("truncated FDT"))
}

/// Reads the value of property `name` on node `path` (a full path such as
/// `/soc/serial@10000000`) out of a flattened device tree.
///
/// Walking the blob rather than string-matching `dtc -I dtb -O dts` output
/// is deliberate: the decompiler's formatting of a `reg` cell list is a
/// property of the dtc version, and a test that breaks when the pinned
/// toolchain moves is a test people delete. The struct block is a flat
/// token stream (§5.4 of the DT spec) and is trivial to walk.
///
/// The walk itself now lives in `rv64_host::fdt`, because the CLI runner
/// needs the same lookup to patch `linux,initrd-start`/`-end` before
/// handing the tree to the guest. Calling it from here rather than keeping
/// a second copy is the same argument `write_blob`'s doc comment makes: a
/// test that exercises a hand-copy of the runner's logic stops being
/// evidence about the runner the moment the two drift.
fn fdt_prop(blob: &[u8], path: &str, name: &str) -> Option<Vec<u8>> {
    rv64_host::fdt::prop(blob, path, name).map(<[u8]>::to_vec)
}

/// Decodes a `reg` value under `#address-cells = <2>; #size-cells = <2>`
/// (what `guest.dts` declares on both the root and the `soc` bus) into the
/// `(address, size)` pair it names.
fn reg_pair(blob: &[u8], path: &str) -> (u64, u64) {
    let v = fdt_prop(blob, path, "reg")
        .unwrap_or_else(|| panic!("the device tree has no `reg` on node {path}"));
    assert_eq!(v.len(), 16, "{path}: expected 4 cells (#address-cells=2, #size-cells=2)");
    let cell = |i: usize| be32(&v, i * 4) as u64;
    ((cell(0) << 32) | cell(1), (cell(2) << 32) | cell(3))
}

/// Compiles `nix/guest/guest.dts` with `dtc` and returns the blob. Shared
/// by every test here so each one checks the same artifact the flake's
/// `packages.dtb` builds, rather than a fixture that could drift into
/// describing a different machine.
///
/// Compiled **once per process** behind a `OnceLock`, which is a
/// correctness requirement and not an optimization. libtest runs the tests
/// in this file concurrently in one process; when each of them ran its own
/// `dtc -o <same path>`, one test would read the file while another's `dtc`
/// had it truncated. That was measured at 29 failures in 200 runs, surfacing
/// as `bad magic` or a missing `/chosen` property — a suite that fails one
/// run in seven teaches people to re-run until green, which is how a real
/// failure gets waved through.
///
/// The path still carries the pid so that two concurrent `cargo test`
/// processes cannot collide either.
fn compile_guest_dtb() -> Vec<u8> {
    static DTB: OnceLock<Vec<u8>> = OnceLock::new();
    DTB.get_or_init(|| {
        let dts = repo_root().join("nix/guest/guest.dts");
        assert!(dts.is_file(), "{} not found", dts.display());

        let out_dir = std::env::temp_dir().join(format!("rv64-dtb-check-{}", std::process::id()));
        std::fs::create_dir_all(&out_dir).unwrap();
        let dtb_path = out_dir.join("guest.dtb");

        let status = Command::new("dtc")
            .arg("-I")
            .arg("dts")
            .arg("-O")
            .arg("dtb")
            .arg("-o")
            .arg(&dtb_path)
            .arg(&dts)
            .status()
            .expect("could not run dtc");
        assert!(status.success(), "dtc failed to compile {}", dts.display());
        std::fs::read(&dtb_path).unwrap()
    })
    .clone()
}

#[test]
fn guest_dtb_compiles_and_is_loadable_by_the_cli_runner() {
    if !dtc_available() {
        rv64_host::suite_prerequisite_missing(
            "guest_dtb_compiles_and_is_loadable_by_the_cli_runner",
            "dtc is not on PATH",
        );
        return;
    }

    let bytes = compile_guest_dtb();

    // FDT magic: 0xd00dfeed, stored big-endian, at offset 0. This is the
    // one structural fact any FDT consumer (this emulator's runner, or a
    // real bootloader) checks before touching the rest of the blob.
    assert_eq!(
        bytes.get(..4),
        Some(0xd00d_feedu32.to_be_bytes().as_slice()),
        "not a valid flattened device tree: bad magic"
    );

    // Prove the CLI runner's `--dtb` loader accepts the compiled bytes: the
    // same `write_blob` call `main.rs` makes to place `--dtb` in guest
    // memory. `RAM_SIZE` bytes of `FakeBacking` easily holds a DTB this
    // small (well under a page), so this only exercises the loader, not
    // memory-sizing edge cases `write_blob`'s own unit test already covers.
    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let mut bus = rv64::Bus::new(
        rv64::PageCache::new(rv64::backing::FakeBacking::new(pages), 64),
        rv64::uart::VecSink::default(),
    );
    rv64_host::write_blob(&mut bus, rv64::RAM_BASE, &bytes)
        .expect("the CLI runner's DTB loader rejected the compiled device tree");

    // Every address and size the guest is told about must be the one this
    // emulator actually implements. The node *names* are derived from the
    // constants too, so a moved base address fails here rather than
    // silently leaving a correctly-named node describing the wrong window.
    assert_eq!(
        reg_pair(&bytes, &format!("/memory@{:x}", rv64::RAM_BASE)),
        (rv64::RAM_BASE, rv64::RAM_SIZE),
        "the guest's memory node must describe the RAM `Bus` actually backs"
    );
    assert_eq!(
        reg_pair(&bytes, &format!("/soc/clint@{:x}", rv64::bus::CLINT_BASE)),
        (rv64::bus::CLINT_BASE, rv64::bus::CLINT_SIZE),
        "the CLINT node must name the window `Bus::is_clint` decodes"
    );
    assert_eq!(
        reg_pair(&bytes, &format!("/soc/serial@{:x}", rv64::bus::UART_BASE)),
        (rv64::bus::UART_BASE, rv64::bus::UART_SIZE),
        "the serial node must name the window `Bus::is_uart` decodes"
    );

    // `stdout-path` is a free-standing string literal in guest.dts, not a
    // reference `dtc` resolves — nothing checks that it actually points at
    // the serial node above except this. A `stdout-path` that dangles
    // (because `UART_BASE` moved and the node name above followed it, but
    // this literal did not) is the console-never-registers failure
    // guest.dts's own `no-loopback-test` comment calls the worst mode
    // available to this project: a totally silent boot with no diagnostic
    // at all.
    let stdout_path =
        fdt_prop(&bytes, "/chosen", "stdout-path").expect("/chosen has no `stdout-path`");
    let expected_stdout_path = format!("/soc/serial@{:x}", rv64::bus::UART_BASE);
    assert_eq!(
        stdout_path,
        [expected_stdout_path.as_bytes(), b"\0"].concat(),
        "stdout-path must name the serial node above (derived from UART_BASE), or the \
         console never registers"
    );
}

/// The runner's `--initrd` support patches `/chosen` in place rather than
/// rebuilding the FDT, which only works if `guest.dts` actually ships the
/// two properties at the width `rv64_host::fdt::set_u64` requires. A DTS
/// edit that dropped or narrowed them would leave `--initrd` failing at
/// run time with no compile-time warning, so check the compiled artifact.
///
/// Also pins the placeholders at zero. That is what makes a run *without*
/// `--initrd` safe: `early_init_dt_check_for_initrd()` computes
/// `phys_initrd_size = end - start`, and `reserve_initrd_mem()` returns
/// immediately when that is zero. A non-zero placeholder would point the
/// kernel at an initrd that is not there.
#[test]
fn chosen_carries_patchable_two_cell_initrd_properties() {
    if !dtc_available() {
        rv64_host::suite_prerequisite_missing(
            "chosen_carries_patchable_two_cell_initrd_properties",
            "dtc is not on PATH",
        );
        return;
    }
    let mut bytes = compile_guest_dtb();

    for name in ["linux,initrd-start", "linux,initrd-end"] {
        let v = fdt_prop(&bytes, "/chosen", name)
            .unwrap_or_else(|| panic!("/chosen has no `{name}`; --initrd cannot be recorded"));
        assert_eq!(v.len(), 8, "`{name}` must be two cells for a 64-bit address");
        assert_eq!(v, vec![0u8; 8], "`{name}` placeholder must be inert when unset");
    }

    // And the patch the runner performs must round-trip, at the exact
    // total size dtc produced — an FDT whose header no longer matches its
    // contents would be rejected by the guest, not silently tolerated.
    let before = bytes.len();
    rv64_host::fdt::set_u64(&mut bytes, "/chosen", "linux,initrd-start", 0x8060_0000).unwrap();
    rv64_host::fdt::set_u64(&mut bytes, "/chosen", "linux,initrd-end", 0x8061_2345).unwrap();
    assert_eq!(bytes.len(), before, "patching must not resize the blob");
    assert_eq!(
        fdt_prop(&bytes, "/chosen", "linux,initrd-start").unwrap(),
        0x8060_0000u64.to_be_bytes()
    );
    assert_eq!(
        fdt_prop(&bytes, "/chosen", "linux,initrd-end").unwrap(),
        0x8061_2345u64.to_be_bytes()
    );
}

/// `rng-seed` is credited in full by `add_bootloader_randomness()`, so an
/// empty or missing property is the difference between the CRNG being ready
/// at boot and userspace `getrandom()` blocking on an emulator with no
/// hardware entropy source.
#[test]
fn chosen_carries_a_non_empty_rng_seed() {
    if !dtc_available() {
        rv64_host::suite_prerequisite_missing(
            "chosen_carries_a_non_empty_rng_seed",
            "dtc is not on PATH",
        );
        return;
    }
    let bytes = compile_guest_dtb();
    let seed = fdt_prop(&bytes, "/chosen", "rng-seed").expect("/chosen has no `rng-seed`");
    assert!(seed.len() >= 16, "rng-seed is only {} bytes", seed.len());
    assert!(seed.iter().any(|&b| b != 0), "an all-zero rng-seed credits no entropy");
}
