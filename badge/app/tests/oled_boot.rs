//! What the badge's OLED actually shows when the real guest boots.
//!
//! This is the highest-value laptop test available to Task 7, and it is the
//! reason `oled`'s grid is a plain Rust module rather than something that only
//! exists below `#[cfg(target_os = "xous")]`. It boots the *real* nixpkgs-built
//! riscv64 guest — the same images `crates/rv64-host/tests/boot.rs` uses, no
//! fixture anywhere — pipes every console byte through the same
//! [`badge_app::oled::OledSink`] the badge runs, and asserts on the grid of
//! characters that would have been on the screen.
//!
//! `boot.rs` asserts what the *guest* printed. This asserts what the *display*
//! would show, which is a different claim and the one the project is judged on:
//! the deliverable is a photograph. Every failure it can catch — a column count
//! that disagrees with the font, a scroll that eats a line, an escape sequence
//! rendered as literal `[1;34m` across the store paths — would otherwise cost a
//! flash-and-photograph cycle through a human to discover.
//!
//! Like the workspace's other integration suites it skips when its images are
//! absent, and `RV64_REQUIRE_SUITES=1` — which the devShell sets — turns that
//! skip into a hard failure. **Run it with `--release`**: booting Linux is 175
//! million emulated instructions, which is ~38 seconds optimized and about
//! 4m40 not.

use badge_app::oled::{Grid, OledSink, Screen, COLS, ROWS};
use rv64::uart::ConsoleSink;
use std::path::PathBuf;

/// The busybox `ash` prompt, and the marker `boot_capturing` stops on.
const SHELL_PROMPT: &str = "~ #";

/// Same safety valve as `crates/rv64-host/tests/boot.rs`: ~5.7x the measured
/// cost of a real boot, so a guest that never reaches a prompt fails in
/// minutes instead of hanging.
const MAX_INSNS: u64 = 1_000_000_000;

/// The alphabet Nix store-path hashes are printed in: base32 without `e`, `o`,
/// `u` or `t`.
const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

fn image(var: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(var)?);
    p.exists().then_some(p)
}

/// Every frame the display would have shown, in order.
///
/// The sink is flushed once per newline rather than once per byte: a frame per
/// byte would be a hundred thousand eight-row snapshots for no extra evidence,
/// since nothing can scroll off the screen without a newline crossing it.
fn screens_during_boot(output: &str) -> Vec<Vec<String>> {
    let mut sink = OledSink::with_screen(badge_app::oled::FakeScreen::default());
    // The spinner would sit in the last cell of the bottom row and this test is
    // about the guest's characters, not about liveness.
    sink.set_heartbeat(false);
    for b in output.bytes() {
        sink.put(b);
        if b == b'\n' {
            sink.flush();
        }
    }
    sink.flush();
    let mut frames = sink.into_screen().frames;
    // Drop the banner frame `OledSink::with_screen` draws before any guest
    // byte arrives; it is asserted separately, in the unit tests.
    frames.remove(0);
    frames
}

/// A genuine store path as it lands on a 16-column screen: the 32-character
/// hash across two full rows, then the package name on the row under it.
///
/// The adjacency is what binds the halves together — the hyphen used to do
/// that when the path was one line, and the row boundary does it now.
fn store_path_on_screen(rows: &[String], i: usize, name: &str) -> bool {
    let (Some(top), Some(bottom)) = (rows.get(i), rows.get(i + 1)) else {
        return false;
    };
    let hash: String = format!("{top}{bottom}");
    hash.len() == 32
        && hash.chars().all(|c| NIX_BASE32.contains(c))
        && rows.get(i + 2).is_some_and(|next| next.contains(name))
}

#[test]
fn the_guest_boot_renders_on_the_badge_grid() {
    let (Some(kernel), Some(dtb), Some(initramfs)) =
        (image("GUEST_KERNEL"), image("GUEST_DTB"), image("GUEST_INITRAMFS"))
    else {
        rv64_host::suite_prerequisite_missing(
            "oled_boot",
            "GUEST_KERNEL, GUEST_DTB and GUEST_INITRAMFS are not all set to files that \
             exist. `nix develop` sets all three from nix/guest; outside it, build them \
             with `nix build .#guest`",
        );
        return;
    };

    let run = rv64_host::boot_capturing(&kernel, &dtb, &initramfs, MAX_INSNS, SHELL_PROMPT)
        .expect("the guest images must load");
    let frames = screens_during_boot(&run.output);

    // The last screen, printed unconditionally: this is the photograph, in
    // text, and `--nocapture` should be able to show it without a failure to
    // hang it off.
    let last = frames.last().expect("the guest printed nothing at all").clone();
    eprintln!("+{}+", "-".repeat(COLS));
    for row in &last {
        eprintln!("|{row}|");
    }
    eprintln!("+{}+", "-".repeat(COLS));

    // Every frame is exactly the shape the renderer promises the display. If
    // this ever fails, the typesetter would be re-wrapping rows the grid had
    // already wrapped, and the screen would be garbage.
    for (n, frame) in frames.iter().enumerate() {
        assert_eq!(frame.len(), ROWS, "frame {n} has {} rows", frame.len());
        assert!(
            frame.iter().all(|r| r.len() == COLS),
            "frame {n} has a row that is not {COLS} wide: {frame:?}"
        );
    }

    // Nothing rendered an escape sequence as literal text. The guest's console
    // is a tty, so this is not hypothetical -- unstripped, `[0;32m` and friends
    // would be scattered through every screen below.
    for (n, frame) in frames.iter().enumerate() {
        for row in frame {
            assert!(!row.contains("[0m") && !row.contains("[1;"), "frame {n} shows a CSI: {row:?}");
        }
    }

    let showed = |text: &str| frames.iter().any(|f| f.iter().any(|r| r.trim_end() == text));

    // `/init` ran, and its banner fits. The model line is the sharp one: it is
    // `cut -c1-16` in `nix/guest/init.sh`, so it is exactly COLS characters and
    // landing it whole on one row is direct evidence the column count agrees
    // with the font.
    assert!(showed("riscv64 Linux"), "the display never showed the init banner");
    assert!(showed("baochip rv64 emu"), "the display never showed the model line whole");
    assert!(showed("/nix/store:"), "the display never reached the store listing");

    // And the deliverable: a genuine store path, read by the guest off its own
    // filesystem, laid out across the screen the photograph is of.
    assert!(
        frames
            .iter()
            .any(|f| (0..f.len()).any(|i| store_path_on_screen(f, i, "busybox"))),
        "no genuine store path was ever on screen"
    );

    // The shell prompt reached the display, not merely the transcript.
    assert!(run.reached_marker, "the guest never printed a `{SHELL_PROMPT}` prompt");
    assert!(
        last.iter().any(|r| r.contains(SHELL_PROMPT)),
        "the guest reached a shell but the prompt is not on the final screen: {last:?}"
    );
}

#[test]
fn the_grid_shows_what_the_guests_own_banner_is_shaped_for() {
    // The same layout claim as above, made without booting anything, so a
    // change to `nix/guest/init.sh`'s widths fails in a second rather than in
    // forty. The strings are init.sh's `cut` widths, not a guess.
    let mut g = Grid::blank();
    g.write_str("\nriscv64 Linux\n6.6.0\nbaochip rv64 emu\n\n/nix/store:\n");
    assert_eq!(g.line(1), "riscv64 Linux");
    assert_eq!(g.line(3), "baochip rv64 emu");
    assert_eq!(g.line(3).len(), COLS, "the model line must fill a row exactly");
    assert_eq!(g.line(5), "/nix/store:");
}

/// A [`Screen`] that fails once and then works, to show at the integration
/// level what the unit tests show at the unit level: a dropped frame is not a
/// dropped byte.
#[derive(Default)]
struct FlakyScreen {
    drawn: Vec<String>,
    fail_next: bool,
}

impl Screen for FlakyScreen {
    fn draw(&mut self, frame: &str) -> Result<(), badge_app::oled::ScreenError> {
        if std::mem::take(&mut self.fail_next) {
            return Err(badge_app::oled::ScreenError::Draw);
        }
        self.drawn.push(frame.to_string());
        Ok(())
    }
}

#[test]
fn a_display_that_misses_a_frame_still_shows_every_byte() {
    let mut sink = OledSink::with_screen(FlakyScreen::default());
    sink.set_heartbeat(false);
    for b in b"before\n" {
        sink.put(*b);
    }
    sink.screen_mut().fail_next = true;
    sink.flush();
    for b in b"after\n" {
        sink.put(*b);
    }
    sink.flush();
    // The frame that failed was never redrawn as such -- but nothing it would
    // have said was lost, because the grid, not the screen, is what remembers.
    let shown = sink.into_screen().drawn.pop().expect("something must have been drawn");
    let rows: Vec<&str> = shown.split('\n').map(|r| r.trim_end()).collect();
    // Rows 0 and 1 are the boot banner; the guest's output starts under it.
    assert_eq!(rows[2], "before");
    assert_eq!(rows[3], "after");
}

/// What `nix/guest/init.sh` prints, shaped exactly as that script shapes it —
/// the `cut -c1-16` model line, the bare 32-character hash, the `cut -c1-16`
/// name line, and the CSI escapes a tty console carries.
///
/// This is a stand-in for the guest, not a substitute for it: the test above
/// is the one that proves the claim. This one exists so that the helpers that
/// test uses are themselves exercised on every `cargo test`, rather than only
/// inside `nix develop` where the images exist. A bug in
/// `store_path_on_screen` would otherwise hide as a silent skip.
///
/// Note the `\x20` escapes: the name lines really do start with two spaces,
/// and a plain literal would lose them — Rust's `\`-newline continuation eats
/// the leading whitespace of the line that follows it. Writing them literally
/// is how this fixture first claimed the grid was dropping indentation that it
/// was not.
const SYNTHETIC_BOOT: &str = "\
[    0.000000] Linux version 6.6.0 (nixbld@localhost)\n\
[    0.512000] Run /init as init process\n\
\n\
riscv64 Linux\n\
6.6.0\n\
baochip rv64 emu\n\
\n\
/nix/store:\n\
0123456789abcdfghijklmnpqrsvwxyz\n\
\x20\x20\x1b[1;34mbusybox-1.36.1\x1b[0m\n\
zyxwvsrqpnmlkjihgfdcba9876543210\n\
\x20\x20glibc-2.40-36\n\
abcdfghijklmnpqrsvwxyz0123456789\n\
\x20\x20linux-headers-\n\
~ # ";

#[test]
fn the_helpers_the_boot_test_relies_on_find_a_store_path_on_a_real_layout() {
    let frames = screens_during_boot(SYNTHETIC_BOOT);

    for frame in &frames {
        assert_eq!(frame.len(), ROWS);
        assert!(frame.iter().all(|r| r.len() == COLS), "{frame:?}");
        for row in frame {
            assert!(!row.contains("[0m") && !row.contains("[1;"), "a CSI survived: {row:?}");
        }
    }

    let showed = |text: &str| frames.iter().any(|f| f.iter().any(|r| r.trim_end() == text));
    assert!(showed("riscv64 Linux"));
    assert!(showed("baochip rv64 emu"));
    assert!(showed("/nix/store:"));

    assert!(
        frames.iter().any(|f| (0..f.len()).any(|i| store_path_on_screen(f, i, "busybox"))),
        "the hash/name layout was not recognised: {frames:?}"
    );
    assert!(
        frames.last().unwrap().iter().any(|r| r.contains(SHELL_PROMPT)),
        "the prompt is not on the final screen"
    );
    // And the negative: a name that is not there must not be found, or the
    // assertion above would pass on anything.
    assert!(
        !frames.iter().any(|f| (0..f.len()).any(|i| store_path_on_screen(f, i, "coreutils"))),
        "store_path_on_screen matches a package that never appeared"
    );
}

/// Exactly the layout `nix/guest/init.sh` produces for one store entry, at the
/// corrected 16-column widths: the 32-character hash on two full rows, then
/// the `cut -c1-16` name on a third. **Nothing spills onto a fourth.**
///
/// This is the assertion the 18 -> 16 correction to `init.sh` exists to make
/// true. At `cut -c1-18` the name line ran two characters over, so the row
/// after every store entry opened with two stray characters instead of the
/// next hash.
///
/// The second block is the worst case a real store produces: a name long
/// enough that `cut` truncates it to exactly the full width, which is the
/// boundary an off-by-one would show up at.
#[test]
fn one_store_entry_is_exactly_three_rows_and_never_four() {
    let mut g = Grid::blank();
    g.write_str(
        "0123456789abcdfghijklmnpqrsvwxyz\n\
         \x20\x20busybox-1.36.1\n\
         zyxwvsrqpnmlkjihgfdcba9876543210\n\
         \x20\x20glibc-2.40-36\n",
    );
    assert_eq!(g.line(0), "0123456789abcdfg");
    assert_eq!(g.line(1), "hijklmnpqrsvwxyz");
    assert_eq!(g.line(2), "  busybox-1.36.1");
    assert_eq!(g.line(3), "zyxwvsrqpnmlkjih");
    assert_eq!(g.line(4), "gfdcba9876543210");
    assert_eq!(g.line(5), "  glibc-2.40-36");
    assert_eq!(g.line(6), "");

    let mut g = Grid::blank();
    g.write_str("abcdfghijklmnpqrsvwxyz0123456789\n\x20\x20linux-headers-\nnext\n");
    assert_eq!(g.line(0), "abcdfghijklmnpqr");
    assert_eq!(g.line(1), "svwxyz0123456789");
    assert_eq!(g.line(2), "  linux-headers-");
    assert_eq!(g.line(2).len(), COLS, "the widest name line must fill a row, not overflow it");
    assert_eq!(g.line(3), "next", "a full-width name line must not push the next line down");
}
