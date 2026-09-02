use super::*;
use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::cpu::Cpu;
use crate::exception::Exception;
use crate::mmu::Access;
use crate::uart::ConsoleSink;

pub fn execute<B: MemBacking, S: ConsoleSink>(
    cpu: &mut Cpu,
    bus: &mut Bus<B, S>,
    insn: u32,
) -> Result<bool, Exception> {
    if opcode(insn) != 0x2F {
        return Ok(false);
    }
    let size: u8 = match funct3(insn) {
        0x2 => 4,
        0x3 => 8,
        _ => return Err(Exception::IllegalInstruction(insn as u64)),
    };
    let vaddr = cpu.reg(rs1(insn));
    let src = cpu.reg(rs2(insn));
    let rd = rd(insn);
    let op = funct7(insn) >> 2;

    // The privileged spec classes both LR/SC and AMOs as store accesses for
    // fault-reporting purposes, even though every one of them also reads
    // memory — an AMO against a read-only page must raise a *store* page
    // fault, not a load page fault, from its read half. Translating once
    // here (rather than through `Cpu::vload`/`vstore`, which would each
    // translate independently) also guarantees the read and write halves of
    // a single AMO/SC target the same physical address.
    let addr = cpu.mmu.translate(bus, &cpu.csrs, cpu.priv_, vaddr, Access::Store)?;

    // Sign-extend 32-bit AMO results, per the spec.
    let ext = |v: u64| -> u64 {
        if size == 4 { v as u32 as i32 as i64 as u64 } else { v }
    };

    match op {
        // LR
        0x02 => {
            let v = bus.load(addr, size)?;
            cpu.reservation = Some(addr);
            cpu.set_reg(rd, ext(v));
        }
        // SC
        0x03 => {
            let success = cpu.reservation == Some(addr);
            // A store-conditional invalidates the reservation regardless of
            // outcome, per the spec — this also stops a failed SC leaving a
            // stale reservation that a later SC could ride on.
            cpu.reservation = None;
            if success {
                bus.store(addr, size, src)?;
                cpu.set_reg(rd, 0);
            } else {
                cpu.set_reg(rd, 1);
            }
        }
        // AMOs: read old, compute, write back, return old
        _ => {
            let old = bus.load(addr, size)?;

            // Comparisons must happen at the access width. bus.load zero-extends, so a
            // 32-bit 0xFFFF_FFFF would otherwise compare as +4294967295 rather than -1.
            // Masking also stops the upper half of a 64-bit source register taking part
            // in a .w comparison.
            let (sold, ssrc) = if size == 4 {
                (old as u32 as i32 as i64, src as u32 as i32 as i64)
            } else {
                (old as i64, src as i64)
            };
            let (uold, usrc) = if size == 4 {
                (old as u32 as u64, src as u32 as u64)
            } else {
                (old, src)
            };

            let new = match op {
                0x00 => old.wrapping_add(src),                    // AMOADD
                0x01 => src,                                      // AMOSWAP
                0x04 => old ^ src,                                // AMOXOR
                0x08 => old | src,                                // AMOOR
                0x0C => old & src,                                // AMOAND
                0x10 => if sold < ssrc { old } else { src },       // AMOMIN
                0x14 => if sold > ssrc { old } else { src },       // AMOMAX
                0x18 => if uold < usrc { old } else { src },       // AMOMINU
                0x1C => if uold > usrc { old } else { src },       // AMOMAXU
                _ => return Err(Exception::IllegalInstruction(insn as u64)),
            };
            bus.store(addr, size, new)?;
            // An AMO is a read-modify-write: its write must break a
            // reservation over the same granule exactly as a plain store
            // does, so a later SC can't succeed over memory it modified.
            cpu.invalidate_reservation(addr, size);
            cpu.set_reg(rd, ext(old));
        }
    }
    Ok(true)
}
