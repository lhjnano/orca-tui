//! # AgentBus
//!
//! The N→1 event funnel. Every PTY reader and state change publishes onto a
//! single [`tokio::sync::mpsc`] unbounded channel; the UI thread drains the
//! receiver in a batch per frame (a `try_recv` loop) so it processes
//! `Vec<AgentUpdate>` rather than one event at a time.
//!
//! ## Why an unbounded tokio channel, no runtime?
//!
//! `portable-pty` reads are **blocking**, so each PTY's reader runs on a plain
//! [`std::thread`] and pushes chunks through a [`std::sync::mpsc`] channel. We
//! bridge N of those into one tokio MPSC via [`forward_session`]: a tiny
//! `std::thread` per session that calls `Sender::send` (synchronous on an
//! unbounded sender — no async/await, no tokio runtime required). The UI loop
//! drains with `UnboundedReceiver::try_recv`, also synchronous. So the whole
//! app runs on the main thread with zero async runtime.
//!
//! ## Backpressure (future work)
//!
//! An unbounded channel can grow without bound under a runaway agent. A
//! drop-oldest policy or a bounded channel with `blocking_send` is a documented
//! future improvement; for v1 (a handful of agents) unbounded is correct and
//! keeps the reader threads from stalling.

use std::sync::mpsc::Receiver;

use crate::agent::AgentState;

/// One update published onto the bus.
///
/// The UI thread matches on `pane_id` to route the update to the right `Pane`.
/// `Clone` so senders can keep copies for replay/debugging and the UI can
/// clone updates off the drain loop if needed.
#[derive(Debug, Clone)]
pub enum AgentUpdate {
    /// Raw PTY bytes for `pane_id`. Feed verbatim into the pane's emulator.
    Output { pane_id: usize, bytes: Vec<u8> },
    /// An out-of-band lifecycle state change (e.g. set to `Running` at spawn).
    State { pane_id: usize, state: AgentState },
    /// The agent process behind `pane_id` has exited. `code` is the raw exit
    /// code if it could be determined, else `None`.
    Exit { pane_id: usize, code: Option<i32> },
}

/// Sending half of the AgentBus. Cloning is cheap and intended: one clone per
/// session forwarder plus (optionally) one held by the UI for control events.
pub type AgentUpdateSender = tokio::sync::mpsc::UnboundedSender<AgentUpdate>;

/// Receiving half of the AgentBus. Drained by the UI loop with `try_recv`.
pub type AgentUpdateReceiver = tokio::sync::mpsc::UnboundedReceiver<AgentUpdate>;

/// Create a fresh AgentBus channel pair.
///
/// Producers are blocking PTY-reader threads calling `UnboundedSender::send`
/// (synchronous on an unbounded sender — no `async`/`await`, no tokio runtime
/// required); the UI loop drains the receiver with `try_recv` in a batch per
/// frame, also synchronous. Neither side needs a tokio runtime.
///
/// The receiver observes `try_recv` errors (disconnected) once **all** sender
/// clones are dropped — which happens when every session forwarder exits. That
/// is the UI loop's "all agents are gone" signal.
///
/// Backpressure / drop-oldest is a documented future improvement, not
/// implemented now: an unbounded channel is correct for v1 (a handful of
/// agents) and keeps reader threads from stalling.
#[must_use]
pub fn agent_bus_channel() -> (AgentUpdateSender, AgentUpdateReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Backward-compatible alias for [`agent_bus_channel`].
///
/// Kept so existing callers (e.g. `app::App::spawn_agents`) that predate the
/// rename continue to compile; new code should call [`agent_bus_channel`].
#[must_use]
pub fn channel() -> (AgentUpdateSender, AgentUpdateReceiver) {
    agent_bus_channel()
}

/// Bridge a blocking [`std::sync::mpsc`] PTY receiver onto the async AgentBus.
///
/// Run one of these per session on a dedicated [`std::thread`]. It pumps each
/// output chunk through as [`AgentUpdate::Output`]; when the PTY reader
/// disconnects (child exited, slave EOF) it emits a single
/// [`AgentUpdate::Exit`] and returns, dropping its sender clone so the UI can
/// detect "all senders gone".
///
/// `UnboundedSender::send` is synchronous (never awaits), so this runs cleanly
/// off any tokio runtime — it is just a channel push that returns `Err` only
/// when the receiver has been dropped (UI is shutting down).
pub fn forward_session(pane_id: usize, rx: Receiver<Vec<u8>>, tx: AgentUpdateSender) {
    loop {
        match rx.recv() {
            Ok(bytes) => {
                if tx.send(AgentUpdate::Output { pane_id, bytes }).is_err() {
                    // UI receiver gone — stop pumping.
                    break;
                }
            }
            Err(_) => {
                // PTY reader disconnected → child has exited. We do not know
                // the exit code from here (the reader only sees EOF); the UI's
                // own `try_wait` sweep / the reader thread's exit is the
                // authoritative signal. Emit `None` so the UI marks the pane
                // Done and can refine the code via `try_wait` if desired.
                let _ = tx.send(AgentUpdate::Exit {
                    pane_id,
                    code: None,
                });
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_delivers_in_order_then_disconnects() {
        let (tx, mut rx) = agent_bus_channel();
        tx.send(AgentUpdate::Output {
            pane_id: 0,
            bytes: vec![1, 2, 3],
        })
        .expect("send");
        tx.send(AgentUpdate::State {
            pane_id: 0,
            state: AgentState::Running,
        })
        .expect("send");
        drop(tx);

        let first = rx.try_recv().expect("first");
        let second = rx.try_recv().expect("second");
        assert!(matches!(first, AgentUpdate::Output { pane_id: 0, .. }));
        assert!(matches!(
            second,
            AgentUpdate::State {
                pane_id: 0,
                state: AgentState::Running,
            }
        ));
        // All senders dropped → receiver is disconnected.
        assert!(rx.try_recv().is_err());
    }

    /// TTY-free forwarder smoke test: feed bytes then drop the std sender to
    /// simulate PTY EOF, run `forward_session` to completion, and assert the
    /// bus received an `Output` followed by an `Exit`.
    #[test]
    fn forward_session_pumps_output_then_exits() {
        let (pty_tx, pty_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (bus_tx, mut bus_rx) = agent_bus_channel();

        // Queue one chunk, then drop the sender so recv() errors on the next
        // iteration (simulating child exit / slave EOF).
        pty_tx.send(vec![b'h', b'i']).expect("send bytes");
        drop(pty_tx);

        // Runs synchronously: one Output, recv errors, one Exit, return.
        forward_session(7, pty_rx, bus_tx);

        let mut got_output = false;
        let mut got_exit = false;
        while let Ok(upd) = bus_rx.try_recv() {
            match upd {
                AgentUpdate::Output { pane_id, bytes } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(bytes, vec![b'h', b'i']);
                    got_output = true;
                }
                AgentUpdate::Exit { pane_id, code } => {
                    assert_eq!(pane_id, 7);
                    assert!(code.is_none(), "forwarder emits code=None");
                    got_exit = true;
                }
                _ => {}
            }
        }
        assert!(got_output, "expected an Output update");
        assert!(got_exit, "expected an Exit update");
    }

    /// Cross-thread delivery: a spawned PTY-reader thread pushes an `Output`
    /// then an `Exit` onto the bus; the main thread drains with `try_recv` and
    /// asserts both arrive in order.
    ///
    /// `UnboundedSender::send` is synchronous (the unbounded sender has no
    /// `blocking_send` — that method only exists on the *bounded* `Sender`),
    /// so calling it from a plain `std::thread` is exactly the producer model
    /// the doc comment describes: no tokio runtime anywhere.
    #[test]
    fn send_from_thread_arrives_in_order_on_main() {
        let (tx, mut rx) = agent_bus_channel();

        let handle = std::thread::spawn(move || {
            tx.send(AgentUpdate::Output {
                pane_id: 3,
                bytes: vec![b'a', b'b'],
            })
            .expect("send output from thread");
            tx.send(AgentUpdate::Exit {
                pane_id: 3,
                code: Some(0),
            })
            .expect("send exit from thread");
        });
        handle.join().expect("reader thread panicked");

        let first = rx.try_recv().expect("first update should be queued");
        let second = rx.try_recv().expect("second update should be queued");
        assert!(
            matches!(first, AgentUpdate::Output { pane_id: 3, ref bytes } if bytes == &vec![b'a', b'b']),
            "first must be the Output, got {first:?}"
        );
        assert!(
            matches!(
                second,
                AgentUpdate::Exit {
                    pane_id: 3,
                    code: Some(0)
                }
            ),
            "second must be the Exit, got {second:?}"
        );
        // Channel drained — third recv errors (Empty, since `tx` was moved and
        // dropped inside the thread).
        assert!(
            rx.try_recv().is_err(),
            "channel should be empty after drain"
        );
    }
}
