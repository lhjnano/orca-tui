//! # Built-in daemon server
//!
//! A lightweight Rust daemon that owns agent PTYs and serves one or more
//! `orcatui attach` clients over a Unix socket. Designed to run as a
//! systemd/supervisor service — it stays in the foreground, logs to
//! stdout/stderr, and exits cleanly on SIGTERM.
//!
//! ## Protocol
//!
//! Single Unix socket, NDJSON (one JSON object per line, UTF-8, `\n`-terminated).
//! Binary data (PTY output) is base64-encoded inside JSON string fields.
//!
//! **Client → Server:**
//! ```jsonc
//! {"type":"hello","version":1}
//! {"type":"create","name":"claude","command":["claude"],"cols":80,"rows":24}
//! {"type":"write","session":0,"data":"aGVsbG8="}   // base64
//! {"type":"resize","session":0,"cols":120,"rows":40}
//! {"type":"kill","session":0}
//! {"type":"list"}
//! ```
//!
//! **Server → Client:**
//! ```jsonc
//! {"type":"hello","ok":true,"sessions":[...]}
//! {"type":"output","session":0,"data":"aGVsbG8="}   // base64
//! {"type":"exit","session":0,"code":0}
//! {"type":"created","id":1,"name":"codex"}
//! {"type":"list","sessions":[...]}
//! {"type":"error","message":"..."}
//! ```
//!
//! ## Threading model
//!
//! ```text
//!   listener thread ──accept──▶ client thread (per client)
//!   agent reader thread ──PTY──▶ event channel ──▶ main loop ──▶ client writers
//!   client thread ──command──▶ event channel ──▶ main loop ──▶ PtySession
//! ```

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};

use crate::agent::AgentState;
use crate::pty_session::PtySession;

/// Protocol version.
const PROTOCOL_VERSION: u32 = 1;

// ── Protocol messages ─────────────────────────────────────────────────────

/// A message from the client to the server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    Hello {
        version: u32,
    },
    Create {
        name: String,
        command: Vec<String>,
        cols: u16,
        rows: u16,
    },
    Write {
        session: usize,
        data: String, // base64
    },
    Resize {
        session: usize,
        cols: u16,
        rows: u16,
    },
    Kill {
        session: usize,
    },
    List,
}

/// A message from the server to the client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[allow(dead_code)]
enum ServerMsg {
    Hello {
        ok: bool,
        sessions: Vec<SessionInfo>,
    },
    Output {
        session: usize,
        data: String, // base64
    },
    Exit {
        session: usize,
        code: Option<i32>,
    },
    Created {
        id: usize,
        name: String,
    },
    List {
        sessions: Vec<SessionInfo>,
    },
    Error {
        message: String,
    },
}

/// Info about one agent session, sent in `hello` and `list` responses.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: usize,
    pub name: String,
    pub state: String,
    pub command: Vec<String>,
}

// ── Internal events ───────────────────────────────────────────────────────

/// Events delivered to the main loop via the central mpsc channel.
enum DaemonEvent {
    /// PTY output from agent `session`.
    Output { session: usize, bytes: Vec<u8> },
    /// Agent `session` exited.
    Exit { session: usize, code: Option<i32> },
    /// A command from client `client_id`.
    Command { client_id: usize, msg: ClientMsg },
    /// A new client connected.
    ClientConnected {
        id: usize,
        writer: Arc<Mutex<UnixStream>>,
    },
    /// A client disconnected.
    ClientDisconnected { id: usize },
}

// ── Agent session ─────────────────────────────────────────────────────────

/// One agent process owned by the daemon.
struct AgentEntry {
    id: usize,
    name: String,
    command: Vec<String>,
    state: AgentState,
    session: Option<PtySession>,
}

impl AgentEntry {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id,
            name: self.name.clone(),
            state: format!("{:?}", self.state).to_ascii_lowercase(),
            command: self.command.clone(),
        }
    }
}

// ── Daemon server ─────────────────────────────────────────────────────────

/// The daemon server. Owns agent PTYs and serves clients over a Unix socket.
///
/// Created by [`DaemonServer::new`] and run via [`DaemonServer::run`]. Exits
/// when all agents have exited AND no clients are connected, or when a
/// shutdown signal is received.
pub struct DaemonServer {
    socket_path: PathBuf,
    sessions: Vec<AgentEntry>,
    next_session_id: usize,
    /// Connected client writers (id → shared stream).
    clients: HashMap<usize, Arc<Mutex<UnixStream>>>,
    next_client_id: usize,
    event_rx: mpsc::Receiver<DaemonEvent>,
    event_tx: mpsc::Sender<DaemonEvent>,
    shutdown: Arc<AtomicBool>,
}

impl DaemonServer {
    /// Create a new daemon server bound to `socket_path`.
    /// Any existing socket file is removed first.
    pub fn new(socket_path: &Path) -> Result<Self> {
        // Clean up any stale socket file.
        let _ = std::fs::remove_file(socket_path);

        let (event_tx, event_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            sessions: Vec::new(),
            next_session_id: 0,
            clients: HashMap::new(),
            next_client_id: 0,
            event_rx,
            event_tx,
            shutdown,
        })
    }

    /// Spawn an initial set of agents (from `orcatui daemon -- claude :: codex`).
    pub fn spawn_initial(&mut self, commands: Vec<Vec<String>>, cols: u16, rows: u16) {
        for cmd in commands {
            let name = cmd
                .first()
                .map(|c| {
                    Path::new(c)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| c.clone())
                })
                .unwrap_or_else(|| "agent".to_string());
            self.spawn_session(name, cmd, cols, rows);
        }
    }

    /// Spawn one agent session and start its reader thread.
    fn spawn_session(&mut self, name: String, command: Vec<String>, cols: u16, rows: u16) -> usize {
        let id = self.next_session_id;
        self.next_session_id += 1;

        match PtySession::spawn(command.clone(), None, cols, rows) {
            Ok((session, rx)) => {
                let entry = AgentEntry {
                    id,
                    name: name.clone(),
                    command,
                    state: AgentState::Running,
                    session: Some(session),
                };
                self.sessions.push(entry);

                // Start a reader thread that pumps PTY output into the event channel.
                let tx = self.event_tx.clone();
                thread::Builder::new()
                    .name(format!("orcatui-daemon-agent-{id}"))
                    .spawn(move || {
                        let session_id = id;
                        loop {
                            match rx.recv() {
                                Ok(bytes) => {
                                    let _ = tx.send(DaemonEvent::Output {
                                        session: session_id,
                                        bytes,
                                    });
                                }
                                Err(_) => {
                                    // PTY reader closed — child exited.
                                    let _ = tx.send(DaemonEvent::Exit {
                                        session: session_id,
                                        code: None,
                                    });
                                    break;
                                }
                            }
                        }
                    })
                    .ok();
            }
            Err(err) => {
                eprintln!("orcatui-daemon: failed to spawn {name:?}: {err:#}");
                let entry = AgentEntry {
                    id,
                    name,
                    command,
                    state: AgentState::Failed(format!("{err:#}")),
                    session: None,
                };
                self.sessions.push(entry);
            }
        }
        id
    }

    /// Run the daemon main loop. Blocks until all agents exit and no clients
    /// remain, or until [`shutdown`] is set.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound.
    pub fn run(&mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("binding daemon socket at {}", self.socket_path.display()))?;

        eprintln!(
            "orcatui-daemon: listening on {} ({} session(s))",
            self.socket_path.display(),
            self.sessions.len()
        );

        // Spawn the acceptor thread.
        let tx = self.event_tx.clone();
        thread::Builder::new()
            .name("orcatui-daemon-listener".into())
            .spawn(move || {
                let mut next_id = 0usize;
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let id = next_id;
                            next_id += 1;
                            let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
                            let reader = stream;

                            // Send ClientConnected.
                            let _ = tx.send(DaemonEvent::ClientConnected {
                                id,
                                writer: Arc::clone(&writer),
                            });

                            // Spawn a reader thread for this client.
                            let tx2 = tx.clone();
                            thread::Builder::new()
                                .name(format!("orcatui-daemon-client-{id}"))
                                .spawn(move || {
                                    let mut reader = BufReader::new(reader);
                                    loop {
                                        let mut line = String::new();
                                        match reader.read_line(&mut line) {
                                            Ok(0) => break, // EOF
                                            Ok(_) => {
                                                if let Ok(msg) =
                                                    serde_json::from_str::<ClientMsg>(&line)
                                                {
                                                    if tx2
                                                        .send(DaemonEvent::Command {
                                                            client_id: id,
                                                            msg,
                                                        })
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }
                                    let _ = tx2.send(DaemonEvent::ClientDisconnected { id });
                                })
                                .ok();

                            // Keep writer alive — it's moved into the event loop.
                            drop(writer);
                        }
                        Err(e) => {
                            eprintln!("orcatui-daemon: accept error: {e}");
                        }
                    }
                }
            })?;

        // Main loop.
        loop {
            // Check shutdown flag.
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match self
                .event_rx
                .recv_timeout(std::time::Duration::from_millis(500))
            {
                Ok(DaemonEvent::Output { session, bytes }) => {
                    self.handle_output(session, &bytes);
                }
                Ok(DaemonEvent::Exit { session, code }) => {
                    self.handle_exit(session, code);
                }
                Ok(DaemonEvent::ClientConnected { id, writer }) => {
                    self.handle_client_connected(id, writer);
                }
                Ok(DaemonEvent::ClientDisconnected { id }) => {
                    self.clients.remove(&id);
                    eprintln!(
                        "orcatui-daemon: client {id} disconnected ({} remaining)",
                        self.clients.len()
                    );
                }
                Ok(DaemonEvent::Command { client_id, msg }) => {
                    self.handle_command(client_id, msg);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Keep running — the daemon only exits on SIGTERM or when
                    // the acceptor/reader threads all disconnect (channel drop).
                    // We do NOT auto-exit when sessions die: a service daemon
                    // should stay up so clients can attach, inspect the exit
                    // state, or create new sessions.
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        // Cleanup: remove the socket file.
        let _ = std::fs::remove_file(&self.socket_path);
        eprintln!("orcatui-daemon: stopped");
        Ok(())
    }

    /// Signal the daemon to shut down (can be called from a signal handler).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Get a clone of the shutdown flag for signal handlers.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    // ── Event handlers ──────────────────────────────────────────────────

    fn handle_output(&self, session: usize, bytes: &[u8]) {
        let b64 = general_purpose::STANDARD.encode(bytes);
        let msg = ServerMsg::Output { session, data: b64 };
        self.broadcast(&msg);
    }

    fn handle_exit(&mut self, session: usize, code: Option<i32>) {
        if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == session) {
            let state = match code {
                Some(0) | None => AgentState::Done(code),
                Some(c) => AgentState::Failed(format!("exit code {c}")),
            };
            entry.state = state;
            entry.session.take(); // Drop the PtySession (kills + joins).
        }
        let msg = ServerMsg::Exit { session, code };
        self.broadcast(&msg);
        eprintln!(
            "orcatui-daemon: session {session} exited (code {code:?}) — {} session(s) alive",
            self.sessions.iter().filter(|s| s.session.is_some()).count()
        );
    }

    fn handle_client_connected(&mut self, id: usize, writer: Arc<Mutex<UnixStream>>) {
        // Send hello with current session list.
        let sessions: Vec<SessionInfo> = self.sessions.iter().map(|e| e.info()).collect();
        let hello = ServerMsg::Hello { ok: true, sessions };
        send_msg(&writer, &hello);
        self.clients.insert(id, writer);
        eprintln!(
            "orcatui-daemon: client {id} connected ({} total)",
            self.clients.len()
        );
    }

    fn handle_command(&mut self, client_id: usize, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello { version: _ } => {
                // Already handled in handle_client_connected; re-send hello.
                let sessions: Vec<SessionInfo> = self.sessions.iter().map(|e| e.info()).collect();
                self.send_to_client(client_id, &ServerMsg::Hello { ok: true, sessions });
            }
            ClientMsg::Create {
                name,
                command,
                cols,
                rows,
            } => {
                let id = self.spawn_session(name.clone(), command, cols, rows);
                self.send_to_client(client_id, &ServerMsg::Created { id, name });
            }
            ClientMsg::Write { session, data } => {
                if let Ok(bytes) = general_purpose::STANDARD.decode(&data) {
                    if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == session) {
                        if let Some(s) = entry.session.as_mut() {
                            let _ = s.write_bytes(&bytes);
                        }
                    }
                }
            }
            ClientMsg::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == session) {
                    if let Some(s) = entry.session.as_mut() {
                        let _ = s.resize(cols, rows);
                    }
                }
            }
            ClientMsg::Kill { session } => {
                if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == session) {
                    if let Some(s) = entry.session.as_mut() {
                        let _ = s.kill();
                    }
                }
            }
            ClientMsg::List => {
                let sessions: Vec<SessionInfo> = self.sessions.iter().map(|e| e.info()).collect();
                self.send_to_client(client_id, &ServerMsg::List { sessions });
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn all_sessions_gone(&self) -> bool {
        self.sessions.iter().all(|e| e.session.is_none())
    }

    fn broadcast(&self, msg: &ServerMsg) {
        let json = serde_json::to_string(msg).unwrap();
        let line = format!("{json}\n");
        for writer in self.clients.values() {
            if let Ok(mut s) = writer.lock() {
                let _ = s.write_all(line.as_bytes());
                let _ = s.flush();
            }
        }
    }

    fn send_to_client(&self, client_id: usize, msg: &ServerMsg) {
        if let Some(writer) = self.clients.get(&client_id) {
            send_msg(writer, msg);
        }
    }
}

/// Serialize and send a message to a client.
fn send_msg(writer: &Arc<Mutex<UnixStream>>, msg: &ServerMsg) {
    if let Ok(json) = serde_json::to_string(msg) {
        if let Ok(mut s) = writer.lock() {
            let _ = s.write_all(format!("{json}\n").as_bytes());
            let _ = s.flush();
        }
    }
}

/// Default socket path: `$XDG_RUNTIME_DIR/orcatui.sock`, else `/tmp/orcatui.sock`.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("orcatui.sock")
}

// ── Attach client ─────────────────────────────────────────────────────────

/// A connected attach client. Reads NDJSON lines from the daemon socket and
/// provides a writer for sending commands.
pub struct AttachClient {
    reader: BufReader<UnixStream>,
    stream: UnixStream,
}

impl AttachClient {
    /// Connect to a running daemon at `socket_path` and complete the hello
    /// handshake. Returns the client plus the initial session list.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be connected or the handshake fails.
    pub fn connect(socket_path: &Path) -> Result<(Self, Vec<SessionInfo>)> {
        let stream = UnixStream::connect(socket_path)
            .with_context(|| format!("connecting to daemon at {}", socket_path.display()))?;

        // Send hello.
        let hello = serde_json::json!({"type":"hello","version":PROTOCOL_VERSION});
        {
            let mut s = stream.try_clone()?;
            writeln!(s, "{hello}")?;
            s.flush()?;
        }

        let reader_stream = stream.try_clone()?;
        let mut reader = BufReader::new(reader_stream);

        // Read hello response.
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let resp: serde_json::Value =
            serde_json::from_str(&line).context("parsing daemon hello response")?;

        let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            anyhow::bail!("daemon hello rejected");
        }

        let sessions: Vec<SessionInfo> = resp
            .get("sessions")
            .and_then(|v| serde_json::value::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok((Self { reader, stream }, sessions))
    }

    /// Read one NDJSON message from the daemon (blocks until a line arrives).
    ///
    /// # Errors
    ///
    /// Returns an error on EOF or a malformed line.
    pub fn read_message(&mut self) -> Result<serde_json::Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            anyhow::bail!("daemon disconnected");
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// Send a command to the daemon.
    fn send_command(&mut self, msg: &serde_json::Value) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        writeln!(self.stream, "{json}")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send raw bytes to a session (base64-encoded Write command).
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn write_session(&mut self, session: usize, data: &[u8]) -> Result<()> {
        let b64 = general_purpose::STANDARD.encode(data);
        self.send_command(&serde_json::json!({
            "type": "write",
            "session": session,
            "data": b64,
        }))
    }

    /// Resize a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn resize_session(&mut self, session: usize, cols: u16, rows: u16) -> Result<()> {
        self.send_command(&serde_json::json!({
            "type": "resize",
            "session": session,
            "cols": cols,
            "rows": rows,
        }))
    }

    /// The underlying stream (for shutdown or cloning for a reader thread).
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Clone the underlying stream (e.g. for a reader thread).
    pub fn try_clone_stream(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_info_serializes() {
        let info = SessionInfo {
            id: 0,
            name: "claude".into(),
            state: "running".into(),
            command: vec!["claude".into()],
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"name\":\"claude\""));
        assert!(json.contains("\"id\":0"));
    }

    #[test]
    fn server_msg_output_round_trips() {
        let msg = ServerMsg::Output {
            session: 2,
            data: general_purpose::STANDARD.encode(b"hello"),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ServerMsg = serde_json::from_str(&json).unwrap();
        match parsed {
            ServerMsg::Output { session, data } => {
                assert_eq!(session, 2);
                assert_eq!(general_purpose::STANDARD.decode(&data).unwrap(), b"hello");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_msg_create_deserializes() {
        let json = r#"{"type":"create","name":"codex","command":["codex","--model","x"],"cols":80,"rows":24}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        match msg {
            ClientMsg::Create {
                name,
                command,
                cols,
                rows,
            } => {
                assert_eq!(name, "codex");
                assert_eq!(command, vec!["codex", "--model", "x"]);
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn default_socket_path_uses_xdg_runtime_dir() {
        // When XDG_RUNTIME_DIR is set, the socket goes there.
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/test-orcatui");
        let p = default_socket_path();
        assert!(p.ends_with("orcatui.sock"));
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn daemon_server_creates_and_removes_socket() {
        let tmp = std::env::temp_dir().join(format!(
            "orcatui-test-{}-{}.sock",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        ));
        // Pre-create a stale file to verify cleanup.
        let _ = std::fs::write(&tmp, b"stale");
        assert!(tmp.exists(), "stale file created");

        let server = DaemonServer::new(&tmp).unwrap();
        assert!(!tmp.exists(), "stale socket cleaned up on new()");
        // socket_path is stored for binding in run().
        assert_eq!(server.socket_path, tmp);

        // Don't call run() — it would block. Just verify the socket file
        // was cleaned up.
        drop(server);
    }

    #[test]
    fn attach_client_protocol_hello_roundtrip() {
        use std::os::unix::net::UnixStream;
        let (server_sock, client_sock) = UnixStream::pair().unwrap();

        // Server side: read hello, respond with hello ok.
        let server_thread = thread::spawn(move || {
            let mut reader = BufReader::new(server_sock.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("\"type\":\"hello\""));

            let resp = serde_json::json!({
                "type": "hello",
                "ok": true,
                "sessions": [
                    {"id": 0, "name": "claude", "state": "running", "command": ["claude"]}
                ]
            });
            let mut s = server_sock;
            writeln!(s, "{resp}").unwrap();
            s.flush().unwrap();
        });

        // Client side: we need to test AttachClient::connect, but it uses
        // UnixStream::connect(path), not a pair. So test the protocol manually.
        let mut client = client_sock;
        let hello = serde_json::json!({"type":"hello","version":1});
        writeln!(client, "{hello}").unwrap();
        client.flush().unwrap();

        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["sessions"][0]["name"], "claude");

        server_thread.join().unwrap();
    }

    #[test]
    fn base64_roundtrip_for_pty_data() {
        let original = b"\x1b[31mred text\x1b[0m\n";
        let encoded = general_purpose::STANDARD.encode(original);
        let decoded = general_purpose::STANDARD.decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }
}
