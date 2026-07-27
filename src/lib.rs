//! # orcatui (library)
//!
//! Terminal multi-agent coding orchestrator — a TUI port of
//! [Orca GUI](https://github.com/stablyai/orca). Runs N coding agents
//! (Claude Code, Codex, OpenCode, …) side-by-side in split terminal panes,
//! each in its own PTY / git worktree, monitored from one screen.
//!
//! The `orcatui` **binary** is a thin wrapper around [`cli::run`]; every piece
//! of logic lives in this library crate so it can be unit-tested, benchmarked
//! (`benches/`), and reused as a dependency. Built on
//! [`ratatui-ppalla`](https://crates.io/crates/ratatui-ppalla).

pub mod activity;
pub mod agent;
pub mod app;
pub mod bus;
pub mod cli;
pub mod config;
pub mod coordinator;
pub mod daemon_server;
pub mod integrations;
pub mod layout;
pub mod mobile;
pub mod orca_daemon;
pub mod osc;
pub mod pane;
pub mod perf_probe;
pub mod pty_session;
pub mod query;
pub mod scheduler;
pub mod sidebar;
pub mod ssh;
pub mod sync;
pub mod terminal_emu;
pub mod toast;
pub mod worktree;
