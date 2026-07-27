# orcatui — Roadmap

Terminal multi-agent coding orchestration — a TUI port of
[Orca GUI](https://github.com/stablyai/orca). Runs N coding agents side-by-side
in split panes, each in its own PTY / git worktree.

> orcatui roadmap · last updated 2026-07-27 · v0.3.x · 403 tests · CI green

**See also:** [README.md](../README.md) (install / usage) ·
[architecture.md](architecture.md) (internals: data flow, modules, perf) ·
[features.md](features.md) (full feature reference).

---

## Completed

A compact index — each entry links to its detailed write-up. (Phase 1 of the
former `ROADMAP-v2.md` — Interrupted status, granular tallies, activity timeline,
sidebar nav — plus recent UX fixes are all shipped.)

### Core

- **Single-pane PTY → vt100 → ratatui** — one agent = one pane. → [architecture.md](architecture.md#3-module-map)
- **Multi-pane grid layout** — `split_panes`, grid-aware focus. → [architecture.md](architecture.md#3-module-map)
- **AgentBus** — tokio mpsc N→1, batch-drained per frame. → [architecture.md](architecture.md#2-data-flow)
- **FrameScheduler** — 60 fps throttle + frame-skip + idle backoff. → [architecture.md](architecture.md#5-performance)
- **WorktreeManager** — git worktree per agent, `--worktree`. → [features.md](features.md#10-coordination--integrations)
- **Coordination** — `plan_chain` / `plan_from_spec`, `orchestrate` CLI. → [features.md](features.md#10-coordination--integrations)
- **SSH remote** — `--remote`, `--reconnect` backoff. → [features.md](features.md#10-coordination--integrations)
- **GitHub / Linear** — `prs` / `issues` via `gh`. → [features.md](features.md#10-coordination--integrations)
- **Mobile companion** — WebSocket server (`--mobile`). → [features.md](features.md#10-coordination--integrations)

### Agent model & rendering

- **11-agent registry + Custom command** — Claude/Codex/OpenCode/Gemini/Amp/Cursor/Aider/Goose/Crush/Cody/Qwen + Generic + type-any-command. → [features.md](features.md#5-agent-registry)
- **OSC 9999 capture** — `AgentActivity` (state / tool / prompt / model). → [features.md](features.md#1-agent-lifecycle--status)
- **7-state `AgentStatus` + `status_tally`** — Working/Blocked/Waiting/Interrupted/Done/Failed/Idle. → [features.md](features.md#1-agent-lifecycle--status)
- **Rendering compatibility** — TERM injection, query responder, mode-2026 synchronized output (fixes blank-pane for opencode & co.). → [architecture.md](architecture.md#4-agent-rendering-compatibility)
- **`orcatui-inject`** — record/replay debug tool for rendering issues. → [architecture.md](architecture.md#4-agent-rendering-compatibility)

### UI & interaction

- **Sidebar** — bordered panel, PINNED / IN PROGRESS sections, status dots, bottom tally summary. → [features.md](features.md#2-sidebar)
- **Modes + `Ctrl+Alt+P` gateway** — Normal (passthrough) / Pane / Jump / Spawn / SpawnCustom / Activity / Sidebar. → [features.md](features.md#3-modes--keybindings)
- **Spawn picker** — `n` in Pane mode; agent list + Custom command modal. → [features.md](features.md#4-spawn-picker--custom-command)
- **Activity timeline** — `a` in Pane mode; fullscreen event log (cap 500). → [features.md](features.md#6-activity-timeline)
- **Sidebar nav hub** — `s` in Pane mode; Activity / Tasks / Settings. → [features.md](features.md#7-sidebar-navigation-hub)
- **Mouse + clipboard** — scroll scrollback; drag-select → copy via shell-out (zero-dep). → [features.md](features.md#8-mouse--clipboard)

### Daemon & orchestration

- **Three daemon modes** — Standalone (`run`), built-in daemon (`daemon` + `attach`), Orca GUI client (`--daemon`). → [features.md](features.md#9-daemon-modes)

---

## Future roadmap

Condensed from the former `ROADMAP-v2.md` (merged here 2026-07-27).

### Phase 2 — Integration views (2-3 weeks)
*Goal: lift CLI-only features into interactive overlays.*
- **Tasks view** — GitHub/GitLab issues + PR browser; select an item → dispatch an agent with the issue body as the prompt.
- **Settings overlay** — live theme / layout / agent toggles, persisted to `config.toml`.
- **Agent Dashboard** — 3-bucket board (needs-attention / working / done).

### Phase 3 — Agent advances (3-4 weeks)
*Goal: accurate status tracking + session persistence.*
- **Agent hook system** — loopback HTTP server; `PreToolUse` / `PostToolUse` / `Stop` events → precise status (also closes the lifecycle-derived `Interrupted` gap).
- **Agent hibernation** — auto-close idle/done agents to save memory (matters at N=20).
- **Sleeping sessions + `--resume`** — persist session metadata to disk; restore on daemon restart.
- **Keep-awake** — `caffeinate` / `systemd-inhibit` while agents are working.

### Phase 4 — Automation & workflow (4-6 weeks)
*Goal: unattended agent operations.*
- **Automation scheduler** — RRULE / cron schedules, precheck, headless dispatch, usage/cost tracking.
- **Orchestration UI** — interactive spec → dependency graph → dispatch; agent-to-agent messaging.
- **Profile switching** — multiple config profiles, quick-switch from the sidebar.

### Phase 5 — Optional extensions (as needed)
- Multi-integrations (Linear / Jira direct APIs).
- Worktree lineage (parent / child).
- External worktree inbox (discover worktrees created outside orcatui).
- i18n (ko / ja / zh).
- Mobile QR pairing.

---

## Out of scope for a TUI

Orca-GUI features that don't fit a terminal character grid:

- **Kanban board** — drag-and-drop isn't expressible in a TUI.
- **Mobile device emulator** — requires a video stream.
- **Rich markdown rendering** — limited in the terminal.
- **Dock badge** — desktop-only.
- **WebGL terminal** — needs a GPU.

---

## Milestones

| Phase | Timeframe | Headline deliverable | Effect |
|-------|-----------|----------------------|--------|
| ✅ Done (v0.3.x) | — | Core 10 features + Phase 1 (status, tallies, activity, sidebar nav) + UX fixes | Solid multi-agent TUI |
| 2 | 2-3 wk | Tasks view + Settings overlay + Dashboard | CLI → interactive |
| 3 | 3-4 wk | Hook system + hibernation + session persistence + keep-awake | Precise status + memory efficiency |
| 4 | 4-6 wk | Automation + orchestration UI + profiles | Unattended operations |
| 5 | as needed | Multi-integrations + lineage + i18n | Extensibility |

> *`ROADMAP-v2.md` was merged into this file and removed (2026-07-27).*
