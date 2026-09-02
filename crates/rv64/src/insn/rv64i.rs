use super::*;
use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::exception::Exception;
use crate::uart::ConsoleSink;

/// Executes OP-IMM, OP-IMM-32, and LUI/AUIPC. Returns `Ok(true)` if the
/// instruction was handled here, `Ok(false)` to let another decoder try.
pub fn execute<B: MemBacking, S: ConsoleSink>(
    cpu: &mut Cpu,
    bus: &mut Bus<B, S>,
    insn: u32,
) -> Result<bool, Exception> {
    let (rd, rs1) = (rd(insn), rs1(insn));
    match opcode(insn) {
        // LUI
        0x37 => {
            cpu.set_reg(rd, imm_u(insn));
            Ok(true)
        }
        // AUIPC
        0x17 => {
            cpu.set_reg(rd, cpu.pc.wrapping_add(imm_u(insn)));
            Ok(true)
        }
        // OP-IMM
        0x13 => {
            let a = cpu.reg(rs1);
            let imm = imm_i(insn);
            let shamt = (insn >> 20) & 0x3F;
            let v = match funct3(insn) {
                0x0 => a.wrapping_add(imm),                    // ADDI
                0x1 => a << shamt,                              // SLLI
                0x2 => ((a as i64) < (imm as i64)) as u64,      // SLTI
                0x3 => (a < imm) as u64,                        // SLTIU
                0x4 => a ^ imm,                                 // XORI
                0x5 if funct7(insn) & 0x20 == 0 => a >> shamt,  // SRLI
                0x5 => ((a as i64) >> shamt) as u64,            // SRAI
                0x6 => a | imm,                                 // ORI
                0x7 => a & imm,                                 // ANDI
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v);
            Ok(true)
        }
        // OP-IMM-32 — 32-bit ops whose results sign-extend to 64 bits
        0x1B => {
            let a = cpu.reg(rs1) as u32;
            let imm = imm_i(insn) as u32;
            let shamt = (insn >> 20) & 0x1F;
            let v: i32 = match (funct3(insn), funct7(insn)) {
                (0x0, _) => a.wrapping_add(imm) as i32,           // ADDIW
                (0x1, _) => (a << shamt) as i32,                  // SLLIW
                (0x5, f) if f & 0x20 == 0 => (a >> shamt) as i32, // SRLIW
                (0x5, _) => (a as i32) >> shamt,                  // SRAIW
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v as i64 as u64);
            Ok(true)
        }
        // OP
        0x33 => {
            let (a, b) = (cpu.reg(rs1), cpu.reg(rs2(insn)));
            let shamt = (b & 0x3F) as u32;
            let v = match (funct3(insn), funct7(insn)) {
                (0x0, 0x00) => a.wrapping_add(b),
                (0x0, 0x20) => a.wrapping_sub(b),
                (0x1, _) => a << shamt,
                (0x2, _) => ((a as i64) < (b as i64)) as u64,
                (0x3, _) => (a < b) as u64,
                (0x4, _) => a ^ b,
                (0x5, 0x00) => a >> shamt,
                (0x5, 0x20) => ((a as i64) >> shamt) as u64,
                (0x6, _) => a | b,
                (0x7, _) => a & b,
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v);
            Ok(true)
        }
        // OP-32
        0x3B => {
            let (a, b) = (cpu.reg(rs1) as u32, cpu.reg(rs2(insn)) as u32);
            let shamt = b & 0x1F;
            let v: i32 = match (funct3(insn), funct7(insn)) {
                (0x0, 0x00) => a.wrapping_add(b) as i32,
                (0x0, 0x20) => a.wrapping_sub(b) as i32,
                (0x1, _) => (a << shamt) as i32,
                (0x5, 0x00) => (a >> shamt) as i32,
                (0x5, 0x20) => (a as i32) >> shamt,
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v as i64 as u64);
            Ok(true)
        }
        // LOAD
        0x03 => {
            let addr = cpu.reg(rs1).wrapping_add(imm_i(insn));
            let v = match funct3(insn) {
                0x0 => cpu.vload(bus, addr, 1)? as u8 as i8 as i64 as u64,   // LB
                0x1 => cpu.vload(bus, addr, 2)? as u16 as i16 as i64 as u64, // LH
                0x2 => cpu.vload(bus, addr, 4)? as u32 as i32 as i64 as u64, // LW
                0x3 => cpu.vload(bus, addr, 8)?,                             // LD
                0x4 => cpu.vload(bus, addr, 1)?,                             // LBU
                0x5 => cpu.vload(bus, addr, 2)?,                             // LHU
                0x6 => cpu.vload(bus, addr, 4)?,                             // LWU
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.set_reg(rd, v);
            Ok(true)
        }
        // STORE
        0x23 => {
            let addr = cpu.reg(rs1).wrapping_add(imm_s(insn));
            let v = cpu.reg(rs2(insn));
            let size = match funct3(insn) {
                0x0 => 1,
                0x1 => 2,
                0x2 => 4,
                0x3 => 8,
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            cpu.vstore(bus, addr, size, v)?;
            Ok(true)
        }
        // BRANCH
        0x63 => {
            let (a, b) = (cpu.reg(rs1), cpu.reg(rs2(insn)));
            let take = match funct3(insn) {
                0x0 => a == b,
                0x1 => a != b,
                0x4 => (a as i64) < (b as i64),
                0x5 => (a as i64) >= (b as i64),
                0x6 => a < b,
                0x7 => a >= b,
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            if take {
                cpu.jump(cpu.pc.wrapping_add(imm_b(insn)));
            }
            Ok(true)
        }
        // JAL
        0x6F => {
            let ret = cpu.pc.wrapping_add(cpu.insn_len);
            cpu.jump(cpu.pc.wrapping_add(imm_j(insn)));
            cpu.set_reg(rd, ret);
            Ok(true)
        }
        // JALR
        0x67 => {
            let ret = cpu.pc.wrapping_add(cpu.insn_len);
            let target = cpu.reg(rs1).wrapping_add(imm_i(insn)) & !1;
            cpu.jump(target);
            cpu.set_reg(rd, ret);
            Ok(true)
        }
        // MISC-MEM: FENCE (funct3 0) and FENCE.I (funct3 1, Zifencei).
        //
        // Both are no-ops here, for the same reason: this emulator executes
        // one instruction at a time, in order, against a single coherent
        // memory image, with no store buffer to drain and no instruction
        // cache to invalidate. There is no reordering for FENCE to forbid
        // and no stale fetch for FENCE.I to flush.
        //
        // Decoding them is not optional, though: `fence` is the first
        // instruction of the `riscv-tests` pass/fail sequence and Linux's
        // `flush_icache_*` emits `fence.i`, so leaving MISC-MEM undecoded
        // turned every single ISA test's *success* path into an illegal
        // instruction. The `rd`/`rs1` fields and the pred/succ set are
        // ignored rather than validated, matching the spec's requirement
        // that unused FENCE fields be reserved-for-future-use, not
        // reserved-illegal — which is also what makes `pause` (`fence w,0`)
        // and a hint-encoded `fence` execute as no-ops rather than trap.
        0x0F => match funct3(insn) {
            0x0 | 0x1 => Ok(true),
            _ => Err(Exception::IllegalInstruction(insn as u64)),
        },
        // SYSTEM
        0x73 => {
            let csr_addr = ((insn >> 20) & 0xFFF) as u16;
            match funct3(insn) {
                0x0 => match insn >> 20 {
                    0x000 => Err(match cpu.priv_ {
                        crate::csr::Priv::U => Exception::EnvironmentCallFromUMode,
                        crate::csr::Priv::S => Exception::EnvironmentCallFromSMode,
                        crate::csr::Priv::M => Exception::EnvironmentCallFromMMode,
                    }),
                    0x001 => Err(Exception::Breakpoint),
                    // MRET requires M-mode; a guest running below M that
                    // executes it must trap rather than escalate itself.
                    0x302 if cpu.priv_ == crate::csr::Priv::M => {
                        cpu.mret();
                        Ok(true)
                    }
                    // SRET requires at least S-mode.
                    0x102 if cpu.priv_ != crate::csr::Priv::U => {
                        cpu.sret();
                        Ok(true)
                    }
                    // WFI: no-op on a single hart. The spec allows execution
                    // in any privilege mode (only mstatus.TW, unimplemented
                    // here, would gate it further), so no privilege check
                    // applies.
                    0x105 => Ok(true),
                    // SFENCE.VMA requires S-mode or above per the spec — a
                    // U-mode guest attempting to flush translation state
                    // must trap rather than silently no-op. mstatus.TVM
                    // (which would additionally trap S-mode SFENCE.VMA when
                    // set) is not implemented.
                    _ if (insn >> 25) == 0x09 && cpu.priv_ != crate::csr::Priv::U => {
                        cpu.mmu.flush();
                        Ok(true)
                    }
                    // Covers 0x302/0x102 with a failing privilege guard, and
                    // SFENCE.VMA attempted from U-mode, too.
                    _ => Err(Exception::IllegalInstruction(insn as u64)),
                },
                f => {
                    // A CSR access attempts a write unless it is CSRRS/CSRRC
                    // (or their immediate forms) with a zero source — those
                    // forms are pure reads per the spec and must not fault
                    // on a read-only CSR or trigger any write side effect.
                    let writes = !(f & 0x3 != 1 && rs1 == 0);

                    // csr_addr[9:8] encodes the minimum privilege required
                    // to access this CSR at all (read or write).
                    if (csr_addr >> 8) & 3 > cpu.priv_ as u16 {
                        return Err(Exception::IllegalInstruction(insn as u64));
                    }
                    // csr_addr[11:10] == 0b11 marks a read-only CSR; only an
                    // attempted write to one is illegal.
                    if (csr_addr >> 10) == 3 && writes {
                        return Err(Exception::IllegalInstruction(insn as u64));
                    }

                    // `time`, `cycle` and `instret` are not register-file
                    // state — they are counters that live outside `Csrs`,
                    // and this is the only place with both the CSR address
                    // and the `bus` (hence the CLINT) in scope. Left to fall
                    // through to `Csrs::read`, each would return a
                    // permanently-zero slot of the flat array, which for
                    // `time` is boot-fatal: Linux's
                    // `riscv_clock_next_event` computes
                    // `get_cycles64() + delta` and programs that as the next
                    // deadline, so with `time` pinned at 0 every re-arm sets
                    // `mtimecmp` to a value `mtime` has already passed, the
                    // interrupt refires on the next instruction, and the
                    // kernel spins in its timer handler forever. A silent
                    // zero here is worse than a trap.
                    //
                    // All three are read-only (`csr_addr[11:10] == 0b11`),
                    // so the guard above has already rejected any write to
                    // them and there is no matching case in `Csrs::write`.
                    // The `mcounteren`/`scounteren` access gates are not
                    // implemented: they only ever *restrict* access, and
                    // Linux needs all three readable from U-mode for its
                    // vDSO anyway.
                    let old = match csr_addr {
                        crate::csr::TIME => bus.clint.mtime,
                        crate::csr::CYCLE | crate::csr::INSTRET => cpu.instret,
                        _ => cpu.csrs.read(csr_addr),
                    };
                    let src = if f & 0x4 != 0 { rs1 as u64 } else { cpu.reg(rs1) };
                    let new = match f & 0x3 {
                        1 => src,
                        2 => old | src,
                        3 => old & !src,
                        _ => return Err(Exception::IllegalInstruction(insn as u64)),
                    };
                    if writes {
                        cpu.csrs.write(csr_addr, new);
                        // A write to satp changes the active page table (or
                        // switches address spaces), so any cached
                        // translations are now stale.
                        if csr_addr == crate::csr::SATP {
                            cpu.mmu.flush();
                        }
                    }
                    cpu.set_reg(rd, old);
                    Ok(true)
                }
            }
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use crate::backing::FakeBacking;
    use crate::cache::PageCache;
    use crate::uart::VecSink;
    use crate::{Bus, Cpu, RAM_BASE};

    fn loaded(words: &[u32]) -> (Cpu, Bus<FakeBacking, VecSink>) {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(16), 4), VecSink::default());
        for (i, w) in words.iter().enumerate() {
            bus.store(RAM_BASE + 4 * i as u64, 4, *w as u64).unwrap();
        }
        (Cpu::new(RAM_BASE), bus)
    }

    /// `fence` (`0x0ff0000f`) and `fence.i` (`0x0000100f`) must execute as
    /// no-ops, not trap.
    ///
    /// This is a regression guard for a real defect: MISC-MEM was entirely
    /// undecoded, and because `fence` is the first instruction of the
    /// `riscv-tests` pass sequence, 105 of the 111 ISA tests failed on
    /// their *success* path rather than on anything they were testing.
    #[test]
    fn fence_and_fence_i_are_no_ops() {
        // addi x1, x0, 7; fence; fence.i
        let (mut cpu, mut bus) = loaded(&[0x0070_0093, 0x0FF0_000F, 0x0000_100F]);
        for _ in 0..3 {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.reg(1), 7);
        assert_eq!(cpu.pc, RAM_BASE + 12, "each must advance pc by 4");
    }

    /// A reserved MISC-MEM funct3 is still an illegal instruction — the
    /// fence fix must not turn the whole opcode into a blanket no-op.
    #[test]
    fn a_reserved_misc_mem_funct3_is_illegal() {
        use crate::exception::Exception;
        let (mut cpu, mut bus) = loaded(&[0x0000_400F]); // funct3 = 4
        assert!(matches!(cpu.step(&mut bus), Err(Exception::IllegalInstruction(_))));
    }
}
