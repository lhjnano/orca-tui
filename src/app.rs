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
use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::agent::{AgentSpec, AgentState};
use crate::bus::{self, AgentUpdate, AgentUpdateReceiver};
use crate::layout::split_panes;
use crate::mobile::AgentSnapshot;
use crate::pane::Pane;
use crate::pty_session::PtySession;
use crate::scheduler::{FrameScheduler, TARGET_FRAME_60FPS};
use crate::terminal_emu::{MIN_COLS, MIN_ROWS};
use crate::worktree::{OwnedWorktrees, WorktreeManager};

use tokio::sync::mpsc::UnboundedSender;

/// One-line footer shown at the bottom of the screen with the keybindings.
const FOOTER: &str =
    " Tab: focus \u{00B7} Shift+Tab: prev \u{00B7} Alt+\u{2191}\u{2193}: scroll \u{00B7} Ctrl+C / Esc: quit \u{00B7} type to send ";

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

        for (idx, spec) in specs.into_iter().enumerate() {
            let name = spec.name.clone();
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
            match PtySession::spawn(spec.command.clone(), agent_cwd.as_deref(), cols, rows) {
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
                    eprintln!("orca-tui: failed to spawn {name:?}: {err:#}");
                    let mut pane = Pane::new(idx, &name, cols, rows);
                    pane.set_state(AgentState::Failed(format!("{err:#}")));
                    panes.push(pane);
                    sessions.push(None);
                }
            }
        }

        // Drop our own sender clone so disconnect is observable once every
        // forwarder has exited.
        drop(bus_tx);

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
            eprintln!("orca-tui: terminal restore failed: {restore_err:#}");
        }
        result
    }

    fn setup_terminal(&mut self) -> Result<()> {
        enable_raw_mode().context("enabling raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
        self.raw_mode_active = true;
        Ok(())
    }

    fn restore_terminal(&mut self) -> Result<()> {
        let mut stdout = io::stdout();
        // LeaveAlternateScreen must run before disable_raw_mode so the alt
        // screen swap isn't done in cooked mode.
        let _ = execute!(stdout, LeaveAlternateScreen);
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

            // Poll with the scheduler-chosen timeout: ~remaining-to-next-frame
            // when active, the longer idle interval when nothing is happening.
            if event::poll(self.scheduler.poll_timeout(now))? {
                let ev = event::read()?;
                // User input is activity — exit idle backoff immediately.
                self.scheduler.record_activity(Instant::now());
                self.handle_event(ev);
            }
            // Auto-exit once no agent process remains (all Exit updates
            // applied). A user can also quit explicitly with Esc / Ctrl+C.
            if self.all_sessions_gone() {
                break;
            }
        }
        Ok(())
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
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.feed(&bytes);
                }
            }
            AgentUpdate::State { pane_id, state } => {
                if let Some(pane) = self.panes.get_mut(pane_id) {
                    pane.set_state(state);
                }
            }
            AgentUpdate::Exit { pane_id, code } => {
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
            }
        }
    }

    /// Render the panes grid plus the footer. Per-pane viewport + PTY sizes are
    /// reconciled here (before the immutable draw closure) so the emulator and
    /// the agent process agree on dimensions.
    fn render(&mut self) -> Result<()> {
        // `Terminal::size` returns a `Size` in ratatui 0.29; `Layout::split`
        // wants a `Rect`, so wrap it at the origin.
        let size = self.terminal.size()?;
        let total = Rect::new(0, 0, size.width, size.height);

        // Reserve the last line for the footer — but only when there is enough
        // vertical room that the panes aren't starved to a 0-line band on a
        // degenerate (tiny) terminal. With no footer, the panes get the whole
        // area and the footer render is skipped below.
        let reserve_footer = total.height >= 3;
        let (pane_area, footer_area) = if reserve_footer {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(total);
            (chunks[0], chunks[1])
        } else {
            (total, Rect::default())
        };

        let rects = split_panes(pane_area, self.panes.len());

        // Resize pass: align each pane's emulator and PTY to its new inner area
        // (a 1-cell border on every side). Done outside the draw closure so we
        // can mutate `self`. Inner dims are clamped to the emulator minimum so
        // a pane starved by a tiny area never feeds `vt100` a `0` (which would
        // underflow at `grid.rs:26`).
        for (i, pane) in self.panes.iter_mut().enumerate() {
            let Some(&rect) = rects.get(i) else { continue };
            let inner_w = rect.width.saturating_sub(2).max(MIN_COLS);
            let inner_h = rect.height.saturating_sub(2).max(MIN_ROWS);
            let (cur_w, cur_h) = pane.size();
            if (cur_w, cur_h) != (inner_w, inner_h) {
                pane.resize_viewport(inner_w, inner_h);
                if let Some(Some(session)) = self.sessions.get_mut(i) {
                    // PTY resize is best-effort: a dead child's fd may reject it.
                    let _ = session.resize(inner_w, inner_h);
                }
            }
        }

        let focus = self.focus;
        let panes = &self.panes;
        self.terminal.draw(|f| {
            for (i, pane) in panes.iter().enumerate() {
                let area = rects.get(i).copied().unwrap_or_default();
                pane.render(f, area, i == focus);
            }
            // Skip the footer entirely on a degenerate terminal (the zero-sized
            // `footer_area` would be a no-op anyway, but being explicit avoids
            // relying on Paragraph's zero-area behavior).
            if reserve_footer {
                f.render_widget(
                    Paragraph::new(FOOTER).style(Style::default().fg(Color::DarkGray)),
                    footer_area,
                );
            }
        })?;

        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => {
                // On unix crossterm emits both Press and Release; only act on
                // Press to avoid firing every binding twice.
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            Event::Resize(_, _) => {
                // No explicit action: the next `render()` re-reads the terminal
                // size and reconciles each pane/PTY to its new rect.
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            // --- Quit -----------------------------------------------------
            // Esc always quits; Ctrl+C quits (plain `c` is forwarded below so
            // agents like a shell still receive it).
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            // --- Focus switching -----------------------------------------
            KeyCode::Tab => self.focus_next(),
            KeyCode::BackTab => self.focus_prev(),
            // --- Per-pane scroll -----------------------------------------
            KeyCode::Up if alt => {
                if let Some(p) = self.focused_pane_mut() {
                    p.scroll_up(1);
                }
            }
            KeyCode::Down if alt => {
                if let Some(p) = self.focused_pane_mut() {
                    p.scroll_down(1);
                }
            }
            // --- Forward to the focused PTY ------------------------------
            KeyCode::Char(c) if !ctrl && !alt => {
                self.send_to_focused(&[c as u8]);
            }
            KeyCode::Enter if !ctrl && !alt => {
                self.send_to_focused(b"\r");
            }
            KeyCode::Backspace if !ctrl && !alt => {
                self.send_to_focused(&[0x7f]);
            }
            _ => {}
        }
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
    fn send_to_focused(&mut self, bytes: &[u8]) {
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

impl<B: Backend> Drop for App<B> {
    fn drop(&mut self) {
        // Panic-safety: if `run` never restored (or panicked mid-loop), make
        // one best-effort attempt to give the user their terminal back. The
        // `PtySession` drops below kill+join any still-running agents.
        if self.raw_mode_active {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen);
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
            let (_tx, rx) = bus::channel();
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
    fn handle_key_esc_and_ctrl_c_quit_tab_focuses() {
        let mut app = App::for_test(vec![pane(0, "a"), pane(1, "b")]);
        assert!(!app.quit);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.quit, "Esc quits");

        app.quit = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit, "Ctrl+C quits");

        app.quit = false;
        app.focus = 0;
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, 1, "Tab advances focus");
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
}
