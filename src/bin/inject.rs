//! # orcatui-inject — record / replay / snapshot tool for orcatui's terminal pipeline
//!
//! A companion debugger modeled on `ratatui-ppalla`'s `tui-inject`: drive
//! orcatui's terminal-emulation + query-responder + render pipeline with
//! **deterministic, recorded PTY byte streams** instead of a live terminal, so
//! rendering bugs (e.g. an agent that draws blank) can be reproduced, inspected,
//! and bisected without the flakiness of live debugging.
//!
//! Where tui-inject injects *keyboard events* into a widget loop, orcatui-inject
//! injects *agent output bytes* into [`orcatui::terminal_emu::TerminalEmulator`]
//! (+ [`orcatui::query::QueryResponder`]), because that byte stream is what
//! determines whether an agent renders.
//!
//! ## Commands
//!
//! ```text
//! orcatui-inject record [--for 10] [--out recording.bin] -- <agent> [args...]
//! orcatui-inject replay <file> [--size WxH] [--resize WxH@chunk] [--chunk N] [--render]
//! ```
//!
//! - `record` spawns a real agent in a PTY and saves its raw output bytes.
//! - `replay` feeds those bytes through the emulator (+ query responder) at a
//!   chosen size, optionally simulating a mid-stream resize, and dumps the
//!   resulting emulator grid as text — so you can see exactly what orcatui's
//!   vt100 produced (logo present? blank? misaligned?). `--render` additionally
//!   renders a real pane frame (with border + theme) to show what the user sees.

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use orcatui::config::ThemeConfig;
use orcatui::pane::Pane;
use orcatui::pty_session::PtySession;
use orcatui::query::QueryResponder;
use orcatui::terminal_emu::TerminalEmulator;

#[derive(Parser)]
#[command(
    name = "orcatui-inject",
    about = "Record/replay/snapshot orcatui's terminal pipeline (tui-inject for PTY bytes)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record a real agent's raw PTY output to a file.
    Record {
        /// How long to record, in seconds.
        #[arg(long, default_value_t = 10)]
        for_secs: u64,
        /// Output file (raw bytes).
        #[arg(long, default_value = "recording.bin")]
        out: String,
        /// PTY size to give the agent while recording.
        #[arg(long, default_value = "80x24")]
        size: String,
        /// Mid-recording PTY resize `WxH@secs`: resize the agent's PTY after
        /// `secs` seconds (reproduces orcatui spawning then resizing). The
        /// byte offset of the resize is printed to stderr.
        #[arg(long)]
        resize: Option<String>,
        /// The agent command, verbatim. Use `--` first if it carries its own
        /// flags, e.g. `record --for 8 -- opencode --foo`.
        #[arg(value_name = "AGENT")]
        agent: Vec<String>,
    },
    /// Replay a recording through the emulator + query responder and dump a frame.
    Replay {
        /// Recording file to replay.
        file: String,
        /// Emulator size `WxH` (use the live pane size to reproduce it).
        #[arg(long, default_value = "80x24")]
        size: String,
        /// Resize to `WxH` after the Nth chunk (reproduce a live layout resize).
        #[arg(long)]
        resize: Option<String>,
        /// Feed in chunks of N bytes (0 = all at once). Reproduces streaming.
        #[arg(long, default_value_t = 0)]
        chunk: usize,
        /// Also render a real pane frame (border + theme) like the user sees.
        #[arg(long)]
        render: bool,
        /// Print per-chunk query-response activity to stderr.
        #[arg(long)]
        verbose: bool,
    },
}

fn parse_size(s: &str) -> Result<(u16, u16)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow!("size must be WxH, got {s:?}"))?;
    Ok((w.parse()?, h.parse()?))
}

/// Parse `WxH@chunk` for `--resize`.
fn parse_resize(s: &str) -> Result<(u16, u16, usize)> {
    let (size, at) = s
        .split_once('@')
        .ok_or_else(|| anyhow!("--resize must be WxH@chunk, got {s:?}"))?;
    let (w, h) = parse_size(size)?;
    Ok((w, h, at.parse()?))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Record {
            for_secs,
            out,
            size,
            resize,
            agent,
        } => record(&agent, for_secs, &out, &size, resize.as_deref()),
        Cmd::Replay {
            file,
            size,
            resize,
            chunk,
            render,
            verbose,
        } => replay(&file, &size, resize.as_deref(), chunk, render, verbose),
    }
}

/// Spawn the agent in a PTY, drain its output for `for_secs`, write raw bytes.
/// If `resize` is `WxH@secs`, resize the PTY mid-recording to reproduce a live
/// spawn-then-resize, and print the byte offset where the resize happened.
fn record(
    agent: &[String],
    for_secs: u64,
    out: &str,
    size: &str,
    resize: Option<&str>,
) -> Result<()> {
    if agent.is_empty() {
        return Err(anyhow!(
            "record requires an agent command after `--`, e.g. `orcatui-inject record -- opencode`"
        ));
    }
    let (cols, rows) = parse_size(size)?;
    let (mut session, rx) = PtySession::spawn(agent.to_vec(), None, cols, rows)
        .with_context(|| format!("spawning {agent:?} in a PTY"))?;
    let mut file = std::fs::File::create(out).with_context(|| format!("creating {out}"))?;
    let start = std::time::Instant::now();
    let deadline = start + Duration::from_secs(for_secs);
    let mut pending_resize = resize.map(parse_resize).transpose()?;
    let mut total = 0usize;
    while std::time::Instant::now() < deadline {
        if let Some((rw, rh, at_secs)) = pending_resize {
            if std::time::Instant::now() >= start + Duration::from_secs(at_secs as u64) {
                let _ = session.resize(rw, rh);
                eprintln!("orcatui-inject: resized PTY → {rw}x{rh} at byte offset {total}");
                pending_resize = None;
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                file.write_all(&chunk)?;
                total += chunk.len();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = session.kill();
    eprintln!("orcatui-inject: recorded {total} bytes → {out}");
    Ok(())
}

/// Feed the recording through the emulator (+ responder), dump the resulting grid.
fn replay(
    file: &str,
    size: &str,
    resize: Option<&str>,
    chunk: usize,
    render: bool,
    verbose: bool,
) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {file}"))?;
    let (w, h) = parse_size(size)?;
    let resize = resize.map(parse_resize).transpose()?;

    let mut emu = TerminalEmulator::new(w, h, 1000);
    let mut responder = QueryResponder::new();
    let theme = ThemeConfig::default();
    let chunk_size = if chunk == 0 { bytes.len() } else { chunk };
    let mut response_bytes = 0usize;
    let mut response_chunks = 0usize;

    for (i, chunk_bytes) in bytes.chunks(chunk_size).enumerate() {
        let resp = responder.process(chunk_bytes, &theme);
        if !resp.is_empty() {
            response_bytes += resp.len();
            response_chunks += 1;
            if verbose {
                eprintln!("orcatui-inject: chunk {i}: +{} response bytes", resp.len());
            }
        }
        emu.feed(chunk_bytes);
        if let Some((rw, rh, at)) = resize {
            if i + 1 == at {
                emu.resize(rw, rh);
                if verbose {
                    eprintln!("orcatui-inject: resized → {rw}x{rh} after chunk {}", i + 1);
                }
            }
        }
    }

    let grid = emu.grid();
    let nonempty = grid
        .iter()
        .map(|r| r.iter().filter(|c| c.has_contents()).count())
        .sum::<usize>();

    if render {
        // Render a real pane frame (border + theme) like the user sees.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut pane = Pane::new(0, "agent", w, h);
        // Refeed into the pane's own emulator (Pane owns its emulator + scanner).
        pane.feed(&bytes);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).context("test terminal")?;
        terminal
            .draw(|f| pane.render(f, f.area(), true, &theme))
            .context("draw")?;
        let buf = terminal.backend().buffer();
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        print!("{out}");
    } else {
        // Dump the raw emulator grid (each cell → first glyph or space).
        let mut out = String::new();
        for row in &grid {
            for cell in row {
                out.push(cell.chars.chars().next().unwrap_or(' '));
            }
            out.push('\n');
        }
        print!("{out}");
    }

    eprintln!(
        "orcatui-inject: {f_b} bytes @ {w}x{h} → {nonempty} non-empty cells; {response_chunks} query chunks ({response_bytes} response bytes)",
        f_b = bytes.len(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_ok() {
        assert_eq!(parse_size("50x20").unwrap(), (50, 20));
        assert_eq!(parse_size("120x30").unwrap(), (120, 30));
    }

    #[test]
    fn parse_size_bad() {
        assert!(parse_size("120").is_err());
        assert!(parse_size("wxh").is_err());
    }

    #[test]
    fn parse_resize_ok() {
        assert_eq!(parse_resize("120x30@3").unwrap(), (120, 30, 3));
    }

    #[test]
    fn parse_resize_bad() {
        assert!(parse_resize("120x30").is_err());
        assert!(parse_resize("120x30@x").is_err());
    }
}
