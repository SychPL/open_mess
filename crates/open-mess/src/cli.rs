//! The `om` command line.

use std::process::ExitCode;

/// Runs the `om` command line and returns its exit code.
pub fn run() -> ExitCode {
    println!("om {}", env!("CARGO_PKG_VERSION"));
    ExitCode::SUCCESS
}
