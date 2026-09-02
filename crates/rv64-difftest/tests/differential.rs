//! The differential test itself: random programs must produce identical
//! architectural state under `rv64` and under Spike.

use rv64_difftest::{describe_divergence, gen, run_ours, spike};

/// Seeds per run. Each seed is one Spike process plus one in-process `rv64`
/// run, so the wall-clock cost is dominated by Spike's start-up; 200 keeps
/// the suite under a minute while covering ~10,000 generated instructions.
const SEEDS: u64 = 200;

#[test]
fn random_programs_match_spike() {
    // Skipping rather than failing when Spike is absent keeps a plain
    // `cargo test --workspace` outside `nix develop` usable, exactly as
    // `rv64-host`'s `riscv-tests` harness does for `RISCV_TESTS`. It happens
    // only when the binary cannot be run at all — a Spike that runs but
    // disagrees still fails — and the skip itself is refusable via
    // `RV64_REQUIRE_SUITES`.
    if !spike::available() {
        rv64_host::suite_prerequisite_missing(
            "random_programs_match_spike",
            "spike is not on PATH",
        );
        return;
    }

    let mut failures = Vec::new();
    let mut steps = 0usize;
    for seed in 0..SEEDS {
        let p = gen::program(seed);
        let ours = match run_ours(&p) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("seed {seed}: rv64 failed: {e}"));
                continue;
            }
        };
        let theirs = match spike::trace(&p) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("seed {seed}: spike failed: {e}"));
                continue;
            }
        };
        steps += ours.len();
        if let Some(d) = describe_divergence(&p, &ours, &theirs) {
            failures.push(d);
        }
    }

    eprintln!("spike differential: {SEEDS} seeds, {steps} instructions compared");
    assert!(steps > 0, "no instructions were compared");
    assert!(
        failures.is_empty(),
        "{} of {SEEDS} seeds diverged:\n{}\n\
         Reproduce a single seed with: cargo run -p rv64-difftest -- --seed <n> --dump",
        failures.len(),
        // Only the first few, in full: the interesting one is almost always
        // the first, and a wall of them buries it.
        failures.iter().take(3).cloned().collect::<Vec<_>>().join("\n")
    );
}
