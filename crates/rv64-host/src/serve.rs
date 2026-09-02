//! The laptop side of the badge link: answers page requests from a flat
//! guest-memory file, and echoes console output to stdout.
//!
//! This operates on a plain file, not on [`crate::HostFile`] /
//! `rv64::cache::PageCache`. Those exist to serve the *emulator core*'s
//! `Bus`, which needs a `MemBacking` it can page in and out of a bounded
//! resident set — machinery this module has no use for, since it does not
//! run guest code at all. It just answers `page * 4096` seeks against
//! whatever `load_boot_images` already wrote to disk during the load phase
//! (see `main.rs`'s `serve` subcommand), so a generic `Read + Write + Seek`
//! is both sufficient and all that is available: `PageCache`'s backing
//! store is private and `Bus` exposes no way to recover it, and reopening
//! the image through `HostFile::new` would truncate the very bytes the load
//! phase just wrote.
use rv64_proto::{encode, Frame, Mux, PAGE, SYNC};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::time::Duration;

/// One CDC bulk packet. Replies are paced in these because the packet is the
/// unit the badge's USB interrupt handler services.
pub const CDC_PACKET: usize = 512;

/// Write replies in [`CDC_PACKET`] chunks with a gap between them.
///
/// # What this is an instrument for
///
/// `usb-bao1x`'s receive path takes **one packet out of the hardware per
/// `UsbDevice::poll()`**, and one interrupt can cover several arrivals — a
/// 4109-byte reply is nine packets and they do not get nine interrupts. The
/// suspicion is that the badge simply cannot keep up with back-to-back packets
/// and the surplus is lost or overwritten.
///
/// Pacing tests that from the host, with no firmware rebuild: if a reply
/// delivered one packet at a time arrives intact, the mechanism is confirmed
/// and a firmware fix has a specification. If it still arrives stale, the
/// overrun theory is wrong and an image rebuild would have been wasted.
///
/// **Off by default.** It is a diagnostic, not a transport policy: a
/// millisecond per packet is ~9 ms per page against a 2 ms round trip, which
/// would quadruple a boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    pub chunk: usize,
    pub gap: Duration,
}

/// Writes `out` to `tx`, optionally paced.
///
/// Each chunk is flushed before the gap, since a buffered writer would
/// otherwise coalesce the chunks back into one burst and measure nothing.
fn write_reply<W: Write>(tx: &mut W, out: &[u8], pace: Option<Pace>) -> io::Result<()> {
    match pace {
        None => {
            tx.write_all(out)?;
            tx.flush()
        }
        Some(p) => {
            for chunk in out.chunks(p.chunk.max(1)) {
                tx.write_all(chunk)?;
                tx.flush()?;
                if !p.gap.is_zero() {
                    std::thread::sleep(p.gap);
                }
            }
            Ok(())
        }
    }
}

/// A one-line diagnosis of a run of bytes the decoder threw away.
///
/// # Why the raw dump was not enough
///
/// `serve_once` puts every not-a-frame byte on stderr verbatim, and that is
/// what made the last two findings possible — but it means a rejected 4109-byte
/// page frame arrives in the transcript as four kilobytes of binary, which is
/// indistinguishable at a glance from a panic mirror, a log line, or noise. It
/// took a laptop measurement to establish that the device-tree bytes in one
/// transcript were the badge's **first `WriteReq`**, carrying the DTB's own
/// page, rejected on CRC. That question should be answerable by reading.
///
/// So: if the discarded bytes contain something frame-shaped, say what it was.
/// The dump still follows, unchanged — this only puts a sentence in front of
/// it.
///
/// # The block comparison
///
/// For a page-sized payload the line also reports whether any two 512-byte
/// blocks are byte-identical. That is not a general-purpose statistic: it is
/// the exact check that identified the *receive*-side defect (one shared
/// hardware packet buffer, reused before the previous packet had been consumed,
/// producing a payload that repeats with the buffer's period), and the
/// transmit side has the same shape — `CorigineWrapper::write` copies every IN
/// packet into one 512-byte buffer with no completion check. If a rejected
/// badge→host page frame shows repeating 512-byte blocks, that is the
/// mechanism, named from the transcript instead of from a rebuild.
fn describe_reject(noise: &[u8]) -> Option<String> {
    let sync = SYNC.to_le_bytes();
    let at = noise.windows(2).position(|w| w == sync)?;
    let rest = &noise[at..];
    if rest.len() < 5 {
        return None;
    }
    let ty = rest[2];
    let len = u16::from_le_bytes([rest[3], rest[4]]) as usize;
    let name = match ty {
        0x01 => "ReadReq",
        0x02 => "ReadResp",
        0x03 => "WriteReq",
        0x04 => "WriteAck",
        0x05 => "ConOut",
        0x06 => "ConIn",
        0x07 => "Err",
        _ => return None,
    };
    let body = &rest[5.min(rest.len())..];
    // The page number is the first four payload bytes of every frame that has
    // one, and it is the single most useful field in this line: it says which
    // page of guest memory the badge was talking about.
    // `Err` alone puts its two-byte code first (see `rv64_proto::encode`), so
    // its page is not where every other frame's is.
    let page_at = match ty {
        0x01 | 0x02 | 0x03 | 0x04 => Some(0),
        0x07 => Some(2),
        _ => None,
    };
    let page = page_at
        .filter(|o| body.len() >= o + 4)
        .map(|o| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]));
    let mut s = format!(
        "[rv64-host: discarded {} bytes; frame-shaped at offset {at}: {name}, declared len {len}",
        noise.len()
    );
    if let Some(p) = page {
        s.push_str(&format!(", page {p}"));
    }
    // Only for a payload big enough to be a page: two 512-byte blocks of a
    // 13-byte request cannot say anything.
    let data = body.get(4..).unwrap_or(&[]);
    if data.len() >= 2 * CDC_PACKET {
        let blocks: Vec<&[u8]> = data.chunks(CDC_PACKET).collect();
        let mut dupes = Vec::new();
        for i in 0..blocks.len() {
            for j in (i + 1)..blocks.len() {
                if blocks[i].len() == blocks[j].len() && blocks[i] == blocks[j] {
                    dupes.push(format!("{i}={j}"));
                }
            }
        }
        s.push_str(&format!(
            "; {} full {CDC_PACKET}-byte blocks, {}",
            data.len() / CDC_PACKET,
            if dupes.is_empty() {
                "all distinct".to_string()
            } else {
                format!("REPEATED: {}", dupes.join(" "))
            }
        ));
    }
    s.push(']');
    Some(s)
}

/// The one place bytes are put on the wire, so that two threads can put them
/// there without landing inside each other's frames.
///
/// # Why a lock and not two file descriptors
///
/// With `--input` there are two writers: the page loop answering `ReadReq`s,
/// and the keyboard reader emitting `ConIn`. `try_clone()` would give each its
/// own descriptor, but a `dup` of a tty is the same tty — two `write_all`s can
/// still interleave, since neither is atomic against a device whose buffer they
/// may fill. A `ConIn` frame landing four bytes into a `ReadResp` produces
/// exactly the failure this project has spent days on: a CRC rejection, a page
/// the badge never gets, and a wall of unframed bytes in the transcript.
///
/// So the lock is held for a whole **reply**, not a whole write — including a
/// paced reply's inter-packet gaps. Pacing exists to keep one packet in flight;
/// a keystroke slipped into a gap would be a tenth packet in the middle of a
/// nine-packet page, which is the thing being avoided.
///
/// The cost is nil: a keystroke waits at most one reply, and the operator types
/// at human speed.
#[derive(Debug)]
pub struct FrameTx<W> {
    inner: Mutex<W>,
}

impl<W: Write> FrameTx<W> {
    pub fn new(w: W) -> Self {
        FrameTx { inner: Mutex::new(w) }
    }

    /// Writes one complete run of frames, indivisibly.
    ///
    /// A poisoned lock means the other writer panicked mid-frame. There is no
    /// recovery from that — the stream is already cut in half — so it is
    /// reported as an error rather than papered over with `into_inner`.
    pub fn send(&self, bytes: &[u8], pace: Option<Pace>) -> io::Result<()> {
        let mut w = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("the link writer was poisoned by a panic mid-frame"))?;
        write_reply(&mut *w, bytes, pace)
    }

    /// Recovers the wrapped writer. Used by tests that need to inspect what was
    /// written.
    pub fn into_inner(self) -> W {
        self.inner.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// `Ctrl-C`. Ends `serve`; never reaches the guest unless escaped.
///
/// It has to be handled here rather than by `ISIG` and a signal: `serve
/// --input` has the operator's terminal in raw mode, and a `SIGINT` that kills
/// the process runs no destructor and no `Restore`, leaving a shell with no
/// echo and no line editing. See `crate::rawtty::Restore`.
pub const QUIT: u8 = 0x03;

/// `Ctrl-]`, telnet's escape: the **next** byte is sent to the guest verbatim,
/// whatever it is.
///
/// This is what keeps `QUIT` from being a permanent hole in the link. `Ctrl-]
/// Ctrl-C` sends a real `0x03` and interrupts the guest's foreground process;
/// `Ctrl-] Ctrl-]` sends a literal `0x1d`; `Ctrl-] Enter` sends whatever Enter
/// produced. One byte of state, and it means no key is unreachable.
pub const ESCAPE: u8 = 0x1d;

/// How the keyboard stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEnd {
    /// The operator typed [`QUIT`]. Put the terminal back and stop.
    Quit,
    /// stdin closed — a script's input ran out, or the terminal went away.
    Eof,
}

/// Reads `src` and puts what it finds on the wire as [`Frame::ConIn`].
///
/// # The one rule this has to obey
///
/// **Never send an empty frame.** A `ConIn` with no payload is a legal frame
/// (`rv64_proto::encode` emits one for an empty `Vec` on purpose), and a poll
/// loop that emitted one every time it found no keystroke would put a steady
/// drip of frames on a link whose spare capacity has already been the difference
/// between a boot working and not. So this only encodes when it holds bytes: a
/// `read` returning zero is EOF and returns, and a read that yields nothing but
/// an [`ESCAPE`] prefix goes round again holding the state, not the wire.
///
/// # Blocking is the point
///
/// This is written to be run on its own thread with a plain blocking `read`.
/// The alternative — polling stdin non-blocking from the serve loop — is worse
/// twice over: it needs `O_NONBLOCK` on fd 0, which is a property of the open
/// file description the operator's shell shares, and famously survives `serve`
/// to break the next program that reads it; and it would interleave a keyboard
/// poll with page service, which is the one thing that must never stall. The
/// badge's deadline is 2000 ms and the guest is stopped for every millisecond a
/// request goes unanswered. A thread costs a stack and touches the page loop
/// only through [`FrameTx`]'s lock, held for the length of one frame.
///
/// `pace` is honoured for the same reason replies honour it: with `--pace-ms`
/// set, the badge is known not to tolerate back-to-back packets, and a
/// keystroke arriving immediately behind the last packet of a reply is exactly
/// that. The gap runs before the frame, so it is a millisecond of a human's
/// typing latency and nothing else.
pub fn pump_input<R: Read, W: Write>(
    src: &mut R,
    tx: &FrameTx<W>,
    pace: Option<Pace>,
) -> io::Result<InputEnd> {
    let mut buf = [0u8; 256];
    let mut payload: Vec<u8> = Vec::with_capacity(buf.len());
    let mut frame: Vec<u8> = Vec::new();
    let mut escaped = false;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(InputEnd::Eof);
        }
        payload.clear();
        let mut quit = false;
        for &b in &buf[..n] {
            if escaped {
                payload.push(b);
                escaped = false;
            } else if b == ESCAPE {
                escaped = true;
            } else if b == QUIT {
                quit = true;
                break;
            } else {
                payload.push(b);
            }
        }
        // Whatever was typed *before* the Ctrl-C still goes, so a half-typed
        // line is not silently swallowed on the way out.
        if !payload.is_empty() {
            if let Some(p) = pace {
                if !p.gap.is_zero() {
                    std::thread::sleep(p.gap);
                }
            }
            frame.clear();
            encode(&Frame::ConIn(payload.clone()), &mut frame);
            // Unpaced *chunking*: `buf` is 256 bytes, so the frame is at most
            // 265 and always fits in one CDC packet. There is nothing to split.
            tx.send(&frame, None)?;
        }
        if quit {
            return Ok(InputEnd::Quit);
        }
    }
}

fn out_of_range(start: u64, len: u64) -> io::Result<()> {
    if start + PAGE as u64 > len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "page out of range"));
    }
    Ok(())
}

fn read_page<F: Read + Seek>(img: &mut F, len: u64, page: u32, buf: &mut [u8; PAGE]) -> io::Result<()> {
    let start = page as u64 * PAGE as u64;
    out_of_range(start, len)?;
    img.seek(SeekFrom::Start(start))?;
    img.read_exact(buf)
}

fn write_page<F: Write + Seek>(img: &mut F, len: u64, page: u32, buf: &[u8; PAGE]) -> io::Result<()> {
    let start = page as u64 * PAGE as u64;
    out_of_range(start, len)?;
    img.seek(SeekFrom::Start(start))?;
    img.write_all(buf)
}

/// What one [`serve_once`] call answered, in the order it answered it.
///
/// # Why the caller is told
///
/// The twenty-fourth hardware run died with `response for page 1513 while
/// waiting for page 1514` — the badge's response stream one frame ahead of its
/// requests — and the badge's own counters ruled out every explanation on its
/// side: `retries=0` (nothing was re-sent) and `stale=0` (nothing was held at
/// the start of the exchange). Its `rx_bytes` was 3 993 948, which is exactly
/// 972 × 4109 against 971 cache misses: one whole surplus `ReadResp` on the
/// wire and not a byte of anything else — no `ConIn`, no noise.
///
/// One surplus answer means one surplus *question*, and only this end can see
/// that. `serve` writes each reply exactly once, so a second answer is a second
/// request — and whether that second request was on the wire is a fact the host
/// has and the badge does not. Recording what was answered is what lets
/// [`serve_shared`] say so out loud, instead of leaving the direction to be
/// argued from byte counts after the run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Served {
    /// `(type byte, page)` for each request answered, in order.
    pub answered: Vec<(u8, u32)>,
}

/// Watches a stream of answered requests for the same one arriving twice.
///
/// **A repeat is not automatically a defect.** The badge re-sends a request
/// whose attempt went unanswered, and a re-sent request is legitimately the
/// same request twice; the badge's own `retries` counter is what tells those
/// apart. What this gives a transcript is the half the badge cannot have:
/// whether a duplicate answer it had to absorb was born here, from a question
/// asked twice, or below its own send path.
#[derive(Debug, Default)]
pub struct Trail {
    last: Option<(u8, u32)>,
    dupes: usize,
    answered: usize,
}

impl Trail {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what was answered and returns one line per repeat.
    ///
    /// Only an *immediately* repeated request counts. That is the shape a
    /// duplicated transmit produces — the badge asks 1513, the wire delivers it
    /// twice, and 1514 follows — and it is the shape that cannot be confused
    /// with the ordinary business of a page being read again later in a boot.
    pub fn note(&mut self, served: &Served) -> Vec<String> {
        let mut lines = Vec::new();
        for &req in &served.answered {
            self.answered += 1;
            if self.last == Some(req) {
                self.dupes += 1;
                let (ty, page) = req;
                let name = if ty == 0x01 { "ReadReq" } else { "WriteReq" };
                lines.push(format!(
                    "[rv64-host: DUPLICATE REQUEST: {name} for page {page} arrived twice in \
                     a row; both were answered ({} duplicate(s) in {} request(s) so far). \
                     The extra answer is on its way back to the badge, which has to absorb \
                     it. If the badge reports retries=0 for this run, the duplication \
                     happened below its send path rather than in it.]",
                    self.dupes, self.answered
                ));
            }
            self.last = Some(req);
        }
        lines
    }

    /// Requests answered, and how many of them were an immediate repeat.
    pub fn tally(&self) -> (usize, usize) {
        (self.answered, self.dupes)
    }
}

/// Services every frame `m` can assemble from `input`, writing replies to
/// `out`. Console bytes from the guest are echoed to stdout.
///
/// `m` is the caller's, not a fresh one built here — see [`serve`]'s doc
/// comment for why a `Mux` local to this function is a bug rather than a
/// simplification: a `WriteReq` frame is 4109 bytes
/// (`HEADER(5) + page(4) + PAGE(4096) + TRAILER(4)`), larger than any single
/// `read()` this crate feeds it, so a fresh `Mux` per call would discard the
/// prefix it buffered on every call but the last that completes a frame.
///
/// `len` is the image's length in bytes, passed in rather than queried here
/// — see [`serve`] for why.
/// `con` is where **guest console output** goes. It is a parameter rather than
/// hard-wired to stdout because the two streams this function produces have to
/// be separable: guest console on one, and bytes that were not a frame at all on
/// the other. A transcript that merges them (`2>&1`, which is what
/// `badge/serve-wait.sh` used to do) cannot answer "is this the guest talking or
/// is it unframed traffic?", and that question has now come up twice.
pub fn serve_once<F: Read + Write + Seek>(
    img: &mut F,
    len: u64,
    m: &mut Mux,
    input: &[u8],
    out: &mut Vec<u8>,
    con: &mut dyn Write,
) -> io::Result<Served> {
    m.push(input);

    let mut served = Served::default();
    while let Some(f) = m.take_matching(0x01).or_else(|| m.take_matching(0x03)) {
        match f {
            Frame::ReadReq { page } => {
                served.answered.push((0x01, page));
                let mut buf = Box::new([0u8; PAGE]);
                let reply = match read_page(img, len, page, &mut buf) {
                    Ok(()) => Frame::ReadResp { page, data: buf },
                    Err(_) => Frame::Err { code: 1, page },
                };
                encode(&reply, out);
            }
            Frame::WriteReq { page, data } => {
                served.answered.push((0x03, page));
                let reply = match write_page(img, len, page, &data) {
                    Ok(()) => Frame::WriteAck { page },
                    Err(_) => Frame::Err { code: 2, page },
                };
                encode(&reply, out);
            }
            _ => unreachable!("take_matching only yields the types we asked for"),
        }
    }

    let console = m.take_console();
    if !console.is_empty() {
        con.write_all(&console)?;
        con.flush()?;
    }

    // Anything that was not a frame goes to **stderr, verbatim**.
    //
    // This is the badge's USB panic mirror. The log server writes `PANIC in
    // PID n:` and the panic text out of the same CDC endpoint the protocol
    // uses, and before this the decoder scanned past it looking for SYNC and
    // dropped it on the floor. The first hardware failure therefore produced no
    // diagnosis at all until the server was killed and a plain reader attached
    // — every bit of panic visibility this project built, defeated by the thing
    // listening in front of it.
    //
    // Verbatim, and on stderr rather than stdout, so the two streams stay
    // separable: `rv64-host serve >guest.log 2>badge.log` gives the guest's
    // console and the badge's own diagnostics as two files. No prefix and no
    // reformatting — a transcript that has been "helpfully" line-wrapped is
    // worth less than the bytes.
    let (noise, dropped) = m.take_noise();
    if !noise.is_empty() {
        // A sentence in front of the bytes, when the bytes look like a frame.
        // See `describe_reject` — the dump itself is untouched.
        if let Some(d) = describe_reject(&noise) {
            writeln!(io::stderr(), "\n{d}")?;
        }
        io::stderr().write_all(&noise)?;
        io::stderr().flush()?;
    }
    if dropped > 0 {
        writeln!(io::stderr(), "\n[rv64-host: dropped {dropped} bytes of off-protocol text]")?;
    }
    Ok(served)
}

/// Runs `serve_once` in a loop over a real byte stream until it closes.
///
/// Owns one [`Mux`] for the whole connection, fed across every `read()`, and
/// queries `img`'s length exactly once up front rather than on every
/// request: the image is sized at load time by [`crate::HostFile::new`] and
/// never grows afterward, so a `seek(SeekFrom::End(0))` per page request
/// would be a syscall spent to learn a constant.
///
/// A local `Mux` per call — what an earlier version of this function did —
/// is a guaranteed bug, not an edge case: `rx.read` can return at most
/// `chunk.len()` (4096) bytes per call by the `Read` contract, but a
/// `WriteReq` frame is 4109 bytes, so *every* `WriteReq` spans at least two
/// reads. A `Mux` scoped to one call buffers the first read's prefix and
/// then discards it when that call returns, leaving the second read's tail
/// with nothing to complete against — the host never assembles the frame,
/// never replies, and the badge hangs waiting for a `WriteAck` that can
/// never arrive, with no diagnostic on either side. `rv64_proto`'s own
/// `Decoder` doc comment says exactly this: "the USB layer truncates at
/// 3840 bytes, so a 4 KiB frame always arrives in pieces. The decoder must
/// accumulate." — accumulation only works if the accumulator lives across
/// calls.
pub fn serve<R: Read, W: Write, F: Read + Write + Seek>(
    img: &mut F,
    rx: R,
    tx: W,
    pace: Option<Pace>,
    con: &mut dyn Write,
) -> io::Result<()> {
    serve_shared(img, rx, &FrameTx::new(tx), pace, con)
}

/// [`serve`], over a writer something else may also be writing to.
///
/// This is the same loop; the only difference is that the reply goes through a
/// [`FrameTx`] the caller still holds a reference to, so `serve --input` can
/// hand the same one to [`pump_input`] on another thread. `serve` is this with
/// a `FrameTx` nobody else can reach, which is why there is one loop and not
/// two.
pub fn serve_shared<R: Read, W: Write, F: Read + Write + Seek>(
    img: &mut F,
    mut rx: R,
    tx: &FrameTx<W>,
    pace: Option<Pace>,
    con: &mut dyn Write,
) -> io::Result<()> {
    let len = img.seek(SeekFrom::End(0))?;
    // Capturing, so `serve_once` can put the badge's panic mirror on stderr
    // instead of eating it. The badge's own `Mux` is deliberately not — see
    // `rv64_proto::Decoder::capturing_noise`.
    let mut m = Mux::capturing_noise();
    let mut chunk = [0u8; 4096];
    // See [`Trail`]: the one fact about a duplicated request that only this end
    // of the cable can establish.
    let mut trail = Trail::new();
    loop {
        let n = rx.read(&mut chunk)?;
        if n == 0 {
            let (answered, dupes) = trail.tally();
            // On the way out, unconditionally. A run that answered exactly as
            // many requests as the badge asked is a fact worth having in the
            // transcript next to the badge's own `late=` count, and it is
            // cheaper to print one line always than to explain its absence.
            writeln!(
                io::stderr(),
                "\n[rv64-host: answered {answered} request(s), {dupes} of them an immediate \
                 duplicate]"
            )?;
            return Ok(());
        }
        let mut out = Vec::new();
        let served = serve_once(img, len, &mut m, &chunk[..n], &mut out, con)?;
        for line in trail.note(&served) {
            writeln!(io::stderr(), "\n{line}")?;
        }
        if !out.is_empty() {
            tx.send(&out, pace)?;
        }
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;

    /// Records the size of every write, so a test can tell one burst from
    /// several packets.
    #[derive(Default)]
    struct Writes {
        bytes: Vec<u8>,
        sizes: Vec<usize>,
    }
    impl Write for Writes {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            self.sizes.push(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Unpaced is one write. That is the normal path and it must stay that way:
    /// pacing is a diagnostic and must cost nothing when it is off.
    #[test]
    fn without_pacing_a_reply_is_a_single_write() {
        let mut w = Writes::default();
        let payload = vec![0xabu8; 4109];
        write_reply(&mut w, &payload, None).unwrap();
        assert_eq!(w.sizes, vec![4109]);
        assert_eq!(w.bytes, payload);
    }

    /// Paced is one write per CDC packet, and — the part that matters — the
    /// bytes are unchanged. An instrument that altered the payload would answer
    /// a different question from the one being asked.
    #[test]
    fn pacing_splits_into_packets_without_changing_the_bytes() {
        let mut w = Writes::default();
        let payload: Vec<u8> = (0..4109u32).map(|i| (i % 251) as u8).collect();
        let pace = Pace { chunk: CDC_PACKET, gap: Duration::ZERO };
        write_reply(&mut w, &payload, Some(pace)).unwrap();

        // 4109 = 8 full packets plus a 13-byte tail.
        assert_eq!(w.sizes.len(), 9);
        assert!(w.sizes[..8].iter().all(|&n| n == CDC_PACKET));
        assert_eq!(w.sizes[8], 4109 - 8 * CDC_PACKET);
        assert_eq!(w.bytes, payload, "pacing must not alter a single byte");
    }

    /// The line that would have answered "where did the device-tree bytes in
    /// the transcript come from?" by reading, rather than by a laptop boot.
    #[test]
    fn a_rejected_page_frame_is_named_with_its_page() {
        let mut buf = Box::new([0u8; PAGE]);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut wire = Vec::new();
        encode(&Frame::WriteReq { page: 1275, data: buf }, &mut wire);
        // What the decoder hands back after rejecting it: the frame, verbatim.
        let d = describe_reject(&wire).expect("a frame-shaped run must be named");
        assert!(d.contains("WriteReq"), "{d}");
        assert!(d.contains("page 1275"), "{d}");
        assert!(d.contains("all distinct"), "{d}");
    }

    /// The discriminator this exists for: a payload whose 512-byte blocks
    /// repeat is one hardware packet buffer being reused before the previous
    /// packet left. Naming the pair is what makes the transcript able to say
    /// so.
    #[test]
    fn repeated_packet_blocks_are_called_out_by_index() {
        let mut buf = Box::new([0u8; PAGE]);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // Block 5 arrives as a copy of block 4 -- the shape a clobbered
        // transmit buffer produces.
        let (a, b) = buf.split_at_mut(5 * CDC_PACKET);
        b[..CDC_PACKET].copy_from_slice(&a[4 * CDC_PACKET..5 * CDC_PACKET]);
        let mut wire = Vec::new();
        encode(&Frame::WriteReq { page: 1275, data: buf }, &mut wire);
        let d = describe_reject(&wire).expect("named");
        assert!(d.contains("REPEATED: 4=5"), "{d}");
    }

    /// Ordinary text -- the panic mirror, a log line -- must not be dressed up
    /// as a frame. The verbatim dump is the whole value there, and a wrong
    /// sentence in front of it is worse than none.
    #[test]
    fn text_without_a_frame_header_is_not_described() {
        assert_eq!(describe_reject(b"PANIC in PID 7: oh no\n"), None);
        // A SYNC pair inside text, with a type byte that is not a frame type.
        let mut s = b"log: ".to_vec();
        s.extend_from_slice(&SYNC.to_le_bytes());
        s.extend_from_slice(&[0x77, 0x00, 0x00]);
        assert_eq!(describe_reject(&s), None);
    }

    /// The line the twenty-fourth run needed and nobody was printing: the same
    /// request answered twice in a row.
    #[test]
    fn a_request_asked_twice_in_a_row_is_named_with_its_page() {
        let mut t = Trail::new();
        assert!(t.note(&Served { answered: vec![(0x01, 1512), (0x01, 1513)] }).is_empty());
        // 1513 again, at the head of the next read -- the shape a duplicated
        // transmit produces.
        let lines = t.note(&Served { answered: vec![(0x01, 1513), (0x01, 1514)] });
        assert_eq!(lines.len(), 1, "one duplicate, one line: {lines:?}");
        assert!(lines[0].contains("DUPLICATE REQUEST"), "{}", lines[0]);
        assert!(lines[0].contains("ReadReq for page 1513"), "{}", lines[0]);
        assert_eq!(t.tally(), (4, 1));
    }

    /// The same page read again *later* is ordinary — an eviction and a re-read
    /// — and must not be dressed up as a defect. Only an immediate repeat is
    /// one, and a `WriteReq` for a page just read is not a repeat at all.
    #[test]
    fn a_page_answered_again_later_is_not_a_duplicate() {
        let mut t = Trail::new();
        let all = Served {
            answered: vec![(0x01, 7), (0x01, 8), (0x01, 7), (0x03, 7), (0x01, 7)],
        };
        assert!(t.note(&all).is_empty(), "no immediate repeat anywhere in that sequence");
        assert_eq!(t.tally(), (5, 0));
    }

    /// A zero chunk would be an infinite loop rather than an error, and the one
    /// place this is configured is a CLI flag.
    #[test]
    fn a_zero_chunk_does_not_hang() {
        let mut w = Writes::default();
        let pace = Pace { chunk: 0, gap: Duration::ZERO };
        write_reply(&mut w, &[1, 2, 3], Some(pace)).unwrap();
        assert_eq!(w.bytes, vec![1, 2, 3]);
    }
}
