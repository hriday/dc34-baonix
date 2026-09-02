//! Running a generated program under Spike and recovering its trace.
//!
//! # The log format, and how it was established
//!
//! Empirically, not from memory: the format is version-dependent, so it was
//! read off a run of the Spike in this repo's devShell (1.1.1-dev) before
//! any of this was written. `spike --isa=rv64imac -l --log-commits` emits
//! two lines per retired instruction:
//!
//! ```text
//! core   0: 0x0000000080000054 (0x000fb083) ld      ra, 0(t6)
//! core   0: 3 0x0000000080000054 (0x000fb083) x1  0x1111111111111111 mem 0x0000000080001100
//! ```
//!
//! The first is the *trace* line: `pc`, the instruction word, and its
//! disassembly. The second is the *commit* line, distinguished by the
//! privilege level (`3` = M-mode) between the core number and the pc; it
//! lists what the instruction wrote — `x<n> <value>` for a register,
//! `c<num>_<name> <value>` for a CSR, `mem <addr> [<value>]` for memory. An
//! instruction that writes nothing still gets a commit line, with no
//! write-back fields. A trap emits no commit line at all, just
//!
//! ```text
//! core   0: exception trap_machine_ecall, epc 0x00000000800000dc
//! ```
//!
//! # Why the whole register file can be recovered from this
//!
//! Spike never prints the full register file, only deltas — so the trace is
//! replayed: registers start at whatever, the prologue's 31 `ld`s set every
//! one of `x1..x31` and are logged like any other write, and by the first
//! body instruction the reconstruction is exact. That is the second reason
//! for the seeding prologue (the first being that Spike's reset state and
//! this emulator's do not match).
//!
//! This gives per-instruction comparison rather than final-state
//! comparison, which is worth the parser: a divergence is reported against
//! the instruction that caused it, instead of against whatever the program
//! happened to end with N instructions later.

use crate::{elf, Program, State};
use std::path::PathBuf;
use std::process::Command;

/// Whether `spike` is on `PATH`. Used to skip rather than fail outside
/// `nix develop`, matching how `rv64-host`'s `riscv-tests` harness handles a
/// missing suite.
pub fn available() -> bool {
    Command::new("spike").arg("--help").output().is_ok()
}

fn workdir(p: &Program) -> PathBuf {
    // Per-seed *and* per-process: `cargo test` runs test binaries in
    // parallel, and two runs sharing a log path would silently compare
    // against each other's output.
    std::env::temp_dir().join(format!("rv64-difftest-{}-{}", std::process::id(), p.seed))
}

/// Runs `p` under Spike and returns the state before each body instruction,
/// ending with the state before the terminating `ecall`.
///
/// On failure the working directory (ELF and log) is left in place and named
/// in the error, so a divergence can be re-examined by hand.
pub fn trace(p: &Program) -> Result<Vec<State>, String> {
    let dir = workdir(p);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let elf_path = dir.join("prog.elf");
    let log_path = dir.join("spike.log");
    std::fs::write(&elf_path, elf::build(p)).map_err(|e| format!("{}: {e}", elf_path.display()))?;

    let out = Command::new("spike")
        .arg("--isa=rv64imac")
        .arg("-l")
        .arg("--log-commits")
        .arg(format!("--log={}", log_path.display()))
        .arg(&elf_path)
        .output()
        .map_err(|e| format!("could not run spike: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "spike exited {} for seed {} (kept {}): {}",
            out.status,
            p.seed,
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let log = std::fs::read_to_string(&log_path)
        .map_err(|e| format!("{}: {e}", log_path.display()))?;
    let states = parse_log(&log, p.body_start, p.ecall_pc)
        .map_err(|e| format!("seed {}: {e} (kept {})", p.seed, dir.display()))?;

    let _ = std::fs::remove_dir_all(&dir);
    Ok(states)
}

/// Reconstructs the register file from a Spike commit log and returns the
/// state before each instruction from `body_start` through `ecall_pc`.
pub fn parse_log(log: &str, body_start: u64, ecall_pc: u64) -> Result<Vec<State>, String> {
    let mut regs = [0u64; 32];
    let mut states: Vec<State> = Vec::new();
    let mut started = false;
    let mut trace_lines = 0usize;

    for line in log.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() < 3 || t[0] != "core" {
            continue;
        }
        if let Some(pc) = t[2].strip_prefix("0x") {
            // Trace line: the instruction is about to execute, so the
            // registers as reconstructed so far are its input state.
            trace_lines += 1;
            let pc = hex(pc)?;
            if pc == body_start {
                started = true;
            }
            if started {
                regs[0] = 0;
                states.push(State { pc, regs });
                if pc == ecall_pc {
                    return Ok(states);
                }
            }
        } else if t[2].len() == 1 && t[2].as_bytes()[0].is_ascii_digit() {
            // Commit line: `core N: <priv> <pc> (<insn>) <writes...>`.
            // Only integer-register writes matter; CSR (`c<n>_<name>`) and
            // memory (`mem`) fields are skipped, and neither can be mistaken
            // for a register because only a register field starts with `x`
            // followed by digits.
            let mut i = 5;
            while i < t.len() {
                if let Some(n) = t[i].strip_prefix('x').and_then(|s| s.parse::<usize>().ok()) {
                    let v = t
                        .get(i + 1)
                        .and_then(|s| s.strip_prefix("0x"))
                        .ok_or_else(|| format!("register write with no value: {line:?}"))?;
                    if n >= 32 {
                        return Err(format!("register index out of range: {line:?}"));
                    }
                    regs[n] = hex(v)?;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }

    // Three distinct failures, distinguished because they mean very
    // different things: a format this parser does not understand, a program
    // that never reached the compared region, and one that ran off the end.
    if trace_lines == 0 {
        return Err("no instruction trace lines in spike's log — the log format changed".into());
    }
    if !started {
        return Err(format!("spike never reached the body at {body_start:#x}"));
    }
    Err(format!("spike never reached the `ecall` marker at {ecall_pc:#x}"))
}

fn hex(s: &str) -> Result<u64, String> {
    u64::from_str_radix(s, 16).map_err(|_| format!("not a hex number: {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim output from `spike --isa=rv64imac -l --log-commits`
    /// (1.1.1-dev, the version pinned by this repo's flake), covering every
    /// line shape the parser has to handle: the reset ROM, a register write,
    /// a CSR write, a load's `mem` field, an instruction that writes
    /// nothing, and the trap that ends the program.
    const SAMPLE: &str = "\
core   0: 0x0000000000001000 (0x00000297) auipc   t0, 0x0
core   0: 3 0x0000000000001000 (0x00000297) x5  0x0000000000001000
core   0: 0x000000000000100c (0x0182b283) ld      t0, 24(t0)
core   0: 3 0x000000000000100c (0x0182b283) x5  0x0000000080000040 mem 0x0000000000001018
core   0: 0x0000000000001010 (0x00028067) jr      t0
core   0: 3 0x0000000000001010 (0x00028067)
core   0: 0x0000000080000048 (0x30529073) csrw    mtvec, t0
core   0: 3 0x0000000080000048 (0x30529073) c773_mtvec 0x0000000080000000
core   0: 0x0000000080000054 (0x000fb083) ld      ra, 0(t6)
core   0: 3 0x0000000080000054 (0x000fb083) x1  0x1111111111111111 mem 0x0000000080001100
core   0: 0x00000000800000d0 (0x00508513) addi    a0, ra, 5
core   0: 3 0x00000000800000d0 (0x00508513) x10 0x1111111111111116
core   0: 0x00000000800000d4 (0x00b53023) sd      a1, 0(a0)
core   0: 3 0x00000000800000d4 (0x00b53023) mem 0x0000000080002000 0x0000000000000007
core   0: 0x00000000800000dc (0x00000073) ecall
core   0: exception trap_machine_ecall, epc 0x00000000800000dc
";

    #[test]
    fn parses_the_real_spike_format() {
        let s = parse_log(SAMPLE, 0x8000_00d0, 0x8000_00dc).unwrap();
        assert_eq!(s.len(), 3, "one state per body instruction, including the ecall");
        assert_eq!(s[0].pc, 0x8000_00d0);
        assert_eq!(s[0].regs[1], 0x1111_1111_1111_1111, "prologue writes must be replayed");
        assert_eq!(s[0].regs[5], 0x8000_0040);
        assert_eq!(s[0].regs[10], 0, "not written yet at the first body instruction");
        assert_eq!(s[1].pc, 0x8000_00d4);
        assert_eq!(s[1].regs[10], 0x1111_1111_1111_1116, "addi's result");
        assert_eq!(s[2].pc, 0x8000_00dc);
        assert_eq!(s[2].regs, s[1].regs, "a store writes no register");
        assert!(s.iter().all(|st| st.regs[0] == 0));
    }

    /// A store's commit line carries two bare hex fields after `mem`. They
    /// must not be mistaken for anything, and in particular must not shift
    /// the field scan.
    #[test]
    fn a_store_commit_line_changes_no_register() {
        let before = parse_log(SAMPLE, 0x8000_00d4, 0x8000_00dc).unwrap();
        assert_eq!(before[0].regs, before[1].regs);
    }

    #[test]
    fn an_unrecognisable_log_is_an_error_not_an_empty_trace() {
        assert!(parse_log("", 0, 4).is_err());
        assert!(parse_log("some other simulator's output\n", 0, 4).is_err());
        // Ran, but never reached the compared region.
        let e = parse_log(SAMPLE, 0xdead_0000, 0xdead_0004).unwrap_err();
        assert!(e.contains("never reached the body"), "{e}");
        // Reached the body but not the marker.
        let e = parse_log(SAMPLE, 0x8000_00d0, 0xdead_0004).unwrap_err();
        assert!(e.contains("never reached the `ecall`"), "{e}");
    }
}
