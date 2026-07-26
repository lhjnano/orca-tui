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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
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

use crate::agent::{AgentKind, AgentSpec, AgentState, AgentStatus};
use crate::bus::{self, AgentUpdate, AgentUpdateReceiver, AgentUpdateSender};
use crate::config::Config;
use crate::coordinator::{self, Coordinator};
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

const FOOTER_NORMAL: &str =
    " Ctrl+P: control \u{00B7} Ctrl+Q: quit \u{00B7} Ctrl+N: new \u{00B7} Ctrl+B: sidebar ";
const FOOTER_PANE: &str =
    " hjkl: focus \u{00B7} Tab: next \u{00B7} p: pin \u{00B7} x: close \u{00B7} z: zoom \u{00B7} ?: help \u{00B7} Esc: back ";
const FOOTER_JUMP: &str =
    " type to filter \u{00B7} \u{2191}\u{2193} select \u{00B7} Enter: focus \u{00B7} Esc: cancel ";
const FOOTER_SPAWN: &str = " \u{2191}\u{2193} select \u{00B7} Enter: spawn \u{00B7} Esc: cancel ";
const FOOTER_ZOOM: &str = " z: unzoom \u{00B7} Ctrl+Q: quit \u{00B7} Ctrl+B: sidebar ";

/// Practical minimum pane inner size for agents to render. Below this, the
/// pane is too small for most TUI agents and spawning is blocked.
const MIN_PANE_COLS: u16 = 24;
const MIN_PANE_ROWS: u16 = 5;

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
            zoomed: false,
            show_help: false,
            conn_state: ConnectionState::Standalone,
            toasts: crate::toast::ToastQueue::new(),
            daemon: None,
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
        let panes = &mut self.panes;
        let theme = &self.config.theme;
        // Agent status tallies for the footer status bar (opencode-style).
        let statuses: Vec<AgentStatus> = sidebar_entries
            .iter()
            .map(|e| AgentStatus::derive(&e.state, e.activity.as_ref().map(|a| a.state.as_str())))
            .collect();
        let n_working = statuses.iter().filter(|s| s.is_active()).count();
        let n_failed = statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Failed))
            .count();
        let n_done = statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::Done))
            .count();
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
                let status_line = Line::from(vec![
                    Span::styled(
                        format!(" ● {n_working} working "),
                        Style::default().fg(theme.success()).bg(theme.panel()),
                    ),
                    Span::styled(
                        format!(" ✗ {n_failed} failed "),
                        Style::default().fg(theme.error()).bg(theme.panel()),
                    ),
                    Span::styled(
                        format!(" ✓ {n_done} done "),
                        Style::default().fg(theme.muted()).bg(theme.panel()),
                    ),
                ]);
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
                f.render_widget(Paragraph::new(lines), inner);
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
                f.render_widget(Paragraph::new(lines), inner);
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
                    Line::from("  Ctrl+P    Enter Pane mode"),
                    Line::from("  Ctrl+Q    Quit"),
                    Line::from("  Ctrl+N    Spawn picker (select agent)"),
                    Line::from("  Ctrl+B    Toggle sidebar"),
                    Line::from("  scroll    Scroll focused pane"),
                    Line::raw(""),
                    Line::from(vec![Span::styled(
                        "Pane mode (Ctrl+P)",
                        Style::default()
                            .fg(theme.accent())
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )]),
                    Line::from("  h j k l   Move focus (or arrows)"),
                    Line::from("  Tab       Next pane (wraps)"),
                    Line::from("  p         Pin / unpin agent"),
                    Line::from("  x         Close focused pane"),
                    Line::from("  z         Zoom / unzoom focused pane"),
                    Line::from("  /         Jump palette (fuzzy-focus)"),
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
                    Line::from("  Ctrl+P → z   Unzoom"),
                    Line::from("  Esc          Normal (interact with agent while zoomed)"),
                ];
                f.render_widget(Paragraph::new(help_lines), inner);
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
        // Global hotkeys (any mode): Ctrl+P → pane mode, Ctrl+Q → quit.
        if ctrl && key.code == KeyCode::Char('p') {
            self.mode = InputMode::Pane;
            return;
        }
        if ctrl && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        // Ctrl+N → open the spawn picker (select which agent to add).
        if ctrl && key.code == KeyCode::Char('n') {
            self.spawn_selected = 0;
            self.mode = InputMode::Spawn;
            return;
        }
        // Ctrl+B → toggle the sidebar (adaptive layout: auto-hides on narrow
        // terminals; the user can force-show/-hide at any width).
        if ctrl && key.code == KeyCode::Char('b') {
            self.sidebar_hidden = !self.sidebar_hidden;
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
                // (no modifiers) — Ctrl+P is intercepted above as the mode-enter
                // hotkey, so it never reaches this arm.
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
                _ => {}
            },
            InputMode::Jump => self.handle_jump_key(key),
            InputMode::Spawn => self.handle_spawn_key(key),
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

    /// Normal-mode passthrough: forward the key to the focused agent's PTY as
    /// raw bytes / VT escape sequences. The agent receives everything — Tab,
    /// Esc, Ctrl+C, arrows — exactly as if it were a real terminal.
    fn forward_key_to_agent(&mut self, key: KeyEvent) {
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
            _ => {}
        }
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
                zoomed: false,
                show_help: false,
                conn_state: ConnectionState::Standalone,
                toasts: crate::toast::ToastQueue::new(),
                daemon: None,
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

        // Ctrl+P enters pane mode.
        app.quit = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, InputMode::Pane, "Ctrl+P enters pane mode");

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
        // Ctrl+B is a global hotkey — works from any mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(app.sidebar_hidden, "Ctrl+B hides the sidebar");
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(!app.sidebar_hidden, "Ctrl+B again re-shows it");
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
        assert!(text.contains("Ctrl+P"), "footer rendered");
        assert!(text.contains("quit"));
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
        // Ctrl+N opens the spawn picker; Enter spawns the selected agent.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, InputMode::Spawn, "Ctrl+N opens spawn picker");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Normal, "Enter closes the picker");
        assert_eq!(app.panes.len(), before + 1, "Ctrl+N → Enter adds one pane");
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
            modifiers: KeyModifiers::CONTROL,
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
}
