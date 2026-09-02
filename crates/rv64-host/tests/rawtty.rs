//! The test that would have caught the missing `tcsetattr`.
//!
//! Every other test in this project drives the protocol over something that is
//! not a tty — `tests/serve.rs` uses byte buffers, `badge/app/tests/dry_run.rs`
//! uses a `TcpStream` — and `serve()`'s `Read + Write` generics are exactly what
//! makes a line discipline invisible from inside them. The host shipped for
//! several commits with no termios call anywhere in the crate, and nothing went
//! red.
//!
//! So this one opens a **real pty**, which is what the badge's `/dev/ttyACM0`
//! or `/dev/cu.usbmodem*` looks like to `serve`, and runs binary frames over it.
//! It has two halves and both are load-bearing:
//!
//! 1. **The hazard is real.** [`canonical_mode_really_does_mangle_a_page_frame`]
//!    sends page bytes over a pty left in its default state and shows they do
//!    not arrive intact. Without this, the fix could be a no-op against a
//!    problem that was never there.
//! 2. **The fix works.** [`raw_mode_carries_a_page_frame_byte_for_byte`] and
//!    [`the_real_serve_answers_a_page_request_over_a_pty`] send the same bytes
//!    through `rawtty::make_raw` and through the real `rv64_host::serve::serve`,
//!    and require them back byte for byte.
//!
//! **Verified by mutation, 2026-08-24.** Deleting the `make_raw` call from the
//! two `serve` tests — which is exactly the code that shipped — fails both, with
//! *"no reply: the request never reached `serve` intact"*. So this file does
//! catch the blocker rather than merely describing it.
//!
//! # The payload
//!
//! [`hostile_page`] is not random. It is the exact set of bytes the line
//! discipline transforms — `\r`, `\n`, `0x03` (VINTR), `0x11`/`0x13` (XON/XOFF),
//! `0x7f` (VERASE), `0x04` (VEOF), `0x1a` (VSUSP) — plus high-bit bytes for `ISTRIP`,
//! laid over a page-sized buffer. A 4 KiB page of a Linux kernel image contains
//! all of them; this just makes sure the test does not depend on luck.
//!
//! # Unix only
//!
//! There is no pty on Windows and no `serve --port` on Windows either — the
//! badge's node is a Unix device path. The whole file is gated rather than
//! pretending otherwise.

#![cfg(unix)]

use rv64_proto::{encode, Frame, Mux, PAGE};
use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// How long to wait for bytes that should arrive in microseconds.
///
/// Generous, because the negative test spends all of it: a pty in canonical
/// mode may deliver nothing at all rather than delivering something wrong, and
/// "nothing" is only observable as a timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Every byte the line discipline touches, laid over a page.
///
/// The first sixteen bytes are the ones that matter, spelled out so a failure
/// message points at a flag rather than at an offset. The rest is a repeating
/// ramp that covers `ISTRIP`'s territory above 0x7f.
fn hostile_page() -> Vec<u8> {
    let mut p = vec![
        b'\r', b'\n', 0x03, 0x11, 0x13, 0x7f, 0x04, 0x1a, 0x00, 0xff, 0x80, 0x0d, 0x0a, 0x1c,
        0x15, 0x17,
    ];
    while p.len() < PAGE {
        p.push((p.len() % 256) as u8);
    }
    p
}

/// A pty pair: the master, and the slave *by path*, so the slave can be opened
/// exactly the way `serve_main` opens `--port`.
struct Pty {
    master: File,
    slave_path: PathBuf,
}

/// Opens a pty pair.
///
/// `posix_openpt` + `grantpt` + `unlockpt` + `ptsname`, which is the portable
/// incantation and works identically on Linux and macOS. `O_NOCTTY` matters:
/// without it the pty could become this process's controlling terminal, and
/// then the `ISIG` half of the negative test below would deliver a real SIGINT
/// to the test runner.
fn open_pty() -> Pty {
    unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(fd >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
        assert_eq!(libc::grantpt(fd), 0, "grantpt: {}", std::io::Error::last_os_error());
        assert_eq!(libc::unlockpt(fd), 0, "unlockpt: {}", std::io::Error::last_os_error());
        let name = libc::ptsname(fd);
        assert!(!name.is_null(), "ptsname: {}", std::io::Error::last_os_error());
        let slave_path = PathBuf::from(CStr::from_ptr(name).to_str().expect("ptsname utf8"));
        Pty { master: File::from_raw_fd(fd), slave_path }
    }
}

impl Pty {
    /// Opens the slave the way `serve_main` opens `--port`: by path, read-write,
    /// nothing else. If this test ever passes because it opened the fd
    /// differently from production, it is testing the wrong thing.
    fn open_slave(&self) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.slave_path)
            .expect("opening the pty slave")
    }
}

/// Starts reading `n` bytes from `f` on a thread and returns a handle to
/// collect them.
///
/// **Reading must be started before writing**, and that is not a style
/// preference: a pty's buffer is on the order of a kilobyte on both platforms,
/// so a single-threaded `write_all` of a 4 KiB page followed by a read
/// deadlocks against itself with nobody draining the far end. (Written the
/// wrong way round first. It hung, which is why this paragraph exists.)
///
/// The read runs on its own thread so that a discipline which *swallows* the
/// payload — canonical mode waiting for a newline that never comes — shows up
/// as a timeout rather than as a hung test.
fn read_exactly(mut f: File, n: usize) -> Reader {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; n];
        let r = f.read_exact(&mut buf).map(|()| buf);
        // The receiver may already have timed out and gone; that is precisely
        // the case this helper exists to produce.
        tx.send(r.ok()).ok();
    });
    Reader(rx)
}

struct Reader(mpsc::Receiver<Option<Vec<u8>>>);

impl Reader {
    /// The bytes, or `None` if they never arrived within [`READ_TIMEOUT`].
    fn get(self) -> Option<Vec<u8>> {
        self.0.recv_timeout(READ_TIMEOUT).ok().flatten()
    }
}

/// **The hazard, demonstrated.** A pty in its default (canonical) state does
/// not carry page bytes.
///
/// This is the half that makes the fix meaningful. Without it,
/// `make_raw` could be clearing flags that were never set and every other
/// assertion here would still pass.
///
/// The assertion is deliberately weak — "did not arrive intact" rather than a
/// specific transformation — because *which* mangling happens first differs
/// between Linux and macOS, and between kernel versions. What does not differ
/// is that it happens.
#[test]
fn canonical_mode_really_does_mangle_a_page_frame() {
    let pty = open_pty();
    let slave = pty.open_slave();
    assert!(rv64_host::rawtty::is_tty(&slave), "a pty slave must look like a tty");

    // Deliberately NOT calling make_raw. This is the state `serve_main` used to
    // leave the port in.
    let page = hostile_page();
    let reader = read_exactly(slave, page.len());
    let mut master = pty.master;
    master.write_all(&page).expect("writing to the pty master");
    master.flush().ok();

    match reader.get() {
        None => { /* swallowed entirely — ICANON waiting, or MAX_CANON discard */ }
        Some(b) => assert_ne!(
            b, page,
            "a pty in canonical mode carried {} page bytes unchanged, which \
             contradicts every reason `rawtty` exists. If this is genuinely true on \
             this platform, the negative control is worthless and the positive tests \
             below prove nothing -- investigate before trusting them.",
            page.len()
        ),
    }
}

/// **The fix, demonstrated.** After `make_raw`, the same bytes cross the same
/// pty untouched, in both directions.
#[test]
fn raw_mode_carries_a_page_frame_byte_for_byte() {
    let page = hostile_page();

    // master -> slave, which is the direction a request travels.
    {
        let pty = open_pty();
        let slave = pty.open_slave();
        assert!(rv64_host::rawtty::make_raw(&slave).expect("make_raw"), "the pty is a tty");
        let reader = read_exactly(slave, page.len());
        let mut master = pty.master;
        master.write_all(&page).expect("write");
        master.flush().ok();
        let got = reader.get().expect("raw mode swallowed the payload instead of delivering it");
        assert_eq!(got, page, "master -> slave was not byte-identical");
    }

    // slave -> master, which is the direction a `ReadResp` travels. `OPOST`
    // and `ONLCR` live on this side, so a fix that only cleared the input
    // flags would pass the block above and fail here.
    {
        let pty = open_pty();
        let mut slave = pty.open_slave();
        assert!(rv64_host::rawtty::make_raw(&slave).expect("make_raw"), "the pty is a tty");
        let reader = read_exactly(pty.master, page.len());
        slave.write_all(&page).expect("write");
        slave.flush().ok();
        let got = reader.get().expect("raw mode swallowed the payload instead of delivering it");
        assert_eq!(got, page, "slave -> master was not byte-identical");
    }
}

/// End to end: the **real** `rv64_host::serve::serve`, over a real pty, opened
/// the way `serve_main` opens it, answering a page request whose payload is the
/// hostile page.
///
/// This is the test that maps directly onto the bench. If it passes, a page
/// request from the badge survives the tty; if the `make_raw` call were removed
/// it does not.
#[test]
fn the_real_serve_answers_a_page_request_over_a_pty() {
    let page = hostile_page();

    // A two-page image whose page 1 is the hostile payload.
    let dir = tempfile::tempdir().expect("tempdir");
    let img_path = dir.path().join("mem.img");
    {
        let mut img = std::fs::File::create(&img_path).expect("create");
        img.write_all(&vec![0u8; PAGE]).expect("page 0");
        img.write_all(&page).expect("page 1");
    }
    let mut img =
        std::fs::OpenOptions::new().read(true).write(true).open(&img_path).expect("reopen");

    let pty = open_pty();
    let slave = pty.open_slave();
    // Exactly what `serve_main` now does, in the same order: open the port,
    // then make it raw, then serve.
    assert!(rv64_host::rawtty::make_raw(&slave).expect("make_raw"), "the pty is a tty");
    let slave_rx = slave.try_clone().expect("clone");
    let host = std::thread::spawn(move || rv64_host::serve::serve(&mut img, slave_rx, slave, None, &mut Vec::new()));

    let mut master = pty.master;
    // A `ReadResp` is 4109 bytes: SYNC(2) + TYPE(1) + LEN(2) + page(4) +
    // PAGE(4096) + CRC32(4). Start draining before asking, for the
    // buffer-deadlock reason `read_exactly` documents.
    let reply_len = 5 + 4 + PAGE + 4;
    let reader = read_exactly(master.try_clone().expect("clone"), reply_len);

    let mut req = Vec::new();
    encode(&Frame::ReadReq { page: 1 }, &mut req);
    master.write_all(&req).expect("request");
    master.flush().ok();

    let raw = reader.get().expect(
        "no reply: the request never reached `serve` intact, which is what a \
         canonical-mode tty does to a binary frame",
    );

    let mut m = Mux::new();
    m.push(&raw);
    match m.take_matching(0x02) {
        Some(Frame::ReadResp { page: p, data }) => {
            assert_eq!(p, 1);
            assert_eq!(
                &data[..],
                &page[..],
                "the page came back altered: the tty rewrote bytes inside the payload"
            );
        }
        other => panic!(
            "expected a ReadResp, got {other:?} -- the reply did not decode, so \
             something transformed it on the way out"
        ),
    }

    // Close the master so `serve` sees EOF and the thread joins.
    drop(master);
    let _ = host.join();
}

/// Panic text arriving on `--port` is **not** eaten by the decoder.
///
/// The badge's log-server mirror shares the CDC endpoint with the protocol, so
/// `PANIC in PID n:` arrives interleaved with frames. `serve` used to scan past
/// it looking for SYNC and drop it, which is why the first hardware failure
/// produced no diagnosis at all until the server was killed and a plain reader
/// attached — every bit of panic visibility this project built, defeated by the
/// thing listening in front of it.
///
/// This asserts the badge half of the guarantee at the `Mux` level (the same
/// object `serve_once` calls), because `serve` writes the bytes to the
/// *process's* stderr and a test cannot capture that without redirecting fd 2.
/// `crates/rv64-proto`'s own tests cover the decoder; this covers that a real
/// interleaving over a real tty still yields both the frame and the text.
#[test]
fn panic_text_interleaved_with_frames_survives_the_link() {
    const PANIC: &[u8] = b"PANIC in PID 4: panicked at 'no ticktimer', main.rs:130\n";

    let pty = open_pty();
    let slave = pty.open_slave();
    assert!(rv64_host::rawtty::make_raw(&slave).expect("make_raw"), "the pty is a tty");

    // The badge's side: a request frame with mirrored panic text either side of
    // it, exactly as the shared transmit endpoint produces.
    let mut wire = Vec::new();
    wire.extend_from_slice(PANIC);
    encode(&Frame::ReadReq { page: 5 }, &mut wire);
    wire.extend_from_slice(b"more mirror text\n");

    let reader = read_exactly(slave, wire.len());
    let mut master = pty.master;
    master.write_all(&wire).expect("write");
    master.flush().ok();
    let got = reader.get().expect("the tty swallowed the interleaved stream");
    assert_eq!(got, wire, "raw mode did not carry the interleaving intact");

    // And the host's side: the frame is decoded and the text is handed back
    // rather than dropped.
    let mut m = rv64_proto::Mux::capturing_noise();
    m.push(&got);
    assert_eq!(m.take_matching(0x01), Some(Frame::ReadReq { page: 5 }));
    let (noise, dropped) = m.take_noise();
    assert_eq!(dropped, 0);
    let text = String::from_utf8_lossy(&noise).into_owned();
    assert!(
        text.contains("PANIC in PID 4") && text.contains("more mirror text"),
        "the panic mirror was eaten by the decoder: {text:?}"
    );
}

/// A `WriteReq` carrying the hostile page lands in the image unchanged.
///
/// The read path above proves the slave-to-master direction; this proves the
/// master-to-slave direction all the way through to the file, which is where a
/// silently-rewritten `\n` would end up as corrupt guest memory.
#[test]
fn a_write_request_over_a_pty_lands_in_the_image_unchanged() {
    let page = hostile_page();

    let dir = tempfile::tempdir().expect("tempdir");
    let img_path = dir.path().join("mem.img");
    std::fs::write(&img_path, vec![0u8; PAGE * 2]).expect("create");
    let mut img =
        std::fs::OpenOptions::new().read(true).write(true).open(&img_path).expect("reopen");

    let pty = open_pty();
    let slave = pty.open_slave();
    assert!(rv64_host::rawtty::make_raw(&slave).expect("make_raw"), "the pty is a tty");
    let slave_rx = slave.try_clone().expect("clone");
    let host = std::thread::spawn(move || rv64_host::serve::serve(&mut img, slave_rx, slave, None, &mut Vec::new()));

    let mut master = pty.master;
    // `WriteAck` is 5 + 4 + 4 = 13 bytes.
    let reader = read_exactly(master.try_clone().expect("clone"), 13);

    let mut req = Vec::new();
    let mut data = Box::new([0u8; PAGE]);
    data.copy_from_slice(&page);
    encode(&Frame::WriteReq { page: 1, data }, &mut req);
    master.write_all(&req).expect("request");
    master.flush().ok();

    let raw = reader.get().expect("no WriteAck: the 4109-byte request did not survive the tty");
    let mut m = Mux::new();
    m.push(&raw);
    assert_eq!(m.take_matching(0x04), Some(Frame::WriteAck { page: 1 }));

    drop(master);
    let _ = host.join();

    let on_disk = std::fs::read(&img_path).expect("read back");
    assert_eq!(
        &on_disk[PAGE..PAGE * 2],
        &page[..],
        "the page reached the image altered: the tty rewrote bytes inside guest memory"
    );
}

/// `--input`'s side of the file: the operator's own terminal, which needs a
/// *different* raw mode from `--port` and needs putting back afterwards.
///
/// # Why this is not the same test as the ones above
///
/// `Raw::Port` is judged by "did a page frame survive". `Raw::Console` cannot
/// be — nothing binary crosses it — and the two things it must get right are
/// invisible to a byte-for-byte check:
///
/// * It must clear the flags that stop a keystroke being a keystroke
///   (`ICANON`, `ECHO`) and the one that would kill `serve` before it could put
///   the terminal back (`ISIG`).
/// * It must **keep** `OPOST`/`ONLCR` and `ICRNL`. stdin and stdout are the same
///   device: clearing the output flags there stair-steps the guest output the
///   operator is typing against, and clearing `ICRNL` changes Enter from the
///   `\n` the dry run proved to a `\r` nothing has.
///
/// And it must restore. A `serve` that exits leaving `ECHO` off hands back a
/// shell that types nothing, which is the most obnoxious way this feature can
/// fail and the easiest one to cause.
#[test]
fn console_mode_clears_what_it_must_and_keeps_what_it_must() {
    let pty = open_pty();
    let slave = pty.open_slave();

    let before = tcget(&slave);
    // A fresh pty is in canonical mode with the usual translations on. If it
    // were not, this test would be asserting against nothing.
    assert_ne!(before.c_lflag & libc::ICANON, 0, "a fresh pty should be canonical");
    assert_ne!(before.c_lflag & libc::ECHO, 0, "a fresh pty should echo");
    assert_ne!(before.c_iflag & libc::ICRNL, 0, "a fresh pty should have ICRNL");
    assert_ne!(before.c_oflag & libc::OPOST, 0, "a fresh pty should post-process output");

    let restore = rv64_host::rawtty::make_raw_console(&slave)
        .expect("configuring a pty slave")
        .expect("a pty is a tty, so there must be something to restore");

    let now = tcget(&slave);
    for (name, bits) in [
        ("ICANON", libc::ICANON),
        ("ECHO", libc::ECHO),
        ("ISIG", libc::ISIG),
        ("IEXTEN", libc::IEXTEN),
    ] {
        assert_eq!(now.c_lflag & bits, 0, "{name} must be cleared on the operator's terminal");
    }
    for (name, bits) in [("IXON", libc::IXON), ("INLCR", libc::INLCR), ("ISTRIP", libc::ISTRIP)] {
        assert_eq!(now.c_iflag & bits, 0, "{name} must be cleared on the operator's terminal");
    }
    assert_ne!(
        now.c_iflag & libc::ICRNL,
        0,
        "ICRNL must survive: Enter has to arrive as the \\n the dry run drives the guest with"
    );
    assert_eq!(
        now.c_oflag & mask_of(&[libc::OPOST, libc::ONLCR]),
        before.c_oflag & mask_of(&[libc::OPOST, libc::ONLCR]),
        "the output flags must survive: this is the same device the guest's console prints on"
    );
    assert_eq!(now.c_cc[libc::VMIN], 1, "a keystroke must cross when it is typed");
    assert_eq!(now.c_cc[libc::VTIME], 0);

    restore.restore();
    let after = tcget(&slave);
    // Compared under a mask of the flags this code touches, not on the whole
    // word. Some bits belong to the driver rather than to us — macOS sets
    // `PENDIN` (0x20000000) itself once input has been through a non-canonical
    // read, and it is still set after a faithful `tcsetattr` of the saved
    // struct. Asserting on the raw word made this test fail against a restore
    // that had in fact put back every setting it changed, which is a test
    // reporting on the tty driver instead of on this crate.
    assert_eq!(
        after.c_iflag & TOUCHED_I,
        before.c_iflag & TOUCHED_I,
        "iflag was not restored"
    );
    assert_eq!(
        after.c_oflag & TOUCHED_O,
        before.c_oflag & TOUCHED_O,
        "oflag was not restored"
    );
    assert_eq!(
        after.c_lflag & TOUCHED_L,
        before.c_lflag & TOUCHED_L,
        "lflag was not restored -- the operator is left without echo"
    );
    assert_eq!(
        after.c_cflag & TOUCHED_C,
        before.c_cflag & TOUCHED_C,
        "cflag was not restored"
    );
    assert_eq!(after.c_cc[libc::VMIN], before.c_cc[libc::VMIN], "VMIN was not restored");

    // Idempotent on purpose: both the serve loop and the keyboard thread call
    // it, and neither knows which of them is ending the run.
    restore.restore();
    assert_eq!(
        tcget(&slave).c_lflag & TOUCHED_L,
        before.c_lflag & TOUCHED_L,
        "a second restore must be a no-op"
    );
}

/// `--input` against a pipe is legitimate — that is how a script types — and
/// there is no line discipline to configure or to put back.
#[test]
fn console_mode_leaves_a_non_tty_alone() {
    let f = std::fs::File::open("/dev/null").expect("/dev/null");
    assert!(
        rv64_host::rawtty::make_raw_console(&f).expect("a non-tty is not an error").is_none(),
        "there is nothing to restore on something that was never configured"
    );
}

/// The flags `rawtty` sets or clears, and therefore the only ones it is
/// answerable for putting back. Everything outside these masks belongs to the
/// tty driver.
const TOUCHED_I: libc::tcflag_t = libc::IGNBRK
    | libc::BRKINT
    | libc::IGNPAR
    | libc::PARMRK
    | libc::INPCK
    | libc::ISTRIP
    | libc::INLCR
    | libc::IGNCR
    | libc::ICRNL
    | libc::IXON
    | libc::IXOFF
    | libc::IXANY
    | libc::IMAXBEL;
const TOUCHED_O: libc::tcflag_t =
    libc::OPOST | libc::ONLCR | libc::OCRNL | libc::ONOCR | libc::ONLRET;
const TOUCHED_L: libc::tcflag_t = libc::ECHO
    | libc::ECHOE
    | libc::ECHOK
    | libc::ECHONL
    | libc::ICANON
    | libc::ISIG
    | libc::IEXTEN;
const TOUCHED_C: libc::tcflag_t =
    libc::PARENB | libc::CSIZE | libc::HUPCL | libc::CS8 | libc::CREAD | libc::CLOCAL;

fn mask_of(flags: &[libc::tcflag_t]) -> libc::tcflag_t {
    flags.iter().fold(0, |a, f| a | f)
}

fn tcget(f: &File) -> libc::termios {
    use std::os::unix::io::AsRawFd;
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(f.as_raw_fd(), &mut t), 0, "tcgetattr");
        t
    }
}
