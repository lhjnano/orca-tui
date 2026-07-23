//! # SSH remote — remote-agent PTY over `ssh` (Feature 8)
//!
//! orca-tui normally spawns a coding agent in a *local* PTY via
//! [`crate::pty_session::PtySession::spawn`]. Feature 8 runs an agent on a
//! *remote* box: the remote agent is conceptually the same
//! [`PtySession::spawn`] call, only the argv is an `ssh` invocation that
//! requests a remote PTY and attaches the agent's stdout to it. orca-tui
//! shells out to the `ssh` CLI binary (via [`crate::pty_session`]); **no Rust
//! SSH crate is added**.
//!
//! This module is the mechanism layer only: it builds the `ssh` argv from a
//! parsed [`SshTarget`] and offers a [`ReconnectPolicy`] abstraction for the
//! later wiring task. The actual spawn (feeding
//! [`SshTarget::command_vec`] into [`crate::pty_session::PtySession::spawn`])
//! is deferred — here we expose only pure command-building logic so it is
//! fully unit-testable with no network connection.
//!
//! ## argv shape
//!
//! [`SshTarget::command_vec`] yields, in order:
//!
//! ```text
//! ssh -tt -o BatchMode=yes -o ConnectTimeout=10 [-p PORT] <user@host|host> <remote_command...>
//! ```
//!
//! `-tt` forces a remote PTY (so escape sequences stream back and the local
//! emulator renders the remote terminal); `BatchMode=yes` refuses interactive
//! prompts (no hanging on a password); `ConnectTimeout=10` bounds the connect.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// A remote SSH target — the user, host, port and the remote command to run.
///
/// Construct with [`SshTarget::parse`] (which accepts `user@host`, `host`, or
/// `user@host:port`), then attach the remote command with
/// [`SshTarget::with_command`]. The argv for
/// [`crate::pty_session::PtySession`] is produced by [`SshTarget::command_vec`].
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// Optional remote user; `None` lets `ssh` pick (typically the local user).
    user: Option<String>,
    /// Remote host (never empty after a successful [`SshTarget::parse`]).
    host: String,
    /// Optional TCP port; `None` means ssh's default (22).
    port: Option<u16>,
    /// The remote command argv (may be empty — ssh then opens an interactive
    /// shell on the remote box).
    remote_command: Vec<String>,
}

impl SshTarget {
    /// Parse an SSH target specification.
    ///
    /// Accepts, leniently:
    /// - `user@host`
    /// - `host`
    /// - `user@host:port`
    /// - `host:port`
    ///
    /// A trailing `:<digits>` is treated as a port only when the digits parse
    /// as a `u16`; otherwise the whole tail (colon included) is kept as part of
    /// the host. The only hard error is an empty host. Surrounding whitespace
    /// is trimmed. The remote command is left empty here — set it via
    /// [`SshTarget::with_command`].
    ///
    /// # Errors
    ///
    /// Returns an error iff the host resolves to empty (e.g. `""`, `"@"`,
    /// `"user@"`, `":2222"`).
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();

        let (user, rest) = match spec.split_once('@') {
            Some((u, r)) => {
                let u = u.trim();
                (
                    if u.is_empty() {
                        None
                    } else {
                        Some(u.to_owned())
                    },
                    r,
                )
            }
            None => (None, spec),
        };

        // A trailing :<digits> is a port iff it parses as u16; otherwise the
        // whole `rest` (colon and all) is treated as the host.
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() => match p.parse::<u16>() {
                Ok(num) => (h, Some(num)),
                Err(_) => (rest, None),
            },
            _ => (rest, None),
        };

        if host.is_empty() {
            bail!("ssh target host is empty in spec {spec:?}");
        }

        Ok(Self {
            user,
            host: host.to_owned(),
            port,
            remote_command: Vec::new(),
        })
    }

    /// Builder: attach the remote command argv (`command[0]` is the remote
    /// program). Consumes and returns `self`.
    #[must_use]
    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.remote_command = command;
        self
    }

    /// Build the `ssh` argv to run this target.
    ///
    /// The first element is the literal `"ssh"`; `-tt` (force remote PTY),
    /// `-o BatchMode=yes` (no interactive prompts) and
    /// `-o ConnectTimeout=10` are always present; `-p <port>` appears only when
    /// a port was set; then the target (`user@host` when a user is set, else
    /// `host`); then each word of the remote command in order.
    ///
    /// No shell quoting is applied — this is an argv, not a shell string.
    #[must_use]
    pub fn command_vec(&self) -> Vec<String> {
        // Upper bound: ssh + -tt + two -o pairs (6) + optional -p pair (2) +
        // target (1) + remote_command. Never under-allocates.
        let mut argv: Vec<String> = Vec::with_capacity(9 + self.remote_command.len());
        argv.push("ssh".to_owned());
        argv.push("-tt".to_owned());
        argv.push("-o".to_owned());
        argv.push("BatchMode=yes".to_owned());
        argv.push("-o".to_owned());
        argv.push("ConnectTimeout=10".to_owned());
        if let Some(port) = self.port {
            argv.push("-p".to_owned());
            argv.push(port.to_string());
        }
        let target = match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        argv.push(target);
        argv.extend(self.remote_command.iter().cloned());
        argv
    }

    /// A short human-readable label for the target.
    ///
    /// `user@host`, `user@host:port`, `host`, or `host:port` — whichever
    /// applies. Intended for a pane title / status line.
    #[must_use]
    pub fn display(&self) -> String {
        let core = match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        match self.port {
            Some(p) => format!("{core}:{p}"),
            None => core,
        }
    }
}

/// Reconnect policy for a dropped SSH session.
///
/// Pure timing math — the caller (a later wiring task) sleeps
/// [`ReconnectPolicy::backoff_for`] before the next attempt and stops once
/// [`ReconnectPolicy::should_retry`] returns false. No network logic lives
/// here.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Maximum number of reconnect attempts (1-based).
    max_attempts: u32,
    /// Backoff at attempt 1; doubled each subsequent attempt.
    base_backoff: Duration,
    /// Upper bound on the computed backoff.
    max_backoff: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    /// The backoff to wait *before* attempt number `attempt` (1-based).
    ///
    /// `base * 2^(attempt-1)`, capped at `max_backoff`. `attempt == 0` is
    /// treated as 1 (the saturating shift). All arithmetic is overflow-safe
    /// (saturating) so an absurd `attempt` cannot panic or wrap.
    #[must_use]
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        // 2^(attempt-1); cap the shift so a huge attempt can't overflow.
        let factor: u128 = 1u128
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u128::MAX);
        let scaled = self.base_backoff.as_nanos().saturating_mul(factor);
        // Saturate at max_backoff; the result then fits in u64 (a real
        // Duration's total nanos always does).
        let capped = scaled.min(self.max_backoff.as_nanos());
        Duration::from_nanos(capped as u64)
    }

    /// Whether the caller should make attempt number `attempt` (1-based).
    ///
    /// True iff `attempt <= max_attempts`.
    #[must_use]
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt <= self.max_attempts
    }
}

/// Stateful reconnect state machine for a dropped remote SSH session.
///
/// Wraps a [`ReconnectPolicy`] with bookkeeping for the *current* retry
/// sequence: how many failures have happened since the last success, and when
/// the most recent failure occurred. It owns only the **timing** of reconnects
/// — it never opens a connection itself (that is the app's job via
/// [`SshTarget::command_vec`]). "Reconnect" means re-establish the remote SSH
/// session (a new spawn) after the previous one dropped, with exponential
/// backoff, up to [`ReconnectPolicy::max_attempts`].
///
/// All time logic takes an injected `now: [`Instant`]` so tests are fully
/// deterministic — no hidden `Instant::now()` and no real sleeps.
#[derive(Debug, Clone)]
pub struct ReconnectSession {
    /// The policy driving backoff and the attempt ceiling.
    policy: ReconnectPolicy,
    /// Number of failures recorded since the last [`ReconnectSession::record_success`].
    attempt: u32,
    /// When the most recent failure happened; `None` while the session is
    /// healthy (or has never failed).
    last_failure: Option<Instant>,
}

impl ReconnectSession {
    /// Create a fresh session tracker driven by `policy`.
    ///
    /// Starts with zero recorded failures and no pending retry.
    #[must_use]
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            last_failure: None,
        }
    }

    /// Mark the session healthy — resets the failure counter and clears the
    /// pending backoff window. Call this after a successful (re)connect.
    pub fn record_success(&mut self) {
        self.attempt = 0;
        self.last_failure = None;
    }

    /// Record a failure at time `now`: increments the attempt counter and
    /// stamps `last_failure`. Takes `now` as a parameter (rather than reading
    /// `Instant::now()` internally) so tests are deterministic.
    pub fn record_failure(&mut self, now: Instant) {
        self.attempt = self.attempt.saturating_add(1);
        self.last_failure = Some(now);
    }

    /// Number of failures recorded since the last [`Self::record_success`].
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Whether the retry budget is spent — no further attempts should be made.
    ///
    /// True once `attempts` exceeds [`ReconnectPolicy::max_attempts`].
    #[must_use]
    pub fn exhausted(&self) -> bool {
        !self.policy.should_retry(self.attempt)
    }

    /// How long until the next reconnect attempt may be made.
    ///
    /// Returns `None` when [`Self::exhausted`] (no retries left) **or** when no
    /// failure has been recorded yet (fresh / healthy session — nothing to
    /// retry). Otherwise returns `Some(remaining)`: the backoff for the current
    /// attempt ([`ReconnectPolicy::backoff_for`]) minus the time already
    /// elapsed since [`Self::record_failure`], clamped to `>= 0` (saturating).
    /// `Some(Duration::ZERO)` means "retry now".
    #[must_use]
    pub fn next_retry_in(&self, now: Instant) -> Option<Duration> {
        if self.exhausted() {
            return None;
        }
        // No failure recorded -> nothing pending. (attempt == 0 here.)
        let last = self.last_failure?;
        let backoff = self.policy.backoff_for(self.attempt);
        // saturating_duration_since yields ZERO if `now < last` (e.g. a
        // monotonic-clock regression), so the subtraction below can never
        // underflow once we clamp with saturating_sub.
        let elapsed = now.saturating_duration_since(last);
        Some(backoff.saturating_sub(elapsed))
    }

    /// Convenience: is it time to retry right now?
    ///
    /// True iff not exhausted and the backoff window has fully elapsed
    /// ([`Self::next_retry_in`] returns `Some(Duration::ZERO)`).
    #[must_use]
    pub fn should_retry_now(&self, now: Instant) -> bool {
        !self.exhausted() && self.next_retry_in(now) == Some(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: does `argv` contain the adjacent pair (flag, value)?
    fn has_pair(argv: &[String], flag: &str, val: &str) -> bool {
        argv.windows(2)
            .any(|w| w[0] == flag && w[1] == val)
    }

    // ---- SshTarget::parse -------------------------------------------------

    #[test]
    fn parse_user_at_host() {
        let t = SshTarget::parse("alice@box.example").expect("parse");
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "box.example");
        assert!(t.port.is_none(), "no port should be set");
        assert!(t.remote_command.is_empty());
    }

    #[test]
    fn parse_bare_host() {
        let t = SshTarget::parse("build-server").expect("parse");
        assert!(t.user.is_none());
        assert_eq!(t.host, "build-server");
        assert!(t.port.is_none());
    }

    #[test]
    fn parse_user_at_host_colon_port() {
        let t = SshTarget::parse("alice@box.example:2222").expect("parse");
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "box.example");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parse_host_colon_port_no_user() {
        // Lenient extension: `host:port` without a user is accepted too.
        let t = SshTarget::parse("box.example:2222").expect("parse");
        assert!(t.user.is_none());
        assert_eq!(t.host, "box.example");
        assert_eq!(t.port, Some(2222));
    }

    #[test]
    fn parse_trims_whitespace() {
        let t = SshTarget::parse("  alice@box  ").expect("parse");
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "box");
    }

    #[test]
    fn parse_empty_or_whitespace_errors() {
        assert!(SshTarget::parse("").is_err(), "empty string must error");
        assert!(SshTarget::parse("   ").is_err(), "whitespace-only must error");
    }

    #[test]
    fn parse_empty_host_errors() {
        // Every form whose host resolves to empty must error.
        assert!(SshTarget::parse("@").is_err(), "'@' -> empty host");
        assert!(SshTarget::parse("user@").is_err(), "'user@' -> empty host");
        assert!(SshTarget::parse(":2222").is_err(), "':2222' -> empty host");
    }

    #[test]
    fn parse_empty_user_is_treated_as_no_user() {
        // Lenient: a leading '@' with an empty user is not an error as long as
        // the host is present — the empty user simply becomes `None`.
        let t = SshTarget::parse("@box").expect("host present -> ok");
        assert!(t.user.is_none());
        assert_eq!(t.host, "box");
    }

    // ---- SshTarget::command_vec ------------------------------------------

    #[test]
    fn command_vec_starts_with_ssh_and_has_tt() {
        let t = SshTarget::parse("alice@box").unwrap();
        let argv = t.command_vec();
        assert_eq!(argv.first().map(String::as_str), Some("ssh"), "first element must be ssh");
        assert!(argv.iter().any(|a| a == "-tt"), "-tt must always be present");
    }

    #[test]
    fn command_vec_always_includes_batchmode_and_connecttimeout() {
        let argv = SshTarget::parse("box").unwrap().command_vec();
        assert!(has_pair(&argv, "-o", "BatchMode=yes"));
        assert!(has_pair(&argv, "-o", "ConnectTimeout=10"));
    }

    #[test]
    fn command_vec_port_present_only_when_set() {
        // No port -> no -p.
        let argv0 = SshTarget::parse("alice@box").unwrap().command_vec();
        assert!(!argv0.iter().any(|a| a == "-p"), "no -p when port unset");

        // Port set -> -p <port> adjacent pair.
        let argv1 = SshTarget::parse("alice@box:2222").unwrap().command_vec();
        let idx = argv1
            .iter()
            .position(|a| a == "-p")
            .expect("-p must be present when port is set");
        assert_eq!(argv1[idx + 1], "2222", "port value must follow -p");
    }

    #[test]
    fn command_vec_target_is_user_at_host_or_host() {
        let with_user = SshTarget::parse("alice@box").unwrap().command_vec();
        assert!(
            with_user.iter().any(|a| a == "alice@box"),
            "target must be user@host when a user is set"
        );

        let bare = SshTarget::parse("box").unwrap().command_vec();
        assert!(bare.iter().any(|a| a == "box"), "target must be host when no user");
    }

    #[test]
    fn command_vec_remote_command_words_in_order_at_tail() {
        let t = SshTarget::parse("alice@box")
            .unwrap()
            .with_command(vec!["claude".into(), "--model".into(), "x".into()]);
        let argv = t.command_vec();
        let len = argv.len();
        assert!(len >= 3, "argv should contain the remote command words");
        // The remote command is the trailing run, in order.
        assert_eq!(argv[len - 3], "claude");
        assert_eq!(argv[len - 2], "--model");
        assert_eq!(argv[len - 1], "x");
    }

    #[test]
    fn command_vec_full_shape_with_port_and_command() {
        // Locks the exact argv order end-to-end.
        let t = SshTarget::parse("bob@host:2222")
            .unwrap()
            .with_command(vec!["claude".into()]);
        assert_eq!(
            t.command_vec(),
            vec![
                "ssh",
                "-tt",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "bob@host",
                "claude",
            ]
        );
    }

    #[test]
    fn command_vec_full_shape_no_port_no_user_no_command() {
        // Minimal argv: ssh + -tt + two -o pairs + bare host.
        let t = SshTarget::parse("box").unwrap();
        assert_eq!(
            t.command_vec(),
            vec!["ssh", "-tt", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "box"]
        );
    }

    // ---- SshTarget::display ----------------------------------------------

    #[test]
    fn display_formats() {
        assert_eq!(SshTarget::parse("alice@box").unwrap().display(), "alice@box");
        assert_eq!(
            SshTarget::parse("alice@box:2222").unwrap().display(),
            "alice@box:2222"
        );
        assert_eq!(SshTarget::parse("box").unwrap().display(), "box");
        assert_eq!(SshTarget::parse("box:2222").unwrap().display(), "box:2222");
    }

    // ---- ReconnectPolicy --------------------------------------------------

    #[test]
    fn reconnect_default_values() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.base_backoff, Duration::from_secs(1));
        assert_eq!(p.max_backoff, Duration::from_secs(30));
    }

    #[test]
    fn backoff_for_attempt_1_is_base_2_is_double() {
        let p = ReconnectPolicy::default(); // base 1s
        assert_eq!(p.backoff_for(1), Duration::from_secs(1), "attempt 1 == base");
        assert_eq!(p.backoff_for(2), Duration::from_secs(2), "attempt 2 == double");
        assert_eq!(p.backoff_for(3), Duration::from_secs(4));
    }

    #[test]
    fn backoff_for_capped_at_max() {
        let p = ReconnectPolicy::default(); // max 30s; base 1s
        // 1,2,4,8,16,32 -> 32 caps to 30.
        assert_eq!(p.backoff_for(5), Duration::from_secs(16));
        assert_eq!(p.backoff_for(6), Duration::from_secs(30), "32s caps to max");
        assert_eq!(p.backoff_for(100), Duration::from_secs(30), "absurd attempt still caps");
    }

    #[test]
    fn backoff_for_does_not_panic_on_zero() {
        // attempt == 0 is undefined by the (1-based) spec but must not panic;
        // the saturating shift yields base.
        let p = ReconnectPolicy::default();
        assert_eq!(p.backoff_for(0), Duration::from_secs(1));
    }

    #[test]
    fn should_retry_boundaries() {
        let p = ReconnectPolicy::default(); // max 3
        assert!(p.should_retry(1));
        assert!(p.should_retry(2));
        assert!(p.should_retry(3), "attempt == max_attempts still allowed");
        assert!(!p.should_retry(4), "beyond max_attempts must be false");
        assert!(!p.should_retry(99));
    }

    #[test]
    fn backoff_for_custom_policy() {
        // Non-default policy to confirm the math isn't hardcoded to 1s/30s.
        let p = ReconnectPolicy {
            max_attempts: 5,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        };
        assert_eq!(p.backoff_for(1), Duration::from_millis(100));
        assert_eq!(p.backoff_for(2), Duration::from_millis(200));
        assert_eq!(p.backoff_for(3), Duration::from_millis(400));
        assert_eq!(p.backoff_for(4), Duration::from_millis(800));
        assert_eq!(p.backoff_for(5), Duration::from_millis(1600));
        assert_eq!(p.backoff_for(6), Duration::from_secs(2), "3.2s caps to 2s");
    }

    // ---- ReconnectSession ------------------------------------------------

    // Small custom policy so backoff math is obvious and tests are instant
    // (no real sleeps — `now` is always injected).
    fn tiny_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: 3,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
        }
    }

    #[test]
    fn fresh_session_has_no_pending_retry() {
        let s = ReconnectSession::new(ReconnectPolicy::default());
        let t = Instant::now();
        assert_eq!(s.attempts(), 0);
        assert!(!s.exhausted(), "fresh session is not exhausted");
        // No failure recorded -> nothing pending.
        assert_eq!(s.next_retry_in(t), None);
        assert!(!s.should_retry_now(t));
    }

    #[test]
    fn record_failure_increments_and_success_resets() {
        let mut s = ReconnectSession::new(ReconnectPolicy::default());
        let t0 = Instant::now();
        assert_eq!(s.attempts(), 0);
        s.record_failure(t0);
        assert_eq!(s.attempts(), 1);
        s.record_failure(t0);
        assert_eq!(s.attempts(), 2);
        s.record_success();
        assert_eq!(s.attempts(), 0, "success resets the counter");
    }

    #[test]
    fn next_retry_in_full_backoff_right_after_failure() {
        let mut s = ReconnectSession::new(tiny_policy());
        let t0 = Instant::now();
        s.record_failure(t0); // attempt 1 -> backoff 100ms
        // Immediately after the failure: the full backoff still remains.
        assert_eq!(s.next_retry_in(t0), Some(Duration::from_millis(100)));
    }

    #[test]
    fn next_retry_in_decreases_with_elapsed_time() {
        let mut s = ReconnectSession::new(tiny_policy());
        let t0 = Instant::now();
        s.record_failure(t0); // backoff 100ms
        // Halfway through the backoff window: ~50ms remains.
        let half = t0.checked_add(Duration::from_millis(50)).expect("checked_add");
        let remaining = s.next_retry_in(half).expect("Some");
        // Exact arithmetic: (t0 + 50ms) - t0 == 50ms, so 100ms - 50ms == 50ms.
        assert_eq!(remaining, Duration::from_millis(50));
    }

    #[test]
    fn next_retry_in_zero_and_should_retry_now_when_window_elapsed() {
        let mut s = ReconnectSession::new(tiny_policy());
        let t0 = Instant::now();
        s.record_failure(t0); // backoff 100ms
        // Before the window elapses -> not yet.
        assert!(!s.should_retry_now(t0));
        assert_ne!(s.next_retry_in(t0), Some(Duration::ZERO));
        // Exactly at the backoff boundary -> time to retry.
        let ready = t0.checked_add(Duration::from_millis(100)).expect("checked_add");
        assert_eq!(s.next_retry_in(ready), Some(Duration::ZERO));
        assert!(s.should_retry_now(ready));
    }

    #[test]
    fn next_retry_in_clamps_to_zero_well_past_window() {
        // saturating_sub must never underflow, even far in the future.
        let mut s = ReconnectSession::new(tiny_policy());
        let t0 = Instant::now();
        s.record_failure(t0);
        let way_past = t0.checked_add(Duration::from_secs(60)).expect("checked_add");
        assert_eq!(s.next_retry_in(way_past), Some(Duration::ZERO));
        assert!(s.should_retry_now(way_past));
    }

    #[test]
    fn exhausted_after_exceeding_max_attempts() {
        let mut s = ReconnectSession::new(tiny_policy()); // max 3
        let t = Instant::now();
        s.record_failure(t); // attempt 1
        s.record_failure(t); // attempt 2
        s.record_failure(t); // attempt 3 == max -> still allowed
        assert!(!s.exhausted(), "attempt == max_attempts is not exhausted");
        s.record_failure(t); // attempt 4 -> exhausted
        assert!(s.exhausted());
        assert_eq!(s.next_retry_in(t), None, "exhausted -> None");
        assert!(!s.should_retry_now(t));
    }

    #[test]
    fn next_retry_in_overflow_safe_on_many_failures_before_exhaustion() {
        // A policy with a huge ceiling so we can push attempt far without
        // becoming exhausted, exercising backoff_for + saturating math.
        let p = ReconnectPolicy {
            max_attempts: u32::MAX,
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
        };
        let mut s = ReconnectSession::new(p);
        let t0 = Instant::now();
        for _ in 0..100 {
            s.record_failure(t0);
        }
        assert_eq!(s.attempts(), 100);
        assert!(!s.exhausted(), "max_attempts = u32::MAX");
        // No panic, and the result is clamped/capped (<= max_backoff).
        let remaining = s.next_retry_in(t0).expect("Some");
        assert!(remaining <= Duration::from_millis(10), "capped at max_backoff");
    }

    #[test]
    fn success_mid_sequence_resets_backoff() {
        let mut s = ReconnectSession::new(tiny_policy());
        let t0 = Instant::now();
        s.record_failure(t0); // attempt 1 -> 100ms
        s.record_failure(t0); // attempt 2 -> 200ms
        assert_eq!(
            s.next_retry_in(t0),
            Some(Duration::from_millis(200)),
            "second attempt has doubled backoff"
        );
        s.record_success(); // reset
        s.record_failure(t0); // attempt 1 again -> fresh 100ms
        assert_eq!(s.attempts(), 1);
        assert_eq!(s.next_retry_in(t0), Some(Duration::from_millis(100)));
    }
}
