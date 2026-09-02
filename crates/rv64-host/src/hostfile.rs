//! `HostFile`: a `MemBacking` whose pages live in an ordinary host file.

use rv64::backing::{Error, MemBacking};
use rv64::PAGE;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// File-backed guest memory. Sparse: the file is created at full length and
/// unwritten regions read as zero, matching the `MemBacking` contract.
pub struct HostFile {
    file: File,
    pages: u32,
}

impl HostFile {
    pub fn new(path: impl AsRef<Path>, pages: u32) -> std::io::Result<Self> {
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        // `set_len` on a freshly truncated file leaves a hole, not `pages *
        // PAGE` bytes of written zeroes — this is what makes the backing
        // sparse, and it is also what makes the "reads as zero before first
        // write" half of the `MemBacking` contract hold without an explicit
        // zero-fill pass.
        file.set_len(pages as u64 * PAGE as u64)?;
        Ok(Self { file, pages })
    }

    pub fn pages(&self) -> u32 {
        self.pages
    }
}

impl MemBacking for HostFile {
    fn read_page(&mut self, page: u32, buf: &mut [u8; PAGE]) -> Result<(), Error> {
        if page >= self.pages {
            return Err(Error::OutOfRange);
        }
        self.file.seek(SeekFrom::Start(page as u64 * PAGE as u64)).map_err(|_| Error::Medium)?;
        self.file.read_exact(buf).map_err(|_| Error::Medium)
    }

    fn write_page(&mut self, page: u32, buf: &[u8; PAGE]) -> Result<(), Error> {
        if page >= self.pages {
            return Err(Error::OutOfRange);
        }
        self.file.seek(SeekFrom::Start(page as u64 * PAGE as u64)).map_err(|_| Error::Medium)?;
        self.file.write_all(buf).map_err(|_| Error::Medium)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.file.flush().map_err(|_| Error::Medium)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts each test's image in its own file: the conformance suite writes
    /// through the backing, so two tests sharing a path would race under
    /// cargo's default threaded runner.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rv64-hostfile-{tag}-{}.img", std::process::id()))
    }

    #[test]
    fn hostfile_passes_conformance() {
        let path = temp_path("conformance");
        let mut b = HostFile::new(&path, 16).unwrap();
        rv64::backing::conformance(&mut b, 16);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fresh_pages_read_as_zero() {
        let path = temp_path("zero");
        let mut b = HostFile::new(&path, 4).unwrap();
        let mut buf = [0xAAu8; PAGE];
        b.read_page(3, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn out_of_range_page_is_rejected_in_both_directions() {
        let path = temp_path("range");
        let mut b = HostFile::new(&path, 4).unwrap();
        let mut r = [0u8; PAGE];
        assert_eq!(b.read_page(4, &mut r), Err(Error::OutOfRange));
        assert_eq!(b.write_page(4, &[0u8; PAGE]), Err(Error::OutOfRange));
        let _ = std::fs::remove_file(&path);
    }

    /// The file is the store, so a page written through one `HostFile`
    /// must be visible to a later one opened over the same path — which is
    /// the whole point of a file-backed store and is not covered by the
    /// in-process conformance suite.
    #[test]
    fn writes_survive_reopening_the_file() {
        let path = temp_path("persist");
        {
            let mut b = HostFile::new(&path, 4).unwrap();
            let mut page = [0u8; PAGE];
            page[0] = 0x5A;
            b.write_page(2, &page).unwrap();
            b.flush().unwrap();
        }
        // `HostFile::new` truncates, so reopen the file directly rather
        // than through `new`.
        let mut f = File::open(&path).unwrap();
        f.seek(SeekFrom::Start(2 * PAGE as u64)).unwrap();
        let mut buf = [0u8; PAGE];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 0x5A);
        let _ = std::fs::remove_file(&path);
    }
}
