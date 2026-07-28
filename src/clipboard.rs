//! Zero-dependency system clipboard access.
//!
//! Copies text by shelling out to the first available platform clipboard
//! tool. No crate is pulled in, so `cargo test` stays green on headless CI
//! (where no clipboard daemon runs and no native lib is linkable).

use std::io::Write;
use std::path::Path;

/// True if `binary` exists as an executable file somewhere on `$PATH`.
///
/// Mirrors the private `binary_on_path` in [`crate::agent`] so this module
/// stays self-contained without making that helper `pub(crate)`.
fn which(binary: &str) -> bool {
    if binary.is_empty() {
        return false;
    }
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if is_executable(&candidate) {
            return true;
        }
        #[cfg(windows)]
        if is_executable(&dir.join(format!("{binary}.exe"))) {
            return true;
        }
    }
    false
}

/// Whether a path is an executable regular file (unix: any execute bit set).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(md) => md.is_file() && (md.permissions().mode() & 0o111 != 0),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

/// Failures from [`copy`].
#[derive(Debug)]
pub enum CopyError {
    /// No supported clipboard tool found on PATH.
    NoClipboardTool,
    /// Spawning the tool failed (program name + message).
    Spawn(String, String),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::NoClipboardTool => {
                write!(
                    f,
                    "no clipboard tool found (install xclip/xsel/wl-copy/pbcopy)"
                )
            }
            CopyError::Spawn(p, m) => write!(f, "failed to spawn {p}: {m}"),
        }
    }
}

impl std::error::Error for CopyError {}

/// Copy `text` to the system clipboard by shelling out to the first available
/// platform tool. Zero-dependency; graceful error if none is present.
///
/// Auto-detection order (first executable on `$PATH` wins):
/// - macOS: `pbcopy`
/// - Windows: `clip`
/// - Linux/BSD: `xsel --clipboard --input`, else `xclip -selection clipboard`,
///   else `wl-copy`, else **`clip.exe`** (WSL → Windows clipboard via interop),
///   else `powershell.exe -Command "$input | Set-Clipboard"` (WSL fallback).
///
/// Pass `command = Some("…")` to override the auto-detection (e.g. force
/// `clip.exe` on WSL, or `termux-clipboard-set` on Termux). The override is
/// shell-split on whitespace into program + args; the text is piped to stdin.
pub fn copy(text: &str) -> Result<(), CopyError> {
    copy_with(text, None)
}

/// Like [`copy`], but with an optional user-configured command override.
pub fn copy_with(text: &str, command: Option<&str>) -> Result<(), CopyError> {
    let (program, args): (String, Vec<String>) = if let Some(cmd) = command {
        // User override: shell-split into program + args (whitespace, v1).
        let mut parts = cmd.split_whitespace();
        let program = parts.next().ok_or(CopyError::NoClipboardTool)?.to_string();
        let args = parts.map(String::from).collect();
        (program, args)
    } else if cfg!(target_os = "macos") {
        ("pbcopy".to_string(), Vec::new())
    } else if cfg!(target_os = "windows") {
        ("clip".to_string(), Vec::new())
    } else if which("xsel") {
        (
            "xsel".to_string(),
            vec!["--clipboard".to_string(), "--input".to_string()],
        )
    } else if which("xclip") {
        (
            "xclip".to_string(),
            vec!["-selection".to_string(), "clipboard".to_string()],
        )
    } else if which("wl-copy") {
        ("wl-copy".to_string(), Vec::new())
    } else if which("clip.exe") {
        // WSL: pipe stdin to Windows' clip.exe (Windows clipboard via interop).
        ("clip.exe".to_string(), Vec::new())
    } else if which("powershell.exe") {
        // WSL fallback when clip.exe is absent.
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "$input | Set-Clipboard".to_string(),
            ],
        )
    } else {
        return Err(CopyError::NoClipboardTool);
    };
    let mut child = std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| CopyError::Spawn(program.clone(), e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    // Best-effort wait; ignore failure (the pipe close already delivered the text).
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_returns_false_for_nonexistent_binary() {
        assert!(!which("definitely_not_a_real_binary_zzz123"));
    }

    #[test]
    fn which_returns_true_for_a_common_binary_if_present() {
        // On unix `ls` exists; on windows `cmd` exists. OR-ing both keeps the
        // assertion true on every mainstream platform without depending on a
        // single specific name.
        let found = which("ls") || which("cmd");
        assert_eq!(found, cfg!(unix) || cfg!(windows));
    }

    #[test]
    fn copy_error_type_is_display_and_error() {
        let err = CopyError::NoClipboardTool;
        let s = err.to_string();
        assert!(
            s.contains("clipboard"),
            "display should mention clipboard, got: {s}"
        );
        // Satisfies the trait bound by constructing and formatting; no
        // explicit trait-object assert required.
        let _: &dyn std::error::Error = &CopyError::Spawn("pbcopy".into(), "boom".into());
        let spawn_str = CopyError::Spawn("xclip".into(), "denied".into()).to_string();
        assert!(spawn_str.contains("xclip") && spawn_str.contains("denied"));
    }

    #[test]
    fn copy_does_not_panic_without_tool() {
        // CI has no clipboard tool/daemon; we only require that `copy` does
        // not panic and returns a sane `Result`.
        let _ = copy("test");
    }
}
