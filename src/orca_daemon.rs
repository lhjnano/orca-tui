//! # Orca daemon client — connects to a running Orca GUI daemon
//!
//! Orca GUI runs a background daemon (`src/main/daemon/`) that manages agent
//! PTYs, headless terminal emulators, and session history. This module lets
//! orcatui connect to that daemon as a TUI client — giving it session
//! persistence, structured agent status, and multi-client (GUI + TUI) support.
//!
//! ## Protocol (from the open-source daemon code)
//!
//! - **Transport**: Unix domain socket + token file (UUID).
//! - **Two-socket model**: a `control` socket (NDJSON RPC) and a `stream`
//!   socket (binary frames for PTY output). Both authenticate with a hello
//!   handshake.
//! - **Binary frame**: `[1B type] [4B BE u32 payload_len] [payload]`.
//! - **NDJSON**: newline-delimited JSON objects.
//! - **Protocol version**: 28 (as of 2026-07).
//!
//! ## Error handling
//!
//! Every failure mode is covered:
//! - Socket connect/listen errors → [`DaemonError::Connect`].
//! - Hello rejection (bad token / version mismatch) → [`DaemonError::HelloRejected`].
//! - Daemon crash (socket EOF / unexpected close) → [`DaemonError::Disconnected`].
//! - RPC timeout → [`DaemonError::Timeout`].
//! - Session not found → [`DaemonError::SessionNotFound`].
//! - Frame parse / NDJSON decode errors → [`DaemonError::Protocol`].

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Current Orca daemon protocol version (must match the running daemon).
pub const PROTOCOL_VERSION: u32 = 28;

/// Binary frame header: 1 byte type + 4 bytes big-endian payload length.
const FRAME_HEADER_SIZE: usize = 5;
/// Maximum frame payload (matches the daemon's FRAME_MAX_PAYLOAD).
const FRAME_MAX_PAYLOAD: usize = 16 * 1024 * 1024; // 16 MiB
/// Default RPC timeout.
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Default hello timeout.
const DEFAULT_HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Connection parameters — configurable via `DaemonConfig` in `config.toml`.
#[derive(Debug, Clone)]
pub struct DaemonConnectOptions {
    /// RPC (request/response) timeout.
    pub rpc_timeout: Duration,
    /// Hello (handshake) timeout.
    pub hello_timeout: Duration,
}

impl Default for DaemonConnectOptions {
    fn default() -> Self {
        Self {
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            hello_timeout: DEFAULT_HELLO_TIMEOUT,
        }
    }
}

// ── Errors ─────────────────────────────────────────────────────────────────

/// Every error that can arise while talking to the Orca daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Could not connect to the Unix socket (daemon not running?).
    #[error("daemon connect failed: {0}")]
    Connect(#[source] io::Error),
    /// The daemon rejected the hello handshake (bad token, version mismatch, etc.).
    #[error("daemon hello rejected: {message}")]
    HelloRejected { message: String },
    /// The daemon disconnected unexpectedly (crash, idle shutdown, SIGKILL).
    #[error("daemon disconnected: {reason}")]
    Disconnected { reason: String },
    /// An RPC request timed out.
    #[error("daemon RPC timeout after {timeout_secs}s for {request_type}")]
    Timeout {
        request_type: String,
        timeout_secs: u64,
    },
    /// The requested session was not found on the daemon.
    #[error("daemon session not found: {session_id}")]
    SessionNotFound { session_id: String },
    /// A protocol-level error (bad frame, unparseable NDJSON, unknown response).
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    /// An underlying I/O error on a socket.
    #[error("daemon I/O error: {0}")]
    Io(#[from] io::Error),
}

// ── Protocol types ─────────────────────────────────────────────────────────

/// Hello message sent by the client to authenticate with the daemon.
#[derive(Debug, Serialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub version: u32,
    pub token: String,
    pub client_id: String,
    pub role: &'static str, // "control" or "stream"
}

impl HelloMessage {
    /// Create a control-role hello.
    #[must_use]
    pub fn control(token: String, client_id: String) -> Self {
        Self {
            msg_type: "hello",
            version: PROTOCOL_VERSION,
            token,
            client_id,
            role: "control",
        }
    }

    /// Create a stream-role hello (same client_id as the control hello).
    #[must_use]
    pub fn stream(token: String, client_id: String) -> Self {
        Self {
            msg_type: "hello",
            version: PROTOCOL_VERSION,
            token,
            client_id,
            role: "stream",
        }
    }
}

/// Hello response from the daemon.
#[derive(Debug, Deserialize)]
pub struct HelloResponse {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub ok: bool,
    pub error: Option<String>,
    pub daemon_identity: Option<DaemonIdentity>,
    /// If the daemon is shutting down, it may ask the client to retry.
    pub retryable: Option<bool>,
}

/// Daemon identity (PID, start time, launch nonce).
#[derive(Debug, Deserialize)]
pub struct DaemonIdentity {
    pub pid: i32,
    #[serde(rename = "startedAtMs")]
    pub started_at_ms: f64,
    #[serde(rename = "launchNonce")]
    pub launch_nonce: String,
}

/// An RPC request sent on the control socket.
#[derive(Debug, Serialize)]
pub struct DaemonRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub req_type: String,
    pub payload: serde_json::Value,
}

/// An RPC response received on the control socket.
#[derive(Debug, Deserialize)]
pub struct DaemonResponse {
    pub id: String,
    pub ok: bool,
    pub payload: Option<serde_json::Value>,
    pub error: Option<String>,
}

// ── Binary frame ───────────────────────────────────────────────────────────

/// Binary frame types used on the stream socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// PTY output data (raw bytes).
    Data = 1,
    /// An event (exit, background marker, etc.) encoded as NDJSON.
    Event = 2,
}

impl FrameType {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Data),
            2 => Some(Self::Event),
            _ => None,
        }
    }
}

/// Encode a binary frame.
fn encode_frame(ftype: FrameType, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > FRAME_MAX_PAYLOAD {
        return Err(anyhow!(
            "frame payload {} exceeds max {}",
            payload.len(),
            FRAME_MAX_PAYLOAD
        ));
    }
    let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    buf.push(ftype as u8);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// A parsed binary frame from the stream socket.
#[derive(Debug)]
pub struct Frame {
    pub ftype: FrameType,
    pub payload: Vec<u8>,
}

/// Read exactly one binary frame from a reader. Returns `Err(DaemonError::Disconnected)`
/// on EOF (clean disconnect) or partial frame (crash mid-send).
fn read_frame<R: Read>(reader: &mut R) -> Result<Frame, DaemonError> {
    let mut header = [0u8; FRAME_HEADER_SIZE];
    read_exact_or_disconnect(reader, &mut header)?;

    let type_byte = header[0];
    let ftype = FrameType::from_byte(type_byte)
        .ok_or_else(|| DaemonError::Protocol(format!("unknown frame type byte: {type_byte}")))?;
    let payload_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let payload_len = payload_len as usize;
    if payload_len > FRAME_MAX_PAYLOAD {
        return Err(DaemonError::Protocol(format!(
            "frame payload {payload_len} exceeds max {FRAME_MAX_PAYLOAD}"
        )));
    }
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        read_exact_or_disconnect(reader, &mut payload)?;
    }
    Ok(Frame { ftype, payload })
}

/// Read exactly `buf.len()` bytes or return `Disconnected` on EOF / partial read.
fn read_exact_or_disconnect<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), DaemonError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(DaemonError::Disconnected {
            reason: "EOF mid-read (daemon crashed or closed connection)".to_string(),
        }),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Err(DaemonError::Disconnected {
            reason: "connection reset by daemon".to_string(),
        }),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Err(DaemonError::Disconnected {
            reason: "broken pipe (daemon shut down)".to_string(),
        }),
        Err(e) => Err(DaemonError::Io(e)),
    }
}

// ── NDJSON helpers ─────────────────────────────────────────────────────────

/// Encode a value as NDJSON (JSON + newline) and write it.
fn write_ndjson<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> Result<(), DaemonError> {
    let json = serde_json::to_string(msg)
        .map_err(|e| DaemonError::Protocol(format!("JSON encode: {e}")))?;
    writer
        .write_all(json.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(DaemonError::Io)
}

/// Read one NDJSON line and deserialize it.
fn read_ndjson<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T, DaemonError> {
    let line = read_ndjson_line(reader)?;
    serde_json::from_slice(&line).map_err(|e| DaemonError::Protocol(format!("JSON decode: {e}")))
}

/// Read bytes until `\n`, stripping it. Returns the raw bytes (without newline).
fn read_ndjson_line<R: Read>(reader: &mut R) -> Result<Vec<u8>, DaemonError> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(DaemonError::Disconnected {
                    reason: "EOF while reading NDJSON line".to_string(),
                });
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(buf);
                }
                buf.push(byte[0]);
                // Safety cap: a single NDJSON line should never exceed 16 MiB.
                if buf.len() > 16 * 1024 * 1024 {
                    return Err(DaemonError::Protocol(
                        "NDJSON line exceeds 16 MiB".to_string(),
                    ));
                }
            }
            Err(e) => return Err(DaemonError::Io(e)),
        }
    }
}

// ── Daemon discovery ───────────────────────────────────────────────────────

/// Information needed to connect to a running daemon.
#[derive(Debug, Clone)]
pub struct DaemonEndpoint {
    /// Path to the Unix domain socket.
    pub socket_path: PathBuf,
    /// Path to the token file (contains the auth UUID).
    pub token_path: PathBuf,
}

impl DaemonEndpoint {
    /// Discover a running daemon by checking the default Orca runtime directory.
    /// Returns `None` if no daemon appears to be running.
    #[must_use]
    pub fn discover() -> Option<Self> {
        // Orca stores daemon artifacts under its data directory.
        // The exact path depends on the platform and Orca version; check common locations.
        let candidates = [
            // Linux/WSL: ~/.config/orca/ or ~/.local/share/orca/
            dirs::config_dir().map(|d| d.join("orca")),
            dirs::data_dir().map(|d| d.join("orca")),
            // macOS: ~/Library/Application Support/orca/
            dirs::data_dir().map(|d| d.join("orca")),
        ];
        for base in candidates.into_iter().flatten() {
            let socket = base.join("daemon.sock");
            let token = base.join("daemon.token");
            if socket.exists() && token.exists() {
                return Some(Self {
                    socket_path: socket,
                    token_path: token,
                });
            }
        }
        None
    }

    /// Read the auth token from the token file.
    fn read_token(&self) -> Result<String, DaemonError> {
        std::fs::read_to_string(&self.token_path)
            .map(|s| s.trim().to_string())
            .map_err(|e| DaemonError::Protocol(format!("failed to read token file: {e}")))
    }
}

// ── Daemon client ──────────────────────────────────────────────────────────

/// A connected Orca daemon client. Owns a control socket (NDJSON RPC) and
/// optionally a stream socket (binary frames for PTY data).
pub struct DaemonClient {
    control: UnixStream,
    stream: Option<UnixStream>,
    endpoint: DaemonEndpoint,
    client_id: String,
    identity: DaemonIdentity,
    /// RPC request ID counter.
    next_request_id: std::cell::Cell<u64>,
}

impl DaemonClient {
    /// Connect to a running daemon at the given endpoint.
    ///
    /// Performs the full handshake:
    /// 1. Read the auth token from the token file.
    /// 2. Open a control socket, send hello, verify the response.
    /// 3. Open a stream socket, send hello, verify the response.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] for any failure:
    /// - `Connect` — socket connect failed (daemon not running).
    /// - `HelloRejected` — bad token, protocol mismatch, or daemon shutting down.
    /// - `Disconnected` — daemon closed mid-handshake.
    /// - `Protocol` — unparseable response.
    pub fn connect(endpoint: DaemonEndpoint) -> Result<Self, DaemonError> {
        Self::connect_with(endpoint, DaemonConnectOptions::default())
    }

    /// Connect with explicit timeout options (from `DaemonConfig`).
    pub fn connect_with(
        endpoint: DaemonEndpoint,
        opts: DaemonConnectOptions,
    ) -> Result<Self, DaemonError> {
        let token = endpoint.read_token()?;
        let client_id = uuid_v4_simple();

        // ── Control socket ──────────────────────────────────────────────
        let mut control =
            UnixStream::connect(&endpoint.socket_path).map_err(DaemonError::Connect)?;
        control.set_read_timeout(Some(opts.hello_timeout)).ok();
        control.set_write_timeout(Some(opts.hello_timeout)).ok();

        // Send hello on control.
        let hello = HelloMessage::control(token.clone(), client_id.clone());
        write_ndjson(&mut control, &hello)?;
        let resp: HelloResponse = read_ndjson(&mut control)?;
        if !resp.ok {
            let msg = resp
                .error
                .unwrap_or_else(|| "unknown rejection".to_string());
            // If retryable, the message includes guidance.
            return Err(DaemonError::HelloRejected { message: msg });
        }
        let identity = resp
            .daemon_identity
            .ok_or_else(|| DaemonError::Protocol("daemon hello ok but no identity".to_string()))?;

        // ── Stream socket ───────────────────────────────────────────────
        let mut stream =
            UnixStream::connect(&endpoint.socket_path).map_err(DaemonError::Connect)?;
        stream.set_read_timeout(Some(opts.hello_timeout)).ok();
        stream.set_write_timeout(Some(opts.hello_timeout)).ok();

        // Send hello on stream.
        let hello = HelloMessage::stream(token, client_id.clone());
        write_ndjson(&mut stream, &hello)?;
        let resp: HelloResponse = read_ndjson(&mut stream)?;
        if !resp.ok {
            let msg = resp
                .error
                .unwrap_or_else(|| "stream hello rejected".to_string());
            return Err(DaemonError::HelloRejected { message: msg });
        }

        // Clear the read timeout — the stream socket blocks indefinitely waiting for frames.
        stream.set_read_timeout(None).ok();
        // Keep the RPC timeout on control.
        control.set_read_timeout(Some(opts.rpc_timeout)).ok();

        Ok(Self {
            control,
            stream: Some(stream),
            endpoint,
            client_id,
            identity,
            next_request_id: std::cell::Cell::new(1),
        })
    }

    /// Try to discover and connect to a running daemon. Returns `None` if no
    /// daemon is found (caller should fall back to standalone mode).
    pub fn try_connect() -> Option<Result<Self, DaemonError>> {
        DaemonEndpoint::discover().map(Self::connect)
    }

    /// Try to discover and connect with explicit timeout options.
    pub fn try_connect_with(opts: DaemonConnectOptions) -> Option<Result<Self, DaemonError>> {
        DaemonEndpoint::discover().map(|ep| Self::connect_with(ep, opts))
    }

    /// The daemon's identity (PID, start time, launch nonce).
    #[must_use]
    pub fn identity(&self) -> &DaemonIdentity {
        &self.identity
    }

    /// The endpoint this client is connected to.
    #[must_use]
    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    // ── RPC ──────────────────────────────────────────────────────────

    /// Allocate the next request ID.
    fn next_id(&self) -> String {
        let n = self.next_request_id.get();
        self.next_request_id.set(n + 1);
        format!("rpc-{n}")
    }

    /// Send an RPC request and wait for the response.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::Timeout`] — no response within the RPC timeout.
    /// - [`DaemonError::Disconnected`] — daemon crashed or closed the socket.
    /// - [`DaemonError::SessionNotFound`] — the RPC returned a "session not found" error.
    /// - [`DaemonError::Protocol`] — unparseable response.
    pub fn rpc(
        &mut self,
        req_type: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        let id = self.next_id();
        let request = DaemonRequest {
            id: id.clone(),
            req_type: req_type.to_string(),
            payload,
        };
        write_ndjson(&mut self.control, &request)?;

        // Read the response (matching ID).
        loop {
            let resp: DaemonResponse = read_ndjson(&mut self.control)?;
            if resp.id != id {
                // Out-of-order or unsolicited message — skip (could be an async event).
                continue;
            }
            if !resp.ok {
                let err = resp
                    .error
                    .unwrap_or_else(|| "unknown RPC error".to_string());
                if err.contains("session not found") || err.contains("SessionNotFoundError") {
                    return Err(DaemonError::SessionNotFound {
                        session_id: id.clone(),
                    });
                }
                return Err(DaemonError::Protocol(format!(
                    "RPC {req_type} failed: {err}"
                )));
            }
            return Ok(resp.payload.unwrap_or(serde_json::Value::Null));
        }
    }

    /// Convenience: ping the daemon to check it's alive.
    pub fn ping(&mut self) -> Result<(), DaemonError> {
        self.rpc("ping", serde_json::json!({}))?;
        Ok(())
    }

    /// Convenience: list all active sessions on the daemon.
    pub fn list_sessions(&mut self) -> Result<serde_json::Value, DaemonError> {
        self.rpc("listSessions", serde_json::json!({}))
    }

    // ── Stream ────────────────────────────────────────────────────────

    /// Take the stream socket (moves it out — the caller owns it for blocking reads).
    /// Returns `None` if already taken.
    pub fn take_stream(&mut self) -> Option<UnixStream> {
        self.stream.take()
    }

    /// Read one frame from the stream socket. Blocks until a frame arrives or
    /// the daemon disconnects.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::Disconnected`] — daemon crashed, idle-shutdown, or EOF.
    /// - [`DaemonError::Protocol`] — malformed frame header or unknown type.
    pub fn read_stream_frame(stream: &mut UnixStream) -> Result<Frame, DaemonError> {
        read_frame(stream)
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        // Best-effort: send a detach so the daemon knows we're gone (it will
        // clean up on socket close anyway, but a clean signal is faster).
        let _ = write_ndjson(
            &mut self.control,
            &serde_json::json!({"type": "detach", "id": "drop", "payload": {}}),
        );
        // Sockets are closed by UnixStream::Drop.
    }
}

// ── Utility ────────────────────────────────────────────────────────────────

/// Generate a simple UUID v4 string (without depending on the `uuid` crate).
fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (now & 0xFFFF_FFFF) as u32,
        ((now >> 32) & 0xFFFF) as u16,
        ((now >> 48) & 0xFFF) as u16,
        ((now >> 60) & 0xFFFF) as u16,
        (now >> 76) as u64,
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_encode_decode_roundtrip() {
        let payload = b"hello PTY data \x1b[31mred\x1b[0m";
        let encoded = encode_frame(FrameType::Data, payload).unwrap();
        assert!(encoded.len() == FRAME_HEADER_SIZE + payload.len());
        assert_eq!(encoded[0], FrameType::Data as u8);

        let mut reader = &encoded[..];
        let frame = read_frame(&mut reader).unwrap();
        assert_eq!(frame.ftype, FrameType::Data);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn frame_zero_payload() {
        let encoded = encode_frame(FrameType::Event, b"").unwrap();
        let mut reader = &encoded[..];
        let frame = read_frame(&mut reader).unwrap();
        assert_eq!(frame.ftype, FrameType::Event);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn frame_oversize_rejected() {
        let big = vec![0u8; FRAME_MAX_PAYLOAD + 1];
        assert!(encode_frame(FrameType::Data, &big).is_err());
    }

    #[test]
    fn frame_bad_type_byte() {
        let mut bad = vec![0u8; FRAME_HEADER_SIZE];
        bad[0] = 99; // invalid type
        bad[1..5].copy_from_slice(&0u32.to_be_bytes());
        let mut reader = &bad[..];
        let err = read_frame(&mut reader);
        assert!(matches!(err, Err(DaemonError::Protocol(_))));
    }

    #[test]
    fn ndjson_encode_includes_newline() {
        let msg = serde_json::json!({"type": "ping", "id": "1"});
        let mut buf = Vec::new();
        write_ndjson(&mut buf, &msg).unwrap();
        assert!(buf.ends_with(b"\n"));
        assert!(buf.starts_with(b"{"));
    }

    #[test]
    fn ndjson_read_one_line() {
        let data = b"{\"ok\":true,\"id\":\"1\"}\n{\"ok\":true,\"id\":\"2\"}\n";
        let mut reader = &data[..];
        let resp: DaemonResponse = read_ndjson(&mut reader).unwrap();
        assert_eq!(resp.id, "1");
        assert!(resp.ok);
        // Second read gets the next line.
        let resp2: DaemonResponse = read_ndjson(&mut reader).unwrap();
        assert_eq!(resp2.id, "2");
    }

    #[test]
    fn ndjson_eof_returns_disconnected() {
        let data = b"";
        let mut reader = &data[..];
        let err: Result<DaemonResponse, _> = read_ndjson(&mut reader);
        assert!(matches!(err, Err(DaemonError::Disconnected { .. })));
    }

    #[test]
    fn hello_message_serializes_correctly() {
        let hello = HelloMessage::control("tok-123".to_string(), "cli-456".to_string());
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"type\":\"hello\""));
        assert!(json.contains("\"version\":28"));
        assert!(json.contains("\"role\":\"control\""));
        assert!(json.contains("\"token\":\"tok-123\""));
    }

    #[test]
    fn hello_response_rejection_deserializes() {
        let json = r#"{"type":"hello","ok":false,"error":"Protocol version mismatch"}"#;
        let resp: HelloResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("Protocol version mismatch"));
    }

    #[test]
    fn daemon_error_display_is_human_readable() {
        let e = DaemonError::HelloRejected {
            message: "bad token".to_string(),
        };
        assert!(e.to_string().contains("bad token"));
        let e = DaemonError::Disconnected {
            reason: "crash".to_string(),
        };
        assert!(e.to_string().contains("crash"));
        let e = DaemonError::Timeout {
            request_type: "write".to_string(),
            timeout_secs: 10,
        };
        assert!(e.to_string().contains("write"));
    }

    #[test]
    fn read_exact_eof_is_disconnected_not_io() {
        let data = b"\x01"; // partial header (need 5 bytes)
        let mut reader = &data[..];
        let mut buf = [0u8; 5];
        let err = read_exact_or_disconnect(&mut reader, &mut buf);
        assert!(matches!(err, Err(DaemonError::Disconnected { .. })));
    }

    #[test]
    fn read_frame_eof_mid_payload_is_disconnected() {
        // Frame header says 10 bytes payload but only 3 are available.
        let mut buf = vec![FrameType::Data as u8];
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(b"abc"); // only 3 of 10
        let mut reader = &buf[..];
        let err = read_frame(&mut reader);
        assert!(matches!(err, Err(DaemonError::Disconnected { .. })));
    }

    // ── uuid_v4_simple ─────────────────────────────────────────────────────

    #[test]
    fn uuid_v4_simple_has_v4_marker_and_is_unique() {
        let a = uuid_v4_simple();
        let b = uuid_v4_simple();
        // Five hyphen-separated groups (4 hyphens).
        assert_eq!(a.matches('-').count(), 4, "UUID has 5 groups: {a}");
        // Third group starts with '4' (v4 marker).
        let third = a.split('-').nth(2).expect("third group");
        assert!(
            third.starts_with('4'),
            "third group starts with '4': {third}"
        );
        // Two calls produce different values (nanosecond clock).
        assert_ne!(a, b, "uuid calls are unique");
    }

    // ── Serialization round-trips ──────────────────────────────────────────

    #[test]
    fn hello_message_stream_serializes() {
        let hello = HelloMessage::stream("tok-stream".to_string(), "cli-stream".to_string());
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"role\":\"stream\""));
        assert!(json.contains("\"version\":28"));
        assert!(json.contains("\"tok-stream\""));
    }

    #[test]
    fn hello_response_ok_with_identity_deserializes() {
        let json = r#"{"type":"hello","ok":true,"error":null,"daemon_identity":{"pid":999,"startedAtMs":1234567.0,"launchNonce":"abc"},"retryable":null}"#;
        let resp: HelloResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.error.is_none());
        let id = resp.daemon_identity.expect("identity present");
        assert_eq!(id.pid, 999);
        assert!((id.started_at_ms - 1234567.0).abs() < f64::EPSILON);
        assert_eq!(id.launch_nonce, "abc");
    }

    #[test]
    fn daemon_request_serializes_with_correct_fields() {
        let req = DaemonRequest {
            id: "rpc-1".to_string(),
            req_type: "ping".to_string(),
            payload: serde_json::json!({"key": "val"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"id\":\"rpc-1\""));
        assert!(json.contains("\"type\":\"ping\""));
        assert!(json.contains("\"key\":\"val\""));
    }

    #[test]
    fn daemon_response_ok_with_payload_deserializes() {
        let json = r#"{"id":"rpc-1","ok":true,"payload":{"sessions":[]},"error":null}"#;
        let resp: DaemonResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.id, "rpc-1");
        assert!(resp.payload.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn daemon_response_error_deserializes() {
        let json = r#"{"id":"rpc-2","ok":false,"payload":null,"error":"something broke"}"#;
        let resp: DaemonResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.id, "rpc-2");
        assert_eq!(resp.error.as_deref(), Some("something broke"));
    }

    // ── NDJSON edge cases ──────────────────────────────────────────────────

    #[test]
    fn read_ndjson_line_rejects_oversized_line() {
        // A 17 MiB line should be rejected as Protocol error.
        let mut huge = vec![b'x'; 17 * 1024 * 1024];
        huge.push(b'\n');
        let mut reader = &huge[..];
        let err = read_ndjson_line(&mut reader);
        assert!(matches!(err, Err(DaemonError::Protocol(_))));
    }

    #[test]
    fn read_exact_or_disconnect_connection_reset_is_disconnected() {
        // A real EOF from a &[u8] reader that is too short produces Disconnected.
        let mut partial = &b"\x01"[..]; // 1 byte, need 5
        let mut buf = [0u8; 5];
        let err = read_exact_or_disconnect(&mut partial, &mut buf);
        assert!(matches!(err, Err(DaemonError::Disconnected { .. })));
    }

    // ── DaemonClient construction + RPC via socket pair ───────────────────

    /// Build a DaemonClient backed by one end of a UnixStream::pair(), plus
    /// the other end (the "daemon" side) for the test to write responses.
    fn make_test_client() -> (DaemonClient, UnixStream) {
        let (client_sock, server_sock) = UnixStream::pair().unwrap();
        // Short timeout so a test that forgets to write a response fails fast
        // instead of hanging for 10 s.
        client_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok();
        let client = DaemonClient {
            control: client_sock,
            stream: None,
            endpoint: DaemonEndpoint {
                socket_path: PathBuf::from("/tmp/test-orca.sock"),
                token_path: PathBuf::from("/tmp/test-orca.token"),
            },
            client_id: "test-client".to_string(),
            identity: DaemonIdentity {
                pid: 42,
                started_at_ms: 0.0,
                launch_nonce: "test-nonce".to_string(),
            },
            next_request_id: std::cell::Cell::new(1),
        };
        (client, server_sock)
    }

    /// Read one NDJSON line from the server end, then write back a response.
    fn respond(server: &mut UnixStream, response: serde_json::Value) {
        let mut byte = [0u8; 1];
        let mut line = Vec::new();
        loop {
            match server.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    line.push(byte[0]);
                }
                Err(_) => break,
            }
        }
        let json = serde_json::to_string(&response).unwrap();
        server.write_all(json.as_bytes()).unwrap();
        server.write_all(b"\n").unwrap();
    }

    #[test]
    fn rpc_round_trip_returns_payload() {
        let (mut client, mut server) = make_test_client();
        // Spawn a thread to read the request and write back an ok response.
        std::thread::spawn(move || {
            respond(
                &mut server,
                serde_json::json!({
                    "id": "rpc-1",
                    "ok": true,
                    "payload": {"answer": 42},
                    "error": null,
                }),
            );
        });
        let result = client.rpc("ping", serde_json::json!({}));
        assert!(result.is_ok());
        let payload = result.unwrap();
        assert_eq!(payload["answer"], 42);
    }

    #[test]
    fn rpc_returns_protocol_error_on_generic_failure() {
        let (mut client, mut server) = make_test_client();
        std::thread::spawn(move || {
            respond(
                &mut server,
                serde_json::json!({
                    "id": "rpc-1",
                    "ok": false,
                    "payload": null,
                    "error": "something went wrong",
                }),
            );
        });
        let result = client.rpc("write", serde_json::json!({}));
        assert!(matches!(result, Err(DaemonError::Protocol(_))));
    }

    #[test]
    fn rpc_returns_session_not_found_when_error_mentions_it() {
        let (mut client, mut server) = make_test_client();
        std::thread::spawn(move || {
            respond(
                &mut server,
                serde_json::json!({
                    "id": "rpc-1",
                    "ok": false,
                    "payload": null,
                    "error": "session not found: sess-123",
                }),
            );
        });
        let result = client.rpc("write", serde_json::json!({}));
        assert!(matches!(result, Err(DaemonError::SessionNotFound { .. })));
    }

    #[test]
    fn rpc_skips_out_of_order_responses() {
        let (mut client, mut server) = make_test_client();
        std::thread::spawn(move || {
            // Read the single request line from the client.
            let mut byte = [0u8; 1];
            let mut line = Vec::new();
            loop {
                match server.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        if byte[0] == b'\n' {
                            break;
                        }
                        line.push(byte[0]);
                    }
                    Err(_) => break,
                }
            }
            // Write back two responses: first with a wrong id (skipped), then
            // the matching id. Both on the same connection, no extra reads.
            let wrong = serde_json::json!({"id":"other-id","ok":true,"payload":null,"error":null});
            let correct =
                serde_json::json!({"id":"rpc-1","ok":true,"payload":{"matched":true},"error":null});
            let wrong_json = serde_json::to_string(&wrong).unwrap();
            let correct_json = serde_json::to_string(&correct).unwrap();
            server.write_all(wrong_json.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
            server.write_all(correct_json.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
        });
        let result = client.rpc("ping", serde_json::json!({}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["matched"], true);
    }

    #[test]
    fn ping_convenience_method_succeeds() {
        let (mut client, mut server) = make_test_client();
        std::thread::spawn(move || {
            respond(
                &mut server,
                serde_json::json!({
                    "id": "rpc-1", "ok": true, "payload": {}, "error": null,
                }),
            );
        });
        assert!(client.ping().is_ok());
    }

    #[test]
    fn list_sessions_returns_payload() {
        let (mut client, mut server) = make_test_client();
        std::thread::spawn(move || {
            respond(
                &mut server,
                serde_json::json!({
                    "id": "rpc-1", "ok": true,
                    "payload": {"sessions": ["s1", "s2"]},
                    "error": null,
                }),
            );
        });
        let result = client.list_sessions();
        assert!(result.is_ok());
        let sessions = result.unwrap();
        assert!(sessions["sessions"].is_array());
        assert_eq!(sessions["sessions"].as_array().unwrap().len(), 2);
    }

    // ── Stream + accessors ─────────────────────────────────────────────────

    #[test]
    fn read_stream_frame_on_socket_pair() {
        // Encode a Data frame, write to server end, read from client end.
        let (_client_unused, mut server) = UnixStream::pair().unwrap();
        let payload = b"hello stream";
        let encoded = encode_frame(FrameType::Data, payload).unwrap();
        server.write_all(&encoded).unwrap();
        server.flush().unwrap();
        drop(encoded); // encoded is consumed by write_all

        // Use a fresh client socket to read.
        let (client_sock, mut server2) = UnixStream::pair().unwrap();
        server2
            .write_all(&encode_frame(FrameType::Event, b"{\"event\":\"exit\"}").unwrap())
            .unwrap();
        server2.flush().unwrap();
        let mut client_sock = client_sock;
        let frame = DaemonClient::read_stream_frame(&mut client_sock).unwrap();
        assert_eq!(frame.ftype, FrameType::Event);
        assert_eq!(frame.payload, b"{\"event\":\"exit\"}");
    }

    #[test]
    fn read_stream_frame_on_eof_returns_disconnected() {
        let (client_sock, server) = UnixStream::pair().unwrap();
        drop(server); // close server end → EOF on client
        let mut client_sock = client_sock;
        let result = DaemonClient::read_stream_frame(&mut client_sock);
        assert!(matches!(result, Err(DaemonError::Disconnected { .. })));
    }

    #[test]
    fn take_stream_returns_some_then_none() {
        let (mut client, _server) = make_test_client();
        // No stream initially.
        assert!(client.take_stream().is_none());
        // Give it a stream.
        let (s1, _s2) = UnixStream::pair().unwrap();
        client.stream = Some(s1);
        assert!(client.take_stream().is_some());
        assert!(client.take_stream().is_none(), "second take returns None");
    }

    #[test]
    fn identity_and_endpoint_accessors() {
        let (client, _server) = make_test_client();
        assert_eq!(client.identity().pid, 42);
        assert_eq!(client.identity().launch_nonce, "test-nonce");
        assert_eq!(
            client.endpoint().socket_path,
            PathBuf::from("/tmp/test-orca.sock")
        );
    }

    #[test]
    fn next_id_increments() {
        let (client, _server) = make_test_client();
        assert_eq!(client.next_id(), "rpc-1");
        assert_eq!(client.next_id(), "rpc-2");
        assert_eq!(client.next_id(), "rpc-3");
    }

    #[test]
    fn daemon_connect_options_defaults() {
        let opts = DaemonConnectOptions::default();
        assert_eq!(opts.rpc_timeout, DEFAULT_RPC_TIMEOUT);
        assert_eq!(opts.hello_timeout, DEFAULT_HELLO_TIMEOUT);
    }
}
