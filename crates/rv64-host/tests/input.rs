//! The host half of console input: `serve --input`'s keyboard reader.
//!
//! # What this covers that nothing else does
//!
//! `badge/app/tests/dry_run.rs` already proves the *badge* half end to end —
//! it injects a `ConIn` frame onto the socket by hand and asserts the guest
//! echoes `BADGE-INPUT-42` back. What it deliberately does not exercise is the
//! host: until now `rv64-host serve` never sent a `ConIn` frame at all, so the
//! dry run had to forge one. These tests are the other end of that chain —
//! bytes in at the keyboard, `ConIn` frames out on the wire — and together with
//! the dry run they make the path complete rather than merely plausible.
//!
//! The three properties that matter, in the order they can hurt:
//!
//! 1. **Typed bytes become `ConIn` frames, unaltered.** The link carries binary
//!    page data; a keyboard reader that mangled its own payload would be
//!    indistinguishable from the tty bugs `rawtty.rs` exists to prevent.
//! 2. **Idle is silent.** `rv64_proto::encode` emits a `ConIn` frame for an
//!    empty payload on purpose, so it is one careless `if` away from a reader
//!    that drips a frame onto the wire every time it finds nothing. Unframed
//!    and unnecessary traffic on this link has already cost days.
//! 3. **A keystroke never lands inside a page reply.** Two threads share one
//!    descriptor. This is the failure that would look like a badge fault.

use rv64_host::serve::{pump_input, FrameTx, InputEnd, Pace, CDC_PACKET, ESCAPE, QUIT};
use rv64_proto::{encode, Frame, Mux, PAGE};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A writer that keeps every byte, shareable across the two threads that write
/// to the real port.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Recorder {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

/// Runs the pump over a fixed script of keystrokes and hands back everything it
/// put on the wire.
fn pump(keys: &[u8]) -> (Vec<u8>, InputEnd) {
    let rec = Recorder::default();
    let tx = FrameTx::new(rec.clone());
    let end = pump_input(&mut io::Cursor::new(keys.to_vec()), &tx, None).expect("pump");
    (rec.bytes(), end)
}

/// Decodes a wire capture the way the badge's `Link` does, and returns the
/// console bytes it would have handed to `Uart::push_input`.
fn console_of(wire: &[u8]) -> Vec<u8> {
    let mut m = Mux::new();
    m.push(wire);
    m.take_console()
}

/// The whole point, in one assertion: what the operator types is what the guest
/// receives.
///
/// The payload is the dry run's own `TYPED` string, so this test and the badge
/// half are driving the guest with the same bytes rather than with two
/// different guesses about what a shell wants.
#[test]
fn typed_bytes_reach_the_wire_as_conin_frames() {
    let typed = b"echo BADGE-INPUT-$((6*7))\n";
    let (wire, end) = pump(typed);

    assert_eq!(end, InputEnd::Eof, "the script ran out, which is an EOF");
    assert_eq!(console_of(&wire), typed, "the guest must receive exactly what was typed");

    // And they are `ConIn` (0x06), not `ConOut`: the badge's `Mux` merges the
    // two into one console stream, so decoding alone cannot tell them apart and
    // a wrong type byte would sail through the assertion above.
    let mut m = Mux::new();
    m.push(&wire);
    assert!(
        wire.windows(3).any(|w| w[2] == 0x06),
        "no frame on the wire has the ConIn type byte: {wire:02x?}"
    );
    assert_eq!(m.take_matching(0x05), None, "nothing may be emitted as ConOut");
}

/// The rule the brief singles out: no empty frames.
///
/// Three ways to have nothing to say, all of which must produce **zero bytes**:
/// a reader that is already at EOF, a reader that returns `Ok(0)` on its first
/// call, and — the one a naive implementation gets wrong — a read that yielded
/// bytes which all turned out to be protocol, leaving no payload.
#[test]
fn nothing_is_sent_when_there_is_nothing_to_say() {
    let (wire, end) = pump(b"");
    assert_eq!(end, InputEnd::Eof);
    assert!(wire.is_empty(), "an idle keyboard put {} bytes on the wire", wire.len());

    // A lone escape prefix: real bytes were read, and still nothing may go out
    // — the escape is consumed and the pump goes back to waiting, holding one
    // bit of state rather than emitting an empty frame.
    let (wire, _) = pump(&[ESCAPE]);
    assert!(wire.is_empty(), "a dangling Ctrl-] emitted {} bytes", wire.len());

    // A bare Ctrl-C: it ends the run and it is not itself a payload.
    let (wire, end) = pump(&[QUIT]);
    assert_eq!(end, InputEnd::Quit);
    assert!(wire.is_empty(), "Ctrl-C emitted {} bytes", wire.len());
}

/// Ctrl-C ends `serve` and does not reach the guest — but whatever was typed
/// before it still does, so a half-typed line is not silently eaten on the way
/// out.
#[test]
fn ctrl_c_stops_the_pump_and_is_not_forwarded() {
    let mut keys = b"ls -l".to_vec();
    keys.push(QUIT);
    keys.extend_from_slice(b"never read");

    let (wire, end) = pump(&keys);
    assert_eq!(end, InputEnd::Quit);
    let got = console_of(&wire);
    assert_eq!(got, b"ls -l", "expected the prefix and nothing else, got {got:02x?}");
    assert!(!got.contains(&QUIT), "Ctrl-C must never cross the wire unescaped");
}

/// And the escape that makes Ctrl-C reachable anyway. Without this, a guest
/// process could never be interrupted from the badge's shell, which is a real
/// hole rather than a theoretical one — `Ctrl-C` is how you stop a `cat` with
/// no argument.
#[test]
fn the_escape_sends_a_literal_ctrl_c_through() {
    let (wire, end) = pump(&[ESCAPE, QUIT]);
    assert_eq!(end, InputEnd::Eof, "an escaped Ctrl-C is a keystroke, not a quit");
    assert_eq!(console_of(&wire), vec![QUIT]);

    // The escape is also how you send the escape.
    let (wire, _) = pump(&[ESCAPE, ESCAPE]);
    assert_eq!(console_of(&wire), vec![ESCAPE]);

    // And it does not swallow the byte after the one it escaped.
    let (wire, _) = pump(&[b'a', ESCAPE, QUIT, b'b']);
    assert_eq!(console_of(&wire), vec![b'a', QUIT, b'b']);
}

/// A paste, rather than a keystroke: more bytes than one read returns and more
/// than one frame's worth. `rv64_proto::encode` chunks `ConIn` at `MAX_PAYLOAD`
/// and the pump reads in bounded bites, so this crosses as several frames — and
/// what must survive is the concatenation, in order, byte for byte.
#[test]
fn a_paste_arrives_in_order_across_several_frames() {
    let pasted: Vec<u8> = (0..5000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let (wire, end) = pump(&pasted);
    assert_eq!(end, InputEnd::Eof);
    assert_eq!(console_of(&wire), pasted, "a paste must not be reordered or dropped");
    assert!(wire.len() > pasted.len(), "the capture should be framed, not raw bytes");
}

/// The concurrency property, which is the one that would present as a hardware
/// fault.
///
/// The page loop and the keyboard write to the same descriptor. If a `ConIn`
/// frame can land inside a paced `ReadResp`, the badge sees a CRC failure, drops
/// a page it is blocked on, and dumps four kilobytes of binary into the
/// transcript — the exact signature this project spent §17–§23 chasing for a
/// different reason. `FrameTx` holds its lock for a whole reply, gaps included;
/// this asserts that it does, by decoding the interleaved capture and requiring
/// **every** frame to have survived.
#[test]
fn a_keystroke_never_lands_inside_a_paced_page_reply() {
    const REPLIES: usize = 20;
    const KEYSTROKES: usize = 200;

    let rec = Recorder::default();
    let tx = Arc::new(FrameTx::new(rec.clone()));

    // Paced, because that is the case with a window to slip into: an unpaced
    // reply is one `write_all`, while a paced one is nine writes with sleeps
    // between them, all of which must be inside the lock.
    let pace = Pace { chunk: CDC_PACKET, gap: Duration::from_micros(100) };

    let pages = Arc::clone(&tx);
    let replies = std::thread::spawn(move || {
        for page in 0..REPLIES as u32 {
            let mut data = Box::new([0u8; PAGE]);
            for (i, b) in data.iter_mut().enumerate() {
                *b = (i.wrapping_add(page as usize) % 251) as u8;
            }
            let mut out = Vec::new();
            encode(&Frame::ReadResp { page, data }, &mut out);
            pages.send(&out, Some(pace)).expect("reply");
        }
    });

    // One frame per keystroke, which is what a human produces.
    let keys: Vec<u8> = (0..KEYSTROKES).map(|i| b'a' + (i % 26) as u8).collect();
    let mut src = SlowKeys(keys.clone(), 0);
    pump_input(&mut src, &tx, None).expect("pump");
    replies.join().expect("reply thread");

    let wire = rec.bytes();
    let mut m = Mux::capturing_noise();
    m.push(&wire);

    let mut seen = 0;
    while let Some(f) = m.take_matching(0x02) {
        match f {
            Frame::ReadResp { page, data } => {
                for (i, b) in data.iter().enumerate() {
                    assert_eq!(
                        *b,
                        (i.wrapping_add(page as usize) % 251) as u8,
                        "page {page} byte {i} was corrupted by an interleaved write"
                    );
                }
                seen += 1;
            }
            other => panic!("take_matching(0x02) yielded {other:?}"),
        }
    }
    assert_eq!(seen, REPLIES, "a page reply was destroyed by an interleaved ConIn frame");
    assert_eq!(m.take_console(), keys, "a keystroke was destroyed by an interleaved reply");

    let (noise, dropped) = m.take_noise();
    assert!(
        noise.is_empty() && dropped == 0,
        "{} bytes of the capture were not a frame, which means one was cut in half",
        noise.len() + dropped
    );
}

/// A reader that hands over one byte at a time, with a pause between them.
///
/// Both halves are load-bearing. One byte at a time makes the pump emit one
/// frame per keystroke, so the test gets two hundred chances to interleave
/// rather than one. The pause is what makes them *concurrent*: without it the
/// pump drains an in-memory buffer in microseconds and is finished before the
/// reply thread has written its second packet, and the test passes against a
/// `FrameTx` that locks per write instead of per frame — verified, 2026-09-01,
/// by making exactly that change and watching it stay green. With the pause the
/// two threads run for comparable spans and the mutation fails.
struct SlowKeys(Vec<u8>, usize);

/// Roughly the paced reply's own inter-packet gap, so the keyboard is producing
/// frames throughout the window a reply is being written in.
const KEY_GAP: Duration = Duration::from_micros(100);

impl Read for SlowKeys {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.1 >= self.0.len() || buf.is_empty() {
            return Ok(0);
        }
        std::thread::sleep(KEY_GAP);
        buf[0] = self.0[self.1];
        self.1 += 1;
        Ok(1)
    }
}

// ---------------------------------------------------------------------------
// The property the twenty-fourth hardware run broke: request N gets answer N
// ---------------------------------------------------------------------------

/// **The regression for the twenty-fourth run**, and the one property
/// `a_keystroke_never_lands_inside_a_paced_page_reply` above cannot see.
///
/// That test proves no frame is *destroyed* by the two writers sharing the
/// port. It says nothing about pairing, because it never asks a question: it
/// pushes twenty replies at a recorder and counts twenty frames back. The
/// failure on hardware was not a destroyed frame — every frame decoded, the CRC
/// was fine, `retries=0` — it was that the badge asked for page 1514 and the
/// first `ReadResp` waiting for it was page 1513's. A stream one frame ahead
/// passes every integrity check there is.
///
/// So this drives the **real** `serve_shared` over a real socket, with the real
/// keyboard pump typing into the same `FrameTx` throughout, and asserts the
/// thing that actually matters: for every request, the next response is that
/// request's. If `--input` could shift the stream by one — by writing outside
/// the lock, by dripping an idle frame, by a reply escaping twice — this is
/// where it would show, and it is the assertion the hardware transcript failed.
#[test]
fn a_concurrent_keyboard_never_shifts_the_response_stream() {
    use std::net::{TcpListener, TcpStream};

    const PAGES: u32 = 48;
    const KEYSTROKES: usize = 400;

    // A guest image whose every page says which page it is, so a response
    // carrying the wrong page's *bytes* is caught even if its header were right.
    let path = std::env::temp_dir().join(format!(
        "rv64-input-pairing-{}-{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    {
        let mut f = std::fs::File::create(&path).expect("create image");
        for page in 0..PAGES {
            f.write_all(&[fill_for(page); PAGE]).expect("write page");
        }
        f.flush().expect("flush");
    }
    let _cleanup = Cleanup(path.clone());
    let mut img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("reopen image");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let badge = TcpStream::connect(addr).expect("connect");
    let (host, _) = listener.accept().expect("accept");
    host.set_nodelay(true).expect("nodelay");
    badge.set_nodelay(true).expect("nodelay");
    badge
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout: a stalled test must fail, not hang");

    let host_rx = host.try_clone().expect("clone");
    let tx = Arc::new(FrameTx::new(host));

    // Paced, for the same reason as the test above: a paced reply is nine
    // writes with gaps, which is nine times the window for the keyboard to
    // land in the middle of one.
    let pace = Pace { chunk: CDC_PACKET, gap: Duration::from_micros(100) };

    let serve_tx = Arc::clone(&tx);
    let server = std::thread::spawn(move || {
        rv64_host::serve::serve_shared(&mut img, host_rx, &serve_tx, Some(pace), &mut Vec::new())
    });

    // The keyboard, typing throughout — the operator at the shell, which is the
    // configuration that failed.
    let keys: Vec<u8> = (0..KEYSTROKES).map(|i| b'a' + (i % 26) as u8).collect();
    let keyboard_tx = Arc::clone(&tx);
    let typed = keys.clone();
    let keyboard = std::thread::spawn(move || {
        pump_input(&mut SlowKeys(typed, 0), &keyboard_tx, None).expect("pump")
    });

    // The badge: strictly synchronous, one request outstanding at a time, which
    // is the doctrine `usbhost::LinkInner` is built on.
    let mut req = badge.try_clone().expect("clone");
    let mut rx = badge.try_clone().expect("clone");
    let mut m = Mux::capturing_noise();
    let mut console = Vec::new();
    let mut buf = [0u8; 4096];

    for page in 0..PAGES {
        let mut wire = Vec::new();
        encode(&Frame::ReadReq { page }, &mut wire);
        req.write_all(&wire).expect("request");
        req.flush().expect("flush request");

        let answer = loop {
            if let Some(f) = m.take_matching(0x02) {
                break f;
            }
            console.extend(m.take_console());
            let n = rx.read(&mut buf).expect("read");
            assert_ne!(n, 0, "the host closed the socket with page {page} unanswered");
            m.push(&buf[..n]);
        };
        console.extend(m.take_console());

        match answer {
            Frame::ReadResp { page: got, data } => {
                assert_eq!(
                    got, page,
                    "THE STREAM SHIFTED: asked for page {page}, the next response was \
                     page {got}'s. This is the twenty-fourth hardware run."
                );
                assert!(
                    data.iter().all(|&b| b == fill_for(page)),
                    "page {page} came back carrying another page's bytes"
                );
            }
            other => panic!("take_matching(0x02) yielded {other:?}"),
        }
    }

    // Every keystroke reached the badge, in order and unmangled: the link was
    // carrying both directions at once, not quiet on one of them.
    let end = keyboard.join().expect("keyboard thread");
    assert_eq!(end, InputEnd::Eof);
    badge.shutdown(std::net::Shutdown::Write).expect("shutdown");
    server.join().expect("server thread").expect("serve");
    // The last reference to the host's end of the socket. Without this the
    // drain below waits out its own read timeout, because `serve_shared`
    // returning does not close a descriptor this thread still holds.
    drop(tx);
    loop {
        let n = rx.read(&mut buf).expect("drain");
        if n == 0 {
            break;
        }
        m.push(&buf[..n]);
        console.extend(m.take_console());
    }
    console.extend(m.take_console());
    assert_eq!(console, keys, "a keystroke was lost or reordered on the way to the badge");

    let (noise, dropped) = m.take_noise();
    assert!(
        noise.is_empty() && dropped == 0,
        "{} bytes on the wire were not a frame, so one was cut in half",
        noise.len() + dropped
    );
}

/// Distinct, non-zero, and different from the page number itself, so neither a
/// zeroed buffer nor an off-by-one in the header can pass for the right bytes.
fn fill_for(page: u32) -> u8 {
    (page as u8).wrapping_mul(11).wrapping_add(0x5b)
}

/// Removes the scratch image however the test ends.
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}
