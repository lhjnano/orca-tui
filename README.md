# 🐋 orca-tui

**Terminal multi-agent coding orchestrator** — a TUI port of
[Orca GUI](https://github.com/stablyai/orca). Run N coding agents
(Claude Code, Codex, OpenCode, Gemini CLI, …) each in its own git worktree,
side-by-side in split terminal panes, monitored and steered from one screen.

> **Status:** scaffold (Task 1). The project compiles and the CLI parses, but no
> PTYs are spawned yet. Features 1–4 (single-pane → multi-pane → AgentBus) are in
> progress. See [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Why a TUI?

Orca GUI is an Electron app. On WSL / headless / no-GPU machines that stack is
heavy and janky. A terminal draws character cells, so the GPU cost is ~0 and we
can hold **20 agents at 60 fps with ≤100 ms end-to-end response** — tmux/zellij
lightness, with an orchestration layer on top.

## Relationship to ratatui-ppalla

[`ratatui-ppalla`](https://crates.io/crates/ratatui-ppalla) is a standalone,
high-performance TUI library built on ratatui. It implements the *Preparable
pattern* (`PreparedText` / `PreparedBuffer` / `PreparedLayout`) which splits the
expensive "what to draw where" computation from the cheap "emit changed cells"
step — the key to caching layout work across frames.

orca-tui consumes **ppalla v0.0.2 from crates.io** as an ordinary dependency
(not a path/local dep). Orca-specific logic (AgentBus, PTY management, terminal
emulation, worktrees) lives only in this repo; ppalla never depends on Orca.

## Install & run

```bash
cargo install orca-tui          # once published
# or, from source:
cargo run -q -- run -- claude    # launch Claude Code in a (stub) pane
```

Examples:

```bash
orca-tui run -- claude                       # default cwd (.)
orca-tui run --cwd ../repo -- codex          # specify a working dir
orca-tui run --worktree ../wt -- claude      # --worktree is reserved (prints TODO)
orca-tui --version
```

The agent invocation is captured verbatim after `--`, so flags are forwarded to
the agent: `orca-tui run -- claude --dangerously-skip-permissions`.

## The agent concept

Each pane runs one agent process behind its own PTY + (eventually) its own git
worktree. You get a live, side-by-side view of parallel coding sessions and can
focus, scroll, resize and steer each independently. The planned agent list:
Claude Code, Codex, OpenCode, Gemini CLI, Amp, Cursor CLI, … (Task 3 adds the
picker UI).

## Tech stack

| Area | Crate | Purpose |
|------|-------|---------|
| TUI rendering | `ratatui` + `ratatui-ppalla` 0.0.2 | widgets, Preparable pattern |
| Terminal backend | `crossterm` 0.28 | raw mode, alt screen, events |
| PTY management | `portable-pty` 0.9 | spawn agent processes |
| Terminal emulation | `vt100` 0.15 | ANSI parse → Cell grid + diff |
| Async runtime | `tokio` (full) | AgentBus MPSC, timers |
| CLI | `clap` 4 (derive) | argument parsing |
| Config | `serde` + `toml` | settings (Task 3+) |
| Errors | `anyhow` | ergonomics |

> **Note on `vt100`:** pinned to 0.15, not 0.16. `ratatui 0.29.0` (forced by
> ppalla 0.0.2 → `ratatui ^0.29`) exact-pins `unicode-width =0.2.0`, while every
> `vt100 0.16.x` requires `unicode-width ^0.2.1` — those cannot coexist.
> `vt100 0.15.x` uses `unicode-width ^0.1.x` (a separate major) and coexists
> cleanly. The API orca-tui relies on is unchanged between the two. Bump to 0.16
> once ratatui relaxes the pin.

## License

MIT OR Apache-2.0.
