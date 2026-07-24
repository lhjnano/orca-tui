//! # Pane
//!
//! One agent == one pane. A [`Pane`] owns the terminal-emulation state
//! ([`crate::terminal_emu::TerminalEmulator`]), agent metadata, a scroll cursor
//! and the visible window of its output. It is the unit of focus, resize and
//! independent scroll.
//!
//! The render path converts the emulator's ratatui-free `EmuCell` grid into a
//! directly-painted [`ratatui::buffer::Buffer`] region — the vt100→ratatui
//! bridge (Feature 1 render half of the roadmap).

use std::fmt;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{BorderType, Borders};
use ratatui::Frame;

use crate::agent::AgentState;
use crate::config::ThemeConfig;
use crate::terminal_emu::{EmuCell, EmuColor, EmuStyle, TerminalEmulator, MIN_COLS, MIN_ROWS};
use ratatui_ppalla::prepared::{
    BlockSpec, LayoutCtx, Preparable, PreparedBlock, PreparedBlockState,
};

/// Scrollback lines retained above the viewport inside the emulator.
const SCROLLBACK: usize = 1000;

/// Default foreground for an emulator `Default` color (light gray on a dark
/// terminal).
const DEFAULT_FG: Color = Color::Gray;

/// One agent pane: emulator + metadata + scroll cursor.
///
/// A manual `Debug` impl avoids requiring `TerminalEmulator: Debug`.
pub struct Pane {
    id: usize,
    name: String,
    emu: TerminalEmulator,
    state: AgentState,
    branch: Option<String>,
    /// Lines scrolled back from the bottom (0 = pinned to the latest line).
    scroll: usize,
    /// OSC 9999 scanner — intercepts agent status from the PTY byte stream.
    osc_scanner: crate::osc::OscScanner,
    /// Synchronized-output (mode 2026) batcher — buffers `ESC[?2026h…2026l`
    /// redraw batches and flushes them atomically so the emulator only ever
    /// holds complete frames (prevents the mid-batch "cleared" frame from
    /// rendering as a blank pane, e.g. for opencode/OpenTUI).
    sync_scanner: crate::sync::SyncScanner,
    /// Last captured agent activity (from OSC 9999), if any.
    activity: Option<crate::osc::AgentActivity>,
    /// Terminal-capability query responder — detects OSC color / DECRQM / DA /
    /// DCS queries in the PTY stream and produces the replies a probing agent
    /// (opencode/​OpenTUI) waits for so it renders instead of going blank.
    query_responder: crate::query::QueryResponder,
    /// ppalla cached border spec (PreparedBlock). The border/title glyph
    /// placement is cached keyed on (w, h, border_type, borders, title); the
    /// border/title *style* (focus color) is applied at paint time and is
    /// excluded from the cache key, so toggling focus never invalidates it.
    block: PreparedBlockState,
    /// Last block title rendered, to detect header changes (the only thing that
    /// can invalidate the block layout cache besides a resize).
    last_title: String,
}

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("emu_size", &self.emu.size())
            .field("state", &self.state)
            .field("branch", &self.branch)
            .field("scroll", &self.scroll)
            .finish()
    }
}

impl Pane {
    /// Create a new pane with an idle agent and the default scrollback.
    ///
    /// Dimensions are clamped to the emulator-safe minimum ([`MIN_COLS`] /
    /// [`MIN_ROWS`]) so a degenerate spawn size cannot reach `vt100`.
    #[must_use]
    pub fn new(id: usize, name: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            id,
            name: name.into(),
            emu: TerminalEmulator::new(cols.max(MIN_COLS), rows.max(MIN_ROWS), SCROLLBACK),
            state: AgentState::Idle,
            branch: None,
            scroll: 0,
            osc_scanner: crate::osc::OscScanner::new(),
            sync_scanner: crate::sync::SyncScanner::new(),
            activity: None,
            query_responder: crate::query::QueryResponder::new(),
            block: PreparedBlockState::default(),
            last_title: String::new(),
        }
    }

    /// Stable pane identifier.
    #[must_use]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Display name shown in the header.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Feed raw PTY bytes into the emulator. OSC 9999 agent-status sequences
    /// are intercepted by the [`OscScanner`](crate::osc::OscScanner) before
    /// reaching the terminal emulator, so they don't corrupt the display and
    /// their structured payload is captured as [`activity`](Pane::activity).
    pub fn feed(&mut self, bytes: &[u8]) {
        let (clean, activities) = self.osc_scanner.process(bytes);
        if let Some(act) = activities.into_iter().last() {
            self.activity = Some(act);
        }
        // Buffer synchronized-output (mode 2026) batches and flush them
        // atomically so the emulator only ever holds complete frames — a render
        // that fires mid-batch sees the previous frame, not the half-cleared one.
        let batched = self.sync_scanner.process(&clean);
        self.emu.feed(&batched);
    }

    /// Resize the emulator viewport (PTY-side resize is Task 4's job).
    ///
    /// Dimensions are clamped to the emulator-safe minimum so a degenerate
    /// render area (e.g. a pane starved by the footer on a 1-line terminal)
    /// cannot reach `vt100`.
    pub fn resize_viewport(&mut self, cols: u16, rows: u16) {
        self.emu.resize(cols.max(MIN_COLS), rows.max(MIN_ROWS));
    }

    /// Current `(cols, rows)` viewport size.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.emu.size()
    }

    /// Replace the agent lifecycle state.
    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
    }

    /// Current agent state.
    #[must_use]
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Set the branch / cwd label shown in the header.
    pub fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    /// The branch / cwd label currently shown in the header (for snapshots).
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// The last OSC 9999 agent activity captured from the PTY stream, if any.
    /// This carries structured status (state, tool, prompt) that agents like
    /// Claude Code emit — the data source for the sidebar's per-agent summary.
    #[must_use]
    pub fn activity(&self) -> Option<&crate::osc::AgentActivity> {
        self.activity.as_ref()
    }

    /// Scan a chunk of PTY bytes for terminal-capability queries (OSC 10/11/12
    /// color, DECRQM private modes, DA1/DA2, DCS terminfo) and return the
    /// synthesized responses to write back to the agent. Probing agents
    /// (opencode/OpenTUI) block until these are answered; without replies they
    /// render blank. Does not consume the bytes — feed them to [`Pane::feed`]
    /// as usual.
    #[must_use]
    pub fn scan_queries(&mut self, bytes: &[u8], theme: &crate::config::ThemeConfig) -> Vec<u8> {
        self.query_responder.process(bytes, theme)
    }

    /// Scroll back `n` lines (towards older output). Saturates; never panics.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_add(n);
    }

    /// Scroll forward `n` lines (towards newer output). Clamps at 0.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Reset scroll to the latest line.
    pub fn scroll_reset(&mut self) {
        self.scroll = 0;
    }

    /// Current scroll offset (lines from the bottom). 0 = latest.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// A read-only view of the underlying emulator (for snapshot tests / Task 4).
    #[must_use]
    pub fn emulator(&self) -> &TerminalEmulator {
        &self.emu
    }

    /// Render this pane into the given frame region.
    ///
    /// Draws a bordered block whose title shows `name · branch · icon state`,
    /// then paints the emulator's cell grid into the inner area. `focused`
    /// brightens the border.
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        focused: bool,
        theme: &ThemeConfig,
    ) {
        let border_color = if focused {
            theme.border_active()
        } else {
            theme.border()
        };
        let title = self.header();

        // Update the cached block spec only when the title changes (a resize is
        // detected at layout time via the LayoutCtx dimensions). The border/title
        // *style* (focus color) is applied at paint time and is deliberately
        // excluded from the cache key, so toggling focus never invalidates the
        // cached glyph placement.
        if title != self.last_title {
            PreparedBlock::append(
                &mut self.block,
                BlockSpec {
                    borders: Borders::ALL,
                    border_type: BorderType::Rounded,
                    title: Some(Line::from(title.clone())),
                    border_style: Style::default(),
                    title_style: Style::default(),
                },
            );
            self.last_title = title;
        }

        let layout = PreparedBlock::layout(&self.block, LayoutCtx::new(area.width, area.height));
        // Inner content rect in absolute buffer coordinates (PreparedBlock's
        // `inner` is relative to the block origin; ratatui Block gave the same
        // inset for an ALL-bordered Double block).
        let inner = Rect::new(
            area.x.saturating_add(layout.inner.x),
            area.y.saturating_add(layout.inner.y),
            layout.inner.width,
            layout.inner.height,
        );
        self.paint_grid(frame.buffer_mut(), inner, theme.bg());
        // Paint the cached border + title glyphs (styles applied here).
        layout.paint(
            frame.buffer_mut(),
            area,
            Style::default().fg(border_color).bg(theme.bg()),
            Style::default().fg(self.state.color()).bg(theme.bg()),
        );
        // Render the cursor: reverse-video the cell at the emulator's cursor
        // position so the user sees where their typing will land. Skipped when
        // the agent has hidden the cursor (ESC[?25l) or it's off-screen.
        if let Some((cur_col, cur_row)) = self.emu.cursor_position() {
            let x = inner.x.saturating_add(cur_col);
            let y = inner.y.saturating_add(cur_row);
            if x < inner.right() && y < inner.bottom() {
                let cell = &mut frame.buffer_mut()[(x, y)];
                let fg = cell.fg;
                let bg = cell.bg;
                cell.set_fg(bg);
                cell.set_bg(fg);
            }
        }
    }

    /// Build the `" name · branch · icon state "` header string.
    fn header(&self) -> String {
        let location = self.branch.as_deref().unwrap_or("\u{2014}"); // —
        format!(
            " {} \u{00B7} {} \u{00B7} {} {} ",
            self.name,
            location,
            self.state.icon(),
            self.state.label()
        )
    }

    /// Paint the emulator grid into `area` of the buffer — **every cell, every
    /// frame**.
    ///
    /// ratatui double-buffers its `Terminal` (the back buffer is swapped with
    /// the front on each `flush`), so a per-frame partial/dirty repaint is
    /// unsound here: unrepainted cells would carry 2-frame-old content from the
    /// other buffer and the diff would smear the screen. We therefore repaint
    /// the full visible window each frame (ratatui's own diff still skips
    /// unchanged cells on the stdout side, so this is not wasteful on output).
    /// `bg_fill` (the theme background) backs `Default`-bg cells so the pane
    /// interior is consistently themed. Wide (CJK/emoji) cells consume two
    /// columns; the following column is left as the block's default blank.
    fn paint_grid(&mut self, buf: &mut Buffer, area: Rect, bg_fill: Color) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let grid = self.emu.grid();
        let max_rows = usize::from(area.height);
        let max_cols = usize::from(area.width);
        // Optional live debug: does the painted window overlap the content?
        if std::env::var("ORCA_DEBUG_LOG").is_ok() {
            let gh = grid.len();
            let gw = grid.first().map_or(0, Vec::len);
            let painted_with_content: usize = grid
                .iter()
                .take(max_rows)
                .map(|r| {
                    r.iter()
                        .take(max_cols)
                        .filter(|c| c.has_contents() && c.chars != " ")
                        .count()
                })
                .sum();
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/orca-live.log")
            {
                let _ = writeln!(
                    f,
                    "paint_grid: area x={} y={} {}x{}, grid={}x{}, window={}x{}, content_in_window={}",
                    area.x, area.y, area.width, area.height, gw, gh, max_cols, max_rows, painted_with_content
                );
            }
        }

        for (row_idx, row) in grid.iter().enumerate() {
            if row_idx >= max_rows {
                break;
            }
            let y = area.y.saturating_add(row_idx as u16);
            let mut col_idx = 0usize;
            while col_idx < row.len() && col_idx < max_cols {
                let cell = &row[col_idx];
                let x = area.x.saturating_add(col_idx as u16);
                paint_cell(buf, x, y, cell, bg_fill);
                col_idx += if cell.wide { 2 } else { 1 };
            }
        }
    }
}

/// Paint one emulator cell into the buffer at `(x, y)`.
fn paint_cell(buf: &mut Buffer, x: u16, y: u16, cell: &EmuCell, bg_fill: Color) {
    let buf_cell = &mut buf[(x, y)];

    // Symbol: the cell's glyph(s), or a space for blank cells so the block's
    // background reads cleanly. `Cell`'s fields are private in ratatui 0.29, so
    // use the setter methods.
    if cell.has_contents() {
        buf_cell.set_symbol(&cell.chars);
    } else {
        buf_cell.set_symbol(" ");
    }

    // Inverse/reverse video swaps fg and bg before mapping.
    let (fg, bg) = if cell.style.inverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    buf_cell.set_fg(map_fg(fg));
    buf_cell.set_bg(map_bg(bg, bg_fill));

    let modifier = map_style(&cell.style);
    if modifier != Modifier::empty() {
        buf_cell.set_style(Style::default().add_modifier(modifier));
    }
}

/// Map a foreground [`EmuColor`] onto a ratatui [`Color`].
///
/// `Default` becomes the theme default foreground ([`DEFAULT_FG`]);
/// `Indexed` and `Rgb` map 1:1. Splitting fg/bg (rather than one shared
/// `map_color`) lets the default *background* paint as the theme background
/// (passed in via `bg_fill`) instead of being forced to the foreground default.
#[must_use]
fn map_fg(color: EmuColor) -> Color {
    match color {
        EmuColor::Default => DEFAULT_FG,
        EmuColor::Indexed(i) => Color::Indexed(i),
        EmuColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Map a background [`EmuColor`] onto a ratatui [`Color`].
///
/// `Default` becomes the supplied `bg_fill` (the theme background threaded
/// down from `render`, so the pane interior is consistently themed instead of
/// terminal-transparent); `Indexed` and `Rgb` map 1:1.
#[must_use]
fn map_bg(color: EmuColor, bg_fill: Color) -> Color {
    match color {
        EmuColor::Default => bg_fill,
        EmuColor::Indexed(i) => Color::Indexed(i),
        EmuColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Map emulator style flags onto a ratatui [`Modifier`] bitset.
#[must_use]
fn map_style(style: &EmuStyle) -> Modifier {
    let mut m = Modifier::empty();
    if style.bold {
        m |= Modifier::BOLD;
    }
    if style.italic {
        m |= Modifier::ITALIC;
    }
    if style.underline {
        m |= Modifier::UNDERLINED;
    }
    // `inverse` is applied as a fg/bg swap in `paint_color`; we do not also
    // emit `REVERSED` to avoid double-inversion.
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeConfig;
    use crate::terminal_emu::{EmuColor, EmuStyle};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn map_fg_map_bg_default_indexed_rgb() {
        // The whole point of the fg/bg split: a `Default` fg paints as the
        // foreground default, a `Default` bg paints as the supplied theme fill.
        let fill = Color::Rgb(0x0d, 0x11, 0x17);
        assert_eq!(map_fg(EmuColor::Default), DEFAULT_FG);
        assert_eq!(map_bg(EmuColor::Default, fill), fill);
        assert_ne!(DEFAULT_FG, fill, "fg default and bg fill must differ");

        // Indexed / Rgb map 1:1 for both slots.
        assert_eq!(map_fg(EmuColor::Indexed(1)), Color::Indexed(1));
        assert_eq!(map_bg(EmuColor::Indexed(1), fill), Color::Indexed(1));
        assert_eq!(map_fg(EmuColor::Indexed(255)), Color::Indexed(255));
        assert_eq!(map_bg(EmuColor::Indexed(255), fill), Color::Indexed(255));
        assert_eq!(map_fg(EmuColor::Rgb(10, 20, 30)), Color::Rgb(10, 20, 30));
        assert_eq!(
            map_bg(EmuColor::Rgb(10, 20, 30), fill),
            Color::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn map_style_combines_flags() {
        let none = map_style(&EmuStyle::default());
        assert_eq!(none, Modifier::empty());

        let bold_underline = map_style(&EmuStyle {
            bold: true,
            italic: false,
            underline: true,
            inverse: false,
        });
        assert!(bold_underline.contains(Modifier::BOLD));
        assert!(bold_underline.contains(Modifier::UNDERLINED));
        assert!(!bold_underline.contains(Modifier::ITALIC));

        // `inverse` does NOT contribute REVERSED here (handled by fg/bg swap).
        let inverse_only = map_style(&EmuStyle {
            bold: false,
            italic: false,
            underline: false,
            inverse: true,
        });
        assert_eq!(inverse_only, Modifier::empty());
    }

    #[test]
    fn pane_lifecycle_and_scroll() {
        let mut pane = Pane::new(0, "test", 20, 4);
        assert_eq!(pane.id(), 0);
        assert_eq!(pane.name(), "test");
        assert_eq!(pane.size(), (20, 4));
        assert!(matches!(pane.state(), AgentState::Idle));
        assert_eq!(pane.scroll(), 0);

        pane.feed(b"hello");
        pane.set_state(AgentState::Running);
        pane.set_branch(Some("main".to_owned()));
        assert!(matches!(pane.state(), AgentState::Running));

        pane.scroll_up(5);
        assert_eq!(pane.scroll(), 5);
        pane.scroll_down(2);
        assert_eq!(pane.scroll(), 3);
        pane.scroll_down(100); // saturates at 0
        assert_eq!(pane.scroll(), 0);
        pane.scroll_reset();
        assert_eq!(pane.scroll(), 0);

        pane.resize_viewport(40, 8);
        assert_eq!(pane.size(), (40, 8));
    }

    #[test]
    fn render_paints_fed_text_into_buffer() {
        // 12 wide x 3 tall pane; feed "AB" which lands on row 0.
        let backend = TestBackend::new(12, 3);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut pane = Pane::new(1, "p", 12, 3);
        pane.feed(b"AB");

        terminal
            .draw(|f| pane.render(f, f.area(), true, &ThemeConfig::default()))
            .expect("draw");

        let buf = terminal.backend().buffer();
        // Inner area starts at (1,1) due to the 1-cell rounded border.
        // Row 0 inner col 0 should be 'A', col 1 'B'.
        let cell_a = &buf[(1, 1)];
        assert!(
            cell_a.symbol().contains('A'),
            "expected 'A' at (1,1), got {:?}",
            cell_a.symbol()
        );
        let cell_b = &buf[(2, 1)];
        assert!(
            cell_b.symbol().contains('B'),
            "expected 'B' at (2,1), got {:?}",
            cell_b.symbol()
        );
    }

    #[test]
    fn render_does_not_panic_on_size_mismatch() {
        // Emulator is 80x24 but the render area is tiny; must clamp, not panic.
        let backend = TestBackend::new(6, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut pane = Pane::new(2, "big", 80, 24);
        terminal
            .draw(|f| pane.render(f, f.area(), false, &ThemeConfig::default()))
            .expect("draw");
    }

    #[test]
    fn render_focused_vs_unfocused_border_color() {
        // Smoke test: both paths render without panic; we cannot easily assert
        // border color via TestBackend without reaching into Cell styles, so
        // this guards against regressions in the block construction.
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut pane = Pane::new(0, "x", 10, 3);
        terminal
            .draw(|f| pane.render(f, f.area(), true, &ThemeConfig::default()))
            .expect("focused draw");
        let backend = TestBackend::new(10, 3);
        terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| pane.render(f, f.area(), false, &ThemeConfig::default()))
            .expect("unfocused draw");
    }

    #[test]
    fn debug_impl_shows_key_fields() {
        let mut pane = Pane::new(7, "gemini", 80, 24);
        pane.set_state(AgentState::Done(Some(0)));
        pane.set_branch(Some("feat/x".to_string()));
        let s = format!("{pane:?}");
        assert!(s.contains("id: 7"), "Debug shows id");
        assert!(s.contains("gemini"), "Debug shows name");
        assert!(s.contains("Done"), "Debug shows state");
        assert!(s.contains("feat/x"), "Debug shows branch");
    }

    #[test]
    fn feed_with_osc9999_activity_sets_activity_field() {
        let mut pane = Pane::new(0, "agent", 40, 6);
        // OSC 9999 with an activity JSON payload.
        let payload = b"\x1b]9999;{\"state\":\"thinking\",\"detail\":\"planning\"}\x07";
        pane.feed(payload);
        let act = pane.activity();
        assert!(act.is_some(), "OSC 9999 activity was captured");
        assert_eq!(act.unwrap().state, "thinking");
    }
}
