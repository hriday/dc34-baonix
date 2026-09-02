use crate::backing::MemBacking;
use crate::bus::Bus;
use crate::csr::{self, Csrs, Priv};
use crate::exception::Exception;
use crate::uart::ConsoleSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

const TLB_ENTRIES: usize = 32;

#[derive(Clone, Copy, Default)]
struct TlbEntry {
    valid: bool,
    vpn: u64,
    ppn: u64,
    perms: u64,
}

#[derive(Default)]
pub struct Mmu {
    tlb: [TlbEntry; TLB_ENTRIES],
    /// Number of full page walks performed. Instrumentation for Task 21.
    pub walks: u64,
}

impl Mmu {
    pub fn flush(&mut self) {
        self.tlb = [TlbEntry::default(); TLB_ENTRIES];
    }

    fn fault(access: Access, vaddr: u64) -> Exception {
        match access {
            Access::Fetch => Exception::InstructionPageFault(vaddr),
            Access::Load => Exception::LoadPageFault(vaddr),
            Access::Store => Exception::StorePageFault(vaddr),
        }
    }

    pub fn translate<B: MemBacking, S: ConsoleSink>(
        &mut self,
        bus: &mut Bus<B, S>,
        csrs: &Csrs,
        priv_: Priv,
        vaddr: u64,
        access: Access,
    ) -> Result<u64, Exception> {
        let satp = csrs.read(csr::SATP);
        let mode = satp >> 60;
        // Bare mode, or M-mode without translation.
        //
        // This deliberately does not honor mstatus.MPRV: per the privileged
        // spec, when MPRV=1 an M-mode load/store (not fetch) should be
        // translated and protection-checked as though executed in the
        // privilege mode held in mstatus.MPP, rather than bypassing
        // translation as M-mode normally does. That requires threading an
        // "effective privilege for this access" distinct from `priv_`
        // (fetches are always exempt) through `Cpu::vload`/`vstore`. This is
        // out of scope for Task 11 — see the task-11 report for the
        // reasoning — so MPRV is currently inert here. `Cpu::mret` still
        // clears MPRV on a privilege drop below M so the CSR itself reads
        // back correctly, independent of this gap.
        if mode != 8 || priv_ == Priv::M {
            return Ok(vaddr);
        }

        let vpn = vaddr >> 12;
        let slot = (vpn as usize) % TLB_ENTRIES;
        if self.tlb[slot].valid && self.tlb[slot].vpn == vpn {
            let e = self.tlb[slot];
            Self::check_perms(e.perms, priv_, access, vaddr)?;
            return Ok((e.ppn << 12) | (vaddr & 0xFFF));
        }

        self.walks += 1;
        let mut table = (satp & 0x0FFF_FFFF_FFFF) << 12;
        let idx = [
            (vaddr >> 30) & 0x1FF,
            (vaddr >> 21) & 0x1FF,
            (vaddr >> 12) & 0x1FF,
        ];

        for level in 0..3 {
            let pte_addr = table + idx[level] * 8;
            // A backing-store failure here is not a guest page fault — it is
            // the emulator's own storage medium failing (e.g. USB or flash
            // I/O). It must propagate unchanged so the run loop aborts,
            // rather than being reinterpreted as `Self::fault`, which would
            // silently look to the guest like an ordinary unmapped page.
            let pte = match bus.load(pte_addr, 8) {
                Ok(v) => v,
                Err(e @ Exception::BackingFailure(_)) => return Err(e),
                Err(_) => return Err(Self::fault(access, vaddr)),
            };

            if pte & 1 == 0 {
                return Err(Self::fault(access, vaddr));
            }
            let ppn = (pte >> 10) & 0x0FFF_FFFF_FFFF;
            let is_leaf = pte & 0b1110 != 0;

            if is_leaf {
                Self::check_perms(pte, priv_, access, vaddr)?;
                // A superpage's unused low PPN bits must be zero — Sv39's
                // translation algorithm (step 6) requires a page fault here,
                // not silent acceptance. `mask` is the same set of bits the
                // splice below fills in from the vaddr, so a nonzero value
                // means the guest's page table names a physical page that
                // isn't actually aligned to the superpage's own size.
                let mask: u64 = match level {
                    0 => 0x3FFFF, // gigapage: ppn[1:0] must be zero
                    1 => 0x1FF,   // megapage: ppn[0] must be zero
                    _ => 0,
                };
                if ppn & mask != 0 {
                    return Err(Self::fault(access, vaddr));
                }
                // Superpage: splice in the untranslated low VPN bits.
                let ppn = match level {
                    0 => (ppn & !0x3FFFF) | ((vaddr >> 12) & 0x3FFFF),
                    1 => (ppn & !0x1FF) | ((vaddr >> 12) & 0x1FF),
                    _ => ppn,
                };
                self.tlb[slot] = TlbEntry { valid: true, vpn, ppn, perms: pte };
                return Ok((ppn << 12) | (vaddr & 0xFFF));
            }
            table = ppn << 12;
        }
        Err(Self::fault(access, vaddr))
    }

    /// Checks R/W/X/U permission bits *and* Accessed/Dirty, against a raw
    /// PTE value. Called both from a fresh walk and from a TLB hit — the
    /// TLB stores the leaf's raw PTE in `perms` precisely so this one
    /// function can gate both paths identically. A page with A=0, or with
    /// D=0 on a store, must fault the same way regardless of whether the
    /// translation came from a walk or a cached TLB entry: this emulator
    /// does not auto-set A/D, so a cached "the page allows writes" fact
    /// must not silently outlive "but D was never observed set".
    fn check_perms(pte: u64, priv_: Priv, access: Access, vaddr: u64) -> Result<(), Exception> {
        let (r, w, x, u) = (
            pte & (1 << 1) != 0,
            pte & (1 << 2) != 0,
            pte & (1 << 3) != 0,
            pte & (1 << 4) != 0,
        );
        let ok = match access {
            Access::Fetch => x,
            Access::Load => r,
            Access::Store => w,
        };
        if !ok {
            return Err(Self::fault(access, vaddr));
        }
        if priv_ == Priv::U && !u {
            return Err(Self::fault(access, vaddr));
        }
        if pte & (1 << 6) == 0 || (access == Access::Store && pte & (1 << 7) == 0) {
            return Err(Self::fault(access, vaddr));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backing::FakeBacking;
    use crate::bus::Bus;
    use crate::cache::PageCache;
    use crate::csr::{self, Csrs, Priv};
    use crate::uart::VecSink;
    use crate::RAM_BASE;

    /// Builds a single 4 KiB identity-ish mapping: vaddr 0x1000 -> RAM_BASE.
    fn mapped() -> (Bus<FakeBacking, VecSink>, Csrs) {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mid = RAM_BASE + 0x11_0000;
        let leaf = RAM_BASE + 0x12_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RWX: u64 = (1 << 1) | (1 << 2) | (1 << 3);
        const AD: u64 = (1 << 6) | (1 << 7);
        const U: u64 = 1 << 4;

        // vaddr 0x1000 -> vpn2=0, vpn1=0, vpn0=1
        bus.store(root, 8, ppn(mid) | V).unwrap();
        bus.store(mid, 8, ppn(leaf) | V).unwrap();
        bus.store(leaf + 8, 8, ppn(RAM_BASE) | V | RWX | AD | U).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));
        (bus, csrs)
    }

    #[test]
    fn bare_mode_is_identity() {
        let mut mmu = Mmu::default();
        let (mut bus, _) = mapped();
        let csrs = Csrs::default(); // satp = 0
        let pa = mmu
            .translate(&mut bus, &csrs, Priv::S, 0xDEAD_000, Access::Load)
            .unwrap();
        assert_eq!(pa, 0xDEAD_000);
    }

    /// The MMU honors exactly two `satp.MODE` values — Bare (0) and Sv39
    /// (8) — and treats every other one as Bare. That is only safe if a
    /// guest can never *read back* a MODE this function would then ignore:
    /// Linux's `set_satp_mode()` writes Sv57, reads satp back, and believes
    /// the readback. This pins the two halves together — whatever survives a
    /// `satp` write must be a mode `translate` actually implements, and the
    /// resulting translation must match the mode the guest can observe.
    #[test]
    fn a_mode_the_mmu_ignores_can_never_be_read_back_from_satp() {
        let mut mmu = Mmu::default();
        let (mut bus, _) = mapped();
        let root = RAM_BASE + 0x10_0000;

        for mode in [9u64, 10, 15] {
            let mut csrs = Csrs::default();
            csrs.write(csr::SATP, (mode << 60) | (root >> 12));
            let observed = csrs.read(csr::SATP) >> 60;
            assert!(
                observed == 0 || observed == 8,
                "satp read back MODE={observed}, which `translate` does not implement"
            );
            // …and the machine behaves the way that readback says it does.
            // MODE=0 is Bare, so translation is identity — visibly off,
            // which is the whole point: the guest is told so.
            mmu.flush();
            let pa = mmu.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load).unwrap();
            assert_eq!(pa, 0x1000, "MODE={observed} must translate as Bare");
        }
    }

    #[test]
    fn sv39_translates_through_three_levels() {
        let mut mmu = Mmu::default();
        let (mut bus, csrs) = mapped();
        let pa = mmu
            .translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load)
            .unwrap();
        assert_eq!(pa, RAM_BASE);
    }

    #[test]
    fn page_offset_is_preserved() {
        let mut mmu = Mmu::default();
        let (mut bus, csrs) = mapped();
        let pa = mmu
            .translate(&mut bus, &csrs, Priv::S, 0x1ABC, Access::Load)
            .unwrap();
        assert_eq!(pa, RAM_BASE + 0xABC);
    }

    #[test]
    fn unmapped_address_page_faults() {
        let mut mmu = Mmu::default();
        let (mut bus, csrs) = mapped();
        let r = mmu.translate(&mut bus, &csrs, Priv::S, 0x9000, Access::Load);
        assert!(matches!(r, Err(crate::Exception::LoadPageFault(0x9000))));
    }

    #[test]
    fn fetch_fault_reports_instruction_page_fault() {
        let mut mmu = Mmu::default();
        let (mut bus, csrs) = mapped();
        let r = mmu.translate(&mut bus, &csrs, Priv::S, 0x9000, Access::Fetch);
        assert!(matches!(r, Err(crate::Exception::InstructionPageFault(0x9000))));
    }

    #[test]
    fn tlb_flush_forces_a_rewalk() {
        let mut mmu = Mmu::default();
        let (mut bus, csrs) = mapped();
        mmu.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load).unwrap();
        assert_eq!(mmu.walks, 1);
        mmu.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load).unwrap();
        assert_eq!(mmu.walks, 1, "second access must hit the TLB");
        mmu.flush();
        mmu.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load).unwrap();
        assert_eq!(mmu.walks, 2);
    }

    /// A backing-store failure while reading a page-table entry must
    /// propagate as `Exception::BackingFailure`, not be swallowed into a
    /// guest-visible page fault — the same defect class previously fixed in
    /// `Cpu::step`'s instruction fetch (Task 5).
    #[test]
    fn pte_load_backing_failure_is_not_masked_as_a_page_fault() {
        let mut mmu = Mmu::default();
        // Only page 0 of the backing store exists; the root table lives far
        // beyond it, so reading the root PTE must fail at the backing layer.
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(1), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let r = mmu.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load);
        assert!(
            matches!(r, Err(crate::Exception::BackingFailure(_))),
            "expected BackingFailure, got {r:?}"
        );
    }

    // --- Fix round 1: superpage coverage (Findings 1, 2, 4) ---

    /// A level-1 leaf (megapage) must translate correctly at a vaddr in the
    /// *middle* of the 2 MiB region, not just at its base — the base case
    /// alone would still pass with a broken low-bit splice.
    #[test]
    fn megapage_translates_through_two_levels() {
        let mut mmu = Mmu::default();
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mid = RAM_BASE + 0x11_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RWX: u64 = (1 << 1) | (1 << 2) | (1 << 3);
        const AD: u64 = (1 << 6) | (1 << 7);

        bus.store(root, 8, ppn(mid) | V).unwrap();
        // Leaf at level 1, vpn1 index 1: maps the 2 MiB vaddr region
        // [0x20_0000, 0x40_0000) to the physical region based at RAM_BASE.
        bus.store(mid + 8, 8, ppn(RAM_BASE) | V | RWX | AD).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        // Offset has nonzero bits in both vpn0 and the page offset, so a
        // splice that only worked at the superpage's base would be caught.
        let offset = 0x12_3456u64;
        let vaddr = 0x20_0000 + offset;
        let pa = mmu
            .translate(&mut bus, &csrs, Priv::S, vaddr, Access::Load)
            .unwrap();
        assert_eq!(pa, RAM_BASE + offset);
    }

    /// A level-0 leaf (gigapage) — the root PTE itself is the leaf — must
    /// likewise translate correctly away from its base.
    #[test]
    fn gigapage_translates_through_one_level() {
        let mut mmu = Mmu::default();
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RWX: u64 = (1 << 1) | (1 << 2) | (1 << 3);
        const AD: u64 = (1 << 6) | (1 << 7);

        // The root PTE (vpn2 index 0) is itself a leaf, mapping the 1 GiB
        // vaddr region [0, 0x4000_0000) to the physical region at RAM_BASE.
        bus.store(root, 8, ppn(RAM_BASE) | V | RWX | AD).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let offset = 0x1234_5678u64;
        assert!(offset < 0x4000_0000, "offset must stay within the gigapage");
        let pa = mmu
            .translate(&mut bus, &csrs, Priv::S, offset, Access::Load)
            .unwrap();
        assert_eq!(pa, RAM_BASE + offset);
    }

    /// Finding 1: a superpage PTE whose unused low PPN bits are nonzero is
    /// malformed per the Sv39 translation algorithm (step 6) and must
    /// page-fault, not be silently masked into a working — but wrong —
    /// mapping.
    #[test]
    fn misaligned_megapage_pte_faults() {
        let mut mmu = Mmu::default();
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mid = RAM_BASE + 0x11_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RWX: u64 = (1 << 1) | (1 << 2) | (1 << 3);
        const AD: u64 = (1 << 6) | (1 << 7);

        bus.store(root, 8, ppn(mid) | V).unwrap();
        // A megapage PTE's ppn[0] (the low 9 bits of the PPN) must be zero.
        // RAM_BASE + PAGE is only 4 KiB-aligned, not 2 MiB-aligned, so this
        // PTE is malformed.
        let misaligned_paddr = RAM_BASE + crate::PAGE as u64;
        bus.store(mid, 8, ppn(misaligned_paddr) | V | RWX | AD).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        // vaddr 0x1234 has vpn2=0, vpn1=0, landing on the malformed mid[0].
        let r = mmu.translate(&mut bus, &csrs, Priv::S, 0x1234, Access::Load);
        assert!(
            matches!(r, Err(Exception::LoadPageFault(0x1234))),
            "misaligned superpage PTE must fault, got {r:?}"
        );
    }

    /// Level-0 counterpart of `misaligned_megapage_pte_faults`: only the
    /// level-1 mask (0x1FF) was previously exercised, so a wrong level-0
    /// mask (e.g. 0x1FF where 0x3FFFF belongs) would have been caught by no
    /// test.
    #[test]
    fn misaligned_gigapage_pte_faults() {
        let mut mmu = Mmu::default();
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RWX: u64 = (1 << 1) | (1 << 2) | (1 << 3);
        const AD: u64 = (1 << 6) | (1 << 7);

        // The root PTE (vpn2 index 0) is itself the leaf. A gigapage PTE's
        // ppn[1:0] (the low 18 bits of the PPN) must be zero. RAM_BASE is
        // itself 1 GiB-aligned, but RAM_BASE + 2 MiB is only 2 MiB-aligned,
        // so this PTE is malformed.
        let misaligned_paddr = RAM_BASE + 0x20_0000;
        bus.store(root, 8, ppn(misaligned_paddr) | V | RWX | AD).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        let r = mmu.translate(&mut bus, &csrs, Priv::S, 0x1234, Access::Load);
        assert!(
            matches!(r, Err(Exception::LoadPageFault(0x1234))),
            "misaligned gigapage PTE must fault, got {r:?}"
        );
    }

    /// Finding 2: a store to a page with D=0 must fault identically whether
    /// the TLB is cold (fresh walk) or warm (a prior load installed an
    /// entry). The TLB's cached `perms` is the leaf's raw PTE, so a hit
    /// must re-check Accessed/Dirty exactly as a walk does — caching "R/W
    /// allow this" must not let a stale "but D was never observed" escape.
    #[test]
    fn store_to_clean_page_faults_identically_cold_and_warm() {
        let mut bus = Bus::new(PageCache::new(FakeBacking::new(512), 64), VecSink::default());
        let root = RAM_BASE + 0x10_0000;
        let mid = RAM_BASE + 0x11_0000;
        let leaf = RAM_BASE + 0x12_0000;
        let ppn = |a: u64| (a >> 12) << 10;
        const V: u64 = 1;
        const RW: u64 = (1 << 1) | (1 << 2);
        const A: u64 = 1 << 6; // Accessed set, Dirty NOT set

        bus.store(root, 8, ppn(mid) | V).unwrap();
        bus.store(mid, 8, ppn(leaf) | V).unwrap();
        bus.store(leaf + 8, 8, ppn(RAM_BASE) | V | RW | A).unwrap();

        let mut csrs = Csrs::default();
        csrs.write(csr::SATP, (8u64 << 60) | (root >> 12));

        // Cold: the very first access is a store, so the walk's own D-check
        // must fault.
        let mut mmu_cold = Mmu::default();
        let r_cold = mmu_cold.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Store);
        assert!(matches!(r_cold, Err(Exception::StorePageFault(0x1000))));

        // Warm: a load first installs a TLB entry (loads don't require D),
        // then a store against the same vaddr must still fault.
        let mut mmu_warm = Mmu::default();
        mmu_warm
            .translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Load)
            .unwrap();
        assert_eq!(mmu_warm.walks, 1, "the load must have installed a TLB entry");
        let r_warm = mmu_warm.translate(&mut bus, &csrs, Priv::S, 0x1000, Access::Store);
        assert!(
            matches!(r_warm, Err(Exception::StorePageFault(0x1000))),
            "a warm TLB entry must not bypass the D-bit check: got {r_warm:?}"
        );
        assert_eq!(mmu_warm.walks, 1, "the store must have hit the TLB, not re-walked");
    }
}
