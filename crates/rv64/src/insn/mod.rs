pub mod rv64a;
pub mod rv64i;
pub mod rv64m;
pub mod rvc;

/// Field accessors shared by all decoders.
#[inline]
pub fn opcode(i: u32) -> u32 {
    i & 0x7F
}
#[inline]
pub fn rd(i: u32) -> usize {
    ((i >> 7) & 0x1F) as usize
}
#[inline]
pub fn rs1(i: u32) -> usize {
    ((i >> 15) & 0x1F) as usize
}
#[inline]
pub fn rs2(i: u32) -> usize {
    ((i >> 20) & 0x1F) as usize
}
#[inline]
pub fn funct3(i: u32) -> u32 {
    (i >> 12) & 0x7
}
#[inline]
pub fn funct7(i: u32) -> u32 {
    (i >> 25) & 0x7F
}

/// I-type immediate, sign-extended to 64 bits.
#[inline]
pub fn imm_i(i: u32) -> u64 {
    ((i as i32) >> 20) as i64 as u64
}

/// U-type immediate, sign-extended.
#[inline]
pub fn imm_u(i: u32) -> u64 {
    ((i & 0xFFFF_F000) as i32) as i64 as u64
}

/// S-type immediate, sign-extended.
#[inline]
pub fn imm_s(i: u32) -> u64 {
    let hi = (i & 0xFE00_0000) as i32 >> 20;
    let lo = ((i >> 7) & 0x1F) as i32;
    (hi | lo) as i64 as u64
}

/// B-type immediate, sign-extended (always even).
#[inline]
pub fn imm_b(i: u32) -> u64 {
    let imm12 = ((i >> 31) & 1) << 12;
    let imm10_5 = ((i >> 25) & 0x3F) << 5;
    let imm4_1 = ((i >> 8) & 0xF) << 1;
    let imm11 = ((i >> 7) & 1) << 11;
    let v = (imm12 | imm11 | imm10_5 | imm4_1) as i32;
    ((v << 19) >> 19) as i64 as u64
}

/// J-type immediate, sign-extended (always even).
#[inline]
pub fn imm_j(i: u32) -> u64 {
    let imm20 = ((i >> 31) & 1) << 20;
    let imm10_1 = ((i >> 21) & 0x3FF) << 1;
    let imm11 = ((i >> 20) & 1) << 11;
    let imm19_12 = ((i >> 12) & 0xFF) << 12;
    let v = (imm20 | imm19_12 | imm11 | imm10_1) as i32;
    ((v << 11) >> 11) as i64 as u64
}
