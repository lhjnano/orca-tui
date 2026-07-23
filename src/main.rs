//! # orca-tui (binary)
//!
//! Thin entry point. All parsing and dispatch lives in the [`orca_tui`]
//! library crate's [`cli`](orca_tui::cli) module; the binary just runs it and
//! maps the result to a process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    match orca_tui::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orca-tui: {err:#}");
            ExitCode::FAILURE
        }
    }
}
