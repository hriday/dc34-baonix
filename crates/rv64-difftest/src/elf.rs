//! A minimal ELF64 writer: exactly the object Spike will accept.
//!
//! The alternative was to assemble with a RISC-V toolchain, but the devShell
//! has no cross-assembler (only Spike, QEMU and the prebuilt `riscv-tests`
//! binaries), so shelling out to one would have meant adding a full GCC
//! cross-compiler to the flake for the sake of thirty instructions. Emitting
//! the object directly is ~80 lines, has no build-time cost, and keeps the
//! generator's output and its encoding in one language.
//!
//! Spike is fussier about the object than `rv64-host`'s loader is, and both
//! sets of requirements were established empirically:
//!
//! * `e_shstrndx < e_shnum` is `assert`ed in Spike's `elfloader.cc`, so an
//!   object with no section headers at all aborts the simulator. Hence the
//!   five section headers below, most of which exist only to satisfy that.
//! * Spike terminates through HTIF, which needs `tohost` and `fromhost` in
//!   the symbol table. Without them the program runs forever. Hence
//!   `.symtab` and `.strtab`.
//!
//! `rv64-host`'s loader ignores all of that and reads only the program
//! headers, so the same bytes load into both simulators.

use crate::{Program, BASE, ENTRY_OFF, FROMHOST_OFF, MEMSZ, TOHOST_OFF};

const EHDR: u64 = 64;
const PHDR: u64 = 56;
const SHDR: u64 = 64;
const SYM: u64 = 24;

/// File offset of the loaded segment. Must be congruent to the segment's
/// virtual address modulo the alignment, which page alignment gives for free
/// and also leaves room for the headers.
const SEG_OFF: u64 = 0x1000;

const SHSTR: &[u8] = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
const STRTAB: &[u8] = b"\0tohost\0fromhost\0";

// One ELF section header. Ten parameters because a section header has ten
// fields; naming a struct for something written once each would be worse.
#[allow(clippy::too_many_arguments)]
fn shdr(
    name: u32,
    typ: u32,
    flags: u64,
    addr: u64,
    off: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(SHDR as usize);
    b.extend_from_slice(&name.to_le_bytes());
    b.extend_from_slice(&typ.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&addr.to_le_bytes());
    b.extend_from_slice(&off.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&link.to_le_bytes());
    b.extend_from_slice(&info.to_le_bytes());
    b.extend_from_slice(&align.to_le_bytes());
    b.extend_from_slice(&entsize.to_le_bytes());
    b
}

fn sym(name: u32, value: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(SYM as usize);
    b.extend_from_slice(&name.to_le_bytes());
    b.push(0x11); // STB_GLOBAL | STT_OBJECT
    b.push(0); // st_other
    b.extend_from_slice(&1u16.to_le_bytes()); // st_shndx = .text
    b.extend_from_slice(&value.to_le_bytes());
    b.extend_from_slice(&8u64.to_le_bytes()); // st_size
    b
}

/// Wraps `p.image` in an ELF64 object loadable by both simulators.
pub fn build(p: &Program) -> Vec<u8> {
    let filesz = p.image.len() as u64;
    assert!(filesz <= MEMSZ, "image is larger than the mapped segment");

    let mut f = vec![0u8; SEG_OFF as usize];
    f.extend_from_slice(&p.image);

    let symtab_off = f.len() as u64;
    f.extend_from_slice(&[0u8; SYM as usize]); // the mandatory null symbol
    f.extend_from_slice(&sym(1, BASE + TOHOST_OFF));
    f.extend_from_slice(&sym(8, BASE + FROMHOST_OFF));
    let symtab_size = f.len() as u64 - symtab_off;

    let strtab_off = f.len() as u64;
    f.extend_from_slice(STRTAB);
    let shstr_off = f.len() as u64;
    f.extend_from_slice(SHSTR);
    while !f.len().is_multiple_of(8) {
        f.push(0);
    }
    let shoff = f.len() as u64;

    f.extend_from_slice(&shdr(0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
    f.extend_from_slice(&shdr(1, 1, 0x6, BASE, SEG_OFF, filesz, 0, 0, 0x1000, 0)); // .text
    f.extend_from_slice(&shdr(7, 2, 0, 0, symtab_off, symtab_size, 3, 1, 8, SYM)); // .symtab
    f.extend_from_slice(&shdr(15, 3, 0, 0, strtab_off, STRTAB.len() as u64, 0, 0, 1, 0));
    f.extend_from_slice(&shdr(23, 3, 0, 0, shstr_off, SHSTR.len() as u64, 0, 0, 1, 0));

    // Header and program header go in the space reserved at the front.
    let e = &mut f[..EHDR as usize];
    e[..4].copy_from_slice(b"\x7fELF");
    e[4] = 2; // ELFCLASS64
    e[5] = 1; // ELFDATA2LSB
    e[6] = 1; // EV_CURRENT
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e[18..20].copy_from_slice(&243u16.to_le_bytes()); // EM_RISCV
    e[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    e[24..32].copy_from_slice(&(BASE + ENTRY_OFF).to_le_bytes());
    e[32..40].copy_from_slice(&EHDR.to_le_bytes()); // e_phoff
    e[40..48].copy_from_slice(&shoff.to_le_bytes());
    e[52..54].copy_from_slice(&(EHDR as u16).to_le_bytes());
    e[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes());
    e[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    e[58..60].copy_from_slice(&(SHDR as u16).to_le_bytes());
    e[60..62].copy_from_slice(&5u16.to_le_bytes()); // e_shnum
    e[62..64].copy_from_slice(&4u16.to_le_bytes()); // e_shstrndx

    let ph = &mut f[EHDR as usize..(EHDR + PHDR) as usize];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    ph[4..8].copy_from_slice(&7u32.to_le_bytes()); // RWX
    ph[8..16].copy_from_slice(&SEG_OFF.to_le_bytes());
    ph[16..24].copy_from_slice(&BASE.to_le_bytes()); // p_vaddr
    ph[24..32].copy_from_slice(&BASE.to_le_bytes()); // p_paddr
    ph[32..40].copy_from_slice(&filesz.to_le_bytes());
    ph[40..48].copy_from_slice(&MEMSZ.to_le_bytes());
    ph[48..56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The object must load through the same loader the emulator uses, at
    /// the address the generator assumed, with the image byte-identical in
    /// guest memory. If this drifts, the two simulators stop running the
    /// same program and every comparison becomes meaningless.
    #[test]
    fn the_emitted_object_round_trips_through_the_host_loader() {
        use rv64::backing::FakeBacking;
        use rv64::bus::Bus;
        use rv64::cache::PageCache;
        use rv64::uart::VecSink;

        let p = crate::gen::program(3);
        let bytes = build(&p);
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(64), 16), VecSink::default());
        let entry = rv64_host::elf::load(&mut bus, &bytes).unwrap();
        assert_eq!(entry, BASE + ENTRY_OFF);
        assert_eq!(rv64_host::elf::find_symbol(&bytes, "tohost"), Some(BASE + TOHOST_OFF));

        for (i, want) in p.image.iter().enumerate() {
            let got = bus.load(BASE + i as u64, 1).unwrap() as u8;
            assert_eq!(got, *want, "byte {i:#x} of the image did not load");
        }
        // The scratch page is file-backed, not `.bss`, so that loads read
        // something other than zero. It must not be all zeros.
        let scratch = crate::gen::scratch_base();
        assert!(
            (0..64).any(|i| bus.load(scratch + 8 * i, 8).unwrap() != 0),
            "the scratch page loaded as all zeros"
        );
    }
}
