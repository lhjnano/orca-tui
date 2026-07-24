//! # Sidebar (Orca-GUI-style left navigational spine)
//!
//! The at-a-glance panel that lists every agent: a colored status dot, the
//! current activity (tool / prompt), the agent name, and its branch or model.
//! It is the navigational spine of the TUI — one row per agent, focusable.
//!
//! This module is deliberately decoupled from the app internals: it renders a
//! standalone [`SidebarEntry`] slice, so anything (a pane manager, a snapshot
//! serializer, a test) can build entries and draw the sidebar without touching
//! [`crate::pane::Pane`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};
use ratatui::Frame;

use crate::agent::{AgentState, AgentStatus};
use crate::config::ThemeConfig;
use crate::osc::AgentActivity;

/// Minimum inner width at which sidebar entries expand to two lines (name +
/// branch on line 1, `tool: input` summary on line 2). Below this the classic
/// one-line layout is kept, so the existing 16/24/32-wide sidebar tests stay
/// one-line and pass unmodified.
const TWO_LINE_MIN_WIDTH: u16 = 36;

/// One row of sidebar display data (no [`crate::pane::Pane`] dependency).
///
/// `activity` carries live OSC 9999 capture ([`AgentActivity`]); when present
/// it overrides the lifecycle-state-based status dot.
#[derive(Debug, Clone)]
pub struct SidebarEntry {
    /// Display name (e.g. `"claude"`, `"codex"`).
    pub name: String,
    /// Agent lifecycle state (drives the fallback status dot).
    pub state: AgentState,
    /// Git branch / cwd label shown at the right edge.
    pub branch: Option<String>,
    /// Live activity from OSC 9999 capture; overrides the state-based dot.
    pub activity: Option<AgentActivity>,
    /// Whether this entry currently has keyboard focus (accent highlight).
    pub focused: bool,
    /// Whether the user pinned this agent (Action item #6). Pinned agents
    /// render in a dedicated "PINNED" sidebar section above "IN PROGRESS".
    pub pinned: bool,
}

/// Render the sidebar into a region of `frame`.
///
/// Layout (top to bottom): the sidebar sits inside a fully **bordered** panel
/// box (raised `theme.panel()` background, `theme.border()` edges) whose top
/// border carries the ` orcatui ` title. Inside the box: an optional `PINNED`
/// section, then the `IN PROGRESS (n)` section header, then one row per entry
/// (windowed to the latest entries when they overflow).
///
/// Never panics: every string is width-truncated via the buffer's own width
/// accounting, so narrow terminals / long names degrade gracefully.
pub fn render_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[SidebarEntry],
    theme: &ThemeConfig,
    status: Option<(&str, Color)>,
) {
    // Bordered panel-box (opencode `backgroundPanel` aesthetic): the sidebar
    // reads as a distinct raised box sitting on the darker terminal bg. The
    // brand is the block title (on the top border), so content starts directly
    // at `inner.y` — no in-content brand row.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border()))
        .style(Style::default().bg(theme.panel()))
        .title(Line::from(" orcatui ").style(Style::default().fg(theme.accent())));
    let inner = block.inner(area);
    frame.render_widget(&block, area);

    let buf = frame.buffer_mut();
    // Guarantee the whole region carries the panel background (no gaps in the
    // unwritten tail below the last entry).
    buf.set_style(area, Style::default().bg(theme.panel()));

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bg = theme.panel();
    let bottom = inner.bottom();
    let lines_each = entry_lines(inner.width);
    let mut y = inner.y;

    // Optional connection status line (first content row).
    if let Some((label, color)) = status {
        if y < bottom {
            let _ = buf.set_stringn(
                inner.x,
                y,
                label,
                usize::from(inner.width),
                Style::default()
                    .fg(color)
                    .bg(bg)
                    .add_modifier(Modifier::DIM),
            );
        }
        y = y.saturating_add(1);
    }

    // Pinned section (Action item #6): a dedicated header + each pinned entry,
    // rendered top-down. Pinned agents are the priority — they stay visible
    // until every row is exhausted (never scrolled out by unpinned overflow).
    let has_pinned = entries.iter().any(|e| e.pinned);
    if has_pinned {
        let pinned_count = entries.iter().filter(|e| e.pinned).count();
        if y < bottom {
            let _ = buf.set_stringn(
                inner.x,
                y,
                &format!("\u{25B8} PINNED ({pinned_count})"),
                usize::from(inner.width),
                Style::default()
                    .fg(theme.fg())
                    .add_modifier(Modifier::DIM)
                    .bg(bg),
            );
        }
        y = y.saturating_add(1);
        for entry in entries.iter().filter(|e| e.pinned) {
            let draw = bottom.saturating_sub(y).min(lines_each);
            if draw == 0 {
                break;
            }
            render_entry_at(buf, inner.x, y, inner.width, draw, entry, theme);
            y = y.saturating_add(draw);
        }
    }

    // ` IN PROGRESS (n) ` — n counts ACTIVE UNPINNED entries (pinned agents
    // were already listed in their own section, so they don't double-count).
    let active_unpinned = entries.iter().filter(|e| !e.pinned && is_active(e)).count();
    if y < bottom {
        let _ = buf.set_stringn(
            inner.x,
            y,
            &format!(" IN PROGRESS ({active_unpinned}) "),
            usize::from(inner.width),
            Style::default()
                .fg(theme.fg())
                .add_modifier(Modifier::DIM)
                .bg(bg),
        );
    }
    y = y.saturating_add(1);

    // Unpinned entries, tail-windowed to the remaining rows so the newest stay
    // visible when they overflow. Each entry occupies `lines_each` rows.
    let unpinned: Vec<&SidebarEntry> = entries.iter().filter(|e| !e.pinned).collect();
    let remaining_rows = bottom.saturating_sub(y);
    let fits = if lines_each == 0 {
        0
    } else {
        usize::from(remaining_rows / lines_each)
    };
    // Auto-scroll: window the unpinned entries so the FOCUSED agent is visible
    // (centered in the window, clamped to the list bounds). Falls back to the
    // tail window when nothing is focused or everything fits.
    let max_start = unpinned.len().saturating_sub(fits);
    let start = match unpinned.iter().position(|e| e.focused) {
        Some(f) if fits > 0 => f.saturating_sub(fits / 2).min(max_start),
        _ => unpinned.len().saturating_sub(fits),
    };
    for entry in unpinned[start..].iter() {
        let draw = bottom.saturating_sub(y).min(lines_each);
        if draw == 0 {
            break;
        }
        render_entry_at(buf, inner.x, y, inner.width, draw, entry, theme);
        y = y.saturating_add(draw);
    }
}

/// Whether an entry counts as "in progress" for the section header. Derived
/// via the unified [`AgentStatus`] model: Working/Blocked/Waiting are all
/// active. (A `Running` process whose OSC payload reports `blocked` or
/// `waiting` is still active; previously only `Running` + `"working"` counted.)
fn is_active(entry: &SidebarEntry) -> bool {
    AgentStatus::derive(
        &entry.state,
        entry.activity.as_ref().map(|a| a.state.as_str()),
    )
    .is_active()
}

/// The `(glyph, style)` status dot for an entry. Derived via the unified
/// [`AgentStatus`] model, which combines the OSC 9999 activity state with the
/// lifecycle state in one place. Glyphs and theme colors are identical to the
/// previous inline implementation, so every existing render test still passes.
fn status_style(entry: &SidebarEntry, theme: &ThemeConfig, bg: Color) -> (&'static str, Style) {
    let dim = |c| Style::default().fg(c).bg(bg).add_modifier(Modifier::DIM);
    let lit = |c| Style::default().fg(c).bg(bg);
    let status = AgentStatus::derive(
        &entry.state,
        entry.activity.as_ref().map(|a| a.state.as_str()),
    );
    match status {
        AgentStatus::Working => ("\u{25CF}", lit(theme.success())), // ●
        AgentStatus::Blocked => ("\u{25CF}", lit(theme.error())),   // ●
        AgentStatus::Waiting => ("\u{25CF}", lit(theme.warning())), // ●
        AgentStatus::Done => ("\u{2713}", dim(theme.fg())),         // ✓
        AgentStatus::Failed => ("\u{2717}", lit(theme.error())),    // ✗
        AgentStatus::Idle => ("\u{25CB}", dim(theme.fg())),         // ○
    }
}

/// One-line activity summary: `{tool}: {input}` if a tool is known, else the
/// prompt, else the state label.
fn activity_summary(entry: &SidebarEntry) -> String {
    if let Some(act) = &entry.activity {
        if let Some(tool) = &act.tool_name {
            return match &act.tool_input {
                Some(input) => format!("{tool}: {input}"),
                None => tool.clone(),
            };
        }
        if let Some(prompt) = &act.prompt {
            return prompt.clone();
        }
    }
    entry.state.label().to_owned()
}

/// Approximate display width: one cell per `char`. Exact for the ASCII that
/// agent names / branches / models are in practice. Used only for right-aligned
/// branch placement; the buffer's own width accounting (which handles wide
/// graphemes) does all real truncation, so this never causes an out-of-bounds.
fn disp_width(s: &str) -> usize {
    s.chars().count()
}

/// Paint one entry row into `buf`, width-truncating every segment.
fn render_entry_row(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    width: u16,
    entry: &SidebarEntry,
    theme: &ThemeConfig,
) {
    if width == 0 {
        return;
    }
    let bg = if entry.focused {
        theme.element()
    } else {
        theme.panel()
    };
    // Focused entry: paint a full-width selection bar so the active agent is
    // obvious at a glance in the sidebar (more than just a small ▶ marker).
    buf.set_style(Rect::new(x0, y, width, 1), Style::default().bg(bg));
    let mut x = x0;
    let right = x0.saturating_add(width);

    // Branch / model: right-aligned, dim. Rendered first so left-side content
    // truncates before it instead of running underneath it.
    let branch: Option<&str> = entry
        .branch
        .as_deref()
        .or_else(|| entry.activity.as_ref().and_then(|a| a.model.as_deref()));
    let bw = u16::try_from(branch.map_or(0, disp_width)).unwrap_or(0);
    let left_end = if bw > 0 && bw.saturating_add(2) <= width {
        let bx = right.saturating_sub(bw);
        buf.set_stringn(
            bx,
            y,
            branch.unwrap_or(""),
            usize::from(bw),
            Style::default()
                .fg(theme.fg())
                .add_modifier(Modifier::DIM)
                .bg(bg),
        );
        bx.saturating_sub(1)
    } else {
        right
    };

    // Focus marker: `▶` (accent) when focused, else a blank.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        if entry.focused {
            let p = buf.set_stringn(
                x,
                y,
                "\u{25B6}",
                usize::from(remaining),
                Style::default().fg(theme.accent()).bg(bg),
            );
            x = p.0;
        } else {
            let p = buf.set_stringn(x, y, " ", usize::from(remaining), Style::default().bg(bg));
            x = p.0;
        }
    }

    // Status dot.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let (glyph, style) = status_style(entry, theme, bg);
        let p = buf.set_stringn(x, y, glyph, usize::from(remaining), style);
        x = p.0;
    }

    // Separator space.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let p = buf.set_stringn(x, y, " ", usize::from(remaining), Style::default().bg(bg));
        x = p.0;
    }

    // Name: accent + bold when focused, else foreground.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let name_style = if entry.focused {
            Style::default()
                .fg(theme.accent())
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg()).bg(bg)
        };
        let p = buf.set_stringn(x, y, &entry.name, usize::from(remaining), name_style);
        x = p.0;
    }

    // Activity summary (dim) — only if there's room for a leading gap.
    let remaining = left_end.saturating_sub(x);
    if remaining > 1 {
        let p = buf.set_stringn(x, y, " ", 1, Style::default().bg(bg));
        x = p.0;
        let remaining = left_end.saturating_sub(x);
        if remaining > 0 {
            let summary = activity_summary(entry);
            let style = Style::default()
                .fg(theme.fg())
                .add_modifier(Modifier::DIM)
                .bg(bg);
            let _ = buf.set_stringn(x, y, &summary, usize::from(remaining), style);
        }
    }
}

/// Rows consumed by one sidebar entry at the given inner width: 2 on wide
/// sidebars (denser two-line layout), 1 on narrow ones. Extracted as a free
/// function so the windowing math is unit-testable in isolation.
fn entry_lines(width: u16) -> u16 {
    if width >= TWO_LINE_MIN_WIDTH {
        2
    } else {
        1
    }
}

/// Dispatch one entry to the one-line or two-line renderer based on the rows
/// actually available at `y` (`draw_lines` is pre-clamped by the caller to 1
/// when only a single row remains, so a two-line entry never overflows).
fn render_entry_at(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    width: u16,
    draw_lines: u16,
    entry: &SidebarEntry,
    theme: &ThemeConfig,
) {
    if draw_lines >= 2 {
        render_entry_two_line(buf, x0, y, width, entry, theme);
    } else {
        render_entry_row(buf, x0, y, width, entry, theme);
    }
}

/// Two-line entry layout (wide sidebars, `inner.width >= TWO_LINE_MIN_WIDTH`):
///   line 1: `[marker][dot] name        model-or-branch`
///   line 2: `       tool: input`  (indented ~7 cols, dim)
///
/// Line 1 mirrors [`render_entry_row`] minus the inline activity summary; line
/// 2 carries that summary on its own row so a wide sidebar can show the live
/// tool call without cramping the name. `y + 1` is guaranteed in-bounds because
/// the caller only requests two lines when at least two rows remain.
fn render_entry_two_line(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    width: u16,
    entry: &SidebarEntry,
    theme: &ThemeConfig,
) {
    if width == 0 {
        return;
    }
    let bg = if entry.focused {
        theme.element()
    } else {
        theme.panel()
    };
    // Focused entry: paint a full-width selection bar so the active agent is
    // obvious at a glance in the sidebar (more than just a small ▶ marker).
    buf.set_style(Rect::new(x0, y, width, 2), Style::default().bg(bg));
    let mut x = x0;
    let right = x0.saturating_add(width);

    // Branch / model: right-aligned on line 1 (dim). Painted first so left-
    // side content truncates before reaching it.
    let branch: Option<&str> = entry
        .branch
        .as_deref()
        .or_else(|| entry.activity.as_ref().and_then(|a| a.model.as_deref()));
    let bw = u16::try_from(branch.map_or(0, disp_width)).unwrap_or(0);
    let left_end = if bw > 0 && bw.saturating_add(2) <= width {
        let bx = right.saturating_sub(bw);
        let _ = buf.set_stringn(
            bx,
            y,
            branch.unwrap_or(""),
            usize::from(bw),
            Style::default()
                .fg(theme.fg())
                .add_modifier(Modifier::DIM)
                .bg(bg),
        );
        bx.saturating_sub(1)
    } else {
        right
    };

    // Focus marker: `▶` (accent) when focused, else a blank.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        if entry.focused {
            let p = buf.set_stringn(
                x,
                y,
                "\u{25B6}",
                usize::from(remaining),
                Style::default().fg(theme.accent()).bg(bg),
            );
            x = p.0;
        } else {
            let p = buf.set_stringn(x, y, " ", usize::from(remaining), Style::default().bg(bg));
            x = p.0;
        }
    }

    // Status dot.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let (glyph, style) = status_style(entry, theme, bg);
        let p = buf.set_stringn(x, y, glyph, usize::from(remaining), style);
        x = p.0;
    }

    // Separator space.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let p = buf.set_stringn(x, y, " ", usize::from(remaining), Style::default().bg(bg));
        x = p.0;
    }

    // Name: accent + bold when focused, else foreground.
    let remaining = left_end.saturating_sub(x);
    if remaining > 0 {
        let name_style = if entry.focused {
            Style::default()
                .fg(theme.accent())
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg()).bg(bg)
        };
        let p = buf.set_stringn(x, y, &entry.name, usize::from(remaining), name_style);
        let _ = p;
    }

    // Line 2: indented activity summary (dim). The indent (~7 cols) tucks the
    // summary under the name rather than the marker/dot column.
    let summary = activity_summary(entry);
    if !summary.is_empty() {
        let indent: u16 = 7;
        let sx = x0.saturating_add(indent);
        let avail = right.saturating_sub(sx);
        if avail > 0 {
            let _ = buf.set_stringn(
                sx,
                y.saturating_add(1),
                &summary,
                usize::from(avail),
                Style::default()
                    .fg(theme.fg())
                    .add_modifier(Modifier::DIM)
                    .bg(bg),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeConfig;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Flatten the buffer to its visible text (one line per row).
    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area();
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|col| buf[(col, row)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_sidebar_shows_brand_and_entries() {
        let entries = vec![
            SidebarEntry {
                name: "claude".to_owned(),
                state: AgentState::Running,
                branch: Some("main".to_owned()),
                activity: Some(AgentActivity {
                    state: "working".to_owned(),
                    tool_name: Some("Edit".to_owned()),
                    tool_input: Some("src/lib.rs".to_owned()),
                    ..AgentActivity::default()
                }),
                focused: true,
                pinned: false,
            },
            SidebarEntry {
                name: "codex".to_owned(),
                state: AgentState::Running,
                branch: None,
                activity: Some(AgentActivity {
                    state: "waiting".to_owned(),
                    model: Some("gpt-5".to_owned()),
                    ..AgentActivity::default()
                }),
                focused: false,
                pinned: false,
            },
            SidebarEntry {
                name: "opencode".to_owned(),
                state: AgentState::Done(Some(0)),
                branch: None,
                activity: Some(AgentActivity {
                    state: "done".to_owned(),
                    ..AgentActivity::default()
                }),
                focused: false,
                pinned: false,
            },
        ];

        let backend = TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orcatui"), "brand header missing:\n{text}");
        assert!(
            text.contains("IN PROGRESS (2)"),
            "section header wrong:\n{text}"
        );
        for name in ["claude", "codex", "opencode"] {
            assert!(text.contains(name), "{name} row missing:\n{text}");
        }
        // Status dots: working/waiting are `●`, done is `✓`.
        assert!(
            text.contains('\u{25CF}'),
            "working/waiting dot missing:\n{text}"
        );
        assert!(text.contains('\u{2713}'), "done dot missing:\n{text}");
    }

    #[test]
    fn render_sidebar_truncates_long_names() {
        let entries = vec![SidebarEntry {
            name: "this-is-a-very-long-agent-name-that-will-not-fit".to_owned(),
            state: AgentState::Running,
            branch: Some("feature/some/long/branch".to_owned()),
            activity: Some(AgentActivity {
                state: "working".to_owned(),
                tool_name: Some("Write".to_owned()),
                tool_input: Some("a-rather-long-input-string".to_owned()),
                ..AgentActivity::default()
            }),
            focused: true,
            pinned: false,
        }];

        // Narrow enough that nothing fits whole; must truncate every segment
        // without panicking. Inner width (15 minus the right border) leaves room
        // for a recognizable name prefix but not the whole thing.
        let backend = TestBackend::new(16, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("this-is"),
            "truncated name prefix should still render:\n{text}"
        );
        // The tail must be cut off — proves truncation, not overflow.
        assert!(
            !text.contains("not-fit"),
            "long name should be truncated, not wrapped/overflowed:\n{text}"
        );
    }

    #[test]
    fn render_sidebar_zero_entries() {
        let entries: Vec<SidebarEntry> = vec![];

        let backend = TestBackend::new(24, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orcatui"), "brand header missing:\n{text}");
        assert!(
            text.contains("IN PROGRESS (0)"),
            "zero-count header wrong:\n{text}"
        );
    }

    #[test]
    fn render_sidebar_focused_highlight() {
        let entries = vec![
            SidebarEntry {
                name: "claude".to_owned(),
                state: AgentState::Running,
                branch: Some("main".to_owned()),
                activity: None,
                focused: true,
                pinned: false,
            },
            SidebarEntry {
                name: "codex".to_owned(),
                state: AgentState::Idle,
                branch: None,
                activity: None,
                focused: false,
                pinned: false,
            },
        ];

        let backend = TestBackend::new(32, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        let accent = theme.accent();
        let fg = theme.fg();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let buf = terminal.backend().buffer();
        // Border offset: the sidebar is now a fully bordered panel box
        // (`Borders::ALL`), so `inner` begins at (1,1). Row 0 is the top border
        // carrying the ` orcatui ` title; row 1 is the `IN PROGRESS` header;
        // the first entry is row 2. `inner.x` is 1, so the focus marker sits at
        // column 1 and the name (marker + dot + space = 3 cols in) at column 4.
        // Focused "claude" name cell is accent; its focus marker is accent.
        let marker = &buf[(1, 2)];
        assert_eq!(
            marker.symbol(),
            "\u{25B6}",
            "focused row should start with the focus marker"
        );
        assert_eq!(marker.fg, accent, "focus marker must be accent");

        let name_cell = &buf[(4, 2)];
        assert_eq!(name_cell.symbol(), "c", "expected 'c' at (4,2)");
        assert_eq!(name_cell.fg, accent, "focused name must be accent-colored");

        // Unfocused "codex" (row 3): blank marker, foreground name.
        let marker2 = &buf[(1, 3)];
        assert_eq!(marker2.symbol(), " ", "unfocused row has a blank marker");
        let name_cell2 = &buf[(4, 3)];
        assert_eq!(name_cell2.symbol(), "c", "expected 'c' at (4,3)");
        assert_eq!(
            name_cell2.fg, fg,
            "unfocused name must be the plain foreground, not accent"
        );
        assert_ne!(
            fg, accent,
            "fg and accent must differ for the test to mean anything"
        );
    }

    #[test]
    fn render_sidebar_degenerate_area_never_panics() {
        // Zero/tiny dimensions on every path: must not index out of bounds.
        let entries = vec![SidebarEntry {
            name: "x".to_owned(),
            state: AgentState::Idle,
            branch: None,
            activity: None,
            focused: false,
            pinned: false,
        }];
        let theme = ThemeConfig::default();
        for (w, h) in [(0, 0), (1, 1), (2, 1), (1, 5)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
                .expect("draw should not panic");
        }
    }

    /// First row index (0-based) whose flattened text contains `needle`, or
    /// `None`. Used to assert which row a given substring lands on.
    fn row_of(buf: &Buffer, needle: &str) -> Option<u16> {
        let area = buf.area();
        for row in 0..area.height {
            let line: String = (0..area.width)
                .map(|col| buf[(col, row)].symbol().to_owned())
                .collect();
            if line.contains(needle) {
                return Some(row);
            }
        }
        None
    }

    #[test]
    fn render_sidebar_pinned_section() {
        // One pinned + one unpinned, narrow width (< 36 ⇒ one-line entries so
        // the assertions stay simple). The pinned agent must appear under a
        // "PINNED" header that sits ABOVE the "IN PROGRESS" header.
        let entries = vec![
            SidebarEntry {
                name: "claude".to_owned(),
                state: AgentState::Running,
                branch: Some("main".to_owned()),
                activity: Some(AgentActivity {
                    state: "working".to_owned(),
                    tool_name: Some("Edit".to_owned()),
                    tool_input: Some("src/lib.rs".to_owned()),
                    ..AgentActivity::default()
                }),
                focused: false,
                pinned: true,
            },
            SidebarEntry {
                name: "codex".to_owned(),
                state: AgentState::Running,
                branch: None,
                activity: Some(AgentActivity {
                    state: "waiting".to_owned(),
                    ..AgentActivity::default()
                }),
                focused: true,
                pinned: false,
            },
        ];

        let backend = TestBackend::new(32, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let buf = terminal.backend().buffer();
        let text = buffer_text(buf);
        assert!(text.contains("PINNED"), "PINNED header missing:\n{text}");
        assert!(
            text.contains("IN PROGRESS"),
            "IN PROGRESS header missing:\n{text}"
        );

        let pinned_header_row = row_of(buf, "PINNED").expect("PINNED header row");
        let pinned_name_row = row_of(buf, "claude").expect("pinned name row");
        let progress_row = row_of(buf, "IN PROGRESS").expect("IN PROGRESS header row");
        let unpinned_name_row = row_of(buf, "codex").expect("unpinned name row");
        assert!(
            pinned_name_row > pinned_header_row,
            "pinned name must be below its header"
        );
        assert!(
            pinned_name_row < progress_row,
            "pinned agent must render ABOVE the IN PROGRESS header"
        );
        assert!(
            unpinned_name_row > progress_row,
            "unpinned agent must render BELOW the IN PROGRESS header"
        );
    }

    #[test]
    fn render_sidebar_two_line_on_wide() {
        // Width 44 ≥ 36 ⇒ two-line entries. The tool summary must land on its
        // OWN row, distinct from (and below) the name row.
        let entries = vec![SidebarEntry {
            name: "claude".to_owned(),
            state: AgentState::Running,
            branch: None,
            activity: Some(AgentActivity {
                state: "working".to_owned(),
                tool_name: Some("Edit".to_owned()),
                tool_input: Some("src/lib.rs".to_owned()),
                ..AgentActivity::default()
            }),
            focused: true,
            pinned: false,
        }];

        let backend = TestBackend::new(44, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let buf = terminal.backend().buffer();
        let name_row = row_of(buf, "claude").expect("name row");
        let summary_row = row_of(buf, "Edit: src/lib.rs").expect("tool summary row");
        assert_eq!(
            summary_row,
            name_row + 1,
            "two-line layout: summary on the row directly below the name"
        );
    }

    #[test]
    fn render_sidebar_one_line_on_narrow() {
        // Width 32 < 36 ⇒ one-line entries. The tool summary shares the name's
        // row (inline), proving no second line was emitted.
        let entries = vec![SidebarEntry {
            name: "claude".to_owned(),
            state: AgentState::Running,
            branch: None,
            activity: Some(AgentActivity {
                state: "working".to_owned(),
                tool_name: Some("Edit".to_owned()),
                tool_input: Some("src/lib.rs".to_owned()),
                ..AgentActivity::default()
            }),
            focused: true,
            pinned: false,
        }];

        let backend = TestBackend::new(32, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let buf = terminal.backend().buffer();
        let name_row = row_of(buf, "claude").expect("name row");
        let summary_row = row_of(buf, "Edit: src/lib.rs").expect("tool summary row");
        assert_eq!(
            summary_row, name_row,
            "one-line layout: summary shares the name's row (no second line)"
        );
    }

    /// The sidebar must render as a distinct bordered panel box: box-drawing
    /// glyphs on the border carry `theme.border()` fg, and the interior is
    /// filled with the raised `theme.panel()` bg — distinct from the root
    /// `theme.bg()`. This locks in the opencode `backgroundPanel` aesthetic.
    #[test]
    fn render_sidebar_is_a_bordered_panel_box() {
        let entries = vec![SidebarEntry {
            name: "claude".to_owned(),
            state: AgentState::Running,
            branch: None,
            activity: None,
            focused: false,
            pinned: false,
        }];

        let backend = TestBackend::new(24, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        let border_color = theme.border();
        let panel_bg = theme.panel();
        let root_bg = theme.bg();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme, None))
            .expect("draw");

        let buf = terminal.backend().buffer();
        // The panel bg must be distinct from the root bg for this test to mean
        // anything (defaults are #161b22 vs #0d1117 — they differ).
        assert_ne!(
            panel_bg, root_bg,
            "panel() and bg() must differ or the box form is invisible"
        );

        // (a) Border cells carry `theme.border()` fg and a box-drawing glyph
        // (Unicode block U+2500..U+257F). Check all four edges + corners.
        let is_box_glyph = |s: &str| {
            s.chars()
                .next()
                .map(|c| (c as u32) >= 0x2500 && (c as u32) <= 0x257F)
                .unwrap_or(false)
        };
        // Corners.
        for &(x, y) in &[(0, 0), (23, 0), (0, 9), (23, 9)] {
            let cell = &buf[(x, y)];
            assert_eq!(
                cell.fg, border_color,
                "border corner ({x},{y}) must carry theme.border() fg"
            );
            assert!(
                is_box_glyph(cell.symbol()),
                "border corner ({x},{y}) must be a box-drawing glyph, got {:?}",
                cell.symbol()
            );
        }
        // Left & right edges (mid-height, clear of the title).
        for &(x, y) in &[(0, 5), (23, 5)] {
            let cell = &buf[(x, y)];
            assert_eq!(
                cell.fg, border_color,
                "border edge ({x},{y}) must carry theme.border() fg"
            );
            assert!(
                is_box_glyph(cell.symbol()),
                "border edge ({x},{y}) must be a box-drawing glyph, got {:?}",
                cell.symbol()
            );
        }
        // Bottom edge (clear of content).
        let bottom_edge = &buf[(12, 9)];
        assert_eq!(
            bottom_edge.fg, border_color,
            "bottom border must carry theme.border() fg"
        );
        assert!(
            is_box_glyph(bottom_edge.symbol()),
            "bottom border must be a box-drawing glyph"
        );

        // (b) An interior cell below all content carries the panel fill (the
        // raised box bg), NOT the root bg — proving the panel fill. Interior =
        // strictly inside the border: x in 1..=22, y in 1..=8. Row 7 is well
        // below the single entry (which lands on row 2), so it is unwritten.
        let interior = &buf[(5, 7)];
        assert_eq!(
            interior.bg, panel_bg,
            "interior cell must carry the panel bg (raised box fill)"
        );
        assert_ne!(
            interior.bg, root_bg,
            "interior cell must NOT carry the root bg"
        );
    }
}
