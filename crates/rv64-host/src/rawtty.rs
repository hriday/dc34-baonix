//! Putting a tty into raw mode: `--port`, which is the difference between the
//! badge link working and the badge link silently not working, and — since
//! `serve --input` — the operator's own terminal too. The two want different
//! raw modes; see "Two ttys, not one" below.
//!
//! # Why this file exists
//!
//! `--port` is a USB-CDC node — `/dev/ttyACM0` on Linux, `/dev/cu.usbmodem*` on
//! macOS — and a freshly opened tty is in **canonical mode**. Against a
//! 4109-byte binary frame carrying arbitrary page data that means:
//!
//! | flag | what it does to a page frame |
//! |---|---|
//! | `ICANON` | `read()` blocks until a `\n`, and the line-discipline buffer overflows at `MAX_CANON` and **discards** |
//! | `ICRNL` / `ONLCR` / `INLCR` / `IGNCR` | every `\r` and `\n` byte *inside page data* is rewritten, in both directions |
//! | `ECHO` | every byte the badge sends is echoed straight back at the badge, into its `Mux` |
//! | `ISIG` | a `0x03` byte in page data raises SIGINT and **kills the server** |
//! | `IXON` / `IXOFF` / `IXANY` | `0x11` and `0x13` in page data are eaten as flow control |
//! | `ISTRIP` | the high bit is cleared off every byte |
//!
//! A 4 KiB page of a Linux kernel image contains those bytes with near
//! certainty, so the *first* page exchange is corrupted.
//!
//! # This project already learned this, in Python, and then lost it
//!
//! `badge/echo-host.py` — the probe's host — configures the tty by hand rather
//! than with `tty.setraw()`, and the comment above its flag lists is the
//! postmortem:
//!
//! > termios state persists on the device node across opens: an earlier program
//! > that left `INLCR` set turns the probe's `REQ\n` into `REQ\r`, this script
//! > never answers, and the badge reports a false `rt: TIMEOUT` that looks like
//! > a hardware finding.
//!
//! The flag lists below are that script's, verbatim. When the host moved from
//! Python to Rust the knowledge did not come with it, and nothing caught it
//! because every test drives the protocol over something that is not a tty —
//! `serve()` is generic over `Read + Write`, which is exactly what makes the
//! line discipline invisible. `tests/rawtty.rs` is the test that closes that:
//! it runs the protocol over a real pty and would have failed on the version of
//! this crate that had no termios call in it at all.
//!
//! `echo-host.py` also *restores* the saved attributes when it exits, so the
//! device really is back in canonical mode by the time `rv64-host serve` opens
//! it. There is no accidental rescue.
//!
//! # Why not a documented `stty` incantation
//!
//! Because termios state lives on the device node, not on the fd, and the next
//! program to touch that node undoes it. A setting that depends on the operator
//! having run a command earlier in the session — and on nothing else having
//! opened the port since — is a setting that will be wrong on the run that
//! matters. `serve` sets it on its own fd, every time, immediately before it
//! starts serving.
//!
//! # What this deliberately does not do
//!
//! It does not set a baud rate. USB-CDC ignores line coding: the rate is a
//! fiction the host stack hands the device, and `usb-bao1x` discards it.
//! Setting one would put a number in a transcript that means nothing.

//! # Two ttys, not one
//!
//! `serve --input` puts a *second* tty into raw mode: the operator's own
//! terminal, so a keystroke reaches the guest shell as a keystroke rather than
//! as a line the local line discipline has already cooked. [`Raw::Console`] is
//! that mode, and it deliberately differs from [`Raw::Port`] in exactly two
//! places — see [`keep`] for the reasoning. It also *saves* the previous
//! settings and hands back a [`Restore`], because unlike `--port` (a device
//! node nothing else is using) the operator's terminal is the thing they keep
//! working in afterwards. A `serve` that exits leaving stdin with `ECHO` and
//! `ICANON` off leaves a shell that types nothing back at you.
//!
//! # Unix only, but the module still compiles elsewhere
//!
//! Everything that touches termios is `#[cfg(unix)]`, with a fallback that
//! reports "not a terminal" on other platforms. Not portability for its own
//! sake: `--port` is a Unix device node and always will be. It is that
//! `rv64-host` is otherwise a plain `std` crate that builds anywhere, and
//! reaching for `std::os::unix` unconditionally would make the whole workspace
//! stop compiling on Windows for the sake of one function nobody there can
//! call. The fallback is honest rather than a stub — on a platform with no line
//! discipline there is nothing to configure, and `serve_main` prints exactly
//! that.

use std::io;

/// Whether `f` is a terminal.
///
/// A plain file, a fifo or a socket has no line discipline to configure, and
/// passing one to `--port` is legitimate: `tests/serve.rs` uses byte buffers
/// and `badge/app/tests/dry_run.rs` uses a socket. Those must keep working.
#[cfg(unix)]
pub fn is_tty<F: std::os::unix::io::AsRawFd>(f: &F) -> bool {
    // SAFETY: `isatty` only inspects the descriptor. A bad fd returns 0.
    unsafe { libc::isatty(f.as_raw_fd()) == 1 }
}

/// No line discipline on this platform, so nothing is ever a terminal here.
#[cfg(not(unix))]
pub fn is_tty<F>(_f: &F) -> bool {
    false
}

/// Which tty is being configured, and therefore which translations survive.
///
/// The two are not interchangeable and the difference is not cosmetic — see
/// [`keep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raw {
    /// `--port`: the badge's USB-CDC node. Arbitrary binary page data crosses
    /// it in both directions, so **every** translation goes.
    Port,
    /// `--input`: the operator's own terminal. Carries typed characters one
    /// way and printed guest output the other, so two groups stay.
    Console,
}

/// Nothing to configure on a platform with no termios.
#[cfg(not(unix))]
pub fn make_raw<F>(_f: &F) -> io::Result<bool> {
    Ok(false)
}

/// Nothing to configure, and so nothing to put back.
#[cfg(not(unix))]
pub fn make_raw_console<F>(_f: &F) -> io::Result<Option<Restore>> {
    Ok(None)
}

/// A platform with no termios has no saved state to restore.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct Restore(());

#[cfg(not(unix))]
impl Restore {
    pub fn restore(&self) {}
}

/// Input flags that must be off. `badge/echo-host.py`'s `IFLAG_CLEAR`.
#[cfg(unix)]
const IFLAGS_OFF: &[libc::tcflag_t] = &[
    libc::IGNBRK,
    libc::BRKINT,
    libc::IGNPAR,
    libc::PARMRK,
    libc::INPCK,
    libc::ISTRIP,
    libc::INLCR,
    libc::IGNCR,
    libc::ICRNL,
    libc::IXON,
    libc::IXOFF,
    libc::IXANY,
    libc::IMAXBEL,
];

/// Output flags that must be off. `OPOST` alone would do on every platform this
/// runs on, but the rest are cleared for the reason the Python clears them:
/// termios state persists on the node, so an inherited `ONLCR` sitting under an
/// `OPOST` that something else re-enables later is a rewrite waiting to happen.
#[cfg(unix)]
const OFLAGS_OFF: &[libc::tcflag_t] =
    &[libc::OPOST, libc::ONLCR, libc::OCRNL, libc::ONOCR, libc::ONLRET];

/// Local flags that must be off: echo in all its forms, line buffering, signal
/// generation, and the implementation-defined extensions.
#[cfg(unix)]
const LFLAGS_OFF: &[libc::tcflag_t] = &[
    libc::ECHO,
    libc::ECHOE,
    libc::ECHOK,
    libc::ECHONL,
    libc::ICANON,
    libc::ISIG,
    libc::IEXTEN,
];

/// Control flags that must be off: parity, the character-size mask (so `CS8`
/// below is not OR-ed into a stale width), and hang-up-on-close.
#[cfg(unix)]
const CFLAGS_OFF: &[libc::tcflag_t] = &[libc::PARENB, libc::CSIZE, libc::HUPCL];

/// Control flags that must be on: eight data bits, receiver enabled, and
/// `CLOCAL` so a missing carrier neither blocks the open nor drops the line.
#[cfg(unix)]
const CFLAGS_ON: &[libc::tcflag_t] = &[libc::CS8, libc::CREAD, libc::CLOCAL];

#[cfg(unix)]
fn mask(flags: &[libc::tcflag_t]) -> libc::tcflag_t {
    flags.iter().fold(0, |acc, f| acc | f)
}

/// The bits of [`IFLAGS_OFF`] and [`OFLAGS_OFF`] that this mode **keeps**.
///
/// `Raw::Port` keeps nothing: a 4 KiB page contains every byte the line
/// discipline rewrites, so any surviving translation corrupts a frame.
///
/// `Raw::Console` keeps two groups, and both are deliberate:
///
/// * **The output flags, all of them.** stdin and stdout are the *same device*
///   on a terminal, so clearing `OPOST`/`ONLCR` here would stop the guest's own
///   console output — which the operator is reading on that terminal, and which
///   is the entire point of typing at it — from getting its carriage returns.
///   The result is the stair-stepped display every raw-mode program that
///   forgets this produces. Nothing binary is ever written to this fd, so there
///   is nothing for `OPOST` to damage.
///
/// * **`ICRNL`.** With it cleared, Enter arrives as `\r`; with it set, as
///   `\n`. Both can work — the guest's own n_tty has `ICRNL` on and would
///   translate — but `\n` is the byte sequence `badge/app/tests/dry_run.rs`
///   drives the guest with (`TYPED` ends in `\n`) and therefore the only one
///   proven end to end on this stack. A hardware cycle costs a power cycle and
///   forty minutes; this is not the place to find out. `Ctrl-]` sends a literal
///   `\r` for anything that needs one — see [`crate::serve::pump_input`].
///
/// Everything else is cleared in both modes. `ISIG` in particular: `serve
/// --input` handles `Ctrl-C` itself, in band, precisely so that it can put this
/// terminal back before it exits — a `SIGINT` that kills the process runs no
/// destructor and leaves the operator with no echo.
#[cfg(unix)]
fn keep(mode: Raw) -> (libc::tcflag_t, libc::tcflag_t) {
    match mode {
        Raw::Port => (0, 0),
        Raw::Console => (libc::ICRNL, mask(OFLAGS_OFF)),
    }
}

/// Reads `fd`'s current settings. Shared by the setter and by the save that
/// [`make_raw_console`] takes before it changes anything.
#[cfg(unix)]
fn get(fd: std::os::unix::io::RawFd) -> io::Result<libc::termios> {
    // SAFETY: `termios` is a plain C struct with no invalid bit patterns for
    // `tcgetattr` to produce, and `fd` is owned by the caller across this call.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(t)
}

/// The body both entry points share: clear the flags this `mode` clears, set
/// the ones it sets, and **read them back** to check they took.
#[cfg(unix)]
fn apply(fd: std::os::unix::io::RawFd, mode: Raw) -> io::Result<()> {
    let (ikeep, okeep) = keep(mode);
    let iclear = mask(IFLAGS_OFF) & !ikeep;
    let oclear = mask(OFLAGS_OFF) & !okeep;

    let mut t = get(fd)?;
    t.c_iflag &= !iclear;
    t.c_oflag &= !oclear;
    t.c_lflag &= !mask(LFLAGS_OFF);
    t.c_cflag = (t.c_cflag & !mask(CFLAGS_OFF)) | mask(CFLAGS_ON);
    // Block until at least one byte is available, with no inter-byte timer.
    // `serve`'s loop is read -> answer -> read, so a timer would only turn
    // "nothing to do yet" into a spin. The keyboard reader wants exactly the
    // same thing for a different reason: a keystroke should cross the wire when
    // it is typed, not when a timer next fires.
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;

    // `TCSANOW`, not `TCSAFLUSH`. Flushing would discard bytes the badge has
    // already sent and is waiting on an answer for. Nothing is in flight when
    // `serve_main` calls this — it has not started reading — but the badge may
    // already be talking, and a link that opens by dropping the first request
    // is precisely the silent hang this file exists to prevent.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Read it back and require the flags to have actually taken. `tcsetattr`
    // is specified to return success if it applied *any* of the requested
    // changes, not all of them, so its return value alone does not mean the
    // port is raw. Checking is three syscall-free comparisons against a number
    // we already have, and the alternative is discovering it on the bench.
    let check = get(fd)?;
    // `CFLAGS_OFF` contains `CSIZE`, which is the *mask* the `CS8` in
    // `CFLAGS_ON` sets bits within — on both Linux and macOS `CS8 == CSIZE`
    // numerically. So the readback must not demand that the bits `CFLAGS_ON`
    // deliberately set are clear; what it checks is `CFLAGS_OFF` minus those.
    // (Written the wrong way round first, and `tests/rawtty.rs` caught it
    // immediately, which is the argument for that test in one line.)
    let cflags_must_be_clear = mask(CFLAGS_OFF) & !mask(CFLAGS_ON);
    let stuck = [
        ("iflag", check.c_iflag & iclear),
        ("oflag", check.c_oflag & oclear),
        ("lflag", check.c_lflag & mask(LFLAGS_OFF)),
        ("cflag", check.c_cflag & cflags_must_be_clear),
    ];
    if let Some((which, bits)) = stuck.iter().find(|(_, b)| *b != 0) {
        return Err(io::Error::other(format!(
            "tcsetattr reported success but {which} still has {bits:#x} set: the port is \
             not in raw mode and binary frames will be mangled"
        )));
    }
    if check.c_cflag & mask(CFLAGS_ON) != mask(CFLAGS_ON) {
        return Err(io::Error::other(
            "tcsetattr reported success but CS8/CREAD/CLOCAL did not take: the port is \
             not in raw mode and binary frames will be mangled",
        ));
    }
    Ok(())
}

/// A tty's settings as they were before [`make_raw_console`] changed them, and
/// the one thing that can put them back.
///
/// # Why this is not `Drop` alone
///
/// `Drop` is the backstop, not the mechanism. The keyboard reader runs on its
/// own thread and ends the process with [`std::process::exit`] when the
/// operator types `Ctrl-C` — which runs **no** destructors, anywhere. So
/// [`Restore::restore`] is called explicitly on every path that ends `serve`,
/// and `Drop` catches the ones that unwind instead.
///
/// Calling it twice is fine and is expected: `restore` is idempotent, so both
/// the serve loop and the keyboard thread can call it without either having to
/// know whether the other got there first.
#[cfg(unix)]
#[derive(Debug)]
pub struct Restore {
    fd: std::os::unix::io::RawFd,
    saved: libc::termios,
    done: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl Restore {
    /// Puts the saved settings back. Idempotent, and deliberately silent about
    /// failure: this runs while `serve` is on its way out, there is nowhere
    /// useful to report to, and a terminal that could not be restored is not
    /// made better by a message printed onto it.
    pub fn restore(&self) {
        if self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // SAFETY: `saved` came from `tcgetattr` on this same fd, which is
        // stdin and is open for the lifetime of the process.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(unix)]
impl Drop for Restore {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Puts the operator's terminal into [`Raw::Console`] mode, saving what was
/// there first.
///
/// `Ok(None)` means `f` is not a tty — a pipe or a file, which is a legitimate
/// way to drive `--input` from a script and needs no configuring. `Ok(Some(r))`
/// means a real terminal was reconfigured and **`r` must outlive the input**:
/// dropping it, or calling [`Restore::restore`], is what gives the operator
/// their echo back.
///
/// # Errors
///
/// Whatever `tcgetattr`/`tcsetattr` reported, plus a readback failure. Unlike
/// `--port`, a failure here is not fatal to the link — the operator simply
/// cannot type usefully — but it is still returned rather than swallowed, since
/// a terminal that silently stayed canonical looks to the operator like a badge
/// that ignores the keyboard.
#[cfg(unix)]
pub fn make_raw_console<F: std::os::unix::io::AsRawFd>(f: &F) -> io::Result<Option<Restore>> {
    if !is_tty(f) {
        return Ok(None);
    }
    let fd = f.as_raw_fd();
    // Saved *before* anything is changed, so a failure part-way through `apply`
    // still leaves the caller holding the means to undo it.
    let saved = get(fd)?;
    let r = Restore { fd, saved, done: std::sync::atomic::AtomicBool::new(false) };
    apply(fd, Raw::Console)?;
    Ok(Some(r))
}

/// Puts `f` into raw mode, byte for byte: no echo, no line discipline, no
/// signal generation, no CR/LF translation, no flow control, no high-bit
/// stripping, and a read that returns as soon as one byte is available.
///
/// A non-tty is left alone and reported as `Ok(false)`. `Ok(true)` means a real
/// terminal was reconfigured.
///
/// # Errors
///
/// Whatever `tcgetattr`/`tcsetattr` reported, plus a readback failure — see
/// below. A failure here **must not** be swallowed: a canonical-mode tty is not
/// a degraded link, it is a link that cannot carry one frame, and it fails in a
/// way that looks like a fault at the far end of the cable.
#[cfg(unix)]
pub fn make_raw<F: std::os::unix::io::AsRawFd>(f: &F) -> io::Result<bool> {
    if !is_tty(f) {
        return Ok(false);
    }
    apply(f.as_raw_fd(), Raw::Port)?;
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_plain_file_is_not_a_tty_and_is_left_alone() {
        let f = tempfile::tempfile().unwrap();
        assert!(!is_tty(&f));
        assert!(!make_raw(&f).unwrap(), "a plain file must be reported as not-a-tty");
    }

    /// A group that folded to zero would clear nothing while looking like it
    /// cleared everything, which is the shape of the bug this whole file is
    /// about.
    #[test]
    fn every_flag_group_is_non_empty() {
        assert_ne!(mask(IFLAGS_OFF), 0);
        assert_ne!(mask(OFLAGS_OFF), 0);
        assert_ne!(mask(LFLAGS_OFF), 0);
        assert_ne!(mask(CFLAGS_OFF), 0);
        assert_ne!(mask(CFLAGS_ON), 0);
    }

    /// The specific flags whose absence corrupts a page frame, named one by one
    /// so an edit that trims the lists cannot quietly drop one.
    #[test]
    fn the_flags_that_corrupt_a_page_frame_are_all_cleared() {
        assert_ne!(mask(LFLAGS_OFF) & libc::ICANON, 0, "ICANON");
        assert_ne!(mask(LFLAGS_OFF) & libc::ECHO, 0, "ECHO");
        assert_ne!(mask(LFLAGS_OFF) & libc::ISIG, 0, "ISIG");
        assert_ne!(mask(IFLAGS_OFF) & libc::IXON, 0, "IXON");
        assert_ne!(mask(IFLAGS_OFF) & libc::ICRNL, 0, "ICRNL");
        assert_ne!(mask(IFLAGS_OFF) & libc::INLCR, 0, "INLCR");
        assert_ne!(mask(IFLAGS_OFF) & libc::IGNCR, 0, "IGNCR");
        assert_ne!(mask(IFLAGS_OFF) & libc::ISTRIP, 0, "ISTRIP");
        assert_ne!(mask(OFLAGS_OFF) & libc::OPOST, 0, "OPOST");
        assert_ne!(mask(OFLAGS_OFF) & libc::ONLCR, 0, "ONLCR");
    }
}
