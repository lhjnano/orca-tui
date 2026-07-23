//! # CLI
//!
//! Argument parsing and subcommand dispatch for the `orca-tui` binary. Kept in
//! the library (not `main.rs`) so the dispatch logic is unit-testable and the
//! binary stays a one-line entry point.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::agent::{AgentKind, AgentSpec};
use crate::app::App;
use crate::integrations::{self, RepoRef};
use crate::ssh::SshTarget;
use crate::{coordinator, mobile};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the CLI: parse argv and dispatch to the selected subcommand. Returns an
/// error to be surfaced by the binary's `main` (which maps it to a non-zero
/// exit code).
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    try_main(cli)
}

#[derive(Parser, Debug)]
#[command(
    name = "orca-tui",
    version = VERSION,
    about = "Terminal multi-agent coding orchestrator (TUI port of Orca GUI)"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run one or more agents in panes.
    ///
    /// The trailing command list is split into per-agent commands on the
    /// literal `::` separator, and each command gets its own pane, e.g.
    ///
    /// - `orca-tui run -- claude codex opencode` → three side-by-side panes.
    /// - `orca-tui run -- claude :: codex --model x :: opencode` → three panes;
    ///   the middle agent's command is `codex --model x`.
    ///
    /// **Splitting rule:** if any `::` is present the list is split into the
    /// segments between `::` tokens (empty segments from leading/trailing/
    /// doubled `::` are dropped); if no `::` is present each token is its own
    /// agent (backward compatible). A stray `::` therefore changes semantics.
    Run {
        /// Working directory shared by every agent. Defaults to the current
        /// directory.
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,

        /// Give each agent its own isolated git worktree. Requires `cwd` (or
        /// the current directory) to be inside a git repository; worktrees are
        /// created under `.orca-worktrees/` and removed when the app exits.
        #[arg(long)]
        worktree: bool,

        /// Run each agent on a REMOTE host over SSH (Feature 8). The host spec
        /// is `user@host`, `host`, or `user@host:port`; each agent command is
        /// wrapped as `ssh <opts> <host> <command...>`.
        #[arg(long, value_name = "HOST")]
        remote: Option<String>,

        /// Start the mobile-companion WebSocket server (Feature 10) alongside
        /// the agents, broadcasting live pane status to a phone/PWA. The URL
        /// and one-time token are printed at startup.
        #[arg(long, value_name = "PORT")]
        mobile: Option<u16>,

        /// One or more agent invocations. Each command (separated by `::`)
        /// becomes its own pane. Without `::`, each token is its own agent.
        /// Everything after `--` is captured verbatim, including flags
        /// intended for the agent itself.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            num_args = 1..,
            value_name = "COMMAND",
        )]
        command: Vec<String>,
    },

    /// Plan multi-agent orchestration from a spec (Feature 7).
    ///
    /// Splits the spec into tasks (one per non-empty line) and dispatches one
    /// agent pane per task, passing the task description as the agent's prompt.
    Orchestrate {
        /// Newline-separated task spec. Use a quoted multi-line string.
        #[arg(long)]
        spec: String,
    },

    /// List open pull requests for a GitHub repo via `gh` (Feature 9).
    Prs {
        /// `owner/name` GitHub repository.
        repo: String,
    },

    /// List open issues for a GitHub repo via `gh` (Feature 9).
    Issues {
        /// `owner/name` GitHub repository.
        repo: String,
    },

    /// Start the mobile-companion WebSocket server (Feature 10).
    ///
    /// Binds a local WebSocket server a phone/PWA can connect to. Prints the
    /// URL and a one-time pairing token. For live snapshots while agents run,
    /// prefer `run --mobile <PORT>`.
    Mobile {
        /// Port to bind. Defaults to 0 (OS-assigned).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}

fn try_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run {
            cwd,
            worktree,
            remote,
            mobile,
            command,
        } => {
            // In worktree-isolation mode `cwd` must resolve to a git repo; if
            // the caller didn't pass --cwd, default to the current directory so
            // the repo can be discovered.
            let cwd = if worktree && cwd.is_none() {
                Some(std::env::current_dir()?)
            } else {
                cwd
            };

            // `trailing_var_arg` does not reliably enforce a minimum, so guard
            // explicitly: at least one non-empty agent command is required.
            let commands = split_agents(command);
            if commands.is_empty() {
                anyhow::bail!(
                    "no agent command given — usage: \
                     orca-tui run [--cwd <DIR>] [--worktree] [--remote <HOST>] -- <command>...  \
                     (separate per-agent commands with '::')"
                );
            }

            // Each command vector is its own agent (its own pane + PTY). To
            // pass extra args to a single agent, either group tokens with `::`
            // or wrap them in `sh -c`.
            let mut specs: Vec<AgentSpec> =
                commands.into_iter().map(AgentSpec::from_command).collect();

            // Feature 8: wrap each agent command for remote execution over SSH.
            if let Some(host) = &remote {
                let target = SshTarget::parse(host)
                    .with_context(|| format!("parsing --remote host {host:?}"))?;
                for spec in &mut specs {
                    spec.command = target
                        .clone()
                        .with_command(spec.command.clone())
                        .command_vec();
                }
            }

            let mut app = App::spawn_agents(specs, cwd.as_deref(), worktree)?;

            // Feature 10: optionally start the mobile-companion WebSocket
            // server on a dedicated tokio runtime thread and feed it live
            // snapshots from the app loop. When `app` drops (run returns) the
            // sender drops too, so `serve` returns and the thread exits.
            if let Some(port) = mobile {
                let token = mobile::random_token();
                let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
                let (snap_tx, snap_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Vec<mobile::AgentSnapshot>>();
                let token_for_server = token.clone();
                let addr_for_server = addr;
                std::thread::Builder::new()
                    .name("orca-mobile-ws".into())
                    .spawn(move || {
                        let Ok(runtime) = tokio::runtime::Runtime::new() else {
                            return;
                        };
                        let _ = runtime.block_on(mobile::serve(
                            addr_for_server,
                            token_for_server,
                            snap_rx,
                        ));
                    })?;
                eprintln!(
                    "orca-tui: mobile companion — ws://{addr}?token={token} \
                     (live pane status while the agents run)"
                );
                app.set_snapshot_sender(snap_tx);
            }

            app.run()?;
            Ok(())
        }

        Command::Orchestrate { spec } => {
            // Feature 7 — LIVE DISPATCH: plan the spec into tasks, then spawn
            // one agent pane per task, passing the task description as the
            // agent's prompt argument. Dependency-gated / sequential dispatch
            // is a future refinement; v1 fans every task out in parallel.
            let detected = AgentKind::detect_installed();
            let agent_bin = detected
                .first()
                .map(AgentKind::binary)
                .unwrap_or("claude")
                .to_string();

            let coord = coordinator::plan_from_spec(&spec, &[agent_bin.clone()]);
            println!(
                "orca-tui: dispatching {} task(s) to agent `{agent_bin}` \
                 (one pane each, task spec passed as the prompt):",
                coord.tasks().len()
            );
            for task in coord.tasks() {
                println!("  [{}] {}", task.id, task.spec);
            }
            println!();

            // Build one AgentSpec per planned task: `[<agent> "<task spec>"]`.
            let specs: Vec<AgentSpec> = coord
                .tasks()
                .iter()
                .map(|t| {
                    let mut s = AgentSpec::from_command(vec![agent_bin.clone(), t.spec.clone()]);
                    s.name = format!("task-{}", t.id);
                    s
                })
                .collect();

            let mut app = App::spawn_agents(specs, None, false)?;
            app.run()?;
            Ok(())
        }

        Command::Prs { repo } => {
            let repo = RepoRef::parse(&repo)?;
            let prs = integrations::list_pull_requests(&repo)?;
            if prs.is_empty() {
                println!("orca-tui: no open pull requests for {repo}");
            }
            for pr in prs {
                match &pr.branch {
                    Some(b) => println!("#{}  {}  ({})", pr.number, pr.title, b),
                    None => println!("#{}  {}", pr.number, pr.title),
                }
            }
            Ok(())
        }

        Command::Issues { repo } => {
            let repo = RepoRef::parse(&repo)?;
            let issues = integrations::list_issues(&repo)?;
            if issues.is_empty() {
                println!("orca-tui: no open issues for {repo}");
            }
            for iss in issues {
                println!("#{}  {}", iss.number, iss.title);
            }
            Ok(())
        }

        Command::Mobile { port } => {
            let token = mobile::random_token();
            let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
            // No live App snapshot feed here; serve with an empty channel so
            // the server is up and accepting connections (clients hold socket).
            let (_snapshot_tx, snapshot_rx) =
                tokio::sync::mpsc::unbounded_channel::<Vec<mobile::AgentSnapshot>>();
            println!("orca-tui: mobile companion server");
            println!("  listen: ws://{addr}");
            println!("  token: {token}");
            println!("  (connect a mobile client to ws://{addr}?token={token})");
            println!("  Ctrl+C to stop.");
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(mobile::serve(addr, token, snapshot_rx))?;
            Ok(())
        }
    }
}

/// Split the trailing command list into per-agent command vectors on the
/// literal `::` separator.
///
/// - **No `::` present:** each token becomes its own one-element agent vector
///   (the original behavior, preserved for backward compatibility — e.g.
///   `claude codex` → two agents).
/// - **At least one `::` present:** the list is split into the segments
///   between `::` tokens; empty segments (from a leading, trailing, or doubled
///   `::`) are dropped.
///
/// A single consistent rule: a stray `::` switches from "one agent per token"
/// to "segment" mode. Documented in the `Run` subcommand help.
pub(crate) fn split_agents(args: Vec<String>) -> Vec<Vec<String>> {
    // No separator at all → backward-compatible one-agent-per-token.
    if !args.iter().any(|a| a == "::") {
        return args.into_iter().map(|t| vec![t]).collect();
    }

    // Split into segments between `::` tokens.
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    for tok in args {
        if tok == "::" {
            segments.push(Vec::new());
        } else {
            segments
                .last_mut()
                .expect("segments starts with one element")
                .push(tok);
        }
    }

    // Drop empty segments (leading / trailing / doubled `::`).
    segments.into_iter().filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_agents_no_separator_is_one_per_token() {
        let got = split_agents(vec!["claude".into(), "codex".into(), "opencode".into()]);
        assert_eq!(
            got,
            vec![vec!["claude"], vec!["codex"], vec!["opencode"]],
            "no '::' → one agent per token (backward compatible)"
        );
    }

    #[test]
    fn split_agents_with_separator_is_segments() {
        let got = split_agents(vec![
            "claude".into(),
            "::".into(),
            "codex".into(),
            "--model".into(),
            "x".into(),
            "::".into(),
            "opencode".into(),
        ]);
        assert_eq!(
            got,
            vec![
                vec!["claude"],
                vec!["codex", "--model", "x"],
                vec!["opencode"]
            ],
            "'::' groups tokens into one agent"
        );
    }

    #[test]
    fn split_agents_drops_empty_segments() {
        let got = split_agents(vec![
            "::".into(),
            "echo".into(),
            "::".into(),
            "::".into(),
            "true".into(),
            "::".into(),
        ]);
        assert_eq!(got, vec![vec!["echo"], vec!["true"]]);
    }

    #[test]
    fn split_agents_single_token_no_separator() {
        let got = split_agents(vec!["claude".into()]);
        assert_eq!(got, vec![vec!["claude"]]);
    }

    #[test]
    fn split_agents_empty_input_yields_nothing() {
        assert!(split_agents(Vec::new()).is_empty());
    }

    #[test]
    fn split_agents_only_separators_yields_nothing() {
        let got = split_agents(vec!["::".into(), "::".into(), "::".into()]);
        assert!(got.is_empty(), "all-empty segments must be dropped");
    }

    /// End-to-end check of the spec-building path: splitting + the empty guard.
    #[test]
    fn smoke_parse_three_agents_via_separator() {
        let commands = split_agents(vec![
            "echo".into(),
            "::".into(),
            "true".into(),
            "::".into(),
            "false".into(),
        ]);
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], vec!["echo"]);
        assert_eq!(commands[1], vec!["true"]);
        assert_eq!(commands[2], vec!["false"]);

        let specs: Vec<AgentSpec> = commands.into_iter().map(AgentSpec::from_command).collect();
        assert_eq!(specs.len(), 3);
        for s in &specs {
            assert_eq!(s.kind, crate::agent::AgentKind::Generic);
        }
    }
}
