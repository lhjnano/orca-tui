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

/// Where crash reports are written. Each crash gets its OWN timestamped file
/// (`crash-<unix_secs>.log`) so multiple crashes in a session are all
/// preserved (the old `last-crash.log` only kept the most recent). A
/// `last-crash.log` symlink/copy is also written for convenience.
fn crash_log_paths() -> (PathBuf, PathBuf) {
    let mut base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.push("orcatui");
    let _ = std::fs::create_dir_all(&base);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let timestamped = base.join(format!("crash-{secs}.log"));
    let latest = base.join("last-crash.log");
    (timestamped, latest)
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

        let path = crash_log_paths();
        let report_written = std::fs::File::create(&path.0)
            .and_then(|mut f| f.write_all(report.as_bytes()))
            .is_ok();
        // Also copy to last-crash.log for convenience.
        let _ = std::fs::write(&path.1, &report);
        if report_written {
            eprintln!(
                "orcatui: panicked — crash log written to {}",
                path.0.display()
            )
        } else {
            eprintln!("orcatui: panicked — could not write crash log");
        }
        eprintln!("{report}");

        // Chain to the default hook for the standard abort behavior/message.
        default_hook(info);
    }));
}
