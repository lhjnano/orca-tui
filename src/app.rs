//! # Application state + main loop
//!
//! Top-level Orca TUI application: owns the [`Pane`]s, their [`PtySession`]s,
//! the focused pane, the ratatui [`Terminal`] and the [`AgentBus`](crate::bus)
//! receiver. The [`App::run`] loop is a plain synchronous poll/drain/render
//! cycle — no tokio runtime — which is exactly the right shape for a
//! blocking-PTY, blocking-crossterm-event TUI.
//!
//! ## Threading summary
//!
//! ```text
//!   per session:  std::thread (portable-pty reader)  ──▶  std::mpsc
//!   per session:  std::thread (bus forward_session)  ──▶  tokio mpsc (AgentBus)
//!   main thread:  App::run  ──try_recv──▶  AgentBus receiver  ──▶  Pane.feed
//! ```
//!
//! All three thread classes are synchronous; the tokio channel is used purely
//! as an N→1 mailbox with a sync `try_recv` drain.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::activity::{ActivityEvent, ActivityLog};
use crate::agent::{status_tally, AgentKind, AgentSpec, AgentState, AgentStatus};
use crate::bus::{self, AgentUpdate, AgentUpdateReceiver, AgentUpdateSender};
use crate::config::{Config, LayoutConfig};
use crate::coordinator::{self, Coordinator};
use crate::integrations::RepoRef;
use crate::layout::split_panes;
use crate::mobile::AgentSnapshot;
use crate::pane::Pane;
use crate::pty_session::PtySession;
use crate::scheduler::{FrameScheduler, TARGET_FRAME_60FPS};
use crate::sidebar;
use crate::ssh;
use crate::terminal_emu::{MIN_COLS, MIN_ROWS};
use crate::worktree::{OwnedWorktrees, WorktreeManager};

use tokio::sync::mpsc::UnboundedSender;

/// Input mode — zellij-style. Normal = passthrough, Pane = focus nav,
/// the fuzzy-focus palette (`/` from Pane mode), or the spawn picker
/// (`Ctrl+N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum InputMode {
    #[default]
    Normal,
    Pane,
    /// Fuzzy-focus jump palette: type to filter agents, Enter to focus.
    Jump,
    /// Agent-spawn picker: Up/Down to select, Enter to spawn, Esc to cancel.
    Spawn,
    /// Full-screen activity timeline overlay: any key closes it.
    Activity,
    /// Read-only 3-bucket agent dashboard overlay (Phase 2): groups the
    /// live per-pane statuses into needs-attention / working / done columns.
    /// Opened with `d` in Pane mode; any key dismisses it back to Normal.
    Dashboard,
    /// Sidebar navigation menu: ↑↓ moves between items, Enter dispatches,
    /// Esc returns to Normal. Ctrl+S enters it from any mode.
    Sidebar,
    /// Custom-command text-entry modal (reached from the spawn picker's
    /// "Custom command…" sentinel entry): type a command, Enter spawns it.
    SpawnCustom,
    /// Tasks view (Phase 2): repo-input text modal. Reached from the sidebar
    /// nav hub. User types `owner/name`, Enter fetches open issues + PRs and
    /// switches to [`InputMode::TasksList`].
    TasksRepo,
    /// Tasks view (Phase 2): the fetched issues/PRs list browser. ↑↓ selects,
    /// Enter dispatches a new agent pane with the issue/PR body as the prompt,
    /// Esc returns to Normal.
    TasksList,
    /// Settings overlay (Phase 2): live toggle/cycle of layout, default-agent,
    /// and theme. ↑↓ moves the cursor, Enter/Space toggles the focused row
    /// (applied live to the render), Esc persists the whole config to
    /// `~/.config/orcatui/config.toml` via [`Config::save`] and returns to
    /// Normal.
    Settings,
}

/// The daemon connection state — drives the sidebar indicator + error handling.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// No daemon — orcatui manages PTYs directly (the default/legacy mode).
    #[default]
    Standalone,
    /// Connected to a daemon — panes are backed by daemon sessions.
    Connected,
    /// Was connected, but the daemon disconnected (crash, idle shutdown).
    /// `reason` is the error message; `next_retry` is when to attempt reconnection.
    Disconnected {
        reason: String,
        next_retry: Option<Instant>,
    },
}

impl ConnectionState {
    /// A short label for the sidebar: `● Standalone` / `● Daemon` / etc.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Standalone => "● Standalone",
            Self::Connected => "● Daemon",
            Self::Disconnected { .. } => "✗ Disconnected",
        }
    }

    /// The color for the sidebar label.
    #[must_use]
    pub fn color(&self, theme: &crate::config::ThemeConfig) -> ratatui::style::Color {
        match self {
            Self::Standalone => theme.muted(),
            Self::Connected => theme.success(),
            Self::Disconnected { .. } => theme.error(),
        }
    }

    /// Whether we are in standalone mode (PTYs managed locally).
    #[must_use]
    pub fn is_standalone(&self) -> bool {
        matches!(self, Self::Standalone)
    }
}

#[derive(Debug, Clone, Copy)]
enum FocusDir {
    Up,
    Down,
    Left,
    Right,
}

const FOOTER_NORMAL: &str = " Ctrl+Alt+P: control \u{00B7} Ctrl+Q: quit ";
const FOOTER_PANE: &str = " hjkl: focus \u{00B7} Tab: next \u{00B7} p: pin \u{00B7} x: close \u{00B7} z: zoom \u{00B7} n: new \u{00B7} b: sidebar \u{00B7} s: nav \u{00B7} ?: help \u{00B7} Esc: back ";
const FOOTER_JUMP: &str =
    " type to filter \u{00B7} \u{2191}\u{2193} select \u{00B7} Enter: focus \u{00B7} Esc: cancel ";
const FOOTER_SPAWN: &str = " \u{2191}\u{2193} select \u{00B7} Enter: spawn \u{00B7} Esc: cancel ";
const FOOTER_ACTIVITY: &str = " \u{2191}\u{2193} scroll activity \u{00B7} Esc: close ";
const FOOTER_DASHBOARD: &str = " Esc: close ";
const FOOTER_SIDEBAR: &str =
    " \u{2191}\u{2193} navigate \u{00B7} Enter: select \u{00B7} Esc: back ";
const FOOTER_SPAWN_CUSTOM: &str =
    " type a command \u{00B7} Enter: spawn \u{00B7} Backspace \u{00B7} Esc: cancel ";
const FOOTER_TASKS_REPO: &str =
    " type owner/name \u{00B7} Enter: fetch \u{00B7} Backspace \u{00B7} Esc: cancel ";
const FOOTER_TASKS_LIST: &str =
    " \u{2191}\u{2193} select \u{00B7} Enter: dispatch agent \u{00B7} Esc: back ";
const FOOTER_SETTINGS: &str =
    " \u{2191}\u{2193} select \u{00B7} Enter/Space: toggle \u{00B7} Esc: save & close ";
const FOOTER_ZOOM: &str = " z: unzoom \u{00B7} Ctrl+Q: quit \u{00B7} Ctrl+B: sidebar ";

/// Sidebar nav menu items (index == sidebar_nav). Activity / Tasks / Settings
/// are all implemented (Phase 1 / 2); a hypothetical future fourth item would
/// be the next "coming soon" placeholder.
const SIDEBAR_NAV_ITEMS: &[&str] = &["Activity", "Tasks", "Settings"];

/// Practical minimum pane inner size for agents to render. Below this, the
/// pane is too small for most TUI agents and spawning is blocked.
const MIN_PANE_COLS: u16 = 24;
const MIN_PANE_ROWS: u16 = 5;

/// One row in the Tasks browser (Phase 2). `prompt` is the full string to hand
/// to the dispatched agent (composed via [`crate::integrations::issue_to_prompt`]
/// or [`crate::integrations::pr_to_prompt`]).
#[derive(Debug, Clone)]
struct TasksEntry {
    /// Issue or pull request.
    kind: TaskKind,
    /// GitHub number (e.g. `42`).
    number: u64,
    /// Issue/PR title.
    title: String,
    /// The prompt string to pass to the agent on Enter. For issues this is the
    /// title-only form initially (the body is fetched lazily on dispatch via
    /// [`crate::integrations::fetch_issue`]); for PRs it is the final form.
    prompt: String,
}

/// Which kind of GitHub item a [`TasksEntry`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Issue,
    PullRequest,
}

/// The interactive Orca TUI application.
///
/// Construct with [`App::spawn_agents`], then drive with [`App::run`]. Dropping
/// an `App` restores the terminal (via [`App::setup_terminal`] tracking) and
/// kills/joins every still-running agent PTY (via [`PtySession`]'s `Drop`).
///
/// Generic over the ratatui [`Backend`] so tests can use a `TestBackend` (no
/// TTY required, runs identically on a dev machine and headless CI). The
/// default `B = CrosstermBackend<Stdout>` is the real backend used by `run`.
pub struct App<B: Backend = CrosstermBackend<Stdout>> {
    panes: Vec<Pane>,
    /// `None` once the session's child has exited and been reaped. Kept
    /// parallel to `panes` (same index == same agent).
    sessions: Vec<Option<PtySession>>,
    focus: usize,
    terminal: Terminal<B>,
    bus_rx: AgentUpdateReceiver,
    quit: bool,
    /// True between [`App::setup_terminal`] and [`App::restore_terminal`] so
    /// the `Drop` impl can restore even on a panic mid-loop.
    raw_mode_active: bool,
    /// Worktrees created in `--worktree` mode. Declared LAST so Rust's
    /// field-drop order removes them only after `sessions` (whose
    /// [`PtySession`] `Drop` kills + joins the child processes) — otherwise
    /// removing a directory that is still a live process's cwd would fail.
    /// `None` when not in worktree-isolation mode.
    worktrees: Option<OwnedWorktrees>,
    /// Feature 5: adaptive frame scheduler — throttles rendering to a 60fps
    /// budget, skips frames when behind (backpressure), and backs off the poll
    /// interval when idle (no input/agent output) to save CPU.
    scheduler: FrameScheduler,
    /// Feature 10: optional snapshot publisher. When set (via
    /// [`App::set_snapshot_sender`]), the loop publishes a `Vec<AgentSnapshot>`
    /// every frame so a mobile-companion WebSocket server can broadcast live
    /// agent status. `None` when no companion is attached.
    snapshot_tx: Option<UnboundedSender<Vec<AgentSnapshot>>>,
    /// Feature 7: orchestration state. When `Some`, [`App::main_loop`] pumps
    /// the coordinator each tick — dispatching the next dependency-gated task
    /// to a fresh pane as previous tasks finish. `None` for the plain `run`
    /// path (all agents spawned up front).
    coordinator: Option<Coordinator>,
    /// The agent binary used to run orchestrated tasks (e.g. `claude`).
    orch_agent: Option<String>,
    /// `pane_task[i]` is the coordinator task id pane `i` is running (`None`
    /// for non-orchestrated panes). Kept parallel to `panes`/`sessions`.
    pane_task: Vec<Option<coordinator::TaskId>>,
    /// Daemon session IDs parallel to panes — `None` for standalone panes or
    /// panes that haven't been assigned a daemon session yet.
    daemon_session_ids: Vec<Option<String>>,
    /// Shared session-ID → pane-index map for the daemon stream reader thread.
    /// `None` when not in daemon mode.
    daemon_session_map:
        Option<std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>>,
    /// Reconnect attempt counter (resets to 0 on successful connect).
    daemon_reconnect_attempts: u32,
    /// Current backoff delay (doubles each failure, capped by config).
    daemon_backoff: Duration,
    /// PTY size captured at construction, reused by [`App::spawn_one`].
    cols: u16,
    rows: u16,
    /// Kept (not dropped) so [`App::spawn_one`] can fan new sessions onto the
    /// bus mid-run. Completion is detected via [`App::all_sessions_gone`], not
    /// channel disconnect, so retaining this sender is safe.
    bus_tx: AgentUpdateSender,
    /// The argv each pane was launched with (captured so a dropped remote
    /// session can be re-spawned verbatim by Feature 8 reconnect).
    pane_command: Vec<Vec<String>>,
    /// Feature 8: per-pane reconnect state. `Some` when the pane is eligible
    /// for auto-reconnect (`--reconnect`); `None` otherwise.
    reconnect: Vec<Option<ssh::ReconnectSession>>,
    /// Feature 8: when non-`None`, the pane is awaiting a backoff-scheduled
    /// respawn at this `Instant`. Drained by [`App::pump_reconnect`].
    reconnect_due: Vec<Option<Instant>>,
    /// Action item #6: per-pane pin flag, parallel to `panes` (`pinned[i]`
    /// ↔ pane `i`). A pinned agent renders in a dedicated "PINNED" sidebar
    /// section above "IN PROGRESS". Toggled in Pane mode with `p`.
    pinned: Vec<bool>,
    /// User configuration (theme, layout, default agent).
    config: Config,
    /// Current input mode (Normal = passthrough, Pane = focus navigation).
    mode: InputMode,
    /// User override for sidebar visibility (Ctrl+B toggles). `false` = let the
    /// adaptive auto-hide logic decide; `true` = force-hidden regardless of width.
    sidebar_hidden: bool,
    /// Jump-palette (mode = Jump) state: the current filter query.
    jump_query: String,
    /// Jump-palette: selected index into the filtered agent list.
    jump_selected: usize,
    /// Spawn-picker: selected index into the agent options list.
    spawn_selected: usize,
    /// Custom-command modal (`InputMode::SpawnCustom`): the in-progress
    /// command string the user is typing. Shell-split on Enter.
    custom_cmd: String,
    /// When true, the focused pane fills the entire content area (other panes
    /// keep running, just not visible). Toggled with `z` in Pane mode.
    zoomed: bool,
    /// When true, a full-screen help overlay is rendered. Toggled with `?` in
    /// Pane mode (or `F1`). Any key dismisses it.
    show_help: bool,
    /// Daemon connection state — drives the sidebar indicator + error handling.
    conn_state: ConnectionState,
    /// Transient UI messages (daemon errors, connection changes, etc.).
    toasts: crate::toast::ToastQueue,
    /// The daemon client when connected to an Orca daemon (None in standalone).
    daemon: Option<crate::orca_daemon::DaemonClient>,
    /// In-memory activity timeline (state transitions + errors). Rendered as a
    /// full-screen overlay via `InputMode::Activity`.
    activity: ActivityLog,
    /// Per-pane previously-derived display status, parallel to `panes`. Used by
    /// [`App::record_activity`] to detect transitions (only record on change).
    last_status: Vec<Option<AgentStatus>>,
    /// Sidebar nav menu selected index (0 = Activity, 1 = Tasks, 2 = Settings).
    /// See [`SIDEBAR_NAV_ITEMS`]. Drives `InputMode::Sidebar` dispatch.
    sidebar_nav: usize,
    /// Last-rendered OUTER pane rects (one per pane, in `panes` order), cached
    /// by [`App::render`] for mouse hit-testing in [`App::handle_mouse`].
    /// Empty until the first frame is drawn.
    pane_rects: Vec<Rect>,
    /// Phase 2 — Tasks view: the in-progress `owner/name` repo string the user
    /// is typing in [`InputMode::TasksRepo`].
    tasks_repo_input: String,
    /// Phase 2 — Tasks view: the parsed repo once submitted (set on Enter in
    /// [`InputMode::TasksRepo`], consumed for lazy body-fetch on dispatch).
    tasks_repo: Option<RepoRef>,
    /// Phase 2 — Tasks view: the fetched issues + PRs being browsed in
    /// [`InputMode::TasksList`]. Empty until a successful fetch.
    tasks_items: Vec<TasksEntry>,
    /// Phase 2 — Tasks view: selected index into `tasks_items`.
    tasks_selected: usize,
    /// Phase 2 — Tasks view: a fetch error (bad repo, gh failure, no network).
    /// When `Some`, the [`InputMode::TasksList`] overlay shows the error
    /// message + "press Esc" instead of the item list.
    tasks_error: Option<String>,
    /// Phase 2 — Settings overlay: the focused row index (0..=3). 0 = Sidebar,
    /// 1 = Status bar, 2 = Default agent, 3 = Theme. Drives the ▶ cursor in
    /// [`InputMode::Settings`].
    settings_cursor: usize,
}

impl App {
    /// Spawn one pane per [`AgentSpec`] and wire each onto a fresh AgentBus.
    ///
    /// Initial pane/PTY size is read from the real terminal (via
    /// `crossterm::terminal::size`, which works without raw mode); if it cannot
    /// be queried a `80 \u{00D7} 24` fallback is used. A spawn failure is
    /// **not** fatal: the pane is recorded with [`AgentState::Failed`] and the
    /// app continues with the agents that did start (mirrors Orca GUI showing a
    /// red pane for an agent that could not launch).
    ///
    /// # Errors
    ///
    /// Returns an error only if the ratatui terminal/backend cannot be
    /// constructed.
    pub fn spawn_agents(specs: Vec<AgentSpec>, cwd: Option<&Path>, isolate: bool) -> Result<Self> {
        // Size the PTYs from the real terminal so the agent's first frame is
        // already correct once we enter raw mode + alt screen in `run`.
        // `unwrap_or` only covers the error path; a freshly spawned PTY (or a
        // non-TTY) can legitimately report `Ok((0, 0))`, which would underflow
        // `vt100` at `scroll_bottom = size.rows - 1`. Clamp explicitly.
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);

        let (bus_tx, bus_rx) = bus::channel();

        // In worktree-isolation mode, create one git worktree per agent from
        // the repo at `cwd` (or the current directory). Fail fast: a worktree
        // creation failure aborts the whole run, and `OwnedWorktrees`'s `Drop`
        // cleans up any worktrees already created before the error propagates.
        let mut owned: Option<OwnedWorktrees> = if isolate {
            let base = cwd
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let manager = WorktreeManager::open(&base).with_context(|| {
                format!(
                    "opening git repo for worktree isolation at {}",
                    base.display()
                )
            })?;
            Some(OwnedWorktrees::new(manager))
        } else {
            None
        };

        let mut panes = Vec::with_capacity(specs.len());
        let mut sessions = Vec::with_capacity(specs.len());
        let mut pane_task: Vec<Option<coordinator::TaskId>> = Vec::with_capacity(specs.len());
        let mut pane_command: Vec<Vec<String>> = Vec::with_capacity(specs.len());
        let mut reconnect: Vec<Option<ssh::ReconnectSession>> = Vec::with_capacity(specs.len());
        let mut reconnect_due: Vec<Option<Instant>> = Vec::with_capacity(specs.len());
        let mut daemon_session_ids: Vec<Option<String>> = Vec::with_capacity(specs.len());
        // Action item #6: one pin flag per spec, default unpinned. Captured
        // before `specs.into_iter()` moves it.
        let pinned: Vec<bool> = vec![false; specs.len()];

        for (idx, spec) in specs.into_iter().enumerate() {
            let name = spec.name.clone();
            let command = spec.command.clone();
            // Per-agent cwd + header branch: an isolated worktree when in
            // worktree mode (each agent runs in its own worktree, header shows
            // the isolation branch); else the shared `cwd` + any spec label.
            let (agent_cwd, branch_label): (Option<PathBuf>, Option<String>) =
                if let Some(owned) = owned.as_mut() {
                    let wt = owned
                        .create_for(&name)
                        .with_context(|| format!("creating worktree for {name:?}"))?;
                    (Some(wt.path.clone()), Some(wt.branch.clone()))
                } else {
                    (
                        cwd.map(PathBuf::from),
                        spec.worktree.as_ref().map(|p| p.display().to_string()),
                    )
                };
            match PtySession::spawn(command.clone(), agent_cwd.as_deref(), cols, rows) {
                Ok((session, rx)) => {
                    let mut pane = Pane::new(idx, &name, cols, rows);
                    pane.set_state(AgentState::Running);
                    if let Some(branch) = branch_label {
                        pane.set_branch(Some(branch));
                    }
                    panes.push(pane);
                    // Pump this session's blocking receiver onto the async bus
                    // on a dedicated thread. The clone of `bus_tx` is the only
                    // sender this forwarder holds; when it returns, that clone
                    // drops and (once all forwarders are gone) the receiver
                    // observes disconnect — the UI's "all agents gone" signal.
                    let tx = bus_tx.clone();
                    let _ = thread::Builder::new()
                        .name(format!("orca-bus-fwd({name})"))
                        .spawn(move || bus::forward_session(idx, rx, tx));
                    sessions.push(Some(session));
                }
                Err(err) => {
                    eprintln!("orcatui: failed to spawn {name:?}: {err:#}");
                    let mut pane = Pane::new(idx, &name, cols, rows);
                    pane.set_state(AgentState::Failed(format!("{err:#}")));
                    panes.push(pane);
                    sessions.push(None);
                }
            }
            // Plain `run` panes are not coordinated; reconnect is opt-in later.
            pane_task.push(None);
            pane_command.push(command);
            reconnect.push(None);
            reconnect_due.push(None);
            daemon_session_ids.push(None);
        }

        // NOTE: we keep `bus_tx` (stored on the App) rather than dropping it,
        // so `spawn_one` can add orchestrated panes mid-run.

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).context("building ratatui terminal")?;

        Ok(Self {
            panes,
            sessions,
            focus: 0,
            terminal,
            bus_rx,
            quit: false,
            raw_mode_active: false,
            worktrees: owned,
            scheduler: FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now()),
            snapshot_tx: None,
            coordinator: None,
            orch_agent: None,
            pane_task,
            daemon_session_ids,
            daemon_session_map: None,
            daemon_reconnect_attempts: 0,
            daemon_backoff: Duration::from_secs(Config::default().daemon.reconnect_initial_secs),
            cols,
            rows,
            bus_tx,
            pane_command,
            reconnect,
            reconnect_due,
            pinned,
            config: Config::load_or_default(),
            mode: InputMode::Normal,
            sidebar_hidden: false,
            jump_query: String::new(),
            jump_selected: 0,
            spawn_selected: 0,
            custom_cmd: String::new(),
            zoomed: false,
            show_help: false,
            conn_state: ConnectionState::Standalone,
            toasts: crate::toast::ToastQueue::new(),
            daemon: None,
            activity: ActivityLog::new(),
            last_status: Vec::new(),
            sidebar_nav: 0,
            pane_rects: Vec::new(),
            tasks_repo_input: String::new(),
            tasks_repo: None,
            tasks_items: Vec::new(),
            tasks_selected: 0,
            tasks_error: None,
            settings_cursor: 0,
        })
    }
}

// Everything below is generic over the backend so a `TestBackend` can be
// injected in tests (no TTY required) — the production type is still
// `App<CrosstermBackend<Stdout>>` via the default type parameter.
impl<B: Backend> App<B> {
    /// Attach a mobile-companion snapshot publisher (Feature 10). Once set,
    /// [`App::main_loop`] publishes a `Vec<AgentSnapshot>` every frame; the
    /// WebSocket server drains it and broadcasts to connected clients.
    pub fn set_snapshot_sender(&mut self, tx: UnboundedSender<Vec<AgentSnapshot>>) {
        self.snapshot_tx = Some(tx);
    }

    /// Feature 7: attach a coordinator + agent binary so the loop dispatches
    /// tasks dependency-gated (one new pane per released task, as its deps
    /// complete). Call after [`App::spawn_agents`] (typically with an empty
    /// spec list — the initial tasks are spawned by the first loop tick) and
    /// before [`App::run`].
    pub fn set_orchestration(&mut self, coordinator: Coordinator, agent: String) {
        self.coordinator = Some(coordinator);
        self.orch_agent = Some(agent);
    }

    /// Publish the current per-pane snapshot if a companion is attached.
    /// Best-effort: a dropped/lagged receiver makes `send` return `Err`, which
    /// we ignore (the companion is optional and may have disconnected).
    fn publish_snapshot(&self) {
        let Some(tx) = &self.snapshot_tx else {
            return;
        };
        let snaps: Vec<AgentSnapshot> = self
            .panes
            .iter()
            .map(|p| AgentSnapshot {
                name: p.name().to_string(),
                state: p.state().label().to_string(),
                branch: p.branch().map(str::to_string),
            })
            .collect();
        let _ = tx.send(snaps);
    }

    /// Enter raw mode + the alternate screen, run the main loop, then
    /// **always** restore the terminal (even on error). A [`Drop`] impl guards
    /// the panic path as well.
    ///
    /// # Errors
    ///
    /// Propagates crossterm/ratatui I/O errors (e.g. stdout is not a TTY).
    pub fn run(&mut self) -> Result<()> {
        self.setup_terminal()?;
        let result = self.main_loop();
        // Always attempt teardown; surface it only if the loop itself succeeded.
        if let Err(restore_err) = self.restore_terminal() {
            if result.is_ok() {
                return Err(restore_err);
            }
            eprintln!("orcatui: terminal restore failed: {restore_err:#}");
        }
        result
    }

    fn setup_terminal(&mut self) -> Result<()> {
        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .context("entering alt screen + mouse")?;
        self.raw_mode_active = true;
        Ok(())
    }

    fn restore_terminal(&mut self) -> Result<()> {
        let mut stdout = io::stdout();
        // DisableMouseCapture MUST run before LeaveAlternateScreen — otherwise
        // the terminal stays in mouse-capture mode after exit and mouse events
        // leak through as garbage characters in the shell.
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        let disable = disable_raw_mode();
        let show = self.terminal.show_cursor();
        self.raw_mode_active = false;
        disable.context("disabling raw mode")?;
        show.context("showing cursor")?;
        Ok(())
    }

    /// The synchronous frame loop: reap exited children, drain the bus, render
    /// (throttled to the frame budget), poll one event, repeat until quit or
    /// every agent has exited. Feature 5's [`FrameScheduler`] drives the render
    /// rate (skip on backlog) and the poll timeout (idle backoff).
    fn main_loop(&mut self) -> Result<()> {
        while !self.quit {
            // Reap FIRST: poll each still-tracked child's real exit code before
            // draining the bus. The forwarder only knows the child is "gone"
            // (it emits `Exit{code: None}` on PTY EOF); the authoritative exit
            // code comes from `try_wait`. Running the sweep before `drain_bus`
            // means a reaped `Exit{Some(code)}` lands before (and is not
            // clobbered by) the forwarder's less-informed `Exit{None}`.
            self.reap_exited();
            let had_output = self.drain_bus();
            // After all updates (reap + bus drain) have landed on the panes,
            // capture any lifecycle/state transitions into the activity log.
            self.record_activity();

            let now = Instant::now();
            // Fresh agent output counts as activity (keeps the scheduler out of
            // idle backoff while agents are producing).
            if had_output {
                self.scheduler.record_activity(now);
            }

            // GC expired toasts every frame.
            self.toasts.gc(now);

            // Render only when the frame budget allows; otherwise note a skip
            // (backpressure: render the latest state next frame, never catch up).
            if self.scheduler.should_render(now) {
                self.render()?;
                self.scheduler.record_render(now);
            } else {
                self.scheduler.note_skipped();
            }

            // Feature 10: publish a live snapshot of every pane to the
            // mobile-companion channel (no-op when none is attached).
            self.publish_snapshot();

            // Feature 7: pump orchestration — dispatch any tasks whose deps
            // just completed. Done BEFORE the all-sessions-gone check so the
            // initial tasks spawn on tick 1 (an orchestrated App starts empty)
            // and so newly-freed dependents keep the loop alive.
            self.pump_orchestration();

            // Feature 8: fire any backoff-scheduled remote-session reconnects
            // whose wait has elapsed (non-blocking).
            self.pump_reconnect();

            // Daemon reconnection: if we were connected and the daemon crashed,
            // attempt to reconnect on an exponential backoff.
            self.pump_daemon_reconnect();

            // Poll with the scheduler-chosen timeout: ~remaining-to-next-frame
            // when active, the longer idle interval when nothing is happening.
            if event::poll(self.scheduler.poll_timeout(now))? {
                let ev = event::read()?;
                // User input is activity — exit idle backoff immediately.
                self.scheduler.record_activity(Instant::now());
                self.handle_event(ev);
            }
            // Auto-exit once no agent process remains AND orchestration has no
            // pending/running tasks left AND no reconnect is awaiting its
            // backoff. A user can also quit explicitly with Esc / Ctrl+C.
            if self.all_sessions_gone()
                && self.orchestration_drained()
                && self.reconnect_due.iter().all(|d| d.is_none())
            {
                break;
            }
        }
        Ok(())
    }

    /// True when orchestration is inactive (plain `run`) or the coordinator
    /// has no non-terminal tasks left (everything Done/Failed). Combined with
    /// [`App::all_sessions_gone`] this is the orchestrated run's completion
    /// signal: nothing left to spawn and nothing still running.
    fn orchestration_drained(&self) -> bool {
        match &self.coordinator {
            None => true,
            Some(coord) => coord.tasks().iter().all(|t| t.status.is_terminal()),
        }
    }

    /// Drain every pending [`AgentUpdate`] into the panes in one batch.
    /// Returns `true` if any update was applied (used by the scheduler to
    /// detect activity).
    fn drain_bus(&mut self) -> bool {
        let mut any = false;
        while let Ok(update) = self.bus_rx.try_recv() {
            self.apply_update(update);
            any = true;
        }
        any
    }

    /// Capture agent lifecycle transitions (and the error message when an agent
    /// transitions INTO `Failed`) into [`ActivityLog`].
    ///
    /// Runs once per loop tick, right after [`App::reap_exited`] +
    /// [`App::drain_bus`] have settled every pane's state for this frame. Only
    /// **changes** are recorded (compared against `last_status`), so a steady
    /// agent produces no events.
    ///
    /// Tool events from the OSC `toolName` payload are deliberately NOT recorded
    /// here: a pane's `activity()` persists across ticks (it is only overwritten
    /// when a new OSC 9999 payload arrives), so recording per-tick would spam
    /// duplicate `Tool` events. State + Error transitions are the high-signal
    /// set; per-tool dedup can be added (via a `last_tool` field) in a follow-up.
    fn record_activity(&mut self) {
        // Keep the per-pane previous-status vec in lockstep with `panes` so a
        // mid-run `spawn_one` (which appends to `panes`) doesn't desync it.
        self.last_status.resize(self.panes.len(), None);
        for (i, pane) in self.panes.iter().enumerate() {
            let name = pane.name().to_string();
            let new = AgentStatus::derive(pane.state(), pane.activity().map(|a| a.state.as_str()));
            let prev = self.last_status[i];
            if prev == Some(new) {
                continue;
            }
            // Only emit a State event when there IS a previous status — the very
            // first derivation (prev == None) establishes the baseline silently.
            if let Some(from) = prev {
                self.activity.record(ActivityEvent::State {
                    agent: name.clone(),
                    from,
                    to: new,
                    at: SystemTime::now(),
                });
            }
            // An error event whenever an agent transitions INTO Failed.
            if matches!(new, AgentStatus::Failed) {
                let message = match pane.state() {
                    AgentState::Failed(m) => m.clone(),
                    _ => String::from("failed"),
                };
                self.activity.record(ActivityEvent::Error {
                    agent: name.clone(),
                    message,
                    at: SystemTime::now(),
                });
            }
            self.last_status[i] = Some(new);
        }
    }

    /// Poll each still-tracked child for its real exit code and feed it back as
    /// an [`AgentUpdate::Exit`] with `code: Some(_)`.
    ///
    /// The bus forwarder only learns that the child is "gone" (PTY EOF); it
    /// emits [`AgentUpdate::Exit`] with `code: None`. The authoritative exit
    /// code lives in the child handle and is only available via
    /// [`PtySession::try_wait`]. This sweep — run once per loop tick (the loop
    /// already polls every ~20 ms) — closes that gap so a non-zero exit shows
    /// as `Failed` rather than a misleading `Done`.
    ///
    /// Borrow-safe two-pass design: snapshot which panes are already terminal
    /// (immutable borrow of `panes`) and collect reaped codes, *then* mutate
    /// `sessions`; only after both immutable reads are dropped do we call
    /// `apply_update` (which mutates `sessions`/`panes`). A session whose slot
    /// is already `None` (reaped by the forwarder's `Exit`) or whose pane is
    /// already terminal is skipped, so a single child is reaped at most once.
    fn reap_exited(&mut self) {
        // Pass 1 (immutable `panes`): which panes are already in a terminal
        // state? Computed before the mutable `sessions` borrow below.
        let terminal: Vec<bool> = self
            .panes
            .iter()
            .map(|p| matches!(p.state(), AgentState::Done(_) | AgentState::Failed(_)))
            .collect();

        // Pass 2 (mutable `sessions`): non-blocking `try_wait` on each live,
        // non-terminal child; collect the codes.
        let mut reaped: Vec<(usize, i32)> = Vec::new();
        for (i, slot) in self.sessions.iter_mut().enumerate() {
            if *terminal.get(i).unwrap_or(&true) {
                continue;
            }
            let Some(session) = slot.as_mut() else {
                continue;
            };
            match session.try_wait() {
                Ok(Some(code)) => reaped.push((i, code)),
                Ok(None) => {} // still running — leave it
                Err(_) => {}   // poll error (e.g. already-reaped fd); leave it
            }
        }

        // Pass 3: apply each reaped exit. `apply_update`'s Exit branch is
        // idempotent, so a duplicate (forwarder `None` after this `Some`) is a
        // harmless no-op.
        for (pane_id, code) in reaped {
            self.apply_update(AgentUpdate::Exit {
                pane_id,
                code: Some(code),
            });
        }
    }

    /// Apply a single update to the matching pane/session.
    fn apply_update(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::Output { pane_id, bytes } => {
                // Answer the agent's terminal-capability queries (OSC color,
                // DECRQM, DA, DCS terminfo) so probing agents (opencode/OpenTUI)
                // render instead of going blank waiting for a reply. The bytes
                // are still fed to the emulator below (the responder only reads).
                let responses = match self.panes.get_mut(pane_id) {
                    Some(pane) => {
                        let r = pane.scan_queries(&bytes, &self.config.theme);
                        pane.feed(&bytes);
                        // Optional live debug log (ORCA_DEBUG_LOG=1): did the
                        // bytes reach the emulator, and does vt100 have content?
                        if std::env::var("ORCA_DEBUG_LOG").is_ok() {
                            let cells = pane
                                .emulator()
                                .grid()
                                .iter()
                                .map(|row| row.iter().filter(|c| c.has_contents()).count())
                                .sum::<usize>();
                            let mut f = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("/tmp/orca-live.log")
                                .unwrap();
                            use std::io::Write;
                            let _ = writeln!(
                                f,
                                "pane {pane_id}: +{} bytes → emulator now has {cells} non-empty cells",
                                bytes.len()
                            );
                        }
                        r
                    }
                    None => Vec::new(),
                };
                if !responses.is_empty() && std::env::var("ORCA_NO_RESPOND").is_err() {
                    if let Some(Some(session)) = self.sessions.get_mut(pane_id) {
                        let _ = session.write_bytes(&responses);
                    }
                }
            }
            AgentUpdate::State { pane_id, state } => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.set_state(state);
                }
            }
            AgentUpdate::Exit { pane_id, code } => {
                // Feature 8: if this pane is reconnect-eligible and still has
                // retries left, schedule a backoff respawn instead of marking
                // it terminal. (Reconnect is for `run --remote`, not orchestrate,
                // so skipping report_done here is correct.)
                let schedule_reconnect = self
                    .reconnect
                    .get(pane_id)
                    .and_then(|o| o.as_ref())
                    .is_some_and(|rs| !rs.exhausted());
                if schedule_reconnect {
                    let now = Instant::now();
                    if let Some(Some(rs)) = self.reconnect.get_mut(pane_id) {
                        rs.record_failure(now);
                        // Just-failed → full backoff for this attempt.
                        let backoff = rs.next_retry_in(now).unwrap_or(Duration::ZERO);
                        if let Some(d) = self.reconnect_due.get_mut(pane_id) {
                            *d = Some(now + backoff);
                        }
                    }
                    if let Some(pane) = self.panes.get_mut(pane_id) {
                        let attempt = self
                            .reconnect
                            .get(pane_id)
                            .and_then(|o| o.as_ref())
                            .map(|rs| rs.attempts())
                            .unwrap_or(0);
                        pane.set_state(AgentState::Failed(format!(
                            "reconnecting (attempt {attempt})…"
                        )));
                    }
                    if let Some(slot) = self.sessions.get_mut(pane_id) {
                        slot.take();
                    }
                    return;
                }
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    // Idempotent: once a pane is in a terminal state (Done /
                    // Failed) it must not be overwritten by a later Exit. The
                    // forwarder's `Exit{code: None}` (PTY EOF, unknown code)
                    // frequently arrives AFTER the reap sweep's
                    // `Exit{code: Some(_)}` (real exit code); keeping the
                    // existing, more-informed state avoids downgrading a
                    // `Failed(non-zero)` back to a bland `Done(None)` (or vice
                    // versa). Only transition a pane that is still non-terminal.
                    let already_terminal =
                        matches!(pane.state(), AgentState::Done(_) | AgentState::Failed(_));
                    if !already_terminal {
                        // code 0 / unknown => Done; a non-zero exit is a failure.
                        let state = match code {
                            Some(0) | None => AgentState::Done(code),
                            Some(c) => AgentState::Failed(format!("exit code {c}")),
                        };
                        pane.set_state(state);
                    }
                }
                // Take the session so its Drop runs (kill + join the reader),
                // guaranteeing no process/thread is leaked once the child is
                // gone. `Option::take` is idempotent: a second Exit for the
                // same pane (forwarder None after reap Some) finds the slot
                // already None and is a no-op.
                if let Some(slot) = self.sessions.get_mut(pane_id) {
                    slot.take();
                }
                // Feature 7: if this pane ran an orchestrated task, report its
                // completion to the coordinator so dependent tasks can be
                // dispatched on the next pump.
                if let Some(tid) = self.pane_task.get(pane_id).copied().flatten() {
                    let failed = self
                        .panes
                        .get(pane_id)
                        .map(|p| matches!(p.state(), AgentState::Failed(_)))
                        .unwrap_or(false);
                    let summary = if failed {
                        "failed".to_string()
                    } else {
                        "done".to_string()
                    };
                    if let Some(coord) = self.coordinator.as_mut() {
                        coord.report_done(tid, summary);
                    }
                }
            }
        }
    }

    /// Append a new agent pane mid-run (Feature 7 orchestration). In daemon
    /// mode, sends a `createOrAttach` RPC instead of spawning a local PTY.
    /// Returns the new pane index.
    fn spawn_one(&mut self, spec: AgentSpec) -> usize {
        if self.daemon.is_some() {
            return self.spawn_one_daemon(spec);
        }
        self.spawn_one_local(spec)
    }

    /// Daemon-mode spawn: sends `createOrAttach` RPC, feeds the snapshot to the
    /// pane emulator, and registers the session ID in the stream reader map.
    fn spawn_one_daemon(&mut self, spec: AgentSpec) -> usize {
        use crate::orca_daemon::DaemonError;
        let idx = self.panes.len();
        let name = spec.name.clone();
        let command = spec.command.clone();
        let cols = self.cols;
        let rows = self.rows;
        let session_id = format!(
            "orcatui-{}-{}",
            idx,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        let daemon = self.daemon.as_mut().unwrap();
        match daemon.rpc(
            "createOrAttach",
            serde_json::json!({
                "sessionId": session_id,
                "cols": cols,
                "rows": rows,
                "command": command.first().cloned().unwrap_or_default(),
            }),
        ) {
            Ok(resp) => {
                let mut pane = Pane::new(idx, &name, cols, rows);
                pane.set_state(AgentState::Running);
                // Feed the snapshot (if any) to restore the terminal state.
                if let Some(snap) = resp.get("snapshot").and_then(|v| v.as_str()) {
                    pane.feed(snap.as_bytes());
                }
                self.panes.push(pane);
                self.sessions.push(None); // no local PTY in daemon mode
                self.daemon_session_ids.push(Some(session_id.clone()));
                // Register in the stream reader's session map.
                if let Some(map) = &self.daemon_session_map {
                    map.lock().unwrap().insert(session_id, idx);
                }
            }
            Err(e) => {
                let reason = match &e {
                    DaemonError::Disconnected { reason } => {
                        self.conn_state = ConnectionState::Disconnected {
                            reason: reason.clone(),
                            next_retry: Some(Instant::now() + Duration::from_secs(3)),
                        };
                        self.daemon = None;
                        reason.clone()
                    }
                    _ => e.to_string(),
                };
                self.toasts.push(crate::toast::Toast::error(format!(
                    "Failed to create daemon session: {reason}"
                )));
                let mut pane = Pane::new(idx, &name, cols, rows);
                pane.set_state(AgentState::Failed(reason));
                self.panes.push(pane);
                self.sessions.push(None);
                self.daemon_session_ids.push(None);
            }
        }
        self.pane_task.push(None);
        self.pane_command.push(command);
        self.reconnect.push(None);
        self.reconnect_due.push(None);
        self.pinned.push(false);
        // Note: daemon_session_ids is already pushed inside the match above.
        idx
    }

    /// Standalone-mode spawn: creates a local PTY via portable-pty.
    fn spawn_one_local(&mut self, spec: AgentSpec) -> usize {
        let idx = self.panes.len();
        let name = spec.name.clone();
        let command = spec.command.clone();
        let cols = self.cols;
        let rows = self.rows;
        match PtySession::spawn(spec.command, None, cols, rows) {
            Ok((session, rx)) => {
                let mut pane = Pane::new(idx, &name, cols, rows);
                pane.set_state(AgentState::Running);
                self.panes.push(pane);
                let tx = self.bus_tx.clone();
                let _ = thread::Builder::new()
                    .name(format!("orca-bus-fwd({name})"))
                    .spawn(move || bus::forward_session(idx, rx, tx));
                self.sessions.push(Some(session));
            }
            Err(err) => {
                // Do NOT eprintln here — we are inside the raw-mode TUI, so a
                // stderr write would corrupt the display. The Failed pane state
                // below carries the error into the header + sidebar instead.
                let mut pane = Pane::new(idx, &name, cols, rows);
                pane.set_state(AgentState::Failed(format!("{err:#}")));
                self.panes.push(pane);
                self.sessions.push(None);
            }
        }
        self.pane_task.push(None);
        self.pane_command.push(command);
        self.reconnect.push(None);
        self.reconnect_due.push(None);
        self.pinned.push(false);
        self.daemon_session_ids.push(None);
        idx
    }

    /// Feature 8: mark every current pane eligible for auto-reconnect (used
    /// with `--remote --reconnect`). A dropped remote session is then re-spawned
    /// on its own pane after an exponential backoff, up to the policy's max.
    /// Try to connect to an Orca GUI daemon (--daemon flag). On success,
    /// switches to daemon mode (agent input is forwarded via RPC). On failure,
    /// falls back to standalone silently (no daemon found) or with a toast
    /// (daemon found but connection rejected).
    pub fn try_connect_daemon(&mut self) {
        use crate::orca_daemon::{DaemonClient, DaemonConnectOptions, DaemonError};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let opts = DaemonConnectOptions {
            rpc_timeout: Duration::from_secs(self.config.daemon.rpc_timeout_secs),
            hello_timeout: Duration::from_secs(self.config.daemon.hello_timeout_secs),
        };
        match DaemonClient::try_connect_with(opts) {
            None => {
                // No daemon socket found — silent standalone fallback.
            }
            Some(Ok(mut client)) => {
                let pid = client.identity().pid;

                // Build the session-ID → pane-index map for the stream reader.
                let session_map: Arc<Mutex<HashMap<String, usize>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                for (i, sid) in self.daemon_session_ids.iter().enumerate() {
                    if let Some(sid) = sid {
                        session_map.lock().unwrap().insert(sid.clone(), i);
                    }
                }

                // Take the stream socket and start a reader thread.
                if let Some(mut stream) = client.take_stream() {
                    // Store the map so spawn_one_daemon can register new sessions.
                    self.daemon_session_map = Some(Arc::clone(&session_map));
                    let map = Arc::clone(&session_map);
                    let tx = self.bus_tx.clone();
                    let _ = thread::Builder::new()
                        .name("orca-daemon-stream".to_string())
                        .spawn(move || {
                            use crate::orca_daemon::{DaemonClient, FrameType};
                            loop {
                                match DaemonClient::read_stream_frame(&mut stream) {
                                    Ok(frame) => {
                                        match frame.ftype {
                                            FrameType::Data => {
                                                // Parse NDJSON {sessionId, data} or treat as raw for pane 0.
                                                let (pane_id, bytes) =
                                                    parse_stream_data(&frame.payload, &map);
                                                let _ =
                                                    tx.send(AgentUpdate::Output { pane_id, bytes });
                                            }
                                            FrameType::Event => {
                                                // Parse NDJSON event (exit, etc.).
                                                if let Some((pane_id, code)) =
                                                    parse_stream_event(&frame.payload, &map)
                                                {
                                                    let _ = tx.send(AgentUpdate::Exit {
                                                        pane_id,
                                                        code: Some(code),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    Err(DaemonError::Disconnected { reason }) => {
                                        // Signal all panes as exited.
                                        let ids: Vec<usize> = {
                                            let m = map.lock().unwrap();
                                            m.values().copied().collect()
                                        };
                                        for id in ids {
                                            let _ = tx.send(AgentUpdate::Exit {
                                                pane_id: id,
                                                code: None,
                                            });
                                        }
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                        });
                }

                self.daemon = Some(client);
                self.conn_state = ConnectionState::Connected;
                self.daemon_reconnect_attempts = 0;
                self.daemon_backoff =
                    Duration::from_secs(self.config.daemon.reconnect_initial_secs);
                self.toasts.push(crate::toast::Toast::success(format!(
                    "Connected to Orca daemon (pid {pid})"
                )));
            }
            Some(Err(e)) => {
                let reason = match &e {
                    DaemonError::Connect(_) => "daemon socket unreachable".to_string(),
                    DaemonError::HelloRejected { message } => format!("rejected: {message}"),
                    DaemonError::Disconnected { reason } => reason.clone(),
                    _ => e.to_string(),
                };
                self.toasts.push(crate::toast::Toast::warning(format!(
                    "Daemon connect failed ({reason}). Running standalone."
                )));
            }
        }
    }

    pub fn enable_reconnect(&mut self) {
        let policy = ssh::ReconnectPolicy::default();
        for slot in &mut self.reconnect {
            *slot = Some(ssh::ReconnectSession::new(policy.clone()));
        }
    }

    /// Feature 8: re-spawn pane `i`'s command into the same pane (preserving
    /// its emulator/scrollback), used when a scheduled reconnect comes due.
    fn respawn(&mut self, i: usize) {
        let Some(command) = self.pane_command.get(i).cloned() else {
            return;
        };
        let cols = self.cols;
        let rows = self.rows;
        match PtySession::spawn(command, None, cols, rows) {
            Ok((session, rx)) => {
                if let Some(slot) = self.sessions.get_mut(i) {
                    *slot = Some(session);
                }
                if let Some(pane) = self.panes.get_mut(i) {
                    pane.set_state(AgentState::Running);
                }
                let tx = self.bus_tx.clone();
                let _ = thread::Builder::new()
                    .name(format!("orca-reconnect({i})"))
                    .spawn(move || bus::forward_session(i, rx, tx));
            }
            Err(err) => {
                // No eprintln (would corrupt the TUI); the Failed state carries it.
                if let Some(pane) = self.panes.get_mut(i) {
                    pane.set_state(AgentState::Failed(format!("reconnect failed: {err:#}")));
                }
            }
        }
    }

    /// Feature 8: fire any backoff-scheduled reconnects whose wait has elapsed
    /// and that haven't exhausted their retry budget. Non-blocking — the loop
    /// keeps rendering other panes while a remote one waits to come back.
    fn pump_reconnect(&mut self) {
        let now = Instant::now();
        // Collect indices due for respawn first to avoid borrow conflicts.
        let mut due: Vec<usize> = Vec::new();
        for (i, slot) in self.reconnect.iter_mut().enumerate() {
            let Some(rs) = slot.as_mut() else {
                continue;
            };
            let Some(&deadline) = self.reconnect_due.get(i).and_then(|o| o.as_ref()) else {
                continue;
            };
            if now >= deadline {
                if rs.exhausted() {
                    // Give up: clear the schedule, leave the pane terminal.
                    if let Some(d) = self.reconnect_due.get_mut(i) {
                        d.take();
                    }
                } else {
                    due.push(i);
                }
            }
        }
        for i in due {
            // Respawn resets the attempt counter for the next drop.
            if let Some(slot) = self.reconnect.get_mut(i) {
                if let Some(rs) = slot.as_mut() {
                    rs.record_success();
                }
            }
            if let Some(d) = self.reconnect_due.get_mut(i) {
                d.take();
            }
            self.respawn(i);
        }
    }

    /// Daemon reconnection pump. Called once per loop tick. When in
    /// `Disconnected` state and the retry deadline has elapsed, attempts to
    /// reconnect. On success: rebuilds the session map and pushes a success
    /// toast. On failure: doubles the backoff (capped at 30s) and tries again.
    fn pump_daemon_reconnect(&mut self) {
        use crate::orca_daemon::{DaemonClient, DaemonError};
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        // Only act when in Disconnected state with a retry deadline.
        let next_retry = match &self.conn_state {
            ConnectionState::Disconnected { next_retry, .. } => *next_retry,
            _ => return,
        };
        let Some(deadline) = next_retry else {
            return; // no retry scheduled (max attempts exhausted)
        };
        if Instant::now() < deadline {
            return; // not yet time
        }

        // Attempt reconnection.
        match DaemonClient::try_connect() {
            None => {
                // Daemon disappeared entirely — give up, go standalone.
                self.conn_state = ConnectionState::Standalone;
                self.toasts.push(crate::toast::Toast::warning(
                    "Daemon gone. Switched to standalone.",
                ));
            }
            Some(Ok(mut client)) => {
                let pid = client.identity().pid;

                // Rebuild the session-ID → pane-index map.
                let session_map: Arc<Mutex<HashMap<String, usize>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                for (i, sid) in self.daemon_session_ids.iter().enumerate() {
                    if let Some(sid) = sid {
                        session_map.lock().unwrap().insert(sid.clone(), i);
                    }
                }

                // Start a fresh stream reader thread.
                if let Some(mut stream) = client.take_stream() {
                    self.daemon_session_map = Some(Arc::clone(&session_map));
                    let map = Arc::clone(&session_map);
                    let tx = self.bus_tx.clone();
                    let _ = thread::Builder::new()
                        .name("orca-daemon-stream".to_string())
                        .spawn(move || {
                            use crate::orca_daemon::{DaemonClient, FrameType};
                            loop {
                                match DaemonClient::read_stream_frame(&mut stream) {
                                    Ok(frame) => match frame.ftype {
                                        FrameType::Data => {
                                            let (pane_id, bytes) =
                                                parse_stream_data(&frame.payload, &map);
                                            let _ = tx.send(AgentUpdate::Output { pane_id, bytes });
                                        }
                                        FrameType::Event => {
                                            if let Some((pane_id, code)) =
                                                parse_stream_event(&frame.payload, &map)
                                            {
                                                let _ = tx.send(AgentUpdate::Exit {
                                                    pane_id,
                                                    code: Some(code),
                                                });
                                            }
                                        }
                                    },
                                    Err(DaemonError::Disconnected { .. }) => {
                                        let ids: Vec<usize> = {
                                            let m = map.lock().unwrap();
                                            m.values().copied().collect()
                                        };
                                        for id in ids {
                                            let _ = tx.send(AgentUpdate::Exit {
                                                pane_id: id,
                                                code: None,
                                            });
                                        }
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                        });
                }

                self.daemon = Some(client);
                self.conn_state = ConnectionState::Connected;
                self.daemon_reconnect_attempts = 0;
                self.daemon_backoff =
                    Duration::from_secs(self.config.daemon.reconnect_initial_secs);
                self.toasts.push(crate::toast::Toast::success(format!(
                    "Reconnected to Orca daemon (pid {pid})"
                )));
            }
            Some(Err(e)) => {
                // Still down — increase backoff (configurable cap).
                self.daemon_reconnect_attempts += 1;
                let max = self.config.daemon.reconnect_max_attempts;
                if max > 0 && self.daemon_reconnect_attempts >= max {
                    // Exhausted — give up, go standalone.
                    self.conn_state = ConnectionState::Standalone;
                    self.daemon = None;
                    self.toasts.push(crate::toast::Toast::warning(
                        "Daemon reconnect attempts exhausted. Switched to standalone.",
                    ));
                    return;
                }
                let prev_reason = match &self.conn_state {
                    ConnectionState::Disconnected { reason, .. } => reason.clone(),
                    _ => String::new(),
                };
                // Exponential backoff using config-driven initial/max.
                let cap = Duration::from_secs(self.config.daemon.reconnect_max_secs);
                let next_delay = (self.daemon_backoff * 2).min(cap);
                self.daemon_backoff = next_delay;
                self.conn_state = ConnectionState::Disconnected {
                    reason: prev_reason,
                    next_retry: Some(Instant::now() + next_delay),
                };
                self.toasts.push(crate::toast::Toast::warning(format!(
                    "Reconnect failed ({e}). Retrying in {}s.",
                    next_delay.as_secs()
                )));
            }
        }
    }

    /// Feature 7: release every dependency-gated task the coordinator can
    /// dispatch right now, each to its own new pane. Called once per loop tick
    /// so a task whose dependencies just completed spawns on the next frame.
    fn pump_orchestration(&mut self) {
        if self.coordinator.is_none() {
            return;
        }
        let agent = self.orch_agent.clone().unwrap_or_default();
        loop {
            let dispatch = self
                .coordinator
                .as_mut()
                .and_then(|c| c.dispatch_next(&[agent.clone()]));
            let Some(dispatch) = dispatch else {
                break;
            };
            let tid = dispatch.task_id;
            let mut spec = AgentSpec::from_command(vec![agent.clone(), dispatch.prompt.clone()]);
            spec.name = format!("task-{tid}");
            if let Some(c) = self.coordinator.as_mut() {
                c.mark_in_progress(tid);
            }
            let pane_id = self.spawn_one(spec);
            if let Some(slot) = self.pane_task.get_mut(pane_id) {
                *slot = Some(tid);
            }
        }
    }

    /// Render the panes grid plus the footer. Per-pane viewport + PTY sizes are
    /// reconciled here (before the immutable draw closure) so the emulator and
    /// the agent process agree on dimensions.
    fn render(&mut self) -> Result<()> {
        let size = self.terminal.size()?;
        // Edge-to-edge — no outer margin, so the layout fills the terminal and
        // feels dense ("꽉찬") like opencode. Double borders on panes provide
        // the visual separation that the margin/spacing previously gave.
        let total = Rect::new(0, 0, size.width, size.height);

        // Reserve the left sidebar (Orca-style agent list with status dots +
        // live activity from OSC 9999). Hidden when sidebar_width is 0 or the
        // terminal is too narrow for panes to be usable.
        let sidebar_w = self.config.layout.sidebar_width;
        let show_sidebar = sidebar_w > 0 && !self.sidebar_hidden && total.width > sidebar_w + 22;
        let (sidebar_area, content_area) = if show_sidebar {
            // spacing(1) adds a 1-cell gap between the sidebar and the pane
            // area — the panes' own Double borders provide the visual boundary,
            // so the sidebar needs no right border of its own.
            let h = Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(1)])
                .spacing(1)
                .split(total);
            (Some(h[0]), h[1])
        } else {
            (None, total)
        };

        let reserve_footer = total.height >= 3 && self.config.layout.show_status_bar;
        let (pane_area, footer_area) = if reserve_footer {
            let chunks =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(content_area);
            (chunks[0], chunks[1])
        } else {
            (content_area, Rect::default())
        };

        // In zoom mode, the focused pane fills the entire pane area.
        let zoomed = self.zoomed && self.focus < self.panes.len();
        let rects = if zoomed {
            vec![pane_area]
        } else {
            split_panes(pane_area, self.panes.len())
        };
        // Cache the OUTER pane rects so handle_mouse can hit-test against the
        // exact panes that were last drawn (before the borrow-holding draw
        // closure below).
        self.pane_rects = rects.clone();

        for (i, pane) in self.panes.iter_mut().enumerate() {
            // In zoom mode, skip all panes except the focused one.
            if zoomed && i != self.focus {
                continue;
            }
            let rect = if zoomed {
                pane_area
            } else {
                rects.get(i).copied().unwrap_or_default()
            };
            let inner_w = rect.width.saturating_sub(2).max(MIN_COLS);
            let inner_h = rect.height.saturating_sub(2).max(MIN_ROWS);
            let (cur_w, cur_h) = pane.size();
            if (cur_w, cur_h) != (inner_w, inner_h) {
                if std::env::var("ORCA_DEBUG_LOG").is_ok() {
                    use std::io::Write;
                    let has_session = self.sessions.get(i).and_then(|o| o.as_ref()).is_some();
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("/tmp/orca-live.log")
                    {
                        let _ = writeln!(f, "RESIZE pane {i}: emulator {cur_w}x{cur_h} → {inner_w}x{inner_h}; session_present={has_session}");
                    }
                }
                pane.resize_viewport(inner_w, inner_h);
                if let Some(Some(session)) = self.sessions.get_mut(i) {
                    let r = session.resize(inner_w, inner_h);
                    if std::env::var("ORCA_DEBUG_LOG").is_ok() {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("/tmp/orca-live.log")
                        {
                            let _ = writeln!(
                                f,
                                "  session.resize({inner_w}x{inner_h}) = {}",
                                if r.is_ok() { "OK" } else { "ERR" }
                            );
                        }
                    }
                }
            }
        }

        // Build sidebar entries (owned Vec, no borrow held across the closure).
        let sidebar_entries: Vec<sidebar::SidebarEntry> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, p)| sidebar::SidebarEntry {
                name: p.name().to_string(),
                state: p.state().clone(),
                branch: p.branch().map(String::from),
                activity: p.activity().cloned(),
                focused: i == self.focus,
                pinned: self.pinned.get(i).copied().unwrap_or(false),
            })
            .collect();

        let focus = self.focus;
        let zoomed_render = zoomed;
        let show_help = self.show_help;
        let mode = self.mode;
        // Snapshot the jump-palette state (computed before the mutable `panes`
        // borrow so the draw closure can render the overlay without touching self).
        let jump_open = mode == InputMode::Jump;
        let jump_filtered_idx: Vec<usize> = if jump_open {
            self.jump_filtered()
        } else {
            Vec::new()
        };
        let jump_query = self.jump_query.clone();
        let jump_selected = self.jump_selected;
        let spawn_open = mode == InputMode::Spawn;
        let spawn_opts: Vec<(String, String)> = if spawn_open {
            self.spawn_options()
                .into_iter()
                .map(|(name, cmd)| (name, cmd.first().cloned().unwrap_or_default()))
                .collect()
        } else {
            Vec::new()
        };
        let spawn_selected = self.spawn_selected;
        // Snapshot the custom-command modal state so the draw closure never
        // touches `self.custom_cmd` (borrow-checker safety on the 60fps path).
        let custom_open = mode == InputMode::SpawnCustom;
        let custom_cmd_view = self.custom_cmd.clone();
        // Snapshot the activity timeline BEFORE the mutable `panes` borrow below
        // so the draw closure never touches `self.activity` (borrow-checker
        // safety on the 60fps render path). Owned Strings only.
        let activity_open = mode == InputMode::Activity;
        let activity_lines: Vec<String> = if activity_open {
            self.activity
                .recent(usize::from(total.height))
                .iter()
                .map(|e| e.render_line())
                .collect()
        } else {
            Vec::new()
        };
        // Snapshot the sidebar-nav popup state so the draw closure never
        // touches `self.sidebar_nav` (borrow-checker safety on the 60fps path).
        let sidebar_open = mode == InputMode::Sidebar;
        let sidebar_selected = self.sidebar_nav;
        // Snapshot the Tasks view state (Phase 2) BEFORE the mutable `panes`
        // borrow so the draw closure never touches the tasks_* fields.
        let tasks_repo_open = mode == InputMode::TasksRepo;
        let tasks_repo_input_view = self.tasks_repo_input.clone();
        let tasks_list_open = mode == InputMode::TasksList;
        let tasks_items_view: Vec<TasksEntry> = if tasks_list_open {
            self.tasks_items.clone()
        } else {
            Vec::new()
        };
        let tasks_selected_view = self.tasks_selected;
        let tasks_error_view = self.tasks_error.clone();
        // Snapshot the Settings overlay state (Phase 2) BEFORE the mutable
        // `panes` borrow so the draw closure never touches the config / cursor.
        // All four row display-values are derived here as owned Strings.
        let settings_open = mode == InputMode::Settings;
        let settings_cursor = self.settings_cursor;
        let settings_sidebar_on = self.config.layout.sidebar_width > 0;
        let settings_status_bar = self.config.layout.show_status_bar;
        let settings_default_agent = self.config.default_agent.clone();
        // Match the live theme against a preset by accent-hex equality; if none
        // match (a hand-edited custom theme), label it "Custom" so the user
        // still sees *something* informative and the cycle has a defined start.
        let settings_theme_name = Config::theme_presets()
            .iter()
            .find(|(_, t)| t.accent.eq_ignore_ascii_case(&self.config.theme.accent))
            .map(|(n, _)| (*n).to_string())
            .unwrap_or_else(|| "Custom".to_string());
        // Snapshot the dashboard overlay state (Phase 2) BEFORE the mutable
        // `panes` borrow so the draw closure never touches `self.panes` (borrow-
        // checker safety on the 60fps path). Derived from each pane's lifecycle
        // state + OSC 9999 activity, exactly like the sidebar tallies.
        let dashboard_open = mode == InputMode::Dashboard;
        let dashboard_entries: Vec<(String, AgentStatus)> = if dashboard_open {
            self.panes
                .iter()
                .map(|p| {
                    let status =
                        AgentStatus::derive(p.state(), p.activity().map(|a| a.state.as_str()));
                    (p.name().to_string(), status)
                })
                .collect()
        } else {
            Vec::new()
        };
        let panes = &mut self.panes;
        let theme = &self.config.theme;
        // Agent status tallies for the footer status bar (opencode-style).
        let statuses: Vec<AgentStatus> = sidebar_entries
            .iter()
            .map(|e| AgentStatus::derive(&e.state, e.activity.as_ref().map(|a| a.state.as_str())))
            .collect();
        let tally = status_tally(&statuses);
        self.terminal.draw(|f| {
            if let Some(sb) = sidebar_area {
                let conn_status = match self.conn_state {
                    ConnectionState::Standalone => Some(("● Standalone", theme.muted())),
                    ConnectionState::Connected => Some(("● Daemon", theme.success())),
                    ConnectionState::Disconnected { .. } => Some(("✗ Disconnected", theme.error())),
                };
                sidebar::render_sidebar(f, sb, &sidebar_entries, theme, conn_status);
                // Fill the gap between sidebar and panes with the theme bg so it
                // isn't a terminal-default strip (background everywhere).
                let gw = content_area.x.saturating_sub(sb.right());
                if gw > 0 {
                    let gap = Rect::new(sb.right(), total.y, gw, total.height);
                    f.buffer_mut()
                        .set_style(gap, Style::default().bg(theme.bg()));
                }
            }
            for (i, pane) in panes.iter_mut().enumerate() {
                if zoomed_render && i != focus {
                    continue;
                }
                let area = if zoomed_render {
                    pane_area
                } else {
                    rects.get(i).copied().unwrap_or_default()
                };
                pane.render(f, area, i == focus, theme);
            }
            if reserve_footer {
                // opencode-style status bar: left = agent tally, right = key-hint.
                let foot = Layout::horizontal([Constraint::Min(1), Constraint::Length(66)])
                    .split(footer_area);
                let mut status_spans: Vec<Span> = Vec::new();
                if tally.working > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} working ", tally.working),
                        Style::default().fg(theme.success()).bg(theme.panel()),
                    ));
                }
                if tally.blocked > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} blocked ", tally.blocked),
                        Style::default().fg(theme.error()).bg(theme.panel()),
                    ));
                }
                if tally.interrupted > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} interrupted ", tally.interrupted),
                        Style::default().fg(theme.accent()).bg(theme.panel()),
                    ));
                }
                if tally.waiting > 0 {
                    status_spans.push(Span::styled(
                        format!(" ● {} waiting ", tally.waiting),
                        Style::default().fg(theme.warning()).bg(theme.panel()),
                    ));
                }
                if tally.failed > 0 {
                    status_spans.push(Span::styled(
                        format!(" ✗ {} failed ", tally.failed),
                        Style::default().fg(theme.error()).bg(theme.panel()),
                    ));
                }
                if tally.done > 0 {
                    status_spans.push(Span::styled(
                        format!(" ✓ {} done ", tally.done),
                        Style::default().fg(theme.muted()).bg(theme.panel()),
                    ));
                }
                let status_line = Line::from(status_spans);
                f.render_widget(
                    Paragraph::new(status_line).style(Style::default().bg(theme.panel())),
                    foot[0],
                );
                let hint = if zoomed_render {
                    FOOTER_ZOOM
                } else {
                    match mode {
                        InputMode::Pane => FOOTER_PANE,
                        InputMode::Jump => FOOTER_JUMP,
                        InputMode::Spawn => FOOTER_SPAWN,
                        InputMode::SpawnCustom => FOOTER_SPAWN_CUSTOM,
                        InputMode::TasksRepo => FOOTER_TASKS_REPO,
                        InputMode::TasksList => FOOTER_TASKS_LIST,
                        InputMode::Settings => FOOTER_SETTINGS,
                        InputMode::Activity => FOOTER_ACTIVITY,
                        InputMode::Dashboard => FOOTER_DASHBOARD,
                        InputMode::Sidebar => FOOTER_SIDEBAR,
                        InputMode::Normal => FOOTER_NORMAL,
                    }
                };
                f.render_widget(
                    Paragraph::new(hint)
                        .style(Style::default().bg(theme.panel()).fg(theme.accent()))
                        .alignment(Alignment::Right),
                    foot[1],
                );
            }
            // Jump palette overlay (drawn last, on top of everything).
            if jump_open {
                use ratatui::style::Modifier;
                use ratatui::text::Span;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let n = jump_filtered_idx.len();
                let pop_h = (n as u16 + 3).min(total.height.saturating_sub(4)).max(5);
                let pop_w = total.width.min(64).max(40);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(
                        Line::from(" Jump to agent ").style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);

                let mut lines: Vec<Line> = Vec::new();
                // Query line with a block cursor.
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("/{}", jump_query),
                        Style::default().fg(theme.accent()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]));
                lines.push(Line::default());
                for (i, &pane_idx) in jump_filtered_idx.iter().enumerate() {
                    if i as u16 + 3 > inner.height {
                        break;
                    }
                    let name = panes
                        .get(pane_idx)
                        .map(|p| p.name().to_string())
                        .unwrap_or_default();
                    let selected = i == jump_selected;
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    let prefix = if selected { "▶ " } else { "  " };
                    lines.push(Line::from(format!("{prefix}{name}")).style(style));
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Spawn picker overlay (drawn on top, like the jump palette).
            if spawn_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                // ~3 items per 12 rows of terminal height, clamped to [2, 6]
                let max_visible = ((total.height / 12) as usize).clamp(2, 6);
                let n = spawn_opts.len();
                let visible = n.min(max_visible);
                let pop_h = (visible as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(48).max(30);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(Line::from(" New pane ").style(Style::default().fg(theme.accent())));
                f.render_widget(&block, pop);
                let inner = block.inner(pop);

                // Scroll offset: keep the selected item visible.
                let scroll = spawn_selected.saturating_sub(max_visible.saturating_sub(1));

                let mut lines: Vec<Line> = Vec::new();
                for (i, (name, cmd_first)) in spawn_opts
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(usize::from(inner.height))
                {
                    let selected = i == spawn_selected;
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    let prefix = if selected { "▶ " } else { "  " };
                    let line = if cmd_first.is_empty() || cmd_first == name {
                        format!("{prefix}{name}")
                    } else {
                        format!("{prefix}{name:<12} {cmd_first}")
                    };
                    lines.push(Line::from(line).style(style));
                }
                // Scroll indicator.
                if n > max_visible {
                    let more_below = spawn_selected + 1 < n;
                    let more_above = scroll > 0;
                    let indicator = match (more_above, more_below) {
                        (true, true) => " ↑↓ more ",
                        (true, false) => " ↑ end ",
                        (false, true) => " ↓ more ",
                        (false, false) => "",
                    };
                    if !indicator.is_empty() {
                        lines.push(Line::from(indicator).style(Style::default().fg(theme.muted())));
                    }
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Activity timeline overlay (drawn on top, any key to close).
            if activity_open {
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(
                        Line::from(" Activity (any key to close) ")
                            .style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);
                // `activity_lines` is newest-first (recent() returns newest-first),
                // so the most recent transition renders at the top.
                let lines: Vec<Line> = if activity_lines.is_empty() {
                    vec![Line::from("(no activity yet)").style(Style::default().fg(theme.muted()))]
                } else {
                    activity_lines
                        .iter()
                        .take(usize::from(inner.height))
                        .map(|s| Line::from(s.as_str()).style(Style::default().fg(theme.fg())))
                        .collect()
                };
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Agent dashboard overlay (Phase 2): a read-only 3-bucket board
            // grouping the live per-pane statuses into needs-attention /
            // working / done columns. Any key dismisses it back to Normal.
            if dashboard_open {
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(
                        Line::from(" Agent Dashboard (any key to close) ")
                            .style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);
                // Split the inner area into 3 equal columns. Using Min(1) × 3
                // fills evenly regardless of odd widths (ratatui distributes any
                // remainder rather than leaving a gap).
                let cols = Layout::horizontal([
                    Constraint::Min(1),
                    Constraint::Min(1),
                    Constraint::Min(1),
                ])
                .split(inner);

                // Status → bucket mapping (see docs/ROADMAP.md Phase 2):
                //   needs-attention = Blocked | Interrupted | Failed
                //   working         = Working  | Waiting     | Idle
                //   done            = Done
                let mut needs: Vec<&str> = Vec::new();
                let mut working_b: Vec<&str> = Vec::new();
                let mut done: Vec<&str> = Vec::new();
                for (name, status) in &dashboard_entries {
                    match status {
                        AgentStatus::Blocked | AgentStatus::Interrupted | AgentStatus::Failed => {
                            needs.push(name.as_str());
                        }
                        AgentStatus::Working | AgentStatus::Waiting | AgentStatus::Idle => {
                            working_b.push(name.as_str());
                        }
                        AgentStatus::Done => done.push(name.as_str()),
                    }
                }

                let buckets: [(&str, &Vec<&str>, ratatui::style::Color); 3] = [
                    ("needs-attention", &needs, theme.error()),
                    ("working", &working_b, theme.success()),
                    ("done", &done, theme.muted()),
                ];
                for (i, (label, members, color)) in buckets.into_iter().enumerate() {
                    let mut lines: Vec<Line> = Vec::new();
                    // Header: "label (count)" in the bucket color.
                    lines.push(
                        Line::from(format!("{label} ({})", members.len()))
                            .style(Style::default().fg(color)),
                    );
                    if members.is_empty() {
                        lines.push(Line::from("(none)").style(Style::default().fg(theme.muted())));
                    } else {
                        for name in members {
                            lines.push(Line::from(*name).style(Style::default().fg(theme.fg())));
                        }
                    }
                    f.render_widget(
                        Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                        cols[i],
                    );
                }
            }
            // Sidebar nav popup (Ctrl+S / `s` in Pane mode): Activity / Tasks /
            // Settings. All three are wired in Phase 1/2 — no placeholders remain.
            if sidebar_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let n = SIDEBAR_NAV_ITEMS.len();
                let pop_h = (n as u16 + 3).min(total.height.saturating_sub(4)).max(5);
                let pop_w = total.width.min(36).max(24);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(Line::from(" Navigate ").style(Style::default().fg(theme.accent())));
                f.render_widget(&block, pop);
                let inner = block.inner(pop);
                let mut lines: Vec<Line> = Vec::new();
                for (i, name) in SIDEBAR_NAV_ITEMS.iter().enumerate() {
                    let selected = i == sidebar_selected;
                    let prefix = if selected { "▶ " } else { "  " };
                    // Activity (0), Tasks (1), and Settings (2) are all
                    // implemented in Phase 1/2; no "(soon)" placeholders remain.
                    let suffix = if i > 2 { " (soon)" } else { "" };
                    let label = format!("{prefix}{name}{suffix}");
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else if i > 2 {
                        // Hypothetical future placeholder — dimmed.
                        Style::default().fg(theme.muted())
                    } else {
                        // Activity / Tasks (implemented) — normal weight.
                        Style::default().fg(theme.fg())
                    };
                    lines.push(Line::from(label).style(style));
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Help overlay: full-screen keybindings reference.
            if show_help {
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let pop = Rect::new(
                    total.x + 2,
                    total.y + 1,
                    total.width.saturating_sub(4).max(40),
                    total.height.saturating_sub(2).max(10),
                );
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(
                        Line::from(" Keybindings (any key to close) ")
                            .style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);

                let help_lines = vec![
                    Line::from(vec![Span::styled(
                        "Global",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  Ctrl+Alt+P  Enter Pane mode"),
                    Line::from("  Ctrl+Q    Quit"),
                    Line::from("  scroll    Scroll focused pane"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Pane mode (Ctrl+Alt+P)",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  h j k l   Move focus (or arrows)"),
                    Line::from("  Tab       Next pane (wraps)"),
                    Line::from("  p         Pin / unpin agent"),
                    Line::from("  x         Close focused pane"),
                    Line::from("  z         Zoom / unzoom focused pane"),
                    Line::from("  n         Spawn picker (new agent)"),
                    Line::from("  b         Toggle sidebar"),
                    Line::from("  s         Sidebar nav hub"),
                    Line::from("  /         Jump palette (fuzzy-focus)"),
                    Line::from("  a         Activity timeline (overlay)"),
                    Line::from("  d         Agent dashboard (overlay)"),
                    Line::from("  ?         This help"),
                    Line::from("  Esc       Back to Normal"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Normal mode",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  (all keys forwarded to the agent)"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Zoom mode (z in Pane)",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  Ctrl+Alt+P → z   Unzoom"),
                    Line::from("  Esc          Normal (interact with agent while zoomed)"),
                ];
                f.render_widget(
                    Paragraph::new(help_lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Custom-command text-entry modal (drawn on top of the spawn
            // picker, mirroring the jump palette's centered box style).
            if custom_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let pop_w = total.width.min(60).max(40);
                let pop_h = 5u16;
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.accent()))
                    .style(Style::default().bg(theme.panel()))
                    .title(
                        Line::from(" Custom command ").style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);
                let line = Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent()).bg(theme.panel())),
                    Span::styled(
                        custom_cmd_view.clone(),
                        Style::default().fg(theme.fg()).bg(theme.panel()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .bg(theme.panel())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]);
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Tasks view — repo-input modal (Phase 2). Mirrors the custom-command
            // modal shape: a centered single-line text entry with a block cursor.
            if tasks_repo_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let pop_w = total.width.min(60).max(40);
                let pop_h = 5u16;
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.accent()))
                    .style(Style::default().bg(theme.panel()))
                    .title(
                        Line::from(" Tasks \u{2014} owner/name ")
                            .style(Style::default().fg(theme.accent())),
                    );
                f.render_widget(&block, pop);
                let inner = block.inner(pop);
                let line = Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent()).bg(theme.panel())),
                    Span::styled(
                        tasks_repo_input_view.clone(),
                        Style::default().fg(theme.fg()).bg(theme.panel()),
                    ),
                    Span::styled(
                        "_",
                        Style::default()
                            .fg(theme.accent())
                            .bg(theme.panel())
                            .add_modifier(Modifier::SLOW_BLINK),
                    ),
                ]);
                f.render_widget(
                    Paragraph::new(line).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Tasks view — issues/PRs list browser (Phase 2). Mirrors the spawn
            // picker shape: a scrollable list with ↑↓ selection + a scroll
            // indicator, or an error message when the fetch failed.
            if tasks_list_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let n = tasks_items_view.len();
                // ~3 items per 12 rows of terminal height, clamped to [2, 8].
                let max_visible = ((total.height / 12) as usize).clamp(2, 8);
                let body_rows = if tasks_error_view.is_some() { 3 } else { n };
                let visible = body_rows.min(max_visible);
                let pop_h = (visible as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(72).max(40);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let title = match &tasks_error_view {
                    Some(_) => " Tasks \u{2014} error ",
                    None => " Tasks \u{2014} open issues + PRs ",
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(Line::from(title).style(Style::default().fg(theme.accent())));
                f.render_widget(&block, pop);
                let inner = block.inner(pop);

                let mut lines: Vec<Line> = Vec::new();
                if let Some(err) = &tasks_error_view {
                    // Fetch failure: show the error + an Esc hint, no list.
                    lines.push(Line::from(err.as_str()).style(Style::default().fg(theme.error())));
                    lines.push(Line::default());
                    lines.push(
                        Line::from("press Esc to close").style(Style::default().fg(theme.muted())),
                    );
                } else {
                    // Scroll offset: keep the selected item visible (spawn-picker
                    // pattern).
                    let scroll = tasks_selected_view.saturating_sub(max_visible.saturating_sub(1));
                    for (i, entry) in tasks_items_view
                        .iter()
                        .enumerate()
                        .skip(scroll)
                        .take(usize::from(inner.height))
                    {
                        let selected = i == tasks_selected_view;
                        let style = if selected {
                            Style::default()
                                .fg(theme.accent())
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.fg())
                        };
                        let prefix = if selected { "▶ " } else { "  " };
                        let tag = match entry.kind {
                            TaskKind::Issue => "issue",
                            TaskKind::PullRequest => "pr",
                        };
                        // `▶ #NNN title  [issue|pr]` — tag right-aligned-ish by
                        // a fixed 2-space gap (titles vary in width).
                        lines.push(
                            Line::from(format!(
                                "{prefix}#{:<4} {}  [{tag}]",
                                entry.number, entry.title
                            ))
                            .style(style),
                        );
                    }
                    // Scroll indicator (only when there are more items than
                    // visible and no error).
                    if n > max_visible {
                        let more_below = tasks_selected_view + 1 < n;
                        let more_above = scroll > 0;
                        let indicator = match (more_above, more_below) {
                            (true, true) => " \u{2191}\u{2193} more ",
                            (true, false) => " \u{2191} end ",
                            (false, true) => " \u{2193} more ",
                            (false, false) => "",
                        };
                        if !indicator.is_empty() {
                            lines.push(
                                Line::from(indicator).style(Style::default().fg(theme.muted())),
                            );
                        }
                    }
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Settings overlay (Phase 2): four toggle/cycle rows. Mirrors the
            // spawn-picker shape — a centered rounded box with a ▶ cursor.
            // Changes are applied live to `self.config` by `toggle_setting`;
            // this render just reflects the current values.
            if settings_open {
                use ratatui::style::Modifier;
                use ratatui::widgets::{Block, BorderType, Borders, Clear};
                let rows = [
                    ("Sidebar", if settings_sidebar_on { "on" } else { "off" }),
                    ("Status bar", if settings_status_bar { "on" } else { "off" }),
                    ("Default agent", settings_default_agent.as_str()),
                    ("Theme", settings_theme_name.as_str()),
                ];
                let pop_h = (rows.len() as u16 + 3)
                    .min(total.height.saturating_sub(4))
                    .max(5);
                let pop_w = total.width.min(44).max(32);
                let pop_x = total.x + (total.width.saturating_sub(pop_w)) / 2;
                let pop_y = total.y + (total.height.saturating_sub(pop_h)) / 2;
                let pop = Rect::new(pop_x, pop_y, pop_w, pop_h);
                f.render_widget(Clear, pop);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.panel()))
                    .border_style(Style::default().fg(theme.accent()))
                    .title(Line::from(" Settings ").style(Style::default().fg(theme.accent())));
                f.render_widget(&block, pop);
                let inner = block.inner(pop);

                let mut lines: Vec<Line> = Vec::new();
                for (i, (label, value)) in rows.iter().enumerate() {
                    let selected = i == settings_cursor;
                    let prefix = if selected { "\u{25B6} " } else { "  " };
                    let style = if selected {
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.fg())
                    };
                    lines.push(Line::from(format!("{prefix}{label}: {value}")).style(style));
                }
                f.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.panel())),
                    inner,
                );
            }
            // Toast overlay: render transient messages at the bottom of the
            // content area, above the footer. Each toast is one line.
            if !self.toasts.is_empty() && reserve_footer {
                let toast_rows = self.toasts.len().min(3) as u16;
                let toast_area = Rect::new(
                    content_area.x,
                    footer_area.y.saturating_sub(toast_rows),
                    content_area.width,
                    toast_rows,
                );
                self.toasts.render_buf(f.buffer_mut(), toast_area, theme);
            }
        })?;

        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(cols, rows) => {
                self.cols = cols.max(MIN_COLS);
                self.rows = rows.max(MIN_ROWS);
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Help overlay: any key dismisses it.
        if self.show_help {
            self.show_help = false;
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+Alt+P gateway (Alt adds an ESC-prefix byte so it's distinct
        // from Ctrl+P, which opencode uses for its command palette).
        if ctrl && key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('p') {
            self.mode = InputMode::Pane;
            return;
        }
        // Ctrl+Q → quit (only other global hotkey; everything else lives
        // behind the gateway in Pane mode to avoid colliding with agent
        // shortcuts like opencode's Ctrl+P command palette).
        if ctrl && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        match self.mode {
            InputMode::Normal => self.forward_key_to_agent(key),
            InputMode::Pane => match key.code {
                KeyCode::Esc => self.mode = InputMode::Normal,
                KeyCode::Tab => self.focus_next(),
                KeyCode::BackTab => self.focus_prev(),
                KeyCode::Up | KeyCode::Char('k') => self.focus_directional(FocusDir::Up),
                KeyCode::Down | KeyCode::Char('j') => self.focus_directional(FocusDir::Down),
                KeyCode::Left | KeyCode::Char('h') => self.focus_directional(FocusDir::Left),
                KeyCode::Right | KeyCode::Char('l') => self.focus_directional(FocusDir::Right),
                // Action item #6: toggle the pin flag on the focused pane so it
                // renders in the dedicated "PINNED" sidebar section. A bare `p`
                // (no modifiers) — only Ctrl+Alt+P is intercepted above as the
                // mode-enter gateway, so plain Ctrl+P is forwarded to the agent
                // and a bare `p` reaches this arm.
                KeyCode::Char('p') => self.toggle_pin_focused(),
                // `x` kills the focused pane (closes the agent + removes it
                // from the grid). Focus moves to the previous pane.
                KeyCode::Char('x') => self.close_focused_pane(),
                // `z` toggles zoom: the focused pane fills the content area.
                KeyCode::Char('z') => self.zoomed = !self.zoomed,
                // `?` toggles the help overlay.
                KeyCode::Char('?') => self.show_help = !self.show_help,
                // `/` opens the fuzzy-focus jump palette.
                KeyCode::Char('/') => {
                    self.jump_query.clear();
                    self.jump_selected = 0;
                    self.mode = InputMode::Jump;
                }
                // `a` opens the full-screen activity timeline overlay.
                KeyCode::Char('a') => self.mode = InputMode::Activity,
                // `d` opens the read-only 3-bucket agent dashboard overlay
                // (Phase 2): groups live statuses into needs-attention /
                // working / done columns. Any key dismisses it.
                KeyCode::Char('d') => self.mode = InputMode::Dashboard,
                // `n` opens the spawn picker (moved from global Ctrl+N to
                // avoid colliding with agent shortcuts).
                KeyCode::Char('n') => {
                    self.spawn_selected = 0;
                    self.mode = InputMode::Spawn;
                }
                // `b` toggles the sidebar visibility (moved from Ctrl+B).
                KeyCode::Char('b') => self.sidebar_hidden = !self.sidebar_hidden,
                // `s` opens the sidebar navigation hub (moved from Ctrl+S,
                // which terminals may swallow as XOFF flow control).
                KeyCode::Char('s') => {
                    self.mode = InputMode::Sidebar;
                    self.sidebar_nav = 0;
                }
                _ => {}
            },
            InputMode::Jump => self.handle_jump_key(key),
            InputMode::Spawn => self.handle_spawn_key(key),
            // Activity overlay: any key dismisses it (back to Normal), mirroring
            // the Help overlay's "any key to close" contract.
            InputMode::Activity => self.mode = InputMode::Normal,
            // Dashboard overlay (Phase 2): read-only status board. Any key
            // dismisses it back to Normal, mirroring the Activity/Help contract.
            InputMode::Dashboard => self.mode = InputMode::Normal,
            InputMode::Sidebar => match key.code {
                KeyCode::Esc => self.mode = InputMode::Normal,
                KeyCode::Up => {
                    if self.sidebar_nav > 0 {
                        self.sidebar_nav -= 1;
                    }
                }
                KeyCode::Down => {
                    if self.sidebar_nav + 1 < SIDEBAR_NAV_ITEMS.len() {
                        self.sidebar_nav += 1;
                    }
                }
                KeyCode::Enter => match self.sidebar_nav {
                    0 => self.mode = InputMode::Activity,
                    1 => {
                        // Phase 2: Tasks view — reset state and open the
                        // repo-input modal. The fetch runs on Enter (see
                        // `handle_tasks_repo_key`).
                        self.tasks_repo_input.clear();
                        self.tasks_items.clear();
                        self.tasks_repo = None;
                        self.tasks_selected = 0;
                        self.tasks_error = None;
                        self.mode = InputMode::TasksRepo;
                    }
                    2 => {
                        // Phase 2: Settings overlay — reset the cursor and open
                        // the live toggle/cycle panel. Persistence runs on Esc
                        // (see `handle_settings_key`).
                        self.settings_cursor = 0;
                        self.mode = InputMode::Settings;
                    }
                    _ => {
                        // Any future nav item beyond Settings stays a stub.
                        let name = SIDEBAR_NAV_ITEMS[self.sidebar_nav];
                        self.toasts.push(crate::toast::Toast::info(format!(
                            "{name} \u{2014} coming soon"
                        )));
                        self.mode = InputMode::Normal;
                    }
                },
                _ => {}
            },
            InputMode::SpawnCustom => match key.code {
                KeyCode::Esc => self.mode = InputMode::Normal,
                KeyCode::Enter => {
                    // Shell-split on whitespace (no quote handling — v1).
                    let parts: Vec<String> = self
                        .custom_cmd
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    if parts.is_empty() {
                        self.mode = InputMode::Normal;
                        return;
                    }
                    if !self.can_spawn_pane() {
                        self.toasts.push(crate::toast::Toast::warning(
                            "Terminal too small for another pane".to_string(),
                        ));
                        self.mode = InputMode::Normal;
                        return;
                    }
                    let spec = AgentSpec::from_command(parts);
                    let idx = self.spawn_one(spec);
                    self.focus = idx;
                    self.mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    self.custom_cmd.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.custom_cmd.push(c);
                }
                _ => {}
            },
            InputMode::TasksRepo => self.handle_tasks_repo_key(key),
            InputMode::TasksList => self.handle_tasks_list_key(key),
            InputMode::Settings => self.handle_settings_key(key),
        }
    }

    /// Jump-palette key handling: build the filter query, move the selection
    /// within the filtered list, focus on Enter, cancel on Esc.
    fn handle_jump_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Enter => {
                // Focus the selected filtered agent (if any) and close.
                if let Some(&idx) = self.jump_filtered().get(self.jump_selected) {
                    self.focus = idx;
                }
                self.mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                self.jump_query.pop();
                self.jump_selected = 0;
            }
            KeyCode::Up => {
                if self.jump_selected > 0 {
                    self.jump_selected -= 1;
                }
            }
            KeyCode::Down => {
                let n = self.jump_filtered().len();
                if self.jump_selected + 1 < n {
                    self.jump_selected += 1;
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.jump_query.push(c);
                self.jump_selected = 0;
            }
            _ => {}
        }
    }

    /// The pane indices whose name matches the jump query (case-insensitive
    /// substring). Empty query ⇒ all panes, in order.
    fn jump_filtered(&self) -> Vec<usize> {
        let q = self.jump_query.to_ascii_lowercase();
        self.panes
            .iter()
            .enumerate()
            .filter(|(_, p)| q.is_empty() || p.name().to_ascii_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Phase 2 — Tasks view: repo-input modal key handling. Type `owner/name`,
    /// Enter parses + fetches open issues + PRs (synchronously for v1; the gh
    /// CLI is sub-second so this is acceptable — async background fetch is a
    /// documented follow-up), Backspace deletes, Esc cancels.
    fn handle_tasks_repo_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Backspace => {
                self.tasks_repo_input.pop();
            }
            KeyCode::Enter => {
                // Parse the repo reference. On error, surface the message and
                // stay in the repo-input modal so the user can fix the typo.
                match RepoRef::parse(&self.tasks_repo_input) {
                    Ok(repo) => {
                        self.tasks_repo = Some(repo.clone());
                        // v1: synchronous fetch. gh is sub-second; a background
                        // async fetch (tokio task + event-channel redraw) is a
                        // follow-up noted in docs/ROADMAP.md §2.
                        let res = self.fetch_tasks(&repo);
                        match res {
                            Ok(items) => {
                                self.tasks_items = items;
                                self.tasks_selected = 0;
                                self.tasks_error = None;
                                self.mode = InputMode::TasksList;
                            }
                            Err(e) => {
                                // Fetch failed: switch to the list overlay
                                // anyway, which renders the error + Esc hint.
                                self.tasks_items = Vec::new();
                                self.tasks_selected = 0;
                                self.tasks_error = Some(format!("{e:#}"));
                                self.mode = InputMode::TasksList;
                            }
                        }
                    }
                    Err(e) => {
                        self.tasks_error = Some(format!("{e:#}"));
                        self.toasts
                            .push(crate::toast::Toast::warning(format!("{e:#}")));
                    }
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.tasks_repo_input.push(c);
            }
            _ => {}
        }
    }

    /// Phase 2 — Tasks view: fetch open issues + PRs for `repo` and merge them
    /// into a single [`Vec<TasksEntry>`] sorted by number ascending. Issues
    /// store a title-only prompt initially (body is fetched lazily on
    /// dispatch); PRs store the final `pr_to_prompt` form.
    fn fetch_tasks(&self, repo: &RepoRef) -> anyhow::Result<Vec<TasksEntry>> {
        use crate::integrations::{issue_to_prompt, list_issues, list_pull_requests, pr_to_prompt};
        let mut items: Vec<TasksEntry> = Vec::new();
        // Issues — body is None from the list endpoint; the issue_to_prompt
        // form here is title-only. The real body is fetched on Enter via
        // `integrations::fetch_issue` so the dispatch prompt includes it.
        let issues = list_issues(repo).context("fetching open issues via `gh issue list`")?;
        for iss in issues {
            // Build a body-less copy for the initial prompt; the lazy fetch
            // replaces this with the full-body prompt on dispatch.
            let title_only = crate::integrations::Issue {
                number: iss.number,
                title: iss.title.clone(),
                body: None,
            };
            items.push(TasksEntry {
                kind: TaskKind::Issue,
                number: iss.number,
                title: iss.title,
                prompt: issue_to_prompt(&title_only),
            });
        }
        // Pull requests — pr_to_prompt is the final form (no lazy body fetch).
        let prs = list_pull_requests(repo).context("fetching open PRs via `gh pr list`")?;
        for pr in prs {
            // Borrow `pr` for the prompt before partially moving `pr.title`.
            let prompt = pr_to_prompt(&pr);
            items.push(TasksEntry {
                kind: TaskKind::PullRequest,
                number: pr.number,
                title: pr.title,
                prompt,
            });
        }
        // Sort ascending by number so the list is stable + scannable.
        items.sort_by_key(|e| e.number);
        Ok(items)
    }

    /// Phase 2 — Tasks view: issues/PRs list browser key handling. ↑↓ moves
    /// the selection (clamped), Enter dispatches a new agent pane with the
    /// issue/PR body as the prompt, Esc returns to Normal.
    fn handle_tasks_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.tasks_error = None;
                self.mode = InputMode::Normal;
            }
            KeyCode::Up => {
                if self.tasks_selected > 0 {
                    self.tasks_selected -= 1;
                }
            }
            KeyCode::Down => {
                let n = self.tasks_items.len();
                if n > 0 && self.tasks_selected + 1 < n {
                    self.tasks_selected += 1;
                }
            }
            KeyCode::Enter => {
                // Nothing to dispatch if the list is empty (e.g. a fetch error
                // left tasks_items empty and the overlay is showing the error).
                let Some(entry) = self.tasks_items.get(self.tasks_selected).cloned() else {
                    return;
                };
                if !self.can_spawn_pane() {
                    self.toasts.push(crate::toast::Toast::warning(
                        "Terminal too small for another pane".to_string(),
                    ));
                    return;
                }
                // For issues, lazily enrich the prompt with the real body so
                // the dispatched agent gets the full issue text. PRs already
                // carry their final prompt. On body-fetch failure, fall back
                // to the stored title-only prompt + warn the operator.
                let final_prompt = match entry.kind {
                    TaskKind::PullRequest => entry.prompt.clone(),
                    TaskKind::Issue => {
                        if let Some(repo) = self.tasks_repo.clone() {
                            match crate::integrations::fetch_issue(&repo, entry.number) {
                                Ok(full) => crate::integrations::issue_to_prompt(&full),
                                Err(e) => {
                                    self.toasts.push(crate::toast::Toast::warning(format!(
                                        "could not fetch issue body: {e:#} \u{2014} using title only"
                                    )));
                                    entry.prompt.clone()
                                }
                            }
                        } else {
                            // No stored repo (shouldn't happen — set on
                            // TasksRepo submit). Fall back to the title-only
                            // prompt rather than blocking dispatch.
                            entry.prompt.clone()
                        }
                    }
                };
                let agent = self.config.default_agent.clone();
                let mut spec = AgentSpec::from_command(vec![agent, final_prompt]);
                spec.name = format!(
                    "{}-#{}",
                    match entry.kind {
                        TaskKind::Issue => "issue",
                        TaskKind::PullRequest => "pr",
                    },
                    entry.number
                );
                let idx = self.spawn_one(spec);
                self.focus = idx;
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    /// The fixed list of selectable default-agent binaries, in cycle order.
    /// Mirrors the spawn-picker agent registry — `bash` first (matches the
    /// Config default), then the AI coding agents.
    const SETTINGS_AGENTS: &'static [&'static str] =
        &["bash", "opencode", "claude", "codex", "gemini", "aider"];

    /// Phase 2 — Settings overlay key handling. ↑↓ moves the cursor (clamped
    /// to 0..=3), Enter OR Space toggles/cycles the focused row LIVE (the very
    /// next render reflects it), Esc persists the whole config via
    /// [`Config::save`] and returns to Normal.
    ///
    /// Persistence is best-effort: on IO failure (e.g. no writable config dir
    /// in a sandboxed test environment) a warning toast is queued but the mode
    /// still returns to Normal so the user isn't trapped in the overlay.
    fn handle_settings_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Persist the whole (possibly mutated) config. A failure here
                // is non-fatal — warn + close so the user isn't stuck.
                match self.config.save() {
                    Ok(()) => self
                        .toasts
                        .push(crate::toast::Toast::success("Settings saved".to_string())),
                    Err(e) => self.toasts.push(crate::toast::Toast::warning(format!(
                        "config save failed: {e:#}"
                    ))),
                }
                self.mode = InputMode::Normal;
            }
            KeyCode::Up => {
                if self.settings_cursor > 0 {
                    self.settings_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.settings_cursor < 3 {
                    self.settings_cursor += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_setting(self.settings_cursor);
            }
            _ => {}
        }
    }

    /// Apply a single settings-row toggle LIVE to `self.config`. Row 0 toggles
    /// the sidebar (sidebar_width 0 hides it via the existing render logic; the
    /// default width 26 is restored on the next toggle). Row 1 flips the status
    /// bar. Row 2 cycles the default agent through [`SETTINGS_AGENTS`]. Row 3
    /// cycles the theme through [`Config::theme_presets`] (matched by accent).
    fn toggle_setting(&mut self, row: usize) {
        match row {
            0 => {
                // Sidebar toggle: 0 hides it; non-zero restores the default
                // width. `sidebar_hidden` (the Ctrl+B user override) is left
                // alone — it's a separate, render-time concern.
                if self.config.layout.sidebar_width > 0 {
                    self.config.layout.sidebar_width = 0;
                } else {
                    self.config.layout.sidebar_width = LayoutConfig::default().sidebar_width;
                }
            }
            1 => {
                self.config.layout.show_status_bar = !self.config.layout.show_status_bar;
            }
            2 => {
                // Cycle the default agent through the fixed list, wrapping.
                let cur = self.config.default_agent.clone();
                let next = Self::SETTINGS_AGENTS
                    .iter()
                    .position(|a| a.eq_ignore_ascii_case(&cur))
                    .map(|i| Self::SETTINGS_AGENTS[(i + 1) % Self::SETTINGS_AGENTS.len()])
                    .unwrap_or(Self::SETTINGS_AGENTS[0]);
                self.config.default_agent = (*next).to_string();
            }
            3 => {
                // Cycle the theme through the presets. Match the current theme
                // by accent-hex equality (presets use distinct accents); if the
                // user has a custom theme that matches no preset, start the
                // cycle from index 0 (GitHub Dark).
                let presets = Config::theme_presets();
                let idx = presets
                    .iter()
                    .position(|(_, t)| t.accent.eq_ignore_ascii_case(&self.config.theme.accent))
                    .map(|i| (i + 1) % presets.len())
                    .unwrap_or(0);
                self.config.theme = presets[idx].1.clone();
            }
            _ => {}
        }
    }

    /// Normal-mode passthrough: forward the key to the focused agent's PTY as
    /// raw bytes / VT escape sequences. The agent receives everything — Tab,
    /// Esc, Ctrl+C, arrows — exactly as if it were a real terminal.
    fn forward_key_to_agent(&mut self, key: KeyEvent) {
        // IME note: committed Hangul/CJK/emoji/accented input is forwarded
        // verbatim via the UTF-8 path below and works correctly. Incomplete IME
        // compositions (e.g. a lone Hangul jamo during preedit) are NOT filtered
        // here — crossterm 0.28's `KeyEventState` has no `COMPOSING` flag under
        // the default terminal encoding, so there's no reliable signal to gate
        // on. Fully fixing preedit cursor desync needs an enhanced keyboard
        // protocol / OSC 51-style preedit; see the `hangul` module for a
        // composition building block. Non-composing events are unaffected.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if ctrl => {
                let byte = (c as u8).to_ascii_lowercase().wrapping_sub(b'a' - 1);
                self.send_to_focused(&[byte]);
            }
            // Send the full UTF-8 encoding — `c as u8` truncates non-ASCII
            // (Korean/CJK/emoji/accents are 2-4 bytes), which corrupts input.
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                self.send_to_focused(s.as_bytes());
            }
            KeyCode::Enter if !ctrl && !alt => self.send_to_focused(b"\r"),
            KeyCode::Backspace if !ctrl && !alt => self.send_to_focused(&[0x7f]),
            KeyCode::Tab if !ctrl && !alt => self.send_to_focused(b"\t"),
            KeyCode::Esc => self.send_to_focused(b"\x1b"),
            KeyCode::Up => self.send_to_focused(b"\x1b[A"),
            KeyCode::Down => self.send_to_focused(b"\x1b[B"),
            KeyCode::Right => self.send_to_focused(b"\x1b[C"),
            KeyCode::Left => self.send_to_focused(b"\x1b[D"),
            KeyCode::Home => self.send_to_focused(b"\x1b[H"),
            KeyCode::End => self.send_to_focused(b"\x1b[F"),
            KeyCode::PageUp => self.send_to_focused(b"\x1b[5~"),
            KeyCode::PageDown => self.send_to_focused(b"\x1b[6~"),
            KeyCode::Delete => self.send_to_focused(b"\x1b[3~"),
            KeyCode::BackTab => self.send_to_focused(b"\x1b[Z"),
            _ => {}
        }
    }

    /// Mouse scroll: move the focused pane's scrollback (3 lines per notch).
    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if let Some(p) = self.focused_pane_mut() {
                    p.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(p) = self.focused_pane_mut() {
                    p.scroll_down(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(idx) = self.pane_at(col, row) {
                    // Clear any selection on other panes, then start a new one here.
                    for (i, p) in self.panes.iter_mut().enumerate() {
                        if i != idx {
                            p.clear_selection();
                        }
                    }
                    self.focus = idx;
                    if let Some((c, r)) = self.map_to_inner(idx, col, row) {
                        if let Some(p) = self.panes.get_mut(idx) {
                            p.start_selection(c, r);
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(idx) = self.pane_at(col, row) {
                    if self.panes.get(idx).map_or(false, |p| p.has_selection()) {
                        if let Some((c, r)) = self.map_to_inner(idx, col, row) {
                            if let Some(p) = self.panes.get_mut(idx) {
                                p.extend_selection(c, r);
                            }
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Copy the focused pane's selection (if any), then clear it.
                let text = self.panes.get(self.focus).and_then(|p| p.selected_text());
                if let Some(t) = text {
                    match crate::clipboard::copy(&t) {
                        Ok(()) => self.toasts.push(crate::toast::Toast::info("Copied")),
                        Err(_) => self.toasts.push(crate::toast::Toast::warning(
                            "No clipboard tool (xclip/xsel/wl-copy/pbcopy)",
                        )),
                    }
                }
                if let Some(p) = self.panes.get_mut(self.focus) {
                    p.clear_selection();
                }
            }
            _ => {}
        }
    }

    /// Index of the pane whose last-rendered OUTER rect contains `(col,row)`,
    /// or `None` when the point is outside every pane (or no rects are cached).
    fn pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.pane_rects
            .iter()
            .position(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
    }

    /// Map a terminal `(col,row)` to pane `idx`'s INNER grid `(col,row)`.
    /// The pane content sits 1 cell inside the rounded border on every side,
    /// so inner = outer shrunk by 1 on each edge. Returns `None` if the point
    /// is outside the pane or the inner area is too small to hold content.
    fn map_to_inner(&self, idx: usize, col: u16, row: u16) -> Option<(u16, u16)> {
        let r = self.pane_rects.get(idx)?;
        if col < r.x || row < r.y {
            return None;
        }
        let c = col.saturating_sub(r.x).saturating_sub(1);
        let rr = row.saturating_sub(r.y).saturating_sub(1);
        // Clamp to the inner dimensions (w-2, h-2); the -2 accounts for both borders.
        let max_c = r.width.saturating_sub(2);
        let max_r = r.height.saturating_sub(2);
        if max_c == 0 || max_r == 0 {
            return None;
        }
        Some((c.min(max_c - 1), rr.min(max_r - 1)))
    }

    /// Grid-aware directional focus (Pane mode: arrows / hjkl).
    fn focus_directional(&mut self, dir: FocusDir) {
        let n = self.panes.len();
        if n == 0 {
            return;
        }
        let cols = ((n as f64).sqrt().ceil() as usize).max(1);
        self.focus = match dir {
            FocusDir::Right => {
                let row_end = (self.focus / cols + 1) * cols;
                if self.focus + 1 < row_end.min(n) {
                    self.focus + 1
                } else {
                    self.focus
                }
            }
            FocusDir::Left => {
                if self.focus % cols > 0 {
                    self.focus - 1
                } else {
                    self.focus
                }
            }
            FocusDir::Down => {
                if self.focus + cols < n {
                    self.focus + cols
                } else {
                    self.focus
                }
            }
            FocusDir::Up => {
                if self.focus >= cols {
                    self.focus - cols
                } else {
                    self.focus
                }
            }
        };
    }

    fn focus_next(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        if self.focus >= self.panes.len() {
            self.focus = 0;
        } else {
            self.focus = (self.focus + 1) % self.panes.len();
        }
    }

    fn focus_prev(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.focus = if self.focus == 0 {
            self.panes.len() - 1
        } else {
            self.focus - 1
        };
    }

    /// Build the list of spawnable agents for the Ctrl+N picker.
    /// Always includes `bash`; then all detected installed agents (deduped);
    /// then the configured default if not already present.
    fn spawn_options(&self) -> Vec<(String, Vec<String>)> {
        let mut opts: Vec<(String, Vec<String>)> = Vec::new();
        // Always-available: bash.
        opts.push(("bash".to_string(), vec!["bash".to_string()]));
        // Detected agents.
        for kind in AgentKind::detect_installed() {
            let bin = kind.binary().to_string();
            let name = kind.display_name().to_string();
            if !opts.iter().any(|(n, _)| n == &name) {
                opts.push((name, vec![bin]));
            }
        }
        // Configured default if not already listed.
        let default = &self.config.default_agent;
        if !opts
            .iter()
            .any(|(_, cmd)| cmd.first().is_some_and(|c| c == default))
            && !default.is_empty()
        {
            opts.push((default.clone(), vec![default.clone()]));
        }
        // Custom-command sentinel: selecting this entry opens the
        // `InputMode::SpawnCustom` text-entry modal so the user can type any
        // command. The empty command vector is the sentinel.
        opts.push(("Custom command\u{2026}".to_string(), Vec::new()));
        opts
    }

    /// Maximum panes that fit in the current terminal without dropping below
    /// [`MIN_PANE_COLS`] × [`MIN_PANE_ROWS`] inner area.
    fn max_panes(&self) -> usize {
        let sidebar = if self.config.layout.sidebar_width > 0 && !self.sidebar_hidden {
            self.config.layout.sidebar_width + 1 // +1 gap
        } else {
            0
        };
        let footer = if self.config.layout.show_status_bar {
            1
        } else {
            0
        };
        let avail_cols = self.cols.saturating_sub(sidebar);
        let avail_rows = self.rows.saturating_sub(footer);
        let h = (avail_cols / (MIN_PANE_COLS + 2)) as usize; // +2 border
        let v = (avail_rows / (MIN_PANE_ROWS + 2)) as usize; // +2 border
        (h * v).max(1)
    }

    /// Check if there is room for one more pane.
    fn can_spawn_pane(&self) -> bool {
        self.panes.len() < self.max_panes()
    }

    /// Spawn-picker key handling: Up/Down to navigate, Enter to spawn, Esc cancel.
    fn handle_spawn_key(&mut self, key: KeyEvent) {
        let opts = self.spawn_options();
        let n = opts.len();
        match key.code {
            KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Up => {
                if self.spawn_selected > 0 {
                    self.spawn_selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.spawn_selected + 1 < n {
                    self.spawn_selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some((_, cmd)) = opts.get(self.spawn_selected) {
                    if cmd.is_empty() {
                        // Custom command sentinel: switch to the text-entry modal.
                        self.custom_cmd.clear();
                        self.mode = InputMode::SpawnCustom;
                        return;
                    }
                    if !self.can_spawn_pane() {
                        self.toasts.push(crate::toast::Toast::warning(
                            "Terminal too small for another pane".to_string(),
                        ));
                        self.mode = InputMode::Normal;
                        return;
                    }
                    let spec = AgentSpec::from_command(cmd.clone());
                    let idx = self.spawn_one(spec);
                    self.focus = idx;
                }
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn toggle_pin_focused(&mut self) {
        if let Some(slot) = self.pinned.get_mut(self.focus) {
            *slot = !*slot;
        }
    }

    /// Close the focused pane: kill the agent, remove it from all parallel
    /// vectors, and move focus to the previous pane.
    fn close_focused_pane(&mut self) {
        if self.focus >= self.panes.len() {
            return;
        }
        let idx = self.focus;
        // Kill the session if it's still alive.
        if let Some(Some(session)) = self.sessions.get_mut(idx) {
            let _ = session.kill();
        }
        // Remove from every parallel vector.
        self.panes.remove(idx);
        self.sessions.remove(idx);
        self.pane_task.remove(idx);
        self.pane_command.remove(idx);
        self.reconnect.remove(idx);
        self.reconnect_due.remove(idx);
        self.pinned.remove(idx);
        self.daemon_session_ids.remove(idx);
        // Adjust focus to the previous pane (or wrap to the last).
        if self.panes.is_empty() {
            self.mode = InputMode::Normal;
            self.focus = 0;
        } else if self.focus >= self.panes.len() {
            self.focus = self.panes.len() - 1;
        }
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        if self.focus >= self.panes.len() {
            None
        } else {
            self.panes.get_mut(self.focus)
        }
    }

    /// Forward raw bytes to the focused pane's PTY. Best-effort: a dead/exited
    /// agent's PTY write will error and we swallow it rather than tear down the
    /// whole TUI (typing into a finished pane is a no-op).
    ///
    /// In daemon mode: sends a `write` RPC to the daemon (the daemon owns the
    /// PTY). If the RPC fails (daemon crash, session gone), a toast is pushed
    /// so the user sees what happened — the typed bytes are lost (no local PTY
    /// to fall back to in daemon mode).
    fn send_to_focused(&mut self, bytes: &[u8]) {
        if let Some(daemon) = &mut self.daemon {
            // Daemon mode: forward via RPC using the pane's daemon session ID.
            let session_id = match self
                .daemon_session_ids
                .get(self.focus)
                .and_then(|s| s.as_ref())
            {
                Some(id) => id.clone(),
                None => return, // no daemon session for this pane yet
            };
            match daemon.rpc(
                "write",
                serde_json::json!({
                    "sessionId": session_id,
                    "data": String::from_utf8_lossy(bytes),
                }),
            ) {
                Ok(_) => {}
                Err(crate::orca_daemon::DaemonError::Disconnected { reason }) => {
                    self.conn_state = ConnectionState::Disconnected {
                        reason: reason.clone(),
                        next_retry: Some(Instant::now() + Duration::from_secs(3)),
                    };
                    self.toasts.push(crate::toast::Toast::error(format!(
                        "Daemon disconnected: {reason}"
                    )));
                    // Drop the daemon client — it's dead.
                    self.daemon = None;
                }
                Err(e) => {
                    self.toasts.push(crate::toast::Toast::warning(format!(
                        "RPC write failed: {e}"
                    )));
                }
            }
            return;
        }
        // Standalone mode: write directly to the local PTY.
        let Some(Some(session)) = self.sessions.get_mut(self.focus) else {
            return;
        };
        let _ = session.write_bytes(bytes);
    }

    /// True once every session slot is `None` (no live agent process remains).
    /// For an empty session set this is vacuously true → the loop exits.
    fn all_sessions_gone(&self) -> bool {
        self.sessions.iter().all(|s| s.is_none())
    }
}

// ── Daemon stream helpers ───────────────────────────────────────────────────

/// Parse a Data-frame payload and route it to the correct pane.
///
/// The daemon sends NDJSON `{"sessionId":"...","data":"..."}`. If the payload
/// doesn't parse as NDJSON (e.g. raw bytes), fall back to pane 0.
fn parse_stream_data(
    payload: &[u8],
    session_map: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
) -> (usize, Vec<u8>) {
    // Try NDJSON first.
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(sid) = json.get("sessionId").and_then(|v| v.as_str()) {
            let pane_id = session_map.lock().unwrap().get(sid).copied().unwrap_or(0);
            if let Some(data) = json.get("data").and_then(|v| v.as_str()) {
                return (pane_id, data.as_bytes().to_vec());
            }
            return (pane_id, Vec::new());
        }
    }
    // Fallback: raw bytes → pane 0.
    (0, payload.to_vec())
}

/// Parse an Event-frame payload for an exit event.
///
/// Returns `Some((pane_id, exit_code))` for exit events, `None` otherwise.
fn parse_stream_event(
    payload: &[u8],
    session_map: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
) -> Option<(usize, i32)> {
    let json: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let event = json.get("event").and_then(|v| v.as_str())?;
    if event != "exit" {
        return None;
    }
    let sid = json.get("sessionId").and_then(|v| v.as_str())?;
    let pane_id = session_map.lock().unwrap().get(sid).copied().unwrap_or(0);
    let code = json
        .get("payload")
        .and_then(|p| p.get("code"))
        .and_then(|c| c.as_i64())
        .map(|c| c as i32)
        .unwrap_or(0);
    Some((pane_id, code))
}

impl<B: Backend> Drop for App<B> {
    fn drop(&mut self) {
        // Panic-safety: if `run` never restored (or panicked mid-loop), make
        // one best-effort attempt to give the user their terminal back. The
        // `PtySession` drops below kill+join any still-running agents.
        if self.raw_mode_active {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
            let _ = self.terminal.show_cursor();
            self.raw_mode_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Decision-logic tests for `App` that do NOT require a real terminal
    //! (no raw mode, no PTYs). They construct an `App` via [`App::for_test`]
    //! and exercise the pure routing/state methods: `apply_update`,
    //! `focus_next`/`focus_prev`, `all_sessions_gone`, `handle_key`, and
    //! `publish_snapshot`. The TUI-bound `run`/`main_loop`/`render` paths still
    //! need a TTY and stay out of reach of unit tests.

    use super::*;
    use crate::agent::AgentKind;
    use ratatui::backend::TestBackend;

    impl App<TestBackend> {
        /// Build an `App` from pre-made panes with no live sessions and no raw
        /// mode — purely for exercising the decision logic under test.
        fn for_test(panes: Vec<Pane>) -> Self {
            let n = panes.len();
            // Use an in-memory TestBackend (NOT CrosstermBackend+stdout) so the
            // tests run identically on a developer TTY and on a headless CI
            // runner whose stdout is a pipe — `CrosstermBackend::new(stdout)`
            // fails at `Terminal::new` when stdout isn't a TTY (it queries the
            // size via an ioctl that errors). The decision logic under test
            // never touches the terminal anyway.
            let terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal backend");
            let (bus_tx, rx) = bus::channel();
            Self {
                panes,
                sessions: (0..n).map(|_| None::<PtySession>).collect(),
                focus: 0,
                terminal,
                bus_rx: rx,
                quit: false,
                raw_mode_active: false,
                worktrees: None,
                scheduler: FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now()),
                snapshot_tx: None,
                coordinator: None,
                orch_agent: None,
                pane_task: (0..n).map(|_| None).collect(),
                daemon_session_ids: (0..n).map(|_| None).collect(),
                daemon_session_map: None,
                daemon_reconnect_attempts: 0,
                daemon_backoff: Duration::from_secs(3),
                cols: 80,
                rows: 24,
                bus_tx,
                pane_command: (0..n).map(|_| Vec::new()).collect(),
                reconnect: (0..n).map(|_| None).collect(),
                reconnect_due: (0..n).map(|_| None).collect(),
                pinned: vec![false; n],
                config: Config::default(),
                mode: InputMode::Normal,
                sidebar_hidden: false,
                jump_query: String::new(),
                jump_selected: 0,
                spawn_selected: 0,
                custom_cmd: String::new(),
                zoomed: false,
                show_help: false,
                conn_state: ConnectionState::Standalone,
                toasts: crate::toast::ToastQueue::new(),
                daemon: None,
                activity: ActivityLog::new(),
                last_status: Vec::new(),
                sidebar_nav: 0,
                pane_rects: Vec::new(),
                tasks_repo_input: String::new(),
                tasks_repo: None,
                tasks_items: Vec::new(),
                tasks_selected: 0,
                tasks_error: None,
                settings_cursor: 0,
            }
        }
    }

    fn pane(id: usize, name: &str) -> Pane {
        let mut p = Pane::new(id, name, 40, 6);
        p.set_state(AgentState::Running);
        p
    }

    #[test]
    fn apply_update_output_feeds_the_matching_pane() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 1,
            bytes: b"hi".to_vec(),
        });
        // Pane 1's emulator should now contain "hi" on its first line.
        let cell = app.panes[1].emulator().cell(0, 0).expect("cell");
        assert_eq!(cell.chars, "h");
        // Pane 0 untouched.
        assert!(!app.panes[0].emulator().cell(0, 0).unwrap().has_contents());
    }

    #[test]
    fn apply_update_exit_zero_is_done_nonzero_is_failed() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(0),
        });
        assert!(matches!(app.panes[0].state(), AgentState::Done(_)));

        app.apply_update(AgentUpdate::Exit {
            pane_id: 1,
            code: Some(2),
        });
        assert!(matches!(app.panes[1].state(), AgentState::Failed(_)));
    }

    #[test]
    fn apply_update_exit_is_idempotent_once_terminal() {
        // A real Some(code) lands first; a later forwarder Exit{None} must NOT
        // downgrade a Failed pane to Done (the reap-before-drain invariant).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(1),
        });
        assert!(matches!(app.panes[0].state(), AgentState::Failed(_)));
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: None,
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Failed(_)),
            "a late None must not overwrite an existing terminal state"
        );
        // The session slot is taken exactly once either way.
        assert!(app.sessions[0].is_none());
    }

    #[test]
    fn focus_next_and_prev_wrap_around() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c")]);
        assert_eq!(app.focus, 0);
        app.focus_next();
        assert_eq!(app.focus, 1);
        app.focus_next();
        assert_eq!(app.focus, 2);
        app.focus_next(); // wraps
        assert_eq!(app.focus, 0);
        app.focus_prev(); // wraps back
        assert_eq!(app.focus, 2);
    }

    #[test]
    fn all_sessions_gone_reflects_live_slots() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        // for_test starts with all slots None → vacuously "gone".
        assert!(app.all_sessions_gone());
        // Put a live session in one slot to exercise the false path.
        app.sessions[1] = dummy_session();
        assert!(!app.all_sessions_gone());
        app.sessions[1] = None;
        assert!(app.all_sessions_gone());
    }

    #[test]
    fn handle_key_mode_switch_ctrl_q_quit_and_pane_focus() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        assert_eq!(app.mode, InputMode::Normal);
        assert!(!app.quit);

        // Ctrl+Q quits from any mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit, "Ctrl+Q quits");

        // Ctrl+Alt+P enters pane mode (gateway — distinct from Ctrl+P which
        // opencode uses for its command palette).
        app.quit = false;
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.mode, InputMode::Pane, "Ctrl+Alt+P enters pane mode");

        // In pane mode: Tab advances focus.
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Tab advances focus in pane mode");

        // Esc exits pane mode back to normal.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Esc exits pane mode");

        // In normal mode: Tab is forwarded to the agent, NOT focus switching.
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "Tab in normal mode does NOT switch focus");
    }

    #[test]
    fn toggle_pin_flips_the_focused_pane_pinned_flag() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.focus = 1;
        app.mode = InputMode::Pane;
        assert!(!app.pinned[0], "pane 0 starts unpinned");
        assert!(!app.pinned[1], "pane 1 starts unpinned");

        // First press of bare `p` pins the focused pane (1).
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.pinned[1], "first press pins the focused pane");
        assert!(!app.pinned[0], "the non-focused pane is untouched");

        // Second press flips it back to unpinned.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(!app.pinned[1], "second press unpins");
    }

    #[test]
    fn ctrl_b_toggles_sidebar_visibility() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(!app.sidebar_hidden, "sidebar visible by default");
        // Sidebar toggle moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `b`. (Was a global Ctrl+B hotkey; moved to avoid
        // colliding with agent shortcuts.)
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(app.sidebar_hidden, "Pane + b hides the sidebar");
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(!app.sidebar_hidden, "b again re-shows it");
    }

    #[test]
    fn sidebar_mode_enters_with_ctrl_s() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert_eq!(app.mode, InputMode::Normal);
        // Sidebar nav moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `s`. (Was a global Ctrl+S hotkey; moved to avoid both
        // collision with agent shortcuts and terminals' XOFF flow control.)
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Sidebar, "Pane + s enters sidebar mode");
        assert_eq!(app.sidebar_nav, 0, "selection starts at the first item");
    }

    #[test]
    fn sidebar_nav_moves_with_arrows() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 0;
        // Down: 0 → 1 → 2 → clamped at 2.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 2);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 2, "clamped at the last item");
        // Up: 2 → 1 → 0 → clamped at 0.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 1);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 0);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.sidebar_nav, 0, "clamped at the first item");
    }

    #[test]
    fn sidebar_enter_activity_opens_overlay() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 0; // Activity
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::Activity,
            "Enter on Activity opens the overlay"
        );
    }

    #[test]
    fn sidebar_enter_tasks_opens_repo_input() {
        // Phase 2: Tasks (index 1) is implemented — Enter opens the repo-input
        // modal (TasksRepo) and resets the Tasks state, with NO toast.
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 1;
        // Dirty the state first to prove Enter resets it.
        app.tasks_repo_input = "stale".to_string();
        app.tasks_selected = 9;
        app.tasks_error = Some("stale error".to_string());
        assert!(app.toasts.is_empty(), "no toast before dispatch");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::TasksRepo,
            "Tasks opens the repo-input modal"
        );
        assert!(
            app.tasks_repo_input.is_empty(),
            "repo input cleared on open"
        );
        assert_eq!(app.tasks_selected, 0, "selection reset on open");
        assert!(app.tasks_error.is_none(), "error cleared on open");
        assert!(app.toasts.is_empty(), "no toast for an implemented feature");
    }

    #[test]
    fn sidebar_enter_settings_opens_overlay() {
        // Phase 2: Settings (index 2) is implemented — Enter opens the settings
        // overlay (cursor reset to row 0), with NO toast.
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 2;
        app.settings_cursor = 9; // dirty to prove Enter resets it
        assert!(app.toasts.is_empty(), "no toast before dispatch");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Settings, "Settings opens the overlay");
        assert_eq!(app.settings_cursor, 0, "cursor reset to the first row");
        assert!(app.toasts.is_empty(), "no toast for an implemented feature");
    }

    #[test]
    fn settings_overlay_toggles_live_and_closes_on_esc() {
        // Phase 2: Settings overlay applies each toggle LIVE to self.config.
        // Toggling the status-bar row (cursor 1) flips show_status_bar; Esc
        // persists (best-effort) and returns to Normal regardless of save
        // outcome (the test env may lack a writable config dir, so only the
        // mode transition — which always happens — is asserted).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Settings;
        app.settings_cursor = 1; // status bar row
        assert!(
            app.config.layout.show_status_bar,
            "status bar on by default"
        );
        // Enter toggles the focused row live.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            !app.config.layout.show_status_bar,
            "status bar toggled off live"
        );
        // Space also toggles (Enter | Space both map to toggle_setting).
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(
            app.config.layout.show_status_bar,
            "status bar toggled back on via Space"
        );
        // Esc persists (best-effort) and returns to Normal.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc closes the overlay and returns to Normal"
        );
    }

    #[test]
    fn sidebar_esc_returns_to_normal() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Sidebar;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Esc exits sidebar mode");
    }

    #[test]
    fn jump_palette_filters_and_focuses_on_enter() {
        let mut app = App::for_test(vec![
            pane(0, "claude"),
            pane(1, "codex"),
            pane(2, "opencode"),
        ]);
        app.mode = InputMode::Pane;
        app.focus = 0;
        // `/` opens the palette; empty query shows all agents.
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Jump);
        assert_eq!(app.jump_filtered(), vec![0, 1, 2], "empty query shows all");

        // type "co" → matches codex + opencode (substring, case-insensitive).
        for c in ['c', 'o'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(
            app.jump_filtered(),
            vec![1, 2],
            "query 'co' filters to codex+opencode"
        );

        // Down → select the 2nd filtered (opencode = pane 2); Enter focuses it.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the palette");
        assert_eq!(app.focus, 2, "Enter focused the selected agent (opencode)");

        // Esc cancels without changing focus.
        app.mode = InputMode::Jump;
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal);
        assert_eq!(app.focus, 0, "Esc cancels without focusing");
    }

    #[test]
    fn publish_snapshot_broadcasts_pane_states() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        app.panes[1].set_state(AgentState::Done(None));
        app.panes[0].set_branch(Some("orca/main".into()));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<AgentSnapshot>>();
        app.set_snapshot_sender(tx);
        app.publish_snapshot();

        let snaps = rx.try_recv().expect("a snapshot was published");
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "claude");
        assert_eq!(snaps[0].state, "Running");
        assert_eq!(snaps[0].branch.as_deref(), Some("orca/main"));
        assert_eq!(snaps[1].name, "codex");
        assert_eq!(snaps[1].state, "Done");
    }

    /// A throwaway `PtySession` is hard to build without spawning; the
    /// `all_sessions_gone` false-path only needs `Some(_)` in a slot, so we
    /// build one off the cheapest possible child (`true`, exits immediately).
    fn dummy_session() -> Option<PtySession> {
        let bin = AgentKind::detect_installed()
            .first()
            .map(AgentKind::binary)
            .unwrap_or("true");
        PtySession::spawn(vec![bin.to_string()], None, 20, 3)
            .map(|(s, _rx)| s)
            .ok()
    }

    /// Flatten the TestBackend's buffer into a string (rows joined by `\n`) so
    /// tests can assert on rendered content without computing exact cell coords.
    fn buffer_text(app: &App<TestBackend>) -> String {
        let buf = app.terminal.backend().buffer();
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_draws_pane_headers_content_and_footer() {
        let mut app = App::for_test(vec![pane(0, "alpha"), pane(1, "beta")]);
        app.panes[0].feed(b"hello-world");
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // Pane names appear in their bordered headers.
        assert!(text.contains("alpha"), "pane 0 header rendered");
        assert!(text.contains("beta"), "pane 1 header rendered");
        // Fed agent output is painted into pane 0's body.
        assert!(text.contains("hello-world"), "fed content rendered");
        // The footer key-hints are drawn on the reserved last line.
        assert!(text.contains("Ctrl+Alt+P"), "footer rendered");
        assert!(text.contains("quit"));
    }

    #[test]
    fn sidebar_nav_popup_renders_items_and_selection() {
        // In Sidebar mode the Navigate popup shows all three nav items and
        // marks the selected one with the ▶ marker.
        let mut app = App::for_test(vec![pane(0, "alpha")]);
        app.mode = InputMode::Sidebar;
        app.sidebar_nav = 1; // Tasks (now implemented in Phase 2)
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // Title + every item label. Activity, Tasks, and Settings are all
        // implemented in Phase 1/2 — none carry a "(soon)" tag.
        assert!(text.contains("Navigate"), "popup title rendered");
        assert!(text.contains("Activity"), "first item rendered");
        assert!(text.contains("Tasks"), "Tasks item rendered");
        assert!(
            !text.contains("Tasks (soon)"),
            "Tasks is implemented — no (soon) tag"
        );
        assert!(text.contains("Settings"), "Settings item rendered");
        assert!(
            !text.contains("Settings (soon)"),
            "Settings is implemented — no (soon) tag"
        );
        // The selected row (index 1 → Tasks) carries the ▶ marker.
        assert!(
            text.contains("▶ Tasks"),
            "selected row carries the ▶ marker"
        );
        // Sanity: the non-selected Activity row does NOT carry the marker.
        assert!(
            !text.contains("▶ Activity"),
            "non-selected Activity row has no marker"
        );
    }

    #[test]
    fn render_resizes_each_pane_viewport_to_its_inner_area() {
        // Panes are created at 40×6; a single pane in an 80×24 terminal (minus
        // the footer + 1-cell border) should be resized to a different size.
        let mut app = App::for_test(vec![pane(0, "solo")]);
        let before = app.panes[0].size();
        app.render().expect("render");
        let after = app.panes[0].size();
        assert_ne!(after, before, "render reconciled the pane viewport");
        // And the content is still present after the resize.
        assert!(buffer_text(&app).contains("solo"));
    }

    #[test]
    fn render_handles_many_panes_without_panic() {
        // A grid of 5 panes must lay out and paint without index panics.
        let panes: Vec<Pane> = (0..5).map(|i| pane(i, &format!("p{i}"))).collect();
        let mut app = App::for_test(panes);
        app.focus = 2;
        app.render().expect("render 5 panes");
        let text = buffer_text(&app);
        for i in 0..5 {
            assert!(text.contains(&format!("p{i}")), "pane {i} rendered");
        }
    }

    #[test]
    fn reap_exited_marks_done_and_takes_the_slot() {
        // `reap_exited` polls each live child via try_wait and, on exit, feeds
        // an Exit{Some(code)} back through apply_update. Spawn a child that
        // exits immediately (code 0) and confirm reap transitions it to Done.
        let mut app = App::for_test(vec![pane(0, "runner")]);
        let (session, _rx) =
            PtySession::spawn(vec!["true".into()], None, 20, 3).expect("spawn true");
        app.sessions[0] = Some(session);

        let mut became_done = false;
        for _ in 0..100 {
            app.reap_exited();
            if matches!(app.panes[0].state(), AgentState::Done(_)) {
                became_done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(became_done, "reap_exited should mark the exited child Done");
        assert!(
            app.sessions[0].is_none(),
            "reap takes the session slot once"
        );
    }

    #[test]
    fn reconnect_schedules_then_respawns_after_backoff() {
        let mut app = App::for_test(vec![pane(0, "remote")]);
        app.enable_reconnect();
        // for_test seeds empty commands; give the pane a real one to respawn.
        app.pane_command[0] = vec!["true".into()];

        // A dropped session schedules a reconnect instead of going terminal.
        app.apply_update(AgentUpdate::Exit {
            pane_id: 0,
            code: Some(1),
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Failed(f) if f.contains("reconnecting")),
            "reconnecting indicator shown"
        );
        assert!(app.sessions[0].is_none(), "dead session taken");
        assert!(app.reconnect_due[0].is_some(), "respawn scheduled");

        // Before the backoff deadline: pump is a no-op.
        app.pump_reconnect();
        assert!(
            app.sessions[0].is_none(),
            "no respawn before backoff elapses"
        );

        // Force the deadline into the past → the next pump respawns the pane.
        app.reconnect_due[0] = Some(Instant::now() - Duration::from_secs(1));
        app.pump_reconnect();
        assert!(app.sessions[0].is_some(), "respawned after backoff");
        assert!(matches!(app.panes[0].state(), AgentState::Running));
        assert!(
            app.reconnect_due[0].is_none(),
            "schedule cleared after respawn"
        );

        // Clean up the spawned child so the test doesn't leak a process.
        if let Some(s) = app.sessions[0].as_mut() {
            let _ = s.kill();
        }
    }

    #[test]
    fn spawn_one_failed_command_adds_failed_pane_and_keeps_vecs_aligned() {
        // Ctrl+N path: spawning an agent whose binary isn't on PATH must NOT
        // panic, must add exactly one Failed pane, and must keep every parallel
        // per-pane vector (sessions/pane_task/pane_command/reconnect/reconnect_due/
        // pinned) the same length as `panes`. A length mismatch here is exactly
        // the class of bug that broke Ctrl+N at runtime.
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        let before = app.panes.len();
        let idx = app.spawn_one(AgentSpec::from_command(vec![
            "definitely-not-a-real-agent-binary-xyz".to_string(),
        ]));
        assert_eq!(idx, before, "new pane index == old panes.len()");
        assert_eq!(app.panes.len(), before + 1, "exactly one pane added");
        assert!(
            matches!(app.panes[idx].state(), AgentState::Failed(_)),
            "unspawnable agent should be a Failed pane (not Running/Idle)"
        );
        assert!(
            app.sessions[idx].is_none(),
            "a failed spawn leaves the session slot empty"
        );
        // Every parallel per-pane vector must stay aligned with `panes`.
        let n = app.panes.len();
        assert_eq!(app.sessions.len(), n, "sessions aligned");
        assert_eq!(app.pane_task.len(), n, "pane_task aligned");
        assert_eq!(app.pane_command.len(), n, "pane_command aligned");
        assert_eq!(app.reconnect.len(), n, "reconnect aligned");
        assert_eq!(app.reconnect_due.len(), n, "reconnect_due aligned");
        assert_eq!(app.pinned.len(), n, "pinned aligned");

        // Focusing the new pane + rendering the whole grid must not panic.
        app.focus = idx;
        app.render().expect("render after a failed spawn_one");
    }

    #[test]
    fn render_shows_new_content_and_retains_old() {
        // Validates the pane render path against ratatui's double-buffering:
        // feeding new content and re-rendering must paint the new content, the
        // old content must persist, and an idle re-render (no new feed) must
        // neither panic nor erase what was drawn. (A dirty/partial repaint would
        // fail this — unrepainted cells carry 2-frame-old content under swap.)
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.panes[0].feed(b"first-frame");
        app.render().expect("first render");
        assert!(
            buffer_text(&app).contains("first-frame"),
            "first content rendered"
        );

        app.panes[0].feed(b"\nsecond-line");
        app.render().expect("second render (dirty rows)");
        let text = buffer_text(&app);
        assert!(text.contains("first-frame"), "first content persists");
        assert!(text.contains("second-line"), "new content rendered");

        // Idle frame: no new feed → no dirty rows → content must be retained.
        app.render().expect("idle re-render");
        let text = buffer_text(&app);
        assert!(
            text.contains("first-frame"),
            "content retained on idle frame"
        );
        assert!(
            text.contains("second-line"),
            "new content retained on idle frame"
        );
    }

    #[test]
    fn handle_mouse_scroll_moves_focused_pane_scrollback() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.focus = 1;
        assert_eq!(app.panes[1].scroll(), 0);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.panes[1].scroll(),
            3,
            "ScrollUp moves the focused pane 3 lines toward older output"
        );
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.panes[1].scroll(),
            0,
            "ScrollDown moves it back to the latest line (clamped at 0)"
        );
        assert_eq!(
            app.panes[0].scroll(),
            0,
            "the non-focused pane is untouched"
        );
    }

    #[test]
    fn handle_mouse_does_not_panic_when_focus_is_out_of_range() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.focus = 99;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        // Non-scroll mouse kinds (click) are ignored entirely.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.panes[0].scroll(), 0, "nothing changed");
    }

    #[test]
    fn mouse_down_starts_selection_in_hit_pane() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Give the pane some content so a selection has something to anchor on.
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        // One pane filling the content area with a 1-cell rounded border, so a
        // click at terminal (5,1) maps to inner grid (4,0) — the "hello" row.
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.panes[0].has_selection(),
            "Down inside a pane starts a selection"
        );
        assert_eq!(app.focus, 0, "Down also moves focus to the hit pane");
    }

    #[test]
    fn mouse_drag_extends_selection() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.panes[0].has_selection(),
            "Drag while a selection is active extends it"
        );
    }

    #[test]
    fn mouse_up_clears_selection() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello world".to_vec(),
        });
        app.pane_rects = vec![Rect::new(0, 0, 40, 12)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        // Do NOT assert clipboard success — CI has no clipboard tool; copy
        // returns Err and pushes a warning toast, but selection is cleared
        // regardless on Up.
        assert!(
            !app.panes[0].has_selection(),
            "Up clears the selection after (attempted) copy"
        );
    }

    #[test]
    fn mouse_down_outside_any_pane_is_noop() {
        use crossterm::event::MouseEvent;
        let mut app = App::for_test(vec![pane(0, "a")]);
        // A small pane in the corner; click far outside it.
        app.pane_rects = vec![Rect::new(0, 0, 10, 5)];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 50,
            row: 50,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            !app.panes[0].has_selection(),
            "a click outside every pane is a no-op (no selection, no panic)"
        );
    }

    #[test]
    fn focus_directional_moves_focus_on_a_grid() {
        // 4 panes ⇒ cols = ceil(sqrt(4)) = 2 ⇒ a 2×2 grid:
        //   0 1
        //   2 3
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b"), pane(2, "c"), pane(3, "d")]);
        app.mode = InputMode::Pane;
        app.focus = 0;

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Right moves within the row");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus, 3, "Down drops a row");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, 2, "Left moves back within the row");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "Up moves up a row");

        // hjkl mirrors the arrow keys.
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "'l' == Right");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.focus, 3, "'j' == Down");
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.focus, 2, "'h' == Left");
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "'k' == Up");
    }

    #[test]
    fn focus_directional_clamps_at_grid_boundaries() {
        // 5 panes ⇒ cols = ceil(sqrt(5)) = 3 ⇒ a 3×2 grid:
        //   0 1 2
        //   3 4
        let mut app = App::for_test(vec![
            pane(0, "a"),
            pane(1, "b"),
            pane(2, "c"),
            pane(3, "d"),
            pane(4, "e"),
        ]);
        app.mode = InputMode::Pane;

        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "no Up from the top row");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, 0, "no Left from column 0");

        app.focus = 4;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.focus, 4, "no Right past the last pane in the row");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.focus, 4, "no Down past the last row");
    }

    #[test]
    fn forward_key_to_agent_is_a_noop_with_no_live_session() {
        // for_test seeds every session slot as None, so every forwarded key is a
        // best-effort write to a dead PTY — swallowed, never panicking. Exercises
        // every arm of the forwarding match (char, enter, backspace, tab, esc,
        // arrows, ctrl+c, and a multi-byte UTF-8 char).
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Normal;
        let keys = [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
            // Ctrl+C maps to the raw byte 0x03 — still swallowed with no session.
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            // A non-ASCII char exercises the multi-byte UTF-8 encoding path.
            KeyEvent::new(KeyCode::Char('\u{D55C}'), KeyModifiers::NONE),
        ];
        for key in keys {
            app.handle_key(key); // must not panic
        }
        assert_eq!(app.focus, 0, "forwarding never moves focus");
        assert_eq!(app.mode, InputMode::Normal, "forwarding never changes mode");
    }

    #[test]
    fn apply_update_output_feeds_content_and_ignores_out_of_range_pane_id() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        app.apply_update(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"hello".to_vec(),
        });
        let cell = app.panes[0].emulator().cell(0, 0).expect("cell exists");
        assert_eq!(cell.chars, "h", "fed content reaches pane 0");
        assert!(
            !app.panes[1].emulator().cell(0, 0).unwrap().has_contents(),
            "pane 1 untouched"
        );
        // An out-of-range pane_id must be a no-op (the .get_mut guard), not a panic.
        app.apply_update(AgentUpdate::Output {
            pane_id: 99,
            bytes: b"nope".to_vec(),
        });
        assert_eq!(app.panes.len(), 2, "no pane added for an OOB pane_id");
    }

    #[test]
    fn parse_stream_data_routes_ndjson_and_falls_back_to_pane_zero() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let mut m = HashMap::new();
        m.insert("sess-1".to_string(), 2usize);
        let map = Arc::new(Mutex::new(m));

        let (pane, bytes) =
            super::parse_stream_data(br#"{"sessionId":"sess-1","data":"hello"}"#, &map);
        assert_eq!(pane, 2, "known session routes to its pane");
        assert_eq!(bytes, b"hello", "data field extracted as bytes");

        let (pane, bytes) = super::parse_stream_data(br#"{"sessionId":"sess-9","data":"x"}"#, &map);
        assert_eq!(pane, 0, "unknown session falls back to pane 0");
        assert_eq!(bytes, b"x");

        // JSON without a sessionId → pane 0 with the raw payload.
        let (pane, bytes) = super::parse_stream_data(br#"{"data":"y"}"#, &map);
        assert_eq!(pane, 0);
        assert_eq!(bytes, br#"{"data":"y"}"#, "raw payload returned verbatim");

        // Non-JSON bytes → pane 0 with the payload verbatim.
        let (pane, bytes) = super::parse_stream_data(b"raw-bytes", &map);
        assert_eq!(pane, 0);
        assert_eq!(bytes, b"raw-bytes");
    }

    #[test]
    fn parse_stream_event_extracts_exit_code_and_ignores_non_exit() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        let mut m = HashMap::new();
        m.insert("sess-1".to_string(), 2usize);
        let map = Arc::new(Mutex::new(m));

        assert_eq!(
            super::parse_stream_event(
                br#"{"event":"exit","sessionId":"sess-1","payload":{"code":42}}"#,
                &map,
            ),
            Some((2, 42)),
            "exit event routed to the mapped pane with its code"
        );
        assert_eq!(
            super::parse_stream_event(
                br#"{"event":"exit","sessionId":"ghost","payload":{"code":7}}"#,
                &map,
            ),
            Some((0, 7)),
            "unknown session → pane 0"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"event":"data","sessionId":"sess-1"}"#, &map),
            None,
            "non-exit events are ignored"
        );
        assert_eq!(
            super::parse_stream_event(b"not json", &map),
            None,
            "non-JSON → None"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"sessionId":"sess-1"}"#, &map),
            None,
            "missing event field → None"
        );
        assert_eq!(
            super::parse_stream_event(br#"{"event":"exit","payload":{"code":0}}"#, &map),
            None,
            "missing sessionId → None"
        );
    }

    #[test]
    fn orchestration_drained_reflects_coordinator_state() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(
            app.orchestration_drained(),
            "no coordinator (plain run) ⇒ vacuously drained"
        );

        // A coordinator with a still-Pending task ⇒ not drained.
        let mut coord = Coordinator::new();
        let tid = coord.add_task("work", Vec::new());
        app.coordinator = Some(coord);
        assert!(!app.orchestration_drained(), "a Pending task ⇒ not drained");

        // Drive it terminal (Done) ⇒ drained.
        if let Some(c) = app.coordinator.as_mut() {
            c.report_done(tid, "finished");
        }
        assert!(app.orchestration_drained(), "Done is terminal");

        // A Failed task is also terminal.
        let mut coord = Coordinator::new();
        let f = coord.add_task("boom", Vec::new());
        coord.report_failed(f, "oops");
        app.coordinator = Some(coord);
        assert!(app.orchestration_drained(), "Failed is terminal too");
    }

    #[test]
    fn ctrl_n_spawns_a_new_pane_and_keeps_parallel_vecs_aligned() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        let before = app.panes.len();
        // Spawn picker moved behind the gateway: enter Pane (Ctrl+Alt+P),
        // then press `n` to open the picker; Enter spawns the selected agent.
        app.handle_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Spawn, "Pane + n opens spawn picker");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the picker");
        assert_eq!(
            app.panes.len(),
            before + 1,
            "Pane + n → Enter adds one pane"
        );
        // Every parallel per-pane vector must stay aligned with `panes`.
        let n = app.panes.len();
        assert_eq!(app.sessions.len(), n, "sessions aligned");
        assert_eq!(app.pane_task.len(), n, "pane_task aligned");
        assert_eq!(app.pane_command.len(), n, "pane_command aligned");
        assert_eq!(app.reconnect.len(), n, "reconnect aligned");
        assert_eq!(app.reconnect_due.len(), n, "reconnect_due aligned");
        assert_eq!(app.pinned.len(), n, "pinned aligned");
        assert_eq!(
            app.daemon_session_ids.len(),
            n,
            "daemon_session_ids aligned"
        );
        assert_eq!(app.focus, before, "spawned pane is focused");
        app.render().expect("render after spawn");
    }

    #[test]
    fn jump_palette_backspace_and_navigation_clamp() {
        let mut app = App::for_test(vec![
            pane(0, "claude"),
            pane(1, "codex"),
            pane(2, "opencode"),
        ]);
        app.mode = InputMode::Jump;
        for c in ['o', 'd', 'e', 'x'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.jump_query, "odex");
        assert_eq!(app.jump_filtered(), vec![1], "only codex matches 'odex'");

        // Backspace trims the last char and resets the selection to the top.
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.jump_query, "ode");
        assert_eq!(
            app.jump_filtered(),
            vec![1, 2],
            "'ode' widens back to codex + opencode"
        );
        assert_eq!(app.jump_selected, 0);

        // Down / Up move the selection; both ends clamp (no under/overflow).
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 1, "Down at the bottom clamps");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.jump_selected, 0, "Up at the top clamps");
    }

    #[test]
    fn jump_palette_enter_with_no_matches_keeps_focus() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        app.mode = InputMode::Jump;
        app.focus = 1;
        for c in ['z', 'z'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(app.jump_filtered().is_empty(), "no agent matches 'zz'");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the palette");
        assert_eq!(app.focus, 1, "Enter with no matches keeps the prior focus");
    }

    #[test]
    fn drain_bus_applies_queued_updates() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        // Push two updates into the bus channel.
        let _ = app.bus_tx.send(AgentUpdate::Output {
            pane_id: 0,
            bytes: b"bus-fed".to_vec(),
        });
        let _ = app.bus_tx.send(AgentUpdate::State {
            pane_id: 1,
            state: AgentState::Done(Some(0)),
        });
        // drain_bus should apply both and return true (activity detected).
        assert!(app.drain_bus(), "drain_bus returns true when updates exist");
        // A second drain with nothing queued returns false.
        assert!(!app.drain_bus(), "drain_bus returns false when empty");
        // Pane 0 got the bytes, pane 1 got the state.
        assert!(
            app.panes[0].emulator().cell(0, 0).unwrap().has_contents(),
            "bus-fed bytes reached pane 0"
        );
        assert!(
            matches!(app.panes[1].state(), AgentState::Done(_)),
            "bus-fed state reached pane 1"
        );
    }

    #[test]
    fn apply_update_state_sets_pane_state() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert!(
            matches!(app.panes[0].state(), AgentState::Running),
            "starts Running"
        );
        app.apply_update(AgentUpdate::State {
            pane_id: 0,
            state: AgentState::Done(Some(0)),
        });
        assert!(
            matches!(app.panes[0].state(), AgentState::Done(_)),
            "State update transitioned to Done"
        );
        // Out-of-range pane_id is a no-op (no panic).
        app.apply_update(AgentUpdate::State {
            pane_id: 99,
            state: AgentState::Failed("nope".into()),
        });
    }

    #[test]
    fn handle_event_mouse_routes_to_handle_mouse() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Feeding some content so the pane has scrollback.
        app.panes[0].feed(b"line1\nline2\nline3");
        // A scroll-up mouse event should not panic.
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        // A scroll-down should also not panic.
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        // Other mouse kinds are no-ops (no panic).
        app.handle_event(Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[test]
    fn handle_event_resize_is_a_noop() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        let mode_before = app.mode;
        app.handle_event(Event::Resize(120, 40));
        assert_eq!(app.mode, mode_before, "Resize does not change mode");
    }

    #[test]
    fn handle_event_key_release_does_not_trigger_handle_key() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        assert_eq!(app.mode, InputMode::Normal);
        // A key RELEASE event should NOT enter pane mode (only Press does).
        app.handle_event(Event::Key(KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        }));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Release does not trigger mode switch"
        );
    }

    #[test]
    fn handle_event_other_event_is_noop() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // FocusGained is an "other" event variant → no-op.
        app.handle_event(Event::FocusGained);
        app.handle_event(Event::FocusLost);
        app.handle_event(Event::Paste("hello".to_string()));
        assert!(!app.quit, "unrecognized events don't quit");
    }

    #[test]
    fn forward_key_to_agent_ctrl_chars_are_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // In Normal mode, Ctrl+C should forward byte 0x03 (but with no
        // session it's a silent no-op — no panic).
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        // Ctrl+A → byte 0x01, Ctrl+Z → 0x1a.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+C in standalone (no Ctrl+Q) does not quit");
    }

    #[test]
    fn forward_key_to_agent_alt_char_is_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Alt+x should be a no-op (falls through to the _ => {} arm because
        // the Char(!ctrl && !alt) arm doesn't match, and there's no
        // Char + alt arm).
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        // No panic = pass.
    }

    #[test]
    fn forward_key_to_agent_special_keys_are_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Various special keys that have explicit arms — all no-ops with
        // no session.
        for code in [
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Delete,
            KeyCode::BackTab,
        ] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        // No panic = pass.
    }

    #[test]
    fn send_to_focused_is_silent_noop_with_no_session() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // No live session → send_to_focused is a silent no-op.
        app.send_to_focused(b"test");
        // No panic, pane unchanged.
        assert!(
            !app.panes[0].emulator().cell(0, 0).unwrap().has_contents(),
            "no bytes written with no session"
        );
    }

    #[test]
    fn render_with_sidebar_shows_connection_state() {
        let mut app = App::for_test(vec![pane(0, "claude"), pane(1, "codex")]);
        // Default connection state is Standalone — render with sidebar on.
        app.render().expect("render");
        let text = buffer_text(&app);
        assert!(
            text.contains("Standalone"),
            "sidebar shows Standalone connection state"
        );
    }

    #[test]
    fn render_with_sidebar_hidden_still_works() {
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.sidebar_hidden = true;
        app.render().expect("render with sidebar hidden");
        let text = buffer_text(&app);
        assert!(text.contains("solo"), "pane name still rendered");
    }

    #[test]
    fn render_with_toasts_overlay_does_not_panic() {
        let mut app = App::for_test(vec![pane(0, "solo")]);
        app.toasts.push(crate::toast::Toast::error("daemon error"));
        app.toasts.push(crate::toast::Toast::warning("slow"));
        app.render().expect("render with toasts");
        let text = buffer_text(&app);
        assert!(text.contains("daemon error"), "toast message rendered");
    }

    #[test]
    fn activity_overlay_toggles_with_a_key() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        app.mode = InputMode::Pane;
        // Bare `a` in Pane mode opens the activity overlay.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Activity,
            "'a' in Pane mode should open the Activity overlay"
        );
        // Esc closes it (back to Normal).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc should close the Activity overlay"
        );
    }

    #[test]
    fn activity_log_records_state_transition() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Pretend pane 0 was previously derived as Working so the first
        // derivation below is treated as a real transition (prev != new).
        app.last_status = vec![Some(AgentStatus::Working)];
        // Flip the pane to a Failed lifecycle state — `record_activity` should
        // then emit both a State{Working→Failed} and an Error event.
        app.panes[0].set_state(AgentState::Failed("boom".into()));
        app.record_activity();

        assert!(
            app.activity.len() >= 1,
            "at least one event should be recorded on a real transition"
        );
        // At least one recorded event's render_line must mention the pane name
        // AND contain either the arrow (State) or "error" (Error).
        let lines: Vec<String> = app
            .activity
            .recent(app.activity.len())
            .iter()
            .map(|e| e.render_line())
            .collect();
        let found = lines
            .iter()
            .any(|l| l.contains("a") && (l.contains("error") || l.contains("\u{2192}")));
        assert!(
            found,
            "expected a line naming pane 'a' with an arrow or 'error'; got {lines:?}"
        );
    }

    #[test]
    fn spawn_options_includes_custom_command_entry() {
        let app = App::for_test(vec![pane(0, "a")]);
        let opts = app.spawn_options();
        // The custom-command sentinel must be the LAST entry.
        let last = opts
            .last()
            .expect("spawn_options should always have at least bash + custom");
        assert!(
            last.0.contains("Custom command"),
            "last entry name should contain 'Custom command'; got {:?}",
            last.0
        );
        assert!(
            last.1.is_empty(),
            "custom-command sentinel must have an empty command vector; got {:?}",
            last.1
        );
    }

    #[test]
    fn custom_command_modal_types_and_spawns() {
        let mut app = App::for_test(vec![pane(0, "a")]);
        // Enter the spawn picker and navigate to the custom-command sentinel
        // (the last entry).
        app.mode = InputMode::Spawn;
        app.spawn_selected = app.spawn_options().len() - 1;
        // Enter on the sentinel switches to the text-entry modal.
        app.handle_spawn_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::SpawnCustom,
            "Enter on the custom sentinel should open SpawnCustom mode"
        );
        assert!(
            app.custom_cmd.is_empty(),
            "custom_cmd should be cleared when entering SpawnCustom"
        );
        // Type "bash".
        for ch in "bash".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
        }
        assert_eq!(
            app.custom_cmd, "bash",
            "typing should accumulate into custom_cmd"
        );
        // Backspace pops one char.
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.custom_cmd, "bas", "Backspace should pop the last char");
        // Esc cancels back to Normal (without spawning).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            app.mode,
            InputMode::Normal,
            "Esc should cancel SpawnCustom back to Normal"
        );
    }

    #[test]
    fn dashboard_groups_panes_by_status_bucket() {
        // Three panes in distinct lifecycle states map to the three dashboard
        // buckets: Running→Working, Done→done, Failed→needs-attention.
        let mut working_pane = pane(0, "alpha");
        working_pane.set_state(AgentState::Running); // Working bucket
        let mut done_pane = pane(1, "beta");
        done_pane.set_state(AgentState::Done(Some(0))); // done bucket
        let mut failed_pane = pane(2, "gamma");
        failed_pane.set_state(AgentState::Failed("boom".into())); // needs-attention bucket

        let mut app = App::for_test(vec![working_pane, done_pane, failed_pane]);
        app.mode = InputMode::Dashboard;
        app.render().expect("render into TestBackend");

        let text = buffer_text(&app);
        // The overlay title + all three column headers render.
        assert!(text.contains("Agent Dashboard"), "overlay title rendered");
        assert!(
            text.contains("needs-attention"),
            "needs-attention column header rendered"
        );
        assert!(text.contains("working"), "working column header rendered");
        assert!(text.contains("done"), "done column header rendered");
        // Each pane name lands in its bucket: alpha (Working), beta (Done),
        // gamma (Failed) all appear somewhere in the overlay text.
        assert!(text.contains("alpha"), "Working pane name rendered");
        assert!(text.contains("beta"), "Done pane name rendered");
        assert!(text.contains("gamma"), "Failed pane name rendered");
        // The counts appear in the headers: each bucket has exactly 1 member.
        assert!(
            text.contains("needs-attention (1)"),
            "needs-attention count is 1"
        );
        assert!(text.contains("working (1)"), "working count is 1");
        assert!(text.contains("done (1)"), "done count is 1");
    }
}
