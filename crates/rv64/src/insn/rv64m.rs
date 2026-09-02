use super::*;
use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::exception::Exception;
use crate::uart::ConsoleSink;

/// RISC-V defines division by zero and signed overflow rather than trapping.
pub fn div_signed(a: i64, b: i64) -> i64 {
    if b == 0 {
        -1
    } else if a == i64::MIN && b == -1 {
        i64::MIN
    } else {
        a / b
    }
}

pub fn rem_signed(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else if a == i64::MIN && b == -1 {
        0
    } else {
        a % b
    }
}

/// 32-bit signed division with RISC-V semantics.
pub fn div_signed_w(a: i32, b: i32) -> i32 {
    if b == 0 {
        -1
    } else if a == i32::MIN && b == -1 {
        i32::MIN
    } else {
        a / b
    }
}

/// 32-bit signed remainder with RISC-V semantics.
pub fn rem_signed_w(a: i32, b: i32) -> i32 {
    if b == 0 {
        a
    } else if a == i32::MIN && b == -1 {
        0
    } else {
        a % b
    }
}

pub fn execute<B: MemBacking, S: ConsoleSink>(
    cpu: &mut Cpu,
    _bus: &mut Bus<B, S>,
    insn: u32,
) -> Result<bool, Exception> {
    if funct7(insn) != 0x01 {
        return Ok(false);
    }
    let (rd, a, b) = (rd(insn), cpu.reg(rs1(insn)), cpu.reg(rs2(insn)));
    let (sa, sb) = (a as i64, b as i64);

    match opcode(insn) {
        0x33 => {
            let v = match funct3(insn) {
                0x0 => a.wrapping_mul(b),                                        // MUL
                0x1 => (((sa as i128) * (sb as i128)) >> 64) as u64,              // MULH
                0x2 => (((sa as i128) * (b as u128 as i128)) >> 64) as u64,       // MULHSU
                0x3 => (((a as u128) * (b as u128)) >> 64) as u64,                // MULHU
                0x4 => div_signed(sa, sb) as u64,                                 // DIV
                0x5 => if b == 0 { u64::MAX } else { a / b },                     // DIVU
                0x6 => rem_signed(sa, sb) as u64,                                 // REM
                0x7 => if b == 0 { a } else { a % b },                            // REMU
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v);
            Ok(true)
        }
        0x3B => {
            let (wa, wb) = (a as i32, b as i32);
            let v: i32 = match funct3(insn) {
                0x0 => wa.wrapping_mul(wb),                                       // MULW
                0x4 => div_signed_w(wa, wb),                                       // DIVW
                0x5 => if wb == 0 { -1 } else { ((wa as u32) / (wb as u32)) as i32 }, // DIVUW
                0x6 => rem_signed_w(wa, wb),                                       // REMW
                0x7 => if wb == 0 { wa } else { ((wa as u32) % (wb as u32)) as i32 }, // REMUW
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v as i64 as u64);
            Ok(true)
        }
        _ => Ok(false),
    }
}
