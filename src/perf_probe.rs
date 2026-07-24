//! Throwaway-friendly performance probes for orcatui's own hot paths.
//!
//! These are `#[ignore]`d so they never run in the normal suite — run them
//! explicitly with:
//!
//! ```sh
//! cargo test perf -- --ignored --nocapture
//! ```
//!
//! They print concrete numbers for the three custom paths the 60 fps goal
//! depends on: terminal-emulation ingest throughput, the FrameScheduler
//! decision cost, and the full N-pane render cost (the orcatui analogue of
//! ratatui-ppalla's `layout_paint_20panes` benchmark).

#![allow(clippy::cast_precision_loss, clippy::needless_pass_by_value)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::layout::split_panes;
use crate::pane::Pane;
use crate::scheduler::{FrameScheduler, TARGET_FRAME_60FPS};
use crate::terminal_emu::TerminalEmulator;

/// Build ~`mb` megabytes of realistic agent-ish output: text lines, a color
/// SGR flip every few chars, cursor moves and newlines.
fn synth_ansi(mb: usize) -> Vec<u8> {
    let target = mb * 1024 * 1024;
    let mut out = Vec::with_capacity(target);
    let line = b"The quick brown fox jumps over the lazy dog 0123456789";
    let mut col = 0u8;
    while out.len() < target {
        // a palette color every other word
        out.extend_from_slice(format!("\x1b[3{}m", col % 8).as_bytes());
        out.extend_from_slice(line);
        out.extend_from_slice(b"\x1b[0m ");
        col = col.wrapping_add(1);
        if col % 8 == 0 {
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

/// `synth_ansi` is a pure helper (no timing); test it directly so the size /
/// SGR / CRLF contract the perf probes rely on can't silently regress. This is
/// a normal (non-ignored) test.
#[test]
fn synth_ansi_builds_realistic_payload() {
    let payload = synth_ansi(1);
    // ~1 MiB or larger: the append loop runs until `out.len() >= target`.
    assert!(
        payload.len() >= 1024 * 1024,
        "synth_ansi(1) too small: {} bytes",
        payload.len()
    );
    let s = std::str::from_utf8(&payload).expect("synth_ansi output is valid UTF-8");
    // A palette-color SGR escape is emitted on every line.
    assert!(s.contains("\x1b[3"), "missing SGR palette escape sequence");
    // The recognizable text payload line.
    assert!(s.contains("The quick brown fox"), "missing the text line");
    // CRLF line endings are emitted every 8th line.
    assert!(s.contains("\r\n"), "missing CRLF line endings");
}

#[test]
#[ignore = "performance probe — run with --ignored --nocapture"]
fn perf_terminal_emu_ingest_throughput() {
    let payload = synth_ansi(4); // 4 MB
    let mut emu = TerminalEmulator::new(200, 50, 1000);
    let start = Instant::now();
    emu.feed(black_box(&payload));
    let elapsed = start.elapsed();
    let mbs = (payload.len() as f64) / 1_048_576.0 / elapsed.as_secs_f64();
    println!(
        "perf: terminal_emu ingest = {mbs:.1} MB/s  (4 MiB in {elapsed:?}, cols=200 scrollback=1000)"
    );
    // The roadmap's bar is alacritty-grade (>50 MB/s); assert a sane floor so a
    // regression is loud rather than just printed.
    assert!(mbs > 20.0, "terminal emulation slower than 20 MB/s: {mbs}");
}

#[test]
#[ignore = "performance probe — run with --ignored --nocapture"]
fn perf_frame_scheduler_decision_cost() {
    let mut s = FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now());
    let n = 1_000_000u64;
    let t0 = Instant::now();
    let now = Instant::now();
    for _ in 0..n {
        // The per-tick work: decide, occasionally record.
        if s.should_render(black_box(now)) {
            s.record_render(black_box(now));
        } else {
            s.note_skipped();
        }
    }
    let elapsed = t0.elapsed();
    let ns = elapsed.as_nanos() as f64 / n as f64;
    println!("perf: frame_scheduler decide+record = {ns:.2} ns/op  (1M iterations, {elapsed:?})");
    // Must be a tiny fraction of a 16.67ms frame.
    assert!(ns < 1_000.0, "scheduler decision > 1µs: {ns} ns");
}

#[test]
#[ignore = "performance probe — run with --ignored --nocapture"]
fn perf_n_pane_render() {
    // 16.67ms is the 60fps budget. Measure orcatui's full render path
    // (Pane.render -> paint_grid -> buffer) for N panes filling a 200x50 area.
    for &n in &[1usize, 5, 10, 20] {
        let mut backend = TestBackend::new(200, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut panes: Vec<Pane> = (0..n)
            .map(|i| {
                let mut p = Pane::new(i, format!("agent{i}"), 80, 24);
                p.feed(b"\x1b[32mhello orca\n\x1b[0mworking...");
                p
            })
            .collect();
        let iters = 200u32;
        let start = Instant::now();
        for _ in 0..iters {
            terminal
                .draw(|f| {
                    let rects = split_panes(f.area(), n);
                    for (i, p) in panes.iter_mut().enumerate() {
                        let area = rects.get(i).copied().unwrap_or_default();
                        p.render(f, area, i == 0, &crate::config::ThemeConfig::default());
                    }
                })
                .unwrap();
        }
        let per_frame = start.elapsed() / iters;
        let budget = TARGET_FRAME_60FPS;
        let pct = per_frame.as_secs_f64() / budget.as_secs_f64() * 100.0;
        println!(
            "perf: render {n:>2} pane(s) = {per_frame:?}/frame  ({pct:.1}% of 16.67ms budget)"
        );
    }
}

#[test]
#[ignore = "sanity: not a perf test, just keeps the Duration import used"]
fn _duration_used() {
    let _ = Duration::from_millis(1);
}
