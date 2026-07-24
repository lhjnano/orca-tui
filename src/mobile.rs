//! # Mobile companion WebSocket server (Feature 10)
//!
//! A small WS server the TUI runs locally so a phone/PWA can connect, see live
//! agent status, and (eventually) send follow-ups. The orcatui side here is
//! the **server**; a mobile client app is out of scope for this binary.
//!
//! ## Threading model
//!
//! The main orcatui app is synchronous; this server is spawned on its own
//! dedicated tokio runtime by a later wiring step. [`serve`] owns the runtime's
//! `state_rx` (an unbounded mpsc the app feeds `Vec<AgentSnapshot>` into) and
//! fans every update out to all connected WS clients via a
//! [`tokio::sync::broadcast`] channel. Each accepted connection runs on its own
//! `tokio::task`.
//!
//! ## Auth
//!
//! Every connection must present the pairing token as a `token=` query
//! parameter, checked by the pure [`check_auth`] helper (verified during the WS
//! handshake). Unauthorized connections are closed immediately. The token is
//! generated once at startup by [`random_token`].
//!
//! ## What this server does *not* do
//!
//! It only pushes snapshots *to* the client — [`serve`]'s signature has no
//! outbound channel, so inbound "follow-up" messages from the phone are not
//! forwarded anywhere yet (a future wiring step adds that). Reading them would
//! be dead work, so the per-connection task is send-only and simply ends when
//! the client disconnects (detected via a failed send).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// A live snapshot of one agent pane, serialized to the mobile client.
///
/// `state` is a short human label like `"Running"` / `"Done"` / `"Failed"`
/// (mirrors the UI's own state vocabulary). `branch` is the git branch the
/// agent is working on, if any (absent when worktree isolation is off).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Display name of the agent (e.g. `"claude"`, `"codex"`).
    pub name: String,
    /// Short lifecycle label, e.g. `"Running"`, `"Done"`, `"Failed"`.
    pub state: String,
    /// Git branch the agent is on, when known / worktree isolation is on.
    pub branch: Option<String>,
}

/// Connection info handed to the wiring layer after the server binds, so it can
/// advertise the bound port and the pairing token to the user (e.g. render a QR
/// code or print a `ws://host:port/?token=…` deep link).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// The TCP port the server actually bound to (lets the caller pass port 0
    /// and learn the OS-assigned port).
    pub port: u16,
    /// The pairing token a mobile client must present as `?token=…`.
    pub token: String,
}

/// Generate a 16-hex-char pairing token from a dependency-free entropy source.
///
/// Mixes the current monotonic nanos (folded with the PID) through a
/// splitmix64-style finalizer so that two calls a few nanoseconds apart still
/// land in widely separated output ranges. The result is exactly 16 lowercase
/// hex digits.
///
/// This is **not** cryptographically secure — it is a pairing token displayed to
/// a user standing next to the machine, which is the threat model described in
/// the Feature 10 brief. Using a CSPRNG would add a dependency for no real
/// security gain in this context.
#[must_use]
pub fn random_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Fold the u128 nanos together with the PID into a 64-bit seed.
    let mut z = (nanos as u64)
        .wrapping_add((nanos >> 64) as u64)
        .wrapping_add(std::process::id() as u64)
        ^ 0x9E37_79B9_7F4A_7C15u64;
    // splitmix64 finalizer — spreads the seed across all 64 output bits.
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9u64);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EBu64);
    z ^= z >> 31;
    format!("{z:016x}")
}

/// Return `true` when `request_path_or_query` carries a `token=VALUE` parameter
/// whose `VALUE` exactly equals `token`.
///
/// A "parameter" is bounded by the start of the string, a `?`, or a `&`, so a
/// value embedded in the *path* (e.g. `/token=evil/ws`) or in a *different*
/// parameter name (e.g. `?eviltoken=abc`) is **not** accepted — this prevents
/// the obvious auth-bypass-by-substring. Pure and dependency-free.
///
/// # Examples
///
/// Note: this crate is binary-only (no library target), so `cargo test --doc`
/// runs no doctests — these examples are documentation, not executed tests
/// (the behavior is locked in by unit tests in [`mod@crate::mobile`]'s test
/// module).
///
/// ```text
/// check_auth("/ws?token=abc", "abc")    -> true
/// check_auth("/ws?token=abc", "zzz")    -> false
/// check_auth("/ws", "abc")              -> false
/// check_auth("/ws?eviltoken=abc", "abc")-> false   // name must be `token`
/// ```
#[must_use]
pub fn check_auth(request_path_or_query: &str, token: &str) -> bool {
    // Split into individual parameter-or-path segments on `?` and `&`. Each
    // segment is at most one `k=v` pair (plus possibly a leading path for the
    // first segment), so `token=` can only match an actual `token` parameter.
    for segment in request_path_or_query.split(['?', '&']) {
        if let Some(value) = segment.strip_prefix("token=") {
            if value == token {
                return true;
            }
        }
    }
    false
}

/// Serialize a batch of snapshots to a compact JSON string.
///
/// This is the wire format pushed to every connected mobile client on each
/// update. Falls back to `"[]"` only if serialization fails — which it cannot
/// for these field types, so the fallback is purely defensive.
#[must_use]
pub fn snapshot_json(snapshots: &[AgentSnapshot]) -> String {
    serde_json::to_string(snapshots).unwrap_or_else(|_| "[]".to_string())
}

/// Run the mobile companion WebSocket server.
///
/// Binds a [`TcpListener`] on `addr`, then repeatedly selects between two
/// events:
///
/// - **a new snapshot arrives on `state_rx`** → store it as the latest and
///   broadcast it to every connected client;
/// - **a new TCP connection arrives** → spawn a per-connection task that
///   upgrades it to a WebSocket, checks auth, and forwards snapshots.
///
/// Returns `Ok(())` when `state_rx` closes (the app shut down its producer →
/// graceful shutdown) and propagates bind/accept errors via `?`.
///
/// `token` is checked against each connection's `?token=` query during the WS
/// handshake (via [`check_auth`]); unauthorized connections are closed.
///
/// Each client receives the **latest** snapshot immediately on connect, then
/// every subsequent update as a `Text` message containing
/// [`snapshot_json`] output.
///
/// # Errors
///
/// Returns an error only if binding the listener fails or `accept`ing a
/// connection fails (propagated via [`anyhow`]). Per-connection errors are
/// logged-and-dropped: one bad client never takes down the server.
pub async fn serve(
    addr: SocketAddr,
    token: String,
    mut state_rx: mpsc::UnboundedReceiver<Vec<AgentSnapshot>>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    // Fan-out: every connected client subscribes to this broadcast. Capacity is
    // intentionally small — snapshots are tiny and frequent; a lagged client
    // just resyncs to the next full snapshot (see `Lagged` handling below).
    let (snap_tx, _) = broadcast::channel::<Vec<AgentSnapshot>>(64);
    let snap_tx = Arc::new(snap_tx);

    // The latest snapshot, handed to a freshly-connected client before it has
    // received any broadcast (so it never renders an empty screen). Guarded by
    // a std mutex — it is only touched for a trivial clone, never held across
    // an await.
    let latest: Arc<Mutex<Vec<AgentSnapshot>>> = Arc::new(Mutex::new(Vec::new()));

    loop {
        tokio::select! {
            // New state from the app: publish to all subscribers + remember as
            // the latest for late-joining clients.
            update = state_rx.recv() => match update {
                Some(snapshots) => {
                    *latest.lock().expect("latest lock poisoned") = snapshots.clone();
                    // send() errs only if there are no subscribers — fine to drop.
                    let _ = snap_tx.send(snapshots);
                }
                None => {
                    // Producer closed state_rx → graceful shutdown.
                    return Ok(());
                }
            },
            // New incoming connection.
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                let token = token.clone();
                let snap_rx = snap_tx.subscribe();
                let latest = Arc::clone(&latest);
                tokio::spawn(async move {
                    handle_connection(stream, token, latest, snap_rx).await;
                });
            }
        }
    }
}

/// Per-connection handler: upgrade to WS, check auth, push the latest snapshot,
/// then forward every broadcast update until the client goes away.
///
/// Factored out of [`serve`] so the accept loop stays readable. Any error
/// (handshake failure, send failure, client disconnect) just ends the task —
/// the server keeps running.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    token: String,
    latest: Arc<Mutex<Vec<AgentSnapshot>>>,
    mut snap_rx: broadcast::Receiver<Vec<AgentSnapshot>>,
) {
    // Capture the request's path+query during the handshake so we can auth the
    // client against `?token=`. The callback is `FnOnce`, invoked exactly once
    // by tungstenite during the upgrade; we stash the captured string through a
    // shared `Arc<Mutex<Option<String>>>` (the only value the closure can hand
    // back out, since it must return the HTTP `Response`).
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_cb = Arc::clone(&captured);
    let mut ws_stream = match accept_hdr_async(stream, move |req: &Request, resp: Response| {
        let pq = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| req.uri().path().to_owned());
        if let Ok(mut slot) = captured_cb.lock() {
            *slot = Some(pq);
        }
        Ok(resp)
    })
    .await
    {
        Ok(ws) => ws,
        Err(_) => return,
    };

    // Auth: the captured path+query must carry the right token.
    let req_path = captured
        .lock()
        .map(|mut slot| slot.take().unwrap_or_default())
        .unwrap_or_default();
    if !check_auth(&req_path, &token) {
        // Unauthorized — close immediately. `close` is the one inherent WS send
        // method, so this needs no extra extension trait.
        let _ = ws_stream.close(None).await;
        return;
    }

    let mut ws = ws_stream;

    // On connect, push the latest snapshot so the client doesn't render empty
    // until the next broadcast.
    let init = latest.lock().map(|guard| guard.clone()).unwrap_or_default();
    if ws.send(Message::Text(snapshot_json(&init))).await.is_err() {
        return;
    }

    // Forward every broadcast update to this client until it disconnects.
    loop {
        match snap_rx.recv().await {
            Ok(snapshots) => {
                if ws
                    .send(Message::Text(snapshot_json(&snapshots)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // The client fell behind (slow phone / app backgrounded). Skip
                // the missed updates; the very next broadcast is a full
                // snapshot, so resync is automatic.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    // Best-effort graceful close; ignore errors (client may already be gone).
    let _ = ws.close(None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_16_hex_chars_and_nonempty() {
        let t = random_token();
        assert!(!t.is_empty(), "token must not be empty");
        assert_eq!(
            t.len(),
            16,
            "token must be exactly 16 chars, got {t:?} (len {})",
            t.len()
        );
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "token must be lowercase hex, got {t:?}"
        );
    }

    #[test]
    fn random_token_is_likely_unique_across_calls() {
        // Not crypto-strong, but two rapid calls should differ (the nanos
        // counter advances and the splitmix finalizer spreads even tiny input
        // deltas across the whole output range).
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b, "two rapid tokens should differ: {a} vs {b}");
    }

    #[test]
    fn check_auth_matches_exact_token() {
        assert!(check_auth("/ws?token=abc", "abc"));
        assert!(check_auth("token=abc", "abc"));
        assert!(check_auth("/?token=abc", "abc"));
        assert!(check_auth("/ws?a=1&token=abc&b=2", "abc"));
    }

    #[test]
    fn check_auth_rejects_mismatched_token() {
        assert!(!check_auth("/ws?token=abc", "zzz"));
        assert!(!check_auth("/ws?token=", "abc"));
        assert!(
            !check_auth("/ws?token=ABC", "abc"),
            "comparison is case-sensitive"
        );
    }

    #[test]
    fn check_auth_rejects_absent_token() {
        assert!(!check_auth("/ws", "abc"));
        assert!(!check_auth("", "abc"));
        assert!(!check_auth("/ws?other=abc", "abc"));
        assert!(!check_auth("/ws?a=1&b=2", "abc"));
    }

    #[test]
    fn check_auth_is_not_bypassed_by_substring_param_name() {
        // A parameter literally named `eviltoken` must NOT authenticate even
        // when its value matches — the name must be exactly `token`.
        assert!(!check_auth("/ws?eviltoken=abc", "abc"));
        assert!(!check_auth("/ws?notoken=abc", "abc"));
        // And `token=` embedded in the path must not count either.
        assert!(!check_auth("/token=abc/ws", "abc"));
    }

    #[test]
    fn snapshot_json_round_trips() {
        let snaps = vec![
            AgentSnapshot {
                name: "claude".into(),
                state: "Running".into(),
                branch: Some("feat/x".into()),
            },
            AgentSnapshot {
                name: "codex".into(),
                state: "Done".into(),
                branch: None,
            },
        ];
        let json = snapshot_json(&snaps);
        let back: Vec<AgentSnapshot> =
            serde_json::from_str(&json).expect("round-trip must deserialize");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "claude");
        assert_eq!(back[0].state, "Running");
        assert_eq!(back[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(back[1].name, "codex");
        assert_eq!(back[1].state, "Done");
        assert!(back[1].branch.is_none());
    }

    #[test]
    fn snapshot_json_empty_is_brackets() {
        assert_eq!(snapshot_json(&[]), "[]");
    }

    #[test]
    fn agent_snapshot_serializes_expected_fields() {
        let snap = AgentSnapshot {
            name: "opencode".into(),
            state: "Failed".into(),
            branch: Some("main".into()),
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        // Field names and values present, in a stable shape.
        assert!(
            json.contains("\"name\":\"opencode\""),
            "missing name field: {json}"
        );
        assert!(
            json.contains("\"state\":\"Failed\""),
            "missing state field: {json}"
        );
        assert!(
            json.contains("\"branch\":\"main\""),
            "missing branch field: {json}"
        );
        // branch is Option, so `null` must appear when None.
        let none_branch = AgentSnapshot {
            name: "x".into(),
            state: "Running".into(),
            branch: None,
        };
        assert!(
            serde_json::to_string(&none_branch)
                .unwrap()
                .contains("\"branch\":null"),
            "None branch must serialize to null"
        );
    }

    // ---- WebSocket integration tests for `serve` ----
    //
    // These drive the real `serve` server over a real loopback TCP socket with
    // a real `tokio_tungstenite` client. Every await is bounded by an explicit
    // `tokio::time::timeout` so a bug can never hang the suite. The crate is a
    // library (see `src/lib.rs`: `pub mod mobile;`), so these tests reach
    // `serve`/`AgentSnapshot`/`Message` unqualified via `use super::*`.

    use futures_util::StreamExt;
    use std::time::Duration;
    use tokio_tungstenite::connect_async;

    /// Bind an ephemeral loopback port, read its address, then drop the listener
    /// so `serve` can rebind it.
    ///
    /// There is a tiny window between the drop and `serve`'s rebind where
    /// another process could in theory grab the port; in practice that never
    /// happens on the loopback test runner and keeps the test dependency-free
    /// (no port-allocator helper).
    async fn bind_ephemeral() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral listener");
        let addr = listener.local_addr().expect("read ephemeral local_addr");
        drop(listener);
        addr
    }

    #[tokio::test]
    async fn serve_broadcasts_snapshots_to_authorized_client() {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<AgentSnapshot>>();
        let token = "testtoken123".to_string();
        let addr = bind_ephemeral().await;

        // Spawn the server. `serve` returns Ok(()) only once `tx` is dropped
        // (graceful shutdown), which we trigger at the end of the test.
        // `addr` is `Copy` so it survives the `async move`; `token` is not, so
        // we hand the server its own owned clone and keep `token` for the URL.
        let server_token = token.clone();
        let handle = tokio::spawn(async move { serve(addr, server_token, rx).await });

        // Publish a snapshot BEFORE connecting so `serve`'s `latest` is
        // populated and the very first push to the client is the real data.
        tx.send(vec![AgentSnapshot {
            name: "claude".into(),
            state: "Running".into(),
            branch: None,
        }])
        .expect("publish initial snapshot");
        // Yield a beat so `serve`'s select loop stores the snapshot as `latest`
        // before the client's handshake races it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect an authorized WS client. Bounded by a timeout so a broken
        // server fails the test instead of hanging it.
        let url = format!("ws://{addr}/?token={token}");
        let (mut ws, _resp) = tokio::time::timeout(Duration::from_secs(2), connect_async(url))
            .await
            .expect("client connect timed out")
            .expect("client connect succeeded");

        // The first message must be the initial snapshot push (Text JSON).
        // `ws.next()` yields `Option<Result<Message, Error>>`, so we unwrap
        // the timeout, the Option, and the Result in turn.
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("receiving first message timed out")
            .expect("stream produced a message before closing")
            .expect("ws read must not error");
        let parsed: Vec<AgentSnapshot> = match msg {
            Message::Text(t) => serde_json::from_str(&t).expect("snapshot JSON parses"),
            other => panic!("expected a Text snapshot, got {other:?}"),
        };
        assert_eq!(parsed.len(), 1, "exactly one snapshot was published");
        assert_eq!(parsed[0].name, "claude");
        assert_eq!(parsed[0].state, "Running");
        assert!(
            parsed[0].branch.is_none(),
            "branch was None on the wire, must round-trip as None"
        );

        // Clean shutdown: drop the client + producer so `serve` returns Ok(()).
        drop(ws);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("serve must shut down promptly after the producer drops")
            .expect("serve task must not panic")
            .expect("serve must return Ok(()) on graceful shutdown");
    }

    #[tokio::test]
    async fn serve_rejects_unauthorized_client() {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<AgentSnapshot>>();
        let token = "goodtoken".to_string();
        let addr = bind_ephemeral().await;
        let handle = tokio::spawn(async move { serve(addr, token, rx).await });

        // Hand the server a snapshot so an authorized path *would* push one —
        // this makes the "no snapshot reaches the bad client" assertion
        // meaningful rather than vacuously true.
        tx.send(vec![AgentSnapshot {
            name: "claude".into(),
            state: "Running".into(),
            branch: None,
        }])
        .expect("publish snapshot");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect with the WRONG token. The WS handshake callback always
        // returns Ok (it only captures the path), so `connect_async` itself
        // succeeds; auth is enforced AFTER the upgrade, after which the server
        // immediately closes the stream. If the upgrade were refused outright,
        // that also counts as "rejected" — accept it and pass.
        let url = format!("ws://{addr}/?token=wrong");
        // `connect` is `Result<Result<(_, _), tungstenite::Error>, Elapsed>`:
        // outer = timeout, inner = the WS handshake. The handshake callback
        // always returns Ok (it only captures the path), so a timeout or an
        // inner error here means the connection was rejected/aborted — both
        // count as "unauthorized" and let the test pass early.
        let connect = tokio::time::timeout(Duration::from_secs(2), connect_async(url)).await;
        let mut ws = match connect {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(_)) | Err(_) => {
                drop(tx);
                let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
                return; // upgrade rejected / timed out — unauthorized, as expected.
            }
        };

        // For up to 1s, no Text snapshot may arrive. A Close frame or stream
        // end (None) means the server rejected us — pass. Any Text message is
        // a leak and must fail.
        let deadline = Duration::from_secs(1);
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() >= deadline {
                break; // no snapshot within the window -> rejected as expected.
            }
            let remaining = deadline
                .checked_sub(start.elapsed())
                .unwrap_or(Duration::ZERO);
            match tokio::time::timeout(remaining, ws.next()).await {
                Err(_elapsed) => break,    // timed out without a snapshot.
                Ok(None) => break,         // stream closed without a snapshot.
                Ok(Some(Err(_))) => break, // read error — treat as rejected.
                Ok(Some(Ok(Message::Text(t)))) => {
                    panic!("unauthorized client must not receive a snapshot, got: {t}");
                }
                Ok(Some(Ok(_))) => continue, // Ping/Pong/Close — keep draining.
            }
        }

        drop(ws);
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
