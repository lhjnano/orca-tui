//! # Toast — transient UI messages for errors and status changes
//!
//! A lightweight, non-blocking notification system for surfacing daemon
//! errors, connection state changes, and other transient events in the TUI
//! without disrupting the agent panes.
//!
//! Toasts appear as a single-line overlay strip at the bottom of the screen
//! (above the footer), auto-dismiss after a configurable duration, and
//! stack up to [`MAX_TOASTS`] visible at once. Each toast has a severity
//! (info / warning / error) that drives its color.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use ratatui::Frame;

use crate::config::ThemeConfig;

/// Maximum toasts shown simultaneously (oldest are dropped).
const MAX_TOASTS: usize = 3;
/// Default auto-dismiss duration.
const DEFAULT_TTL: Duration = Duration::from_secs(5);

/// Toast severity — drives the foreground color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    /// Informational (blue/accent).
    Info,
    /// Warning (amber).
    Warning,
    /// Error (red).
    Error,
    /// Success (green).
    Success,
}

impl ToastLevel {
    fn fg(self, theme: &ThemeConfig) -> Color {
        match self {
            Self::Info => theme.accent(),
            Self::Warning => theme.warning(),
            Self::Error => theme.error(),
            Self::Success => theme.success(),
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Info => "●",
            Self::Warning => "●",
            Self::Error => "●",
            Self::Success => "●",
        }
    }
}

/// A single transient message.
#[derive(Debug, Clone)]
pub struct Toast {
    pub level: ToastLevel,
    pub message: String,
    pub created_at: Instant,
    pub ttl: Duration,
}

impl Toast {
    /// Create an info toast.
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Info,
            message: message.into(),
            created_at: Instant::now(),
            ttl: DEFAULT_TTL,
        }
    }

    /// Create a warning toast.
    #[must_use]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Warning,
            message: message.into(),
            created_at: Instant::now(),
            ttl: DEFAULT_TTL,
        }
    }

    /// Create an error toast (longer TTL — errors deserve attention).
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Error,
            message: message.into(),
            created_at: Instant::now(),
            ttl: Duration::from_secs(8),
        }
    }

    /// Create a success toast.
    #[must_use]
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            level: ToastLevel::Success,
            message: message.into(),
            created_at: Instant::now(),
            ttl: Duration::from_secs(3),
        }
    }

    /// Whether this toast has expired (past its TTL).
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.created_at) >= self.ttl
    }
}

/// A collection of toasts with automatic expiry.
#[derive(Debug, Default)]
pub struct ToastQueue {
    toasts: Vec<Toast>,
}

impl ToastQueue {
    /// Create an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self { toasts: Vec::new() }
    }

    /// Push a new toast. If the queue is full, the oldest is dropped.
    pub fn push(&mut self, toast: Toast) {
        if self.toasts.len() >= MAX_TOASTS {
            self.toasts.remove(0);
        }
        self.toasts.push(toast);
    }

    /// Garbage-collect expired toasts.
    pub fn gc(&mut self, now: Instant) {
        self.toasts.retain(|t| !t.is_expired(now));
    }

    /// Whether any toasts are visible.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    /// The number of visible toasts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    /// Render the toast strip at the bottom of the content area (just above
    /// the footer). Each toast is one line: `{glyph} {message}`.
    /// Returns the number of rows it occupied (0 if empty).
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &ThemeConfig) -> u16 {
        if self.toasts.is_empty() || area.height == 0 {
            return 0;
        }
        let rows = self.toasts.len().min(usize::from(area.height)) as u16;
        let toast_area = Rect {
            x: area.x,
            y: area.y.saturating_add(area.height.saturating_sub(rows)),
            width: area.width,
            height: rows,
        };

        let bg = theme.element();
        let buf = frame.buffer_mut();
        for (i, toast) in self.toasts.iter().take(usize::from(rows)).enumerate() {
            let y = toast_area.y.saturating_add(i as u16);
            if y >= toast_area.bottom() {
                break;
            }
            let fg = toast.level.fg(theme);
            // Fill the row background.
            for x in toast_area.x..toast_area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(bg);
            }
            // Glyph.
            let mut x = toast_area.x;
            let glyph = toast.level.glyph();
            for ch in glyph.chars() {
                if x >= toast_area.right() {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(ch);
                cell.set_fg(fg);
                cell.set_bg(bg);
                cell.set_style(Style::default().add_modifier(Modifier::BOLD));
                x += 1;
            }
            // Space.
            if x < toast_area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(bg);
                x += 1;
            }
            // Message (truncated to fit).
            let remaining = (toast_area.right().saturating_sub(x)) as usize;
            let msg: String = toast.message.chars().take(remaining).collect();
            let style = Style::default().fg(fg).bg(bg);
            for ch in msg.chars() {
                if x >= toast_area.right() {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(ch);
                cell.set_style(style);
                x += 1;
            }
        }
        rows
    }

    /// Direct buffer rendering (for use inside a draw closure without Frame).
    pub fn render_buf(&self, buf: &mut Buffer, area: Rect, theme: &ThemeConfig) {
        if self.toasts.is_empty() || area.height == 0 {
            return;
        }
        let bg = theme.element();
        for (i, toast) in self
            .toasts
            .iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            let y = area.y.saturating_add(i as u16);
            if y >= area.bottom() {
                break;
            }
            let fg = toast.level.fg(theme);
            // Fill background.
            for x in area.x..area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(bg);
            }
            // Glyph + message.
            let mut x = area.x;
            for ch in toast.level.glyph().chars() {
                if x >= area.right() {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(ch);
                cell.set_fg(fg);
                cell.set_bg(bg);
                cell.set_style(Style::default().add_modifier(Modifier::BOLD));
                x += 1;
            }
            if x < area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_bg(bg);
                x += 1;
            }
            let remaining = (area.right().saturating_sub(x)) as usize;
            let msg: String = toast.message.chars().take(remaining).collect();
            let style = Style::default().fg(fg).bg(bg);
            for ch in msg.chars() {
                if x >= area.right() {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(ch);
                cell.set_style(style);
                x += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Concatenate the symbols of every cell in row `y` across cols `x0..x1`.
    fn row_symbols(buf: &Buffer, y: u16, x0: u16, x1: u16) -> String {
        (x0..x1).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn toast_levels_have_glyphs() {
        assert_eq!(ToastLevel::Info.glyph(), "●");
        assert_eq!(ToastLevel::Warning.glyph(), "●");
        assert_eq!(ToastLevel::Error.glyph(), "●");
        assert_eq!(ToastLevel::Success.glyph(), "●");
    }

    #[test]
    fn toast_queue_push_and_gc() {
        let mut q = ToastQueue::new();
        assert!(q.is_empty());
        q.push(Toast::info("hello"));
        q.push(Toast::warning("warn"));
        assert_eq!(q.len(), 2);
        assert!(!q.is_empty());

        // Nothing expired yet.
        q.gc(Instant::now());
        assert_eq!(q.len(), 2);

        // After TTL, they expire.
        let later = Instant::now() + Duration::from_secs(10);
        q.gc(later);
        assert!(q.is_empty());
    }

    #[test]
    fn toast_queue_drops_oldest_when_full() {
        let mut q = ToastQueue::new();
        for i in 0..(MAX_TOASTS + 2) {
            q.push(Toast::info(format!("msg-{i}")));
        }
        assert_eq!(q.len(), MAX_TOASTS);
    }

    #[test]
    fn error_toast_has_longer_ttl() {
        let t_info = Toast::info("x");
        let t_error = Toast::error("x");
        assert!(t_error.ttl > t_info.ttl, "error toasts should last longer");
    }

    #[test]
    fn toast_expired_check() {
        let t = Toast::info("test");
        assert!(!t.is_expired(Instant::now()));
        assert!(t.is_expired(Instant::now() + Duration::from_secs(10)));
    }

    #[test]
    fn render_buf_empty_queue_is_noop() {
        let q = ToastQueue::new();
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        q.render_buf(&mut buf, area, &ThemeConfig::default());
        // Nothing should have been written: every cell stays the default space.
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "empty queue must not write ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn render_buf_renders_glyph_and_message() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("hello"));
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        q.render_buf(&mut buf, area, &ThemeConfig::default());

        // Row 0 layout: glyph '●' at col 0, space at col 1, "hello" at cols 2..7.
        assert!(buf[(0, 0)].symbol().contains('●'), "glyph at (0,0)");
        assert_eq!(buf[(1, 0)].symbol(), " ", "space at (1,0)");
        assert_eq!(buf[(2, 0)].symbol(), "h");
        assert_eq!(buf[(3, 0)].symbol(), "e");
        assert_eq!(buf[(4, 0)].symbol(), "l");
        assert_eq!(buf[(5, 0)].symbol(), "l");
        assert_eq!(buf[(6, 0)].symbol(), "o");
        // Untouched rows remain default spaces.
        assert_eq!(buf[(0, 1)].symbol(), " ");
        assert_eq!(buf[(0, 2)].symbol(), " ");
    }

    #[test]
    fn render_buf_renders_multiple_toasts_on_separate_rows() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("first"));
        q.push(Toast::warning("second"));
        q.push(Toast::error("third"));
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        q.render_buf(&mut buf, area, &ThemeConfig::default());

        for (y, expected) in ["first", "second", "third"].iter().enumerate() {
            let row = row_symbols(&buf, y as u16, 0, area.width);
            assert!(row.contains('●'), "row {y} should carry the glyph: {row:?}");
            assert!(
                row.contains(expected),
                "row {y} should contain {expected:?}: {row:?}"
            );
        }
    }

    #[test]
    fn render_buf_truncates_message_to_area_width() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("abcdefghij"));
        // Buffer is wider than the render area so we can detect overflow past
        // `area.right()` (==4) in the cells beyond it.
        let buf_area = Rect::new(0, 0, 10, 1);
        let render_area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(buf_area);
        q.render_buf(&mut buf, render_area, &ThemeConfig::default());

        // glyph '●' at col 0, space at col 1, then only "ab" fits (cols 2,3).
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), "b");
        // Nothing past area.right() (cols 4..10) should have been written.
        for x in 4..buf_area.width {
            assert_eq!(buf[(x, 0)].symbol(), " ", "no overflow at col {x}");
        }
    }

    #[test]
    fn render_buf_zero_height_area_is_noop() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("hello"));
        let area = Rect::new(0, 0, 10, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        // Must return without panicking and without writing anything.
        q.render_buf(&mut buf, area, &ThemeConfig::default());
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }

    #[test]
    fn render_via_frame_returns_expected_row_count() {
        // Empty queue ⇒ 0 rows.
        {
            let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
            let q = ToastQueue::new();
            let mut rows = 999;
            terminal
                .draw(|f| rows = q.render(f, f.area(), &ThemeConfig::default()))
                .unwrap();
            assert_eq!(rows, 0, "empty queue renders 0 rows");
        }
        // 1 toast in a 5-row area ⇒ 1 row.
        {
            let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
            let mut q = ToastQueue::new();
            q.push(Toast::info("hi"));
            let mut rows = 999;
            terminal
                .draw(|f| rows = q.render(f, f.area(), &ThemeConfig::default()))
                .unwrap();
            assert_eq!(rows, 1, "single toast renders 1 row");
        }
        // 2 toasts clamped into a 1-row area ⇒ 1 row.
        {
            let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
            let mut q = ToastQueue::new();
            q.push(Toast::info("a"));
            q.push(Toast::info("b"));
            let mut rows = 999;
            terminal
                .draw(|f| rows = q.render(f, f.area(), &ThemeConfig::default()))
                .unwrap();
            assert_eq!(rows, 1, "2 toasts in a 1-row area clamp to 1");
        }
    }

    #[test]
    fn render_clamps_to_area_height() {
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        let mut q = ToastQueue::new();
        q.push(Toast::info("first"));
        q.push(Toast::warning("second"));
        q.push(Toast::error("third"));

        let mut rows = 999;
        terminal
            .draw(|f| rows = q.render(f, f.area(), &ThemeConfig::default()))
            .unwrap();

        assert_eq!(rows, 1, "only 1 of 3 toasts fits in a 1-row area");
        let buf = terminal.backend().buffer();
        let row = row_symbols(buf, 0, 0, 40);
        // render() takes the first `rows` toasts, so the front one is shown and
        // the newer two are dropped.
        assert!(row.contains("first"), "front toast renders: {row:?}");
        assert!(
            !row.contains("second"),
            "second toast must be dropped: {row:?}"
        );
        assert!(
            !row.contains("third"),
            "third toast must be dropped: {row:?}"
        );
    }

    #[test]
    fn constructors_set_correct_levels_and_ttls() {
        assert_eq!(Toast::info("x").level, ToastLevel::Info);
        assert_eq!(Toast::warning("x").level, ToastLevel::Warning);
        assert_eq!(Toast::error("x").level, ToastLevel::Error);
        assert_eq!(Toast::success("x").level, ToastLevel::Success);

        let info = Toast::info("x");
        let warning = Toast::warning("x");
        let error = Toast::error("x");
        let success = Toast::success("x");

        // info/warning share the default TTL.
        assert_eq!(info.ttl, DEFAULT_TTL);
        assert_eq!(warning.ttl, DEFAULT_TTL);
        // error lasts longer than info; success is shorter than info.
        assert!(
            error.ttl > info.ttl,
            "error TTL ({:?}) should exceed info ({:?})",
            error.ttl,
            info.ttl
        );
        assert!(
            success.ttl < info.ttl,
            "success TTL ({:?}) should be below info ({:?})",
            success.ttl,
            info.ttl
        );
    }

    #[test]
    fn fg_returns_distinct_theme_colors_per_level() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("i"));
        q.push(Toast::error("e"));
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        let theme = ThemeConfig::default();
        q.render_buf(&mut buf, area, &theme);

        // The glyph cell (col 0) carries the level's foreground color.
        let info_fg = buf[(0, 0)].fg;
        let error_fg = buf[(0, 1)].fg;
        assert_ne!(info_fg, error_fg, "info (accent) and error fg must differ");
        assert_eq!(info_fg, theme.accent(), "info glyph fg should be accent");
        assert_eq!(error_fg, theme.error(), "error glyph fg should be error");
    }

    #[test]
    fn push_drops_oldest_when_full() {
        let mut q = ToastQueue::new();
        // MAX_TOASTS + 1 pushes; the very first (index 0) must be evicted.
        for i in 0..(MAX_TOASTS + 1) {
            q.push(Toast::info(format!("oldest-check-{i}")));
        }
        assert_eq!(q.len(), MAX_TOASTS, "queue caps at MAX_TOASTS");

        // `toasts` is private, so verify eviction indirectly by rendering: the
        // dropped "oldest-check-0" must be absent and its successor present.
        let area = Rect::new(0, 0, 40, MAX_TOASTS as u16);
        let mut buf = Buffer::empty(area);
        q.render_buf(&mut buf, area, &ThemeConfig::default());

        let rendered: String = (0..area.height)
            .map(|y| row_symbols(&buf, y, 0, area.width))
            .collect();
        assert!(
            !rendered.contains("oldest-check-0"),
            "oldest toast must be dropped: {rendered:?}"
        );
        assert!(
            rendered.contains("oldest-check-1"),
            "second-oldest should now lead the queue: {rendered:?}"
        );
    }
}
