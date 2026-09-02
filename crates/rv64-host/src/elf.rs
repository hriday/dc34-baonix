//! A deliberately minimal ELF64 loader: little-endian RISC-V, `PT_LOAD`
//! segments only, plus a symbol lookup for the `tohost` address the
//! `riscv-tests` binaries report their verdict through.
//!
//! Nothing here relocates, resolves, or interprets dynamic linking
//! information — the images this loads (ISA tests now, a Linux kernel
//! later) are statically linked and already laid out at their physical
//! addresses.

use rv64::backing::MemBacking;
use rv64::bus::Bus;
use rv64::uart::ConsoleSink;

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;

const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;
const SHDR_LEN: usize = 64;
const SYM_LEN: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Too short, bad magic, or not a little-endian 64-bit RISC-V object.
    NotAnElf64Riscv,
    /// A header or segment ran off the end of the buffer.
    Truncated,
    /// A `PT_LOAD` segment did not fit in guest memory.
    Unmapped(u64),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotAnElf64Riscv => write!(f, "not a little-endian 64-bit RISC-V ELF"),
            Error::Truncated => write!(f, "truncated ELF"),
            Error::Unmapped(a) => write!(f, "PT_LOAD segment at {a:#x} is outside guest memory"),
        }
    }
}

impl std::error::Error for Error {}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Validates the ELF header and returns `(entry, phoff, phentsize, phnum,
/// shoff, shentsize, shnum)`.
struct Header {
    entry: u64,
    phoff: u64,
    phentsize: u16,
    phnum: u16,
    shoff: u64,
    shentsize: u16,
    shnum: u16,
}

fn header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < EHDR_LEN
        || &bytes[..4] != b"\x7fELF"
        || bytes[EI_CLASS] != ELFCLASS64
        || bytes[EI_DATA] != ELFDATA2LSB
        || u16_at(bytes, 18) != Some(EM_RISCV)
    {
        return Err(Error::NotAnElf64Riscv);
    }
    Ok(Header {
        entry: u64_at(bytes, 24).ok_or(Error::Truncated)?,
        phoff: u64_at(bytes, 32).ok_or(Error::Truncated)?,
        phentsize: u16_at(bytes, 54).ok_or(Error::Truncated)?,
        phnum: u16_at(bytes, 56).ok_or(Error::Truncated)?,
        shoff: u64_at(bytes, 40).ok_or(Error::Truncated)?,
        shentsize: u16_at(bytes, 58).ok_or(Error::Truncated)?,
        shnum: u16_at(bytes, 60).ok_or(Error::Truncated)?,
    })
}

/// Copies `data` to guest physical `addr` through the bus, then zero-fills
/// out to `memsz` (the `.bss` tail of the segment).
///
/// Writes go through `Bus::store` rather than straight into the page cache
/// so that the bus's own bounds checks apply — a segment that overhangs the
/// end of RAM is reported as `Unmapped` here instead of silently corrupting
/// whatever the cache would have wrapped onto.
fn write_segment<B: MemBacking, S: ConsoleSink>(
    bus: &mut Bus<B, S>,
    addr: u64,
    data: &[u8],
    memsz: u64,
) -> Result<(), Error> {
    let mut off = 0u64;
    // `memsz >= filesz` for any sane object; the `max` only guards a
    // malformed header from truncating the data that is actually there.
    // Reading past `data` yields zero, which is exactly the `.bss` tail.
    let total = memsz.max(data.len() as u64);
    let byte_of = |i: u64| data.get(i as usize).copied().unwrap_or(0);
    while off < total {
        let a = addr.wrapping_add(off);
        // 8 bytes at a time when the destination is aligned and there is a
        // full word left; the remainder byte-at-a-time. A kernel image is
        // tens of megabytes, so the fast path is worth the four lines.
        if a.is_multiple_of(8) && total - off >= 8 {
            let mut w = [0u8; 8];
            for (i, slot) in w.iter_mut().enumerate() {
                *slot = byte_of(off + i as u64);
            }
            bus.store(a, 8, u64::from_le_bytes(w)).map_err(|_| Error::Unmapped(a))?;
            off += 8;
        } else {
            bus.store(a, 1, byte_of(off) as u64).map_err(|_| Error::Unmapped(a))?;
            off += 1;
        }
    }
    Ok(())
}

/// Loads every `PT_LOAD` segment at its physical address and returns the
/// entry point.
///
/// `p_paddr` is used, not `p_vaddr`: these images are loaded into guest
/// *physical* memory with paging off, and for a kernel the two differ.
pub fn load<B: MemBacking, S: ConsoleSink>(
    bus: &mut Bus<B, S>,
    bytes: &[u8],
) -> Result<u64, Error> {
    let h = header(bytes)?;
    if h.phnum != 0 && (h.phentsize as usize) < PHDR_LEN {
        return Err(Error::Truncated);
    }
    for i in 0..h.phnum as u64 {
        let base = (h.phoff + i * h.phentsize as u64) as usize;
        if u32_at(bytes, base).ok_or(Error::Truncated)? != PT_LOAD {
            continue;
        }
        let offset = u64_at(bytes, base + 8).ok_or(Error::Truncated)?;
        let paddr = u64_at(bytes, base + 24).ok_or(Error::Truncated)?;
        let filesz = u64_at(bytes, base + 32).ok_or(Error::Truncated)?;
        let memsz = u64_at(bytes, base + 40).ok_or(Error::Truncated)?;
        let start = offset as usize;
        let end = start.checked_add(filesz as usize).ok_or(Error::Truncated)?;
        let data = bytes.get(start..end).ok_or(Error::Truncated)?;
        write_segment(bus, paddr, data, memsz)?;
    }
    Ok(h.entry)
}

/// Returns the highest guest physical address (exclusive) touched by any
/// `PT_LOAD` segment — `max(p_paddr + p_memsz)` across all of them.
///
/// The CLI runner uses this to place the DTB immediately above the loaded
/// kernel image: `load` only returns the entry point, and the DTB must not
/// land inside the kernel's own `.bss` tail. This re-parses the program
/// headers rather than being folded into `load` because it needs no `Bus` —
/// it is a pure computation over the object's bytes, useful before (or
/// without) ever touching guest memory.
pub fn extent(bytes: &[u8]) -> Result<u64, Error> {
    let h = header(bytes)?;
    if h.phnum != 0 && (h.phentsize as usize) < PHDR_LEN {
        return Err(Error::Truncated);
    }
    let mut end = 0u64;
    for i in 0..h.phnum as u64 {
        let base = (h.phoff + i * h.phentsize as u64) as usize;
        if u32_at(bytes, base).ok_or(Error::Truncated)? != PT_LOAD {
            continue;
        }
        let paddr = u64_at(bytes, base + 24).ok_or(Error::Truncated)?;
        let memsz = u64_at(bytes, base + 40).ok_or(Error::Truncated)?;
        end = end.max(paddr.saturating_add(memsz));
    }
    Ok(end)
}

/// Looks up a symbol's value (address) in the ELF's `.symtab`.
///
/// Used to find `tohost`, the magic address a `riscv-tests` binary stores
/// its verdict to. Returns `None` if the object was stripped or has no such
/// symbol.
pub fn find_symbol(bytes: &[u8], name: &str) -> Option<u64> {
    let h = header(bytes).ok()?;
    if (h.shentsize as usize) < SHDR_LEN {
        return None;
    }
    for i in 0..h.shnum as u64 {
        let sh = (h.shoff + i * h.shentsize as u64) as usize;
        if u32_at(bytes, sh + 4)? != SHT_SYMTAB {
            continue;
        }
        let sym_off = u64_at(bytes, sh + 24)? as usize;
        let sym_size = u64_at(bytes, sh + 32)? as usize;
        let entsize = u64_at(bytes, sh + 56)? as usize;
        if entsize < SYM_LEN {
            continue;
        }

        // sh_link of a symbol table names the string table its st_name
        // values index into.
        let strtab = (h.shoff + u32_at(bytes, sh + 40)? as u64 * h.shentsize as u64) as usize;
        let str_off = u64_at(bytes, strtab + 24)? as usize;
        let str_size = u64_at(bytes, strtab + 32)? as usize;
        let strs = bytes.get(str_off..str_off.checked_add(str_size)?)?;

        for s in (0..sym_size / entsize).map(|n| sym_off + n * entsize) {
            // A symbol whose name index is out of range is skipped rather
            // than aborting the search: one malformed entry must not hide a
            // well-formed `tohost` later in the same table.
            let Some(st_name) = u32_at(bytes, s) else { continue };
            let Some(rest) = strs.get(st_name as usize..) else { continue };
            let len = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
            if &rest[..len] == name.as_bytes() {
                return u64_at(bytes, s + 8);
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rv64::backing::FakeBacking;
    use rv64::cache::PageCache;
    use rv64::uart::VecSink;
    use rv64::RAM_BASE;

    fn bus() -> Bus<FakeBacking, VecSink> {
        Bus::new(PageCache::new(FakeBacking::new(64), 16), VecSink::default())
    }

    /// Builds a one-`PT_LOAD` ELF64 whose segment holds `data` at
    /// `RAM_BASE`, padded out to `memsz`.
    ///
    /// `pub(crate)` so `lib.rs`'s `load_kernel` tests can exercise the ELF
    /// branch of the magic sniff against the same object this module's own
    /// tests use, rather than hand-rolling a second one that could drift.
    pub(crate) fn tiny_elf(data: &[u8], memsz: u64) -> Vec<u8> {
        let mut b = vec![0u8; EHDR_LEN + PHDR_LEN];
        b[..4].copy_from_slice(b"\x7fELF");
        b[EI_CLASS] = ELFCLASS64;
        b[EI_DATA] = ELFDATA2LSB;
        b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        b[18..20].copy_from_slice(&EM_RISCV.to_le_bytes());
        b[24..32].copy_from_slice(&RAM_BASE.to_le_bytes()); // e_entry
        b[32..40].copy_from_slice(&(EHDR_LEN as u64).to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

        let p = EHDR_LEN;
        b[p..p + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        let data_off = (EHDR_LEN + PHDR_LEN) as u64;
        b[p + 8..p + 16].copy_from_slice(&data_off.to_le_bytes());
        b[p + 16..p + 24].copy_from_slice(&RAM_BASE.to_le_bytes()); // p_vaddr
        b[p + 24..p + 32].copy_from_slice(&RAM_BASE.to_le_bytes()); // p_paddr
        b[p + 32..p + 40].copy_from_slice(&(data.len() as u64).to_le_bytes());
        b[p + 40..p + 48].copy_from_slice(&memsz.to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn loads_a_pt_load_segment_and_returns_the_entry() {
        let mut bus = bus();
        let entry = load(&mut bus, &tiny_elf(&[1, 2, 3, 4, 5, 6, 7, 8, 9], 9)).unwrap();
        assert_eq!(entry, RAM_BASE);
        assert_eq!(bus.load(RAM_BASE, 8).unwrap(), 0x0807_0605_0403_0201);
        assert_eq!(bus.load(RAM_BASE + 8, 1).unwrap(), 9);
    }

    /// The CLI runner rounds this up to place the DTB, so it must reflect
    /// `memsz` (the `.bss` tail included), not just `filesz`.
    #[test]
    fn extent_is_paddr_plus_memsz() {
        let e = tiny_elf(&[1, 2, 3, 4], 32);
        assert_eq!(extent(&e).unwrap(), RAM_BASE + 32);
    }

    /// The `memsz > filesz` tail is the segment's `.bss` and must be zeroed,
    /// not left holding whatever was in guest memory before.
    #[test]
    fn bss_tail_is_zero_filled() {
        let mut bus = bus();
        bus.store(RAM_BASE + 16, 8, u64::MAX).unwrap();
        load(&mut bus, &tiny_elf(&[0xFF; 4], 32)).unwrap();
        assert_eq!(bus.load(RAM_BASE, 4).unwrap(), 0xFFFF_FFFF);
        assert_eq!(bus.load(RAM_BASE + 16, 8).unwrap(), 0);
    }

    #[test]
    fn a_non_elf_is_rejected_rather_than_panicking() {
        let mut bus = bus();
        assert_eq!(load(&mut bus, b"not an elf at all"), Err(Error::NotAnElf64Riscv));
        assert_eq!(load(&mut bus, &[]), Err(Error::NotAnElf64Riscv));
    }

    /// A `.dump` disassembly sitting next to the binaries, or a truncated
    /// image, must be an error rather than a panic — the harness reads
    /// whole directories.
    #[test]
    fn a_truncated_elf_is_rejected_rather_than_panicking() {
        let mut bus = bus();
        let mut e = tiny_elf(&[1, 2, 3, 4], 4);
        e.truncate(EHDR_LEN + PHDR_LEN); // header claims 4 bytes of data
        assert_eq!(load(&mut bus, &e), Err(Error::Truncated));
    }

    #[test]
    fn a_segment_outside_ram_is_reported_not_silently_dropped() {
        let mut bus = bus();
        let mut e = tiny_elf(&[1; 8], 8);
        let p = EHDR_LEN;
        e[p + 24..p + 32].copy_from_slice(&0x1234u64.to_le_bytes()); // p_paddr
        assert_eq!(load(&mut bus, &e), Err(Error::Unmapped(0x1234)));
    }

    #[test]
    fn find_symbol_returns_none_on_a_stripped_object() {
        assert_eq!(find_symbol(&tiny_elf(&[0; 4], 4), "tohost"), None);
    }
}
