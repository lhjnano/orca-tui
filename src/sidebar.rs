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
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::agent::AgentState;
use crate::config::ThemeConfig;
use crate::osc::AgentActivity;

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
}

/// Render the sidebar into a region of `frame`.
///
/// Layout (top to bottom): brand header, `IN PROGRESS (n)` section header, then
/// one row per entry (windowed to the latest entries when they overflow). The
/// whole region sits inside a right-border-only [`Block`] on the theme
/// background, so it reads as a separate column next to the main pane area.
///
/// Never panics: every string is width-truncated via the buffer's own width
/// accounting, so narrow terminals / long names degrade gracefully.
pub fn render_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[SidebarEntry],
    theme: &ThemeConfig,
) {
    // No border — the 1-cell gap between sidebar and panes (from app.rs
    // Layout::horizontal().spacing(1)) provides visual separation, and the
    // panes' own Double borders form the boundary. A background fill is enough.
    let block = Block::default().style(Style::default().bg(theme.bg()));
    let inner = block.inner(area);
    frame.render_widget(&block, area);

    let buf = frame.buffer_mut();
    // Guarantee the whole region carries the theme background (no gaps in the
    // unwritten tail below the last entry).
    buf.set_style(area, Style::default().bg(theme.bg()));

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bg = theme.bg();

    // Row 0: brand header (` 🐋 orca-tui `).
    buf.set_stringn(
        inner.x,
        inner.y,
        "\u{25B8} orca-tui",
        usize::from(inner.width),
        Style::default().fg(theme.accent()).bg(bg),
    );

    // Row 1: ` IN PROGRESS (n) ` (dim).
    let active = entries.iter().filter(|e| is_active(e)).count();
    let section = format!(" IN PROGRESS ({active}) ");
    if inner.height > 1 {
        buf.set_stringn(
            inner.x,
            inner.y.saturating_add(1),
            &section,
            usize::from(inner.width),
            Style::default()
                .fg(theme.fg())
                .add_modifier(Modifier::DIM)
                .bg(bg),
        );
    }

    // Agent rows: window to the latest entries so the newest stay visible.
    let list_top = inner.y.saturating_add(2);
    let rows_for_entries = inner.height.saturating_sub(2);
    let start = entries.len().saturating_sub(usize::from(rows_for_entries));
    for (i, entry) in entries[start..].iter().enumerate() {
        let y = list_top.saturating_add(i as u16);
        if y >= inner.bottom() {
            break;
        }
        render_entry_row(buf, inner.x, y, inner.width, entry, theme);
    }
}

/// Whether an entry counts as "in progress" for the section header: the process
/// is [`AgentState::Running`], or OSC activity reports `"working"`.
fn is_active(entry: &SidebarEntry) -> bool {
    entry.state == AgentState::Running
        || entry
            .activity
            .as_ref()
            .map(|a| a.state == "working")
            .unwrap_or(false)
}

/// The `(glyph, style)` status dot for an entry. OSC activity takes precedence
/// over the lifecycle state; `"done"`/[`AgentState::Done`] / [`AgentState::Idle`]
/// are dimmed.
fn status_style(entry: &SidebarEntry, theme: &ThemeConfig) -> (&'static str, Style) {
    let bg = theme.bg();
    let dim = |c| Style::default().fg(c).bg(bg).add_modifier(Modifier::DIM);
    let lit = |c| Style::default().fg(c).bg(bg);

    if let Some(act) = &entry.activity {
        match act.state.as_str() {
            "working" => return ("\u{25CF}", lit(theme.success())), // ●
            "blocked" => return ("\u{25CF}", lit(theme.error())),   // ●
            "waiting" => return ("\u{25CF}", lit(theme.warning())), // ●
            "done" => return ("\u{2713}", dim(theme.fg())),         // ✓
            _ => {}
        }
    }
    match entry.state {
        AgentState::Running => ("\u{25CF}", lit(theme.success())), // ●
        AgentState::Done(_) => ("\u{2713}", dim(theme.fg())),      // ✓
        AgentState::Failed(_) => ("\u{2717}", lit(theme.error())), // ✗
        AgentState::Idle => ("\u{25CB}", dim(theme.fg())),         // ○
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
    let bg = theme.bg();
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
        let (glyph, style) = status_style(entry, theme);
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
            },
        ];

        let backend = TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orca-tui"), "brand header missing:\n{text}");
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
        }];

        // Narrow enough that nothing fits whole; must truncate every segment
        // without panicking. Inner width (15 minus the right border) leaves room
        // for a recognizable name prefix but not the whole thing.
        let backend = TestBackend::new(16, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme))
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
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orca-tui"), "brand header missing:\n{text}");
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
            },
            SidebarEntry {
                name: "codex".to_owned(),
                state: AgentState::Idle,
                branch: None,
                activity: None,
                focused: false,
            },
        ];

        let backend = TestBackend::new(32, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = ThemeConfig::default();
        let accent = theme.accent();
        let fg = theme.fg();
        terminal
            .draw(|f| render_sidebar(f, f.area(), &entries, &theme))
            .expect("draw");

        let buf = terminal.backend().buffer();
        // Layout: brand row 0, section row 1, first entry row 2. The name
        // starts at column 3 (marker, dot, space). Only a RIGHT border, so the
        // inner area begins at column 0.
        // Focused "claude" name cell is accent; its focus marker is accent.
        let marker = &buf[(0, 2)];
        assert_eq!(
            marker.symbol(),
            "\u{25B6}",
            "focused row should start with the focus marker"
        );
        assert_eq!(marker.fg, accent, "focus marker must be accent");

        let name_cell = &buf[(3, 2)];
        assert_eq!(name_cell.symbol(), "c", "expected 'c' at (3,2)");
        assert_eq!(name_cell.fg, accent, "focused name must be accent-colored");

        // Unfocused "codex" (row 3): blank marker, foreground name.
        let marker2 = &buf[(0, 3)];
        assert_eq!(marker2.symbol(), " ", "unfocused row has a blank marker");
        let name_cell2 = &buf[(3, 3)];
        assert_eq!(name_cell2.symbol(), "c", "expected 'c' at (3,3)");
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
        }];
        let theme = ThemeConfig::default();
        for (w, h) in [(0, 0), (1, 1), (2, 1), (1, 5)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render_sidebar(f, f.area(), &entries, &theme))
                .expect("draw should not panic");
        }
    }
}
