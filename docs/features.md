> orcatui feature reference · last updated 2026-07-27

# Features

This document describes **what orcatui does** — the user-facing behavior,
grouped by subsystem. It is the counterpart to [`architecture.md`](architecture.md)
(how the internals fit together) and [`../README.md`](../README.md) (install,
quick-start, and the big-picture "why a TUI"). Keybindings here were verified
against `src/app.rs` (`InputMode` enum, `FOOTER_*` consts, `handle_key`); the
README's keybindings table predates the gateway move and is stale.

## 1. Agent lifecycle & status

Each pane's display state is a single `AgentStatus` (`src/agent.rs`) derived
from two signals: the process lifecycle state and the live OSC 9999 activity
payload emitted by the agent.

| Variant | Meaning | OSC state string |
|---------|---------|------------------|
| `Working` | actively producing output | `working` |
| `Blocked` | waiting on user approval / input | `blocked` |
| `Waiting` | paused on an external dependency | `waiting` |
| `Interrupted` | interrupted mid-task | `interrupted` |
| `Done` | finished successfully | `done` |
| `Failed` | crashed / non-zero exit | *(lifecycle only)* |
| `Idle` | spawned but not yet running | *(lifecycle only)* |

Derivation priority (`AgentStatus::derive(lifecycle, osc_state)`): a `Failed`
lifecycle always wins; otherwise a recognized OSC state overrides the
lifecycle; otherwise the lifecycle maps directly (`Running`→`Working`,
`Done`→`Done`, `Idle`→`Idle`). `status_tally(&[AgentStatus]) -> StatusTally`
counts each bucket and feeds the footer/sidebar summaries. The OSC 9999 payload
(`osc.rs` → `AgentActivity`) carries `state`, `toolName` (alias `tool`),
`toolInput`, `prompt`, and `model`. Note: `Interrupted` is **OSC-derived only**;
detecting a live Ctrl+C / signal-driven interruption is a known gap (TODO), not
an implemented feature.

## 2. Sidebar

The left navigational spine (`src/sidebar.rs`): a fully bordered, rounded panel
box whose top border carries the ` orcatui ` brand title. Top to bottom inside
the box: a connection indicator, an optional `▸ PINNED (n)` section, then the
` IN PROGRESS (n) ` section header (`n` = active unpinned agents), then one row
per agent (windowed to the latest when they overflow), then an optional bottom
status-tally summary.

| Indicator | Mode |
|-----------|------|
| `● Standalone` | local PTYs (`orcatui run`) |
| `● Daemon` | connected to a daemon |
| `✗ Disconnected` | daemon dropped (with reason + retry timer) |

Entries expand to **two lines** (name + branch on line 1, `tool: input` summary
on line 2) when the inner sidebar width is ≥ 36 cols; below that the classic
single-line layout is kept. Status dots are colored per status:

| Status | Glyph | Color |
|--------|-------|-------|
| Working | `●` | success (green) |
| Blocked | `●` | error (red) |
| Interrupted | `●` | accent |
| Waiting | `●` | warning (yellow) |
| Done | `✓` | foreground, dimmed |
| Failed | `✗` | error (red) |
| Idle | `○` | foreground, dimmed |

The sidebar auto-scrolls to keep the focused agent visible. The bottom
status-tally summary is height-gated (rendered only when inner height ≥ 8 rows)
and emits one compact colored token per non-zero bucket.

## 3. Modes & keybindings

orcatui uses zellij-style modes. **`Ctrl+Alt+P` is the gateway** to controlling
orcatui; in the default (Normal) mode every other key is forwarded straight to
the focused agent's PTY. The `Alt` modifier adds an ESC-prefix byte so the
chord is byte-distinct from bare `Ctrl+P`, which inner agents like opencode use
for their own command palette.

**Global (any mode):**

| Key | Action |
|-----|--------|
| `Ctrl+Alt+P` | Enter **Pane mode** |
| `Ctrl+Q` | Quit orcatui |

**Normal mode (default) — passthrough:** every key (`Tab`, `Esc`, `Ctrl+C`,
arrows, all typing) goes to the focused agent verbatim. Enter Pane mode first
to drive orcatui itself.

**Pane mode (`Ctrl+Alt+P`):**

| Key | Action |
|-----|--------|
| `h j k l` / `← ↑ ↓ →` | Move focus (grid-aware) |
| `Tab` / `Shift+Tab` | Focus next / previous pane (wraps) |
| `p` | Pin / unpin the focused agent → sidebar PINNED |
| `x` | Close the focused pane (kill agent + drop from grid) |
| `z` | Toggle zoom (focused pane fills the screen) |
| `n` | Open the spawn picker |
| `b` | Toggle the sidebar |
| `s` | Open the sidebar navigation hub |
| `/` | Open the fuzzy-focus jump palette |
| `a` | Open the activity timeline overlay |
| `?` | Toggle the help overlay |
| `Esc` | Return to Normal (passthrough) |

The gateway was recently moved from `Ctrl+P` → `Ctrl+Alt+P` to avoid colliding
with inner agents, and `N` / `B` / `S` were moved from global `Ctrl+` chords
into Pane mode for the same reason. The footer always shows the current mode's
hints. Note: if your terminal has XON/XOFF flow control enabled, `Ctrl+Q` (and
`Ctrl+S`) may be swallowed — run `stty -ixon` to free them.

## 4. Spawn picker & custom command

Press `n` in Pane mode to open a centered spawn picker. The list contains:
`bash` (always available), every agent binary discovered on `PATH`, the
configured `default_agent`, and a trailing **"Custom command…"** sentinel.
`↑`/`↓` selects, `Enter` spawns the selection in a new pane, `Esc` cancels.

Selecting **"Custom command…"** opens a text-entry modal (`InputMode::SpawnCustom`):
type any command, `Enter` shells it out (split on whitespace, no quote handling)
and spawns the result; `Esc` cancels. If the terminal is too small for another
pane, a toast warns *"Terminal too small for another pane"*.

## 5. Agent registry

`AgentKind` (`src/agent.rs`) classifies the binary in `command[0]`. There are
**11 known kinds** (canonical order) plus a `Generic` fallback for anything
else:

| Kind | Display name | Binary |
|------|--------------|--------|
| `ClaudeCode` | Claude Code | `claude` |
| `Codex` | Codex | `codex` |
| `OpenCode` | OpenCode | `opencode` |
| `Gemini` | Gemini | `gemini` |
| `Amp` | Amp | `amp` |
| `Cursor` | Cursor | `cursor` |
| `Aider` | Aider | `aider` |
| `Goose` | Goose | `goose` |
| `Crush` | Crush | `crush` |
| `Cody` | Cody | `cody` |
| `Qwen` | Qwen Code | `qwen` |
| `Generic` | Custom | *(empty)* |

`AgentKind::detect_installed()` scans `$PATH` and returns the subset present on
this machine (this is what populates the spawn picker). `from_binary()` maps a
name or path to a kind, falling back to `Generic`.

## 6. Activity timeline

Press `a` in Pane mode to open a fullscreen overlay (`src/activity.rs`) showing
recent agent events. The log is a bounded ring buffer (`DEFAULT_CAP = 500`;
oldest evicted when full). Events come in three kinds:

- **State** — `{ agent, from, to, at }` — a status transition.
- **Tool** — `{ agent, tool, input, at }` — a tool invocation (from OSC 9999).
- **Error** — `{ agent, message, at }` — an exit/launch failure.

The overlay renders events newest-first, one per line, e.g.
`[12:03:45] claude: working → waiting`. **Any key closes the overlay** and
returns to Normal mode (mirroring the Help overlay's contract).

## 7. Sidebar navigation hub

Press `s` in Pane mode to open the Navigate popup. It lists three items
(`SIDEBAR_NAV_ITEMS`):

| Index | Item | Behavior |
|-------|------|----------|
| 0 | Activity | Opens the activity timeline overlay |
| 1 | Tasks | Toasts *"Tasks — coming soon"* |
| 2 | Settings | Toasts *"Settings — coming soon"* |

`↑`/`↓` moves the selection, `Enter` dispatches, `Esc` returns to Normal.
Only Activity is implemented; Tasks and Settings are placeholders.

## 8. Mouse & clipboard

| Input | Action |
|-------|--------|
| Mouse scroll | Scroll the focused pane's scrollback (1000 lines retained, 3 lines/notch) |
| Left-button down (in a pane) | Focus that pane and begin a text selection |
| Left-button drag | Extend the selection (reverse-video highlight) |
| Left-button up | Copy the selection to the system clipboard, then clear it |

Clipboard copy (`src/clipboard.rs`) is **zero-dependency**: it shells out to
the first available platform tool — `pbcopy` (macOS), `clip` (Windows), or
`xsel` / `xclip` / `wl-copy` (Linux/BSD). On success a *"Copied"* toast
appears; if no tool is found, a warning toast names the missing tools and the
copy is a graceful no-op. Mouse selection works in both `orcatui run` and
`orcatui attach`.

## 9. Daemon modes

orcatui runs in one of three modes; the sidebar indicator reflects the active
one:

| Mode | Command | Agents survive client exit | Extra deps |
|------|---------|:--------------------------:|:----------:|
| Standalone | `orcatui run -- claude` | no | none |
| Built-in daemon | `orcatui daemon` then `orcatui attach` | yes | none |
| Orca GUI daemon | `orcatui run --daemon` | yes | Orca GUI |

In the built-in daemon, the daemon process owns the PTYs and serves TUI clients
over a Unix socket, so closing your terminal leaves the agents running —
re-`attach` from any terminal, even simultaneously. In `--daemon` mode, orcatui
is a client of a running Orca GUI (which owns the PTYs + session state); if no
daemon socket is found it silently falls back to standalone.

## 10. Coordination & integrations

| Subcommand / flag | Behavior |
|-------------------|----------|
| `orcatui orchestrate --spec "..."` | Sequential chain — `plan_chain`, each task depends on the previous |
| `orcatui orchestrate --spec "..." --parallel` | Fan-out — `plan_from_spec`, one root task per line, all at once |
| `orcatui orchestrate --issues OWNER/NAME` | Same, but each open issue becomes a task (via `gh`) |
| `orcatui prs OWNER/NAME` | List pull requests (GitHub, via the `gh` CLI) |
| `orcatui issues OWNER/NAME` | List issues (GitHub, via the `gh` CLI) |
| `--remote HOST` | Run agents over SSH on a remote host |
| `--reconnect` | Exponential-backoff reconnect on SSH drop |
| `--mobile PORT` | Start the mobile-companion WebSocket **server** on `PORT` |

Coordination lives in `src/coordinator.rs` (`plan_chain` for sequential,
`plan_from_spec` for parallel); GitHub access in `src/integrations.rs`; SSH in
`src/ssh.rs`; the mobile server in `src/mobile.rs`.

## 11. Configuration

Zero-config by default — everything has a built-in. Override via
`~/.config/orcatui/config.toml` (or `$XDG_CONFIG_HOME`):

```toml
default_agent = "bash"        # "bash" (always), or "opencode", "claude", …

[layout]
sidebar_width   = 26          # 0 hides the sidebar
show_status_bar = true

[theme]
# 3-level opencode-style box-form background palette (GitHub-dark defaults)
background         = "#0d1117"   # root background
background_panel   = "#161b22"   # raised panel/box bg (sidebar, footer)
background_element = "#21262d"   # further-raised (hover, nested)
border       = "#30363d"
border_active = "#58a6ff"        # focused box border
foreground   = "#e6edf3"
text_muted   = "#8b949e"
accent  = "#58a6ff"
success = "#3fb950"
warning = "#d29922"
error   = "#f85149"

[daemon]
reconnect_initial_secs = 3       # first retry delay
reconnect_max_secs     = 30      # backoff cap
reconnect_max_attempts = 0       # 0 = unlimited
rpc_timeout_secs       = 10
hello_timeout_secs     = 5
```

## 12. Known limitations

- **Hangul / CJK IME.** Committed (precomposed) text forwards to the agent
  correctly. In-progress composition is **not** filtered: crossterm 0.28's
  `KeyEventState` exposes only `KEYPAD` / `CAPS_LOCK` / `NUM_LOCK` / `NONE` —
  there is no `COMPOSING` flag — so buffering jamos in-app risks double-emit or
  cursor desync. A full fix needs a kitty / OSC 51-style preedit protocol
  between the multiplexer and the agent. `src/hangul.rs` ships a *tested*
  `compose_syllable` building block (the Unicode Hangul Composition Algorithm)
  but it is deliberately **not wired** into the live key-forwarding path.
- **Mobile client.** Only the WebSocket **server** (`src/mobile.rs`) is
  implemented; there is no iOS/Android companion app yet.
- **Sidebar-nav placeholders.** The Tasks and Settings entries in the Navigate
  hub (see §7) are "coming soon" stubs, not implemented views.
