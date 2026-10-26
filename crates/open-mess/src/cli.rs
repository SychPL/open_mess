//! The `om` command line.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::daemon;

#[derive(Debug, Parser)]
#[command(name = "om", bin_name = "om", version, about = "open_mess: your messengers, for AI agents", arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the open_mess daemon (one per Windows user).
    Daemon,
}

/// Runs the `om` command line and returns its exit code.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // Help and --version go to stdout and succeed; anything else is bad usage.
            return if err.use_stderr() { ExitCode::from(3) } else { ExitCode::SUCCESS };
        }
    };
    match cli.command {
        Command::Daemon => match daemon::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(1)
            }
        },
    }
}
