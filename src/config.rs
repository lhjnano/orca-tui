//! # Configuration (Feature: settings)
//!
//! User configuration loaded from `~/.config/orca-tui/config.toml` (or
//! `$XDG_CONFIG_HOME/orca-tui/config.toml`). Everything has a built-in default,
//! so orca-tui runs with zero configuration; the file only overrides what the
//! user sets.
//!
//! ```toml
//! # ~/.config/orca-tui/config.toml
//! default_agent = "claude"
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
}

impl Default for Config {
    /// Orca-inspired dark defaults: dark slate background, light text, a blue
    /// accent, and green/amber/red status semantics.
    fn default() -> Self {
        Self {
            default_agent: "claude".to_string(),
            layout: LayoutConfig::default(),
            theme: ThemeConfig::default(),
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
                    eprintln!("orca-tui: ignoring bad config at {}: {err}", path.display());
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
}

/// Resolve the config file path: `$XDG_CONFIG_HOME/orca-tui/config.toml`, else
/// `$HOME/.config/orca-tui/config.toml`. `None` if neither env var is set.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("orca-tui").join("config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/orca-tui/config.toml"))
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
        assert_eq!(c.default_agent, "claude");
        assert_eq!(c.layout.sidebar_width, 26);
        assert!(c.layout.show_status_bar);
        // theme parses to the expected RGB values.
        assert_eq!(c.theme.bg(), Color::Rgb(0x0d, 0x11, 0x17));
        assert_eq!(c.theme.accent(), Color::Rgb(0x58, 0xa6, 0xff));
        assert_eq!(c.theme.success(), Color::Rgb(0x3f, 0xb9, 0x50));
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
        // malformed hex → fallback (the documented default), never a panic.
        assert_eq!(t.color(&t.success, Color::Green), Color::Green);
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
    fn missing_file_uses_default() {
        // load_or_default never panics even with no config file / env.
        let cfg = Config::load_or_default();
        assert_eq!(cfg.default_agent, "claude");
    }

    #[test]
    fn serde_default_fill_works_for_partial_toml() {
        // An empty TOML document must still deserialize to defaults.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.layout.sidebar_width, 26);
    }
}
