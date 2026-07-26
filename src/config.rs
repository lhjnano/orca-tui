//! # Configuration (Feature: settings)
//!
//! User configuration loaded from `~/.config/orcatui/config.toml` (or
//! `$XDG_CONFIG_HOME/orcatui/config.toml`). Everything has a built-in default,
//! so orcatui runs with zero configuration; the file only overrides what the
//! user sets.
//!
//! ```toml
//! # ~/.config/orcatui/config.toml
//! default_agent = "bash"
//!
//! [layout]
//! sidebar_width = 26     # 0 hides the sidebar
//! show_status_bar = true
//!
//! [theme]
//! background = "#0d1117"
//! foreground = "#e6edf3"
//! accent   = "#58a6ff"   # focus borders, headers
//! success  = "#3fb950"   # running / done
//! warning  = "#d29922"   # idle / waiting
//! error    = "#f85149"   # failed / error
//! ```
//!
//! Missing file / invalid TOML / unknown fields never crash the app — they fall
//! back to [`Config::default`] and log a note to stderr.

#![allow(clippy::module_name_repetitions)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Agent binary used when none is detected / specified (e.g. orchestrate).
    pub default_agent: String,
    /// Layout options (sidebar width, status bar visibility).
    pub layout: LayoutConfig,
    /// Color theme (hex strings, parsed to ratatui [`Color::Rgb`]).
    pub theme: ThemeConfig,
    /// Orca daemon client settings (reconnection, timeouts).
    pub daemon: DaemonConfig,
}

impl Default for Config {
    /// Orca-inspired dark defaults: dark slate background, light text, a blue
    /// accent, and green/amber/red status semantics.
    fn default() -> Self {
        Self {
            default_agent: "bash".to_string(),
            layout: LayoutConfig::default(),
            theme: ThemeConfig::default(),
            daemon: DaemonConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from the user config dir, falling back to
    /// [`Config::default`] when the file is absent or unreadable.
    ///
    /// # Errors
    /// Never returns an error — a bad config is logged and replaced with the
    /// default so the app always starts.
    pub fn load_or_default() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("orcatui: ignoring bad config at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Load configuration strictly (used by tests / a future `config check`).
    ///
    /// # Errors
    /// Propagates IO / parse errors.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }
}

/// Layout options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Left sidebar width in columns. `0` hides the sidebar entirely.
    pub sidebar_width: u16,
    /// Draw the bottom status bar.
    pub show_status_bar: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 26,
            show_status_bar: true,
        }
    }
}

/// Orca daemon client settings — reconnection backoff, timeouts, retry limits.
///
/// All durations are in **seconds** (TOML-friendly integers). The exponential
/// backoff multiplies `reconnect_initial` by 2 each failure, capped at
/// `reconnect_max`. After `reconnect_max_attempts` failures the client gives
/// up and falls back to standalone mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Initial reconnect delay in seconds (first retry waits this long).
    pub reconnect_initial_secs: u64,
    /// Maximum reconnect delay in seconds (backoff caps here).
    pub reconnect_max_secs: u64,
    /// Maximum reconnect attempts before giving up (0 = unlimited).
    pub reconnect_max_attempts: u32,
    /// RPC timeout in seconds (how long to wait for a single request/response).
    pub rpc_timeout_secs: u64,
    /// Hello (handshake) timeout in seconds.
    pub hello_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            reconnect_initial_secs: 3,
            reconnect_max_secs: 30,
            reconnect_max_attempts: 0, // unlimited
            rpc_timeout_secs: 10,
            hello_timeout_secs: 5,
        }
    }
}

/// Hex-string color theme, mirroring Orca's dark UI + status accents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub success: String,
    pub warning: String,
    pub error: String,
    /// Slightly raised panel/box background (sidebars, bordered regions).
    pub background_panel: String,
    /// Further-raised element background (hover, nested boxes).
    pub background_element: String,
    /// Default border color for box panels.
    pub border: String,
    /// Border color for the focused/active box.
    pub border_active: String,
    /// Dimmed foreground text (labels, secondary info).
    pub text_muted: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            background: "#0d1117".to_string(),
            foreground: "#e6edf3".to_string(),
            accent: "#58a6ff".to_string(),
            success: "#3fb950".to_string(),
            warning: "#d29922".to_string(),
            error: "#f85149".to_string(),
            background_panel: "#161b22".to_string(), // canvas-subtle
            background_element: "#21262d".to_string(), // elevated
            border: "#30363d".to_string(),           // border-default
            border_active: "#58a6ff".to_string(),    // accent blue (focused)
            text_muted: "#8b949e".to_string(),       // fg-muted
        }
    }
}

impl ThemeConfig {
    /// Parse a `#rrggbb` string into a ratatui [`Color::Rgb`]; falls back to the
    /// provided default color on a malformed value so a bad theme never panics.
    #[must_use]
    pub fn color(&self, hex: &str, fallback: Color) -> Color {
        parse_hex(hex).unwrap_or(fallback)
    }
    #[must_use]
    pub fn bg(&self) -> Color {
        self.color(&self.background, Color::Rgb(0x0d, 0x11, 0x17))
    }
    #[must_use]
    pub fn fg(&self) -> Color {
        self.color(&self.foreground, Color::Rgb(0xe6, 0xed, 0xf3))
    }
    #[must_use]
    pub fn accent(&self) -> Color {
        self.color(&self.accent, Color::Rgb(0x58, 0xa6, 0xff))
    }
    #[must_use]
    pub fn success(&self) -> Color {
        self.color(&self.success, Color::Rgb(0x3f, 0xb9, 0x50))
    }
    #[must_use]
    pub fn warning(&self) -> Color {
        self.color(&self.warning, Color::Rgb(0xd2, 0x99, 0x22))
    }
    #[must_use]
    pub fn error(&self) -> Color {
        self.color(&self.error, Color::Rgb(0xf8, 0x51, 0x49))
    }
    #[must_use]
    pub fn panel(&self) -> Color {
        self.color(&self.background_panel, Color::Rgb(0x16, 0x1b, 0x22))
    }
    #[must_use]
    pub fn element(&self) -> Color {
        self.color(&self.background_element, Color::Rgb(0x21, 0x26, 0x2d))
    }
    #[must_use]
    pub fn border(&self) -> Color {
        self.color(&self.border, Color::Rgb(0x30, 0x36, 0x3d))
    }
    #[must_use]
    pub fn border_active(&self) -> Color {
        self.color(&self.border_active, Color::Rgb(0x58, 0xa6, 0xff))
    }
    #[must_use]
    pub fn muted(&self) -> Color {
        self.color(&self.text_muted, Color::Rgb(0x8b, 0x94, 0x9e))
    }
}

/// Resolve the config file path: `$XDG_CONFIG_HOME/orcatui/config.toml`, else
/// `$HOME/.config/orcatui/config.toml`. `None` if neither env var is set.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("orcatui").join("config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/orcatui/config.toml"))
}

/// Parse a `#rrggbb` / `#rgb` hex string into a ratatui RGB color.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#').unwrap_or(s);
    let (r, g, b) = if s.len() == 6 {
        (
            u8::from_str_radix(s.get(0..2)?, 16).ok()?,
            u8::from_str_radix(s.get(2..4)?, 16).ok()?,
            u8::from_str_radix(s.get(4..6)?, 16).ok()?,
        )
    } else if s.len() == 3 {
        let expand = |c: char| u8::from_str_radix(&format!("{c}{c}"), 16).ok();
        (
            expand(s.chars().nth(0)?)?,
            expand(s.chars().nth(1)?)?,
            expand(s.chars().nth(2)?)?,
        )
    } else {
        return None;
    };
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_orca_dark() {
        let c = Config::default();
        assert_eq!(c.default_agent, "bash");
        assert_eq!(c.layout.sidebar_width, 26);
        assert!(c.layout.show_status_bar);
        // theme parses to the expected RGB values.
        assert_eq!(c.theme.bg(), Color::Rgb(0x0d, 0x11, 0x17));
        assert_eq!(c.theme.accent(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(c.theme.success(), Color::Rgb(0x3f, 0xb9, 0x50));
        // New panel/border/muted tokens parse to their GitHub-dark defaults.
        assert_eq!(c.theme.panel(), Color::Rgb(0x16, 0x1b, 0x22));
        assert_eq!(c.theme.element(), Color::Rgb(0x21, 0x26, 0x2d));
        assert_eq!(c.theme.border(), Color::Rgb(0x30, 0x36, 0x3d));
        assert_eq!(c.theme.border_active(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(c.theme.muted(), Color::Rgb(0x8b, 0x94, 0x9e));
    }

    #[test]
    fn parse_hex_formats() {
        assert_eq!(parse_hex("#3fb950"), Some(Color::Rgb(0x3f, 0xb9, 0x50)));
        assert_eq!(parse_hex("3fb950"), Some(Color::Rgb(0x3f, 0xb9, 0x50)));
        assert_eq!(parse_hex("#abc"), Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_hex("#xyz"), None);
        assert_eq!(parse_hex("12345"), None);
    }

    #[test]
    fn bad_theme_hex_falls_back_safely() {
        let mut t = ThemeConfig::default();
        t.success = "not-a-color".into();
        t.border = "garbage".into();
        // malformed hex → fallback (the documented default), never a panic.
        assert_eq!(t.color(&t.success, Color::Green), Color::Green);
        // The new accessors also fall back to their hardcoded RGB when the
        // stored hex is unparseable.
        assert_eq!(t.border(), Color::Rgb(0x30, 0x36, 0x3d));
    }

    #[test]
    fn load_round_trips_full_toml() {
        let toml = r##"
default_agent = "codex"

[layout]
sidebar_width = 30
show_status_bar = false

[theme]
background = "#000000"
accent = "#ff00ff"
"##;
        let tmp = std::env::temp_dir().join(format!("orca-cfg-{}.toml", std::process::id()));
        std::fs::write(&tmp, toml).unwrap();
        let cfg = Config::load(&tmp).expect("parse");
        assert_eq!(cfg.default_agent, "codex");
        assert_eq!(cfg.layout.sidebar_width, 30);
        assert!(!cfg.layout.show_status_bar);
        assert_eq!(cfg.theme.bg(), Color::Rgb(0, 0, 0));
        assert_eq!(cfg.theme.accent(), Color::Rgb(0xff, 0x00, 0xff));
        // Unspecified theme keys fall back to defaults (serde default).
        assert_eq!(cfg.theme.success(), Color::Rgb(0x3f, 0xb9, 0x50));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn new_theme_tokens_round_trip_through_toml() {
        // All 11 theme fields specified; each new accessor must return the
        // over-ridden RGB, not the default.
        let toml = r##"
[theme]
background = "#0a0a0a"
foreground = "#f0f0f0"
accent = "#112233"
success = "#223344"
warning = "#334455"
error = "#445566"
background_panel = "#556677"
background_element = "#667788"
border = "#778899"
border_active = "#8899aa"
text_muted = "#99aabb"
"##;
        let tmp = std::env::temp_dir().join(format!("orca-cfg-new-{}.toml", std::process::id()));
        std::fs::write(&tmp, toml).unwrap();
        let cfg = Config::load(&tmp).expect("parse");
        assert_eq!(cfg.theme.panel(), Color::Rgb(0x55, 0x66, 0x77));
        assert_eq!(cfg.theme.element(), Color::Rgb(0x66, 0x77, 0x88));
        assert_eq!(cfg.theme.border(), Color::Rgb(0x77, 0x88, 0x99));
        assert_eq!(cfg.theme.border_active(), Color::Rgb(0x88, 0x99, 0xaa));
        assert_eq!(cfg.theme.muted(), Color::Rgb(0x99, 0xaa, 0xbb));
        let _ = std::fs::remove_file(&tmp);

        // Partial TOML: unspecified new fields fall back to the serde defaults.
        let partial = r##"
[theme]
border = "#abcdef"
"##;
        let tmp2 =
            std::env::temp_dir().join(format!("orca-cfg-partial-{}.toml", std::process::id()));
        std::fs::write(&tmp2, partial).unwrap();
        let cfg2 = Config::load(&tmp2).expect("parse");
        assert_eq!(cfg2.theme.border(), Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(cfg2.theme.panel(), Color::Rgb(0x16, 0x1b, 0x22));
        assert_eq!(cfg2.theme.border_active(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(cfg2.theme.muted(), Color::Rgb(0x8b, 0x94, 0x9e));
        let _ = std::fs::remove_file(&tmp2);
    }

    #[test]
    fn missing_file_uses_default() {
        // load_or_default never panics even with no config file / env.
        let cfg = Config::load_or_default();
        assert_eq!(cfg.default_agent, "bash");
    }

    #[test]
    fn serde_default_fill_works_for_partial_toml() {
        // An empty TOML document must still deserialize to defaults.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.layout.sidebar_width, 26);
    }

    #[test]
    fn all_theme_accessors_match_defaults() {
        // Every accessor (including fg/warning/error, which no earlier test
        // exercised directly) must return the documented GitHub-dark RGB for a
        // freshly-defaulted ThemeConfig.
        let t = ThemeConfig::default();
        assert_eq!(t.bg(), Color::Rgb(0x0d, 0x11, 0x17));
        assert_eq!(t.fg(), Color::Rgb(0xe6, 0xed, 0xf3));
        assert_eq!(t.accent(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(t.success(), Color::Rgb(0x3f, 0xb9, 0x50));
        assert_eq!(t.warning(), Color::Rgb(0xd2, 0x99, 0x22));
        assert_eq!(t.error(), Color::Rgb(0xf8, 0x51, 0x49));
        assert_eq!(t.panel(), Color::Rgb(0x16, 0x1b, 0x22));
        assert_eq!(t.element(), Color::Rgb(0x21, 0x26, 0x2d));
        assert_eq!(t.border(), Color::Rgb(0x30, 0x36, 0x3d));
        assert_eq!(t.border_active(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(t.muted(), Color::Rgb(0x8b, 0x94, 0x9e));
    }

    #[test]
    fn daemon_config_defaults() {
        // DaemonConfig had no dedicated coverage; pin the documented defaults.
        let d = DaemonConfig::default();
        assert_eq!(d.reconnect_initial_secs, 3);
        assert_eq!(d.reconnect_max_secs, 30);
        assert_eq!(d.reconnect_max_attempts, 0, "0 = unlimited");
        assert_eq!(d.rpc_timeout_secs, 10);
        assert_eq!(d.hello_timeout_secs, 5);
        // And it is wired into Config::default.
        assert_eq!(Config::default().daemon.reconnect_initial_secs, 3);
    }

    #[test]
    fn load_or_default_and_config_path_branches() {
        // This test manipulates process-global env vars. To stay safe under
        // the parallel test runner (and panic-safe on a failed assertion) it
        // (a) performs all env reads FIRST, (b) restores env BEFORE asserting,
        // and (b) keeps any config it writes defaulting to default_agent =
        // "bash" so a concurrent `missing_file_uses_default` can't observe a
        // surprising value (load_or_default always falls back to a valid
        // default anyway).

        // --- config_path XDG branch (line 247) + the three load_or_default
        //     outcomes (lines 75-76 parse-Ok, 77-79 bad-toml, 82 read-Err) ---
        let xdg_prev = std::env::var_os("XDG_CONFIG_HOME");
        let temp = std::env::temp_dir().join(format!("orca-cfg-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("orcatui")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &temp);

        // line 247: XDG set → path resolves under it.
        let path_xdg = config_path();

        // line 82: file absent → read_to_string Err → default.
        let cfg_missing = Config::load_or_default();

        // lines 75-76: valid file present → parsed config returned.
        std::fs::write(
            temp.join("orcatui").join("config.toml"),
            "default_agent = \"bash\"\n[theme]\naccent = \"#aabbcc\"\n",
        )
        .unwrap();
        let cfg_valid = Config::load_or_default();

        // lines 77-79: unparseable file → logged + replaced with default.
        std::fs::write(
            temp.join("orcatui").join("config.toml"),
            "this is = = not valid toml [[[",
        )
        .unwrap();
        let cfg_bad = Config::load_or_default();

        // Restore XDG (and clean up) BEFORE asserting so a panic can't leak.
        match &xdg_prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&temp);

        assert_eq!(
            path_xdg,
            Some(temp.join("orcatui").join("config.toml")),
            "line 247: XDG_CONFIG_HOME drives the path"
        );
        assert_eq!(
            cfg_missing.default_agent, "bash",
            "line 82: missing file → default"
        );
        assert_eq!(cfg_valid.default_agent, "bash", "lines 75-76: parsed");
        assert_eq!(
            cfg_valid.theme.accent(),
            Color::Rgb(0xaa, 0xbb, 0xcc),
            "lines 75-76: the custom accent from disk was parsed"
        );
        assert_eq!(
            cfg_bad.default_agent, "bash",
            "lines 77-79: bad toml → default"
        );

        // --- config_path None branch (line 72): neither HOME nor XDG set ---
        let home_prev = std::env::var_os("HOME");
        let xdg_prev2 = std::env::var_os("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
        let cfg_no_env = Config::load_or_default();
        // Restore FIRST, then assert.
        match &home_prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match &xdg_prev2 {
            Some(x) => std::env::set_var("XDG_CONFIG_HOME", x),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert_eq!(
            cfg_no_env.default_agent, "bash",
            "line 72: no HOME/XDG → config_path None → default"
        );
    }
}
