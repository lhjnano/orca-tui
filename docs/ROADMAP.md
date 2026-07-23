# Orca TUI — Roadmap & Architecture

Multi-agent coding orchestration in the terminal · Built on
[ratatui-ppalla](https://crates.io/crates/ratatui-ppalla) v0.0.2 ·
Target: **20 agents @ 60 fps @ ≤100 ms response**.

> Migrated & condensed from the original local HTML roadmap
> (`~/documents/2-Projects/orca-tui-roadmap/index.html`, 2026-07-22).

---

## 1. Project overview

Orca TUI is the terminal version of **Orca GUI** ([stablyai/orca](https://github.com/stablyai/orca)).
It runs N coding agents (Claude Code, Codex, OpenCode, …) each in its own git
worktree, in parallel, and monitors/steers them from a single screen.

**Why TUI not GUI?** WSL environments often lack GPU acceleration, so the
Electron-based Orca GUI is heavy and janky. A terminal draws only character
cells — near-zero GPU cost — so we can drive N=20 agents at 60 fps with the
lightness of tmux/zellij plus an orchestration layer on top.

### Orca GUI vs Orca TUI vs Claude Squad vs tmux

| Feature | Orca GUI | Orca TUI (goal) | Claude Squad | tmux |
|---------|----------|-----------------|--------------|------|
| Interface | Electron GUI | TUI (Rust + ratatui) | TUI (Go + bubbletea) | TUI (C) |
| GPU needed | ⚠️ for smoothness | ❌ no | ❌ no | ❌ no |
| tmux dependency | ❌ | ❌ (direct PTY mgmt) | ✅ tmux required | — (it *is* tmux) |
| Multi-agent orchestration | ✅ | ✅ (goal) | ❌ (manual) | ❌ |
| Performance optimization | Electron (heavy) | Preparable pattern (goal) | tmux-based | C (fast) |
| SSH remote worktrees | ✅ | ✅ (goal) | ❌ | ❌ |

---

## 2. Performance goals

| Metric | Target |
|--------|--------|
| Concurrent agents | **20** |
| Sustained frame rate | **60 fps** |
| Frame budget | **16.67 ms** |
| End-to-end response | **≤100 ms** |
| Frame drops (goal) | **0** |

**Frame-budget breakdown (16.67 ms):**

```
AgentBus drain (MPSC batch)              ~1 ms
Agent output ingest (incremental prepare) ~2 ms
Visible-line layout (PreparedText math)   ~4 ms   ★ hot path
Cell diff + escape-code emit              ~5 ms   ★ damage tracking
Terminal I/O                              ~3 ms
Safety margin                             ~1.67 ms
─────────────────────────────────────────────────
                                         16.67 ms ✅
```

**Measured (ratatui-ppalla v0.0.2 benchmarks):**

| Benchmark | Result | % of 16.67 ms budget |
|-----------|--------|----------------------|
| `paint_text/80x24` (single pane paint) | **23.2 µs** | 0.14% |
| `layout_paint_20panes/80x24` (20 panes, layout+paint each) | **6.65 ms** | **39.9%** |

Conclusion: the ppalla layer (layout+paint) for 20 panes consumes ~40% of the
budget, leaving ~10 ms for AgentBus drain (~1 ms) + terminal I/O (~3 ms) +
terminal emulation + margin. **60 fps is technically achievable.** The bottleneck
will be terminal emulation (vt100 parsing + state machine) and AgentBus batch
drain — i.e. the quality of Orca's own code, not ppalla.

---

## 3. Architecture

### Full pipeline

```
┌─────────────────────────────────────────────────────────┐
│  N Agent Processes (Claude Code, Codex, OpenCode, ...)  │
│  each in its own PTY + git worktree                     │
└──────────┬──────────────────────────┬───────────────────┘
           │ PTY byte stream          │ Agent state events
           ▼                          ▼
┌──────────────────────────────────────────────────────────┐
│  vt100 terminal emulation (ANSI escape → Cell grid)      │
│  independent terminal state per agent                    │
└──────────┬───────────────────────────────────────────────┘
           │ ParsedOutput { pane_id, cells, cursor }
           ▼
┌──────────────────────────────────────────────────────────┐
│  AgentBus (tokio MPSC, N→1)                              │
│  lock-free, batch-drained per frame                      │
└──────────┬───────────────────────────────────────────────┘
           │ Vec<AgentUpdate> per frame
           ▼
┌──────────────────────────────────────────────────────────┐
│  FrameScheduler (16 ms budget)                           │
│  backlog > 1 frame → frame skip (backpressure)           │
│  idle → expand poll interval (CPU saving)                │
└──────────┬───────────────────────────────────────────────┘
           ▼
┌──────────────────────────────────────────────────────────┐
│  ratatui-ppalla (Preparable pattern)                     │
│  PreparedText (per pane) · PreparedBuffer (damage) ·     │
│  PreparedLayout (pane split)                             │
└──────────┬───────────────────────────────────────────────┘
           ▼
┌──────────────────────────────────────────────────────────┐
│  Terminal (crossterm escape codes → stdout)              │
└──────────────────────────────────────────────────────────┘
```

### Terminal-emulation layer — Orca's core implementation challenge

ratatui-ppalla and ratatui provide *output primitives*, but **terminal
emulation** (ingesting another program's PTY output into a cell grid) is not
their job. This is the biggest piece Orca TUI must build itself.

```
agent process (Claude Code)
  │ PTY byte stream (incl. ANSI escapes)
  ▼
[portable-pty]   PTY spawn + blocking read        ← Orca code
  ▼
[vt100]          ANSI parse + virtual terminal     ← Orca code (★ core)
  │ cell grid (styled) + cursor + scrollback
  ▼
[ratatui]        copy cell grid into Buffer        ← library from here
  ▼
[crossterm]      escape sequences → terminal
```

**Key finding:** `vt100` exposes `Screen::contents_diff(&previous)`, which
returns the diff between two screens as escape sequences. Terminal emulation +
double-buffer diff + escape emit are therefore handled by the crate, so Orca
does not need to implement diff/emit separately — only the vt100-cell →
ratatui-Buffer bridge.

The libraries are battle-tested in real high-performance terminals:
- `portable-pty` — wezterm's PTY layer
- `vt100` — vte-based emulator used by zellij

> **Crate-version note:** the original roadmap left "vt100 vs termwiz vs
> hand-rolled" open pending a benchmark, and this project's manifest declared
> `vt100 = "0.16"`. In practice `ratatui 0.29.0` (forced by ratatui-ppalla 0.0.2
> → `ratatui ^0.29`) exact-pins `unicode-width =0.2.0`, while every `vt100
> 0.16.x` requires `unicode-width ^0.2.1` — those cannot coexist. orca-tui
> therefore pins **`vt100 = "0.15"`** (uses `unicode-width ^0.1.x`, a separate
> major that coexists). The vt100 API orca-tui depends on is unchanged between
> 0.15 and 0.16. Bump to 0.16 once ratatui relaxes the pin; the benchmark
> (vt100 vs termwiz, N=20 memory/throughput) can then be re-run.

### Core components

| Component | Responsibility | ppalla? | Impl order |
|-----------|----------------|---------|------------|
| `AgentBus` | N agents → 1 UI-thread event funnel (tokio MPSC) | ❌ Orca | 4 |
| `FrameScheduler` | 16 ms budget, batch drain, frame skip, idle poll | ❌ Orca | 5 |
| `Pane` | 1 agent = 1 pane. PreparedText + scroll cursor + state | ✅ PreparedText | 2 |
| `PtyManager` | N PTY spawn, vt100 parse, terminal emulation per pane | ❌ (portable-pty + vt100) | 1 |
| `WorktreeManager` | git worktree create/delete per agent, branch mgmt | ❌ (git2 / CLI) | 6 |
| `Coordinator` | task dispatch, decision gate, worker-done tracking | ❌ Orca | 7 |
| `Layout` | screen split (N panes + sidebar + status bar) | ✅ PreparedLayout | 3 |
| `Terminal I/O` | crossterm escape output, user input | ✅ runtime (EventSource) | 1 |

---

## 4. Feature roadmap

Features are independently developable; recommended in numbered order.

| # | Feature | Difficulty | Est. | Status |
|---|---------|-----------|------|--------|
| **1** | **Single-agent pane** — spawn 1 PTY, render its output live in one pane | medium | 2–3 d | ⏳ starting point |
| **2** | **Multi-pane layout** — split screen into N panes, focus switch, resize, independent scroll | medium | 1–2 d | next |
| **3** | **Agent selection & management UI** — agent picker, supported list, per-agent permissions (yolo/manual/sandboxed) & API keys | medium | 1 d | later |
| **4** | **AgentBus** — async N→1 event funnel (tokio MPSC), backpressure, per-frame batch drain. **Done = N=10 agents, no frame drop, ≤100 ms** | high | 2–3 d | performance-critical |
| **5** | **FrameScheduler** — 16 ms budget, adaptive skip, idle backoff, render priority (focus > changed > all). **Done = N=20 @ 60 fps sustained** | high | 2 d | performance-critical |
| **6** | **Worktree manager** — auto `git worktree add` per agent, branch naming, cleanup, diff viewer, cherry-pick/merge UI | medium | 1–2 d | later |
| **7** | **Coordination layer** — Task/Dispatch/Decision-Gate/Worker-Done/Inbox; `orca orchestrate --spec "…"` auto-distribution | high | 3–5 d | later |
| **8** | **SSH remote worktrees** — run agents on a remote backend, stream pane locally, reconnect on drop | high | 3–4 d | extension |
| **9** | **GitHub / Linear integration** — PR browser, issue→task conversion, CI status in pane headers | medium | 2 d | extension |
| **10** | **Mobile companion** — local WebSocket server + iOS/Android/PWA + QR pairing | high | 1 wk+ | extension |

> **Scope note (Feature 4 vs 5):** Feature 4's "N=10, no frame drop, ≤100 ms"
> criterion implicitly needs Feature 5 (FrameScheduler). Without a scheduler the
> AgentBus task can't meet that bar. Resolution pending: either fold a minimal
> adaptive frame timer into Feature 4, or descope the fps/latency criterion and
> verify N=1..N correctness only.

---

## 5. Resolved decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| GUI vs TUI | TUI | WSL has no GPU; Electron too heavy |
| tmux usage | none (direct PTY mgmt) | control, performance, personal preference |
| Language | Rust | performance, safety, ratatui ecosystem |
| Async runtime | tokio | Rust async de-facto standard, MPSC perf |
| Performance paradigm | Preparable pattern (prepare/layout split) | never recompute unchanged layout |
| Project split | ratatui-ppalla (library) / orca-tui (app) | library is general; app is specialized |
| ppalla source | crates.io v0.0.2 (not path dep) | reproducible, publishable |

## 6. Open questions

| Question | Options | Status |
|----------|---------|--------|
| Terminal-emulation crate | vt100 0.15 (current) vs termwiz vs hand-rolled | **vt100 chosen**; benchmark deferred — re-evaluate if N=20 memory/throughput falls short |
| Backpressure strategy | (a) drop oldest output (b) compress scrollback (c) buffer all (OOM risk) | (a) recommended, user-configurable |
| PreparedText per pane vs shared | (a) independent per pane (b) common pool | (a) recommended — isolation, independent scroll |
| Config format | TOML / YAML / JSON | TOML (Rust convention) |
| Install | `cargo install` / Homebrew / binary download | cargo first, others later |
| Agent registry source | builtin list discovered via PATH vs TOML config file | open (Task 3) |
| Agent keys/permissions storage | ? | open (Task 3) |

---

## 7. Resuming in a new session

1. `cd ~/source/project/orca-tui && cargo build` — confirm it compiles.
2. Read this `docs/ROADMAP.md`.
3. Confirm ratatui-ppalla prerequisites (`PreparedText`/`Buffer`/`Layout`) ship
   in v0.0.2 (they do).
4. Start at **Feature 1**: single-agent pane (`portable-pty` + `vt100`).

### Related links

- ratatui-ppalla: <https://crates.io/crates/ratatui-ppalla>
- Orca GUI (original): <https://github.com/stablyai/orca>
- Claude Squad (reference): <https://github.com/smtg-ai/claude-squad>
- Pretext (inspiration): <https://github.com/chenglou/pretext>
- portable-pty: <https://crates.io/crates/portable-pty>
- vt100: <https://crates.io/crates/vt100>
