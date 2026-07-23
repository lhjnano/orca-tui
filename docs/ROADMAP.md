# Orca TUI — Roadmap & Architecture

Multi-agent coding orchestration in the terminal · Built on
[ratatui-ppalla](https://crates.io/crates/ratatui-ppalla) v0.0.2 ·
Target: **20 agents @ 60 fps @ ≤100 ms response**.

> Last updated: 2026-07-23 · 167 tests · 67% coverage · CI green ·
> [github.com/lhjnano/orca-tui](https://github.com/lhjnano/orca-tui)

---

## 1. Project overview

Orca TUI is the terminal version of **Orca GUI** ([stablyai/orca](https://github.com/stablyai/orca)).
It runs N coding agents (Claude Code, Codex, OpenCode, …) each in its own git
worktree, in parallel, and monitors/steers them from a single screen.

**Why TUI not GUI?** WSL environments often lack GPU acceleration, so the
Electron-based Orca GUI is heavy and janky. A terminal draws only character
cells — near-zero GPU cost.

### Comparison

| Feature | Orca GUI | Orca TUI | Claude Squad | tmux |
|---------|----------|----------|--------------|------|
| Interface | Electron | TUI (Rust + ratatui) | TUI (Go + bubbletea) | TUI (C) |
| GPU needed | ⚠️ | ❌ | ❌ | ❌ |
| tmux dependency | ❌ | ❌ (direct PTY) | ✅ tmux required | — |
| Orchestration | ✅ | ✅ | ❌ | ❌ |
| SSH remote | ✅ | ✅ (`--remote`) | ❌ | ❌ |
| Agent activity (OSC 9999) | ✅ | ✅ (sidebar) | ❌ | ❌ |

---

## 2. Current state — all 10 features implemented

| # | Feature | Status | Module(s) | Key API |
|---|---------|--------|-----------|---------|
| 1 | Single-agent pane | ✅ done | `pty_session.rs` `terminal_emu.rs` `pane.rs` | `PtySession::spawn` → `TerminalEmulator::feed` → `Pane::render` |
| 2 | Multi-pane layout | ✅ done | `layout.rs` `app.rs` | `split_panes(area, n)` — grid; `Ctrl+P` + arrows/hjkl focus |
| 3 | Agent registry | ✅ done | `agent.rs` | `AgentKind::detect_installed()` — Claude/Codex/OpenCode/Gemini/Amp/Cursor + Generic |
| 4 | AgentBus | ✅ done | `bus.rs` | tokio mpsc N→1; `forward_session` bridge; batch `try_recv` drain |
| 5 | FrameScheduler | ✅ done | `scheduler.rs` | 60fps throttle + frame-skip + idle backoff; clock-injectable |
| 6 | WorktreeManager | ✅ done | `worktree.rs` | git CLI; `OwnedWorktrees` Drop guard; `--worktree` flag |
| 7 | Coordination | ✅ done | `coordinator.rs` | `plan_chain` (sequential) / `plan_from_spec` (parallel); `orchestrate` CLI |
| 8 | SSH remote | ✅ done | `ssh.rs` | `SshTarget::command_vec`; `ReconnectSession` backoff; `--remote --reconnect` |
| 9 | GitHub/Linear | ✅ done | `integrations.rs` | `prs`/`issues` via gh CLI; `orchestrate --issues`; `LinearSource` stub |
| 10 | Mobile companion | ✅ server | `mobile.rs` | tokio-tungstenite WS server; `run --mobile <PORT>`; live snapshots |

> **F10 mobile client** (iOS/Android/PWA) is NOT implemented — only the WS
> server side. The client is a separate project that connects to the server.

### UI/UX features (implemented in the latest sessions)

| Feature | Module | Details |
|---------|--------|---------|
| **OSC 9999 capture** | `osc.rs` | OscScanner DFA — intercepts `\x1b]9999;{json}` from PTY stream, extracts `AgentActivity` (state/tool/toolInput/prompt/model), passes all other bytes through |
| **Orca-style sidebar** | `sidebar.rs` | `render_sidebar` — brand header, `IN PROGRESS (N)`, colored status dots (●green/✗red/○amber/✓gray), agent name + activity + branch |
| **Config system** | `config.rs` | TOML (`~/.config/orca-tui/config.toml`) — theme (hex colors), layout (sidebar_width, show_status_bar), default_agent |
| **Mode system** | `app.rs` | zellij-style: `Normal` (passthrough — all keys → agent) / `Ctrl+P` `Pane` (arrows/hjkl/Tab focus) / `Ctrl+Q` quit |
| **Mouse scroll** | `app.rs` | `EnableMouseCapture` + scroll up/down → focused pane scrollback (1000 lines, 3/notch) |
| **Focused pane color** | `pane.rs` | LightBlue border when focused, DarkGray when not |
| **Double borders** | `pane.rs` | `BorderType::Double` for clear pane separation |

---

## 3. Architecture

```
N Agent Processes (Claude Code, Codex, OpenCode, ...)
each in its own PTY + optional git worktree
   │
   ▼ PTY byte stream
[portable-pty]  PTY spawn + blocking read (std::thread per agent)
   ▼
[OscScanner]    intercept OSC 9999 → AgentActivity  ← NEW
   ▼
[vt100]         ANSI parse + terminal emulation → Cell grid
   │
   ▼ AgentBus (tokio mpsc, N→1, batch drain per frame)
[FrameScheduler]  16 ms budget, skip on backlog, idle backoff
   ▼
[App::render]   Layout::horizontal [sidebar | panes]
                Layout::vertical [panes | footer]
                sidebar::render_sidebar + Pane::render + footer
   ▼
[crossterm]     escape sequences → stdout
```

### Module map

```
src/
├── main.rs          binary entry (thin — calls cli::run)
├── lib.rs           pub mod declarations
├── cli.rs           clap CLI (run/orchestrate/prs/issues/mobile)
├── app.rs           App<B: Backend> — state + main_loop + render + handle_key
├── config.rs        Config + ThemeConfig (TOML, hex colors)
├── osc.rs           OscScanner (OSC 9999 DFA) + AgentActivity
├── sidebar.rs       SidebarEntry + render_sidebar (Orca-style)
├── pane.rs          Pane (vt100 → ratatui bridge + OSC feed + scroll)
├── layout.rs        split_panes (grid)
├── terminal_emu.rs  TerminalEmulator (vt100 wrapper, ratatui-free)
├── pty_session.rs   PtySession (portable-pty spawn + reader thread)
├── bus.rs           AgentBus (tokio mpsc) + forward_session
├── scheduler.rs     FrameScheduler (60fps policy, clock-injectable)
├── agent.rs         AgentKind + AgentState + AgentSpec
├── coordinator.rs   Coordinator + Task/Dispatch/DecisionGate/Inbox
├── ssh.rs           SshTarget + ReconnectPolicy + ReconnectSession
├── integrations.rs  GitHub (gh CLI) + LinearSource (stub)
├── worktree.rs      WorktreeManager + OwnedWorktrees (Drop guard)
├── mobile.rs        WS server (tokio-tungstenite) + AgentSnapshot
└── perf_probe.rs    #[ignore] perf tests (terminal_emu/scheduler/render)
```

---

## 4. Performance (measured, release build)

| Hot path | Result | % of 16.67 ms budget |
|----------|--------|----------------------|
| FrameScheduler decision | **9.69 ns/op** | ~0% |
| Terminal emulation ingest | **48.5 MB/s** | alacritty-grade |
| 1-pane render | **515 µs** | 3.1% |
| 5-pane render | **1.13 ms** | 6.8% |
| 10-pane render | **1.98 ms** | 11.9% |
| 20-pane render | **3.68 ms** | 22.1% |

> 20-pane render at 22% of budget → **60 fps for N=20 is achievable**.

---

## 5. Test coverage

```
167 tests · 67% line coverage (tarpaulin)

Well-covered (80-100%):     coordinator 100%, scheduler 100%, ssh 99%,
                            terminal_emu 98%, layout 95%, agent 93%,
                            pty_session 88%, pane 89%, bus 92%
Gaps (TTY/IO-bound):        app.rs main_loop/run (19%), cli.rs handlers (9%),
                            mobile.rs serve (89%), integrations gh (38%)
```

---

## 6. CLI reference

```
orca-tui run [--cwd DIR] [--worktree] [--remote HOST] [--reconnect] [--mobile PORT] -- <agent> [:: <agent> ...]
orca-tui orchestrate [--spec TEXT | --issues OWNER/NAME] [--parallel]
orca-tui prs <owner/name>
orca-tui issues <owner/name>
orca-tui mobile [--port PORT]
orca-tui --version
```

### Key bindings

| Key | Mode | Action |
|-----|------|--------|
| `Ctrl+P` | any | Enter Pane mode |
| `Ctrl+Q` | any | Quit orca-tui |
| `←↑↓→` / `hjkl` | Pane | Grid-aware focus switch |
| `Tab` / `Shift+Tab` | Pane | Sequential focus next/prev |
| `Esc` | Pane | Return to Normal (passthrough) |
| Mouse scroll | any | Scroll focused pane scrollback |
| [any key] | Normal | Forwarded to focused agent's PTY |

### Config file (`~/.config/orca-tui/config.toml`)

```toml
default_agent = "claude"

[layout]
sidebar_width = 26        # 0 hides the sidebar
show_status_bar = true

[theme]
background = "#0d1117"
foreground = "#e6edf3"
accent   = "#58a6ff"
success  = "#3fb950"
warning  = "#d29922"
error    = "#f85149"
```

---

## 7. Resolved decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| GUI vs TUI | TUI | WSL has no GPU; Electron too heavy |
| tmux usage | none (direct PTY) | control, performance |
| Language | Rust | performance, safety, ratatui ecosystem |
| Async runtime | tokio | mpsc perf; used as channel only (no runtime in UI loop) |
| ppalla source | crates.io v0.0.2 | reproducible, publishable |
| Terminal emulation | vt100 0.15 (not 0.16) | ratatui 0.29 pins unicode-width 0.2.0; vt100 0.16 needs ^0.2.1 |
| Input model | zellij-style modes (Ctrl+P) | passthrough lets agents use Tab/Esc freely |
| Agent activity | OSC 9999 capture (OscScanner) | same channel Orca uses; data IS in PTY stream |
| Config format | TOML | Rust convention |
| Agent registry | builtin + PATH detection | mirrors Orca "run any CLI agent" |
| git operations | CLI (not git2-rs) | no native dep, dev env has git |

---

## 8. Next steps

### Immediate (layout/UX polish)
- **Adaptive layout** — hide sidebar on small terminals (<100 cols), toggle with key
- **Sidebar scroll** — j/k when sidebar focused, auto-scroll to active agent
- **Jump palette** — `/` fuzzy search to find and focus an agent (ppalla list widget)
- **Tabs** — worktree/session tab strip at the top

### Medium
- Linear real implementation (reqwest + GraphQL + LINEAR_API_KEY)
- README rewrite with screenshot + install instructions (cargo install / binstall / brew / binary)
- Release workflow (taiki-e/upload-rust-binary-action for cross-platform binaries)
- Coverage → 80% (TestBackend-driven main_loop tests)

### Research
- opencode TUI source analysis (`packages/tui/src/app.tsx`) for layout/region patterns
- Orca GUI `AgentStateDot.tsx` + `agent-status-types.ts` for richer status mapping
- vt100 0.16 bump when ratatui relaxes unicode-width pin

---

## 9. Resuming in a new session

```bash
cd ~/source/project/orca-tui
cargo build          # 0 errors
cargo test           # 167 passed
cargo bench --bench orca   # 3 hot paths
cargo run --example screenshot   # renders a sample frame with sidebar
```

### Related links

- ratatui-ppalla: <https://crates.io/crates/ratatui-ppalla>
- Orca GUI (original): <https://github.com/stablyai/orca>
- opencode (TUI reference): <https://github.com/sst/opencode>
- Claude Squad (reference): <https://github.com/smtg-ai/claude-squad>
- Pretext (inspiration): <https://github.com/chenglou/pretext>
