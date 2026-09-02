//! `OledSink` — the guest's console on the badge's 128x128 OLED.
//!
//! This is the third [`rv64::uart::ConsoleSink`], after `VecSink` and
//! `StdoutSink`, and the one the project exists to photograph. Guest UART
//! bytes go in one at a time; a character grid comes out and gets blitted to
//! the display.
//!
//! # Where the `cfg` boundary is, and why it is there
//!
//! Everything that decides *what the screen says* — line wrapping, scrolling,
//! control characters, ANSI stripping, tab stops, the heartbeat — is in
//! [`Grid`] and [`OledSink`], which are plain Rust and are unit-tested on the
//! laptop. The only thing below `#[cfg(target_os = "xous")]` is [`GfxScreen`],
//! whose entire job is to hand a finished string to `Gfx::draw_textview` and
//! call `Gfx::flush`. It contains no policy at all.
//!
//! That split is deliberate and it is the lesson of the transport in
//! [`crate::usbhost`]: every hardware-only bug there was a policy decision that
//! had been written below a `cfg`, where no laptop test could reach it. If you
//! find yourself adding an `if` to `GfxScreen`, it belongs up here instead.
//!
//! The seam is [`Screen`], and [`FakeScreen`] is the laptop end of it: it
//! records exactly the grid of characters the display would have shown, so a
//! test can assert the screen contents for any byte stream — including the real
//! guest's boot output, which `tests/oled_boot.rs` does.
//!
//! # 16 columns, not 18 — the glyph advance includes a kern
//!
//! **This corrects `../../docs/xous-api-notes.md` §4c**, which
//! derives an 18-column grid from `128 / 7`. Every ASCII entry of
//! `libs/blitstr2/src/fonts/mono.rs`'s `WIDTHS` really is `7`, but that is the
//! width of the *ink*, not the advance. Both the layout pass and the blit add
//! `kern` on top of it:
//!
//! * `libs/ux-api/src/wordwrap.rs:76` — `self.width += (gs.wide + gs.kern) as isize;`
//! * `libs/ux-api/src/wordwrap.rs:154` — `point.x += (glyph.wide + glyph.kern) as isize;`
//!
//! and `libs/blitstr2/src/fonts.rs:21` is `const DEFAULT_KERN: u8 = 1;`, which
//! `mono_glyph` (`fonts.rs:143-164`) uses unconditionally. So a monospace cell
//! advances **8** pixels, and `128 / 8` is [`COLS`] = 16. Nothing in-tree
//! computes a column count, so there was no counter-example to catch this;
//! `glyph_height_hint` returns the height only and there is no width call at
//! all, so it cannot be queried at runtime either.
//!
//! This is not read off the sources and hoped for. `ux-api`'s typesetter and
//! `blitstr2`'s blitter are ordinary Rust that builds for the host, so the
//! layout below was *run* — the real `Typesetter::typeset` followed by the
//! real `ComposedType::render` into a 128x128 bit buffer, with the exact
//! geometry [`GfxScreen`] uses — and these are the lit pixels it produced:
//!
//! ```text
//! glyph 'A': wide=7 kern=1 high=15 => advance=8   (same for '0', ' ', '/', '-')
//!
//! 18 chars, box 128 wide:  band y0..14  x 1..=117
//!                          band y15..29 x 1..=22     <-- wrapped, 3 chars spilled
//! 16 chars, box 128 wide:  band y0..14  x 1..=117
//!                          band y15..29 x 1..=4      <-- wrapped, 1 char spilled
//! 16 chars, box 256 wide:  band y0..14  x 1..=124    <-- fits, whole
//! 8 rows of 16, box 256 wide, height 127:
//!     bands at y 0, 15, 30, 45, 60, 75, 90, 105, each x 1..=124, overflow=false
//! ```
//!
//! The last block is exactly what [`OledSink::frame`] emits: eight rows, each
//! landing on its own 15-pixel band, each ending 3 pixels short of the right
//! edge. An eighteen-column grid does not fit and never did.
//!
//! Task 1 had reshaped the guest's output for 18, and `nix/guest/init.sh` has
//! since been corrected to match: it prints the device-tree model with
//! `cut -c1-16` (exactly [`COLS`] — a row-filling fit), the 32-character store
//! hash on a line of its own (exactly two full rows, which is *better* than 18
//! would have managed), and the package-name line with `cut -c1-16` too. As of
//! that change nothing the guest prints spills, and `tests/oled_boot.rs`
//! asserts it against the real boot rather than trusting it.
//!
//! [`ROWS`] = 8 is unchanged and confirmed: `glyph_to_height_hint(Monospace)`
//! is 15, and `wordwrap.rs:523`'s `is_newline_available` admits a ninth line
//! only if `15 + 105 + 15 < 128`, which is false.
//!
//! # Why the bounding box is wider than the screen
//!
//! [`Grid`] has already wrapped the text at [`COLS`]; the typesetter must not
//! wrap it again. Its predicates (`wordwrap.rs:526-528`) are
//!
//! ```text
//! does_word_fit_on_line:    candidate.width + cursor.x  <  bb.max.x
//! is_word_longer_than_line: candidate.width            >=  bb.max.x - bb.min.x
//! ```
//!
//! and a full 16-character row is `16 * 8 = 128` wide — which against a
//! screen-sized box (`bb.max.x == 128`) trips *both*. So [`GfxScreen`] hands
//! `TextBounds::BoundingBox` a rectangle [`TYPESET_WIDTH`] wide. The
//! typesetter then never re-wraps, while `clip_rect` — which is the screen —
//! is what actually bounds the drawing: `wordwrap.rs:146` drops any glyph
//! whose origin is past `clip_rect.br().x`, and `op::rectangle`'s iterator
//! (`op.rs:177`) filters the background fill per pixel. An over-wide box is
//! therefore clipped, not overrun, and the failure mode of a grid that is one
//! column too wide is a character quietly falling off the right edge rather
//! than the whole layout shifting down a line.
//!
//! # Every row is padded to [`COLS`], and never empty
//!
//! [`Grid::row`] returns all [`COLS`] cells including trailing spaces, and the
//! frame is those rows joined with `\n` and no trailing newline. That is not
//! cosmetic. `wordwrap.rs:567-575`'s `move_candidate_to_newline` advances by
//! `cursor.line_height`, which starts at **0** — so a leading empty line does
//! not advance `y` at all and the whole screen shifts up one row. A row that
//! always contains at least one space always establishes a line height of 15
//! before the newline that follows it, which makes row *r* land at *15r* for
//! every *r*, unconditionally.
//!
//! # Telling blank from garbled without a debugger
//!
//! Two channels, and they are complementary.
//!
//! **Panics do reach the wire.** `../../docs/xous-api-notes.md` and an earlier
//! revision of [`crate::usbhost`]'s module docs claimed otherwise — that the
//! transport's binary listen mode forecloses the USB log mirror. It does not.
//! `TryHookUsbMirror` stores a TX CID in the *log server*
//! (`services/xous-log/src/main.rs:250-293`), which is independent of
//! `usb-bao1x`'s `serial_listen_mode`; only `UnhookUsbMirror` clears it. A
//! binary park does not undo it, and a hardware run has both working at once.
//! So whoever writes this app's `main` should hook the mirror directly, the way
//! `../probe/src/main.rs`'s `try_hook_panic_mirror` does — a blocking
//! `TryHookUsbMirror` straight to the log server, explicitly *not*
//! `serial_console_input_injection()`, which additionally flips the mode to
//! `ConsoleListener` and is the part that genuinely cannot coexist with the
//! transport. Everything this module wants to say about itself goes through
//! `log::`, so it rides that mirror; nothing here writes to the port.
//!
//! **The screen answers what the mirror cannot.** A kernel panic still goes
//! only to the physical debug UART, a wedge prints nothing by definition, and
//! neither says whether the display path itself works. From a photograph:
//!
//! | what you see | what it means |
//! |---|---|
//! | nothing at all, dark screen, and a `PANIC in PID n:` on the wire | it panicked before drawing. The mirror says where |
//! | nothing at all, dark screen, wire silent | the draw path never ran: `Gfx::new` blocked or `flush` was never called. [`OledSink::new`] renders the banner before it returns, so a live app cannot show this |
//! | the banner and the ruler, unchanging | the display path works end to end and the *guest* produced nothing — look at the transport, not at this module |
//! | the ruler wrapped, or short of the right edge | [`COLS`] disagrees with the font. `f` must sit flush against the right edge with nothing spilled onto the row below |
//! | three solid bars | `draw_textview` failed [`ALERT_AFTER`] times running but `draw_rectangle` still worked — the text path specifically is broken |
//! | a spinner in the bottom-right corner | see below |
//!
//! [`OledSink::tick`] advances a spinner in the last cell of the bottom row —
//! **but only when that cell is blank**, so it can never eat a character of a
//! store path. It is the wedged-versus-panicked signal the README promises:
//! at an idle shell prompt the bottom row is short, the spinner is visible,
//! and a spinner that has stopped is a run that has stopped.

use rv64::uart::ConsoleSink;

/// Columns in the grid. See the module docs — this is `128 / 8`, where 8 is a
/// mono glyph's 7-pixel ink plus its 1-pixel kern, and *not* the `128 / 7`
/// that the API notes derive.
pub const COLS: usize = 16;

/// Rows in the grid: `128 / 15`, where 15 is `glyph_to_height_hint(Monospace)`.
pub const ROWS: usize = 8;

/// Horizontal advance of one monospace cell, in pixels: `wide` + `kern`.
pub const GLYPH_ADVANCE: isize = 8;

/// Vertical advance of one row, in pixels: `blitstr2::fonts::mono::MAX_HEIGHT`.
pub const GLYPH_HEIGHT: isize = 15;

/// The badge display, both axes (`ux_api::platform::baosec::{WIDTH, LINES}`).
pub const SCREEN: isize = 128;

/// Width handed to `TextBounds::BoundingBox`, in pixels. Anything strictly
/// greater than `COLS * GLYPH_ADVANCE` disables the typesetter's own wrapping;
/// twice the screen leaves room for a future column without re-deriving it.
pub const TYPESET_WIDTH: isize = SCREEN * 2;

/// Columns between tab stops. Tabs are expanded here rather than passed
/// through, because the typesetter renders `\t` as a *proportional* large
/// space (`wordwrap.rs:399`, 19px under `bao1x`) which would shear the grid.
pub const TAB: usize = 8;

/// Consecutive failed draws before [`Screen::alert`] paints the bars.
pub const ALERT_AFTER: u32 = 3;

/// The spinner, in order. All four are in the mono font.
const HEARTBEAT: &[u8] = b"|/-\\";

// ---------------------------------------------------------------------------
// The grid: all of the policy, none of the hardware
// ---------------------------------------------------------------------------

/// Where the escape-sequence stripper is between bytes.
///
/// The guest's console is a tty, so it can emit CSI sequences — busybox
/// colourizes, and an interactive `ash` sends more. `rv64-host`'s boot test
/// strips them from its transcript with the same rule (`tests/boot.rs`'s
/// `strip_ansi`); on a 16-column display *not* stripping them is not a
/// cosmetic problem but eight rows of `[1;34m` where the store paths should be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Esc {
    /// Ordinary text.
    Ground,
    /// An `ESC` has been seen; the next byte decides what it was.
    Seen,
    /// Inside `ESC [ ... final`, consuming parameter bytes.
    Csi,
}

/// The character grid: [`COLS`] by [`ROWS`] of ASCII, a cursor, and a dirty
/// flag. Pure — it never talks to a display, which is what makes every rule it
/// implements testable on a laptop.
pub struct Grid {
    cells: [[u8; COLS]; ROWS],
    row: usize,
    col: usize,
    dirty: bool,
    esc: Esc,
}

impl Default for Grid {
    fn default() -> Self {
        Self::new()
    }
}

impl Grid {
    /// A grid holding the boot banner.
    ///
    /// The banner is not decoration. Row 1 is a ruler of exactly [`COLS`]
    /// characters, and it is the whole reason a photograph can distinguish a
    /// wrong column count from a working display without a debugger: if `f`
    /// does not sit flush against the right edge with nothing spilled below
    /// it, [`COLS`] is wrong. See the module docs.
    pub fn new() -> Self {
        let mut g = Self {
            cells: [[b' '; COLS]; ROWS],
            row: 0,
            col: 0,
            dirty: true,
            esc: Esc::Ground,
        };
        g.write_str("rv64 emu 16x8\n");
        g.write_str("0123456789abcdef\n");
        g
    }

    /// A grid with no banner, for tests that want to assert on guest bytes
    /// alone.
    pub fn blank() -> Self {
        Self { cells: [[b' '; COLS]; ROWS], row: 0, col: 0, dirty: true, esc: Esc::Ground }
    }

    /// Feeds one byte from the guest. Never blocks, never refuses, never
    /// allocates.
    pub fn put(&mut self, b: u8) {
        self.dirty = true;
        match self.esc {
            Esc::Seen => {
                // `ESC [` opens a CSI; every other two-byte sequence is
                // swallowed whole. Same rule as `rv64-host`'s `strip_ansi`.
                self.esc = if b == b'[' { Esc::Csi } else { Esc::Ground };
                return;
            }
            Esc::Csi => {
                // Parameter and intermediate bytes are 0x20..=0x3f; the
                // sequence ends at the first final byte, 0x40..=0x7e.
                if (0x40..=0x7e).contains(&b) {
                    self.esc = Esc::Ground;
                }
                return;
            }
            Esc::Ground => {}
        }
        match b {
            0x1b => self.esc = Esc::Seen,
            b'\n' => {
                self.col = 0;
                self.newline();
            }
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                // At least one space, then on to the next tab stop. Expanding
                // here keeps the renderer free of variable-width cells.
                let stop = (self.col / TAB + 1) * TAB;
                for _ in self.col..stop.min(COLS) {
                    self.write(b' ');
                }
            }
            b if b.is_ascii_graphic() || b == b' ' => self.write(b),
            // Everything else — other C0 controls, DEL, and any byte with the
            // high bit set — is dropped. The mono font has no coverage for
            // most of it and the guest's console output is ASCII.
            _ => {}
        }
    }

    /// Places one printable byte, wrapping first if the cursor is parked past
    /// the last column.
    ///
    /// The wrap is *deferred*: writing into the last column leaves the cursor
    /// at `COLS` rather than moving to the next row, so a `\r` or a `\n` that
    /// follows a full line does not also cost a blank row. That is what a real
    /// terminal does, and it is why the guest's exactly-16-column model line
    /// does not leave a hole under itself.
    fn write(&mut self, b: u8) {
        if self.col == COLS {
            self.col = 0;
            self.newline();
        }
        self.cells[self.row][self.col] = b;
        self.col += 1;
    }

    /// Moves to the next row, scrolling the whole grid up when there is none.
    fn newline(&mut self) {
        if self.row + 1 < ROWS {
            self.row += 1;
            return;
        }
        self.cells.rotate_left(1);
        self.cells[ROWS - 1] = [b' '; COLS];
    }

    /// Feeds a whole string, byte by byte. Convenience for banners and tests.
    pub fn write_str(&mut self, s: &str) {
        for b in s.bytes() {
            self.put(b);
        }
    }

    /// Row `r` as it would read on the screen, trailing blanks removed.
    ///
    /// This is the assertion surface. The renderer uses [`Grid::row`] instead,
    /// which keeps the padding — see the module docs on why an empty row would
    /// collapse the layout.
    pub fn line(&self, r: usize) -> String {
        String::from_utf8_lossy(&self.cells[r]).trim_end().to_string()
    }

    /// Row `r`'s raw cells, all [`COLS`] of them.
    pub fn row(&self, r: usize) -> &[u8; COLS] {
        &self.cells[r]
    }

    /// Where the cursor is, as `(row, col)`. `col` may be [`COLS`], meaning a
    /// wrap is pending.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Whether anything has changed since the last call, clearing the flag.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// What went wrong in the display server. Deliberately coarse: nothing above
/// this line can do anything about the difference except count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenError {
    /// `draw_textview` failed.
    Draw,
    /// The blit to the panel failed.
    Flush,
}

/// The one thing that touches hardware.
///
/// `frame` arrives as exactly [`ROWS`] lines of exactly [`COLS`] characters,
/// joined by `\n` with no trailing newline. An implementation renders it
/// verbatim; it makes no decisions.
pub trait Screen {
    /// Paints one whole frame and makes it visible.
    fn draw(&mut self, frame: &str) -> Result<(), ScreenError>;

    /// Paints a wordless distress pattern, for when [`Screen::draw`] itself is
    /// the thing that is broken. Failure here is not reportable — this is
    /// already the fallback.
    fn alert(&mut self) {}
}

// ---------------------------------------------------------------------------
// The sink
// ---------------------------------------------------------------------------

/// A [`ConsoleSink`] that draws to a [`Screen`].
///
/// `put` only ever touches the grid, so it never blocks and never refuses; the
/// display is caught up by [`OledSink::flush`], which the run loop calls when
/// it can afford to. If the guest outruns the display the intermediate *frames*
/// are dropped, never the bytes — a screen is a view of the last [`ROWS`]
/// lines, and every byte reaches the grid that decides them.
pub struct OledSink<S: Screen> {
    grid: Grid,
    screen: S,
    /// A frame is owed. Set by the grid going dirty, by a tick, and by a draw
    /// that failed — which is what makes a failed draw retry.
    pending: bool,
    /// Spinner phase.
    beat: usize,
    heartbeat: bool,
    frames: u32,
    fails: u32,
    /// Consecutive failures, reset by a success. [`Screen::alert`] fires when
    /// this reaches [`ALERT_AFTER`], and only once per outage.
    streak: u32,
}

impl<S: Screen> OledSink<S> {
    /// Wraps a screen, and paints the banner before returning.
    ///
    /// The eager first draw is the load-bearing half of the diagnostic table
    /// in the module docs: it makes "dark screen" mean "the draw path never
    /// ran", which is otherwise indistinguishable from "the guest has not
    /// printed anything yet".
    pub fn with_screen(screen: S) -> Self {
        let mut sink = Self {
            grid: Grid::new(),
            screen,
            pending: true,
            beat: 0,
            heartbeat: true,
            frames: 0,
            fails: 0,
            streak: 0,
        };
        sink.flush();
        sink
    }

    /// Renders, if and only if something changed since the last frame.
    pub fn flush(&mut self) {
        if self.grid.take_dirty() {
            self.pending = true;
        }
        if !self.pending {
            return;
        }
        let frame = self.frame();
        match self.screen.draw(&frame) {
            Ok(()) => {
                self.pending = false;
                self.frames = self.frames.wrapping_add(1);
                self.streak = 0;
            }
            Err(e) => {
                // `pending` stays set, so the next flush tries again.
                self.fails = self.fails.wrapping_add(1);
                self.streak += 1;
                // Through `log::`, so it rides the log server's USB mirror
                // alongside panic text rather than touching the port, which
                // the transport owns. See the module docs.
                log::error!("oled: draw failed ({e:?}), {} in a row", self.streak);
                if self.streak == ALERT_AFTER {
                    log::error!("oled: falling back to the alert bars");
                    self.screen.alert();
                }
            }
        }
    }

    /// Advances the heartbeat spinner and owes a frame.
    ///
    /// Call it on a period from the run loop. A spinner that has stopped is a
    /// run that has stopped — which, with no USB log mirror available, is the
    /// only way to tell a panic from a wedge. See the module docs.
    pub fn tick(&mut self) {
        self.beat = self.beat.wrapping_add(1);
        self.pending = true;
    }

    /// Turns the spinner off, for a photograph that wants nothing moving.
    /// It never overwrites guest text either way.
    pub fn set_heartbeat(&mut self, on: bool) {
        self.heartbeat = on;
        self.pending = true;
    }

    /// The exact string the screen is showing: [`ROWS`] lines of [`COLS`]
    /// characters, `\n`-joined, no trailing newline.
    ///
    /// The heartbeat is an overlay applied here rather than a cell the grid
    /// reserves, and it is applied **only over a blank cell**. That is what
    /// keeps it from ever eating the last character of a store path, which is
    /// the one thing on this screen the project is for.
    pub fn frame(&self) -> String {
        let mut buf = Vec::with_capacity(ROWS * (COLS + 1));
        for r in 0..ROWS {
            if r > 0 {
                buf.push(b'\n');
            }
            buf.extend_from_slice(self.grid.row(r));
        }
        if self.heartbeat {
            let last = buf.len() - 1;
            if buf[last] == b' ' {
                buf[last] = HEARTBEAT[self.beat % HEARTBEAT.len()];
            }
        }
        // Lossy rather than `expect`: the grid holds only ASCII by
        // construction, and a panic on the badge has nowhere to print.
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The grid behind the screen, for tests and for the run loop's own
    /// diagnostics.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// The screen itself. Only useful when it is a recording one — see
    /// [`FakeScreen`] and `tests/oled_boot.rs`.
    pub fn screen_mut(&mut self) -> &mut S {
        &mut self.screen
    }

    /// Unwraps the screen, for a test that wants what it recorded.
    pub fn into_screen(self) -> S {
        self.screen
    }

    /// Frames drawn, and draws that failed. Nowhere to print them on the
    /// badge; they exist so a test can assert that frames really were dropped
    /// rather than bytes.
    pub fn counts(&self) -> (u32, u32) {
        (self.frames, self.fails)
    }
}

impl<S: Screen> ConsoleSink for OledSink<S> {
    fn put(&mut self, byte: u8) {
        self.grid.put(byte);
    }
}

// ---------------------------------------------------------------------------
// The laptop end of the seam
// ---------------------------------------------------------------------------

/// A [`Screen`] that records what would have been drawn.
///
/// Absent from the badge build. This is what lets a test assert the literal
/// contents of the display for a given byte stream — see `tests/oled_boot.rs`,
/// which drives the real guest's boot output through it.
#[cfg(not(target_os = "xous"))]
#[derive(Default)]
pub struct FakeScreen {
    /// Every frame drawn, each already split into its [`ROWS`] rows.
    pub frames: Vec<Vec<String>>,
    /// How many [`Screen::alert`]s were painted.
    pub alerts: u32,
    /// While set, every draw fails.
    pub failing: bool,
}

#[cfg(not(target_os = "xous"))]
impl FakeScreen {
    /// The most recent frame, trailing blanks trimmed off each row.
    pub fn last(&self) -> Vec<String> {
        self.frames
            .last()
            .map(|f| f.iter().map(|r| r.trim_end().to_string()).collect())
            .unwrap_or_default()
    }
}

#[cfg(not(target_os = "xous"))]
impl Screen for FakeScreen {
    fn draw(&mut self, frame: &str) -> Result<(), ScreenError> {
        if self.failing {
            return Err(ScreenError::Draw);
        }
        self.frames.push(frame.split('\n').map(|s| s.to_string()).collect());
        Ok(())
    }

    fn alert(&mut self) {
        self.alerts += 1;
    }
}

// ---------------------------------------------------------------------------
// The hardware end of the seam. Syscalls only — no policy below this line.
// ---------------------------------------------------------------------------

#[cfg(target_os = "xous")]
mod hw {
    use core::fmt::Write;

    use blitstr2::GlyphStyle;
    use ux_api::minigfx::*;
    use ux_api::service::api::Gid;
    use ux_api::service::gfx::Gfx;

    use super::{ScreenError, Screen, GLYPH_HEIGHT, SCREEN, TYPESET_WIDTH};

    /// The real display, via the `_Graphics_` server (`services/bao-video` on
    /// this board).
    pub struct GfxScreen {
        gfx: Gfx,
    }

    impl GfxScreen {
        /// Connects to the graphics server. Blocks until it is up, which is
        /// what `request_connection_blocking` does and what every other app
        /// on this board relies on.
        pub fn new(xns: &xous_names::XousNames) -> Result<Self, xous::Error> {
            let screen = Self { gfx: Gfx::new(xns)? };
            screen.check_metrics();
            Ok(screen)
        }

        /// Confirms, out loud, that the font agrees with [`super::COLS`].
        ///
        /// This is the one assumption in the module that no laptop test can
        /// reach: the constant is derived from `mono`'s `WIDTHS` and
        /// `DEFAULT_KERN` read out of the sources, and if a future xous-core
        /// changes either, the first symptom is a screen that wraps a row
        /// early with nothing to say why. There is no width call in the
        /// graphics API to ask at runtime (`QueryGlyphProps` returns the
        /// height only, `handlers.rs:181-191`), so ask the font directly.
        ///
        /// It logs rather than refuses. A grid that is one column out is still
        /// a readable screen — see the module docs on why an over-wide
        /// bounding box makes that failure clip rather than cascade — and a
        /// badge that will not draw at all is strictly worse than one that
        /// draws slightly wrong and says so.
        fn check_metrics(&self) {
            // Any locale that is not zh/ja/kr/en-tts takes the english rules;
            // the mono face is the same either way.
            let g = blitstr2::style_glyph("en", 'A', &GlyphStyle::Monospace);
            let advance = (g.wide + g.kern) as isize;
            if advance != super::GLYPH_ADVANCE {
                log::error!(
                    "oled: mono advance is {advance}px, not {}px -- COLS={} is wrong, \
                     the grid should be {} columns",
                    super::GLYPH_ADVANCE,
                    super::COLS,
                    SCREEN / advance.max(1),
                );
            }
            let height = self.gfx.glyph_height_hint(GlyphStyle::Monospace).unwrap_or(0) as isize;
            if height != GLYPH_HEIGHT {
                log::error!(
                    "oled: mono height is {height}px, not {GLYPH_HEIGHT}px -- ROWS={} is wrong",
                    super::ROWS,
                );
            }
        }

        /// The screen, as a clipping rectangle. `br` is inclusive, hence the
        /// `- 1` — the same expression `draw_text_view` uses when it fills in
        /// a missing `clip_rect` (`libs/ux-api/src/minigfx/handlers.rs:199`).
        fn screen_rect() -> Rectangle {
            Rectangle::new_coords(0, 0, SCREEN - 1, SCREEN - 1)
        }
    }

    impl Screen for GfxScreen {
        fn draw(&mut self, frame: &str) -> Result<(), ScreenError> {
            // The bounding box is TYPESET_WIDTH wide on purpose; `clip_rect`
            // is the screen. See the module docs — a screen-wide box would
            // re-wrap rows the grid has already wrapped.
            let mut tv = TextView::new(
                Gid::dummy(),
                TextBounds::BoundingBox(Rectangle::new_coords(
                    0,
                    0,
                    TYPESET_WIDTH,
                    SCREEN - 1,
                )),
            );
            tv.clip_rect = Some(Self::screen_rect());
            // Monospace is what makes the grid a grid: it is the only font
            // whose `WIDTHS` are uniform.
            tv.style = GlyphStyle::Monospace;
            // A border would cost two of eight rows.
            tv.draw_border = false;
            tv.border_width = 0;
            tv.rounded_border = None;
            tv.margin = Point::new(0, 0);
            // `invert` gives the fill `PixelColor::Dark`, which is the OLED's
            // unlit state -- what `services/bao-video/src/testing.rs` and
            // `apps-baosec/vault2/src/ux.rs` both do. Glyph polarity is not
            // ours to choose: `wordwrap.rs:163-169` forces it on this board.
            tv.invert = true;
            tv.clear_area = true;
            // No ellipsis: on overflow we would rather lose the last row
            // silently than spend a cell saying so.
            tv.ellipsis = false;
            tv.insertion = None;
            // `write!` on a TextView appends to `tv.text` (textview.rs:217).
            // A full frame is ROWS * (COLS + 1) - 1 = 135 bytes, well under
            // the TEXTVIEW_LEN of 3072 at which `draw_textview` truncates.
            write!(tv, "{}", frame).map_err(|_| ScreenError::Draw)?;

            self.gfx.draw_textview(&mut tv).map_err(|_| ScreenError::Draw)?;
            self.gfx.flush().map_err(|_| ScreenError::Flush)
        }

        /// Three solid bars: `draw_textview` is broken but `draw_rectangle`
        /// still works. Uses only the primitives, so it is about as likely to
        /// survive as anything can be.
        fn alert(&mut self) {
            for i in 0..3 {
                let top = GLYPH_HEIGHT * (1 + 2 * i);
                let mut bar =
                    Rectangle::new_coords(0, top, SCREEN - 1, top + GLYPH_HEIGHT);
                bar.style = DrawStyle::new(PixelColor::Light, PixelColor::Light, 1);
                self.gfx.draw_rectangle(bar).ok();
            }
            self.gfx.flush().ok();
        }
    }
}

#[cfg(target_os = "xous")]
pub use hw::GfxScreen;

/// The badge's console sink: the grid, on the real display.
#[cfg(target_os = "xous")]
pub type BadgeOled = OledSink<GfxScreen>;

#[cfg(target_os = "xous")]
impl OledSink<GfxScreen> {
    /// Connects to the graphics server and paints the banner.
    pub fn new(xns: &xous_names::XousNames) -> Result<Self, xous::Error> {
        Ok(Self::with_screen(GfxScreen::new(xns)?))
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> Grid {
        let mut g = Grid::blank();
        for b in bytes {
            g.put(*b);
        }
        g
    }

    #[test]
    fn text_wraps_at_the_column_count() {
        let g = fed(b"abcdefghijklmnopqrstuvwxyz");
        assert_eq!(g.line(0), "abcdefghijklmnop");
        assert_eq!(g.line(1), "qrstuvwxyz");
    }

    #[test]
    fn the_grid_scrolls_and_keeps_the_last_eight_lines() {
        let mut g = Grid::blank();
        for i in 0..11 {
            g.write_str(&format!("line{i}\n"));
        }
        // The twelfth is left unterminated, which is what a console that has
        // just printed looks like: the newline that would follow it has not
        // arrived, so the newest line is on the last row rather than scrolled
        // one past it.
        g.write_str("line11");
        assert_eq!(g.line(0), "line4");
        assert_eq!(g.line(7), "line11");
    }

    #[test]
    fn a_trailing_newline_leaves_the_bottom_row_blank() {
        // The other half of the rule above, asserted so it cannot drift: the
        // `\n` that ends the last line scrolls, and the guest's next output
        // will land on the blank row it left.
        let mut g = Grid::blank();
        for i in 0..12 {
            g.write_str(&format!("line{i}\n"));
        }
        assert_eq!(g.line(0), "line5");
        assert_eq!(g.line(6), "line11");
        assert_eq!(g.line(7), "");
    }

    #[test]
    fn carriage_return_does_not_advance_a_line() {
        let g = fed(b"ab\rc");
        assert_eq!(g.line(0), "cb");
    }

    #[test]
    fn backspace_moves_back_one_and_stops_at_the_margin() {
        let g = fed(b"abc\x08\x08X");
        assert_eq!(g.line(0), "aXc");
        // Backspacing past column zero must not underflow into the row above.
        let g = fed(b"\x08\x08\x08z");
        assert_eq!(g.line(0), "z");
        assert_eq!(g.cursor(), (0, 1));
    }

    #[test]
    fn a_tab_advances_to_the_next_stop_and_never_past_the_last_column() {
        let g = fed(b"ab\tc");
        assert_eq!(g.line(0), "ab      c");
        assert_eq!(g.row(0)[8], b'c');
        // A tab in the last cell fills to the edge and leaves a wrap pending
        // rather than emitting a row of spaces.
        let g = fed(b"0123456789abcde\tZ");
        assert_eq!(g.line(0), "0123456789abcde");
        assert_eq!(g.line(1), "Z");
    }

    #[test]
    fn the_wrap_is_deferred_so_a_full_line_costs_no_blank_row() {
        // Exactly COLS characters then a newline: the next row is row 1, not
        // row 2. This is the guest's `cut -c1-16` model line.
        let mut g = Grid::blank();
        g.write_str("baochip rv64 emu\nnext\n");
        assert_eq!(g.line(0), "baochip rv64 emu");
        assert_eq!(g.line(1), "next");
        assert_eq!(g.line(2), "");
    }

    #[test]
    fn csi_sequences_are_stripped_and_their_payload_never_reaches_a_cell() {
        // What busybox `ls` emits around a colourized directory name.
        let g = fed(b"\x1b[1;34mbusybox\x1b[0m ok");
        assert_eq!(g.line(0), "busybox ok");
    }

    #[test]
    fn a_two_byte_escape_swallows_exactly_its_second_byte() {
        let g = fed(b"a\x1b(Bb");
        assert_eq!(g.line(0), "aBb");
    }

    #[test]
    fn bytes_the_font_cannot_render_are_dropped_not_mangled() {
        let g = fed(b"a\x00\x07\x7f\xc3\xa9b");
        assert_eq!(g.line(0), "ab");
    }

    #[test]
    fn a_thirty_two_character_hash_lands_on_exactly_two_rows() {
        // The money shot. `nix/guest/init.sh` prints the store hash on a line
        // of its own precisely so this is legible.
        let mut g = Grid::blank();
        g.write_str("1abcdefghijklmnpqrsvwxyz0123456f\n  busybox-1.36.1\n");
        assert_eq!(g.line(0), "1abcdefghijklmnp");
        assert_eq!(g.line(1), "qrsvwxyz0123456f");
        // And the name line under it, `echo "  ${n#*-}" | cut -c1-16`, which
        // cannot exceed a row.
        assert_eq!(g.line(2), "  busybox-1.36.1");
        assert_eq!(g.line(3), "");
    }

    #[test]
    fn a_line_longer_than_the_grid_spills_rather_than_truncating() {
        // Nothing the guest prints is this wide any more -- `init.sh` cuts
        // every line to COLS. This pins what would happen if something were,
        // because the answer matters: the overflow lands on the next row and
        // stays readable, rather than being dropped or pushing the layout out
        // of alignment.
        let mut g = Grid::blank();
        g.write_str("  busybox-1.36.1-bin\n");
        assert_eq!(g.line(0), "  busybox-1.36.1");
        assert_eq!(g.line(1), "-bin");
    }

    #[test]
    fn every_rendered_row_is_padded_to_the_full_width() {
        // Not cosmetic: an empty row would collapse the typesetter's layout.
        // See the module docs.
        let mut sink = OledSink::with_screen(FakeScreen::default());
        sink.set_heartbeat(false);
        sink.grid_mut_for_test().write_str("hi\n");
        sink.flush();
        let frame = sink.frame();
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(rows.len(), ROWS);
        assert!(rows.iter().all(|r| r.len() == COLS), "{rows:?}");
        assert!(!frame.ends_with('\n'));
    }

    #[test]
    fn the_banner_ruler_is_exactly_one_full_row() {
        let g = Grid::new();
        assert_eq!(g.line(1).len(), COLS);
        assert_eq!(g.line(1), "0123456789abcdef");
        // If it had spilled, `f` would be on row 2 instead.
        assert_eq!(g.line(2), "");
    }

    #[test]
    fn the_banner_is_drawn_before_the_constructor_returns() {
        // "Dark screen" has to mean "the draw path never ran".
        let sink = OledSink::with_screen(FakeScreen::default());
        assert_eq!(sink.counts(), (1, 0));
        assert_eq!(sink.screen_for_test().last()[1], "0123456789abcdef");
    }

    #[test]
    fn frames_are_dropped_but_bytes_are_not() {
        let mut sink = OledSink::with_screen(FakeScreen::default());
        let (before, _) = sink.counts();
        for i in 0..11 {
            for b in format!("line{i}\n").bytes() {
                sink.put(b);
            }
        }
        for b in b"line11" {
            sink.put(*b);
        }
        sink.flush();
        // One frame for twelve lines of output -- eleven frames' worth dropped.
        assert_eq!(sink.counts().0, before + 1);
        // And not one byte lost: the grid holds the last eight lines.
        assert_eq!(sink.grid().line(0), "line4");
        assert_eq!(sink.grid().line(7), "line11");
    }

    #[test]
    fn an_unchanged_grid_costs_no_frame() {
        let mut sink = OledSink::with_screen(FakeScreen::default());
        let (before, _) = sink.counts();
        sink.flush();
        sink.flush();
        assert_eq!(sink.counts().0, before);
    }

    #[test]
    fn a_failed_draw_is_retried_and_then_raises_the_alert() {
        let screen = FakeScreen { failing: true, ..Default::default() };
        let mut sink = OledSink::with_screen(screen);
        // `with_screen` already burned one attempt.
        sink.flush();
        sink.flush();
        assert_eq!(sink.counts(), (0, ALERT_AFTER));
        assert_eq!(sink.screen_for_test().alerts, 1);
        // Once only, however long the outage lasts.
        sink.flush();
        assert_eq!(sink.screen_for_test().alerts, 1);
        // And a recovery draws the frame that was owed all along.
        sink.screen_for_test_mut().failing = false;
        sink.flush();
        assert_eq!(sink.counts().0, 1);
        assert_eq!(sink.screen_for_test().last()[0], "rv64 emu 16x8");
    }

    #[test]
    fn the_heartbeat_advances_and_forces_a_frame() {
        let mut sink = OledSink::with_screen(FakeScreen::default());
        let first = sink.frame();
        sink.tick();
        sink.flush();
        let second = sink.frame();
        assert_ne!(first, second);
        assert_eq!(first.as_bytes()[first.len() - 1], b'|');
        assert_eq!(second.as_bytes()[second.len() - 1], b'/');
    }

    #[test]
    fn the_heartbeat_never_overwrites_guest_text() {
        let mut sink = OledSink::with_screen(FakeScreen::default());
        // Fill the bottom row edge to edge. The last line is unterminated, or
        // its newline would scroll a blank row into place under it.
        for _ in 0..ROWS {
            sink.grid_mut_for_test().write_str("0123456789abcdef\n");
        }
        sink.grid_mut_for_test().write_str("0123456789abcdef");
        sink.tick();
        let frame = sink.frame();
        assert_eq!(frame.as_bytes()[frame.len() - 1], b'f');
        // It comes back as soon as the cell is free again.
        sink.grid_mut_for_test().write_str("~ # \n");
        let frame = sink.frame();
        assert_eq!(frame.as_bytes()[frame.len() - 1], b'/');
    }

    #[test]
    fn the_frame_fits_the_screen_it_was_sized_for() {
        // The two facts the whole layout rests on, asserted rather than
        // trusted: a full row of glyph advances fits across, and ROWS rows of
        // glyph heights fit down.
        assert!(COLS as isize * GLYPH_ADVANCE <= SCREEN);
        assert!(ROWS as isize * GLYPH_HEIGHT <= SCREEN);
        // One more column would not fit -- this is the *maximum* grid.
        assert!((COLS as isize + 1) * GLYPH_ADVANCE > SCREEN);
        // And the typeset box must clear a full row, or the typesetter wraps
        // rows the grid has already wrapped.
        assert!(TYPESET_WIDTH > COLS as isize * GLYPH_ADVANCE);
    }

    // Test-only accessors. Kept here rather than on the public type so the
    // badge build carries no way to reach past the sink into its own grid.
    impl<S: Screen> OledSink<S> {
        fn grid_mut_for_test(&mut self) -> &mut Grid {
            &mut self.grid
        }
        fn screen_for_test(&self) -> &S {
            &self.screen
        }
        fn screen_for_test_mut(&mut self) -> &mut S {
            &mut self.screen
        }
    }
}
