//! `om` - the open_mess daemon and CLI.
//!
//! The design lives in `.specs/2026-10-26-open-mess-design.md`.

use std::process::ExitCode;

fn main() -> ExitCode {
    open_mess::cli::run()
}
