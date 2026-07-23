//! Render a representative orca-tui frame to the terminal using the REAL
//! rendering pipeline (`Pane::render` + `split_panes`), via an in-memory
//! `TestBackend`. The output is a faithful text "screenshot" of what the TUI
//! looks like — used for the README and for visual regression.
//!
//! Run: `cargo run --example screenshot`

use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use orca_tui::agent::AgentState;
use orca_tui::layout::split_panes;
use orca_tui::pane::Pane;

const WIDTH: u16 = 104;
const HEIGHT: u16 = 30;

const FOOTER: &str =
    " Tab: focus \u{00B7} Shift+Tab: prev \u{00B7} Alt+\u{2191}\u{2193}: scroll \u{00B7} Ctrl+C / Esc: quit \u{00B7} type to send ";

fn agent_pane(id: usize, name: &str, branch: &str, state: AgentState, output: &str) -> Pane {
    let mut p = Pane::new(id, name, WIDTH, HEIGHT);
    p.set_state(state);
    p.set_branch(Some(branch.to_string()));
    p.feed(output.as_bytes());
    p
}

fn draw(panes: &[Pane], focus: usize) {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("backend");
    terminal
        .draw(|f| {
            let total = f.area();
            // Reserve the last line for the footer, mirroring App::render.
            use ratatui::layout::{Constraint, Layout};
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(total);
            let pane_area = chunks[0];
            let footer_area = chunks[1];
            let rects = split_panes(pane_area, panes.len());
            for (i, p) in panes.iter().enumerate() {
                let area = rects.get(i).copied().unwrap_or_default();
                p.render(f, area, i == focus);
            }
            f.render_widget(
                Paragraph::new(FOOTER).style(Style::default().fg(Color::DarkGray)),
                footer_area,
            );
        })
        .expect("draw");

    // Print the buffer as a text screenshot.
    let buf = terminal.backend().buffer();
    let area = buf.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        // Trim trailing spaces on each line for a cleaner embed.
        out.truncate(out.trim_end().len());
        out.push('\n');
    }
    print!("{out}");
}

fn main() {
    // Four agents in mid-flight: two running, one done, one failed.
    let panes = vec![
        agent_pane(
            0,
            "claude",
            "orca/claude-a1b2",
            AgentState::Running,
            "\x1b[32m\u{2713}\x1b[0m Read src/app.rs (412 lines)\n\x1b[32m\u{2713}\x1b[0m Edited src/app.rs (+18 -3)\n\x1b[36m?\x1b[0m Run cargo test? [y/n] ",
        ),
        agent_pane(
            1,
            "codex",
            "orca/codex-c3d4",
            AgentState::Running,
            "thinking...\n\x1b[33m\u{270b}\x1b[0m Approve write to lib.rs?\n  use std::sync::Arc;\n  pub fn new() -> Self { .. }",
        ),
        agent_pane(
            2,
            "opencode",
            "main",
            AgentState::Done(Some(0)),
            "\x1b[32m\u{2713}\x1b[0m Done \u{2014} summary:\n  added 12 tests, all green",
        ),
        agent_pane(
            3,
            "gemini",
            "orca/gemini-e5f6",
            AgentState::Failed("exit code 1".into()),
            "\x1b[31m\u{2717}\x1b[0m Error: rate limit hit\n  retrying in 32s...",
        ),
    ];
    draw(&panes, 0);
}
