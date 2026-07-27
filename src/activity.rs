//! In-memory activity timeline (pure logic — no ratatui, no app coupling).
//!
//! Captures timestamped [`ActivityEvent`]s derived from agent lifecycle
//! transitions and OSC 9999 activity payloads into a bounded ring buffer
//! ([`ActivityLog`]). A later task will hook this into the App and render an
//! overlay; this module is deliberately decoupled so it stays unit-testable
//! and dependency-free.

use crate::agent::AgentStatus;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_CAP: usize = 500;

/// Render a [`SystemTime`] as UTC `HH:MM:SS` using only `std`.
fn format_hms(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

/// One timestamped activity event captured from agent lifecycle / OSC output.
#[derive(Debug, Clone)]
pub enum ActivityEvent {
    /// An agent transitioned between display statuses.
    State {
        agent: String,
        from: AgentStatus,
        to: AgentStatus,
        at: SystemTime,
    },
    /// An agent invoked a tool (from OSC 9999 activity).
    Tool {
        agent: String,
        tool: String,
        input: Option<String>,
        at: SystemTime,
    },
    /// An agent errored (exit/launch failure message).
    Error {
        agent: String,
        message: String,
        at: SystemTime,
    },
}

impl ActivityEvent {
    /// The display name of the agent this event concerns.
    #[must_use]
    pub fn agent(&self) -> &str {
        match self {
            Self::State { agent, .. } | Self::Tool { agent, .. } | Self::Error { agent, .. } => {
                agent
            }
        }
    }

    /// When the event occurred.
    #[must_use]
    pub fn at(&self) -> SystemTime {
        match self {
            Self::State { at, .. } | Self::Tool { at, .. } | Self::Error { at, .. } => *at,
        }
    }

    /// Single-line human rendering, e.g. `[12:03:45] claude: working → waiting`.
    #[must_use]
    pub fn render_line(&self) -> String {
        let ts = format_hms(self.at());
        match self {
            Self::State {
                agent, from, to, ..
            } => {
                format!("[{ts}] {agent}: {} → {}", from.label(), to.label())
            }
            Self::Tool {
                agent, tool, input, ..
            } => match input {
                Some(i) => format!("[{ts}] {agent}: {tool}: {i}"),
                None => format!("[{ts}] {agent}: {tool}"),
            },
            Self::Error { agent, message, .. } => {
                format!("[{ts}] {agent}: error: {message}")
            }
        }
    }
}

/// Bounded in-memory timeline of recent [`ActivityEvent`]s (ring buffer).
///
/// Newest events are kept; oldest are evicted once `cap` is exceeded.
#[derive(Debug, Default)]
pub struct ActivityLog {
    cap: usize,
    events: VecDeque<ActivityEvent>,
}

impl ActivityLog {
    /// Create a log with the default capacity ([`DEFAULT_CAP`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_CAP)
    }

    /// Create a log with a custom capacity. `cap` is clamped to `>= 1`.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            events: VecDeque::new(),
        }
    }

    /// Append an event, evicting the oldest if over capacity.
    pub fn record(&mut self, ev: ActivityEvent) {
        self.events.push_back(ev);
        while self.events.len() > self.cap {
            self.events.pop_front();
        }
    }

    /// Current number of retained events.
    // NOTE: keep as `len`/`is_empty` — not a raw `Vec` accessor, so the clippy
    // `len_without_is_empty` convention is satisfied by the paired `is_empty`
    // below.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log holds no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Configured maximum capacity.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The newest `n` events, newest first (for overlay display).
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<&ActivityEvent> {
        self.events.iter().rev().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A fixed instant whose UTC wall-clock is `12:03:45`.
    fn fixed_at() -> SystemTime {
        // 12h * 3600 + 3*60 + 45 = 43425 seconds past midnight UTC.
        UNIX_EPOCH + Duration::from_secs(43_425)
    }

    fn state(agent: &str, from: AgentStatus, to: AgentStatus) -> ActivityEvent {
        ActivityEvent::State {
            agent: agent.to_string(),
            from,
            to,
            at: fixed_at(),
        }
    }

    #[test]
    fn record_appends_and_recent_is_newest_first() {
        let mut log = ActivityLog::new();
        assert!(log.is_empty());
        log.record(state("alpha", AgentStatus::Idle, AgentStatus::Working));
        log.record(state("alpha", AgentStatus::Working, AgentStatus::Waiting));
        log.record(state("beta", AgentStatus::Idle, AgentStatus::Done));

        assert_eq!(log.len(), 3);
        let recent: Vec<&str> = log.recent(10).iter().map(|e| e.agent()).collect();
        // newest first
        assert_eq!(recent, vec!["beta", "alpha", "alpha"]);

        // requesting more than available returns everything available.
        assert_eq!(log.recent(100).len(), 3);
    }

    #[test]
    fn capacity_is_enforced_and_oldest_evicted() {
        let mut log = ActivityLog::with_cap(3);
        for to in [
            AgentStatus::Working,
            AgentStatus::Waiting,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Idle,
        ] {
            log.record(state("claude", AgentStatus::Idle, to));
        }
        assert_eq!(log.len(), 3);

        // The retained 3 are the NEWEST 3 (to == Done, Failed, Idle),
        // returned newest-first by `recent`.
        let lines: Vec<String> = log.recent(3).iter().map(|e| e.render_line()).collect();
        assert_eq!(
            lines,
            vec![
                "[12:03:45] claude: idle → idle",
                "[12:03:45] claude: idle → failed",
                "[12:03:45] claude: idle → done",
            ]
        );
    }

    #[test]
    fn cap_is_clamped_to_at_least_one() {
        let log = ActivityLog::with_cap(0);
        assert_eq!(log.cap(), 1);
    }

    #[test]
    fn render_line_state_contains_arrow() {
        let ev = state("claude", AgentStatus::Working, AgentStatus::Waiting);
        let line = ev.render_line();
        assert!(line.contains("→"), "state line must contain arrow: {line}");
        assert!(line.contains("claude"));
        assert!(line.contains("working"));
        assert!(line.contains("waiting"));
    }

    #[test]
    fn render_line_tool_with_and_without_input() {
        let with_input = ActivityEvent::Tool {
            agent: "codex".to_string(),
            tool: "edit".to_string(),
            input: Some("src/main.rs".to_string()),
            at: fixed_at(),
        };
        assert_eq!(
            with_input.render_line(),
            "[12:03:45] codex: edit: src/main.rs"
        );

        let no_input = ActivityEvent::Tool {
            agent: "codex".to_string(),
            tool: "ls".to_string(),
            input: None,
            at: fixed_at(),
        };
        assert_eq!(no_input.render_line(), "[12:03:45] codex: ls");
        assert!(!no_input.render_line().contains("edit"));
    }

    #[test]
    fn render_line_error_contains_error_marker() {
        let ev = ActivityEvent::Error {
            agent: "claude".to_string(),
            message: "launch failed".to_string(),
            at: fixed_at(),
        };
        let line = ev.render_line();
        assert!(
            line.contains("error:"),
            "error line must contain 'error:' : {line}"
        );
        assert!(line.contains("launch failed"));
        assert!(line.contains("claude"));
    }

    #[test]
    fn timestamp_formatting_is_deterministic() {
        // fixed_at() is exactly 12:03:45 UTC.
        let ev = state("x", AgentStatus::Idle, AgentStatus::Done);
        assert!(
            ev.render_line().starts_with("[12:03:45] "),
            "{}",
            ev.render_line()
        );
    }

    #[test]
    fn agent_accessor_works_for_all_variants() {
        let s = state("alpha", AgentStatus::Idle, AgentStatus::Working);
        let t = ActivityEvent::Tool {
            agent: "beta".to_string(),
            tool: "ls".to_string(),
            input: None,
            at: fixed_at(),
        };
        let e = ActivityEvent::Error {
            agent: "gamma".to_string(),
            message: "boom".to_string(),
            at: fixed_at(),
        };
        assert_eq!(s.agent(), "alpha");
        assert_eq!(t.agent(), "beta");
        assert_eq!(e.agent(), "gamma");
        // at() round-trips
        assert_eq!(s.at(), fixed_at());
        assert_eq!(t.at(), fixed_at());
        assert_eq!(e.at(), fixed_at());
    }
}
