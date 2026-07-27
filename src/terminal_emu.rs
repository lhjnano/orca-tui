//! # Terminal emulation
//!
//! The single largest piece of bespoke implementation: turn an agent process's
//! raw PTY byte stream into a styled cell grid (the "virtual terminal").
//!
//! Layering:
//! ```text
//! agent PTY bytes ──▶ portable_pty ──▶ vt100 (ANSI parse + state) ──▶ Cell grid
//!   (cursor, styles, scrollback) ──▶ [Task 3] ratatui Buffer copy ──▶ crossterm
//! ```
//!
//! This module owns the `vt100 → owned cell grid` half. It is deliberately
//! **independent of ratatui** so it can be unit-tested in isolation. The
//! vt100-cell → ratatui-`Buffer` bridge lives in Task 3 (the `Pane` widget),
//! not here — we expose a portable [`EmuCell`] / [`EmuColor`] surface instead.
//!
//! ## API conventions
//!
//! The public surface uses **(col, row)** ordering to match ratatui's `(x, y)`
//! model (a `Pane` will index `grid()[row][col]` while positioning cells at
//! `Rect { x: col, y: row }`). Internally vt100 uses `(row, col)`, so the
//! ordering is translated at the boundary.

/// Minimum column count ever handed to the emulator.
///
/// `vt100` computes `scroll_bottom = size.rows - 1` (`grid.rs:26`) and uses
/// `size.cols - 1` / `size.cols - 2` throughout, so a `0` width or height
/// underflows and panics. We never let a degenerate PTY size (e.g. a freshly
/// spawned PTY reporting `0` before the terminal settles) reach vt100.
///
/// The floor is **2**, not 1: at `cols = 1` the line-wrap check
/// `pos.col > size.cols - width` underflows for a double-width glyph
/// (`grid.rs:668`), and at `rows = 1` the scroll-on-wrap `prev_pos.row -=
/// scrolled` underflows (`grid.rs:672`). 2×2 is the smallest size at which
/// `vt100` is internally consistent.
pub const MIN_COLS: u16 = 2;
/// Minimum row count ever handed to the emulator. See [`MIN_COLS`].
pub const MIN_ROWS: u16 = 2;

/// Portable foreground/background color mirroring vt100's color model.
///
/// Decoupled from `ratatui::style::Color` so this module has no ratatui
/// dependency and is unit-testable on its own. Task 3 will map this onto
/// `ratatui::style::Color` when painting the `Buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmuColor {
    /// The terminal's default color (foreground or background).
    #[default]
    Default,
    /// A 256-color-palette index (`0..=255`).
    Indexed(u8),
    /// A 24-bit true-color value.
    Rgb(u8, u8, u8),
}

/// Style flags copied out of a vt100 cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmuStyle {
    /// Bold / increased-intensity (`\x1b[1m`).
    pub bold: bool,
    /// Italic (`\x1b[3m`).
    pub italic: bool,
    /// Underline (`\x1b[4m`).
    pub underline: bool,
    /// Inverse / reverse video (`\x1b[7m`).
    pub inverse: bool,
}

/// One terminal cell, value-copied from a [`vt100::Cell`].
///
/// Owned (not a borrow) so a `Pane` can cache/snapshot a frame without holding
/// a borrow on the emulator across the ratatui render call. An unwritten /
/// blank cell has an empty `chars` string and all-default colors/style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmuCell {
    /// The printable glyph(s) in the cell. May include combining marks (a
    /// vt100 cell holds up to 6 codepoints), but at most one of them has a
    /// non-zero display width.
    pub chars: String,
    /// Foreground color.
    pub fg: EmuColor,
    /// Background color.
    pub bg: EmuColor,
    /// Text style flags.
    pub style: EmuStyle,
    /// Whether the primary glyph is a double-width (CJK / emoji) character
    /// that occupies two columns. The following column is a wide-continuation
    /// placeholder that a renderer should leave blank.
    pub wide: bool,
}

impl EmuCell {
    /// Returns `true` if the cell holds any printed content (a space counts).
    #[must_use]
    pub fn has_contents(&self) -> bool {
        !self.chars.is_empty()
    }
}

/// A virtual terminal: parses raw PTY bytes into a styled cell grid.
///
/// Wraps a [`vt100::Parser`], owning cursor position, styles, alternate-screen
/// state and a configurable scrollback buffer. Cheap to clone-by-rebuilding
/// but intended to be owned long-term by a `Pane`.
pub struct TerminalEmulator {
    parser: vt100::Parser,
}

impl TerminalEmulator {
    /// Create a new emulator.
    ///
    /// `scrollback` is the number of scrolled-off lines retained above the
    /// visible viewport (0 disables scrollback; ~1000 is a sensible default
    /// for an agent pane).
    ///
    /// `cols`/`rows` are clamped to at least [`MIN_COLS`]/[`MIN_ROWS`] so a
    /// degenerate PTY size (`0`) cannot reach `vt100` (which underflows at
    /// `scroll_bottom = size.rows - 1`).
    #[must_use]
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        Self {
            // vt100::Parser::new takes (rows, cols) — note the order.
            parser: vt100::Parser::new(rows, cols, scrollback),
        }
    }

    /// Feed a chunk of raw PTY bytes (ANSI escape sequences + printable text)
    /// into the emulator, updating its in-memory cell grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the terminal. `cols` × `rows` is the new visible viewport; the
    /// scrollback is preserved. Existing content is reflowed by vt100 and may
    /// be truncated if the viewport shrinks.
    ///
    /// Dimensions are clamped to at least [`MIN_COLS`]/[`MIN_ROWS`] — a
    /// degenerate resize to `0` cannot reach `vt100`.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        // vt100 uses (rows, cols) order.
        self.parser.set_size(rows, cols);
    }

    /// Scroll the viewport `offset` lines back into the scrollback history.
    ///
    /// `offset = 0` shows the latest (normal) screen; `offset = N` shifts the
    /// view up by `N` history lines so [`grid`](Self::grid)/[`cell`](Self::cell)
    /// return the older content. The value is **clamped internally** by vt100
    /// to the number of history lines actually stored, so passing a value
    /// larger than the available history just lands on the oldest line (never
    /// panics). This does NOT affect [`feed`](Self::feed) — new bytes still
    /// append to the live screen regardless of the scroll offset.
    ///
    /// This is what wires mouse-wheel scrollback to the rendered view: a `Pane`
    /// calls this with its `scroll` cursor before snapshotting [`grid`](Self::grid).
    pub fn set_scroll(&mut self, offset: usize) {
        self.parser.set_scrollback(offset);
    }

    /// Returns `(cols, rows)` — the current viewport size.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols, rows)
    }

    /// Read the cell at `(col, row)`, or `None` if out of bounds.
    ///
    /// Returns an [`EmuCell`] value-copy. A blank/never-written cell yields a
    /// default `EmuCell` (empty `chars`).
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<EmuCell> {
        // vt100::Screen::cell takes (row, col).
        self.parser.screen().cell(row, col).map(convert_cell)
    }

    /// Snapshot the entire visible grid as `grid[row][col]` (row-major, i.e.
    /// `grid[0]` is the top visible line).
    ///
    /// Allocates a fresh `Vec` every call — a `Pane` should cache this and
    /// only re-snapshot when new PTY bytes arrive or the viewport resizes.
    #[must_use]
    pub fn grid(&self) -> Vec<Vec<EmuCell>> {
        let (rows, cols) = self.parser.screen().size();
        let mut out = Vec::with_capacity(usize::from(rows));
        for row in 0..rows {
            let mut line = Vec::with_capacity(usize::from(cols));
            for col in 0..cols {
                line.push(
                    self.parser
                        .screen()
                        .cell(row, col)
                        .map(convert_cell)
                        .unwrap_or_default(),
                );
            }
            out.push(line);
        }
        out
    }

    /// The cursor position as `(col, row)`, or `None` if the agent has hidden
    /// the cursor (ESC[?25l). Used by the pane to render a visible cursor.
    #[must_use]
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        if screen.hide_cursor() {
            return None;
        }
        let (row, col) = screen.cursor_position();
        Some((col, row))
    }
}

/// Convert a borrowed vt100 cell into an owned [`EmuCell`] value-copy.
fn convert_cell(cell: &vt100::Cell) -> EmuCell {
    EmuCell {
        chars: cell.contents(),
        fg: convert_color(cell.fgcolor()),
        bg: convert_color(cell.bgcolor()),
        style: EmuStyle {
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        },
        wide: cell.is_wide(),
    }
}

/// Map vt100's color enum onto our ratatui-free [`EmuColor`].
fn convert_color(color: vt100::Color) -> EmuColor {
    match color {
        vt100::Color::Default => EmuColor::Default,
        vt100::Color::Idx(i) => EmuColor::Indexed(i),
        vt100::Color::Rgb(r, g, b) => EmuColor::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: assert a cell's primary glyph is the expected single char.
    fn assert_char(emu: &TerminalEmulator, col: u16, row: u16, expected: char) {
        let cell = emu
            .cell(col, row)
            .unwrap_or_else(|| panic!("cell ({col},{row}) out of bounds"));
        assert_eq!(
            cell.chars,
            expected.to_string(),
            "at ({col},{row}) expected '{expected}' got {:?}",
            cell.chars
        );
    }

    #[test]
    fn red_style_then_reset() {
        // 20 cols × 2 rows. "\x1b[31m" = fg red (palette idx 1), "\x1b[0m" = reset.
        let mut emu = TerminalEmulator::new(20, 2, 0);
        emu.feed(b"\x1b[31mred\x1b[0m text");

        // "red" should be palette-index 1 (red).
        for (col, ch) in ['r', 'e', 'd'].into_iter().enumerate() {
            let cell = emu.cell(col as u16, 0).unwrap();
            assert_eq!(cell.chars, ch.to_string(), "glyph at col {col}");
            assert_eq!(cell.fg, EmuColor::Indexed(1), "fg at col {col}");
        }
        // " text" after the reset — space + 4 chars, all default fg.
        let after = [' ', 't', 'e', 'x', 't'];
        for (i, ch) in after.into_iter().enumerate() {
            let col = 3 + i as u16;
            let cell = emu.cell(col, 0).unwrap();
            assert_eq!(cell.chars, ch.to_string(), "glyph at col {col}");
            assert_eq!(cell.fg, EmuColor::Default, "fg at col {col} after reset");
        }
    }

    #[test]
    fn newline_and_cursor_positioning() {
        // CRLF moves to the start of the next line.
        let mut emu = TerminalEmulator::new(20, 3, 0);
        emu.feed(b"hello\r\nworld");
        assert_char(&emu, 0, 0, 'h');
        assert_char(&emu, 4, 0, 'o');
        assert_char(&emu, 0, 1, 'w');
        assert_char(&emu, 4, 1, 'd');

        // Cursor-position escape "\x1b[r;cH" is 1-indexed: row;col.
        let mut emu = TerminalEmulator::new(20, 3, 0);
        emu.feed(b"\x1b[2;5HXY"); // row 2, col 5
        assert_char(&emu, 4, 1, 'X'); // 0-indexed col 4 == 1-indexed col 5
        assert_char(&emu, 5, 1, 'Y');
        // Row 0 should be untouched / blank.
        assert!(!emu.cell(0, 0).unwrap().has_contents());
    }

    #[test]
    fn empty_grid_is_all_defaults() {
        let emu = TerminalEmulator::new(80, 24, 0);
        assert_eq!(emu.size(), (80, 24));

        let grid = emu.grid();
        assert_eq!(grid.len(), 24, "row count");
        assert_eq!(grid[0].len(), 80, "col count");

        for (row_idx, row) in grid.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate() {
                assert!(cell.chars.is_empty(), "({col_idx},{row_idx}) not blank");
                assert_eq!(cell.fg, EmuColor::Default);
                assert_eq!(cell.bg, EmuColor::Default);
                assert_eq!(cell.style, EmuStyle::default());
                assert!(!cell.wide);
            }
        }
    }

    #[test]
    fn resize_updates_size_without_panic() {
        let mut emu = TerminalEmulator::new(30, 5, 0);
        emu.feed(b"hello world");
        assert_eq!(emu.size(), (30, 5));

        // Shrink — must not panic and size must reflect new dimensions.
        emu.resize(10, 2);
        assert_eq!(emu.size(), (10, 2));
        let grid = emu.grid();
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0].len(), 10);

        // Grow back.
        emu.resize(40, 8);
        assert_eq!(emu.size(), (40, 8));
    }

    #[test]
    fn bold_and_underline_style_flags() {
        let mut emu = TerminalEmulator::new(20, 1, 0);
        // "\x1b[1;4m" = bold + underline.
        emu.feed(b"\x1b[1;4mX");
        let cell = emu.cell(0, 0).unwrap();
        assert_eq!(cell.chars, "X");
        assert!(cell.style.bold, "bold");
        assert!(cell.style.underline, "underline");
        assert!(!cell.style.italic);
        assert!(!cell.style.inverse);
    }

    /// Regression guard for the real-PTY panic: a degenerate PTY may report a
    /// `0` size at spawn time, which made `vt100` underflow at
    /// `scroll_bottom = size.rows - 1` (`grid.rs:26`). Every dimension that
    /// reaches the emulator must be clamped to the safe minimum first.
    #[test]
    fn degenerate_sizes_clamp_instead_of_panicking() {
        // Construction with (0,0) must not panic and must report the clamped
        // minimum (2×2 — see MIN_COLS/MIN_ROWS rationale).
        let mut emu = TerminalEmulator::new(0, 0, 1000);
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));

        // Feeding wrapping text into a clamped-tiny emulator must not panic
        // either (vt100 has `cols - width` / `prev_pos.row -= scrolled` sites
        // that underflow at 1×1, which is exactly why the floor is 2×2).
        emu.feed(b"\x1b[31mhello world this wraps\r\nsecond line\x1b[0m");
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));

        // resize(0, 0) must clamp, not underflow.
        emu.resize(0, 0);
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));

        // resize(1, 1) is below the floor, so it also clamps to the minimum.
        emu.resize(1, 1);
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));

        // Grow back out of the degenerate state.
        emu.resize(40, 8);
        assert_eq!(emu.size(), (40, 8));
    }

    /// The minimum (2×2) must be genuinely safe for wrapping text and a
    /// double-width glyph — not just that `0` clamps to it.
    #[test]
    fn minimum_size_is_safe_for_wrapping_and_wide_chars() {
        let mut emu = TerminalEmulator::new(MIN_COLS, MIN_ROWS, 1000);
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));
        // Printable text long enough to force a line wrap at the floor.
        emu.feed(b"abcdefgh");
        // A double-width glyph (中, U+4E2D): exercises `size.cols - width`.
        emu.feed("中".as_bytes());
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));

        // Same via resize into the floor from a larger size.
        emu.resize(40, 8);
        emu.feed(b"0123456789");
        emu.resize(MIN_COLS, MIN_ROWS);
        emu.feed(b"xy");
        assert_eq!(emu.size(), (MIN_COLS, MIN_ROWS));
    }

    /// DIAGNOSTIC (ignored): feed opencode's real captured PTY output into the
    /// emulator and report whether vt100 turns it into cells. Run with:
    ///   cargo test diag_opencode -- --nocapture --ignored
    /// High non-empty cell count + logo glyphs ⇒ the emulator CAN render
    /// opencode (so the blank pane is a plumbing/timing bug). Near-zero ⇒ vt100
    /// 0.15 can't emulate opencode's drawing sequences (an emulation gap).
    #[test]
    #[ignore]
    fn diag_opencode_renders_in_emulator() {
        let bytes = std::fs::read("/tmp/oc_solo.txt").unwrap_or_default();
        let mut emu = TerminalEmulator::new(120, 30, 1000);
        emu.feed(&bytes);
        let grid = emu.grid();
        let nonempty: usize = grid
            .iter()
            .map(|r| r.iter().filter(|c| c.has_contents()).count())
            .sum();
        let has_logo = grid.iter().any(|r| {
            r.iter()
                .any(|c| c.chars.contains('█') || c.chars.contains('▀') || c.chars.contains('▄'))
        });
        let has_ask = grid.iter().any(|r| {
            r.iter()
                .any(|c| c.chars.contains("Ask") || c.chars.contains("anythin"))
        });
        eprintln!(
            "DIAG opencode: {} bytes fed, {} non-empty cells, logo_glyphs={}, ask_prompt={}",
            bytes.len(),
            nonempty,
            has_logo,
            has_ask
        );
    }

    /// DIAGNOSTIC (ignored): does `grid()` return the ALTERNATE screen after
    /// `ESC[?1049h`? If row0 shows the alt marker, alt screen is visible; if it
    /// shows the main-screen text or is blank, vt100 0.15's `screen()` returns
    /// the main screen while the agent drew on alt — the blank-pane cause.
    ///   cargo test diag_alt_screen -- --nocapture --ignored
    #[test]
    #[ignore]
    fn diag_alt_screen_visible() {
        let mut emu = TerminalEmulator::new(40, 5, 0);
        emu.feed(b"MAIN-TEXT");
        emu.feed(b"\x1b[?1049h"); // enter alternate screen
        emu.feed(b"\x1b[2J\x1b[H"); // clear + cursor home
        emu.feed(b"ALT-MARKER-XYZ");
        let grid = emu.grid();
        let row0: String = grid[0]
            .iter()
            .map(|c| c.chars.chars().next().unwrap_or(' '))
            .collect();
        let joined: String = grid
            .iter()
            .flat_map(|r| r.iter().map(|c| c.chars.chars().next().unwrap_or(' ')))
            .collect();
        eprintln!(
            "DIAG alt: row0={:?} has_marker={} has_main={}",
            row0,
            joined.contains("ALT-MARKER-XYZ"),
            joined.contains("MAIN-TEXT")
        );
    }

    #[test]
    fn set_scroll_shifts_the_view_into_history() {
        // 10 cols × 3 rows, 50 lines of scrollback. Write 6 lines so the first
        // three (AAA, BBB, CCC) scroll off into history and the viewport shows
        // the latest three (DDD, EEE, FFF). Each line uses "\r\n" so the cursor
        // returns to column 0 and advances a row (scrolling when at the bottom).
        let mut emu = TerminalEmulator::new(10, 3, 50);
        emu.feed(b"AAA\r\nBBB\r\nCCC\r\nDDD\r\nEEE\r\nFFF");

        // Latest view (scroll = 0): DDD / EEE / FFF.
        assert_eq!(emu.cell(0, 0).unwrap().chars, "D");
        assert_eq!(emu.cell(0, 1).unwrap().chars, "E");
        assert_eq!(emu.cell(0, 2).unwrap().chars, "F");

        // Scroll back 3 lines → the oldest history (AAA / BBB / CCC).
        emu.set_scroll(3);
        assert_eq!(emu.cell(0, 0).unwrap().chars, "A", "scroll=3 row0");
        assert_eq!(emu.cell(0, 1).unwrap().chars, "B", "scroll=3 row1");
        assert_eq!(emu.cell(0, 2).unwrap().chars, "C", "scroll=3 row2");

        // Scroll back 1 line → the boundary (CCC / DDD / EEE).
        emu.set_scroll(1);
        assert_eq!(emu.cell(0, 0).unwrap().chars, "C", "scroll=1 row0");
        assert_eq!(emu.cell(0, 1).unwrap().chars, "D", "scroll=1 row1");
        assert_eq!(emu.cell(0, 2).unwrap().chars, "E", "scroll=1 row2");

        // Back to latest (scroll = 0) restores the live screen.
        emu.set_scroll(0);
        assert_eq!(
            emu.cell(0, 2).unwrap().chars,
            "F",
            "scroll=0 back to latest"
        );
    }

    #[test]
    fn set_scroll_clamps_beyond_available_history() {
        // 3 rows, 50 scrollback, only 2 lines of history available. Asking for
        // a huge offset must clamp to the oldest line rather than panicking or
        // reading out of bounds.
        let mut emu = TerminalEmulator::new(10, 3, 50);
        emu.feed(b"AAA\r\nBBB\r\nCCC\r\nDDD");
        // History = AAA (1 line), live = BBB / CCC / DDD.
        emu.set_scroll(usize::MAX);
        assert_eq!(
            emu.cell(0, 0).unwrap().chars,
            "A",
            "clamped to the oldest line"
        );
    }

    #[test]
    fn set_scroll_does_not_affect_subsequent_feed() {
        // Scrolling back must not corrupt the live screen: feeding new bytes
        // still lands on the normal screen (vt015 guarantees process() is
        // independent of the scroll offset).
        let mut emu = TerminalEmulator::new(10, 3, 50);
        emu.feed(b"AAA\r\nBBB\r\nCCC\r\nDDD");
        // Live = BBB / CCC / DDD (AAA in history); cursor sits right after "DDD".
        emu.set_scroll(1);
        emu.feed(b"X"); // appended at the live cursor (row 2, col 3) → "DDDX"
                        // Reset to the latest view and check the live screen kept DDD + the
                        // just-fed 'X'.
        emu.set_scroll(0);
        assert_eq!(emu.cell(0, 2).unwrap().chars, "D", "live row preserved");
        assert_eq!(
            emu.cell(3, 2).unwrap().chars,
            "X",
            "fed byte landed on the live screen"
        );
    }
}
