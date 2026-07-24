# Orca TUI — Roadmap & Architecture

Multi-agent coding orchestration in the terminal · Built on
[ratatui-ppalla](https://crates.io/crates/ratatui-ppalla) v0.0.3 ·
Target: **20 agents @ 60 fps @ ≤100 ms response**.

> Last updated: 2026-07-24 · 226 tests · 56% coverage · CI green ·
> [github.com/lhjnano/orcatui](https://github.com/lhjnano/orcatui)

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
| **Config system** | `config.rs` | TOML (`~/.config/orcatui/config.toml`) — theme (hex colors), layout (sidebar_width, show_status_bar), default_agent |
| **Mode system** | `app.rs` | zellij-style: `Normal` (passthrough — all keys → agent) / `Ctrl+P` `Pane` (arrows/hjkl/Tab focus) / `Ctrl+Q` quit |
| **Mouse scroll** | `app.rs` | `EnableMouseCapture` + scroll up/down → focused pane scrollback (1000 lines, 3/notch) |
| **Focused pane color** | `pane.rs` | themed borders — `border_active` when focused, `border` when not (was LightBlue/DarkGray) |
| **Double borders** | `pane.rs` | `BorderType::Double` for clear pane separation |

### opencode-style box form (session 2026-07-23)

Derived from analyzing `sst/opencode` `packages/tui/src/` (see §8 for the full
study + what was blocked). Applied the box-panel aesthetic to orcatui's chrome:

| Feature | Module | Details |
|---------|--------|---------|
| **Unified `AgentStatus`** | `agent.rs` | Orca 4-state model (`working/blocked/waiting/done/failed/idle`) — single `derive(state, osc_state)` reused by sidebar + (future) auto-scroll/jump |
| **opencode theme palette** | `config.rs` | 3-level bg (`background`/`background_panel`/`background_element`) + `border`/`border_active` + `text_muted` (GitHub-dark) |
| **Bordered sidebar panel box** | `sidebar.rs` | `Borders::ALL` + `theme.border()` + `theme.panel()` fill + `orcatui` title (opencode `backgroundPanel` aesthetic) |
| **2-line sidebar entries** | `sidebar.rs` | on wide sidebars (≥36 cols): `name + model/branch` / `tool: input`; 1-line fallback below |
| **Pinned section** | `sidebar.rs` + `app.rs` | `PINNED (n)` section above `IN PROGRESS`; `pinned: Vec<bool>`, Pane-mode `p` toggles focused agent |
| **Themed footer strip** | `app.rs` | footer on `theme.panel()` bg with `accent`/`muted` fg (was hardcoded DarkGray) |

> **ppalla 0.0.3 adoption:** pane borders use `PreparedBlock` (cached glyph
> placement; focus-color toggles never invalidate the cache). `PreparedBuffer`
> was tried for dirty-only repaint but **reverted** (incompatible with ratatui's
> double-buffered `Terminal` — caught by a rigorous render test). The sidebar box
> still uses ratatui `Block`. See §2 below.

### ppalla 0.0.3 adoption (session 2026-07-23)

Bumped `ratatui-ppalla` **0.0.2 → 0.0.3** and adopted its Prepared primitives
where they earn their keep on the hot path:

| Primitive | Applied | Status |
|-----------|---------|--------|
| **`PreparedBlock`** | `Pane` border — cached glyph placement keyed on (w, h, border_type, borders, title); the focus *color* is applied at paint time and excluded from the cache key, so toggling focus is free. Replaces ratatui `Block` for the (up to 20×) pane borders. | ✅ shipped |
| ~~`PreparedBuffer`~~ | `Pane::paint_grid` dirty-only repaint. | ❌ **reverted** — incompatible with ratatui's double-buffered `Terminal`: the back buffer swaps each `flush`, so unrepainted cells carry 2-frame-old content and the diff smears the screen. A rigorous render test (`render_shows_new_content_and_retains_old`) caught it; reverted to a correct full-grid repaint each frame. |
| `PreparedLayout` | — | ⏸️ **poor fit**: `split_panes` is a *multi*-split (1 vertical + N horizontal); the sidebar split needs `.spacing(1)`, which `PreparedLayout` does not support. Only the footer split would fit (negligible). |
| `list` / `PreparedList` | — | ⏸️ **wrong target**: the sidebar is a status panel (2-line entries / status dots / Pinned), not a filterable list. ppalla `List` belongs in the **jump palette** (its natural home) — deferred to that UX task. |
| `text_input` | — | ⏸️ pairs with the jump-palette filter (deferred). |

### Agent rendering compatibility (session 2026-07-23)

A naive PTY + vt100 embedder shows **blank/flickering panes** for sophisticated
TUI agents (opencode/OpenTUI, and any agent using synchronized output). orcatui
now handles the three things they need:

| Layer | Module | What it does |
|-------|--------|--------------|
| **`TERM` injection** | `pty_session.rs` | Child PTY gets `TERM=xterm-256color` + `COLORTERM=truecolor` + `TERM_PROGRAM=orcatui`, so the agent emits sequences the vt100 emulator understands. |
| **Query responder** | `query.rs` | A stateful DFA answers the agent's capability probes — OSC 10/11/12 color, DECRQM private modes, DA1/DA2, DCS terminfo — synthesized from `ThemeConfig`. Without replies a probing agent can't determine the terminal and renders blank. |
| **Synchronized output** | `sync.rs` | Buffers `ESC[?2026h … clear+redraw … ESC[?2026l` batches and flushes them to the emulator **atomically**, so a render that fires mid-batch sees the previous frame, not the half-cleared intermediate (the "blank pane" symptom). **General** — fixes any agent using mode 2026. |

**`orcatui-inject`** (second binary, `src/bin/inject.rs`) — a `record` / `replay`
tool that captures an agent's raw PTY bytes and feeds them through the emulator +
responder + sync batcher at a chosen size (`--resize` reproduces a live
spawn-then-resize). This is what isolated the opencode blank-pane bug
deterministically: it proved the emulator / alt-screen / size / resize / responder
were all fine and pinned the cause on mode 2026. `ORCA_DEBUG_LOG=1` on `orcatui`
logs per-chunk byte/cell counts + resize events to `/tmp/orca-live.log`.

**Test count 183 → 226 (+43: query 11, sync 7, toast 5, orca_daemon 12, render/spawn 2, config 6). Build: 0 errors.**

### Orca daemon client (session 2026-07-24)

orcatui can now connect to a running **Orca GUI daemon** (`--daemon` flag) for
session persistence, multi-client (GUI + TUI), and auto-reconnect.

| Component | Module | What it does |
|-----------|--------|--------------|
| **Daemon protocol** | `orca_daemon.rs` | Binary-frame + NDJSON client: hello handshake, RPC (createOrAttach/write/resize/kill), stream frame reader. 12 tests. |
| **Stream reader** | `app.rs` (thread) | Reads Data/Event frames from the stream socket → routes to `AgentBus` via session-ID→pane map. Disconnect → all panes Exit. |
| **Session lifecycle** | `app.rs` `spawn_one_daemon` | `createOrAttach` RPC instead of local PTY. Snapshot fed to `Pane::feed`. Session map registered. |
| **Reconnection** | `app.rs` `pump_daemon_reconnect` | Exponential backoff (config-driven: initial/max/attempts). Auto-retry each loop tick. Give up → standalone. |
| **Error UI** | `toast.rs` + `ConnectionState` | `● Standalone` / `● Daemon` / `✗ Disconnected` in sidebar. Transient toasts for connect/disconnect/reconnect/fail. |
| **Config** | `config.rs` `DaemonConfig` | `reconnect_initial_secs`, `reconnect_max_secs`, `reconnect_max_attempts`, `rpc_timeout_secs`, `hello_timeout_secs`. |

```toml
[daemon]
reconnect_initial_secs = 3
reconnect_max_secs = 30
reconnect_max_attempts = 0   # 0 = unlimited
rpc_timeout_secs = 10
hello_timeout_secs = 5
```

**Standalone mode is fully preserved** — no daemon → direct PTY, exactly as before.

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
├── config.rs        Config + ThemeConfig (TOML; 3-level bg + border/muted tokens)
├── osc.rs           OscScanner (OSC 9999 DFA) + AgentActivity
├── sidebar.rs       SidebarEntry + render_sidebar (bordered panel box, 2-line + Pinned)
├── pane.rs          Pane (vt100 → ratatui bridge + OSC feed + scroll, themed borders)
├── layout.rs        split_panes (grid)
├── terminal_emu.rs  TerminalEmulator (vt100 wrapper, ratatui-free)
├── pty_session.rs   PtySession (portable-pty spawn + reader thread)
├── bus.rs           AgentBus (tokio mpsc) + forward_session
├── scheduler.rs     FrameScheduler (60fps policy, clock-injectable)
├── agent.rs         AgentKind + AgentState + AgentStatus (4-state) + AgentSpec
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
| 1-pane render | **330 µs** | 2.0% |
| 5-pane render | **818 µs** | 4.9% |
| 10-pane render | **1.25 ms** | 7.5% |
| 20-pane render | **2.16 ms** | 13.0% |

> 20-pane render at 13% of budget → **60 fps for N=20 with 87% headroom**.
> (PreparedBlock cached borders — 37-43% faster than the original ratatui Block.)

---

## 5. Test coverage

```
226 tests · 56% line coverage (tarpaulin)

Well-covered (80-100%):     coordinator 100%, scheduler 100%, ssh 99%,
                            terminal_emu 98%, layout 95%, agent 93%,
                            pty_session 88%, pane 89%, bus 92%
Gaps (TTY/IO-bound):        app.rs main_loop/run (19%), cli.rs handlers (9%),
                            mobile.rs serve (89%), integrations gh (38%)
```

---

## 6. CLI reference

```
orcatui run [--cwd DIR] [--worktree] [--remote HOST] [--reconnect] [--mobile PORT] -- <agent> [:: <agent> ...]
orcatui orchestrate [--spec TEXT | --issues OWNER/NAME] [--parallel]
orcatui prs <owner/name>
orcatui issues <owner/name>
orcatui mobile [--port PORT]
orcatui --version
```

### Key bindings

| Key | Mode | Action |
|-----|------|--------|
| `Ctrl+P` | any | Enter Pane mode |
| `Ctrl+Q` | any | Quit orcatui |
| `Ctrl+N` | any | Spawn a new agent pane (auto-detects an installed agent) |
| `←↑↓→` / `hjkl` | Pane | Grid-aware focus switch |
| `Tab` / `Shift+Tab` | Pane | Sequential focus next/prev |
| `p` | Pane | Pin/unpin focused agent (sidebar "PINNED" section) |
| `Esc` | Pane | Return to Normal (passthrough) |
| Mouse scroll | any | Scroll focused pane scrollback |
| [any key] | Normal | Forwarded to focused agent's PTY |

### Config file (`~/.config/orcatui/config.toml`)

```toml
default_agent = "claude"

[layout]
sidebar_width = 26        # 0 hides the sidebar
show_status_bar = true

[theme]
background = "#0d1117"          # root bg
foreground = "#e6edf3"
accent   = "#58a6ff"
success  = "#3fb950"
warning  = "#d29922"
error    = "#f85149"
# opencode-style box-form tokens (optional; default to GitHub-dark)
background_panel   = "#161b22"  # raised panel/box bg (sidebar, footer)
background_element = "#21262d"  # further-raised (hover, nested)
border      = "#30363d"         # box border color
border_active = "#58a6ff"       # focused box border
text_muted  = "#8b949e"         # secondary text
```

---

## 7. Resolved decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| GUI vs TUI | TUI | WSL has no GPU; Electron too heavy |
| tmux usage | none (direct PTY) | control, performance |
| Language | Rust | performance, safety, ratatui ecosystem |
| Async runtime | tokio | mpsc perf; used as channel only (no runtime in UI loop) |
| ppalla source | crates.io v0.0.3 | reproducible, publishable |
| Terminal emulation | vt100 0.15 (not 0.16) | ratatui 0.29 pins unicode-width 0.2.0; vt100 0.16 needs ^0.2.1 |
| Input model | zellij-style modes (Ctrl+P) | passthrough lets agents use Tab/Esc freely |
| Agent activity | OSC 9999 capture (OscScanner) | same channel Orca uses; data IS in PTY stream |
| Config format | TOML | Rust convention |
| Agent registry | builtin + PATH detection | mirrors Orca "run any CLI agent" |
| git operations | CLI (not git2-rs) | no native dep, dev env has git |

---

## 8. Next steps

### Immediate (layout/UX polish)
- ✅ **Adaptive layout** — `Ctrl+B` toggles the sidebar; auto-hides on narrow terminals
- ✅ **Sidebar scroll** — auto-scrolls to keep the focused agent visible in the list
- ✅ **Jump palette** — `/` in Pane mode opens a fuzzy-filter overlay (type to filter, Enter to focus)
- ⏸️ **Tabs** — deferred: "tab" is undefined (worktree? session? run?) — needs a data-model decision

### Layout study: opencode + Orca GUI source analysis

**opencode** (`github.com/sst/opencode`, `packages/tui/src/`) — a TypeScript/Solid
TUI built on `@opentui` (a Bubble Tea/Ink-style component model). Files analyzed
this session: `app.tsx` (provider stack + route switch), `routes/session/index.tsx`
(89 KB — the main screen layout), `routes/session/sidebar.tsx`, `routes/session/footer.tsx`,
`routes/home.tsx`, plus the `component/`, `theme/`, `routes/session/` trees.

**opencode session layout** (`routes/session/index.tsx`):
```
<box flexDirection="row">                       ← horizontal: [main | sidebar]
  <box paddingLeft=2 paddingRight=2 paddingBottom=1 gap=1>   ← main (has padding!)
    <scrollbox stickyScroll stickyStart="bottom"> ...messages... </scrollbox>
    <box flexShrink=0> <Prompt/> | <PermissionPrompt/> </box>
  </box>
  <Show when=sidebarVisible>                    ← sidebar: wide(>120)=relative, narrow=overlay
    <Sidebar width=42/>                          ← toggle: kv("sidebar","auto"|"hide")
  </Show>
</box>
```
- `theme/index.ts` (28 KB): 3-level bg — `background` / `backgroundPanel` / `backgroundElement`
  — plus `border` / `borderActive`, `text` / `textMuted`, `success/warning/error`.
- `sidebar.tsx`: `backgroundColor={theme.backgroundPanel}`, padding, `scrollbox` with
  title + content + a version footer.
- `footer.tsx`: `justifyContent="space-between"` — left = cwd, right = `• N LSP · ⊙ N MCP · /status`.
- `index.tsx` messages: `border=["left"]` colored accent + `customBorderChars` (SplitBorder)
  + `backgroundPanel` fill.

**Orca GUI** (`github.com/stablyai/orca`, `src/shared/` + `src/renderer/src/components/`):
- `agent-status-types.ts`: status states = `working | blocked | waiting | done` (4 states,
  NOT our Idle/Running/Done/Failed). `agent-status-osc.ts` = OSC 9999 parser (same protocol
  our `OscScanner` captures). Sidebar sections: "Pinned" / "In Progress (N)".

---

#### Session 2026-07-23 — what was APPLIED (opencode → orcatui)

> Goal stated by the user: convert the **text-based layout** into an
> **opencode-style "box form"** (bordered panel boxes with panel backgrounds).
> Sidebar position is **unchanged** (kept on the LEFT — orchestrator convention).

| # | opencode / Orca-GUI pattern | Applied to orcatui | Files |
|---|-----------------------------|---------------------|-------|
| 1 | Orca 4-state `working/blocked/waiting/done` vocabulary | `AgentStatus` enum — single `derive(state, osc_state)` source of truth; sidebar `status_style`/`is_active` delegate to it (glyphs/colors byte-identical) | `agent.rs` (+202), `sidebar.rs` |
| 2 | opencode 3-level bg (`background`/`backgroundPanel`/`backgroundElement`) + `border`/`borderActive` + `textMuted` | 5 new `ThemeConfig` fields + accessors `panel()/element()/border()/border_active()/muted()` (GitHub-dark values) | `config.rs` (+89) |
| 3 | opencode `backgroundPanel` box aesthetic | Sidebar → bordered **panel box**: `Borders::ALL` + `theme.border()` + `theme.panel()` fill + `"orcatui"` title; brand row removed (now the title) | `sidebar.rs` |
| 4 | opencode denser multi-line region content | Sidebar **2-line entries** on wide sidebars (≥36 cols): `name + model/branch` / `tool: input`; 1-line fallback below 36 (existing tests stay green) | `sidebar.rs` |
| 5 | Orca "Pinned" / "In Progress (N)" sections | Sidebar **PINNED section** above IN PROGRESS; `pinned: Vec<bool>` on `App`, Pane-mode `p` toggles the focused agent | `sidebar.rs`, `app.rs` |
| 6 | Consistent box framing across regions | Pane borders themed (`border_active` focused / `border` unfocused); footer → **panel strip** (`theme.panel()` bg + `accent`/`muted` fg). No more hardcoded `LightBlue`/`DarkGray` | `pane.rs`, `app.rs` |

**Test count: 167 → 183 (+16). Build: 0 errors.**

#### BLOCKED / used differently / NOT applied

| Item | Status | Reason |
|------|--------|--------|
| **ppalla box rendering** | ✅ resolved (0.0.3) | ppalla 0.0.2's `style` module does not apply borders (was blocked). **0.0.3 added `PreparedBlock`** (cached border drawing) — orcatui now uses it for pane borders. The sidebar box still uses ratatui `Block` until the sidebar→`List` rewrite. |
| **Pane empty-area bg fill** (action #3) | ⚠️ partial | Panes are **live vt100 terminals** — the inner area IS the emulator cell grid, so there is no "empty area" to fill like a chat bubble. Pane frame bg is `theme.bg()`; uncovered emulator cells stay terminal-transparent (`Color::Reset`). |
| **Sidebar position (opencode = RIGHT)** | ⏭️ intentionally not applied | orcatui keeps the sidebar on the **LEFT** (Orca GUI / Claude Squad convention for multi-agent orchestrators). Box *form* applied; *position* preserved. |
| **Padding / density (opencode paddingLeft/Right=2)** | ⏭️ intentionally not applied | orcatui is deliberately **edge-to-edge** ("꽉찬") for density; box separation comes from panel-bg contrast + borders, not outer padding. |
| **opencode chat-message model** | ➖ N/A | opencode is a **chat interface** (messages + prompt); orcatui panes show **live terminal output**. Box-form applies to the chrome (sidebar/footer/framing), not the pane content. |
| **opencode footer (space-between: cwd \| LSP·MCP·/status)** | 🔜 future | orcatui footer kept as a themed key-hint panel strip. Evolving it into a space-between status bar (cwd \| `●N working · ✗N failed`) is a follow-up. |
| **Jump palette** (action #5, `/`) | ✅ done | `/` in Pane mode opens a centered overlay: type to filter agents by name (case-insensitive substring), ↑↓ to select, Enter to focus. |
| **ppalla adoption (List/viewport/text_input)** | 🔜 opportunity | ppalla's real value here is the **Preparable pattern** (perf caching) + **stateful widgets**, *not* box styling. Candidates: sidebar → ppalla `List` (replace hand-painted windowing), jump palette → `List`+`text_input`, scrollback → `viewport`. Currently orcatui uses ppalla only nominally (referenced in comments). |

#### Action items — status
1. ✅ Map `AgentState` → `working/blocked/waiting/done` → `AgentStatus`
2. ✅ Denser sidebar: 2-line entries on wide terminals
3. ⚠️ Background fill on pane empty areas — partial (panes are live terminals)
4. ✅ Consistent box border on sidebar + panes + footer (themed, not hardcoded)
5. ✅ Jump palette — `/` in Pane mode, fuzzy-filter overlay, Enter to focus
6. ✅ Sidebar "Pinned" section

### Medium
- Linear real implementation (reqwest + GraphQL + LINEAR_API_KEY)
- README rewrite with screenshot + install instructions (cargo install / binstall / brew / binary)
- Release workflow (taiki-e/upload-rust-binary-action for cross-platform binaries)
- Coverage → 80% (TestBackend-driven main_loop tests)

### Research
- ~~opencode TUI source analysis (`packages/tui/src/`)~~ — **done 2026-07-23** (see §8: applied box-form + theme; blocked on ppalla box rendering)
- ~~Orca GUI `AgentStateDot.tsx` + `agent-status-types.ts`~~ — **done** (`AgentStatus` 4-state model shipped)
- vt100 0.16 bump when ratatui relaxes unicode-width pin
- ppalla `style` module — still does not apply borders/padding in 0.0.3 (box framing uses `PreparedBlock` instead); watch for Lipgloss-parity border rendering
- sidebar → ppalla `List` + jump palette (`List` + `text_input`) — the natural home for ppalla's filterable widgets

---

## 9. Resuming in a new session

```bash
cd ~/source/project/orcatui
cargo build          # 0 errors
cargo test           # 226 passed
cargo bench --bench orca   # 3 hot paths
cargo run --example screenshot   # renders a sample frame with sidebar
```

### Related links

- ratatui-ppalla: <https://crates.io/crates/ratatui-ppalla>
- Orca GUI (original): <https://github.com/stablyai/orca>
- opencode (TUI reference): <https://github.com/sst/opencode>
- Claude Squad (reference): <https://github.com/smtg-ai/claude-squad>
- Pretext (inspiration): <https://github.com/chenglou/pretext>
