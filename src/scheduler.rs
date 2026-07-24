//! # Frame scheduler
//!
//! Frame-budget policy for the main render loop. orcatui's loop currently
//! drains the [`crate::bus`] AgentBus, renders, and polls input with a fixed
//! 20 ms sleep. [`FrameScheduler`] turns that into:
//!
//! - a **~16.67 ms (60 fps) frame budget**: render at most once per budget;
//!   when behind, *skip* intermediate renders and draw the latest state once
//!   instead of catching up (catch-up would pile on back-to-back frames and
//!   starve input draining);
//! - **idle back-off**: once nothing has happened for `idle_threshold`
//!   (default 1 s), widen the poll interval to `max_idle_poll` (default 100 ms)
//!   so an idle session stops busy-polling at 60 fps and frees the CPU.
//!
//! The policy is the *testable* part — wiring it into [`crate::app::App`] is a
//! later step. Every method takes `now: Instant` as a parameter so unit tests
//! are deterministic (no hidden `Instant::now()` inside the policy). The loop
//! supplies the clock; the scheduler never reads one itself.

use std::time::{Duration, Instant};

/// 60 fps frame budget — `1_000_000_000 / 60 ≈ 16_666_666` ns (~16.67 ms).
pub const TARGET_FRAME_60FPS: Duration = Duration::from_nanos(16_666_666);

/// Frame-budget + idle-backoff policy for the main loop.
///
/// Owns no threads and performs no I/O — it is pure arithmetic over injected
/// `Instant`s. The expected call sequence per loop iteration is:
///
/// 1. `record_activity(now)` whenever input or PTY output arrives;
/// 2. if `should_render(now)`, render once and `record_render(now)`,
///    else `note_skipped()` and proceed;
/// 3. block on input draining for `poll_timeout(now)`.
///
/// `should_render` / `is_idle` / `poll_timeout` are all `&self` and
/// side-effect-free; only `record_*` / `note_skipped` mutate state.
pub struct FrameScheduler {
    target: Duration,
    last_render: Instant,
    last_activity: Instant,
    idle_threshold: Duration,
    max_idle_poll: Duration,
    frames_rendered: u64,
    frames_skipped: u64,
}

impl FrameScheduler {
    /// Create a scheduler with the given per-frame `target` and initial clock
    /// reading `now`. `last_render` and `last_activity` are both seeded with
    /// `now` so the scheduler starts neither overdue nor idle. Counters start
    /// at zero — creation is not a render.
    #[must_use]
    pub fn new(target: Duration, now: Instant) -> Self {
        Self {
            target,
            last_render: now,
            last_activity: now,
            idle_threshold: Duration::from_secs(1),
            max_idle_poll: Duration::from_millis(100),
            frames_rendered: 0,
            frames_skipped: 0,
        }
    }

    /// `true` when at least one frame budget has elapsed since the last
    /// recorded render, i.e. the loop should render *now*. Uses
    /// [`Instant::saturating_duration_since`] so a small monotonic-clock
    /// regression yields `false` rather than panicking.
    pub fn should_render(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_render) >= self.target
    }

    /// Record that a frame was rendered at `now`: advances `last_render` and
    /// bumps `frames_rendered`. Call exactly once per actual render.
    pub fn record_render(&mut self, now: Instant) {
        self.last_render = now;
        self.frames_rendered += 1;
    }

    /// Record user input or PTY output at `now`, (re)starting the idle timer.
    /// Call whenever *anything* happens that a future frame might need to
    /// reflect; otherwise the loop backs off to `max_idle_poll`.
    pub fn record_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// `true` when no activity has been recorded for at least `idle_threshold`.
    pub fn is_idle(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_activity) >= self.idle_threshold
    }

    /// How long the loop may block waiting for input before it must check the
    /// clock again.
    ///
    /// - **Idle:** `max_idle_poll` (default 100 ms) — stop busy-polling at 60
    ///   fps when nothing is happening.
    /// - **Active:** the remaining time until the next frame boundary, clamped
    ///   to `[1 ms, target]`. The 1 ms floor avoids a zero-length poll when the
    ///   boundary is microseconds away; the `target` ceiling is redundant given
    ///   `saturating_duration_since` but documents the invariant.
    ///
    /// When `should_render` is already `true` the loop renders instead of
    /// polling, so this value is only consulted on the active-but-not-due path.
    pub fn poll_timeout(&self, now: Instant) -> Duration {
        if self.is_idle(now) {
            return self.max_idle_poll;
        }
        let elapsed = now.saturating_duration_since(self.last_render);
        let remaining = self.target.saturating_sub(elapsed);
        remaining.clamp(Duration::from_millis(1), self.target)
    }

    /// Record that the loop skipped a render because it was behind. Bumps
    /// `frames_skipped` only — does not advance `last_render` (no frame was
    /// drawn) and does not touch `frames_rendered`.
    pub fn note_skipped(&mut self) {
        self.frames_skipped += 1;
    }

    /// Total frames actually rendered since construction.
    #[must_use]
    pub const fn frames_rendered(&self) -> u64 {
        self.frames_rendered
    }

    /// Total frames skipped (behind-budget) since construction.
    #[must_use]
    pub const fn frames_skipped(&self) -> u64 {
        self.frames_skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The scheduler is clock-injectable, so every test is deterministic: it
    // anchors on a single `t0` and reasons about `t0 + <constant>`. No sleeps,
    // no wall-clock dependence — the assertions compare two Instants the test
    // itself constructs, so the value of `t0` is irrelevant.
    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn should_render_is_false_right_after_record_render() {
        let t0 = t0();
        let mut s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        s.record_render(t0);
        // Elapsed is ~0 < 16.6 ms.
        assert!(!s.should_render(t0), "just rendered → not due yet");
    }

    #[test]
    fn should_render_is_true_one_frame_later() {
        let t0 = t0();
        let mut s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        s.record_render(t0);
        // At t0 + one full frame budget (16.6 ms) we are due.
        let due = t0 + TARGET_FRAME_60FPS;
        assert!(s.should_render(due), "one frame later → due");
        // And strictly before the boundary, still not due.
        let just_before = t0 + Duration::from_nanos(16_000_000); // ~16.0 ms < 16.6 ms
        assert!(
            !s.should_render(just_before),
            "before the boundary → not due"
        );
    }

    #[test]
    fn poll_timeout_when_active_is_remaining_to_next_frame() {
        let t0 = t0();
        let s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        // Immediately after construction, elapsed ≈ 0 → remaining ≈ target.
        assert_eq!(s.poll_timeout(t0), TARGET_FRAME_60FPS);
        // Halfway through the budget, less than the full target remains.
        let half = t0 + Duration::from_millis(8);
        let to = s.poll_timeout(half);
        assert!(
            to <= TARGET_FRAME_60FPS && to >= Duration::from_millis(1),
            "active poll_timeout must stay within [1ms, target], got {to:?}"
        );
        assert!(
            to < TARGET_FRAME_60FPS,
            "after some elapsed time remaining must be < target, got {to:?}"
        );
    }

    #[test]
    fn poll_timeout_clamps_near_boundary_to_one_ms() {
        let t0 = t0();
        let s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        // 0.5 ms before the boundary → remaining 0.5 ms → clamped up to 1 ms.
        let near = t0 + Duration::from_nanos(16_166_666);
        assert_eq!(s.poll_timeout(near), Duration::from_millis(1));
    }

    #[test]
    fn poll_timeout_when_idle_is_max_idle_poll() {
        let t0 = t0();
        let s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        // Active right at birth.
        assert!(!s.is_idle(t0));
        // Past the idle threshold (default 1 s).
        let later = t0 + Duration::from_secs(2);
        assert!(s.is_idle(later), "2s of silence → idle");
        assert_eq!(
            s.poll_timeout(later),
            Duration::from_millis(100),
            "idle poll_timeout must be max_idle_poll (100 ms)"
        );
    }

    #[test]
    fn is_idle_flips_after_idle_threshold() {
        let t0 = t0();
        let mut s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        // Just under the threshold → still active.
        let just_under = t0 + Duration::from_millis(999);
        assert!(!s.is_idle(just_under));
        // At exactly the threshold → idle (>= comparison).
        let at = t0 + Duration::from_secs(1);
        assert!(s.is_idle(at));
        // Fresh activity resets the timer: now `at` is the new baseline, so a
        // moment later we are active again.
        s.record_activity(at);
        assert!(!s.is_idle(at + Duration::from_millis(999)));
    }

    #[test]
    fn skipping_raises_frames_skipped_not_rendered() {
        let t0 = t0();
        let mut s = FrameScheduler::new(TARGET_FRAME_60FPS, t0);
        assert_eq!(s.frames_rendered(), 0);
        assert_eq!(s.frames_skipped(), 0);

        // Loop is behind: skip three times in a row.
        s.note_skipped();
        s.note_skipped();
        s.note_skipped();
        assert_eq!(s.frames_skipped(), 3);
        assert_eq!(
            s.frames_rendered(),
            0,
            "skipping must not count as rendering"
        );

        // A real render bumps frames_rendered and leaves the skip count alone.
        s.record_render(t0 + Duration::from_millis(20));
        assert_eq!(s.frames_rendered(), 1);
        assert_eq!(s.frames_skipped(), 3, "rendering must not change skipped");
    }
}
