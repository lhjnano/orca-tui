//! # OSC 9999 agent-status scanner
//!
//! Some coding agents (Claude Code, Codex, …) emit structured status updates
//! inline in the PTY byte stream as **OSC 9999** escape sequences:
//!
//! - `\x1b]9999;{json}\x07` (BEL terminator), or
//! - `\x1b]9999;{json}\x1b\\` (ST = ESC-backslash terminator).
//!
//! The JSON payload carries fields like `state`, `toolName`, `toolInput`,
//! `prompt` and `model` (mirroring Orca's `AgentStatusPayload`). This module
//! captures those sequences from the raw PTY byte stream, extracts the
//! structured status into [`AgentActivity`], and passes **every other byte** —
//! including unrelated OSC sequences (title sets, color queries, OSC 8
//! hyperlinks, …) — through unchanged, so terminal rendering is not disturbed.
//!
//! ## Streaming / stateful
//!
//! The PTY is read in arbitrary chunks, so a single OSC 9999 sequence may be
//! split across several [`OscScanner::process`] calls. [`OscScanner`] is a
//! small deterministic finite automaton (DFA): it buffers the in-progress
//! escape sequence internally and resumes exactly where it left off on the
//! next chunk. Callers feed bytes in and get back `(cleaned_bytes, activities)`
//! per call.
//!
//! ## Robustness
//!
//! Malformed JSON (or non-UTF-8) inside a 9999 payload never panics: the
//! payload is best-effort parsed via `serde_json` and dropped on failure. Only
//! fully-recognized `\x1b]9999;…\x07` / `\x1b]9999;…\x1b\\` sequences are
//! stripped from the stream; a half-seen ESC at a chunk boundary is held back
//! until it resolves.

use serde::{Deserialize, Serialize};

// ---- Control bytes that drive the DFA -------------------------------------

/// `\x1b` — introduces an escape sequence; also the first byte of ST.
const ESC: u8 = 0x1b;
/// `]` (0x5d) — `ESC ]` opens an Operating System Command (OSC).
const OSC_START: u8 = b']';
/// `;` (0x3b) — separates the OSC parameter from its payload.
const SEPARATOR: u8 = b';';
/// `\x07` — one of the two OSC string terminators.
const BEL: u8 = 0x07;
/// `\` (0x5c) — second byte of the ST terminator (`ESC \`).
const ST_SECOND: u8 = b'\\';

/// The OSC parameter this module extracts. Any other parameter is passed
/// through verbatim.
const PARAM_9999: &[u8] = b"9999";

/// The parsed agent-status payload carried by an OSC 9999 sequence.
///
/// Mirrors Orca's `AgentStatusPayload`. `state` is the one required field
/// (`"working"` / `"blocked"` / `"waiting"` / `"interrupted"` / `"done"`); the
/// rest are optional and absent when the agent did not report them. Field names
/// on the wire are camelCase (`toolName`, `toolInput`) to match what the agents
/// emit; `tool` is accepted as an alias for `toolName`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentActivity {
    /// Lifecycle label from the agent: `"working"`, `"blocked"`, `"waiting"`,
    /// `"interrupted"`, `"done"` (vocabulary defined by the agent's status
    /// hooks).
    pub state: String,
    /// Current tool name, e.g. `"Edit"`, `"Bash"`, `"Read"`.
    #[serde(default, rename = "toolName", alias = "tool")]
    pub tool_name: Option<String>,
    /// Short preview of the tool input (a file path, shell command, …).
    #[serde(default, rename = "toolInput")]
    pub tool_input: Option<String>,
    /// The user's most recent prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Provider model identifier, e.g. `"gpt-5"`, `"opus"`.
    #[serde(default)]
    pub model: Option<String>,
}

/// Best-effort parse of an OSC 9999 payload into an [`AgentActivity`].
///
/// Returns `None` on invalid UTF-8 or malformed JSON — the caller drops the
/// payload but never panics.
fn parse_payload(bytes: &[u8]) -> Option<AgentActivity> {
    let s = std::str::from_utf8(bytes).ok()?;
    serde_json::from_str(s).ok()
}

/// Which kind of OSC accumulation an in-flight ESC interrupted. Only needed
/// while waiting for the possible second byte of an ST (`ESC \`) terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscCtx {
    /// Reading the OSC parameter (before any `;`).
    Param,
    /// Reading a 9999 payload (after `;`).
    Payload,
    /// Buffering a non-9999 OSC for verbatim pass-through.
    Passthrough,
}

/// DFA state for [`OscScanner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal terminal bytes — pass straight through. On `ESC`, start buffering.
    Normal,
    /// Saw a lone `ESC`; deciding what kind of escape this is.
    EscSeen,
    /// Inside an OSC, reading the parameter until `;` or a terminator.
    OscParam,
    /// Inside a **9999** OSC payload, accumulating bytes until a terminator.
    OscPayload,
    /// Inside a **non-9999** OSC: buffer everything, then flush verbatim at the
    /// terminator so downstream rendering is unaffected.
    PassthroughOsc,
    /// Saw an `ESC` while accumulating an OSC — waiting to see whether the next
    /// byte is `\` (confirming an ST terminator). Carries the prior context.
    EscInOsc(OscCtx),
}

/// Stateful byte-stream scanner that extracts OSC 9999 agent-status payloads.
///
/// Feed PTY bytes in with [`OscScanner::process`]; it returns the input with
/// OSC 9999 sequences stripped (`cleaned_bytes`) plus any [`AgentActivity`]s
/// decoded from them. All other bytes — including unrelated OSC sequences —
/// pass through unchanged, so the terminal stream stays renderable.
///
/// The scanner is a DFA: a sequence split across several `process` calls is
/// still recognized, because the partial sequence is buffered internally.
pub struct OscScanner {
    state: State,
    /// Bytes held back from the output because they may belong to an
    /// in-progress escape / OSC sequence. For a non-9999 OSC this is the buffer
    /// flushed verbatim at the terminator.
    held: Vec<u8>,
    /// The OSC parameter accumulated in [`State::OscParam`] (compared to
    /// `b"9999"`).
    param: Vec<u8>,
    /// The raw payload of an in-progress 9999 sequence ([`State::OscPayload`]).
    payload: Vec<u8>,
}

impl OscScanner {
    /// Create a scanner in the initial (normal) state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            held: Vec::new(),
            param: Vec::new(),
            payload: Vec::new(),
        }
    }

    /// Process a chunk of PTY bytes.
    ///
    /// Returns `(cleaned_bytes, activities)`:
    ///
    /// - `cleaned_bytes` is `input` with every fully-recognized OSC 9999
    ///   sequence removed; **all** other bytes (including non-9999 OSC and any
    ///   bytes of a sequence still waiting for its terminator across calls) are
    ///   preserved verbatim.
    /// - `activities` holds one [`AgentActivity`] per well-formed 9999 payload
    ///   found in this chunk. A malformed payload produces no entry (and does
    ///   not panic).
    pub fn process(&mut self, input: &[u8]) -> (Vec<u8>, Vec<AgentActivity>) {
        let mut out = Vec::with_capacity(input.len());
        let mut activities = Vec::new();

        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Normal => {
                    if b == ESC {
                        self.held.push(ESC);
                        self.state = State::EscSeen;
                    } else {
                        out.push(b);
                    }
                    i += 1;
                }
                State::EscSeen => match b {
                    OSC_START => {
                        self.held.push(OSC_START);
                        self.param.clear();
                        self.state = State::OscParam;
                        i += 1;
                    }
                    ESC => {
                        // A lone ESC followed by another ESC: emit the first,
                        // keep buffering the new one. (`held` stays `[ESC]`.)
                        out.push(ESC);
                        i += 1;
                    }
                    _ => {
                        // Not an OSC introducer (e.g. '[' starts a CSI): pass
                        // the ESC and this byte through unchanged.
                        out.push(ESC);
                        out.push(b);
                        self.held.clear();
                        self.state = State::Normal;
                        i += 1;
                    }
                },
                State::OscParam => match b {
                    SEPARATOR => {
                        if self.param.as_slice() == PARAM_9999 {
                            // Confirmed 9999: extract the payload and drop the
                            // held introducer (it never reaches the terminal).
                            self.held.clear();
                            self.payload.clear();
                            self.state = State::OscPayload;
                        } else {
                            // Non-9999 OSC: preserve the separator and switch
                            // to verbatim pass-through.
                            self.held.push(SEPARATOR);
                            self.state = State::PassthroughOsc;
                        }
                        i += 1;
                    }
                    BEL => {
                        // Terminator before any payload section.
                        if self.param.as_slice() == PARAM_9999 {
                            self.held.clear();
                        } else {
                            self.held.push(BEL);
                            out.extend(self.held.drain(..));
                        }
                        self.state = State::Normal;
                        i += 1;
                    }
                    ESC => {
                        // Possible ST terminator — buffer the ESC and wait.
                        self.held.push(ESC);
                        self.state = State::EscInOsc(OscCtx::Param);
                        i += 1;
                    }
                    _ => {
                        self.held.push(b);
                        self.param.push(b);
                        i += 1;
                    }
                },
                State::OscPayload => match b {
                    BEL => {
                        if let Some(act) = parse_payload(&self.payload) {
                            activities.push(act);
                        }
                        self.payload.clear();
                        self.state = State::Normal;
                        i += 1;
                    }
                    ESC => {
                        // Possible ST terminator; keep the payload clean until
                        // the next byte confirms it.
                        self.state = State::EscInOsc(OscCtx::Payload);
                        i += 1;
                    }
                    _ => {
                        self.payload.push(b);
                        i += 1;
                    }
                },
                State::PassthroughOsc => match b {
                    BEL => {
                        self.held.push(BEL);
                        out.extend(self.held.drain(..));
                        self.state = State::Normal;
                        i += 1;
                    }
                    ESC => {
                        self.held.push(ESC);
                        self.state = State::EscInOsc(OscCtx::Passthrough);
                        i += 1;
                    }
                    _ => {
                        self.held.push(b);
                        i += 1;
                    }
                },
                State::EscInOsc(ctx) => match b {
                    ST_SECOND => {
                        // ST confirmed: the terminator was `\x1b\\`.
                        match ctx {
                            OscCtx::Param => {
                                if self.param.as_slice() == PARAM_9999 {
                                    self.held.clear();
                                } else {
                                    self.held.push(ST_SECOND);
                                    out.extend(self.held.drain(..));
                                }
                            }
                            OscCtx::Payload => {
                                if let Some(act) = parse_payload(&self.payload) {
                                    activities.push(act);
                                }
                                self.payload.clear();
                            }
                            OscCtx::Passthrough => {
                                self.held.push(ST_SECOND);
                                out.extend(self.held.drain(..));
                            }
                        }
                        self.state = State::Normal;
                        i += 1;
                    }
                    _ => {
                        // The ESC was not followed by '\' -> not an ST. End the
                        // in-progress OSC abnormally and re-feed this byte from
                        // Normal so a following escape (or plain byte) is still
                        // handled. For a 9999 extraction the partial payload is
                        // dropped; for pass-through contexts the bytes seen so
                        // far (incl. the stray ESC) are preserved on the output.
                        match ctx {
                            OscCtx::Payload => {
                                self.payload.clear();
                            }
                            OscCtx::Param | OscCtx::Passthrough => {
                                out.extend(self.held.drain(..));
                            }
                        }
                        self.state = State::Normal;
                        continue; // re-process `b` without advancing
                    }
                },
            }
        }

        (out, activities)
    }
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_extraction_bel() {
        let input = b"hello\x1b]9999;{\"state\":\"working\",\"toolName\":\"Edit\"}\x07world";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, b"helloworld");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "working");
        assert_eq!(acts[0].tool_name.as_deref(), Some("Edit"));
    }

    #[test]
    fn st_terminator() {
        let input = b"\x1b]9999;{\"state\":\"blocked\",\"toolName\":\"Bash\"}\x1b\\";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(
            clean.is_empty(),
            "9999 sequence must be stripped, got {clean:?}"
        );
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "blocked");
        assert_eq!(acts[0].tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn non_9999_osc_passes_through() {
        let input = b"\x1b]0;my title\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, input, "non-9999 OSC must pass through verbatim");
        assert!(acts.is_empty());
    }

    #[test]
    fn non_9999_osc_st_terminator_passes_through() {
        // A non-9999 OSC terminated with ST (ESC backslash) must also pass
        // through unchanged, including the ST terminator bytes.
        let input = b"\x1b]2;set me\x1b\\";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, input);
        assert!(acts.is_empty());
    }

    #[test]
    fn cross_chunk_split() {
        let mut s = OscScanner::new();
        // Split the JSON `{"state":"working"}` mid-value across two calls so
        // the payload, the introducer, and the terminator each straddle a
        // chunk boundary (the hardest streaming case).
        let (clean1, acts1) = s.process(b"hi\x1b]9999;{\"state\":\"work");
        let (clean2, acts2) = s.process(b"ing\"}\x07bye");
        assert!(acts1.is_empty(), "no activity until the terminator arrives");
        let clean: Vec<u8> = [clean1, clean2].concat();
        let acts: Vec<AgentActivity> = [acts1, acts2].concat();
        assert_eq!(clean, b"hibye");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "working");
    }

    #[test]
    fn malformed_json_is_dropped_no_panic() {
        let input = b"\x1b]9999;{broken}\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(
            clean.is_empty(),
            "sequence must be stripped even on bad JSON"
        );
        assert!(acts.is_empty(), "no activity from malformed JSON");
    }

    #[test]
    fn invalid_utf8_payload_is_dropped() {
        // Lone continuation bytes are not valid UTF-8 -> payload parse returns
        // None and the bytes are still stripped from the stream.
        let input = b"\x1b]9999;\xff\xfe\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(clean.is_empty());
        assert!(acts.is_empty());
    }

    #[test]
    fn multiple_sequences_one_chunk() {
        let input = b"\x1b]9999;{\"state\":\"working\"}\x07mid\x1b]9999;{\"state\":\"done\"}\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, b"mid");
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0].state, "working");
        assert_eq!(acts[1].state, "done");
    }

    #[test]
    fn no_esc_passes_through() {
        let input = b"just plain text, no escapes here";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, input);
        assert!(acts.is_empty());
    }

    #[test]
    fn partial_esc_at_chunk_end() {
        let mut s = OscScanner::new();
        let (clean1, acts1) = s.process(b"hello\x1b");
        let (clean2, acts2) = s.process(b"]9999;{\"state\":\"waiting\"}\x07world");
        assert!(
            acts1.is_empty(),
            "no activity expected from the first chunk"
        );
        let clean: Vec<u8> = [clean1, clean2].concat();
        let acts: Vec<AgentActivity> = [acts1, acts2].concat();
        assert_eq!(clean, b"helloworld");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "waiting");
    }

    #[test]
    fn tool_alias_accepted() {
        let input = b"\x1b]9999;{\"state\":\"working\",\"tool\":\"Bash\"}\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(clean.is_empty());
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn full_payload_round_trips() {
        let input = b"\x1b]9999;{\"state\":\"working\",\"toolName\":\"Edit\",\"toolInput\":\"src/app.rs\",\"prompt\":\"implement feature\",\"model\":\"gpt-5\"}\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(clean.is_empty());
        assert_eq!(acts.len(), 1);
        let a = &acts[0];
        assert_eq!(a.state, "working");
        assert_eq!(a.tool_name.as_deref(), Some("Edit"));
        assert_eq!(a.tool_input.as_deref(), Some("src/app.rs"));
        assert_eq!(a.prompt.as_deref(), Some("implement feature"));
        assert_eq!(a.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn csi_sequence_passes_through() {
        // A CSI color sequence (\x1b[ ... m) is not an OSC and must pass
        // through untouched alongside surrounding text.
        let input = b"\x1b[31mred\x1b]9999;{\"state\":\"done\"}\x07\x1b[0m";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert_eq!(clean, b"\x1b[31mred\x1b[0m");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "done");
    }

    #[test]
    fn lone_esc_then_plain_byte_passes_through() {
        // A stray ESC buffered at a chunk boundary, followed by a non-`]` byte,
        // must flush ESC + byte (not eat the following byte).
        let mut s = OscScanner::new();
        let (clean1, _acts1) = s.process(b"x\x1b");
        let (clean2, _acts2) = s.process(b"y");
        let clean: Vec<u8> = [clean1, clean2].concat();
        assert_eq!(clean, b"x\x1by");
    }

    #[test]
    fn scanner_default_works() {
        let mut s = OscScanner::default();
        let (clean, acts) = s.process(b"\x1b]9999;{\"state\":\"done\"}\x07");
        assert!(clean.is_empty());
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "done");
    }

    #[test]
    fn consecutive_esc_emits_first_then_starts_new() {
        // A lone ESC immediately followed by another ESC: the first is emitted
        // to the output (as a stray ESC), and the second begins a fresh escape
        // — here an OSC 9999 that is then stripped. Covers the EscSeen ESC arm.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"a\x1b\x1b]9999;{\"state\":\"done\"}\x07b");
        assert_eq!(clean, b"a\x1bb");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "done");
    }

    #[test]
    fn osc_9999_empty_bel_terminator_no_activity() {
        // A 9999 OSC closed by BEL before any payload separator: nothing to
        // parse, no activity, and the introducer is dropped (no bytes emitted).
        // Covers the OscParam BEL arm for param == 9999.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]9999\x07");
        assert!(clean.is_empty());
        assert!(acts.is_empty());
    }

    #[test]
    fn osc_non_9999_empty_bel_passes_through() {
        // A non-9999 OSC closed by BEL with no payload passes through verbatim.
        // Covers the OscParam BEL arm for param != 9999.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]0\x07");
        assert_eq!(clean, b"\x1b]0\x07");
        assert!(acts.is_empty());
    }

    #[test]
    fn osc_9999_empty_st_terminator_no_activity() {
        // A 9999 OSC closed by ST (`ESC \`) before any payload separator: no
        // activity, introducer stripped. Covers OscParam -> EscInOsc(Param) and
        // the EscInOsc(Param) ST arm for param == 9999.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]9999\x1b\\");
        assert!(clean.is_empty());
        assert!(acts.is_empty());
    }

    #[test]
    fn osc_non_9999_param_st_passes_through() {
        // A non-9999 OSC param (no `;` yet) closed by ST passes through
        // verbatim. Covers the EscInOsc(Param) ST arm for param != 9999.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]0\x1b\\");
        assert_eq!(clean, b"\x1b]0\x1b\\");
        assert!(acts.is_empty());
    }

    #[test]
    fn stray_esc_in_9999_payload_drops_partial() {
        // An ESC inside a 9999 payload that is NOT followed by '\' is not an ST;
        // the partial payload is dropped and the re-fed byte plus following
        // bytes pass through as normal terminal content. Covers the
        // EscInOsc non-ST arm (Payload context) + the re-feed `continue`.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]9999;{\"state\":\"working\"}\x1bX\x07z");
        assert_eq!(clean, b"X\x07z");
        assert!(acts.is_empty());
    }

    #[test]
    fn stray_esc_in_osc_param_preserves_bytes() {
        // An ESC inside a non-9999 OSC param (no `;` yet) not followed by '\' is
        // literal content; everything seen so far plus the stray byte flush
        // verbatim, and the trailing BEL is preserved. Covers the EscInOsc
        // non-ST arm (Param context).
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]0\x1bX\x07");
        assert_eq!(clean, b"\x1b]0\x1bX\x07");
        assert!(acts.is_empty());
    }

    #[test]
    fn stray_esc_in_passthrough_osc_preserves_bytes() {
        // A non-9999 OSC past its separator, with a stray ESC (not ST) inside
        // the payload, still flushes all bytes verbatim at the final BEL.
        // Covers the EscInOsc non-ST arm (Passthrough context).
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]0;title\x1bX\x07");
        assert_eq!(clean, b"\x1b]0;title\x1bX\x07");
        assert!(acts.is_empty());
    }

    #[test]
    fn payload_with_unknown_fields_still_parses() {
        // Unknown JSON fields are ignored by serde; the known `state` still
        // parses into an activity.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]9999;{\"state\":\"waiting\",\"bogus\":42}\x07");
        assert!(clean.is_empty());
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "waiting");
    }

    #[test]
    fn near_miss_osc_param_passes_through() {
        // "99999" (one digit too many) is not "9999" → treated as a normal OSC
        // and passed through verbatim; no activity is extracted.
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(b"\x1b]99999;x\x07");
        assert_eq!(clean, b"\x1b]99999;x\x07");
        assert!(acts.is_empty());
    }

    #[test]
    fn st_terminator_split_across_chunks() {
        // The ST terminator itself straddles a chunk boundary: the ESC lands at
        // the end of one process() call and the '\' at the start of the next.
        let mut s = OscScanner::new();
        let (c1, a1) = s.process(b"\x1b]9999;{\"state\":\"done\"}\x1b");
        let (c2, a2) = s.process(b"\\tail");
        assert!(a1.is_empty(), "no activity until ST completes");
        let clean: Vec<u8> = [c1, c2].concat();
        let acts: Vec<AgentActivity> = [a1, a2].concat();
        assert_eq!(clean, b"tail");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "done");
    }

    #[test]
    fn interrupted_state_round_trips() {
        // The "interrupted" OSC state (AgentStatus::Interrupted) must parse
        // into an AgentActivity carrying the literal state string, matching the
        // derive() vocabulary on the agent side.
        let input = b"\x1b]9999;{\"state\":\"interrupted\"}\x07";
        let mut s = OscScanner::new();
        let (clean, acts) = s.process(input);
        assert!(clean.is_empty());
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].state, "interrupted");
    }
}
