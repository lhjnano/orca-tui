//! # Synchronized-output (mode 2026) batcher
//!
//! Agents like opencode (OpenTUI) redraw inside a synchronized-output batch:
//! `ESC[?2026h` … (clear + redraw) … `ESC[?2026l`. A terminal that HONORS mode
//! 2026 displays only the completed frame; vt100 0.15 does not implement it, so
//! without this layer orcatui renders the intermediate CLEARED state whenever
//! its frame lands inside a batch — the "agent pane is blank / flickery"
//! symptom.
//!
//! [`SyncScanner`] buffers everything between `2026h` and `2026l` and releases
//! it to the emulator **atomically** at `2026l`, so the emulator only ever
//! holds complete frames: a render that fires mid-batch still sees the previous
//! frame (the batch hasn't been fed yet), never the half-cleared intermediate.
//!
//! The scanner passes all other escape sequences (CSI, OSC, DCS, plain text)
//! through verbatim, so it composes cleanly with the OSC-9999 scanner and the
//! query responder (which both see the raw bytes independently).

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CSI: u8 = b'[';
const OSC: u8 = b']';
const DCS: u8 = b'P';
const ST_SECOND: u8 = b'\\';

/// Safety cap: if a batch grows beyond this without a closing `2026l` (a buggy
/// agent or a dropped `2026l`), flush it anyway so the emulator is not starved.
const MAX_BATCH: usize = 1 << 20; // 1 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// Saw a lone `ESC`; deciding the escape kind.
    Esc,
    /// Inside a CSI (`ESC [ …`), accumulating until a final byte.
    Csi,
    /// Inside an OSC (`ESC ] …`), passing through until BEL or ST.
    Osc,
    /// Saw `ESC` inside an OSC — deciding if it is ST.
    OscEsc,
    /// Inside a DCS (`ESC P …`), passing through until ST.
    Dcs,
    /// Saw `ESC` inside a DCS — deciding if it is ST.
    DcsEsc,
}

/// Stateful synchronized-output batcher. Feed PTY bytes (after OSC-9999
/// stripping) in with [`SyncScanner::process`]; it returns the bytes to feed
/// the vt100 emulator now — pass-through outside a batch, or a whole batched
/// frame flushed at the closing `ESC[?2026l`.
pub struct SyncScanner {
    /// True between `ESC[?2026h` and `ESC[?2026l` — bytes are buffered, not
    /// emitted, until the batch closes.
    in_sync: bool,
    /// Buffered bytes of the in-progress synchronized batch.
    buf: Vec<u8>,
    /// Bytes accumulated to return from the current `process` call.
    out: Vec<u8>,
    /// Bytes of the in-progress escape sequence being classified.
    seq: Vec<u8>,
    state: State,
}

impl Default for SyncScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncScanner {
    /// Create a scanner in the initial (ground, not batching) state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_sync: false,
            buf: Vec::new(),
            out: Vec::new(),
            seq: Vec::new(),
            state: State::Ground,
        }
    }

    /// Process a chunk. Returns the bytes to feed the emulator now (pass-through
    /// outside a sync batch, or the flushed batch at `ESC[?2026l`).
    pub fn process(&mut self, input: &[u8]) -> Vec<u8> {
        for &b in input {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.seq.clear();
                        self.seq.push(ESC);
                        self.state = State::Esc;
                    } else {
                        self.route(b);
                    }
                }
                State::Esc => match b {
                    CSI => {
                        self.seq.push(b);
                        self.state = State::Csi;
                    }
                    OSC => {
                        self.seq.push(b);
                        self.state = State::Osc;
                    }
                    DCS => {
                        self.seq.push(b);
                        self.state = State::Dcs;
                    }
                    ESC => { /* consecutive ESC; keep seq = [ESC] */ }
                    other => {
                        // A 2-byte escape (e.g. ESC M) — pass through verbatim.
                        self.route_slice(&[ESC, other]);
                        self.state = State::Ground;
                    }
                },
                State::Csi => {
                    self.seq.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        // CSI complete — intercept the sync markers, pass the rest.
                        if self.seq == b"\x1b[?2026h" {
                            self.begin_sync();
                        } else if self.seq == b"\x1b[?2026l" {
                            self.end_sync();
                        } else {
                            let taken_seq = std::mem::take(&mut self.seq);
                            self.route_slice(&taken_seq);
                        }
                        self.seq.clear();
                        self.state = State::Ground;
                    }
                }
                State::Osc => {
                    self.seq.push(b);
                    match b {
                        BEL => {
                            let taken_seq = std::mem::take(&mut self.seq);
                            self.route_slice(&taken_seq);
                            self.seq.clear();
                            self.state = State::Ground;
                        }
                        ESC => self.state = State::OscEsc,
                        _ => {}
                    }
                }
                State::OscEsc => {
                    self.seq.push(b);
                    if b == ST_SECOND {
                        let taken_seq = std::mem::take(&mut self.seq);
                        self.route_slice(&taken_seq);
                        self.seq.clear();
                        self.state = State::Ground;
                    } else {
                        // Not ST; the ESC was content. Keep accumulating the OSC.
                        self.state = State::Osc;
                    }
                }
                State::Dcs => {
                    self.seq.push(b);
                    if b == ESC {
                        self.state = State::DcsEsc;
                    } else if self.in_sync && self.buf.len() > MAX_BATCH {
                        self.flush_overflow();
                    }
                }
                State::DcsEsc => {
                    self.seq.push(b);
                    if b == ST_SECOND {
                        let taken_seq = std::mem::take(&mut self.seq);
                        self.route_slice(&taken_seq);
                        self.seq.clear();
                        self.state = State::Ground;
                    } else {
                        self.state = State::Dcs;
                    }
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    /// Route a single byte to the active target (output now, or batch buffer).
    fn route(&mut self, b: u8) {
        if self.in_sync {
            self.buf.push(b);
            if self.buf.len() > MAX_BATCH {
                self.flush_overflow();
            }
        } else {
            self.out.push(b);
        }
    }

    /// Route a byte slice to the active target.
    fn route_slice(&mut self, s: &[u8]) {
        if self.in_sync {
            self.buf.extend_from_slice(s);
            if self.buf.len() > MAX_BATCH {
                self.flush_overflow();
            }
        } else {
            self.out.extend_from_slice(s);
        }
    }

    fn begin_sync(&mut self) {
        self.in_sync = true;
        self.buf.clear();
    }

    fn end_sync(&mut self) {
        // Release the whole batched frame atomically.
        self.out.extend_from_slice(&self.buf);
        self.buf.clear();
        self.in_sync = false;
    }

    /// Safety valve: a batch grew too large without closing — flush it rather
    /// than hold the emulator back indefinitely.
    fn flush_overflow(&mut self) {
        self.out.extend_from_slice(&self.buf);
        self.buf.clear();
        self.in_sync = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"hello world"), b"hello world");
    }

    #[test]
    fn nonsync_csi_passes_through() {
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1b[31mred\x1b[0m"), b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn osc_passes_through() {
        let mut s = SyncScanner::new();
        // OSC title set, BEL-terminated.
        assert_eq!(
            s.process(b"\x1b]0;title\x07after"),
            b"\x1b]0;title\x07after"
        );
    }

    #[test]
    fn sync_batch_is_buffered_until_close_then_flushed_atomically() {
        let mut s = SyncScanner::new();
        // Open batch; the clear (spaces) is buffered, NOT emitted yet. The 2026
        // markers themselves are consumed (vt100 never sees the mode toggle);
        // the batch CONTENT is flushed at the close, then post-batch text passes.
        let a = s.process(b"\x1b[?2026h\x1b[2J CLEARED \x1b[?2026l DRAWN");
        assert_eq!(a, b"\x1b[2J CLEARED  DRAWN");
    }

    #[test]
    fn mid_batch_bytes_are_not_emitted() {
        let mut s = SyncScanner::new();
        let a = s.process(b"pre \x1b[?2026h mid-clear");
        // 'pre ' is emitted; the open-batch marker is consumed and mid-clear is
        // buffered, so nothing else comes out yet.
        assert_eq!(a, b"pre ");
        // Closing the batch flushes the buffered frame (markers consumed).
        let b = s.process(b" more \x1b[?2026l post");
        assert_eq!(b, b" mid-clear more  post");
    }

    #[test]
    fn batch_split_across_chunks() {
        let mut s = SyncScanner::new();
        let a = s.process(b"\x1b[?202");
        assert!(a.is_empty(), "marker not complete yet");
        let b = s.process(b"6h frame ");
        assert!(b.is_empty(), "batch open — frame buffered, nothing emitted");
        let c = s.process(b"end\x1b[?2026l");
        assert_eq!(c, b" frame end");
    }

    #[test]
    fn unclosed_huge_batch_flushes_for_safety() {
        let mut s = SyncScanner::new();
        s.process(b"\x1b[?2026h");
        let big = vec![b'x'; MAX_BATCH + 10];
        let out = s.process(&big);
        assert!(out.len() >= MAX_BATCH, "overflow flush releases the batch");
        assert!(!s.in_sync, "overflow clears the batching state");
    }

    #[test]
    fn dcs_sequence_passes_through_st() {
        // A DCS (here DECRQSS `ESC P 1 $ q`) terminated by ST (`ESC \`) is not a
        // sync marker and must pass through verbatim.
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1bP1$q\x1b\\"), b"\x1bP1$q\x1b\\");
    }

    #[test]
    fn two_byte_escape_passes_through() {
        // ESC M (RI) and ESC D (IND) are 2-byte escapes — pass through verbatim.
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1bM"), b"\x1bM");
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1bD"), b"\x1bD");
    }

    #[test]
    fn osc_st_termination_passes_through() {
        // An OSC title set terminated with ST (ESC backslash) instead of BEL.
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1b]0;title\x1b\\"), b"\x1b]0;title\x1b\\");
    }

    #[test]
    fn osc_non_st_esc_treated_as_content() {
        // An ESC inside an OSC that is NOT followed by '\' is literal content:
        // the scanner returns to the Osc state and keeps accumulating until the
        // real terminator (BEL), preserving the stray ESC + byte verbatim.
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1b]0;a\x1bXb\x07"), b"\x1b]0;a\x1bXb\x07");
    }

    #[test]
    fn csi_inside_sync_batch_buffered_until_close() {
        // Open a sync batch, emit a CSI color inside it, then close. The CSI is
        // buffered (via route_slice in sync mode) and flushed atomically at the
        // close; the 2026 markers themselves are consumed (never reach the
        // emulator).
        let mut s = SyncScanner::new();
        let out = s.process(b"\x1b[?2026h\x1b[31mred\x1b[?2026l");
        assert_eq!(out, b"\x1b[31mred");
    }

    #[test]
    fn consecutive_esc_keeps_single_esc_buffered() {
        // Two ESCs back-to-back: in Esc state the second ESC keeps `seq` as a
        // single [ESC] (the first ESC byte is dropped), and the second ESC then
        // introduces the following CSI, which passes through.
        let mut s = SyncScanner::new();
        assert_eq!(s.process(b"\x1b\x1b[31m"), b"\x1b[31m");
    }

    #[test]
    fn dcs_inside_sync_batch_buffered_until_close() {
        // A DCS fully contained in a sync batch is buffered (route_slice in sync
        // mode) and flushed atomically at the close, including its ST.
        let mut s = SyncScanner::new();
        let out = s.process(b"\x1b[?2026h\x1bP1$q\x1b\\\x1b[?2026l");
        assert_eq!(out, b"\x1bP1$q\x1b\\");
    }
}
