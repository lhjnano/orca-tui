//! # Agent registry
//!
//! Kinds of supported coding agents (Claude Code, Codex, OpenCode, Gemini,
//! Amp, Cursor), their discovery via `$PATH` lookup, and the per-agent launch
//! spec. This is the data layer behind the agent picker / pane model
//! (Feature 3 of the roadmap).
//!
//! The split mirrors Orca GUI's "run any CLI agent, each in its own
//! worktree": an [`AgentSpec`] carries the verbatim command plus an optional
//! worktree path, and [`AgentKind`] classifies it so the UI can show a
//! recognizable icon/name and offer the installed set in a picker.

use std::fmt;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

/// Supported agent kinds.
///
/// `Generic` is the fallback for any command that doesn't match a known agent
/// binary; it has no fixed binary name (the verbatim command is used as-is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// Anthropic's `claude` CLI ("Claude Code").
    ClaudeCode,
    /// OpenAI's `codex` CLI.
    Codex,
    /// OpenCode (`opencode`).
    OpenCode,
    /// Google's `gemini` CLI.
    Gemini,
    /// Sourcegraph's `amp` CLI.
    Amp,
    /// Cursor CLI (`cursor`).
    Cursor,
    /// Paul Gauthier's `aider` AI pair-programming CLI.
    Aider,
    /// Block's `goose` AI agent CLI.
    Goose,
    /// Charm's `crush` terminal AI coding agent.
    Crush,
    /// Sourcegraph's `cody` agent CLI.
    Cody,
    /// Alibaba's Qwen Code (`qwen`) CLI.
    Qwen,
    /// Anything else; resolved from the verbatim command.
    Generic,
}

impl AgentKind {
    /// Human-readable display name shown in the UI.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini",
            Self::Amp => "Amp",
            Self::Cursor => "Cursor",
            Self::Aider => "Aider",
            Self::Goose => "Goose",
            Self::Crush => "Crush",
            Self::Cody => "Cody",
            Self::Qwen => "Qwen Code",
            Self::Generic => "Custom",
        }
    }

    /// The CLI binary name looked up on `$PATH` (empty for `Generic`).
    #[must_use]
    pub fn binary(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Amp => "amp",
            Self::Cursor => "cursor",
            Self::Aider => "aider",
            Self::Goose => "goose",
            Self::Crush => "crush",
            Self::Cody => "cody",
            Self::Qwen => "qwen",
            Self::Generic => "",
        }
    }

    /// The concrete agent kinds (excludes [`Generic`]) in canonical order.
    #[must_use]
    pub fn all_known() -> &'static [AgentKind] {
        &[
            Self::ClaudeCode,
            Self::Codex,
            Self::OpenCode,
            Self::Gemini,
            Self::Amp,
            Self::Cursor,
            Self::Aider,
            Self::Goose,
            Self::Crush,
            Self::Cody,
            Self::Qwen,
        ]
    }

    /// Scan `$PATH` for installed agents and return the subset present, in
    /// canonical order. `Generic` is never returned.
    ///
    /// Uses [`std::env::split_paths`] which handles the OS-specific path
    /// separator (`:` on unix, `;` on windows) and quoted entries correctly.
    #[must_use]
    pub fn detect_installed() -> Vec<AgentKind> {
        Self::all_known()
            .iter()
            .copied()
            .filter(|kind| binary_on_path(kind.binary()))
            .collect()
    }

    /// Classify a kind from a binary name or path. Falls back to [`Generic`].
    /// Comparison is on the basename so `./claude` and `/usr/bin/claude` both
    /// match `ClaudeCode`.
    fn from_binary(name_or_path: &str) -> Self {
        let base = basename(name_or_path);
        Self::all_known()
            .iter()
            .copied()
            .find(|k| k.binary() == base)
            .unwrap_or(Self::Generic)
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Lifecycle state of an agent process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Not yet started.
    Idle,
    /// Process is running.
    Running,
    /// Process exited. Carries the exit code, if known.
    Done(Option<i32>),
    /// Process failed to start or exited with an error. Carries a message.
    Failed(String),
}

impl AgentState {
    /// Short label for the pane header.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Done(_) => "Done",
            Self::Failed(_) => "Failed",
        }
    }

    /// Single-glyph icon for the pane header.
    #[must_use]
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Idle => "\u{25CB}",      // ○
            Self::Running => "\u{25CF}",   // ●
            Self::Done(_) => "\u{2713}",   // ✓
            Self::Failed(_) => "\u{2717}", // ✗
        }
    }

    /// Color used to tint the pane header / border for this state.
    #[must_use]
    pub fn color(&self) -> Color {
        match self {
            Self::Idle => Color::DarkGray,
            Self::Running => Color::Green,
            Self::Done(_) => Color::Cyan,
            Self::Failed(_) => Color::Red,
        }
    }
}

/// Unified display status — the Orca-GUI vocabulary derived from lifecycle
/// state plus the OSC 9999 activity payload. This is the single source of
/// truth for "what state is this agent in?" used by the sidebar, snapshots,
/// auto-scroll targeting and the jump palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Working,
    Blocked,
    Waiting,
    Interrupted,
    Done,
    Failed,
    Idle,
}

impl AgentStatus {
    /// Derive the display status from the lifecycle state and the (optional)
    /// OSC 9999 activity state string. Priority:
    ///   1. a `Failed` lifecycle always wins (a crash overrides everything);
    ///   2. a recognized OSC state ("working"|"blocked"|"waiting"|"interrupted"
    ///      |"done") wins next (it is more granular than the lifecycle);
    ///   3. otherwise fall back to the lifecycle (Running→Working, Done→Done,
    ///      Idle→Idle).
    #[must_use]
    pub fn derive(state: &AgentState, osc_state: Option<&str>) -> Self {
        if matches!(state, AgentState::Failed(_)) {
            return Self::Failed;
        }
        if let Some(s) = osc_state {
            return match s {
                "working" => Self::Working,
                "blocked" => Self::Blocked,
                "waiting" => Self::Waiting,
                "interrupted" => Self::Interrupted,
                "done" => Self::Done,
                _ => Self::from_lifecycle(state),
            };
        }
        Self::from_lifecycle(state)
    }

    fn from_lifecycle(state: &AgentState) -> Self {
        match state {
            AgentState::Running => Self::Working,
            AgentState::Done(_) => Self::Done,
            AgentState::Idle => Self::Idle,
            AgentState::Failed(_) => Self::Failed,
        }
    }

    /// Status dot glyph (matches the current sidebar glyphs exactly).
    #[must_use]
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Working | Self::Blocked | Self::Waiting | Self::Interrupted => "\u{25CF}", // ●
            Self::Done => "\u{2713}",                                                        // ✓
            Self::Failed => "\u{2717}",                                                      // ✗
            Self::Idle => "\u{25CB}",                                                        // ○
        }
    }

    /// Short lowercase label (Orca vocabulary).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
            Self::Interrupted => "interrupted",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Idle => "idle",
        }
    }

    /// Whether this status counts as "in progress" (the sidebar section header
    /// and auto-scroll targeting). Working/blocked/waiting are all active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Working | Self::Blocked | Self::Waiting | Self::Interrupted
        )
    }

    /// Relative activity ranking for "most active agent" selection (higher =
    /// more active / needs-attention). Used by future auto-scroll + jump
    /// palette default selection.
    #[must_use]
    pub fn priority(&self) -> u8 {
        match self {
            Self::Working => 6,
            Self::Blocked => 5,
            Self::Interrupted => 4,
            Self::Waiting => 3,
            Self::Idle => 2,
            Self::Done => 1,
            Self::Failed => 0,
        }
    }
}

/// Tallied counts of each [`AgentStatus`] across a set of agents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusTally {
    /// Number of agents whose status is [`AgentStatus::Working`].
    pub working: usize,
    /// Number of agents whose status is [`AgentStatus::Blocked`].
    pub blocked: usize,
    /// Number of agents whose status is [`AgentStatus::Interrupted`].
    pub interrupted: usize,
    /// Number of agents whose status is [`AgentStatus::Waiting`].
    pub waiting: usize,
    /// Number of agents whose status is [`AgentStatus::Done`].
    pub done: usize,
    /// Number of agents whose status is [`AgentStatus::Failed`].
    pub failed: usize,
    /// Number of agents whose status is [`AgentStatus::Idle`].
    pub idle: usize,
}

impl StatusTally {
    /// Total number of agents represented by this tally.
    #[must_use]
    pub fn total(&self) -> usize {
        self.working
            + self.blocked
            + self.interrupted
            + self.waiting
            + self.done
            + self.failed
            + self.idle
    }
}

/// Count how many statuses fall into each [`AgentStatus`] bucket.
#[must_use]
pub fn status_tally(statuses: &[AgentStatus]) -> StatusTally {
    let mut t = StatusTally::default();
    for s in statuses {
        match s {
            AgentStatus::Working => t.working += 1,
            AgentStatus::Blocked => t.blocked += 1,
            AgentStatus::Interrupted => t.interrupted += 1,
            AgentStatus::Waiting => t.waiting += 1,
            AgentStatus::Done => t.done += 1,
            AgentStatus::Failed => t.failed += 1,
            AgentStatus::Idle => t.idle += 1,
        }
    }
    t
}

/// Launch specification for one agent.
///
/// `command[0]` is the binary (classified into [`AgentKind`]); the rest are its
/// args. `worktree` is the optional per-agent git worktree (Orca GUI model).
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Classified agent kind.
    pub kind: AgentKind,
    /// Display name (defaults to the kind's name, or the binary basename for
    /// [`AgentKind::Generic`]).
    pub name: String,
    /// Verbatim invocation; `command[0]` is the binary.
    pub command: Vec<String>,
    /// Optional per-agent git worktree directory.
    pub worktree: Option<PathBuf>,
}

impl AgentSpec {
    /// Build a spec from a verbatim command vector.
    ///
    /// `command[0]` is matched against the known agent binaries to pick
    /// [`AgentKind`]; an unknown binary yields [`AgentKind::Generic`]. The
    /// display name defaults to the matched kind's name, or for `Generic` the
    /// binary basename.
    ///
    /// Returns a `Generic` spec with an empty command if the slice is empty.
    #[must_use]
    pub fn from_command(command: Vec<String>) -> Self {
        let kind = command
            .first()
            .map(|bin| AgentKind::from_binary(bin))
            .unwrap_or(AgentKind::Generic);
        let name = match kind {
            AgentKind::Generic => command
                .first()
                .map(|b| basename(b))
                .unwrap_or_else(|| AgentKind::Generic.display_name().to_owned()),
            _ => kind.display_name().to_owned(),
        };
        Self {
            kind,
            name,
            command,
            worktree: None,
        }
    }
}

/// Return the basename of a path-like string (no directory component).
fn basename(s: &str) -> String {
    Path::new(s)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(s)
        .to_owned()
}

/// True if `binary` exists as an executable file somewhere on `$PATH`.
fn binary_on_path(binary: &str) -> bool {
    if binary.is_empty() {
        return false;
    }
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if is_executable(&candidate) {
            return true;
        }
        #[cfg(windows)]
        if is_executable(&dir.join(format!("{binary}.exe"))) {
            return true;
        }
    }
    false
}

/// Whether a path is an executable regular file (unix: any execute bit set).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(md) => md.is_file() && (md.permissions().mode() & 0o111 != 0),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_binary_names() {
        assert_eq!(AgentKind::ClaudeCode.binary(), "claude");
        assert_eq!(AgentKind::Codex.binary(), "codex");
        assert_eq!(AgentKind::OpenCode.binary(), "opencode");
        assert_eq!(AgentKind::Gemini.binary(), "gemini");
        assert_eq!(AgentKind::Amp.binary(), "amp");
        assert_eq!(AgentKind::Cursor.binary(), "cursor");
        assert_eq!(AgentKind::Generic.binary(), "");
    }

    #[test]
    fn display_names() {
        assert_eq!(AgentKind::ClaudeCode.display_name(), "Claude Code");
        assert_eq!(AgentKind::Generic.display_name(), "Custom");
        // Display impl delegates to display_name.
        assert_eq!(format!("{}", AgentKind::Codex), "Codex");
    }

    #[test]
    fn all_known_excludes_generic() {
        let known = AgentKind::all_known();
        assert_eq!(known.len(), 11);
        assert!(!known.contains(&AgentKind::Generic));
        // Canonical order: Claude Code first.
        assert_eq!(known[0], AgentKind::ClaudeCode);
        // Defensive: every known kind has a non-empty binary and display name,
        // catching a future arm that forgets to wire up both methods.
        for k in known {
            assert!(!k.binary().is_empty(), "{k:?} has empty binary()");
            assert!(
                !k.display_name().is_empty(),
                "{k:?} has empty display_name()"
            );
        }
    }

    #[test]
    fn new_agents_classify_from_binary() {
        // Each of the five new binaries must classify to its kind via the
        // shared from_binary path exercised by AgentSpec::from_command.
        let cases = [
            ("aider", AgentKind::Aider),
            ("goose", AgentKind::Goose),
            ("crush", AgentKind::Crush),
            ("cody", AgentKind::Cody),
            ("qwen", AgentKind::Qwen),
        ];
        for (bin, expected) in cases {
            let spec = AgentSpec::from_command(vec![bin.to_owned()]);
            assert_eq!(spec.kind, expected, "binary {bin:?}");
            assert_eq!(spec.name, expected.display_name());
        }
        // Path-style binaries resolve by basename too.
        assert_eq!(
            AgentSpec::from_command(vec!["/usr/local/bin/goose".to_owned()]).kind,
            AgentKind::Goose
        );
    }

    #[test]
    fn detect_installed_returns_subset_and_never_panics() {
        let installed = AgentKind::detect_installed();
        // Must be a subset of all_known, in canonical order, no duplicates.
        let known = AgentKind::all_known();
        for k in &installed {
            assert!(known.contains(k), "{k:?} not a known kind");
            assert!(
                binary_on_path(k.binary()),
                "{} not actually on PATH",
                k.binary()
            );
        }
        // Canonical ordering preserved (stable filter).
        let mut sorted_by_known: Vec<AgentKind> = installed.clone();
        sorted_by_known.sort_by_key(|k| known.iter().position(|x| x == k).unwrap());
        assert_eq!(installed, sorted_by_known);
    }

    #[test]
    fn from_command_matches_known_binary() {
        let spec = AgentSpec::from_command(vec!["claude".to_owned()]);
        assert_eq!(spec.kind, AgentKind::ClaudeCode);
        assert_eq!(spec.name, "Claude Code");
        assert_eq!(spec.command, vec!["claude".to_owned()]);
        assert!(spec.worktree.is_none());
    }

    #[test]
    fn from_command_matches_basename_for_paths() {
        let spec = AgentSpec::from_command(vec!["/usr/local/bin/codex".to_owned()]);
        assert_eq!(spec.kind, AgentKind::Codex);
        assert_eq!(spec.name, "Codex");
    }

    #[test]
    fn from_command_unknown_is_generic_with_basename_name() {
        let spec =
            AgentSpec::from_command(vec!["/opt/weird/my-agent".to_owned(), "--flag".to_owned()]);
        assert_eq!(spec.kind, AgentKind::Generic);
        assert_eq!(spec.name, "my-agent");
        assert_eq!(spec.command.len(), 2);
    }

    #[test]
    fn from_command_empty_is_generic() {
        let spec = AgentSpec::from_command(vec![]);
        assert_eq!(spec.kind, AgentKind::Generic);
        assert!(spec.command.is_empty());
    }

    #[test]
    fn state_colors_and_icons() {
        assert_eq!(AgentState::Idle.color(), Color::DarkGray);
        assert_eq!(AgentState::Running.color(), Color::Green);
        assert_eq!(AgentState::Done(None).color(), Color::Cyan);
        assert_eq!(AgentState::Done(Some(0)).color(), Color::Cyan);
        assert_eq!(AgentState::Failed("boom".to_owned()).color(), Color::Red);

        assert_eq!(AgentState::Idle.icon(), "\u{25CB}");
        assert_eq!(AgentState::Running.icon(), "\u{25CF}");
        assert_eq!(AgentState::Done(None).icon(), "\u{2713}");
        assert_eq!(AgentState::Failed("x".to_owned()).icon(), "\u{2717}");

        assert_eq!(AgentState::Idle.label(), "Idle");
        assert_eq!(AgentState::Running.label(), "Running");
        assert_eq!(AgentState::Done(Some(2)).label(), "Done");
        assert_eq!(AgentState::Failed("e".to_owned()).label(), "Failed");
    }

    // ---- AgentStatus -------------------------------------------------------

    #[test]
    fn status_failed_overrides_everything() {
        // A crash wins regardless of the OSC payload.
        let failed = AgentState::Failed("boom".to_owned());
        assert_eq!(
            AgentStatus::derive(&failed, Some("working")),
            AgentStatus::Failed
        );
        assert_eq!(
            AgentStatus::derive(&failed, Some("done")),
            AgentStatus::Failed
        );
        assert_eq!(AgentStatus::derive(&failed, None), AgentStatus::Failed);
    }

    #[test]
    fn status_each_osc_state_maps_correctly() {
        let running = AgentState::Running;
        assert_eq!(
            AgentStatus::derive(&running, Some("working")),
            AgentStatus::Working
        );
        assert_eq!(
            AgentStatus::derive(&running, Some("blocked")),
            AgentStatus::Blocked
        );
        assert_eq!(
            AgentStatus::derive(&running, Some("waiting")),
            AgentStatus::Waiting
        );
        assert_eq!(
            AgentStatus::derive(&running, Some("done")),
            AgentStatus::Done
        );
    }

    #[test]
    fn status_interrupted_derived_from_osc() {
        assert_eq!(
            AgentStatus::derive(&AgentState::Running, Some("interrupted")),
            AgentStatus::Interrupted
        );
    }

    #[test]
    fn status_lifecycle_fallback_running_to_working() {
        assert_eq!(
            AgentStatus::derive(&AgentState::Running, None),
            AgentStatus::Working
        );
    }

    #[test]
    fn status_lifecycle_fallback_done_to_done() {
        assert_eq!(
            AgentStatus::derive(&AgentState::Done(Some(0)), None),
            AgentStatus::Done
        );
        assert_eq!(
            AgentStatus::derive(&AgentState::Done(None), None),
            AgentStatus::Done
        );
    }

    #[test]
    fn status_lifecycle_fallback_idle_to_idle() {
        assert_eq!(
            AgentStatus::derive(&AgentState::Idle, None),
            AgentStatus::Idle
        );
    }

    #[test]
    fn status_unknown_osc_string_falls_back_to_lifecycle() {
        // An unrecognized OSC state string must not change the lifecycle
        // derivation. Running stays Working, not something arbitrary.
        assert_eq!(
            AgentStatus::derive(&AgentState::Running, Some("thinking")),
            AgentStatus::Working
        );
        // Empty string also falls back (treated as unknown).
        assert_eq!(
            AgentStatus::derive(&AgentState::Running, Some("")),
            AgentStatus::Working
        );
        // Done lifecycle + unknown OSC stays Done.
        assert_eq!(
            AgentStatus::derive(&AgentState::Done(None), Some("zzz")),
            AgentStatus::Done
        );
    }

    #[test]
    fn status_is_active_expectations() {
        assert!(AgentStatus::Working.is_active());
        assert!(AgentStatus::Blocked.is_active());
        assert!(AgentStatus::Waiting.is_active());
        assert!(!AgentStatus::Done.is_active());
        assert!(!AgentStatus::Failed.is_active());
        assert!(!AgentStatus::Idle.is_active());
    }

    #[test]
    fn status_priority_ordering() {
        // Higher = more active / needs-attention.
        assert!(AgentStatus::Working.priority() > AgentStatus::Blocked.priority());
        assert!(AgentStatus::Blocked.priority() > AgentStatus::Waiting.priority());
        assert!(AgentStatus::Waiting.priority() > AgentStatus::Idle.priority());
        assert!(AgentStatus::Idle.priority() > AgentStatus::Done.priority());
        assert!(AgentStatus::Done.priority() > AgentStatus::Failed.priority());
        // A blocked-but-alive agent outranks a finished one.
        assert!(AgentStatus::Blocked.priority() > AgentStatus::Done.priority());
    }

    #[test]
    fn status_icons_match_sidebar_glyphs() {
        assert_eq!(AgentStatus::Working.icon(), "\u{25CF}");
        assert_eq!(AgentStatus::Blocked.icon(), "\u{25CF}");
        assert_eq!(AgentStatus::Waiting.icon(), "\u{25CF}");
        assert_eq!(AgentStatus::Done.icon(), "\u{2713}");
        assert_eq!(AgentStatus::Failed.icon(), "\u{2717}");
        assert_eq!(AgentStatus::Idle.icon(), "\u{25CB}");
    }

    #[test]
    fn status_labels_are_orca_vocabulary() {
        assert_eq!(AgentStatus::Working.label(), "working");
        assert_eq!(AgentStatus::Blocked.label(), "blocked");
        assert_eq!(AgentStatus::Waiting.label(), "waiting");
        assert_eq!(AgentStatus::Done.label(), "done");
        assert_eq!(AgentStatus::Failed.label(), "failed");
        assert_eq!(AgentStatus::Idle.label(), "idle");
    }

    #[test]
    fn status_tally_counts_each_bucket() {
        let statuses = [
            AgentStatus::Working,
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Interrupted,
            AgentStatus::Waiting,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Idle,
            AgentStatus::Done,
        ];
        let t = status_tally(&statuses);
        assert_eq!(t.working, 2);
        assert_eq!(t.blocked, 1);
        assert_eq!(t.interrupted, 1);
        assert_eq!(t.waiting, 1);
        assert_eq!(t.done, 2);
        assert_eq!(t.failed, 1);
        assert_eq!(t.idle, 1);
        assert_eq!(t.total(), statuses.len());
    }

    #[test]
    fn status_tally_empty_is_all_zero() {
        let t = status_tally(&[]);
        assert_eq!(
            t,
            StatusTally {
                working: 0,
                blocked: 0,
                interrupted: 0,
                waiting: 0,
                done: 0,
                failed: 0,
                idle: 0,
            }
        );
        assert_eq!(t.total(), 0);
    }
}
