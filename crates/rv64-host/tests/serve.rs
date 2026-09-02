//! `serve` operates on a plain file, not on `HostFile`/`PageCache` — see
//! ruling R1 in the task-4 brief. The load phase (building a `HostFile`,
//! wrapping it in a `PageCache`/`Bus`, calling `load_boot_images`, then
//! `flush`) is exercised by `main.rs`'s `serve` subcommand and by the
//! existing `load_boot_images` tests; this suite drives `serve_once` and
//! `serve` directly against a plain file, exactly the shape the serve phase
//! uses after the load phase hands off.
use rv64_proto::{encode, Frame, Mux, PAGE};
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Drives `serve_once` over a plain file: a READ of a page we wrote must
/// come back byte-identical, and a `ConOut` arriving in the same input must
/// not be mistaken for a page request.
#[test]
fn serve_answers_a_read_from_the_backing_image() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guest.img");

    // Write a page directly into a plain file — no `HostFile`, no
    // `MemBacking`, matching what the serve phase actually opens.
    let mut img =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    let len = 64 * PAGE as u64;
    img.set_len(len).unwrap();
    let mut page = [0u8; PAGE];
    page[0..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    img.seek(SeekFrom::Start(3 * PAGE as u64)).unwrap();
    img.write_all(&page).unwrap();

    let mut req = Vec::new();
    encode(&Frame::ReadReq { page: 3 }, &mut req);
    encode(&Frame::ConOut(b"hi".to_vec()), &mut req);

    let mut m = Mux::new();
    let mut out = Vec::new();
    rv64_host::serve::serve_once(&mut img, len, &mut m, &req, &mut out, &mut Vec::new()).unwrap();

    let mut resp = Mux::new();
    resp.push(&out);
    match resp.take_matching(0x02) {
        Some(Frame::ReadResp { page: p, data }) => {
            assert_eq!(p, 3);
            assert_eq!(&data[0..4], &[0xde, 0xad, 0xbe, 0xef]);
        }
        other => panic!("expected a ReadResp, got {other:?}"),
    }
    // The ConOut frame went to stdout, not into `out` as a held request —
    // nothing else should be sitting in the reply stream.
    assert_eq!(resp.take_matching(0x02), None);
    assert_eq!(resp.take_console(), Vec::<u8>::new());
}

/// A read for a page beyond the file's length must come back as an `Err`
/// frame, not panic and not a silently zeroed page.
#[test]
fn serve_reports_a_read_past_the_end_of_the_image_as_an_error_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guest.img");
    let mut img =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    let len = 4 * PAGE as u64;
    img.set_len(len).unwrap();

    let mut req = Vec::new();
    encode(&Frame::ReadReq { page: 4 }, &mut req); // one page past the end

    let mut m = Mux::new();
    let mut out = Vec::new();
    rv64_host::serve::serve_once(&mut img, len, &mut m, &req, &mut out, &mut Vec::new()).unwrap();

    let mut resp = Mux::new();
    resp.push(&out);
    match resp.take_matching(0x07) {
        Some(Frame::Err { page, .. }) => assert_eq!(page, 4),
        other => panic!("expected an Err frame, got {other:?}"),
    }
}

/// A write for a page beyond the file's length must also come back as an
/// `Err` frame rather than silently growing the file — the write-side twin
/// of the read out-of-range test above, since the two share `out_of_range`
/// but that sharing is exactly what "shares code with a tested path" hides
/// bugs in if only one side is ever actually run.
#[test]
fn serve_reports_a_write_past_the_end_of_the_image_as_an_error_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guest.img");
    let mut img =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    let len = 4 * PAGE as u64;
    img.set_len(len).unwrap();

    let mut req = Vec::new();
    encode(&Frame::WriteReq { page: 4, data: Box::new([0xABu8; PAGE]) }, &mut req); // one page past the end

    let mut m = Mux::new();
    let mut out = Vec::new();
    rv64_host::serve::serve_once(&mut img, len, &mut m, &req, &mut out, &mut Vec::new()).unwrap();

    let mut resp = Mux::new();
    resp.push(&out);
    match resp.take_matching(0x07) {
        Some(Frame::Err { page, .. }) => assert_eq!(page, 4),
        other => panic!("expected an Err frame, got {other:?}"),
    }
    // And the file must not have grown to accommodate the rejected write.
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
}

/// A `Read` that hands back at most `cap` bytes per call, regardless of how
/// much of the caller's buffer is free — standing in for a real serial link,
/// where a single `read()` cannot return more than the transport's own
/// packet size. Used below to force a frame to span multiple `read()` calls
/// deterministically, rather than relying on `serve`'s internal chunk size
/// (currently 4096, but not part of its contract) to happen to split it.
struct ChunkedReader {
    data: Vec<u8>,
    pos: usize,
    cap: usize,
}

impl Read for ChunkedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.data.len() - self.pos;
        if remaining == 0 {
            return Ok(0);
        }
        let n = remaining.min(self.cap).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Ruling R10's regression test: `serve` must own one `Mux` across its whole
/// read loop, not build a fresh one per `serve_once` call. A `WriteReq`
/// frame is `HEADER(5) + page(4) + PAGE(4096) + TRAILER(4)` = 4109 bytes,
/// which cannot arrive in a single read from a `ChunkedReader` capped well
/// below that. Before the fix, the first call's `Mux` buffered the prefix
/// and discarded it on return; the tail arrived next call with nothing to
/// complete against, so the write was never acknowledged and never landed —
/// silently, on every write, not as a flaky-transport edge case.
#[test]
fn serve_assembles_a_write_req_split_across_multiple_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guest.img");
    let mut img =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    img.set_len(64 * PAGE as u64).unwrap();

    let mut page = [0u8; PAGE];
    page[0..4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    let mut wire = Vec::new();
    encode(&Frame::WriteReq { page: 5, data: Box::new(page) }, &mut wire);
    assert!(wire.len() > 1024, "fixture must be big enough that a small cap actually splits it");

    // 1024-byte reads: a 4109-byte frame takes 5 of them to arrive.
    let rx = ChunkedReader { data: wire, pos: 0, cap: 1024 };
    let mut tx = Vec::new();

    rv64_host::serve::serve(&mut img, rx, &mut tx, None, &mut Vec::new()).unwrap();

    let mut resp = Mux::new();
    resp.push(&tx);
    match resp.take_matching(0x04) {
        Some(Frame::WriteAck { page }) => assert_eq!(page, 5),
        other => panic!("expected a WriteAck, got {other:?}"),
    }

    // The page must actually have landed in the file, not just been
    // acknowledged.
    let mut check = std::fs::File::open(&path).unwrap();
    check.seek(SeekFrom::Start(5 * PAGE as u64)).unwrap();
    let mut written = [0u8; PAGE];
    check.read_exact(&mut written).unwrap();
    assert_eq!(&written[0..4], &[0x11, 0x22, 0x33, 0x44]);
}

/// **The guest's console and the unframed bytes must not mix.**
///
/// `serve` produces two streams: guest console output from `ConOut` frames, and
/// every byte that was not a frame at all (the badge's log mirror, and anything
/// damaged in transit). They go to different places on purpose — but a
/// transcript that merges them, which `serve-wait.sh` did with `2>&1`, cannot
/// answer "did the guest print this, or did it arrive unframed?".
///
/// That question came up twice: once for the panic mirror, and once for a large
/// amount of raw device-tree content in a transcript, where the answer decides
/// whether the emulator is emitting something it should not or the wire is
/// carrying damaged frames. This pins the property the answer depends on: the
/// console sink receives the `ConOut` payload and **nothing else**.
#[test]
fn guest_console_and_unframed_bytes_go_to_different_places() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem.img");
    let mut img = OpenOptions::new().read(true).write(true).create(true).open(&path).unwrap();
    let len = (4 * PAGE) as u64;
    img.set_len(len).unwrap();

    // A ConOut frame with unframed text either side of it, which is exactly
    // what the badge's shared CDC endpoint produces.
    let mut wire = Vec::new();
    wire.extend_from_slice(b"INFO:some_other_process: not the guest\n");
    encode(&Frame::ConOut(b"guest says hello".to_vec()), &mut wire);
    wire.extend_from_slice(b"#address-cells\0compatible\0model\0bootargs\0");

    let mut m = Mux::capturing_noise();
    let mut out = Vec::new();
    let mut console = Vec::new();
    rv64_host::serve::serve_once(&mut img, len, &mut m, &wire, &mut out, &mut console).unwrap();

    assert_eq!(
        console, b"guest says hello",
        "the console stream must carry the ConOut payload and nothing else"
    );
    // The unframed bytes went to stderr inside `serve_once`, so they are not
    // re-takeable here — which is itself the separation being asserted. What is
    // checkable, and what the whole question turns on, is that **none** of them
    // reached the console stream.
    let text = String::from_utf8_lossy(&console).into_owned();
    for stray in ["not the guest", "#address-cells", "compatible", "bootargs"] {
        assert!(
            !text.contains(stray),
            "unframed bytes leaked into the guest console stream ({stray:?}): {text:?}"
        );
    }
}
