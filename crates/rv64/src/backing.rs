use crate::PAGE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Page index is beyond the configured store size.
    OutOfRange,
    /// The underlying medium failed (I/O error, USB timeout, flash fault).
    Medium,
}

/// Backing store for guest physical memory, one page at a time.
///
/// Implementations must behave as a flat array of `PAGE`-sized pages that
/// reads back as zero before first write.
pub trait MemBacking {
    fn read_page(&mut self, page: u32, buf: &mut [u8; PAGE]) -> Result<(), Error>;
    fn write_page(&mut self, page: u32, buf: &[u8; PAGE]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}

extern crate alloc;
use alloc::vec::Vec;

/// In-memory backing used by tests.
pub struct FakeBacking {
    pages: Vec<[u8; PAGE]>,
}

impl FakeBacking {
    pub fn new(pages: u32) -> Self {
        Self { pages: alloc::vec![[0u8; PAGE]; pages as usize] }
    }
}

impl MemBacking for FakeBacking {
    fn read_page(&mut self, page: u32, buf: &mut [u8; PAGE]) -> Result<(), Error> {
        let p = self.pages.get(page as usize).ok_or(Error::OutOfRange)?;
        buf.copy_from_slice(p);
        Ok(())
    }

    fn write_page(&mut self, page: u32, buf: &[u8; PAGE]) -> Result<(), Error> {
        let p = self.pages.get_mut(page as usize).ok_or(Error::OutOfRange)?;
        p.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Behavioural conformance suite. Every `MemBacking` implementation must pass
/// this, including the USB and Xous-swap backends in later plans.
pub fn conformance<B: MemBacking>(b: &mut B, pages: u32) {
    assert!(pages >= 4, "conformance needs at least 4 pages");

    // Untouched pages read as zero.
    let mut buf = [0xFFu8; PAGE];
    b.read_page(0, &mut buf).unwrap();
    assert!(buf.iter().all(|&x| x == 0), "fresh page must read zero");

    // Write/read round-trip.
    let mut w = [0u8; PAGE];
    for (i, byte) in w.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    b.write_page(1, &w).unwrap();
    let mut r = [0u8; PAGE];
    b.read_page(1, &mut r).unwrap();
    assert_eq!(r, w, "round-trip must preserve bytes");

    // Writes do not bleed into neighbours.
    b.read_page(0, &mut r).unwrap();
    assert!(r.iter().all(|&x| x == 0), "page 0 must be untouched");
    b.read_page(2, &mut r).unwrap();
    assert!(r.iter().all(|&x| x == 0), "page 2 must be untouched");

    // Overwrite replaces rather than merges.
    let z = [0u8; PAGE];
    b.write_page(1, &z).unwrap();
    b.read_page(1, &mut r).unwrap();
    assert_eq!(r, z, "overwrite must fully replace");

    // Flush after writes succeeds and preserves data.
    b.write_page(3, &w).unwrap();
    b.flush().unwrap();
    b.read_page(3, &mut r).unwrap();
    assert_eq!(r, w, "flush must not corrupt");

    // Out-of-range is an error, not a panic.
    assert_eq!(b.read_page(pages, &mut r), Err(Error::OutOfRange));

    // Out-of-range write must also be an error, not panic or silent failure.
    assert_eq!(b.write_page(pages, &w), Err(Error::OutOfRange));

    // Flush must still succeed after a rejected write; backend must not be left broken.
    b.flush().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_backing_passes_conformance() {
        let mut b = FakeBacking::new(16);
        conformance(&mut b, 16);
    }

    #[test]
    fn read_of_untouched_page_is_zeroed() {
        let mut b = FakeBacking::new(4);
        let mut buf = [0xAAu8; PAGE];
        b.read_page(2, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == 0));
    }

    #[test]
    fn out_of_range_page_errors() {
        let mut b = FakeBacking::new(4);
        let mut buf = [0u8; PAGE];
        assert_eq!(b.read_page(4, &mut buf), Err(Error::OutOfRange));
    }
}
