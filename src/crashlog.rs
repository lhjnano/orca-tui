//! Crash logging: install a panic hook that writes a detailed report
//! (timestamp, panic location + message, and a forced backtrace) to a file so
//! an edge-case crash — e.g. shrinking the window past a layout underflow —
//! leaves a trace instead of vanishing silently.
//!
//! The report goes to `<data_dir>/orcatui/last-crash.log`
//! (`~/.local/share/orcatui/last-crash.log` on Linux), with a `/tmp` fallback.
//! It is ALSO printed to stderr, and the default hook still runs (so the
//! process aborts normally with the standard message).

use std::backtrace::Backtrace;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where crash reports are written. `~/.local/share/orcatui/last-crash.log`,
/// or `/tmp/orcatui-last-crash.log` if no data dir is resolvable.
fn crash_log_path() -> PathBuf {
    let mut base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.push("orcatui");
    let _ = std::fs::create_dir_all(&base);
    base.push("last-crash.log");
    base
}

/// Install the crash-logging panic hook. Call once at startup, before anything
/// that can panic. Idempotent-ish: each call replaces the current hook, so the
/// last one wins (fine for a single-call startup).
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Force a backtrace regardless of RUST_BACKTRACE so a crash always has
        // one to read. (`Backtrace::capture()` only fills in when the env var
        // is set; `force_capture` always does.)
        let bt = Backtrace::force_capture();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let report = format!(
            "orcatui crash report\n\
             time: {secs} (unix seconds)\n\
             panic: {info}\n\
             \n\
             backtrace:\n\
             {bt}"
        );

        let path = crash_log_path();
        match std::fs::File::create(&path).and_then(|mut f| f.write_all(report.as_bytes())) {
            Ok(()) => eprintln!(
                "orcatui: panicked — crash log written to {}",
                path.display()
            ),
            Err(e) => eprintln!(
                "orcatui: panicked — could not write crash log to {}: {e}",
                path.display()
            ),
        }
        eprintln!("{report}");

        // Chain to the default hook for the standard abort behavior/message.
        default_hook(info);
    }));
}
