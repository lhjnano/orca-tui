//! # Terminal capability query responder
//!
//! Some agents — full-screen TUIs, and notably opencode (OpenTUI) — probe the
//! host terminal's capabilities by emitting query escape sequences and
//! **waiting for a response** before they render:
//!
//! - `OSC 10 ; ?` / `OSC 11 ; ?` / `OSC 12 ; ?` — default fg / bg / cursor color
//! - `CSI ? Ps $ p` — DECRQM: "is private mode `Ps` set?" (focus reporting,
//!   bracketed paste, synchronized output, mouse modes, …)
//! - `CSI c` / `CSI > c` — DA1 / DA2 (primary / secondary device attributes)
//! - `DCS + q <hex>` — Request Terminfo String (xterm)
//!
//! A real terminal answers these; orcatui's PTY does not, so a probing agent
//! cannot determine the terminal and renders blank or minimally (the "opencode
//! shows rarely, feels overwritten" symptom). [`QueryResponder`] runs alongside
//! the vt100 emulator: it detects these query sequences in the PTY byte stream
//! and synthesizes the responses the agent expects, so it proceeds to render.
//!
//! The scanner is a small DFA: it buffers an in-progress escape sequence across
//! `process` calls (a query may be split over PTY read chunks) and, when a
//! recognized query completes, appends the matching response to the returned
//! byte vector. Unrecognized / non-query sequences are ignored (the vt100
//! emulator still receives every byte unchanged).
//!
//! Responses use the theme's actual fg/bg colors so the agent picks up orcatui
//! `ThemeConfig`.

use ratatui::style::Color;

use crate::config::ThemeConfig;

// ---- Control bytes --------------------------------------------------------
const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const ST_SECOND: u8 = b'\\'; // second byte of ST (ESC \)
const OSC_START: u8 = b']';
const CSI_START: u8 = b'[';
const DCS_START: u8 = b'P';

/// DFA state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Normal,
    /// Saw a lone `ESC`; deciding the escape kind.
    Esc,
    /// Inside an OSC (`ESC ] …`), accumulating until BEL or ST.
    Osc,
    /// Saw `ESC` while inside an OSC — deciding if it is ST (`ESC \`).
    OscEsc,
    /// Inside a CSI (`ESC [ …`), accumulating until a final byte.
    Csi,
    /// Inside a DCS (`ESC P …`), accumulating until ST.
    Dcs,
    /// Saw `ESC` while inside a DCS — deciding if it is ST.
    DcsEsc,
}

/// Stateful scanner that extracts terminal-capability queries from a PTY byte
/// stream and produces the responses a probing agent expects.
///
/// Feed PTY bytes in with [`QueryResponder::process`]; it returns the
/// concatenation of all responses to recognized queries found in the chunk
/// (empty if none). Call it on the same bytes you feed the vt100 emulator — it
/// does not consume or strip them.
pub struct QueryResponder {
    state: State,
    /// Bytes of the in-progress escape sequence (without the leading `ESC`
    /// when in an `Osc`/`Csi`/`Dcs` body — the kind is encoded in `state`).
    buf: Vec<u8>,
}

impl Default for QueryResponder {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryResponder {
    /// Create a responder in the initial (normal) state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            buf: Vec::new(),
        }
    }

    /// Process a chunk of PTY bytes. Returns the concatenated responses to any
    /// recognized capability queries completed within this chunk.
    pub fn process(&mut self, input: &[u8], theme: &ThemeConfig) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in input {
            match self.state {
                State::Normal => {
                    if b == ESC {
                        self.buf.clear();
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    OSC_START => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    CSI_START => {
                        self.buf.clear();
                        self.state = State::Csi;
                    }
                    DCS_START => {
                        self.buf.clear();
                        self.state = State::Dcs;
                    }
                    ESC => { /* consecutive ESC: keep buffering a fresh one */ }
                    _ => {
                        // A 2-byte escape (e.g. ESC M) — not a query we handle.
                        self.state = State::Normal;
                    }
                },
                State::Osc => {
                    if b == BEL {
                        if let Some(resp) = osc_color_response(&self.buf, theme) {
                            out.extend_from_slice(&resp);
                        }
                        self.state = State::Normal;
                    } else if b == ESC {
                        self.state = State::OscEsc;
                    } else {
                        self.buf.push(b);
                    }
                }
                State::OscEsc => {
                    if b == ST_SECOND {
                        // ST terminator: OSC complete.
                        if let Some(resp) = osc_color_response(&self.buf, theme) {
                            out.extend_from_slice(&resp);
                        }
                        self.state = State::Normal;
                    } else {
                        // Not ST: the ESC was literal content; keep accumulating.
                        self.buf.push(ESC);
                        self.buf.push(b);
                        self.state = State::Osc;
                    }
                }
                State::Csi => {
                    // Final byte ranges 0x40..=0x7e end a CSI.
                    if (0x40..=0x7e).contains(&b) {
                        self.buf.push(b);
                        if let Some(resp) = csi_response(&self.buf) {
                            out.extend_from_slice(&resp);
                        }
                        self.state = State::Normal;
                    } else {
                        // Parameter / intermediate byte (0x20..=0x3f).
                        self.buf.push(b);
                    }
                }
                State::Dcs => {
                    if b == ESC {
                        self.state = State::DcsEsc;
                    } else {
                        self.buf.push(b);
                    }
                }
                State::DcsEsc => {
                    if b == ST_SECOND {
                        if let Some(resp) = dcs_response(&self.buf) {
                            out.extend_from_slice(&resp);
                        }
                        self.state = State::Normal;
                    } else {
                        self.buf.push(ESC);
                        self.buf.push(b);
                        self.state = State::Dcs;
                    }
                }
            }
        }
        out
    }
}

/// Build the response for an OSC color query (`10`/`11`/`12`), if `body` is one.
/// `body` is the OSC payload after `ESC ]` and before the terminator, e.g.
/// `b"10;?"`.
fn osc_color_response(body: &[u8], theme: &ThemeConfig) -> Option<Vec<u8>> {
    // Split on the first ';' into the parameter ("10"/"11"/"12") and the rest.
    let sep = body.iter().position(|&c| c == b';')?;
    let param = &body[..sep];
    let rest = &body[sep + 1..];
    // Only the query form (`Ps;?`) is a query — anything else is a set command.
    if rest != b"?" {
        return None;
    }
    let color = match param {
        b"10" => theme.fg(),
        b"11" => theme.bg(),
        b"12" => theme.accent(),
        _ => return None,
    };
    let (r, g, b) = rgb_of(color);
    let mut resp = Vec::new();
    resp.extend_from_slice(b"\x1b]");
    resp.extend_from_slice(param);
    resp.extend_from_slice(b";rgb:");
    resp.extend_from_slice(format!("{r:04x}/{g:04x}/{b:04x}").as_bytes());
    resp.push(BEL);
    Some(resp)
}

/// Build the response for a CSI query — DA1/DA2 (`CSI … c`) or private DECRQM
/// (`CSI ? Ps $ p`). `body` is the CSI payload after `ESC [` and INCLUDING the
/// final byte.
fn csi_response(body: &[u8]) -> Option<Vec<u8>> {
    let final_byte = *body.last()?;
    match final_byte {
        b'c' => {
            // Device attributes. `>…` ⇒ DA2, else DA1.
            if body.starts_with(b">") {
                Some(b"\x1b[>0;276;0c".to_vec())
            } else {
                Some(b"\x1b[?6c".to_vec())
            }
        }
        b'p' => {
            // DECRQM: the request ends in `$ p` (0x24 0x70). Only the PRIVATE
            // form (`CSI ? Ps $ p`) is answered; reply "not recognized" (Pv=0)
            // so the agent falls back rather than hanging.
            if body.len() >= 2 && body[body.len() - 2] == b'$' {
                let params = &body[..body.len() - 2];
                if params.starts_with(b"?") {
                    let ps: String = params[1..]
                        .iter()
                        .filter(|c| c.is_ascii_digit())
                        .map(|c| *c as char)
                        .collect();
                    return Some(format!("\x1b[?{ps};0$y").into_bytes());
                }
            }
            None
        }
        _ => None,
    }
}

/// Build the response for a DCS Request Terminfo (`DCS + q <hex> ST`). xterm's
/// failure response (`DCS 0 + r ST`) tells the requester the capability is
/// unknown, which makes it stop waiting and fall back. `body` is the DCS
/// payload (starts with `+q` for a request).
fn dcs_response(body: &[u8]) -> Option<Vec<u8>> {
    if body.starts_with(b"+q") {
        // Request terminfo → reply "not found" (0 + r).
        Some(b"\x1bP0+r\x1b\\".to_vec())
    } else {
        None
    }
}

/// Extract `(r, g, b)` (0-255 each) from a ratatui [`Color`]. Named colors map
/// to sensible defaults so a malformed theme never yields a nonsense reply.
fn rgb_of(color: Color) -> (u16, u16, u16) {
    match color {
        Color::Rgb(r, g, b) => (u16::from(r), u16::from(g), u16::from(b)),
        Color::Black => (0, 0, 0),
        Color::White => (255, 255, 255),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::Red => (229, 81, 73),
        Color::Green => (63, 185, 80),
        Color::Yellow => (210, 153, 34),
        Color::Blue => (88, 166, 255),
        Color::Cyan => (56, 139, 253),
        Color::Magenta => (188, 63, 253),
        Color::LightGreen => (110, 200, 110),
        Color::LightBlue => (140, 190, 255),
        Color::LightRed => (240, 130, 120),
        Color::LightYellow => (230, 200, 100),
        Color::LightMagenta => (210, 140, 255),
        Color::LightCyan => (140, 220, 255),
        _ => (13, 17, 23),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeConfig;

    fn theme() -> ThemeConfig {
        ThemeConfig::default()
    }

    #[test]
    fn osc10_fg_query_returns_theme_fg_color() {
        // ESC ] 10 ; ? BEL → reply with the theme fg color as rgb:.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]10;?\x07", &theme());
        let s = String::from_utf8(resp).unwrap();
        // Default fg is #e6edf3 → (0xe6, 0xed, 0xf3).
        assert!(
            s.starts_with("\x1b]10;rgb:") && s.ends_with('\u{7}'),
            "OSC10 response shape: {s:?}"
        );
        assert!(s.contains("00e6/00ed/00f3"), "fg color echoed: {s:?}");
    }

    #[test]
    fn osc11_bg_query_returns_theme_bg_color() {
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]11;?\x07", &theme());
        let s = String::from_utf8(resp).unwrap();
        // Default bg #0d1117 → (0x0d,0x11,0x17) → 000d/0011/0017.
        assert!(s.contains("000d/0011/0017"), "bg color echoed: {s:?}");
    }

    #[test]
    fn osc_color_set_command_is_not_a_query() {
        // `OSC 11 ; rgb:…` is a SET, not a query (no trailing `?`) → no reply.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]11;rgb:0000/0000/0000\x07", &theme());
        assert!(resp.is_empty(), "set commands get no response");
    }

    #[test]
    fn osc_color_st_terminator_also_handled() {
        // ESC ] 10 ; ? ESC \  (ST terminator instead of BEL).
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]10;?\x1b\\", &theme());
        assert!(!resp.is_empty(), "ST-terminated OSC query is answered");
    }

    #[test]
    fn decrqm_private_mode_answered_not_recognized() {
        // CSI ? 2026 $ p → reply CSI ? 2026 ; 0 $ y.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[?2026$p", &theme());
        assert_eq!(resp, b"\x1b[?2026;0$y");
    }

    #[test]
    fn da1_query_answered() {
        // CSI c (DA1) → reply with a vt102-ish DA.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[c", &theme());
        assert_eq!(resp, b"\x1b[?6c");
        // CSI 0 c is also DA1.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[0c", &theme());
        assert_eq!(resp, b"\x1b[?6c");
    }

    #[test]
    fn da2_query_answered() {
        // CSI > c (DA2).
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[>c", &theme());
        assert_eq!(resp, b"\x1b[>0;276;0c");
    }

    #[test]
    fn dcs_terminfo_request_answered_not_found() {
        // DCS + q 4d73 ST → reply DCS 0 + r ST.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1bP+q4d73\x1b\\", &theme());
        assert_eq!(resp, b"\x1bP0+r\x1b\\");
    }

    #[test]
    fn query_split_across_chunks_is_still_answered() {
        // OSC 10 query split mid-payload across two process() calls.
        let mut r = QueryResponder::new();
        let a = r.process(b"\x1b]1", &theme());
        let b = r.process(b"0;?\x07", &theme());
        assert!(a.is_empty(), "no response until the query completes");
        assert!(
            String::from_utf8_lossy(&b).starts_with("\x1b]10;rgb:"),
            "completed across chunks: {b:?}"
        );
    }

    #[test]
    fn plain_text_and_non_query_escapes_produce_no_response() {
        let mut r = QueryResponder::new();
        // Ordinary text + a cursor-up (ESC [ A) + a set-cursor-style (ESC > 0 q)
        // — none are queries, so nothing is answered.
        let resp = r.process(b"hello world\x1b[A\x1b[>0qmore text", &theme());
        assert!(resp.is_empty(), "non-query stream yields no response");
    }

    #[test]
    fn multiple_queries_one_chunk_all_answered() {
        // opencode-style burst: OSC10, OSC11, several DECRQMs, a DA, a DCS.
        let burst: &[u8] =
            b"\x1b]10;?\x07\x1b]11;?\x07\x1b[?1016$p\x1b[?2026$p\x1b[c\x1bP+q4d73\x1b\\";
        let mut r = QueryResponder::new();
        let resp = r.process(burst, &theme());
        let s = String::from_utf8_lossy(&resp);
        assert!(s.contains("\x1b]10;rgb:"), "OSC10 answered");
        assert!(s.contains("\x1b]11;rgb:"), "OSC11 answered");
        assert!(s.contains("\x1b[?1016;0$y"), "DECRQM 1016 answered");
        assert!(s.contains("\x1b[?2026;0$y"), "DECRQM 2026 answered");
        assert!(s.contains("\x1b[?6c"), "DA1 answered");
        assert!(s.contains("\x1bP0+r\x1b\\"), "DCS answered");
    }

    #[test]
    fn osc12_cursor_query_returns_theme_accent_color() {
        // OSC 12 ; ? (default cursor color) → reply with the theme accent color.
        // Covers the `b"12" => theme.accent()` arm of osc_color_response.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]12;?\x07", &theme());
        let s = String::from_utf8(resp).unwrap();
        assert!(
            s.starts_with("\x1b]12;rgb:") && s.ends_with('\u{7}'),
            "OSC12 response shape: {s:?}"
        );
        // Default accent is #58a6ff → (0x58, 0xa6, 0xff).
        assert!(s.contains("0058/00a6/00ff"), "accent color echoed: {s:?}");
    }

    #[test]
    fn dcs_non_terminfo_request_no_response() {
        // A DCS that is NOT a Request Terminfo (`+q`) → no response. Covers the
        // `else` arm of dcs_response.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1bP$q\x1b\\", &theme());
        assert!(resp.is_empty());
    }

    #[test]
    fn decrqm_non_private_mode_no_response() {
        // The ANSI (non-private) DECRQM form `CSI Ps $ p` (no `?`) is not
        // answered. Covers the `params.starts_with(b"?") == false` path.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[12$p", &theme());
        assert!(resp.is_empty());
    }

    #[test]
    fn csi_p_final_without_dollar_no_response() {
        // A CSI ending in 'p' but not preceded by '$' is not a DECRQM → no
        // reply. Covers the `body[len-2] != b'$'` path of csi_response.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[1p", &theme());
        assert!(resp.is_empty());
    }

    #[test]
    fn two_byte_escape_no_response() {
        // ESC M (and any 2-byte escape) is not a query → no response. Covers the
        // Esc-state `other` arm.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1bM", &theme());
        assert!(resp.is_empty());
    }

    #[test]
    fn osc_query_with_stray_esc_no_response() {
        // An ESC inside an OSC color query not followed by '\' breaks the `?`
        // payload, so the OSC body is no longer a clean query → no response at
        // the final BEL. Covers the OscEsc non-ST arm.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]10;?\x1bX\x07", &theme());
        assert!(resp.is_empty());
    }

    #[test]
    fn dcs_with_stray_esc_then_st_still_answered() {
        // An ESC inside a DCS not followed by '\' is treated as content; the DCS
        // body keeps accumulating and still completes at the real ST. Covers the
        // DcsEsc non-ST arm.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1bP+q4d73\x1bX\x1b\\", &theme());
        assert_eq!(resp, b"\x1bP0+r\x1b\\");
    }

    #[test]
    fn da1_query_split_across_chunks_still_answered() {
        // A DA1 query (`CSI c`) split so the final byte lands in the next call.
        let mut r = QueryResponder::new();
        let a = r.process(b"\x1b[", &theme());
        let b = r.process(b"c", &theme());
        assert!(a.is_empty(), "no response until the final byte arrives");
        assert_eq!(b, b"\x1b[?6c");
    }

    #[test]
    fn non_query_csi_with_high_final_byte_no_response() {
        // A CSI the responder does not recognize (final byte neither 'c' nor
        // 'p') yields no response.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b[6n", &theme());
        assert!(resp.is_empty(), "DSR request is not answered");
    }

    #[test]
    fn query_responder_default_equals_new() {
        let mut d = QueryResponder::default();
        let mut n = QueryResponder::new();
        // Both start in the normal state — processing the same input gives
        // the same response.
        let theme = ThemeConfig::default();
        assert_eq!(d.process(b"\x1b[c", &theme), n.process(b"\x1b[c", &theme),);
    }

    #[test]
    fn osc_color_unsupported_param_no_response() {
        // OSC 13 (cursor color) is not handled → no response.
        let mut r = QueryResponder::new();
        let resp = r.process(b"\x1b]13;?\x07", &theme());
        assert!(resp.is_empty(), "OSC 13 is not a recognized color query");
    }

    #[test]
    fn rgb_of_all_named_colors_returns_sensible_values() {
        // Every named color branch should produce a distinct-ish (non-zero
        // default) value; we don't hardcode exact values but verify they are
        // in the 0–255 range and the black/white extremes are correct.
        assert_eq!(rgb_of(Color::Black), (0, 0, 0));
        assert_eq!(rgb_of(Color::White), (255, 255, 255));
        assert_eq!(rgb_of(Color::Gray), (128, 128, 128));
        assert_eq!(rgb_of(Color::DarkGray), (64, 64, 64));
        // Red family.
        let (r, _, _) = rgb_of(Color::Red);
        assert!(r > 200, "Red has a high red channel");
        let (r, _, _) = rgb_of(Color::LightRed);
        assert!(r > 200, "LightRed has a high red channel");
        // Green family.
        let (_, g, _) = rgb_of(Color::Green);
        assert!(g > 100, "Green has a non-trivial green channel");
        let (_, g, _) = rgb_of(Color::LightGreen);
        assert!(g > 100, "LightGreen has a non-trivial green channel");
        // Blue family.
        let (_, _, b) = rgb_of(Color::Blue);
        assert!(b > 200, "Blue has a high blue channel");
        let (_, _, b) = rgb_of(Color::LightBlue);
        assert!(b > 100, "LightBlue has a non-trivial blue channel");
        // Yellow family — both channels non-zero.
        let (yr, yg, _) = rgb_of(Color::Yellow);
        assert!(yr > 100 && yg > 100, "Yellow has high r+g");
        let (yr, yg, _) = rgb_of(Color::LightYellow);
        assert!(yr > 100 && yg > 100, "LightYellow has high r+g");
        // Magenta family — r and b channels non-zero.
        let (mr, _, mb) = rgb_of(Color::Magenta);
        assert!(mr > 100 && mb > 100, "Magenta has high r+b");
        let (mr, _, mb) = rgb_of(Color::LightMagenta);
        assert!(mr > 100 && mb > 100, "LightMagenta has high r+b");
        // Cyan family — g and b channels non-zero.
        let (_, cg, cb) = rgb_of(Color::Cyan);
        assert!(cg > 100 && cb > 100, "Cyan has high g+b");
        let (_, cg, cb) = rgb_of(Color::LightCyan);
        assert!(cg > 100 && cb > 100, "LightCyan has high g+b");
        // Fallback (Reset or indexed) → GitHub dark default (13,17,23).
        assert_eq!(rgb_of(Color::Reset), (13, 17, 23));
        assert_eq!(rgb_of(Color::Indexed(42)), (13, 17, 23));
    }
}
