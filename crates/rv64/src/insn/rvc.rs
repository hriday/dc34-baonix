/// Expands a 16-bit compressed instruction into its 32-bit equivalent.
/// Returns `None` if `half` is not a compressed instruction.
pub fn expand(half: u16) -> Option<u32> {
    let h = half as u32;
    let op = h & 0x3;
    if op == 0x3 {
        return None; // 32-bit instruction
    }
    let funct3 = (h >> 13) & 0x7;
    let rd = (h >> 7) & 0x1F;
    let rs2 = (h >> 2) & 0x1F;
    // 3-bit register fields address x8..x15
    let rdp = 8 + ((h >> 2) & 0x7);
    let rs1p = 8 + ((h >> 7) & 0x7);
    let rs2p = 8 + ((h >> 2) & 0x7);

    let i_type = |imm: u32, rs1: u32, f3: u32, rd: u32, opc: u32| -> u32 {
        ((imm & 0xFFF) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
    };
    let s_type = |imm: u32, rs2: u32, rs1: u32, f3: u32, opc: u32| -> u32 {
        ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12)
            | ((imm & 0x1F) << 7) | opc
    };
    let r_type = |funct7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, opc: u32| -> u32 {
        (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
    };
    // Inverse of `imm_b` in mod.rs: scrambles a sign-extended branch offset
    // into the base ISA's split B-immediate field. `imm_b(b_type(x, ...))`
    // must return `x` for any in-range offset — see the round-trip tests.
    let b_type = |imm: u32, rs2: u32, rs1: u32, f3: u32, opc: u32| -> u32 {
        (((imm >> 12) & 1) << 31)
            | (((imm >> 5) & 0x3F) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (f3 << 12)
            | (((imm >> 1) & 0xF) << 8)
            | (((imm >> 11) & 1) << 7)
            | opc
    };
    // Inverse of `imm_j` in mod.rs: scrambles a sign-extended jump offset
    // into the base ISA's split J-immediate field.
    let j_type = |imm: u32, rd: u32, opc: u32| -> u32 {
        (((imm >> 20) & 1) << 31)
            | (((imm >> 1) & 0x3FF) << 21)
            | (((imm >> 11) & 1) << 20)
            | (((imm >> 12) & 0xFF) << 12)
            | (rd << 7)
            | opc
    };

    match (op, funct3) {
        // C.ADDI4SPN -> addi rd', x2, nzuimm
        (0x0, 0x0) => {
            let imm = (((h >> 7) & 0xF) << 6)   // nzuimm[9:6]
                | (((h >> 11) & 0x3) << 4)      // nzuimm[5:4]
                | (((h >> 5) & 0x1) << 3)       // nzuimm[3]
                | (((h >> 6) & 0x1) << 2);      // nzuimm[2]
            if imm == 0 { return None; }
            Some(i_type(imm, 2, 0x0, rdp, 0x13))
        }
        // C.LW -> lw rd', off(rs1')
        (0x0, 0x2) => {
            let imm = (((h >> 5) & 1) << 6) | (((h >> 10) & 0x7) << 3) | (((h >> 6) & 1) << 2);
            Some(i_type(imm, rs1p, 0x2, rdp, 0x03))
        }
        // C.LD -> ld rd', off(rs1')
        (0x0, 0x3) => {
            let imm = (((h >> 5) & 0x3) << 6) | (((h >> 10) & 0x7) << 3);
            Some(i_type(imm, rs1p, 0x3, rdp, 0x03))
        }
        // C.SW -> sw rs2', off(rs1')
        (0x0, 0x6) => {
            let imm = (((h >> 5) & 1) << 6) | (((h >> 10) & 0x7) << 3) | (((h >> 6) & 1) << 2);
            Some(s_type(imm, rs2p, rs1p, 0x2, 0x23))
        }
        // C.SD -> sd rs2', off(rs1')
        (0x0, 0x7) => {
            let imm = (((h >> 5) & 0x3) << 6) | (((h >> 10) & 0x7) << 3);
            Some(s_type(imm, rs2p, rs1p, 0x3, 0x23))
        }
        // C.ADDI / C.NOP -> addi rd, rd, nzimm
        (0x1, 0x0) => {
            let imm = sext6((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F));
            Some(i_type(imm, rd, 0x0, rd, 0x13))
        }
        // C.ADDIW -> addiw rd, rd, imm
        (0x1, 0x1) => {
            if rd == 0 { return None; }
            let imm = sext6((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F));
            Some(i_type(imm, rd, 0x0, rd, 0x1B))
        }
        // C.LI -> addi rd, x0, imm
        (0x1, 0x2) => {
            let imm = sext6((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F));
            Some(i_type(imm, 0, 0x0, rd, 0x13))
        }
        // C.ADDI16SP (rd == 2) -> addi x2, x2, nzimm
        // C.LUI (rd != 2) -> lui rd, nzimm[17:12]. rd == 0 is a HINT, not a
        // reserved encoding, so it falls through to the normal expansion —
        // it produces `lui x0, nzimm`, which set_reg's x0-write-discard
        // already turns into a no-op.
        (0x1, 0x3) => {
            if rd == 2 {
                let imm = sext10(
                    (((h >> 12) & 1) << 9)   // nzimm[9]
                        | (((h >> 3) & 0x3) << 7)  // nzimm[8:7]
                        | (((h >> 5) & 1) << 6)    // nzimm[6]
                        | (((h >> 2) & 1) << 5)    // nzimm[5]
                        | (((h >> 6) & 1) << 4),   // nzimm[4]
                );
                if imm == 0 { return None; }
                Some(i_type(imm, 2, 0x0, 2, 0x13))
            } else {
                let sign = (h >> 12) & 1;
                let lo5 = (h >> 2) & 0x1F;
                let raw6 = (sign << 5) | lo5;
                if raw6 == 0 { return None; }
                let nzimm = sext6(raw6);
                Some(((nzimm & 0xFFFFF) << 12) | (rd << 7) | 0x37)
            }
        }
        // C.SRLI / C.SRAI / C.ANDI / C.SUB / C.XOR / C.OR / C.AND / C.SUBW / C.ADDW
        (0x1, 0x4) => {
            let funct2 = (h >> 10) & 0x3;
            match funct2 {
                0x0 => {
                    // C.SRLI -> srli rs1', rs1', shamt
                    let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F);
                    Some(i_type(shamt, rs1p, 0x5, rs1p, 0x13))
                }
                0x1 => {
                    // C.SRAI -> srai rs1', rs1', shamt
                    let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F);
                    Some(i_type(shamt | 0x400, rs1p, 0x5, rs1p, 0x13))
                }
                0x2 => {
                    // C.ANDI -> andi rs1', rs1', imm
                    let imm = sext6((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F));
                    Some(i_type(imm, rs1p, 0x7, rs1p, 0x13))
                }
                _ => {
                    // register-register group: SUB/XOR/OR/AND (bit12=0),
                    // SUBW/ADDW (bit12=1; the other two bits6:5 combos are reserved)
                    let bit12 = (h >> 12) & 1;
                    let sel = (h >> 5) & 0x3;
                    if bit12 == 0 {
                        match sel {
                            0x0 => Some(r_type(0x20, rs2p, rs1p, 0x0, rs1p, 0x33)), // SUB
                            0x1 => Some(r_type(0x00, rs2p, rs1p, 0x4, rs1p, 0x33)), // XOR
                            0x2 => Some(r_type(0x00, rs2p, rs1p, 0x6, rs1p, 0x33)), // OR
                            _ => Some(r_type(0x00, rs2p, rs1p, 0x7, rs1p, 0x33)),   // AND
                        }
                    } else {
                        match sel {
                            0x0 => Some(r_type(0x20, rs2p, rs1p, 0x0, rs1p, 0x3B)), // SUBW
                            0x1 => Some(r_type(0x00, rs2p, rs1p, 0x0, rs1p, 0x3B)), // ADDW
                            _ => None, // reserved
                        }
                    }
                }
            }
        }
        // C.J -> jal x0, offset
        (0x1, 0x5) => {
            let imm = sext12(
                (((h >> 12) & 1) << 11)     // imm[11]
                    | (((h >> 11) & 1) << 4)     // imm[4]
                    | (((h >> 9) & 0x3) << 8)    // imm[9:8]
                    | (((h >> 8) & 1) << 10)     // imm[10]
                    | (((h >> 7) & 1) << 6)      // imm[6]
                    | (((h >> 6) & 1) << 7)      // imm[7]
                    | (((h >> 3) & 0x7) << 1)    // imm[3:1]
                    | (((h >> 2) & 1) << 5),     // imm[5]
            );
            Some(j_type(imm, 0, 0x6F))
        }
        // C.BEQZ -> beq rs1', x0, offset
        (0x1, 0x6) => {
            let imm = sext9(
                (((h >> 12) & 1) << 8)      // imm[8]
                    | (((h >> 10) & 0x3) << 3)   // imm[4:3]
                    | (((h >> 5) & 0x3) << 6)    // imm[7:6]
                    | (((h >> 3) & 0x3) << 1)    // imm[2:1]
                    | (((h >> 2) & 1) << 5),     // imm[5]
            );
            Some(b_type(imm, 0, rs1p, 0x0, 0x63))
        }
        // C.BNEZ -> bne rs1', x0, offset
        (0x1, 0x7) => {
            let imm = sext9(
                (((h >> 12) & 1) << 8)      // imm[8]
                    | (((h >> 10) & 0x3) << 3)   // imm[4:3]
                    | (((h >> 5) & 0x3) << 6)    // imm[7:6]
                    | (((h >> 3) & 0x3) << 1)    // imm[2:1]
                    | (((h >> 2) & 1) << 5),     // imm[5]
            );
            Some(b_type(imm, 0, rs1p, 0x1, 0x63))
        }
        // C.SLLI -> slli rd, rd, shamt
        (0x2, 0x0) => {
            let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1F);
            Some(i_type(shamt, rd, 0x1, rd, 0x13))
        }
        // C.LWSP -> lw rd, uimm(x2)
        (0x2, 0x2) => {
            if rd == 0 { return None; }
            let imm = (((h >> 12) & 1) << 5)   // uimm[5]
                | (((h >> 4) & 0x7) << 2)      // uimm[4:2]
                | (((h >> 2) & 0x3) << 6);     // uimm[7:6]
            Some(i_type(imm, 2, 0x2, rd, 0x03))
        }
        // C.LDSP -> ld rd, uimm(x2)
        (0x2, 0x3) => {
            if rd == 0 { return None; }
            let imm = (((h >> 12) & 1) << 5)   // uimm[5]
                | (((h >> 5) & 0x3) << 3)      // uimm[4:3]
                | (((h >> 2) & 0x7) << 6);     // uimm[8:6]
            Some(i_type(imm, 2, 0x3, rd, 0x03))
        }
        // C.SWSP -> sw rs2, uimm(x2)
        (0x2, 0x6) => {
            let imm = (((h >> 9) & 0xF) << 2)  // uimm[5:2]
                | (((h >> 7) & 0x3) << 6);     // uimm[7:6]
            Some(s_type(imm, rs2, 2, 0x2, 0x23))
        }
        // C.SDSP -> sd rs2, uimm(x2)
        (0x2, 0x7) => {
            let imm = (((h >> 10) & 0x7) << 3) // uimm[5:3]
                | (((h >> 7) & 0x7) << 6);     // uimm[8:6]
            Some(s_type(imm, rs2, 2, 0x3, 0x23))
        }
        // C.MV / C.ADD / C.JR / C.JALR / C.EBREAK
        (0x2, 0x4) => {
            let bit12 = (h >> 12) & 1;
            if bit12 == 0 {
                if rs2 == 0 {
                    if rd == 0 {
                        return None; // reserved
                    }
                    return Some(i_type(0, rd, 0x0, 0, 0x67)); // C.JR
                }
                Some((rs2 << 20) | (0 << 15) | (rd << 7) | 0x33) // C.MV
            } else {
                if rs2 == 0 {
                    if rd == 0 {
                        // C.EBREAK -> the canonical 32-bit ebreak encoding.
                        return Some(0x0010_0073);
                    }
                    return Some(i_type(0, rd, 0x0, 1, 0x67)); // C.JALR
                }
                Some((rs2 << 20) | (rd << 15) | (rd << 7) | 0x33) // C.ADD
            }
        }
        _ => None,
    }
}

#[inline]
fn sext6(v: u32) -> u32 {
    (((v as i32) << 26) >> 26) as u32
}

/// Sign-extends the 9-bit branch offset used by C.BEQZ / C.BNEZ.
#[inline]
fn sext9(v: u32) -> u32 {
    (((v as i32) << 23) >> 23) as u32
}

/// Sign-extends the 10-bit stack-adjustment immediate used by C.ADDI16SP.
#[inline]
fn sext10(v: u32) -> u32 {
    (((v as i32) << 22) >> 22) as u32
}

/// Sign-extends the 12-bit jump offset used by C.J.
#[inline]
fn sext12(v: u32) -> u32 {
    (((v as i32) << 20) >> 20) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// c.addi x1, 5  ->  addi x1, x1, 5
    #[test]
    fn c_addi_expands() {
        assert_eq!(expand(0x0095), Some(0x0050_8093));
    }

    /// c.nop -> addi x0, x0, 0
    #[test]
    fn c_nop_expands_to_canonical_nop() {
        assert_eq!(expand(0x0001), Some(0x0000_0013));
    }

    /// A 32-bit instruction pattern is not compressed.
    #[test]
    fn full_length_pattern_is_rejected() {
        assert_eq!(expand(0x0093), None);
    }

    /// c.jr x0 is reserved, not a jump to address 0.
    #[test]
    fn c_jr_with_rd_zero_is_reserved() {
        assert_eq!(expand(0x8002), None);
    }

    /// c.jalr x0 is C.EBREAK, not a jump to address 0.
    #[test]
    fn c_ebreak_expands_to_the_canonical_ebreak() {
        assert_eq!(expand(0x9002), Some(0x0010_0073));
    }

    /// c.addi4spn x8, x2, 8  ->  addi x8, x2, 8
    #[test]
    fn c_addi4spn_expands() {
        assert_eq!(expand(0x0020), Some(0x0081_0413));
    }

    /// c.lw x8, 0(x9)  ->  lw x8, 0(x9)
    #[test]
    fn c_lw_expands() {
        assert_eq!(expand(0x4080), Some(0x0004_A403));
    }

    /// c.ld x8, 0(x9)  ->  ld x8, 0(x9)
    #[test]
    fn c_ld_expands() {
        assert_eq!(expand(0x6080), Some(0x0004_B403));
    }

    /// c.sw x8, 0(x9)  ->  sw x8, 0(x9)
    #[test]
    fn c_sw_expands() {
        assert_eq!(expand(0xC080), Some(0x0084_A023));
    }

    /// c.sd x8, 0(x9)  ->  sd x8, 0(x9)
    #[test]
    fn c_sd_expands() {
        assert_eq!(expand(0xE080), Some(0x0084_B023));
    }

    /// c.addiw x1, 5  ->  addiw x1, x1, 5
    #[test]
    fn c_addiw_expands() {
        assert_eq!(expand(0x2095), Some(0x0050_809B));
    }

    /// c.li x1, 5  ->  addi x1, x0, 5
    #[test]
    fn c_li_expands() {
        assert_eq!(expand(0x4095), Some(0x0050_0093));
    }

    /// c.mv x1, x2  ->  add x1, x0, x2
    #[test]
    fn c_mv_expands() {
        assert_eq!(expand(0x808A), Some(0x0020_00B3));
    }

    /// c.add x1, x2  ->  add x1, x1, x2
    #[test]
    fn c_add_expands() {
        assert_eq!(expand(0x908A), Some(0x0020_80B3));
    }

    // ---- Round 2 continuation: quadrant 1 and quadrant 2 SP-relative ops ----
    //
    // Every fixed-vector test below (halfword, expected 32-bit word) was
    // independently cross-checked two ways before being written here: (1) by
    // hand, tracing the field layout bit by bit; (2) with a small Python
    // oracle that builds the halfword by direct bit placement (independent
    // of this file's extraction logic) and computes the expected 32-bit word
    // from the semantic values (rd, imm, rs2) directly via fresh I/S/B/J/R
    // encoders, then confirms this file's `expand()`-equivalent logic
    // reproduces the same word. All values below matched on both checks.

    /// c.lui x1, 0x15  ->  lui x1, 0x15000  (positive nzimm)
    #[test]
    fn c_lui_expands() {
        assert_eq!(expand(0x60D5), Some(0x0001_50B7));
    }

    /// c.lui x3, -32  ->  lui x3, 0xFFFE0  (nzimm sign bit set)
    #[test]
    fn c_lui_sign_extends_negative_immediate() {
        assert_eq!(expand(0x7181), Some(0xFFFE_01B7));
    }

    /// c.lui with a zero immediate is reserved.
    #[test]
    fn c_lui_zero_immediate_is_reserved() {
        assert_eq!(expand(0x6281), None);
    }

    /// c.lui x0, nzimm is a HINT, not a reserved encoding, per the RVC spec —
    /// unlike C.ADDIW's rd == 0, which genuinely is reserved. It must expand
    /// rather than trap; the result writes to x0 and is therefore a no-op.
    ///
    /// Halfword derived independently: op=01 (bits1:0), funct3=011
    /// (bits15:13), rd=0 (bits11:7), sign=0 (bit12), nzimm[16:12]=00001
    /// (bits6:2, so the immediate is nonzero) -> 0110_0000_0000_0101 = 0x6005.
    #[test]
    fn c_lui_with_rd_zero_is_a_hint_not_reserved() {
        let expanded = expand(0x6005);
        assert!(expanded.is_some(), "c.lui x0 is a HINT and must not trap");
        let w = expanded.unwrap();
        assert_eq!(w & 0x7F, 0x37, "expands to LUI");
        assert_eq!((w >> 7) & 0x1F, 0, "destination is x0");
    }

    /// c.addi16sp x2, 32  ->  addi x2, x2, 32
    #[test]
    fn c_addi16sp_expands() {
        assert_eq!(expand(0x6105), Some(0x0201_0113));
    }

    /// c.addi16sp x2, -16  ->  addi x2, x2, -16
    #[test]
    fn c_addi16sp_negative_expands() {
        assert_eq!(expand(0x717D), Some(0xFF01_0113));
    }

    /// c.addi16sp with a zero immediate is reserved.
    #[test]
    fn c_addi16sp_zero_immediate_is_reserved() {
        assert_eq!(expand(0x6101), None);
    }

    /// c.srli x9, x9, 19  ->  srli x9, x9, 19
    #[test]
    fn c_srli_expands() {
        assert_eq!(expand(0x80CD), Some(0x0134_D493));
    }

    /// c.srai x10, x10, 44  ->  srai x10, x10, 44
    #[test]
    fn c_srai_expands() {
        assert_eq!(expand(0x9531), Some(0x42C5_5513));
    }

    /// c.andi x11, x11, -5  ->  andi x11, x11, -5
    #[test]
    fn c_andi_expands() {
        assert_eq!(expand(0x99ED), Some(0xFFB5_F593));
    }

    /// c.sub x12, x13  ->  sub x12, x12, x13
    #[test]
    fn c_sub_expands() {
        assert_eq!(expand(0x8E15), Some(0x40D6_0633));
    }

    /// c.xor x12, x13  ->  xor x12, x12, x13
    #[test]
    fn c_xor_expands() {
        assert_eq!(expand(0x8E35), Some(0x00D6_4633));
    }

    /// c.or x12, x13  ->  or x12, x12, x13
    #[test]
    fn c_or_expands() {
        assert_eq!(expand(0x8E55), Some(0x00D6_6633));
    }

    /// c.and x12, x13  ->  and x12, x12, x13
    #[test]
    fn c_and_expands() {
        assert_eq!(expand(0x8E75), Some(0x00D6_7633));
    }

    /// c.subw x12, x13  ->  subw x12, x12, x13
    #[test]
    fn c_subw_expands() {
        assert_eq!(expand(0x9E15), Some(0x40D6_063B));
    }

    /// c.addw x12, x13  ->  addw x12, x12, x13
    #[test]
    fn c_addw_expands() {
        assert_eq!(expand(0x9E35), Some(0x00D6_063B));
    }

    /// The register-register group's bit12=1, bits6:5=10 combination is
    /// reserved (only SUBW=00 and ADDW=01 are defined for RV64).
    #[test]
    fn and_group_reserved_combination_10_is_rejected() {
        assert_eq!(expand(0x9E55), None);
    }

    /// Same group, bits6:5=11 — also reserved.
    #[test]
    fn and_group_reserved_combination_11_is_rejected() {
        assert_eq!(expand(0x9E75), None);
    }

    /// c.j +100  ->  jal x0, +100 (nonzero positive offset)
    #[test]
    fn c_j_expands_with_nonzero_offset() {
        assert_eq!(expand(0xA095), Some(0x0640_006F));
    }

    /// c.j -100  ->  jal x0, -100 (negative offset must sign-extend)
    #[test]
    fn c_j_expands_with_negative_offset() {
        assert_eq!(expand(0xBF71), Some(0xF9DF_F06F));
    }

    /// c.beqz x8, +4  ->  beq x8, x0, +4 (nonzero positive offset)
    #[test]
    fn c_beqz_expands_with_nonzero_offset() {
        assert_eq!(expand(0xC011), Some(0x0004_0263));
    }

    /// c.beqz x10, -100  ->  beq x10, x0, -100 (negative offset)
    #[test]
    fn c_beqz_expands_with_negative_offset() {
        assert_eq!(expand(0xDD51), Some(0xF805_0EE3));
    }

    /// c.bnez x11, +100  ->  bne x11, x0, +100 (nonzero positive offset)
    #[test]
    fn c_bnez_expands_with_nonzero_offset() {
        assert_eq!(expand(0xE1B5), Some(0x0605_9263));
    }

    /// c.bnez x9, -2  ->  bne x9, x0, -2 (negative offset)
    #[test]
    fn c_bnez_expands_with_negative_offset() {
        assert_eq!(expand(0xFCFD), Some(0xFE04_9FE3));
    }

    /// c.slli x5, x5, 33  ->  slli x5, x5, 33
    #[test]
    fn c_slli_expands() {
        assert_eq!(expand(0x1286), Some(0x0212_9293));
    }

    /// c.lwsp x6, 84(x2)  ->  lw x6, 84(x2)
    #[test]
    fn c_lwsp_expands() {
        assert_eq!(expand(0x4356), Some(0x0541_2303));
    }

    /// c.lwsp with rd == x0 is reserved.
    #[test]
    fn c_lwsp_rd_zero_is_reserved() {
        assert_eq!(expand(0x4002), None);
    }

    /// c.ldsp x7, 168(x2)  ->  ld x7, 168(x2)
    #[test]
    fn c_ldsp_expands() {
        assert_eq!(expand(0x73AA), Some(0x0A81_3383));
    }

    /// c.ldsp with rd == x0 is reserved.
    #[test]
    fn c_ldsp_rd_zero_is_reserved() {
        assert_eq!(expand(0x6002), None);
    }

    /// c.swsp x14, 84(x2)  ->  sw x14, 84(x2)
    #[test]
    fn c_swsp_expands() {
        assert_eq!(expand(0xCABA), Some(0x04E1_2A23));
    }

    /// c.sdsp x15, 168(x2)  ->  sd x15, 168(x2)
    #[test]
    fn c_sdsp_expands() {
        assert_eq!(expand(0xF53E), Some(0x0AF1_3423));
    }

    /// Hand-encodes a C.J halfword for a given (even, in-range) offset,
    /// independently of `expand`'s own field extraction, so the round-trip
    /// test below is a genuine check of the J-type builder rather than a
    /// tautology.
    fn encode_cj(off: i32) -> u16 {
        let imm = (off as u32) & 0xFFF;
        let b11 = (imm >> 11) & 1;
        let b4 = (imm >> 4) & 1;
        let b9_8 = (imm >> 8) & 0x3;
        let b10 = (imm >> 10) & 1;
        let b6 = (imm >> 6) & 1;
        let b7 = (imm >> 7) & 1;
        let b3_1 = (imm >> 1) & 0x7;
        let b5 = (imm >> 5) & 1;
        let h = (0b101u32 << 13)
            | (b11 << 12)
            | (b4 << 11)
            | (b9_8 << 9)
            | (b10 << 8)
            | (b6 << 7)
            | (b7 << 6)
            | (b3_1 << 3)
            | (b5 << 2)
            | 0b01;
        h as u16
    }

    /// Hand-encodes a C.BEQZ (funct3=0x6) / C.BNEZ (funct3=0x7) halfword for
    /// a given (even, in-range) offset and rs1' register, independently of
    /// `expand`'s own field extraction.
    fn encode_cbz(off: i32, rs1p_val: u32, funct3: u32) -> u16 {
        let imm = (off as u32) & 0x1FF;
        let b8 = (imm >> 8) & 1;
        let b4_3 = (imm >> 3) & 0x3;
        let b7_6 = (imm >> 6) & 0x3;
        let b2_1 = (imm >> 1) & 0x3;
        let b5 = (imm >> 5) & 1;
        let h = (funct3 << 13)
            | (b8 << 12)
            | (b4_3 << 10)
            | (rs1p_val << 7)
            | (b7_6 << 5)
            | (b2_1 << 3)
            | (b5 << 2)
            | 0b01;
        h as u16
    }

    /// The B-type and J-type builders inside `expand` unscramble the RVC
    /// offset and re-scramble it into the base ISA's split immediate field —
    /// the most error-prone step in this file. This feeds several offsets,
    /// including negative ones, through encode -> expand -> imm_b/imm_j and
    /// checks that the original offset comes back out, per the coordinator's
    /// suggested check.
    #[test]
    fn branch_and_jump_offsets_round_trip_through_imm_b_and_imm_j() {
        for off in [0i32, 2, 4, 100, -2, -4, -100, 2046, -2048] {
            let h = encode_cj(off);
            let insn = expand(h).unwrap_or_else(|| panic!("C.J offset {off} failed to expand"));
            assert_eq!(
                crate::insn::imm_j(insn) as i64,
                off as i64,
                "C.J offset {off} round-tripped incorrectly, halfword 0x{h:04X}"
            );
        }
        for off in [0i32, 4, -2, -100, 100, 254, -256] {
            for (rs1p_val, funct3) in [(0u32, 0x6u32), (1u32, 0x7u32)] {
                let h = encode_cbz(off, rs1p_val, funct3);
                let insn = expand(h)
                    .unwrap_or_else(|| panic!("C.BEQZ/C.BNEZ offset {off} failed to expand"));
                assert_eq!(
                    crate::insn::imm_b(insn) as i64,
                    off as i64,
                    "branch offset {off} round-tripped incorrectly, halfword 0x{h:04X}"
                );
            }
        }
    }
}
