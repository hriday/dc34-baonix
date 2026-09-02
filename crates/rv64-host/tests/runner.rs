//! End-to-end tests driven through `rv64_host::run_program_capturing` —
//! the same `run_until` loop `main.rs` boots a kernel with, a real SBI
//! stub, a ticking CLINT and a console sink.

/// A hand-assembled S-mode program that writes "hi" via SBI console_putchar
/// and then shuts down. Exercises the whole runner path end to end.
#[test]
fn runner_prints_sbi_console_output_and_halts() {
    let program: Vec<u32> = vec![
        0x00100893, // li a7, 1        (console_putchar)
        0x06800513, // li a0, 'h'
        0x00000073, // ecall
        0x00100893, // li a7, 1
        0x06900513, // li a0, 'i'
        0x00000073, // ecall
        0x00800893, // li a7, 8        (shutdown)
        0x00000073, // ecall
    ];
    let out = rv64_host::run_program_capturing(&program, 1000);
    assert_eq!(out, "hi");
}

// --- Instruction encoders -------------------------------------------------
//
// The timer program below is ~25 instructions with a backward branch and
// six CSR accesses. Hand-computed hex at that length is a liability: a
// mistyped field would silently make this test exercise a different program
// than its comments claim, which is precisely the class of defect the rest
// of this branch has been closing. These build the words from named fields
// instead.

const OP_IMM: u32 = 0x13;
const BRANCH: u32 = 0x63;
const AUIPC: u32 = 0x17;
const SYSTEM: u32 = 0x73;

const CSR_SSTATUS: u32 = 0x100;
const CSR_SIE: u32 = 0x104;
const CSR_STVEC: u32 = 0x105;
const CSR_TIME: u32 = 0xC01;

/// `mstatus`/`sstatus` SIE — the S-mode global interrupt enable.
const SSTATUS_SIE: i32 = 1 << 1;
/// `sie` STIE — the supervisor *timer* interrupt enable.
const SIE_STIE: i32 = 1 << 5;

const A0: u32 = 10;
const A7: u32 = 17;

fn i_type(opcode: u32, funct3: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    ((imm as u32) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
}

fn b_type(funct3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
    let i = imm as u32;
    (((i >> 12) & 1) << 31)
        | (((i >> 5) & 0x3F) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (((i >> 1) & 0xF) << 8)
        | (((i >> 11) & 1) << 7)
        | BRANCH
}

fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(OP_IMM, 0, rd, rs1, imm)
}
fn li(rd: u32, imm: i32) -> u32 {
    addi(rd, 0, imm)
}
fn auipc(rd: u32, imm20: u32) -> u32 {
    (imm20 << 12) | (rd << 7) | AUIPC
}
fn bne(rs1: u32, rs2: u32, imm: i32) -> u32 {
    b_type(1, rs1, rs2, imm)
}
/// `csrw csr, rs1` — CSRRW with rd = x0.
fn csrw(csr: u32, rs1: u32) -> u32 {
    i_type(SYSTEM, 1, 0, rs1, csr as i32)
}
/// `csrs csr, rs1` — CSRRS with rd = x0.
fn csrs(csr: u32, rs1: u32) -> u32 {
    i_type(SYSTEM, 2, 0, rs1, csr as i32)
}
/// `csrr rd, csr` — CSRRS with rs1 = x0, a pure read.
fn csrr(rd: u32, csr: u32) -> u32 {
    i_type(SYSTEM, 2, rd, 0, csr as i32)
}
const ECALL: u32 = 0x0000_0073;
const SRET: u32 = 0x1020_0073;

const SBI_SET_TIMER: i32 = 0;
const SBI_CONSOLE_PUTCHAR: i32 = 1;
const SBI_SHUTDOWN: i32 = 8;

/// Ticks between timer deadlines. The CLINT advances by one per retired
/// instruction (`run_until`), so this is "100 instructions from now".
const DELTA: i32 = 100;
/// Iterations of the main work loop, two instructions each.
const WORK: i32 = 200;

/// Builds the timer program described in the test below.
fn timer_program() -> Vec<u32> {
    // Registers: t0/x5 scratch, t2/x7 the work counter, a0/a7 for SBI.
    // The handler only touches a0 and a7, and the main path holds nothing
    // live in those across a point where an interrupt can land.
    const T0: u32 = 5;
    const T2: u32 = 7;

    let setup_and_main: Vec<u32> = vec![
        // --- install the trap vector ---
        auipc(T0, 0),          // t0 = &this instruction
        addi(T0, T0, 0),       // t0 += handler offset  (patched below)
        csrw(CSR_STVEC, T0),   // stvec = handler
        // --- enable the supervisor timer interrupt ---
        li(T0, SIE_STIE),
        csrs(CSR_SIE, T0),
        li(T0, SSTATUS_SIE),
        csrs(CSR_SSTATUS, T0),
        // --- arm the first deadline, exactly as riscv_clock_next_event
        //     does: next = get_cycles64() + delta, then sbi_set_timer. ---
        csrr(A0, CSR_TIME),
        addi(A0, A0, DELTA),
        li(A7, SBI_SET_TIMER),
        ECALL,
        // --- main work: a loop that must actually make progress ---
        li(T2, WORK),
        addi(T2, T2, -1),  // loop:
        bne(T2, 0, -4),    //   back to `loop`
        // --- shut down. SIE is cleared first so no interrupt can land
        //     between loading a7 and the ecall and clobber it. ---
        csrw(CSR_SSTATUS, 0),
        li(A7, SBI_SHUTDOWN),
        ECALL,
    ];

    let handler: Vec<u32> = vec![
        // One 't' per timer tick, so the count is observable on the console.
        li(A7, SBI_CONSOLE_PUTCHAR),
        li(A0, b't' as i32),
        ECALL,
        // Re-arm from `time`, the way a real clock driver does.
        csrr(A0, CSR_TIME),
        addi(A0, A0, DELTA),
        li(A7, SBI_SET_TIMER),
        ECALL,
        SRET,
    ];

    let mut p = setup_and_main;
    // Patch the handler offset now that the prologue's length is known. The
    // `auipc` at index 0 captured its own address, so the displacement is
    // measured from there.
    let handler_off = 4 * p.len() as i32;
    p[1] = addi(T0, T0, handler_off);
    p.extend(handler);
    p
}

/// The `time` CSR (0xC01) must report the CLINT's `mtime`, observed rather
/// than reasoned about.
///
/// This program is `riscv_clock_next_event` in miniature: it arms a
/// deadline at `get_cycles64() + delta`, and its timer handler re-arms the
/// same way on every tick. That is the whole shape of the Critical. With
/// `time` reading a constant zero — a permanently-empty slot in the flat
/// CSR array, which is what an unhandled CSR address returns — every re-arm
/// programs `mtimecmp` to a small constant that `mtime` has already run
/// past, so the interrupt is pending again before the interrupted
/// instruction can retire, the main loop never advances a single step, and
/// the guest spins in its timer handler forever. Linux's boot dies there
/// with no console output at all.
///
/// Two things are asserted, and they fail in different directions:
///
///   * the program *terminates* — `run_program_capturing` panics on the
///     instruction cap, so a livelock is a hard failure and not a hang;
///   * the handler ran a handful of times, not hundreds. This is the
///     graceful half: it catches a `time` that advances but wrongly (too
///     slowly, or not monotonically with `mtime`), which would still
///     terminate but would preempt the work loop far too often.
#[test]
fn a_timer_rearmed_from_rdtime_does_not_livelock_the_run_loop() {
    let out = rv64_host::run_program_capturing(&timer_program(), 100_000);
    let ticks = out.matches('t').count();

    assert!(
        ticks >= 2,
        "the timer fired {ticks} times; the handler's re-arm never produced a \
         second tick, so `set_timer` and the STIP forward are not round-tripping"
    );
    assert!(
        ticks <= 20,
        "the timer fired {ticks} times over ~{} instructions of work at a \
         {DELTA}-tick period — it is refiring far faster than `mtime` advances, \
         which is the livelock signature",
        2 * WORK
    );
}
