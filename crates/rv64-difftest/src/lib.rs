//! Differential testing of the `rv64` core against Spike, the reference
//! RISC-V ISA simulator.
//!
//! The `riscv-tests` suite (see `rv64-host/tests/riscv_tests.rs`) is the
//! authoritative per-instruction check: it covers every implemented opcode
//! with hand-picked operands. What it does *not* cover is instruction
//! *sequences* — one instruction's result feeding the next one's operand,
//! feeding a third one's address — and it uses a fixed, small set of operand
//! values. This harness covers that gap: it generates random straight-line
//! programs over the implemented instruction set, runs each one under both
//! simulators, and compares the architectural state they produce.
//!
//! # What is compared, and what is not
//!
//! `pc` and `x0..x31`, before every instruction of the generated body.
//!
//! CSRs are deliberately *not* compared. Spike and this emulator do not
//! agree on reset state and never will: Spike boots through its own reset
//! vector with a device tree in memory, and this emulator initialises
//! `mideleg`/`medeleg` to values Spike does not. Rather than chase that,
//! every generated program begins with a prologue that loads `x1..x31` from
//! a table embedded in the image, so the compared region starts from state
//! the *program* established rather than state the simulator did. Comparison
//! begins at the first body instruction, after the prologue has run.
//!
//! # Why the programs cannot trap
//!
//! A trap diverges the two simulators immediately (different `mtvec`
//! behaviour, different privilege bookkeeping) and says nothing about the
//! instruction under test. The generator therefore emits only encodings that
//! are architecturally guaranteed not to fault: see `gen` for the full list
//! of the constraints that guarantee it.

use rv64::backing::FakeBacking;
use rv64::bus::Bus;
use rv64::cache::PageCache;
use rv64::uart::VecSink;
use rv64::Cpu;

pub mod elf;
pub mod gen;
pub mod spike;

pub use gen::Program;

/// Base of guest RAM. Both simulators map RAM here (Spike by default,
/// this emulator by construction), so a single link address works for both.
pub const BASE: u64 = rv64::RAM_BASE;

/// Offset of the exit stub. It is the trap vector *and* the fall-through
/// target: the terminating `ecall` traps here, and the stub reports
/// completion to Spike's HTIF by storing 1 to `tohost`.
pub const EXIT_OFF: u64 = 0x0000;

/// Offset of the ELF entry point (the register-seeding prologue).
pub const ENTRY_OFF: u64 = 0x0040;

/// `tohost`/`fromhost`, the pair of symbols Spike's HTIF looks up in the
/// symbol table to find its exit channel. This emulator ignores them.
pub const TOHOST_OFF: u64 = 0x1000;
pub const FROMHOST_OFF: u64 = 0x1008;

/// 31 eight-byte seed values, loaded into `x1..x31` by the prologue.
pub const TABLE_OFF: u64 = 0x1100;

/// The only page generated memory operands may touch. Every load, store,
/// AMO and LR/SC in a generated body is preceded by an address computation
/// that provably lands inside it.
pub const SCRATCH_OFF: u64 = 0x2000;

/// Total mapped size of the single `PT_LOAD` segment: code, seed table and
/// scratch page. Everything past the end of the file image is `.bss`, which
/// both loaders zero-fill.
pub const MEMSZ: u64 = 0x3000;

/// Hard ceiling on code size. `SCRATCH_OFF` is reached from the body with
/// `auipc rt, 2`, which is only guaranteed to land in the scratch page while
/// the code offset stays below 4 KiB.
pub const CODE_LIMIT: u64 = 0x1000;

/// A single point of comparison: the architectural state visible *before*
/// the instruction at `pc` executes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub pc: u64,
    pub regs: [u64; 32],
}

impl core::fmt::Debug for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "State {{ pc: {:#x}, .. }}", self.pc)
    }
}

/// Instruction budget for one generated program. Bodies are a few hundred
/// instructions with no backward control flow, so anything near this cap
/// means the emulator has lost the plot rather than that the program is long.
const MAX_STEPS: u64 = 100_000;

/// Runs `p` under the `rv64` core and returns the state before each body
/// instruction, ending with the state before the terminating `ecall`.
///
/// The `ecall` itself is never executed: it is a marker, and executing it
/// would trap, at which point the two simulators would be comparing trap
/// handlers rather than instructions.
pub fn run_ours(p: &Program) -> Result<Vec<State>, String> {
    let pages = (rv64::RAM_SIZE / rv64::PAGE as u64) as u32;
    let mut bus = Bus::new(PageCache::new(FakeBacking::new(pages), 64), VecSink::default());
    let image = elf::build(p);
    let entry = rv64_host::elf::load(&mut bus, &image).map_err(|e| e.to_string())?;

    let mut cpu = Cpu::new(entry);
    let mut states = Vec::new();
    let mut started = false;
    for _ in 0..MAX_STEPS {
        if cpu.pc == p.body_start {
            started = true;
        }
        if started {
            states.push(State { pc: cpu.pc, regs: cpu.regs });
            if cpu.pc == p.ecall_pc {
                // The generator's memory-safety argument is that every
                // operand lands in the scratch page. A store that escaped it
                // would land in the code or the seed table — where it would
                // corrupt *both* simulators identically and so pass the
                // comparison while silently testing something other than the
                // program that was generated. Check it rather than assert it
                // in a doc comment.
                for a in (0..SCRATCH_OFF).step_by(8) {
                    let got = bus.load(BASE + a, 8).map_err(|e| format!("{e:?}"))?;
                    let want = u64::from_le_bytes(
                        p.image[a as usize..a as usize + 8].try_into().unwrap(),
                    );
                    if got != want {
                        return Err(format!(
                            "seed {}: a memory operand escaped the scratch page — \
                             {:#x} is {got:#x}, should be {want:#x}",
                            p.seed,
                            BASE + a
                        ));
                    }
                }
                return Ok(states);
            }
        }
        // Interrupts are never checked and the CLINT is never ticked: both
        // would make the run depend on wall-clock-like state that Spike
        // advances on its own schedule. `mstatus.MIE` is clear at reset in
        // both simulators, so no interrupt can fire in either regardless.
        cpu.step(&mut bus).map_err(|e| {
            format!(
                "trapped at pc {:#x} ({}): {e:?} — the generator is supposed to \
                 make traps impossible, so this is a generator or emulator bug",
                cpu.pc,
                p.disasm_at(cpu.pc).unwrap_or("?"),
            )
        })?;
    }
    Err(format!("no `ecall` marker after {MAX_STEPS} instructions (pc = {:#x})", cpu.pc))
}

/// Renders the first difference between two traces, with the offending
/// instruction's source text, as a human-readable report.
pub fn describe_divergence(p: &Program, ours: &[State], theirs: &[State]) -> Option<String> {
    let mut out = String::new();
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        if a == b {
            continue;
        }
        out.push_str(&format!("seed {}: diverged at step {i}\n", p.seed));
        if a.pc != b.pc {
            out.push_str(&format!("  pc:  ours {:#x}  spike {:#x}\n", a.pc, b.pc));
        }
        for r in 0..32 {
            if a.regs[r] != b.regs[r] {
                out.push_str(&format!(
                    "  x{r}: ours {:#018x}  spike {:#018x}\n",
                    a.regs[r], b.regs[r]
                ));
            }
        }
        // The state at step `i` is the state *before* the instruction at
        // `pc`, so the instruction that produced the difference is the one
        // executed at step `i - 1`.
        out.push_str("  context (last executed instruction first):\n");
        for s in ours[i.saturating_sub(4)..=i].iter().rev() {
            out.push_str(&format!(
                "    {:#010x}  {}\n",
                s.pc,
                p.disasm_at(s.pc).unwrap_or("<not in body>")
            ));
        }
        return Some(out);
    }
    if ours.len() != theirs.len() {
        return Some(format!(
            "seed {}: step count differs — ours {}, spike {} (common prefix matches)",
            p.seed,
            ours.len(),
            theirs.len()
        ));
    }
    None
}
