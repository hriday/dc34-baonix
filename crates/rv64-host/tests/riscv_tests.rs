//! Conformance against the official `riscv-tests` ISA suite.
//!
//! Each binary is an ELF linked at `RAM_BASE` that runs to completion and
//! reports its verdict by storing to the `tohost` symbol: `1` means pass,
//! and any other value encodes the failing sub-test as `(n << 1) | 1`.
//!
//! The binaries come from the pinned `riscv-tests` derivation
//! (`nix/riscv-tests.nix`); `RISCV_TESTS` points at the install directory
//! and is set by the devShell.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Suites this emulator is expected to pass in full, keyed by the filename
/// prefix the derivation installs. `ui`/`um`/`ua`/`uc` are the user-mode
/// I, M, A and C suites (Tasks 5-9); `mi`/`si` are the machine- and
/// supervisor-mode privileged suites, which cover the CSR, trap and MMU
/// work of Tasks 10, 11 and 14 and are the only reference validation those
/// tasks get.
const SUITES: [&str; 6] =
    ["rv64ui-p-", "rv64um-p-", "rv64ua-p-", "rv64uc-p-", "rv64mi-p-", "rv64si-p-"];

/// Tests that exercise architectural features this emulator deliberately
/// does not implement. Each entry must name *why*, and the reason must be a
/// feature that is out of scope for booting a Linux guest through the SBI
/// firmware role this emulator plays — never "it fails and I don't know
/// why". Anything not listed here is required to pass.
const UNSUPPORTED: &[(&str, &str)] = &[
    // Reaches `bad6` and then fails: after setting `mstatus.TVM`, the test
    // requires `sfence.vma` and a read of `satp` from S-mode to raise an
    // illegal instruction. This emulator does not implement TVM (nor the
    // TSR gate on `sret` that the same test checks a few instructions
    // later) — see `insn/rv64i.rs`, which says so at the SFENCE.VMA arm.
    // Verified by trace: the `sfence.vma` at `bad6` (0x80000268) executes
    // in S-mode with TVM=1 and falls through to `j fail`.
    ("rv64mi-p-illegal", "requires mstatus.TVM and mstatus.TSR"),
    // Fails at sub-test 2, the first store. The test sets `mstatus.MPRV`
    // with MPP=S so that an M-mode store is translated and permission-
    // checked as S-mode; this MMU deliberately ignores MPRV (documented in
    // `mmu::translate`, deferred by Task 11), so the store bypasses
    // translation entirely. Verified by trace: mcause = 0x7
    // (StoreAccessFault) with mtval = 0x2008, the *untranslated* address,
    // where the reference raises 0xF (StorePageFault) on the translated
    // one. The rest of the test additionally needs `mstatus.SUM` and
    // hardware A/D bit updating, neither of which is implemented.
    ("rv64si-p-dirty", "requires mstatus.MPRV, mstatus.SUM, and hardware A/D updates"),
];

fn run_one(path: &PathBuf) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    rv64_host::run_test_elf(&bytes)
}

#[test]
fn riscv_tests_isa_suite_passes() {
    // Skipping rather than panicking when `RISCV_TESTS` is unset keeps a
    // plain `cargo test --workspace` (outside `nix develop`) usable, which
    // is how every other crate in this workspace is tested. It is a skip
    // only when the variable is *absent*: a variable that is set but points
    // at a directory with no binaries still fails, so this cannot quietly
    // report green against an empty suite. And the skip itself is
    // refusable — see `suite_prerequisite_missing`, which turns it into a
    // failure wherever the conformance suite is expected to have run.
    let Ok(dir) = std::env::var("RISCV_TESTS") else {
        rv64_host::suite_prerequisite_missing(
            "riscv_tests_isa_suite_passes",
            "RISCV_TESTS is unset",
        );
        return;
    };

    let unsupported: BTreeSet<&str> = UNSUPPORTED.iter().map(|(n, _)| *n).collect();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut passed = Vec::new();

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("RISCV_TESTS={dir} is not readable: {e}"))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();

    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !SUITES.iter().any(|s| name.starts_with(s)) {
            continue;
        }
        if unsupported.contains(name.as_str()) {
            skipped.push(name);
            continue;
        }
        match run_one(&path) {
            Ok(()) => passed.push(name),
            Err(e) => failures.push(format!("  {name}: {e}")),
        }
    }

    let ran = passed.len() + failures.len();
    eprintln!(
        "riscv-tests: {}/{ran} passed, {} skipped as unsupported",
        passed.len(),
        skipped.len()
    );

    // A suite that finds nothing to run must fail, not pass: silently
    // testing zero instructions is worse than not having the test at all.
    assert!(ran > 0, "no test binaries matching {SUITES:?} found in {dir}");
    assert!(
        failures.is_empty(),
        "{} of {ran} riscv-tests failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
