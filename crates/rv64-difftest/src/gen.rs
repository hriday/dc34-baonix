//! Deterministic random program generator.
//!
//! A generated program has three parts:
//!
//! * an **exit stub** at offset 0, installed as `mtvec` by the prologue. The
//!   terminating `ecall` traps into it and it reports completion to Spike's
//!   HTIF. This emulator never reaches it — `run_ours` stops at the `ecall`.
//! * a **prologue** at the ELF entry point, which sets `mtvec` and then
//!   loads `x1..x31` from a table embedded in the image. This is what makes
//!   the two simulators comparable: neither one's reset state is used.
//! * a **body** of random instructions, ending in `ecall`.
//!
//! # Why the encoding guards below are `assert!`, not `debug_assert!`
//!
//! Every immediate and displacement this module splices into an instruction
//! word is range-checked before the splice. Those checks are unconditional
//! on purpose. If one is violated the field silently truncates, and the
//! result is not a crash or a divergence but something far worse for a
//! differential harness: *both* simulators are handed the same wrongly
//! encoded program, agree on it perfectly, and the seed passes while
//! testing a different program than its own disassembly claims. The escape
//! is invisible by construction, so it cannot be left to a build profile —
//! and the 25,000-seed campaign runs `--release`, where `debug_assert!`
//! compiles to nothing. Generation is not on any hot path; these cost
//! nothing that matters.
//!
//! # Why no generated program can trap
//!
//! A trap would divert one simulator into a trap handler and tell us nothing
//! about the instruction under test, so every source of traps is closed off
//! by construction rather than by luck:
//!
//! * **Illegal encodings.** Every instruction comes from an explicit table of
//!   (opcode, funct3, funct7) triples that both simulators implement. RVC
//!   encodings additionally avoid every reserved/HINT operand combination.
//! * **Memory faults.** Every memory operand is built by [`scratch_addr`],
//!   which provably lands in the one scratch page, aligned to the access
//!   width. Nothing else in the body touches memory.
//! * **Non-terminating control flow.** Branch, `jal` and `jalr` targets are
//!   always a *later* chunk boundary, so control flow is a DAG that always
//!   reaches the `ecall`.
//! * **`ecall`/`ebreak`/`mret`/`sret`/`sfence.vma`/CSR access.** Not
//!   generated at all. CSR writes could change privilege or trap behaviour,
//!   and CSRs are not compared anyway.
//!
//! Division by zero and the `INT_MIN / -1` overflow case are *deliberately*
//! reachable: RISC-V defines both (no trap), the seed table stocks registers
//! with `0`, `-1` and `i64::MIN` so they actually occur, and getting them
//! wrong is a real and easy bug.

use crate::{BASE, CODE_LIMIT, ENTRY_OFF, EXIT_OFF, SCRATCH_OFF, TABLE_OFF, TOHOST_OFF};

/// `ecall`, used only as the body's stop marker.
pub const ECALL: u32 = 0x0000_0073;

// ---------------------------------------------------------------- encoders

pub fn r_type(op: u32, f3: u32, f7: u32, rd: usize, rs1: usize, rs2: usize) -> u32 {
    op | ((rd as u32) << 7) | (f3 << 12) | ((rs1 as u32) << 15) | ((rs2 as u32) << 20) | (f7 << 25)
}

pub fn i_type(op: u32, f3: u32, rd: usize, rs1: usize, imm: i32) -> u32 {
    op | ((rd as u32) << 7) | (f3 << 12) | ((rs1 as u32) << 15) | (((imm as u32) & 0xFFF) << 20)
}

pub fn s_type(op: u32, f3: u32, rs1: usize, rs2: usize, imm: i32) -> u32 {
    let imm = (imm as u32) & 0xFFF;
    op | ((imm & 0x1F) << 7)
        | (f3 << 12)
        | ((rs1 as u32) << 15)
        | ((rs2 as u32) << 20)
        | ((imm >> 5) << 25)
}

pub fn b_type(f3: u32, rs1: usize, rs2: usize, imm: i32) -> u32 {
    let i = imm as u32;
    0x63 | (((i >> 11) & 1) << 7)
        | (((i >> 1) & 0xF) << 8)
        | (f3 << 12)
        | ((rs1 as u32) << 15)
        | ((rs2 as u32) << 20)
        | (((i >> 5) & 0x3F) << 25)
        | (((i >> 12) & 1) << 31)
}

pub fn u_type(op: u32, rd: usize, imm: u32) -> u32 {
    op | ((rd as u32) << 7) | ((imm & 0xF_FFFF) << 12)
}

pub fn j_type(rd: usize, imm: i32) -> u32 {
    let i = imm as u32;
    0x6F | ((rd as u32) << 7)
        | (((i >> 12) & 0xFF) << 12)
        | (((i >> 11) & 1) << 20)
        | (((i >> 1) & 0x3FF) << 21)
        | (((i >> 20) & 1) << 31)
}

// --------------------------------------------------------------------- rng

/// xorshift64. Small, deterministic, and identical on every host — the
/// harness's whole value depends on `program(n)` being the same program
/// everywhere, so `rand` (whose stream is not a stability guarantee) is
/// deliberately not used.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Any nonzero state works; the multiply spreads adjacent seeds apart
        // so that seeds 0, 1, 2... do not produce near-identical programs.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }

    /// Any register, including `x0`.
    fn reg(&mut self) -> usize {
        self.below(32) as usize
    }

    /// Any register but `x0` — for destinations that must actually change,
    /// and for the address temporaries.
    fn nzreg(&mut self) -> usize {
        1 + self.below(31) as usize
    }

    /// `x8..x15`, the three-bit register field of the RVC formats.
    fn creg(&mut self) -> usize {
        8 + self.below(8) as usize
    }
}

// ------------------------------------------------------------------- items

/// One emitted instruction, with its source text for divergence reports.
#[derive(Clone)]
pub struct Ins {
    pub size: u64,
    pub word: u32,
    pub text: String,
}

impl Ins {
    fn w(word: u32, text: impl Into<String>) -> Self {
        Ins { size: 4, word, text: text.into() }
    }
    fn c(half: u16, text: impl Into<String>) -> Self {
        Ins { size: 2, word: half as u32, text: text.into() }
    }
}

/// A unit of generation. Everything but control transfer is `Plain`; the
/// three control-transfer forms carry a *chunk* target that is resolved to a
/// byte offset once the layout is known.
enum Chunk {
    Plain(Vec<Ins>),
    Branch { f3: u32, rs1: usize, rs2: usize, skip: u64 },
    Jal { rd: usize, skip: u64 },
    /// `auipc rt, 0; addi rt, rt, d1; jalr rd, rt, d2`. Split across two
    /// immediates because a whole body can be further away than the 12-bit
    /// field of `jalr` alone reaches.
    Jalr { rt: usize, rd: usize, skip: u64 },
    /// `c.beqz`/`c.bnez rs1', <chunk target>`.
    CBranch { ne: bool, rs1: usize, skip: u64 },
    /// `c.j <chunk target>`.
    CJ { skip: u64 },
    /// `auipc rt, 0; addi rt, rt, d; c.jr|c.jalr rt`. `c.jr` has no
    /// displacement of its own, so the whole distance goes in the `addi`.
    CJr { rt: usize, link: bool, skip: u64 },
}

impl Chunk {
    fn len(&self) -> u64 {
        match self {
            Chunk::Plain(v) => v.iter().map(|i| i.size).sum(),
            Chunk::Branch { .. } | Chunk::Jal { .. } => 4,
            Chunk::Jalr { .. } => 12,
            Chunk::CBranch { .. } | Chunk::CJ { .. } => 2,
            Chunk::CJr { .. } => 10,
        }
    }
}

/// Longest a chunk can be, in bytes: the six-instruction LR/SC sequence.
/// Control-transfer targets are at most `MAX_SKIP` chunks ahead, so this
/// bounds every displacement — which is what lets `c.beqz` (±256 bytes) and
/// `c.jr`'s single `addi` (±2048) be used without a range check at
/// resolution time.
const MAX_CHUNK: u64 = 24;

/// Farthest a control transfer may reach, in chunks.
const MAX_SKIP: u64 = 6;

// ----------------------------------------------------------------- program

pub struct Program {
    pub seed: u64,
    /// File image of the single `PT_LOAD` segment: exit stub, prologue,
    /// body, and the `x1..x31` seed table.
    pub image: Vec<u8>,
    /// Address of the first body instruction — where comparison starts.
    pub body_start: u64,
    /// Address of the terminating `ecall` — where comparison stops.
    pub ecall_pc: u64,
    /// Number of instructions in the body (static count, not executed count).
    pub body_len: usize,
    listing: Vec<(u64, String)>,
}

impl Program {
    /// Source text of the instruction at `pc`, if `pc` is an instruction
    /// boundary in this program.
    pub fn disasm_at(&self, pc: u64) -> Option<&str> {
        self.listing
            .binary_search_by_key(&pc, |(a, _)| *a)
            .ok()
            .map(|i| self.listing[i].1.as_str())
    }

    /// The whole program as `(address, text)` pairs, for `--dump`.
    pub fn listing(&self) -> impl Iterator<Item = (u64, &str)> {
        self.listing.iter().map(|(a, t)| (*a, t.as_str()))
    }
}

/// Values the prologue loads into `x1..x31`.
///
/// A third of the file is drawn from a table of architecturally interesting
/// values rather than being uniformly random. Uniform 64-bit noise almost
/// never produces a zero divisor, an `i64::MIN` dividend, a shift amount
/// that is exactly 0 or 63, or a value whose upper half is all-ones — and
/// those are exactly the operands that separate a correct implementation
/// from a plausible one.
fn seed_table(rng: &mut Rng) -> [u64; 31] {
    const INTERESTING: [u64; 12] = [
        0,
        1,
        u64::MAX,                // -1
        1 << 63,                 // i64::MIN
        i64::MAX as u64,
        0xFFFF_FFFF,             // sign bit of the 32-bit forms
        0x8000_0000,
        0xFFFF_FFFF_8000_0000,   // sign-extended i32::MIN
        63,
        32,
        0x0000_0000_0000_00FF,
        0xAAAA_AAAA_5555_5555,
    ];
    let mut t = [0u64; 31];
    for slot in t.iter_mut() {
        *slot = if rng.below(3) == 0 { rng.pick(&INTERESTING) } else { rng.next_u64() };
    }
    t
}

/// Width of the window a memory operand may land in, within the scratch
/// page.
///
/// Deliberately much smaller than the page. With a whole 4 KiB to aim at,
/// the eight or so memory operands in a body almost never touch the same
/// bytes, so stores, AMOs and LR/SC never interact and the harness only ever
/// tests each in isolation. A 256-byte window makes them collide constantly,
/// which is the interesting case.
const WINDOW: i32 = 0xFF;

/// Emits the four instructions that leave a scratch-page address, aligned to
/// `align` bytes, in the returned register.
///
/// ```text
///   auipc rt, 2          ; rt = SCRATCH + code_off, code_off < 0x1000
///   andi  rt, rt, -2048  ; rt = SCRATCH or SCRATCH+0x800 — 2 KiB-aligned
///   andi  ru, rs, mask   ; ru = aligned offset in [0, WINDOW]
///   add   rt, rt, ru
/// ```
///
/// The final address is therefore inside the scratch page and a multiple of
/// `align`, whatever the register contents are. `rs` is a freely chosen
/// register, so the address still depends on computed state: a wrong result
/// from an earlier instruction shows up here as a load from a different
/// address, which amplifies the divergence rather than hiding it.
fn scratch_addr(rng: &mut Rng, align: u64, out: &mut Vec<Ins>) -> usize {
    let rt = rng.nzreg();
    scratch_addr_in(rng, align, out, rt)
}

/// `scratch_addr` with the base register chosen by the caller — needed by
/// the compressed memory forms, whose base must be `x8..x15` (`c.lw`) or
/// exactly `x2` (`c.lwsp`).
fn scratch_addr_in(rng: &mut Rng, align: u64, out: &mut Vec<Ins>, rt: usize) -> usize {
    assert!(rt != 0);
    let mut ru = rng.nzreg();
    while ru == rt {
        ru = rng.nzreg();
    }
    let rs = rng.reg();
    let mask = WINDOW & !(align as i32 - 1);
    out.push(Ins::w(u_type(0x17, rt, 2), format!("auipc x{rt}, 2")));
    out.push(Ins::w(i_type(0x13, 7, rt, rt, -2048), format!("andi x{rt}, x{rt}, -2048")));
    out.push(Ins::w(i_type(0x13, 7, ru, rs, mask), format!("andi x{ru}, x{rs}, {mask:#x}")));
    out.push(Ins::w(r_type(0x33, 0, 0, rt, rt, ru), format!("add x{rt}, x{rt}, x{ru}")));
    rt
}

/// Relative weights of each generated form. Memory and control transfer are
/// deliberately not rare: they are where sequencing bugs live, and they are
/// also the forms the fixed `riscv-tests` suite exercises least in
/// combination with each other.
const WEIGHTS: [(u32, Kind); 18] = [
    (12, Kind::Op),
    (6, Kind::Op32),
    (12, Kind::OpImm),
    (6, Kind::OpImm32),
    (3, Kind::Upper),
    (10, Kind::Mul),
    (9, Kind::Mem),
    (5, Kind::Amo),
    (2, Kind::LrSc),
    (7, Kind::Branch),
    (3, Kind::Jal),
    (2, Kind::Jalr),
    (12, Kind::Compressed),
    (6, Kind::CMem),
    (4, Kind::CBranch),
    (2, Kind::CJ),
    (2, Kind::CJr),
    (1, Kind::Fence),
];

#[derive(Clone, Copy)]
enum Kind {
    Op,
    Op32,
    OpImm,
    OpImm32,
    Upper,
    Mul,
    Mem,
    Amo,
    LrSc,
    Branch,
    Jal,
    Jalr,
    Compressed,
    CMem,
    CBranch,
    CJ,
    CJr,
    Fence,
}

fn gen_chunk(rng: &mut Rng) -> Chunk {
    let total: u32 = WEIGHTS.iter().map(|(w, _)| w).sum();
    let mut pick = rng.below(total as u64) as u32;
    let kind = WEIGHTS
        .iter()
        .find(|(w, _)| {
            if pick < *w {
                true
            } else {
                pick -= w;
                false
            }
        })
        .map(|(_, k)| *k)
        .expect("weights are exhaustive");

    let mut v = Vec::new();
    match kind {
        Kind::Op => {
            const OPS: [(u32, u32, &str); 10] = [
                (0, 0x00, "add"),
                (0, 0x20, "sub"),
                (1, 0x00, "sll"),
                (2, 0x00, "slt"),
                (3, 0x00, "sltu"),
                (4, 0x00, "xor"),
                (5, 0x00, "srl"),
                (5, 0x20, "sra"),
                (6, 0x00, "or"),
                (7, 0x00, "and"),
            ];
            let (f3, f7, n) = rng.pick(&OPS);
            let (rd, a, b) = (rng.reg(), rng.reg(), rng.reg());
            v.push(Ins::w(r_type(0x33, f3, f7, rd, a, b), format!("{n} x{rd}, x{a}, x{b}")));
        }
        Kind::Op32 => {
            const OPS: [(u32, u32, &str); 5] = [
                (0, 0x00, "addw"),
                (0, 0x20, "subw"),
                (1, 0x00, "sllw"),
                (5, 0x00, "srlw"),
                (5, 0x20, "sraw"),
            ];
            let (f3, f7, n) = rng.pick(&OPS);
            let (rd, a, b) = (rng.reg(), rng.reg(), rng.reg());
            v.push(Ins::w(r_type(0x3B, f3, f7, rd, a, b), format!("{n} x{rd}, x{a}, x{b}")));
        }
        Kind::OpImm => {
            const OPS: [(u32, &str); 6] =
                [(0, "addi"), (2, "slti"), (3, "sltiu"), (4, "xori"), (6, "ori"), (7, "andi")];
            let (rd, a) = (rng.reg(), rng.reg());
            if rng.below(4) == 0 {
                // Shift-immediate: RV64 takes a 6-bit shamt, and bit 30
                // selects arithmetic. No other bit of the immediate may be
                // set or the encoding is reserved.
                let shamt = rng.below(64) as i32;
                let arith = rng.below(2) == 1;
                let imm = if arith { shamt | 0x400 } else { shamt };
                let n = if arith { "srai" } else { "srli" };
                let (f3, n) = if rng.below(2) == 0 && !arith { (1, "slli") } else { (5, n) };
                v.push(Ins::w(i_type(0x13, f3, rd, a, imm), format!("{n} x{rd}, x{a}, {shamt}")));
            } else {
                let (f3, n) = rng.pick(&OPS);
                let imm = (rng.next_u64() as i32) >> 20; // sign-extended 12 bits
                v.push(Ins::w(i_type(0x13, f3, rd, a, imm), format!("{n} x{rd}, x{a}, {imm}")));
            }
        }
        Kind::OpImm32 => {
            let (rd, a) = (rng.reg(), rng.reg());
            if rng.below(2) == 0 {
                // The `*w` shift-immediates take a *5*-bit shamt; a set bit
                // 5 is a reserved encoding, not a shift by 32 or more.
                let shamt = rng.below(32) as i32;
                let arith = rng.below(2) == 1;
                let imm = if arith { shamt | 0x400 } else { shamt };
                let n = if arith { "sraiw" } else { "srliw" };
                let (f3, n) = if rng.below(2) == 0 && !arith { (1, "slliw") } else { (5, n) };
                v.push(Ins::w(i_type(0x1B, f3, rd, a, imm), format!("{n} x{rd}, x{a}, {shamt}")));
            } else {
                let imm = (rng.next_u64() as i32) >> 20;
                v.push(Ins::w(i_type(0x1B, 0, rd, a, imm), format!("addiw x{rd}, x{a}, {imm}")));
            }
        }
        Kind::Upper => {
            let rd = rng.reg();
            let imm = (rng.next_u64() & 0xF_FFFF) as u32;
            // `auipc` is worth as much as `lui` here for a different reason:
            // its result is a function of `pc`, so a pc bug that a branch
            // would only reveal through control flow instead lands directly
            // in a compared register.
            if rng.below(2) == 0 {
                v.push(Ins::w(u_type(0x37, rd, imm), format!("lui x{rd}, {imm:#x}")));
            } else {
                // Kept below 2 so the result cannot leave the mapped image;
                // it is never used as an address, but keeping it in range
                // makes the value legible in a divergence report.
                let imm = imm & 1;
                v.push(Ins::w(u_type(0x17, rd, imm), format!("auipc x{rd}, {imm:#x}")));
            }
        }
        Kind::Mul => {
            const M64: [(u32, &str); 8] = [
                (0, "mul"),
                (1, "mulh"),
                (2, "mulhsu"),
                (3, "mulhu"),
                (4, "div"),
                (5, "divu"),
                (6, "rem"),
                (7, "remu"),
            ];
            const M32: [(u32, &str); 5] =
                [(0, "mulw"), (4, "divw"), (5, "divuw"), (6, "remw"), (7, "remuw")];
            let (rd, a, b) = (rng.reg(), rng.reg(), rng.reg());
            let (op, f3, n) =
                if rng.below(3) == 0 { let (f3, n) = rng.pick(&M32); (0x3B, f3, n) } else {
                    let (f3, n) = rng.pick(&M64);
                    (0x33, f3, n)
                };
            v.push(Ins::w(r_type(op, f3, 0x01, rd, a, b), format!("{n} x{rd}, x{a}, x{b}")));
        }
        Kind::Mem => {
            const LOADS: [(u32, u64, &str); 7] = [
                (0, 1, "lb"),
                (1, 2, "lh"),
                (2, 4, "lw"),
                (3, 8, "ld"),
                (4, 1, "lbu"),
                (5, 2, "lhu"),
                (6, 4, "lwu"),
            ];
            const STORES: [(u32, u64, &str); 4] =
                [(0, 1, "sb"), (1, 2, "sh"), (2, 4, "sw"), (3, 8, "sd")];
            if rng.below(2) == 0 {
                let (f3, w, n) = rng.pick(&LOADS);
                let rt = scratch_addr(rng, w, &mut v);
                let rd = rng.reg();
                v.push(Ins::w(i_type(0x03, f3, rd, rt, 0), format!("{n} x{rd}, 0(x{rt})")));
            } else {
                let (f3, w, n) = rng.pick(&STORES);
                let rt = scratch_addr(rng, w, &mut v);
                let rs = rng.reg();
                v.push(Ins::w(s_type(0x23, f3, rt, rs, 0), format!("{n} x{rs}, 0(x{rt})")));
            }
        }
        Kind::Amo => {
            const AMOS: [(u32, &str); 9] = [
                (0x00, "amoadd"),
                (0x01, "amoswap"),
                (0x04, "amoxor"),
                (0x08, "amoor"),
                (0x0C, "amoand"),
                (0x10, "amomin"),
                (0x14, "amomax"),
                (0x18, "amominu"),
                (0x1C, "amomaxu"),
            ];
            let (op, n) = rng.pick(&AMOS);
            let (f3, w, sfx) = if rng.below(2) == 0 { (2, 4, "w") } else { (3, 8, "d") };
            let rt = scratch_addr(rng, w, &mut v);
            let (rd, rs) = (rng.reg(), rng.reg());
            v.push(Ins::w(
                r_type(0x2F, f3, op << 2, rd, rt, rs),
                format!("{n}.{sfx} x{rd}, x{rs}, (x{rt})"),
            ));
        }
        Kind::LrSc => {
            // Emitted only as an adjacent pair over one address, with
            // nothing in between. The RISC-V spec permits `sc` to fail
            // spuriously and leaves reservation invalidation largely
            // implementation-defined, so a `sc` separated from its `lr` by
            // other memory traffic is legitimately allowed to differ between
            // two conformant implementations — that would be a false
            // divergence, not a bug. The adjacent pair is the one shape both
            // are required to agree on: the `sc` must succeed.
            let (f3, w, sfx) = if rng.below(2) == 0 { (2, 4, "w") } else { (3, 8, "d") };
            let rt = scratch_addr(rng, w, &mut v);
            // The `lr` must not overwrite the address register before the
            // `sc` uses it.
            let mut rd1 = rng.reg();
            while rd1 == rt {
                rd1 = rng.reg();
            }
            let (rd2, rs) = (rng.reg(), rng.reg());
            v.push(Ins::w(
                r_type(0x2F, f3, 0x02 << 2, rd1, rt, 0),
                format!("lr.{sfx} x{rd1}, (x{rt})"),
            ));
            v.push(Ins::w(
                r_type(0x2F, f3, 0x03 << 2, rd2, rt, rs),
                format!("sc.{sfx} x{rd2}, x{rs}, (x{rt})"),
            ));
        }
        Kind::Branch => {
            let f3 = rng.pick(&[0u32, 1, 4, 5, 6, 7]);
            let (rs1, rs2) = (rng.reg(), rng.reg());
            let skip = 1 + rng.below(MAX_SKIP);
            return Chunk::Branch { f3, rs1, rs2, skip };
        }
        Kind::Jal => {
            return Chunk::Jal { rd: rng.reg(), skip: 1 + rng.below(MAX_SKIP) };
        }
        Kind::Jalr => {
            return Chunk::Jalr { rt: rng.nzreg(), rd: rng.reg(), skip: 1 + rng.below(MAX_SKIP) };
        }
        Kind::CBranch => {
            return Chunk::CBranch {
                ne: rng.below(2) == 1,
                rs1: rng.creg(),
                skip: 1 + rng.below(MAX_SKIP),
            };
        }
        Kind::CJ => {
            return Chunk::CJ { skip: 1 + rng.below(MAX_SKIP) };
        }
        Kind::CJr => {
            return Chunk::CJr {
                rt: rng.nzreg(),
                link: rng.below(2) == 1,
                skip: 1 + rng.below(MAX_SKIP),
            };
        }
        Kind::Compressed => {
            let ins = gen_compressed(rng);
            v.push(ins);
        }
        Kind::CMem => gen_compressed_mem(rng, &mut v),
        Kind::Fence => {
            // `fence iorw, iorw`. Cheap, and MISC-MEM being undecoded was a
            // real defect found by the ISA suite — keep exercising it.
            v.push(Ins::w(0x0FF0_000F, "fence"));
        }
    }
    Chunk::Plain(v)
}

/// One compressed instruction, from the subset that has no reserved or HINT
/// operand combinations once the guards below are applied.
///
/// Excluded on purpose: the stack-pointer memory forms (`c.lwsp`, `c.sdsp`,
/// ...) and `c.ld`/`c.sw`, because their addresses come from a register this
/// generator cannot constrain to the scratch page; and `c.j`/`c.beqz`, whose
/// targets would need the same relocation machinery as the 32-bit branches
/// for coverage the 32-bit forms already provide.
fn gen_compressed(rng: &mut Rng) -> Ins {
    match rng.below(16) {
        // c.addi rd, nzimm — rd != 0 and nzimm != 0, else HINT/c.nop.
        0 => {
            let rd = rng.nzreg();
            let imm = nz_imm6(rng);
            Ins::c(ci(0b000, 0b01, rd, imm), format!("c.addi x{rd}, {imm}"))
        }
        // c.addiw rd, imm — rd != 0 (rd == 0 is reserved); imm may be 0.
        1 => {
            let rd = rng.nzreg();
            let imm = imm6(rng);
            Ins::c(ci(0b001, 0b01, rd, imm), format!("c.addiw x{rd}, {imm}"))
        }
        // c.li rd, imm — rd != 0.
        2 => {
            let rd = rng.nzreg();
            let imm = imm6(rng);
            Ins::c(ci(0b010, 0b01, rd, imm), format!("c.li x{rd}, {imm}"))
        }
        // c.lui rd, nzimm — rd not in {0, 2} (2 is c.addi16sp) and nzimm != 0.
        3 => {
            let mut rd = rng.nzreg();
            while rd == 2 {
                rd = rng.nzreg();
            }
            let imm = nz_imm6(rng);
            Ins::c(ci(0b011, 0b01, rd, imm), format!("c.lui x{rd}, {imm}"))
        }
        // c.slli rd, shamt — rd != 0, shamt != 0 (RV64 allows 1..63).
        4 => {
            let rd = rng.nzreg();
            let sh = 1 + rng.below(63) as i32;
            Ins::c(ci(0b000, 0b10, rd, sh), format!("c.slli x{rd}, {sh}"))
        }
        // c.srli / c.srai rd', shamt — shamt != 0.
        5 | 6 => {
            let rd = rng.creg();
            let sh = 1 + rng.below(63) as i32;
            let (f2, n) = if rng.below(2) == 0 { (0b00, "c.srli") } else { (0b01, "c.srai") };
            Ins::c(cb_imm(f2, rd, sh), format!("{n} x{rd}, {sh}"))
        }
        // c.andi rd', imm — any 6-bit immediate is valid.
        7 => {
            let rd = rng.creg();
            let imm = imm6(rng);
            Ins::c(cb_imm(0b10, rd, imm), format!("c.andi x{rd}, {imm}"))
        }
        // c.sub / c.xor / c.or / c.and rd', rs2'
        8 | 9 => {
            let (rd, rs2) = (rng.creg(), rng.creg());
            let (f2, n) =
                rng.pick(&[(0b00, "c.sub"), (0b01, "c.xor"), (0b10, "c.or"), (0b11, "c.and")]);
            Ins::c(ca(0, f2, rd, rs2), format!("{n} x{rd}, x{rs2}"))
        }
        // c.subw / c.addw rd', rs2'
        10 => {
            let (rd, rs2) = (rng.creg(), rng.creg());
            let (f2, n) = rng.pick(&[(0b00, "c.subw"), (0b01, "c.addw")]);
            Ins::c(ca(1, f2, rd, rs2), format!("{n} x{rd}, x{rs2}"))
        }
        // c.mv rd, rs2 — both != 0 (rs2 == 0 is c.jr).
        11 | 12 => {
            let (rd, rs2) = (rng.nzreg(), rng.nzreg());
            Ins::c(0x8002 | ((rd as u16) << 7) | ((rs2 as u16) << 2), format!("c.mv x{rd}, x{rs2}"))
        }
        // c.addi4spn rd', x2, nzuimm — scaled by 4, in 4..=1020, never 0
        // (a zero immediate is the reserved all-zero halfword). Pure
        // arithmetic on x2: nothing dereferences the result.
        13 => {
            let rd = rng.creg();
            let imm = 4 * (1 + rng.below(255)) as i32;
            let half = (((imm >> 4) as u16 & 0x3) << 11)
                | (((imm >> 6) as u16 & 0xF) << 7)
                | (((imm >> 2) as u16 & 1) << 6)
                | (((imm >> 3) as u16 & 1) << 5)
                | (((rd - 8) as u16) << 2);
            Ins::c(half, format!("c.addi4spn x{rd}, x2, {imm}"))
        }
        // c.addi16sp x2, nzimm — scaled by 16, in -512..=496, never 0.
        14 => {
            let mut n = rng.below(64) as i32 - 32;
            if n == 0 {
                n = 1;
            }
            let imm = n * 16;
            let u = imm as u32;
            let half = (0b011 << 13)
                | (((u >> 9) as u16 & 1) << 12)
                | (2 << 7)
                | (((u >> 4) as u16 & 1) << 6)
                | (((u >> 6) as u16 & 1) << 5)
                | (((u >> 7) as u16 & 0x3) << 3)
                | (((u >> 5) as u16 & 1) << 2)
                | 0b01;
            Ins::c(half, format!("c.addi16sp x2, {imm}"))
        }
        // c.add rd, rs2 — both != 0 (rs2 == 0 is c.jalr/c.ebreak).
        _ => {
            let (rd, rs2) = (rng.nzreg(), rng.nzreg());
            let half = 0x9002 | ((rd as u16) << 7) | ((rs2 as u16) << 2);
            Ins::c(half, format!("c.add x{rd}, x{rs2}"))
        }
    }
}

/// The compressed memory forms.
///
/// Both base-register conventions are covered: the `CL`/`CS` forms take
/// `x8..x15`, and the `*sp` forms take `x2` implicitly. `x2` is not special
/// to this generator — it is seeded and clobbered like any other register —
/// so pointing it at the scratch page costs nothing.
///
/// The zero-extended, scaled immediate is added on top of the window offset,
/// which is why `WINDOW` plus the largest of these (504, for `c.ldsp`) has
/// to stay inside the scratch page.
fn gen_compressed_mem(rng: &mut Rng, v: &mut Vec<Ins>) {
    if rng.below(2) == 0 {
        // CL/CS: `c.lw`/`c.ld`/`c.sw`/`c.sd`, base in x8..x15.
        let wide = rng.below(2) == 0;
        let (w, scale, maxoff) = if wide { (8u64, 8i32, 248) } else { (4, 4, 124) };
        let base = rng.creg();
        let rt = scratch_addr_in(rng, w, v, base);
        let off = scale * rng.below((maxoff / scale + 1) as u64) as i32;
        let r = rng.creg();
        let store = rng.below(2) == 0;
        let f3 = match (wide, store) {
            (false, false) => 0b010, // c.lw
            (true, false) => 0b011,  // c.ld
            (false, true) => 0b110,  // c.sw
            (true, true) => 0b111,   // c.sd
        };
        let imm = if wide {
            (((off >> 3) as u16 & 0x7) << 10) | (((off >> 6) as u16 & 0x3) << 5)
        } else {
            (((off >> 3) as u16 & 0x7) << 10)
                | (((off >> 2) as u16 & 1) << 6)
                | (((off >> 6) as u16 & 1) << 5)
        };
        let half = (f3 << 13) | imm | (((rt - 8) as u16) << 7) | (((r - 8) as u16) << 2);
        const N: [&str; 4] = ["c.lw", "c.ld", "c.sw", "c.sd"];
        let n = N[(store as usize) * 2 + wide as usize];
        v.push(Ins::c(half, format!("{n} x{r}, {off}(x{rt})")));
    } else {
        // CI/CSS: the `*sp` forms, base implicitly x2.
        let wide = rng.below(2) == 0;
        let (w, scale, maxoff) = if wide { (8u64, 8i32, 504) } else { (4, 4, 252) };
        scratch_addr_in(rng, w, v, 2);
        let off = scale * rng.below((maxoff / scale + 1) as u64) as i32;
        let store = rng.below(2) == 0;
        let half = if store {
            let rs2 = rng.reg() as u16;
            if wide {
                // c.sdsp: 111 uimm[5:3] uimm[8:6] rs2 10
                (0b111 << 13)
                    | (((off >> 3) as u16 & 0x7) << 10)
                    | (((off >> 6) as u16 & 0x7) << 7)
                    | (rs2 << 2)
                    | 0b10
            } else {
                // c.swsp: 110 uimm[5:2] uimm[7:6] rs2 10
                (0b110 << 13)
                    | (((off >> 2) as u16 & 0xF) << 9)
                    | (((off >> 6) as u16 & 0x3) << 7)
                    | (rs2 << 2)
                    | 0b10
            }
        } else {
            // `rd == x0` is reserved for both load forms.
            let rd = rng.nzreg() as u16;
            if wide {
                // c.ldsp: 011 uimm[5] rd uimm[4:3] uimm[8:6] 10
                (0b011 << 13)
                    | (((off >> 5) as u16 & 1) << 12)
                    | (rd << 7)
                    | (((off >> 3) as u16 & 0x3) << 5)
                    | (((off >> 6) as u16 & 0x7) << 2)
                    | 0b10
            } else {
                // c.lwsp: 010 uimm[5] rd uimm[4:2] uimm[7:6] 10
                (0b010 << 13)
                    | (((off >> 5) as u16 & 1) << 12)
                    | (rd << 7)
                    | (((off >> 2) as u16 & 0x7) << 4)
                    | (((off >> 6) as u16 & 0x3) << 2)
                    | 0b10
            }
        };
        const N: [&str; 4] = ["c.lwsp", "c.ldsp", "c.swsp", "c.sdsp"];
        let n = N[(store as usize) * 2 + wide as usize];
        let r = (half >> 7) & 0x1F;
        let r = if store { (half >> 2) & 0x1F } else { r };
        v.push(Ins::c(half, format!("{n} x{r}, {off}(x2)")));
    }
}

/// Signed 6-bit immediate.
fn imm6(rng: &mut Rng) -> i32 {
    ((rng.below(64) as i32) << 26) >> 26
}

/// Signed 6-bit immediate, never zero.
fn nz_imm6(rng: &mut Rng) -> i32 {
    let mut v = imm6(rng);
    while v == 0 {
        v = imm6(rng);
    }
    v
}

/// CI format: `funct3 | imm[5] | rd | imm[4:0] | op`.
fn ci(f3: u16, op: u16, rd: usize, imm: i32) -> u16 {
    let i = (imm as u32) & 0x3F;
    (f3 << 13) | (((i >> 5) as u16) << 12) | ((rd as u16) << 7) | (((i & 0x1F) as u16) << 2) | op
}

/// The `funct3 = 100`, quadrant-1 shift/`andi` format:
/// `100 | imm[5] | funct2 | rd' | imm[4:0] | 01`.
fn cb_imm(f2: u16, rd: usize, imm: i32) -> u16 {
    let i = (imm as u32) & 0x3F;
    (0b100 << 13)
        | (((i >> 5) as u16) << 12)
        | (f2 << 10)
        | (((rd - 8) as u16) << 7)
        | (((i & 0x1F) as u16) << 2)
        | 0b01
}

/// CA format: `100 | w | 11 | rd' | funct2 | rs2' | 01`, where `w` selects
/// the 32-bit (`c.subw`/`c.addw`) group.
fn ca(w: u16, f2: u16, rd: usize, rs2: usize) -> u16 {
    (0b100 << 13)
        | (w << 12)
        | (0b11 << 10)
        | (((rd - 8) as u16) << 7)
        | (f2 << 5)
        | (((rs2 - 8) as u16) << 2)
        | 0b01
}

/// The exit stub at offset 0: `mtvec` points here, so the terminating
/// `ecall` lands here. It stores 1 to `tohost`, which is how Spike's HTIF
/// learns the run is over and exits 0. This emulator stops at the `ecall`
/// and never executes any of it.
fn exit_stub() -> Vec<Ins> {
    vec![
        // `auipc x5, 1` at offset 0 yields BASE + 0x1000 = tohost.
        Ins::w(u_type(0x17, 5, (TOHOST_OFF >> 12) as u32), "auipc x5, 1  # &tohost"),
        Ins::w(i_type(0x13, 0, 6, 0, 1), "addi x6, x0, 1"),
        Ins::w(s_type(0x23, 3, 5, 6, 0), "sd x6, 0(x5)  # tohost = 1"),
        Ins::w(j_type(0, 0), "j ."),
    ]
}

/// The prologue: install the exit stub as `mtvec`, then load `x1..x31` from
/// the seed table.
///
/// `auipc` rather than `lui` throughout, because RAM is at `0x8000_0000`:
/// `lui` sign-extends bit 31, so `lui x, 0x80000` produces
/// `0xFFFF_FFFF_8000_0000`, not the address. `auipc` adds to `pc`, which is
/// already in the right half of the address space.
fn prologue() -> Vec<Ins> {
    let mut v = vec![
        Ins::w(u_type(0x17, 5, 0), "auipc x5, 0"),
        Ins::w(i_type(0x13, 0, 5, 5, -(ENTRY_OFF as i32)), format!("addi x5, x5, -{ENTRY_OFF}")),
        Ins::w(i_type(0x73, 1, 0, 5, 0x305), "csrw mtvec, x5"),
    ];
    // `auipc x31, 1` reads pc at the offset this instruction will occupy.
    let at = ENTRY_OFF + 4 * v.len() as u64;
    let delta = TABLE_OFF as i64 - (at as i64 + 0x1000);
    assert!((-2048..=2047).contains(&delta), "seed table is out of `addi` range");
    v.push(Ins::w(u_type(0x17, 31, 1), "auipc x31, 1"));
    v.push(Ins::w(i_type(0x13, 0, 31, 31, delta as i32), format!("addi x31, x31, {delta}")));
    for n in 1..32usize {
        let off = 8 * (n as i32 - 1);
        // x31 is loaded last, so the base register survives until it is no
        // longer needed.
        v.push(Ins::w(i_type(0x03, 3, n, 31, off), format!("ld x{n}, {off}(x31)")));
    }
    v
}

/// Number of chunks in a body. Each expands to one to five instructions, so
/// a body is roughly 60-150 instructions — long enough for values to flow
/// through several instructions before being compared, short enough that a
/// divergence report is readable.
const CHUNKS: usize = 48;

/// Builds the program for `seed`.
pub fn program(seed: u64) -> Program {
    let mut rng = Rng::new(seed);
    let table = seed_table(&mut rng);

    let pro = prologue();
    let body_start_off = ENTRY_OFF + pro.iter().map(|i| i.size).sum::<u64>();

    // Leave room for the `ecall` and keep the whole body under CODE_LIMIT,
    // which is what makes `auipc rt, 2` land in the scratch page.
    let budget = CODE_LIMIT - 64;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut off = body_start_off;
    while chunks.len() < CHUNKS {
        let c = gen_chunk(&mut rng);
        assert!(c.len() <= MAX_CHUNK, "MAX_CHUNK is stale; branch ranges depend on it");
        if off + c.len() > budget {
            break;
        }
        off += c.len();
        chunks.push(c);
    }
    chunks.push(Chunk::Plain(vec![Ins::w(ECALL, "ecall  # stop marker")]));

    // Lay the chunks out, then resolve every control transfer against the
    // resulting offsets. Targets are clamped to the terminator, so a chunk
    // near the end simply jumps to the `ecall`.
    let last = chunks.len() - 1;
    let mut offs = Vec::with_capacity(chunks.len());
    let mut off = body_start_off;
    for c in &chunks {
        offs.push(off);
        off += c.len();
    }
    let ecall_off = offs[last];

    let mut body: Vec<Ins> = Vec::new();
    for (i, c) in chunks.into_iter().enumerate() {
        let here = offs[i];
        match c {
            Chunk::Plain(v) => body.extend(v),
            Chunk::Branch { f3, rs1, rs2, skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as i32;
                const N: [&str; 8] = ["beq", "bne", "?", "?", "blt", "bge", "bltu", "bgeu"];
                body.push(Ins::w(
                    b_type(f3, rs1, rs2, d),
                    format!("{} x{rs1}, x{rs2}, .+{d}", N[f3 as usize]),
                ));
            }
            Chunk::Jal { rd, skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as i32;
                body.push(Ins::w(j_type(rd, d), format!("jal x{rd}, .+{d}")));
            }
            Chunk::Jalr { rt, rd, skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as i32;
                // 12-bit fields, so a target more than 2 KiB away needs the
                // displacement split between the `addi` and the `jalr`.
                let d1 = d.clamp(-2048, 2047);
                let d2 = d - d1;
                assert!((-2048..=2047).contains(&d2), "jalr target out of two-field range");
                body.push(Ins::w(u_type(0x17, rt, 0), format!("auipc x{rt}, 0")));
                body.push(Ins::w(i_type(0x13, 0, rt, rt, d1), format!("addi x{rt}, x{rt}, {d1}")));
                body.push(Ins::w(
                    i_type(0x67, 0, rd, rt, d2),
                    format!("jalr x{rd}, {d2}(x{rt})  # -> .+{d}"),
                ));
            }
            Chunk::CBranch { ne, rs1, skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as u32;
                // CB: 11x | imm[8] imm[4:3] | rs1' | imm[7:6] imm[2:1] imm[5] | 01
                assert!(d < 256, "c.beqz target out of range");
                let f3: u16 = if ne { 0b111 } else { 0b110 };
                let half = (f3 << 13)
                    | (((d >> 8) as u16 & 1) << 12)
                    | (((d >> 3) as u16 & 0x3) << 10)
                    | (((rs1 - 8) as u16) << 7)
                    | (((d >> 6) as u16 & 0x3) << 5)
                    | (((d >> 1) as u16 & 0x3) << 3)
                    | (((d >> 5) as u16 & 1) << 2)
                    | 0b01;
                let n = if ne { "c.bnez" } else { "c.beqz" };
                body.push(Ins::c(half, format!("{n} x{rs1}, .+{d}")));
            }
            Chunk::CJ { skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as u32;
                // CJ: 101 | imm[11] imm[4] imm[9:8] imm[10] imm[6] imm[7] imm[3:1] imm[5] | 01
                assert!(d < 2048, "c.j target out of range");
                let half = (0b101u16 << 13)
                    | (((d >> 11) as u16 & 1) << 12)
                    | (((d >> 4) as u16 & 1) << 11)
                    | (((d >> 8) as u16 & 0x3) << 9)
                    | (((d >> 10) as u16 & 1) << 8)
                    | (((d >> 6) as u16 & 1) << 7)
                    | (((d >> 7) as u16 & 1) << 6)
                    | (((d >> 1) as u16 & 0x7) << 3)
                    | (((d >> 5) as u16 & 1) << 2)
                    | 0b01;
                body.push(Ins::c(half, format!("c.j .+{d}")));
            }
            Chunk::CJr { rt, link, skip } => {
                let t = (i + skip as usize).min(last);
                let d = (offs[t] - here) as i32;
                // `c.jr` carries no displacement, so the whole distance has
                // to fit in the `addi` — which `MAX_CHUNK * MAX_SKIP`
                // guarantees.
                assert!((-2048..=2047).contains(&d), "c.jr target out of `addi` range");
                body.push(Ins::w(u_type(0x17, rt, 0), format!("auipc x{rt}, 0")));
                body.push(Ins::w(i_type(0x13, 0, rt, rt, d), format!("addi x{rt}, x{rt}, {d}")));
                let (base, n) = if link { (0x9002u16, "c.jalr") } else { (0x8002, "c.jr") };
                body.push(Ins::c(base | ((rt as u16) << 7), format!("{n} x{rt}  # -> .+{d}")));
            }
        }
    }

    // ---- assemble the file image ----
    // The scratch page is part of the file image, not `.bss`, so that it
    // starts out full of pseudo-random data rather than zeros. That matters
    // more than it looks: with a zeroed page, a body's handful of stores
    // leave almost every load reading `0`, and a load that reads zero cannot
    // distinguish `lhu` from `lh`, `lwu` from `lw`, or a byte-select bug from
    // a correct one. (Found by deliberately breaking `lhu` and watching 200
    // seeds pass anyway.)
    let mut image = vec![0u8; (SCRATCH_OFF + rv64::PAGE as u64) as usize];
    for o in (SCRATCH_OFF as usize..image.len()).step_by(8) {
        // A quarter all-zeros/all-ones/sign-bit words: uniform noise never
        // produces the byte and halfword patterns that separate the sign- and
        // zero-extending load forms from each other.
        let w = if rng.below(4) == 0 {
            rng.pick(&[0u64, u64::MAX, 0x8000_8000_8000_8000])
        } else {
            rng.next_u64()
        };
        image[o..o + 8].copy_from_slice(&w.to_le_bytes());
    }
    let mut listing: Vec<(u64, String)> = Vec::new();
    fn put(image: &mut [u8], listing: &mut Vec<(u64, String)>, start: u64, v: &[Ins]) {
        let mut o = start;
        for ins in v {
            let n = ins.size as usize;
            image[o as usize..o as usize + n].copy_from_slice(&ins.word.to_le_bytes()[..n]);
            listing.push((BASE + o, ins.text.clone()));
            o += ins.size;
        }
    }
    put(&mut image, &mut listing, EXIT_OFF, &exit_stub());
    put(&mut image, &mut listing, ENTRY_OFF, &pro);
    put(&mut image, &mut listing, body_start_off, &body);
    for (n, val) in table.iter().enumerate() {
        let o = (TABLE_OFF as usize) + 8 * n;
        image[o..o + 8].copy_from_slice(&val.to_le_bytes());
    }
    listing.sort_by_key(|(a, _)| *a);

    Program {
        seed,
        image,
        body_start: BASE + body_start_off,
        ecall_pc: BASE + ecall_off,
        body_len: body.len(),
        listing,
    }
}

/// Address of the scratch page, exported for tests that assert the
/// generator's memory-safety invariant.
pub const fn scratch_base() -> u64 {
    BASE + SCRATCH_OFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bodies must be a real mix, not "twelve `addi`s and a branch". A
    /// differential test whose programs are too simple passes for the wrong
    /// reason, so this asserts on the shape of what is generated.
    #[test]
    fn bodies_exercise_a_broad_instruction_mix() {
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        let mut compressed = 0usize;
        for seed in 0..20u64 {
            let p = program(seed);
            for (addr, text) in p.listing() {
                if addr < p.body_start || addr >= p.ecall_pc {
                    continue;
                }
                let mnemonic = text.split_whitespace().next().unwrap().to_string();
                if mnemonic.starts_with("c.") {
                    compressed += 1;
                }
                seen.insert(mnemonic);
            }
        }
        for required in [
            "add", "sub", "mul", "div", "rem", "ld", "sd", "lw", "beq", "jal", "jalr", "fence",
            "lr.d", "sc.d", "c.add", "c.srli", "amoadd.d", "sraiw", "lui", "auipc", "c.lw",
            "c.sd", "c.ldsp", "c.swsp", "c.beqz", "c.bnez", "c.j", "c.jr", "c.jalr",
            "c.addi4spn", "c.addi16sp",
        ] {
            assert!(seen.contains(required), "no `{required}` in 20 generated bodies: {seen:?}");
        }
        assert!(compressed > 100, "only {compressed} compressed instructions in 20 bodies");
        assert!(seen.len() > 45, "only {} distinct mnemonics: {seen:?}", seen.len());
    }

    /// The same seed must produce byte-identical programs, or a divergence
    /// report cannot be reproduced.
    #[test]
    fn generation_is_deterministic() {
        for seed in [0u64, 1, 7, 12345] {
            let (a, b) = (program(seed), program(seed));
            assert_eq!(a.image, b.image);
            assert_eq!(a.body_start, b.body_start);
            assert_eq!(a.ecall_pc, b.ecall_pc);
        }
        assert_ne!(program(0).image, program(1).image, "seeds must differ");
    }

    /// Every body must fit under `CODE_LIMIT`, which is the precondition for
    /// `auipc rt, 2` landing in the scratch page.
    #[test]
    fn bodies_stay_within_the_code_limit() {
        for seed in 0..200u64 {
            let p = program(seed);
            assert!(
                p.ecall_pc + 4 <= BASE + CODE_LIMIT,
                "seed {seed}: body ends at {:#x}, past the code limit",
                p.ecall_pc + 4
            );
            assert!(p.body_start < p.ecall_pc);
        }
    }

    /// The encoding guards (see the module doc) must hold across the whole
    /// seed range the campaign uses, not just the 200 the tests above walk.
    /// Now that they are unconditional `assert!`s, generating the programs
    /// *is* the check: a truncated field panics here instead of quietly
    /// encoding a different program into both simulators.
    #[test]
    fn no_seed_in_the_campaign_range_trips_an_encoding_guard() {
        for seed in 0..25_000u64 {
            let _ = program(seed);
        }
    }

    /// No body may contain a backward control transfer: the harness relies
    /// on every program terminating at the `ecall`.
    #[test]
    fn no_control_transfer_goes_backwards() {
        for seed in 0..200u64 {
            let p = program(seed);
            for (addr, text) in p.listing() {
                if addr < p.body_start {
                    continue;
                }
                if let Some(rest) = text.split(".+").nth(1) {
                    let d: i64 = rest.split_whitespace().next().unwrap().parse().unwrap();
                    assert!(d > 0, "seed {seed}: backward transfer at {addr:#x}: {text}");
                }
            }
        }
    }
}
