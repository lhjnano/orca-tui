> orcatui technical architecture · last updated 2026-07-27 · 403 tests · CI green

# orcatui — Technical Architecture

This is the engineering reference for orcatui: how a byte from a coding agent
becomes a rendered cell, which module owns which concern, where the performance
budget goes, and why each major design choice was made. For install, CLI
reference, key bindings, and configuration see [README.md](../README.md); for
per-feature behavior see [features.md](features.md).

## 1. Overview

orcatui is the terminal port of **Orca GUI** ([stablyai/orca]). It runs N coding
agents (Claude Code, Codex, OpenCode, Gemini, ...) each in its own PTY and
optional git worktree, side-by-side in split terminal panes, monitored and
steered from one screen. Target: **20 agents @ 60 fps @ ≤ 100 ms response**.

**Why TUI not GUI?** WSL / headless / no-GPU machines cannot accelerate the
Electron-based Orca GUI, so it is heavy and janky. A terminal draws only
character cells — near-zero GPU cost — which lets orcatui hold 20 agents at 60 fps
inside a tight end-to-end budget while still running an orchestration layer on
top (unlike tmux/zellij). orcatui drives PTYs directly: **no tmux dependency**.

| Feature | Orca GUI | orcatui | Claude Squad | tmux |
|---------|----------|---------|--------------|------|
| Interface | Electron | TUI (Rust + ratatui) | TUI (Go + bubbletea) | TUI (C) |
| GPU needed | Yes | No | No | No |
| tmux dependency | No | No (direct PTY) | Yes (required) | — |
| Orchestration | Yes | Yes | No | No |
| SSH remote | Yes | Yes (`--remote`) | No | No |
| Agent activity (OSC 9999) | Yes | Yes (sidebar) | No | No |

## 2. Data flow

Standalone path (the default — orcatui owns the PTYs):

```
N Agent Processes (Claude Code, Codex, OpenCode, ...)
each in its own PTY + optional git worktree
    │
    ▼  PTY byte stream
[portable-pty]     PTY spawn + blocking read (std::thread per agent)
    ▼
[OscScanner]       intercept OSC 9999 → AgentActivity (all other bytes pass through)
    ▼
[SyncScanner]      buffer mode-2026 batches (ESC[?2026h … clear+draw … ESC[?2026l),
    │              flush atomically so a mid-batch render sees the previous frame
    ▼
[vt100]            ANSI parse + terminal emulation → styled Cell grid
    │
    ▼  AgentBus (tokio mpsc, N→1, batch-drained per frame)
[FrameScheduler]   16 ms budget, frame-skip on backlog, idle backoff
    ▼
[App::render]      Layout::horizontal [sidebar | panes]
                   Layout::vertical   [panes | footer]
                   sidebar::render_sidebar + Pane::render + footer + toast overlay
    ▼
[crossterm]         escape sequences → stdout
```

**Daemon path** (`orcatui daemon` built-in server, or `--daemon` Orca GUI client):
the daemon owns the PTYs and ships PTY bytes over a Unix socket
(`daemon_server` / `orca_daemon`). `App`'s stream-reader thread routes those frames
into the same `AgentBus`, so everything downstream of the bus — `FrameScheduler`,
`App::render`, crossterm — is identical to the standalone path.

> The query responder (`query.rs`) operates on the reciprocal **input** path:
> when an agent probes the terminal it writes query sequences into its PTY, and
> orcatui synthesizes the replies (see §4). It is not part of the render pipeline
> above, but without it the agent never emits a drawable frame.

## 3. Module map

Authoritative module list from [`src/lib.rs`](../src/lib.rs) — 26 library modules
plus 2 binaries:

```
src/
├── main.rs            orcatui binary — thin entry; calls cli::run, maps to ExitCode
├── lib.rs             pub mod declarations (the authoritative module list)
├── activity.rs        ActivityEvent + ActivityLog (bounded ring buffer, cap 500) — pure logic, not yet wired to App
├── agent.rs           AgentKind + AgentState + AgentStatus (4-state) + AgentSpec; detect_installed()
├── app.rs             App<B: Backend> — state + main_loop + render + handle_key + daemon stream-reader thread
├── bus.rs             AgentBus (tokio mpsc, N→1, batch drain) + forward_session bridge
├── cli.rs             clap CLI (run / orchestrate / prs / issues / mobile / daemon / attach)
├── clipboard.rs       zero-dependency clipboard copy — shells out to the first available platform tool
├── config.rs          Config + ThemeConfig + DaemonConfig (TOML; 3-level bg + border/muted tokens)
├── coordinator.rs     Coordinator + Task/Dispatch/DecisionGate/Inbox; plan_chain (seq) / plan_from_spec (parallel)
├── daemon_server.rs   built-in Rust daemon — owns PTYs, NDJSON over Unix socket, serves `orcatui attach` clients
├── hangul.rs          Hangul jamo composition (UAX #15) — tested building block, NOT wired to live input
├── integrations.rs    GitHub (gh CLI) + LinearSource (stub)
├── layout.rs          split_panes (grid)
├── mobile.rs          WebSocket server (tokio-tungstenite) + AgentSnapshot
├── orca_daemon.rs     Orca GUI daemon client — binary-frame + NDJSON, hello/RPC/stream, reconnect
├── osc.rs             OscScanner (OSC 9999 DFA) + AgentActivity
├── pane.rs            Pane — vt100→ratatui bridge + OSC feed + scrollback, themed borders
├── perf_probe.rs      #[ignore] perf tests (terminal_emu / scheduler / render)
├── pty_session.rs     PtySession — portable-pty spawn + reader thread; TERM injection
├── query.rs           QueryResponder — stateful DFA answering OSC color / DECRQM / DA / DCS probes
├── scheduler.rs       FrameScheduler — 60 fps policy, clock-injectable
├── sidebar.rs         SidebarEntry + render_sidebar (bordered panel box, 2-line entries, Pinned section)
├── ssh.rs             SshTarget + ReconnectPolicy + ReconnectSession
├── sync.rs            SyncScanner — mode-2026 batch buffering, atomic flush
├── terminal_emu.rs    TerminalEmulator — vt100 wrapper (ratatui-free)
├── toast.rs           transient toast overlay + ConnectionState (Standalone / Daemon / Disconnected)
└── worktree.rs        WorktreeManager + OwnedWorktrees (git CLI, Drop guard)

src/bin/
└── inject.rs          orcatui-inject binary — record / replay / snapshot tool (see §4)
```

## 4. Agent rendering compatibility

A naive PTY + vt100 embedder shows **blank or flickering panes** for sophisticated
TUI agents (opencode and any agent using synchronized output). orcatui handles
the three things they need:

| Layer | Module | What it does |
|-------|--------|--------------|
| **`TERM` injection** | `pty_session.rs` | Child PTY gets `TERM=xterm-256color` + `COLORTERM=truecolor` + `TERM_PROGRAM=orcatui`, so the agent emits sequences the vt100 emulator understands (a multiplexer must declare what it emulates, like tmux). |
| **Query responder** | `query.rs` | A stateful DFA answers the agent's capability probes — OSC 10/11/12 color, DECRQM private modes, DA1/DA2, DCS terminfo — synthesized from `ThemeConfig`. Without replies a probing agent cannot determine the terminal and renders blank. |
| **Synchronized output** | `sync.rs` | Buffers `ESC[?2026h … clear+redraw … ESC[?2026l` batches and flushes them to the emulator **atomically**, so a render that fires mid-batch sees the previous frame, not the half-cleared intermediate (the "blank pane" symptom). General — fixes any agent using mode 2026, not just opencode. |

**`orcatui-inject`** (`src/bin/inject.rs`) is the deterministic debugger for all
three layers: `record` captures a real agent's raw PTY bytes (optionally resizing
mid-recording to reproduce orcatui's spawn-then-resize), and `replay` feeds them
through the emulator + query responder + sync batcher at a chosen size, optionally
rendering a real pane (`--render`). This is what isolated the opencode blank-pane
bug: it proved the emulator, alt-screen, size, resize, and responder were all fine
and pinned the cause on mode-2026 synchronized output. Set `ORCA_DEBUG_LOG=1` on
`orcatui` to log per-chunk byte/cell counts + resize events to `/tmp/orca-live.log`.

## 5. Performance

Measured, release build:

| Hot path | Result | % of 16.67 ms budget |
|----------|--------|----------------------|
| FrameScheduler decision | **9.69 ns/op** | ~0% |
| Terminal emulation ingest | **48.5 MB/s** | alacritty-grade |
| 1-pane render | **330 µs** | 2.0% |
| 5-pane render | **818 µs** | 4.9% |
| 10-pane render | **1.25 ms** | 7.5% |
| 20-pane render | **2.16 ms** | 13.0% |

> 20-pane render at 13% of budget → **60 fps for N=20 with 87% headroom**.
> Pane borders use ppalla `PreparedBlock` — cached glyph placement keyed on
> dimensions + title, with the focus color applied at paint time and excluded
> from the cache key, so toggling focus is free. 37–43% faster than the original
> ratatui `Block`. (`PreparedBuffer` was tried for dirty-only cell repaint but
> reverted: incompatible with ratatui's double-buffered `Terminal` — the back
> buffer swaps each flush, so unrepainted cells carry 2-frame-old content.)

## 6. Test coverage

**403 tests passing · 0 failures · CI green**
(`cargo fmt` + `cargo clippy -A dead_code` + `cargo test`).
The line-coverage percentage below is from an older `tarpaulin` run and has not
been re-measured since the test count grew from 226 to 403.

```
Well-covered (80–100%):     coordinator 100%, scheduler 100%, ssh 99%,
                            terminal_emu 98%, layout 95%, agent 93%,
                            pty_session 88%, pane 89%, bus 92%
Gaps (TTY/IO-bound):        app.rs main_loop/run (19%), cli.rs handlers (9%),
                            mobile.rs serve (89%), integrations gh (38%)
```

## 7. Resolved design decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| GUI vs TUI | TUI | WSL has no GPU; Electron too heavy |
| tmux usage | none (direct PTY) | control, performance |
| Language | Rust | performance, safety, ratatui ecosystem |
| Async runtime | tokio | mpsc perf; used as a channel only (no runtime in the UI loop) |
| ppalla source | crates.io v0.0.3 | reproducible, publishable |
| Terminal emulation | vt100 0.15 (not 0.16) | ratatui 0.29 exact-pins `unicode-width =0.2.0`; vt100 0.16 needs `^0.2.1` |
| Input model | zellij-style modes (`Ctrl+P`) | passthrough lets agents use Tab/Esc freely |
| Agent activity | OSC 9999 capture (OscScanner) | same channel Orca uses; the data is already in the PTY stream |
| Config format | TOML | Rust convention |
| Agent registry | builtin + PATH detection | mirrors Orca "run any CLI agent" |
| git operations | CLI (not git2-rs) | no native dep; dev env has git |

---

### See also

- [README.md](../README.md) — install, CLI reference, key bindings, configuration, tech stack.
- [ROADMAP.md](ROADMAP.md) — feature status, UI/UX session log, opencode layout study.
- [features.md](features.md) — per-feature behavior reference (planned).

[stablyai/orca]: https://github.com/stablyai/orca
