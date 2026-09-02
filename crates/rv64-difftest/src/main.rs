//! `rv64-difftest --seed <n> [--count <n>] [--dump]`
//!
//! Reproduces one seed (or a range) outside the test harness, printing the
//! divergence and the surrounding instructions. `--dump` prints the whole
//! generated program, which is the only readable form it has — there is no
//! object file to disassemble unless the run fails, in which case the ELF
//! and Spike log are left in a temporary directory named in the error.

use rv64_difftest::{describe_divergence, gen, run_ours, spike};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: u64| -> u64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let seed = flag("--seed", 0);
    let count = flag("--count", 1);
    let dump = args.iter().any(|a| a == "--dump");
    // Writing the object out is the only way to get a second opinion on the
    // generator's own encodings: `spike -l` disassembles everything it
    // executes, so `--elf` plus a Spike run cross-checks this crate's
    // hand-written instruction encoders against Spike's decoder.
    if let Some(path) = args.iter().position(|a| a == "--elf").and_then(|i| args.get(i + 1)) {
        let p = gen::program(seed);
        std::fs::write(path, rv64_difftest::elf::build(&p)).expect("write elf");
        println!("wrote {path} for seed {seed}: body {:#x}..{:#x}", p.body_start, p.ecall_pc);
        return std::process::ExitCode::SUCCESS;
    }

    if !spike::available() {
        eprintln!("spike is not on PATH; run inside `nix develop`");
        return std::process::ExitCode::FAILURE;
    }

    let mut failures = 0;
    for s in seed..seed + count {
        let p = gen::program(s);
        if dump {
            println!("--- seed {s}: {} body instructions ---", p.body_len);
            for (addr, text) in p.listing() {
                let marker = if addr == p.body_start {
                    " <- body starts"
                } else if addr == p.ecall_pc {
                    " <- body ends"
                } else {
                    ""
                };
                println!("  {addr:#010x}  {text}{marker}");
            }
        }

        let ours = match run_ours(&p) {
            Ok(v) => v,
            Err(e) => {
                println!("seed {s}: rv64 failed: {e}");
                failures += 1;
                continue;
            }
        };
        let theirs = match spike::trace(&p) {
            Ok(v) => v,
            Err(e) => {
                println!("seed {s}: spike failed: {e}");
                failures += 1;
                continue;
            }
        };
        match describe_divergence(&p, &ours, &theirs) {
            Some(d) => {
                println!("{d}");
                failures += 1;
            }
            None => {
                println!("seed {s}: ok ({} steps, {} body instructions)", ours.len(), p.body_len)
            }
        }
    }

    if failures == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("{failures} of {count} seeds diverged");
        std::process::ExitCode::FAILURE
    }
}
