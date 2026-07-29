//! # orcatui (binary)
//!
//! Thin entry point. All parsing and dispatch lives in the [`orcatui`]
//! library crate's [`cli`](orcatui::cli) module; the binary just runs it and
//! maps the result to a process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Install the crash-logging panic hook FIRST, before anything that can
    // panic, so an edge-case crash (e.g. shrinking the window past a layout
    // underflow) leaves a detailed report in last-crash.log instead of dying
    // silently.
    orcatui::crashlog::install();
    match orcatui::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orcatui: {err:#}");
            ExitCode::FAILURE
        }
    }
}
