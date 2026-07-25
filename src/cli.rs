//! # CLI
//!
//! Argument parsing and subcommand dispatch for the `orcatui` binary. Kept in
//! the library (not `main.rs`) so the dispatch logic is unit-testable and the
//! binary stays a one-line entry point.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    name = "orcatui",
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
    /// - `orcatui run -- claude codex opencode` → three side-by-side panes.
    /// - `orcatui run -- claude :: codex --model x :: opencode` → three panes;
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

        /// Try to connect to a running Orca GUI daemon for session
        /// persistence + multi-client (GUI + TUI). Falls back to standalone
        /// (direct PTY) if no daemon is found or the connection fails.
        #[arg(long)]
        daemon: bool,

        /// Run each agent on a REMOTE host over SSH (Feature 8). The host spec
        /// is `user@host`, `host`, or `user@host:port`; each agent command is
        /// wrapped as `ssh <opts> <host> <command...>`.
        #[arg(long, value_name = "HOST")]
        remote: Option<String>,

        /// With `--remote`: auto-reconnect a dropped remote session on the same
        /// pane after an exponential backoff, up to a few attempts (Feature 8).
        #[arg(long)]
        reconnect: bool,

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
    /// Splits the spec into tasks (one per non-empty line) and dispatches them
    /// to agents dependency-gated: by default tasks run **sequentially** (each
    /// depends on the previous), so only one is in flight at a time; pass
    /// `--parallel` to fan every task out at once. Each task's description is
    /// passed to the agent as its prompt argument.
    Orchestrate {
        /// Newline-separated task spec. Use a quoted multi-line string. Ignored
        /// when `--issues` is given.
        #[arg(long)]
        spec: Option<String>,
        /// Fan every task out in parallel instead of running them sequentially.
        #[arg(long)]
        parallel: bool,
        /// Source tasks from a GitHub repo's open issues via `gh`
        /// (`owner/name`); each issue becomes one task (Feature 9). Overrides
        /// `--spec`.
        #[arg(long, value_name = "REPO")]
        issues: Option<String>,
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

    /// Start the built-in daemon server (run as a systemd/supervisor service).
    ///
    /// Owns agent PTYs and serves `orcatui attach` clients over a Unix socket.
    /// Agents survive client disconnect — the daemon keeps running until all
    /// agents exit and no clients remain, or until SIGTERM.
    ///
    /// Designed for `systemctl --user start orcatui` or equivalent. Logs to
    /// stdout/stderr (captured by journald/supervisor).
    Daemon {
        /// Unix socket path. Defaults to `$XDG_RUNTIME_DIR/orcatui.sock`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// Agent commands (same `::` separator as `run`). If omitted, the
        /// daemon starts empty and clients create sessions via the protocol.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },

    /// Attach to a running orcatui daemon as a TUI client.
    ///
    /// Connects to the daemon's Unix socket, renders all live agent panes,
    /// and forwards keyboard input. Multiple clients can attach simultaneously.
    /// Detaching (Ctrl+Q) does NOT kill the agents — they keep running in the
    /// daemon.
    Attach {
        /// Unix socket path. Defaults to `$XDG_RUNTIME_DIR/orcatui.sock`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

fn try_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run {
            cwd,
            worktree,
            daemon,
            remote,
            reconnect,
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
                     orcatui run [--cwd <DIR>] [--worktree] [--remote <HOST>] -- <command>...  \
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

            // Try to connect to an Orca GUI daemon (--daemon). Falls back to
            // standalone silently if no daemon is found; shows a toast if a
            // daemon was found but the connection failed.
            if daemon {
                app.try_connect_daemon();
            }

            // Feature 8: mark every pane reconnect-eligible so a dropped remote
            // session is re-spawned after a backoff (harmless without --remote).
            if reconnect {
                app.enable_reconnect();
            }

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
                    "orcatui: mobile companion — ws://{addr}?token={token} \
                     (live pane status while the agents run)"
                );
                app.set_snapshot_sender(snap_tx);
            }

            app.run()?;
            Ok(())
        }

        Command::Orchestrate {
            spec,
            parallel,
            issues,
        } => {
            // Feature 7 + 9 — dependency-gated LIVE DISPATCH. Tasks come either
            // from `--spec` (free-form lines) or `--issues <owner/name>` (each
            // open GitHub issue → one task, via `gh`). Then a Coordinator drives
            // the App: sequential chain by default, --parallel fans out.
            let detected = AgentKind::detect_installed();
            let agent_bin = detected
                .first()
                .map(AgentKind::binary)
                .unwrap_or("claude")
                .to_string();

            // Resolve the task list to a newline spec the planner consumes.
            let task_spec: String = if let Some(repo) = &issues {
                let repo_ref = RepoRef::parse(repo)
                    .with_context(|| format!("parsing --issues repo {repo:?}"))?;
                let issues = integrations::list_issues(&repo_ref)?;
                if issues.is_empty() {
                    anyhow::bail!("no open issues in {repo_ref} to orchestrate");
                }
                issues
                    .iter()
                    .map(integrations::issue_to_prompt)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if let Some(s) = spec {
                s
            } else {
                anyhow::bail!("orchestrate needs --spec <text> or --issues <owner/name>");
            };

            let coord = if parallel {
                coordinator::plan_from_spec(&task_spec, &[agent_bin.clone()])
            } else {
                coordinator::plan_chain(&task_spec, &[agent_bin.clone()])
            };
            let source = if issues.is_some() { "issues" } else { "spec" };
            let mode = if parallel { "parallel" } else { "sequential" };
            println!(
                "orcatui: orchestrating {} task(s) via agent `{agent_bin}` \
                 ({mode}, source: {source}, task text passed as the prompt):",
                coord.tasks().len()
            );
            for task in coord.tasks() {
                println!("  [{}] {}", task.id, task.spec);
            }
            println!();

            // Start empty; the loop's pump spawns tasks as their deps allow.
            let mut app = App::spawn_agents(Vec::new(), None, false)?;
            app.set_orchestration(coord, agent_bin);
            app.run()?;
            Ok(())
        }

        Command::Prs { repo } => {
            let repo = RepoRef::parse(&repo)?;
            let prs = integrations::list_pull_requests(&repo)?;
            if prs.is_empty() {
                println!("orcatui: no open pull requests for {repo}");
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
                println!("orcatui: no open issues for {repo}");
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
            println!("orcatui: mobile companion server");
            println!("  listen: ws://{addr}");
            println!("  token: {token}");
            println!("  (connect a mobile client to ws://{addr}?token={token})");
            println!("  Ctrl+C to stop.");
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(mobile::serve(addr, token, snapshot_rx))?;
            Ok(())
        }

        Command::Daemon { socket, command } => {
            use crate::daemon_server::{default_socket_path, DaemonServer};

            let socket_path = socket.unwrap_or_else(default_socket_path);

            let mut server = DaemonServer::new(&socket_path)?;

            // Parse initial agents (same `::` separator as `run`).
            if !command.is_empty() {
                let commands = split_agents(command);
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                server.spawn_initial(commands, cols.max(20), rows.max(3));
            }

            // Install SIGTERM handler for graceful shutdown.
            let shutdown_flag = server.shutdown_flag();
            install_sigterm_handler(shutdown_flag);

            server.run()?;
            Ok(())
        }

        Command::Attach { socket } => {
            use crate::daemon_server::{default_socket_path, AttachClient};

            let socket_path = socket.unwrap_or_else(default_socket_path);
            run_attach(&socket_path)?;
            Ok(())
        }
    }
}

/// Install a SIGTERM handler that sets the atomic shutdown flag.
fn install_sigterm_handler(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    // Use a raw signal handler via libc (avoids adding signal-hook dep).
    // SAFETY: AtomicBool::store is signal-safe.
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    static mut SHUTDOWN_FLAG: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    unsafe {
        SHUTDOWN_FLAG = Some(flag);
        let handler = sigterm_handler as usize;
        signal(15, handler); // SIGTERM = 15
    }
    extern "C" fn sigterm_handler(_sig: i32) {
        unsafe {
            if let Some(flag) = &SHUTDOWN_FLAG {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Connect to a daemon and run a TUI client loop.
fn run_attach(socket_path: &Path) -> Result<()> {
    use crate::daemon_server::AttachClient;
    use crate::layout::split_panes;
    use crate::pane::Pane;
    use base64::{engine::general_purpose, Engine as _};
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::size as term_size;
    use crossterm::{
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let (mut client, sessions) = AttachClient::connect(socket_path)?;

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (cols, rows) = term_size().unwrap_or((80, 24));
    let mut panes: Vec<Pane> = sessions
        .iter()
        .map(|s| {
            let mut p = Pane::new(s.id, &s.name, cols.max(20), rows.max(3));
            p.set_state(crate::agent::AgentState::Running);
            p
        })
        .collect();

    let mut focus: usize = 0;
    let config = crate::config::Config::default();
    let theme = &config.theme;

    // Reader thread: reads NDJSON from the daemon, feeds output to a channel.
    let reader_stream = client.try_clone_stream()?;
    let (data_tx, data_rx) = mpsc::channel::<(usize, Vec<u8>)>(); // (session_id, bytes)
    let (exit_tx, exit_rx) = mpsc::channel::<(usize, Option<i32>)>(); // (session_id, code)
    std::thread::Builder::new()
        .name("orcatui-attach-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                            let msg_type = msg.get("type").and_then(|v| v.as_str());
                            match msg_type {
                                Some("output") => {
                                    let session = msg["session"].as_u64().unwrap_or(0) as usize;
                                    if let Some(data) = msg["data"].as_str() {
                                        if let Ok(bytes) = general_purpose::STANDARD.decode(data) {
                                            let _ = data_tx.send((session, bytes));
                                        }
                                    }
                                }
                                Some("exit") => {
                                    let session = msg["session"].as_u64().unwrap_or(0) as usize;
                                    let code = msg["code"].as_i64().map(|c| c as i32);
                                    let _ = exit_tx.send((session, code));
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    // Main attach loop.
    let result = (|| -> Result<()> {
        loop {
            // Drain pending output from the daemon.
            while let Ok((session_id, bytes)) = data_rx.try_recv() {
                if let Some(p) = panes.iter_mut().find(|p| p.id() == session_id) {
                    p.feed(&bytes);
                }
            }
            // Drain exit events.
            while let Ok((session_id, code)) = exit_rx.try_recv() {
                if let Some(p) = panes.iter_mut().find(|p| p.id() == session_id) {
                    let state = match code {
                        Some(0) | None => crate::agent::AgentState::Done(code),
                        Some(c) => crate::agent::AgentState::Failed(format!("exit code {c}")),
                    };
                    p.set_state(state);
                }
            }

            // Render.
            terminal.draw(|f| {
                let area = f.area();
                let rects = split_panes(area, panes.len());
                for (i, pane) in panes.iter_mut().enumerate() {
                    let pane_area = rects.get(i).copied().unwrap_or_default();
                    pane.render(f, pane_area, i == focus, theme);
                }
            })?;

            // Poll for input (10ms timeout — keeps the UI responsive to daemon output).
            if event::poll(std::time::Duration::from_millis(10))? {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.kind == KeyEventKind::Press {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('q'), KeyModifiers::CONTROL) => break,
                            (KeyCode::Tab, _) => {
                                if !panes.is_empty() {
                                    focus = (focus + 1) % panes.len();
                                }
                            }
                            (KeyCode::Enter, _) => {
                                if let Some(p) = panes.get(focus) {
                                    let _ = client.write_session(p.id(), b"\r");
                                }
                            }
                            (KeyCode::Backspace, _) => {
                                if let Some(p) = panes.get(focus) {
                                    let _ = client.write_session(p.id(), &[0x7f]);
                                }
                            }
                            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                                if let Some(p) = panes.get(focus) {
                                    let mut buf = [0u8; 4];
                                    let s = c.encode_utf8(&mut buf);
                                    let _ = client.write_session(p.id(), s.as_bytes());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Exit if all panes are terminal.
            if !panes.is_empty()
                && panes.iter().all(|p| {
                    matches!(
                        p.state(),
                        crate::agent::AgentState::Done(_) | crate::agent::AgentState::Failed(_)
                    )
                })
            {
                break;
            }
        }
        Ok(())
    })();

    // Restore terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
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

    // ---- clap argument-parsing coverage (struct fields + dispatch inputs) ----

    #[test]
    fn parse_run_with_all_flags() {
        let cli = Cli::try_parse_from([
            "orcatui",
            "run",
            "--cwd",
            "/tmp",
            "--worktree",
            "--daemon",
            "--remote",
            "user@host",
            "--reconnect",
            "--mobile",
            "8080",
            "--",
            "claude",
        ])
        .expect("valid run invocation parses");
        match cli.command {
            Command::Run {
                cwd,
                worktree,
                daemon,
                remote,
                reconnect,
                mobile,
                command,
            } => {
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/tmp")));
                assert!(worktree, "--worktree parsed");
                assert!(daemon, "--daemon parsed");
                assert!(reconnect, "--reconnect parsed");
                assert_eq!(remote.as_deref(), Some("user@host"));
                assert_eq!(mobile, Some(8080));
                assert_eq!(command, vec!["claude".to_string()]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_run_defaults_when_flags_absent() {
        // Only the required trailing command is given; every flag defaults.
        let cli = Cli::try_parse_from(["orcatui", "run", "claude"]).expect("minimal run parses");
        match cli.command {
            Command::Run {
                cwd,
                worktree,
                daemon,
                remote,
                reconnect,
                mobile,
                command,
            } => {
                assert!(cwd.is_none());
                assert!(!worktree);
                assert!(!daemon);
                assert!(remote.is_none());
                assert!(!reconnect);
                assert!(mobile.is_none());
                assert_eq!(command, vec!["claude".to_string()]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_orchestrate_spec_and_parallel() {
        let cli = Cli::try_parse_from([
            "orcatui",
            "orchestrate",
            "--spec",
            "task one\ntask two",
            "--parallel",
        ])
        .expect("orchestrate --spec --parallel parses");
        match cli.command {
            Command::Orchestrate {
                spec,
                parallel,
                issues,
            } => {
                assert_eq!(spec.as_deref(), Some("task one\ntask two"));
                assert!(parallel);
                assert!(issues.is_none());
            }
            other => panic!("expected Orchestrate, got {other:?}"),
        }
    }

    #[test]
    fn parse_orchestrate_issues_overrides_spec() {
        let cli = Cli::try_parse_from(["orcatui", "orchestrate", "--issues", "owner/name"])
            .expect("orchestrate --issues parses");
        match cli.command {
            Command::Orchestrate {
                spec,
                parallel,
                issues,
            } => {
                assert_eq!(issues.as_deref(), Some("owner/name"));
                assert!(spec.is_none());
                assert!(!parallel, "--parallel defaults to false");
            }
            other => panic!("expected Orchestrate, got {other:?}"),
        }
    }

    #[test]
    fn parse_prs_and_issues_repo() {
        let prs =
            Cli::try_parse_from(["orcatui", "prs", "octocat/hello-world"]).expect("prs parses");
        match prs.command {
            Command::Prs { repo } => assert_eq!(repo, "octocat/hello-world"),
            other => panic!("expected Prs, got {other:?}"),
        }

        let issues = Cli::try_parse_from(["orcatui", "issues", "octocat/hello-world"])
            .expect("issues parses");
        match issues.command {
            Command::Issues { repo } => assert_eq!(repo, "octocat/hello-world"),
            other => panic!("expected Issues, got {other:?}"),
        }
    }

    #[test]
    fn parse_mobile_default_and_custom_port() {
        let default = Cli::try_parse_from(["orcatui", "mobile"]).expect("mobile parses");
        match default.command {
            Command::Mobile { port } => assert_eq!(port, 0, "port defaults to 0"),
            other => panic!("expected Mobile, got {other:?}"),
        }

        let custom =
            Cli::try_parse_from(["orcatui", "mobile", "--port", "9090"]).expect("mobile --port");
        match custom.command {
            Command::Mobile { port } => assert_eq!(port, 9090),
            other => panic!("expected Mobile, got {other:?}"),
        }
    }

    #[test]
    fn version_flag_is_recognized() {
        // clap handles `--version` by yielding a DisplayVersion error (in the
        // real binary it would print + exit). Assert the flag is wired up.
        let err = Cli::try_parse_from(["orcatui", "--version"])
            .expect_err("--version is a display action, not a normal parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn run_captures_separator_tokens_verbatim() {
        // Everything after `--` is captured verbatim, including `::`.
        let cli = Cli::try_parse_from([
            "orcatui", "run", "--", "claude", "::", "codex", "--model", "x", "::", "opencode",
        ])
        .expect("trailing capture parses");
        let command = match cli.command {
            Command::Run { command, .. } => command,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(
            command,
            vec!["claude", "::", "codex", "--model", "x", "::", "opencode",]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            "clap must hand `::` and agent flags through untouched"
        );
        // And the dispatch-time step regroups them into per-agent segments.
        let agents = split_agents(command);
        assert_eq!(
            agents,
            vec![
                vec!["claude"],
                vec!["codex", "--model", "x"],
                vec!["opencode"],
            ]
        );
    }

    #[test]
    fn run_without_command_parses_empty_then_guarded_at_dispatch() {
        // clap's `trailing_var_arg` does NOT enforce a non-empty minimum, so
        // `run` with no trailing command parses successfully to an EMPTY
        // command vec. The "no agent command given" guard lives in `try_main`
        // (split_agents + bail), NOT in the parser — this test pins that
        // contract so the runtime guard is never accidentally removed.
        let cli = Cli::try_parse_from(["orcatui", "run"])
            .expect("clap accepts an empty trailing-var-arg list");
        let command = match cli.command {
            Command::Run { command, .. } => command,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(command.is_empty(), "no trailing args → empty command vec");
        // The dispatch-time guard fires on exactly this empty input.
        assert!(split_agents(command).is_empty());
    }

    #[test]
    fn prs_without_repo_is_a_required_arg_error() {
        let err =
            Cli::try_parse_from(["orcatui", "prs"]).expect_err("prs requires the repo positional");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        let err = Cli::try_parse_from(["orcatui", "frobnicate"])
            .expect_err("unknown subcommand rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
