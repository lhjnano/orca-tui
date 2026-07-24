//! Criterion benchmarks for orcatui's three hot paths.
//!
//! Mirrors the `#[ignore]`d timing probes in `src/perf_probe.rs`, but as
//! proper criterion benches so `cargo bench` yields stable, comparable numbers
//! (MB/s, ns/op, time/frame) and HTML reports under `target/criterion/`.
//!
//! ```sh
//! cargo bench --bench orca                                     # all three
//! cargo bench --bench orca -- terminal_emu_ingest              # one group
//! cargo bench --bench orca -- frame_scheduler -- --quick       # fast sanity
//! ```
//!
//! The three benches correspond to the same paths `perf_probe` measures:
//!
//! 1. [`terminal_emu_ingest`] — ANSI ingest throughput (MB/s).
//! 2. [`frame_scheduler_decide`] — per-tick scheduler decision cost (ns/op).
//! 3. [`n_pane_render`] — full N-pane render cost per frame, N ∈ {1,5,10,20}.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use orcatui::layout::split_panes;
use orcatui::pane::Pane;
use orcatui::scheduler::{FrameScheduler, TARGET_FRAME_60FPS};
use orcatui::terminal_emu::TerminalEmulator;

/// Build ~`mb` megabytes of realistic agent-ish output: text lines, a palette
/// color SGR flip every chunk, and CRLFs. Identical to
/// `perf_probe::synth_ansi` so the bench numbers are directly comparable to the
/// probe's printed ones.
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

/// Throughput of the terminal-emulation ingest path: feed a ~2 MiB synthetic
/// ANSI stream through a fresh 200×50 emulator (scrollback 1000).
///
/// Reports MB/s via `Throughput::Bytes`. The per-iteration emulator
/// construction (setup) is excluded from timing via `iter_batched` — only
/// `feed` is measured.
fn terminal_emu_ingest(c: &mut Criterion) {
    let payload = synth_ansi(2);

    let mut group = c.benchmark_group("terminal_emu_ingest");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3))
        .throughput(Throughput::Bytes(payload.len() as u64));

    group.bench_function("2mib", |b| {
        b.iter_batched(
            || TerminalEmulator::new(200, 50, 1000),
            |mut emu| emu.feed(black_box(&payload)),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Per-tick cost of the frame-scheduler decision: `should_render` followed by
/// either `record_render` (when due) or `note_skipped` (when ahead of budget).
///
/// Reports ns/op for one decision. A fixed `now` (seeded equal to `last_render`
/// at construction) means `should_render` is `false`, so the measured path is
/// the common "not yet due → skip" branch — the dominant per-tick cost while
/// the loop is ahead of the frame budget. This mirrors `perf_probe`'s loop
/// exactly (same fixed-`now` shape).
fn frame_scheduler_decide(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_scheduler_decide");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(2));

    group.bench_function("decide", |b| {
        b.iter_batched(
            || FrameScheduler::new(TARGET_FRAME_60FPS, Instant::now()),
            |mut s| {
                let now = Instant::now();
                if s.should_render(black_box(now)) {
                    s.record_render(black_box(now));
                } else {
                    s.note_skipped();
                }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Full N-pane render path: draw N panes (each a bordered block + painted
/// emulator grid) into a 200×50 `TestBackend` terminal, parameterized over
/// N ∈ {1, 5, 10, 20}. Reports time/frame.
///
/// The 60 fps budget is **16.67 ms/frame** (`TARGET_FRAME_60FPS`); a pane count
/// whose time/frame approaches that is the cliff where the loop starts
/// skipping frames to keep up. `iter_batched_ref` reuses one terminal + pane
/// set across a batch of frames (matching `perf_probe`, which draws 200 frames
/// on the same terminal) — setup is excluded from timing.
fn n_pane_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("n_pane_render");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));

    for &n in &[1usize, 5, 10, 20] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched_ref(
                || {
                    let terminal = Terminal::new(TestBackend::new(200, 50)).expect("terminal");
                    let panes: Vec<Pane> = (0..n)
                        .map(|i| {
                            let mut p = Pane::new(i, format!("agent{i}"), 80, 24);
                            p.feed(b"\x1b[32mhello orca\n\x1b[0mworking...");
                            p
                        })
                        .collect();
                    (terminal, panes)
                },
                |(terminal, panes)| {
                    terminal
                        .draw(|f| {
                            let rects = split_panes(f.area(), n);
                            for (i, p) in panes.iter_mut().enumerate() {
                                let area = rects.get(i).copied().unwrap_or_default();
                                p.render(f, area, i == 0, &orcatui::config::ThemeConfig::default());
                            }
                        })
                        .expect("draw");
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    terminal_emu_ingest,
    frame_scheduler_decide,
    n_pane_render,
);
criterion_main!(benches);
