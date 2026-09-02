extern crate alloc;
use alloc::vec::Vec;

use crate::backing::{Error, MemBacking};
use crate::{PAGE, RAM_BASE, RAM_SIZE};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub writebacks: u64,
    pub evictions: u64,
    /// Evictions that were *declined* because the victim was dirty and the
    /// backing refused to take it, and that were served from a clean frame
    /// instead. See `resident`: non-zero means the backing failed a write and
    /// the cache carried on rather than ending the run.
    pub declined: u64,
}

fn check_range(gpa: u64, len: usize) -> Result<(), Error> {
    let end = gpa.checked_add(len as u64).ok_or(Error::OutOfRange)?;
    if gpa < RAM_BASE || end > RAM_BASE + RAM_SIZE {
        return Err(Error::OutOfRange);
    }
    Ok(())
}

struct Frame {
    page: u32,
    dirty: bool,
    referenced: bool,
    pinned: bool,
    data: [u8; PAGE],
}

/// Guest RAM in pages: the domain of the residency index below, and the
/// hard ceiling on how many frames can ever be occupied at once.
const RAM_PAGES: usize = (RAM_SIZE / PAGE as u64) as usize;

/// Residency index entry: `0` means "not resident", anything else is the
/// frame slot *plus one*. Biasing by one is what lets the whole index start
/// out as zeroes, which is both the cheapest thing to allocate and the only
/// state that needs no initialisation pass.
type Slot = u16;
const NOT_RESIDENT: Slot = 0;

// The largest value ever stored is a *biased* slot, and the largest slot is
// `RAM_PAGES - 1`, so the constraint is `RAM_PAGES <= Slot::MAX` — a ceiling
// of 65535 guest pages, i.e. a `RAM_SIZE` of 0x0FFF_F000 (4 KiB short of
// 256 MiB). Raising `RAM_SIZE` past that fails the build here rather than
// silently aliasing two guest pages onto one frame.
const _: () =
    assert!(RAM_PAGES <= Slot::MAX as usize, "a biased slot must fit in Slot; RAM_SIZE too large");

/// Demand-paged view of guest physical RAM over a `MemBacking`.
///
/// Uses CLOCK eviction with a pin set. Pinning matters because evicting a
/// guest page table costs a walk-triggered refill on the very next access.
pub struct PageCache<B: MemBacking> {
    backing: B,
    frames: Vec<Frame>,
    /// Guest page number -> frame slot, biased as [`NOT_RESIDENT`] describes.
    ///
    /// Direct-mapped over all of guest RAM rather than hashed, because guest
    /// RAM is a fixed [`RAM_PAGES`] pages, so a flat `u16` per page is 16 KiB
    /// whatever the frame count is. Being constant cuts both ways, and the
    /// badge is the case that matters: 16 KiB is 1.6% of the frame array at
    /// the default 256 frames, but 256 frames is already a megabyte, and at a
    /// badge-plausible 32-64 frames the index is 6-12% of the cache it
    /// indexes. It stays ~1.1-1.6% of the badge's RAM budget either way,
    /// which is what makes the trade acceptable — not the flattering ratio
    /// against a cache size the badge will not be running.
    ///
    /// What it buys is the property that matters most on a microcontroller
    /// with no debugger attached: every lookup is one array read, with no
    /// probe sequence, no clustering, no tombstones and no resize, so the
    /// worst case equals the average case and the whole thing is fifteen
    /// lines you can read in one sitting.
    ///
    /// The invariant, maintained at exactly the three sites that move a page
    /// into or out of a frame: `resident_slot(p) == Some(i)` if and only if
    /// `frames[i].page == p`. It is checked exhaustively in the tests.
    slot_of: Vec<Slot>,
    hand: usize,
    capacity: usize,
    stats: Stats,
}

impl<B: MemBacking> PageCache<B> {
    pub fn new(backing: B, capacity: usize) -> Self {
        assert!(capacity >= 1, "cache needs at least one frame");
        // Frames beyond one per guest page can never be occupied, so a larger
        // request is a mistake worth reporting rather than ~410 MB of `Vec`
        // silently reserved and never used. It is also what makes
        // `set_resident`'s narrowing to `Slot` locally obvious: `slot <
        // frames.len() <= capacity <= RAM_PAGES`, with no appeal to how pages
        // are admitted.
        assert!(
            capacity <= RAM_PAGES,
            "cache cannot hold more frames than guest RAM has pages"
        );
        Self {
            backing,
            frames: Vec::with_capacity(capacity),
            slot_of: alloc::vec![NOT_RESIDENT; RAM_PAGES],
            hand: 0,
            capacity,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The frame holding `page`, or `None` if it is not resident.
    ///
    /// Tolerates a page number outside guest RAM by reporting it as
    /// not resident: `read_bytes`/`write_bytes` can never produce one
    /// (`check_range` rejects the address first), but `pin` is public and
    /// unchecked, and under the old linear scan such a page simply matched
    /// nothing.
    fn resident_slot(&self, page: u32) -> Option<usize> {
        match self.slot_of.get(page as usize) {
            None | Some(&NOT_RESIDENT) => None,
            Some(&slot) => Some(slot as usize - 1),
        }
    }

    /// Records `page` as living in `slot`.
    ///
    /// Both bounds are established one function away, in `resident`, which
    /// rejects a page outside guest RAM before it can reach here — so
    /// `page < RAM_PAGES` indexes in range, and `slot < frames.len() <=
    /// capacity <= RAM_PAGES` (the assertion in `new`) makes the narrowing to
    /// [`Slot`] lossless.
    fn set_resident(&mut self, page: u32, slot: usize) {
        self.slot_of[page as usize] = slot as Slot + 1;
    }

    /// Removes `page` from the index. Same bounds argument as
    /// [`Self::set_resident`]: `resident` has already rejected an
    /// out-of-RAM page.
    fn clear_resident(&mut self, page: u32) {
        self.slot_of[page as usize] = NOT_RESIDENT;
    }

    /// Mark a guest page as never-evictable.
    pub fn pin(&mut self, page: u32) {
        if let Some(i) = self.resident_slot(page) {
            self.frames[i].pinned = true;
        }
    }

    /// Writes every dirty frame back.
    ///
    /// Stops at the first failure and reports it: the frames already written
    /// are clean, the rest are still dirty and still hold the guest's bytes,
    /// and nothing is dropped. A caller that ignores the `Err` *would* lose
    /// data — `rv64-host`'s `main.rs` does not, and the badge never calls this
    /// at all, because `UsbHost`'s writes are acknowledged synchronously and
    /// there is no such thing as an unflushed one.
    pub fn flush(&mut self) -> Result<(), Error> {
        for i in 0..self.frames.len() {
            if self.frames[i].dirty {
                let (page, data) = (self.frames[i].page, self.frames[i].data);
                self.backing.write_page(page, &data)?;
                self.frames[i].dirty = false;
                self.stats.writebacks += 1;
            }
        }
        self.backing.flush()
    }

    fn resident(&mut self, page: u32) -> Result<usize, Error> {
        if let Some(i) = self.resident_slot(page) {
            self.frames[i].referenced = true;
            self.stats.hits += 1;
            return Ok(i);
        }
        // Off the hot path, and the only place this is checked: everything
        // below writes the index by direct indexing, and `Bus::cache_mut`
        // hands out a `&mut PageCache`, so a future prefetch or badge-side
        // refill helper that calls in here without going through
        // `read_bytes`'s `check_range` gets an error rather than a panic in
        // `set_resident`. An address outside RAM is not a cache miss, so it
        // is rejected before the counter moves.
        if page as usize >= RAM_PAGES {
            return Err(Error::OutOfRange);
        }
        self.stats.misses += 1;

        let mut data = [0u8; PAGE];
        self.backing.read_page(page, &mut data)?;

        if self.frames.len() < self.capacity {
            self.frames.push(Frame {
                page,
                dirty: false,
                referenced: true,
                pinned: false,
                data,
            });
            let slot = self.frames.len() - 1;
            self.set_resident(page, slot);
            return Ok(slot);
        }

        // **A failed writeback must not lose the page**, and it need not end
        // the run either.
        //
        // The dirty victim is never overwritten on a write failure, so the
        // guest's bytes stay resident and stay dirty — that half has always
        // been true and `a_failed_writeback_keeps_the_dirty_page` pins it. A
        // dropped dirty page would be silent data corruption rather than a
        // stall, which is strictly worse, so the ordering below is not
        // negotiable.
        //
        // What is new is the second half: a refused write is no longer
        // automatically fatal. The miss still has to be served, and it can be
        // served out of any **clean** frame, because a clean frame's contents
        // are already in the backing and evicting one writes nothing. So the
        // cache declines the eviction it cannot pay for, takes a clean frame
        // instead, and the guest carries on with a smaller effective cache —
        // every dirty page still resident, still dirty, and retried by the
        // next eviction that picks it. Only when there is no clean frame at
        // all does the error propagate as before.
        //
        // This is bounded and self-healing by construction: exactly one write
        // is attempted per miss either way, so a link that has gone slow
        // cannot be made slower by the fallback, and a link that comes back
        // resumes writing back on the very next eviction. It cannot lose data
        // — `select_clean_victim` returns only `!dirty` frames.
        //
        // The failure this does *not* reach is a backing whose `write_page`
        // never returns; nothing on this side of that call can.
        let mut victim = self.select_victim();
        if self.frames[victim].dirty {
            let (p, d) = (self.frames[victim].page, self.frames[victim].data);
            match self.backing.write_page(p, &d) {
                Ok(()) => self.stats.writebacks += 1,
                Err(e) => match self.select_clean_victim() {
                    Some(clean) => {
                        self.stats.declined += 1;
                        victim = clean;
                    }
                    None => return Err(e),
                },
            }
        }
        self.stats.evictions += 1;
        // Drop the outgoing page from the index before the incoming one is
        // added. The two can never be the same page — `page` reached here
        // precisely because it was not resident — so the order is a matter
        // of reading clearly rather than of correctness.
        self.clear_resident(self.frames[victim].page);
        self.frames[victim] = Frame {
            page,
            dirty: false,
            referenced: true,
            pinned: false,
            data,
        };
        self.set_resident(page, victim);
        Ok(victim)
    }

    /// CLOCK: sweep clearing reference bits until an unreferenced, unpinned
    /// frame is found.
    fn select_victim(&mut self) -> usize {
        let n = self.frames.len();
        for _ in 0..(2 * n) {
            let i = self.hand;
            self.hand = (self.hand + 1) % n;
            if self.frames[i].pinned {
                continue;
            }
            if self.frames[i].referenced {
                self.frames[i].referenced = false;
                continue;
            }
            return i;
        }
        // Everything pinned or referenced: take the first unpinned frame.
        self.frames
            .iter()
            .position(|f| !f.pinned)
            .expect("all frames pinned; cache too small")
    }

    /// A clean, unpinned frame — the fallback victim when a writeback has
    /// just been refused.
    ///
    /// Deliberately *not* part of `select_victim`: preferring clean frames in
    /// the ordinary case is a well-known way to make a page cache much worse,
    /// and this guest is the case that proves it. Measured over the full
    /// 173.5 M-instruction boot at 1024 frames, a clean-first CLOCK moved
    /// misses from 2,826 to 3,787,409 — three orders of magnitude — while
    /// saving 8% of writebacks, because the clean set here is almost entirely
    /// hot read-only text and preferring it evicts exactly the pages the guest
    /// is about to want. So this runs only after a write has already failed,
    /// where the alternative is not a worse cache but no cache at all.
    ///
    /// Reference bits are read and never cleared: this is an emergency path,
    /// and a sweep that aged every frame would corrupt the CLOCK approximation
    /// for the ordinary evictions that follow. An unreferenced frame is
    /// preferred, with the first clean one taken if every clean frame is hot.
    ///
    /// The single property the caller depends on: **the returned frame is
    /// never dirty**, so taking it writes nothing and loses nothing.
    fn select_clean_victim(&mut self) -> Option<usize> {
        let n = self.frames.len();
        let mut hot = None;
        for k in 0..n {
            let i = (self.hand + k) % n;
            if self.frames[i].pinned || self.frames[i].dirty {
                continue;
            }
            if !self.frames[i].referenced {
                self.hand = (i + 1) % n;
                return Some(i);
            }
            if hot.is_none() {
                hot = Some(i);
            }
        }
        if let Some(i) = hot {
            self.hand = (i + 1) % n;
        }
        hot
    }

    fn page_of(gpa: u64) -> u32 {
        ((gpa - RAM_BASE) / PAGE as u64) as u32
    }

    /// Exhaustively checks the index against the frames it describes, in
    /// both directions. Test-only: it is `O(RAM_PAGES)` and exists so that
    /// tests can assert the invariant directly rather than inferring it from
    /// counters that a stale entry might leave undisturbed.
    #[cfg(test)]
    fn assert_index_matches_frames(&self) {
        for (i, f) in self.frames.iter().enumerate() {
            let page = f.page;
            assert_eq!(self.resident_slot(page), Some(i), "frame {i} (page {page}) not indexed");
        }
        for page in 0..RAM_PAGES {
            if let Some(i) = self.resident_slot(page as u32) {
                assert!(i < self.frames.len(), "page {page} indexed to absent frame {i}");
                assert_eq!(self.frames[i].page, page as u32, "stale index entry for page {page}");
            }
        }
    }

    pub fn read_bytes(&mut self, gpa: u64, buf: &mut [u8]) -> Result<(), Error> {
        check_range(gpa, buf.len())?;
        let mut done = 0;
        while done < buf.len() {
            let addr = gpa + done as u64;
            let off = (addr as usize) % PAGE;
            let n = core::cmp::min(PAGE - off, buf.len() - done);
            let idx = self.resident(Self::page_of(addr))?;
            buf[done..done + n].copy_from_slice(&self.frames[idx].data[off..off + n]);
            done += n;
        }
        Ok(())
    }

    pub fn write_bytes(&mut self, gpa: u64, buf: &[u8]) -> Result<(), Error> {
        check_range(gpa, buf.len())?;
        let mut done = 0;
        while done < buf.len() {
            let addr = gpa + done as u64;
            let off = (addr as usize) % PAGE;
            let n = core::cmp::min(PAGE - off, buf.len() - done);
            let idx = self.resident(Self::page_of(addr))?;
            self.frames[idx].data[off..off + n].copy_from_slice(&buf[done..done + n]);
            self.frames[idx].dirty = true;
            done += n;
        }
        Ok(())
    }

    /// Reads `N` bytes at `gpa` as a little-endian integer, zero-extended to
    /// `u64`. `N` must be in `1..=8`; the build fails otherwise.
    ///
    /// This exists because *every* guest load is one of these, and routing
    /// them through [`Self::read_bytes`] made the width a runtime property of
    /// a slice. `copy_from_slice` on a slice whose length the compiler cannot
    /// see is a call to `memmove` — through the PLT, on macOS — and profiling
    /// the Linux boot found **8.0% of samples** inside that call and its stub,
    /// copying one to eight bytes at a time (task-8 report §30d). With `N` a
    /// const parameter the copy is a fixed width and lowers to a single
    /// machine load. On the badge the saving is larger, not smaller: RV32 has
    /// no vectorised `memmove`, so `compiler_builtins` answers a short,
    /// possibly-unaligned copy with a byte loop.
    ///
    /// `read_bytes` stays for genuine variable-length work (frame fills,
    /// writebacks, the host's image loader) and for the straddling case here.
    ///
    /// # Bounds and page crossings
    ///
    /// The bounds are [`check_range`]'s, unchanged: the *whole* access must
    /// lie inside guest RAM, so an 8-byte read starting four bytes before the
    /// end of RAM is [`Error::OutOfRange`] rather than a truncated read.
    ///
    /// An access is only handled inline when all `N` bytes share one page.
    /// RV64 permits unaligned loads, so that is a real question and not a
    /// formality — but a 1/2/4/8-byte access can only cross a page when it is
    /// misaligned, and then the tail is handed straight back to `read_bytes`,
    /// whose loop already gets this right. The fast path is therefore an
    /// addition to the existing code, not a replacement for the part of it
    /// that is hard.
    pub fn read_le<const N: usize>(&mut self, gpa: u64) -> Result<u64, Error> {
        const { assert!(N >= 1 && N <= 8, "a sized access is 1 to 8 bytes wide") };
        check_range(gpa, N)?;
        let mut bytes = [0u8; 8];
        // `gpa as usize` truncates on a 32-bit host; the low 12 bits, which
        // are all `% PAGE` keeps, survive it. `read_bytes` relies on the same
        // thing.
        let off = (gpa as usize) % PAGE;
        if off + N <= PAGE {
            let idx = self.resident(Self::page_of(gpa))?;
            bytes[..N].copy_from_slice(&self.frames[idx].data[off..off + N]);
        } else {
            self.read_bytes(gpa, &mut bytes[..N])?;
        }
        Ok(u64::from_le_bytes(bytes))
    }

    /// Writes the low `N` bytes of `value`, little-endian, at `gpa`. The
    /// mirror of [`Self::read_le`]; see it for why the width is a const
    /// parameter, and for the bounds and page-crossing rules, which are the
    /// same.
    pub fn write_le<const N: usize>(&mut self, gpa: u64, value: u64) -> Result<(), Error> {
        const { assert!(N >= 1 && N <= 8, "a sized access is 1 to 8 bytes wide") };
        check_range(gpa, N)?;
        let bytes = value.to_le_bytes();
        let off = (gpa as usize) % PAGE;
        if off + N > PAGE {
            return self.write_bytes(gpa, &bytes[..N]);
        }
        let idx = self.resident(Self::page_of(gpa))?;
        let frame = &mut self.frames[idx];
        frame.data[off..off + N].copy_from_slice(&bytes[..N]);
        frame.dirty = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backing::{Error, FakeBacking};
    use crate::RAM_BASE;

    fn cache(frames: usize, pages: u32) -> PageCache<FakeBacking> {
        PageCache::new(FakeBacking::new(pages), frames)
    }

    #[test]
    fn write_then_read_round_trips() {
        let mut c = cache(2, 8);
        c.write_bytes(RAM_BASE + 0x10, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        c.read_bytes(RAM_BASE + 0x10, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn second_access_to_same_page_is_a_hit() {
        let mut c = cache(2, 8);
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        let after_first = c.stats();
        c.read_bytes(RAM_BASE + 8, &mut buf).unwrap();
        let after_second = c.stats();
        assert_eq!(after_first.misses, 1);
        assert_eq!(after_second.misses, 1, "same page must not miss twice");
        assert_eq!(after_second.hits, after_first.hits + 1);
    }

    #[test]
    fn dirty_page_survives_eviction() {
        let mut c = cache(1, 8);
        c.write_bytes(RAM_BASE, &[0xAB]).unwrap();
        // Touch a different page, forcing eviction of the dirty one.
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE + PAGE as u64, &mut buf).unwrap();
        assert!(c.stats().writebacks >= 1, "dirty eviction must write back");
        // Bring it back and confirm the byte survived.
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);
    }

    #[test]
    fn clean_page_eviction_does_not_write_back() {
        let mut c = cache(1, 8);
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        c.read_bytes(RAM_BASE + PAGE as u64, &mut buf).unwrap();
        assert_eq!(c.stats().writebacks, 0);
    }

    /// A backing that reads fine and cannot write — the badge's transmit
    /// failure, which is the only failure mode this port has ever actually
    /// had.
    struct WriteOnlyFails {
        inner: FakeBacking,
        writes_attempted: usize,
    }
    impl MemBacking for WriteOnlyFails {
        fn read_page(&mut self, page: u32, buf: &mut [u8; PAGE]) -> Result<(), Error> {
            self.inner.read_page(page, buf)
        }
        fn write_page(&mut self, _page: u32, _buf: &[u8; PAGE]) -> Result<(), Error> {
            self.writes_attempted += 1;
            Err(Error::Medium)
        }
        fn flush(&mut self) -> Result<(), Error> {
            Err(Error::Medium)
        }
    }

    /// **A failed writeback must not lose the guest's dirty page.** Losing one
    /// would be silent data corruption — the guest would read back stale bytes
    /// later with nothing anywhere saying why — and it is strictly worse than
    /// the stall a propagated error produces.
    ///
    /// So the eviction must not happen at all: the error comes out, the dirty
    /// frame is still resident and still dirty, `evictions` and `writebacks`
    /// have not moved, and the bytes are readable from the cache afterwards.
    #[test]
    fn a_failed_writeback_keeps_the_dirty_page() {
        let mut c = PageCache::new(
            WriteOnlyFails { inner: FakeBacking::new(8), writes_attempted: 0 },
            1,
        );
        c.write_bytes(RAM_BASE, &[0xAB]).unwrap();

        // One frame, so touching page 1 must evict the dirty page 0.
        let mut buf = [0u8; 1];
        assert_eq!(c.read_bytes(RAM_BASE + PAGE as u64, &mut buf), Err(Error::Medium));
        assert_eq!(c.backing.writes_attempted, 1, "the writeback was attempted");

        let s = c.stats();
        assert_eq!((s.writebacks, s.evictions), (0, 0), "neither counter may move on a failure");

        // The bytes are still here, and still dirty: a later eviction will try
        // again rather than having thrown them away.
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(buf, [0xAB], "the guest's dirty bytes survived the failed eviction");
        assert!(c.frames.iter().any(|f| f.page == 0 && f.dirty));
    }

    /// **A refused writeback must not end the run while a clean frame is
    /// available.** This is the badge's whole failure mode: the backing is a
    /// USB link, its only real fault is a transmit that gives up, and a boot
    /// that dies on the first such fault is a boot that never reaches a shell.
    ///
    /// Four frames, three of them clean. Page 0 is dirtied and the backing
    /// then refuses every write, so every eviction that picks page 0 must be
    /// declined and served from a clean frame instead — and the run must keep
    /// going, with page 0's bytes still resident, still dirty, and still
    /// correct at the end.
    #[test]
    fn a_refused_writeback_falls_back_to_a_clean_frame_instead_of_failing() {
        let mut c = PageCache::new(
            WriteOnlyFails { inner: FakeBacking::new(64), writes_attempted: 0 },
            4,
        );
        let mut buf = [0u8; 1];
        // Page 0 is the only dirty frame there will ever be.
        c.write_bytes(RAM_BASE, &[0xAB]).unwrap();

        // Cycle far more pages than there are frames. Every one of these is a
        // miss that must be served, and the CLOCK hand passes the dirty frame
        // repeatedly.
        for i in 1..40u64 {
            c.read_bytes(RAM_BASE + i * PAGE as u64, &mut buf)
                .unwrap_or_else(|e| panic!("read of page {i} ended the run: {e:?}"));
        }

        let s = c.stats();
        assert_eq!(s.writebacks, 0, "the backing accepted a write it was supposed to refuse");
        assert!(s.declined > 0, "the fallback never fired, so this proved nothing");
        assert!(c.backing.writes_attempted > 0, "no writeback was ever attempted");

        // The dirty page is still here and still dirty: nothing was dropped,
        // and a later eviction will try to write it again.
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(buf, [0xAB], "the guest's dirty bytes did not survive the declined eviction");
        assert!(c.frames.iter().any(|f| f.page == 0 && f.dirty));
        c.assert_index_matches_frames();
    }

    /// The fallback's one invariant, checked directly rather than inferred
    /// from counters: `select_clean_victim` never names a dirty frame. If it
    /// ever did, the caller would overwrite guest bytes that are not in the
    /// backing — silent corruption, and the one outcome worse than stalling.
    #[test]
    fn the_clean_fallback_never_names_a_dirty_frame() {
        let mut c = cache(8, 64);
        // A mix: some frames dirtied, some only read.
        for i in 0..8u64 {
            if i % 2 == 0 {
                c.write_bytes(RAM_BASE + i * PAGE as u64, &[i as u8]).unwrap();
            } else {
                let mut b = [0u8; 1];
                c.read_bytes(RAM_BASE + i * PAGE as u64, &mut b).unwrap();
            }
        }
        for _ in 0..32 {
            match c.select_clean_victim() {
                Some(i) => assert!(!c.frames[i].dirty && !c.frames[i].pinned, "frame {i}"),
                None => break,
            }
        }
    }

    /// And the other side of it: with **no** clean frame to fall back to, a
    /// refused writeback is still fatal. The fallback narrows the failure, it
    /// does not paper over it.
    #[test]
    fn a_refused_writeback_with_every_frame_dirty_still_fails() {
        let mut c = PageCache::new(
            WriteOnlyFails { inner: FakeBacking::new(64), writes_attempted: 0 },
            3,
        );
        for i in 0..3u64 {
            c.write_bytes(RAM_BASE + i * PAGE as u64, &[i as u8]).unwrap();
        }
        let mut buf = [0u8; 1];
        assert_eq!(
            c.read_bytes(RAM_BASE + 3 * PAGE as u64, &mut buf),
            Err(Error::Medium),
            "with no clean frame anywhere, the error must still reach the caller"
        );
        let s = c.stats();
        assert_eq!((s.writebacks, s.evictions, s.declined), (0, 0, 0));
    }

    #[test]
    fn pinned_page_is_never_evicted() {
        let mut c = cache(2, 8);
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        c.pin(0);
        // Cycle through many other pages.
        for i in 1..6u64 {
            c.read_bytes(RAM_BASE + i * PAGE as u64, &mut buf).unwrap();
        }
        let before = c.stats().misses;
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(c.stats().misses, before, "pinned page must still be resident");
    }

    #[test]
    fn access_spanning_two_pages_works() {
        let mut c = cache(2, 8);
        let gpa = RAM_BASE + PAGE as u64 - 2;
        c.write_bytes(gpa, &[9, 8, 7, 6]).unwrap();
        let mut buf = [0u8; 4];
        c.read_bytes(gpa, &mut buf).unwrap();
        assert_eq!(buf, [9, 8, 7, 6]);
    }

    #[test]
    fn address_below_ram_base_is_rejected() {
        let mut c = cache(2, 8);
        let mut buf = [0u8; 4];
        assert_eq!(c.read_bytes(RAM_BASE - 4, &mut buf), Err(Error::OutOfRange));
        assert_eq!(c.write_bytes(RAM_BASE - 4, &[0; 4]), Err(Error::OutOfRange));
    }

    #[test]
    fn access_past_end_of_ram_is_rejected() {
        let mut c = cache(2, 8);
        let mut buf = [0u8; 8];
        let last = RAM_BASE + crate::RAM_SIZE - 4;
        assert_eq!(c.read_bytes(last, &mut buf), Err(Error::OutOfRange));
    }

    /// A page number no `read_bytes`/`write_bytes` call could ever produce,
    /// because `check_range` rejects the address first. `pin` is public and
    /// unchecked, so it is the one way a caller can hand the cache a page
    /// outside guest RAM; it must stay a no-op rather than panicking.
    #[test]
    fn pinning_a_page_outside_ram_is_a_no_op() {
        let mut c = cache(2, 8);
        c.pin(u32::MAX);
        c.pin((crate::RAM_SIZE / PAGE as u64) as u32);
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(c.stats().misses, 1);
    }

    /// `resident` is private and every path to it today is `check_range`-
    /// gated, but `PageCache` is public and `Bus::cache_mut` hands out a
    /// `&mut` to it, so the guard has to hold at `resident`'s own boundary
    /// rather than two functions up. An out-of-RAM page is an error, and
    /// specifically *not* a miss: it must not move the counters.
    #[test]
    fn a_page_outside_ram_is_an_error_rather_than_a_miss() {
        let mut c = cache(2, 8);
        let before = c.stats();
        assert_eq!(c.resident(RAM_PAGES as u32), Err(Error::OutOfRange));
        assert_eq!(c.resident(u32::MAX), Err(Error::OutOfRange));
        assert_eq!(c.stats(), before, "a bad address is not a cache miss");
    }

    /// A frame per guest page is the most that can ever be occupied, so a
    /// larger request is a mistake — and left unchecked it would reserve
    /// hundreds of megabytes of frames that can never hold anything.
    #[test]
    #[should_panic(expected = "more frames than guest RAM has pages")]
    fn a_capacity_larger_than_guest_ram_is_rejected() {
        let _ = cache(RAM_PAGES + 1, 8);
    }

    #[test]
    fn a_capacity_of_exactly_guest_ram_is_allowed() {
        let mut c = cache(RAM_PAGES, 8);
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(c.stats().misses, 1);
    }

    /// Deterministic LCG, so the workload below is the same sequence on every
    /// machine and every run. Not cryptographic and not meant to be.
    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state >> 17
    }

    /// Characterisation test: a long, eviction-heavy access pattern whose
    /// counters are pinned to values *recorded from the linear-scan
    /// implementation*. Any change to which frame a lookup finds, to the
    /// CLOCK hand's path, or to the dirty-bit lifetime moves at least one of
    /// these numbers.
    ///
    /// It exists because the only other check on those semantics is a 175 M
    /// instruction Linux boot: slow, coarse, and unable to say *which* of the
    /// four counters moved. This runs in milliseconds and localises the
    /// damage. The shadow copy alongside it proves the separate, stronger
    /// property that a lookup returns the frame actually holding the page —
    /// a mis-indexed lookup that happened to keep the counters intact would
    /// still be caught here, as wrong bytes.
    #[test]
    fn eviction_heavy_workload_reproduces_recorded_statistics() {
        const PAGES: u32 = 96;
        const FRAMES: usize = 16;
        let mut c = cache(FRAMES, PAGES);
        let mut shadow = alloc::vec![0u8; PAGES as usize * PAGE];
        let mut state = 0x1234_5678_9abc_def0u64;

        for step in 0..40_000u64 {
            let r = lcg(&mut state);
            // Skewed towards a hot subset that fits in the frames, so the
            // run mixes hits, misses and evictions rather than thrashing
            // uniformly.
            let page = if r.is_multiple_of(4) { r % PAGES as u64 } else { r % (FRAMES as u64 * 2) };
            let off = ((r >> 20) as usize) % (PAGE - 4);
            let gpa = RAM_BASE + page * PAGE as u64 + off as u64;
            let idx = page as usize * PAGE + off;
            if r.is_multiple_of(3) {
                let v = (step as u32).wrapping_mul(2654435761).to_le_bytes();
                c.write_bytes(gpa, &v).unwrap();
                shadow[idx..idx + 4].copy_from_slice(&v);
            } else {
                let mut buf = [0u8; 4];
                c.read_bytes(gpa, &mut buf).unwrap();
                assert_eq!(buf, shadow[idx..idx + 4], "wrong bytes at step {step}");
            }
        }

        // Every page read back through the cache, which forces the pages
        // written earlier and since evicted to come back from the backing.
        for page in 0..PAGES as usize {
            let mut buf = [0u8; 16];
            for chunk in 0..PAGE / 16 {
                let off = chunk * 16;
                c.read_bytes(RAM_BASE + (page * PAGE + off) as u64, &mut buf).unwrap();
                assert_eq!(buf, shadow[page * PAGE + off..page * PAGE + off + 16]);
            }
        }

        c.assert_index_matches_frames();

        // Recorded from the linear-scan implementation this index replaced.
        assert_eq!(
            c.stats(),
            Stats {
                hits: 40_489,
                misses: 24_087,
                writebacks: 10_501,
                evictions: 24_071,
                // This backing never refuses a write, so the decline path in
                // `resident` is unreachable here. Spelled out rather than
                // `..Default::default()`: the whole point of this assertion is
                // that it is exhaustive.
                declined: 0,
            }
        );
    }

    /// The index's own invariant, checked after every step of a workload
    /// that fills the frames, evicts from them, and re-admits pages that were
    /// evicted earlier — the three transitions that can leave it stale.
    #[test]
    fn index_agrees_with_the_frames_after_every_access() {
        let mut c = cache(4, 32);
        let mut buf = [0u8; 1];
        let order = [0u64, 1, 2, 3, 4, 0, 5, 1, 6, 2, 7, 0, 0, 8, 3, 9, 1, 10, 4, 11];
        for (step, page) in order.iter().enumerate() {
            if step % 2 == 0 {
                c.write_bytes(RAM_BASE + page * PAGE as u64, &[step as u8]).unwrap();
            } else {
                c.read_bytes(RAM_BASE + page * PAGE as u64, &mut buf).unwrap();
            }
            c.assert_index_matches_frames();
        }
    }

    /// A page that was resident, got evicted, and is touched again must be
    /// reported as *absent*, not found in the frame that replaced it. This is
    /// the failure a stale index entry produces, and it is silent: the
    /// counters still balance, the guest just reads another page's bytes.
    #[test]
    fn evicted_page_is_not_found_in_its_old_frame() {
        let mut c = cache(1, 8);
        c.write_bytes(RAM_BASE, &[0xAA]).unwrap();
        c.write_bytes(RAM_BASE + PAGE as u64, &[0xBB]).unwrap(); // evicts page 0
        assert_eq!(c.resident_slot(0), None, "evicted page must not stay indexed");
        assert_eq!(c.resident_slot(1), Some(0));
        let mut buf = [0u8; 1];
        c.read_bytes(RAM_BASE, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAA, "page 0's own byte, not page 1's");
    }

    /// Round-trips `value` at `gpa` through the sized path at width `N`, and
    /// checks the bytes it left behind against the byte path — the two are
    /// the same memory and must never disagree, whichever side of the
    /// page-crossing branch `gpa` falls on.
    fn round_trip<const N: usize>(c: &mut PageCache<FakeBacking>, gpa: u64, value: u64) {
        let truncated = if N == 8 { value } else { value & ((1u64 << (8 * N)) - 1) };
        c.write_le::<N>(gpa, value).unwrap();
        assert_eq!(c.read_le::<N>(gpa).unwrap(), truncated, "sized read at {gpa:#x}/{N}");
        let mut buf = [0u8; 8];
        c.read_bytes(gpa, &mut buf[..N]).unwrap();
        assert_eq!(
            u64::from_le_bytes(buf),
            truncated,
            "byte path disagrees with sized write at {gpa:#x}/{N}"
        );
        // And the other direction: bytes laid down by the generic path must
        // read back identically through the sized one.
        let other = !value;
        let other_truncated = if N == 8 { other } else { other & ((1u64 << (8 * N)) - 1) };
        c.write_bytes(gpa, &other.to_le_bytes()[..N]).unwrap();
        assert_eq!(
            c.read_le::<N>(gpa).unwrap(),
            other_truncated,
            "sized read disagrees with byte write at {gpa:#x}/{N}"
        );
    }

    #[test]
    fn every_width_round_trips_through_the_sized_path() {
        let mut c = cache(4, 8);
        let v = 0x0102_0304_0506_0708u64;
        round_trip::<1>(&mut c, RAM_BASE + 0x40, v);
        round_trip::<2>(&mut c, RAM_BASE + 0x50, v);
        round_trip::<4>(&mut c, RAM_BASE + 0x60, v);
        round_trip::<8>(&mut c, RAM_BASE + 0x70, v);
    }

    /// The case the fast path must decline. RV64 permits unaligned accesses,
    /// so a 2/4/8-byte access really can have its bytes on two different
    /// frames — and those two frames need not be adjacent, or even both
    /// resident. Every starting offset from eight bytes before a page
    /// boundary to eight bytes after it, at every width, against a cache with
    /// enough frames to hold both pages and (below) with too few.
    #[test]
    fn a_sized_access_across_a_page_boundary_matches_the_byte_path() {
        for frames in [1usize, 2, 4] {
            let mut c = cache(frames, 8);
            for delta in 0..16u64 {
                let gpa = RAM_BASE + PAGE as u64 - 8 + delta;
                let v = 0xDEAD_BEEF_CAFE_F00Du64 ^ delta;
                round_trip::<1>(&mut c, gpa, v);
                round_trip::<2>(&mut c, gpa, v);
                round_trip::<4>(&mut c, gpa, v);
                round_trip::<8>(&mut c, gpa, v);
            }
        }
    }

    /// A straddling write must land on *both* pages. Read back one byte at a
    /// time so a fast path that quietly dropped the tail cannot hide behind
    /// the same fast path on the way back in.
    #[test]
    fn a_straddling_sized_write_reaches_the_second_page() {
        let mut c = cache(2, 8);
        let gpa = RAM_BASE + PAGE as u64 - 3;
        c.write_le::<8>(gpa, 0x8877_6655_4433_2211).unwrap();
        let expect = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        for (i, want) in expect.iter().enumerate() {
            let mut b = [0u8; 1];
            c.read_bytes(gpa + i as u64, &mut b).unwrap();
            assert_eq!(b[0], *want, "byte {i} of a straddling store");
        }
    }

    /// The bounds are the whole access, not its first byte — the same rule
    /// `read_bytes` has always had, and the reason `Bus::load` can turn an
    /// error here into `BackingFailure` without re-deriving the range.
    #[test]
    fn a_sized_access_outside_ram_is_rejected() {
        let mut c = cache(2, 8);
        assert_eq!(c.read_le::<4>(RAM_BASE - 4), Err(Error::OutOfRange));
        assert_eq!(c.write_le::<4>(RAM_BASE - 4, 0), Err(Error::OutOfRange));
        // Starts inside RAM, ends past it.
        let last = RAM_BASE + crate::RAM_SIZE - 4;
        assert_eq!(c.read_le::<8>(last), Err(Error::OutOfRange));
        assert_eq!(c.write_le::<8>(last, 0), Err(Error::OutOfRange));
        // The other boundary, and the one the fast path's `off + N <= PAGE`
        // test gets wrong if it is written with `<`: an access ending exactly
        // on a page boundary does not cross it.
        let flush = RAM_BASE + PAGE as u64 - 8;
        assert_eq!(c.write_le::<8>(flush, 0x0102_0304_0506_0708), Ok(()));
        assert_eq!(c.read_le::<8>(flush), Ok(0x0102_0304_0506_0708));
    }

    /// The sized path must not change how often the cache is consulted:
    /// `PageCache::resident` is the emulator's hottest function and its
    /// hit/miss counters are what §28f's frame-count decision was made on.
    #[test]
    fn the_sized_path_touches_the_cache_exactly_as_often_as_the_byte_path() {
        for (gpa, width) in [(RAM_BASE + 0x100, 8u8), (RAM_BASE + PAGE as u64 - 3, 8)] {
            let mut sized = cache(4, 8);
            let mut bytes = cache(4, 8);
            for _ in 0..3 {
                sized.write_le::<8>(gpa, 0x1234_5678_9abc_def0).unwrap();
                let _ = sized.read_le::<8>(gpa).unwrap();
                bytes.write_bytes(gpa, &0x1234_5678_9abc_def0u64.to_le_bytes()).unwrap();
                let mut b = [0u8; 8];
                bytes.read_bytes(gpa, &mut b).unwrap();
            }
            assert_eq!(sized.stats(), bytes.stats(), "counters differ at {gpa:#x}/{width}");
        }
    }

    #[test]
    fn multibyte_access_within_one_page_counts_once() {
        let mut c = cache(2, 8);
        let mut buf = [0u8; 4];
        c.read_bytes(RAM_BASE, &mut buf).unwrap(); // miss, page now resident
        let before = c.stats();
        c.read_bytes(RAM_BASE + 8, &mut buf).unwrap();
        let after = c.stats();
        assert_eq!(after.hits, before.hits + 1, "one hit per page touched, not per byte");
        assert_eq!(after.misses, before.misses);
    }
}
