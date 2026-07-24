//! # orcatui (binary)
//!
//! Thin entry point. All parsing and dispatch lives in the [`orcatui`]
//! library crate's [`cli`](orcatui::cli) module; the binary just runs it and
//! maps the result to a process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    match orcatui::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orcatui: {err:#}");
            ExitCode::FAILURE
        }
    }
}
