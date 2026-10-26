//! Repository tasks.
//!
//! `cargo xtask ci` runs the same gates as CI (fmt, clippy, tests); run it before every commit.
//! `cargo xtask hooks` points git at `.githooks/`, which sends every commit to the Codex and GLM reviewers.

use std::process::{Command, ExitCode};

const CI_STEPS: &[&[&str]] = &[
    &["fmt", "--all", "--check"],
    &["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"],
    &["test", "--workspace", "--locked"],
];

fn run(program: &str, args: &[&str]) -> bool {
    println!("> {program} {}", args.join(" "));
    Command::new(program).args(args).status().is_ok_and(|status| status.success())
}

fn main() -> ExitCode {
    let ok = match std::env::args().nth(1).as_deref() {
        // `all` stops at the first failing step, like CI.
        Some("ci") => CI_STEPS.iter().all(|args| run("cargo", args)),
        Some("hooks") => run("git", &["config", "core.hooksPath", ".githooks"]),
        _ => {
            eprintln!("usage: cargo xtask <ci|hooks>");
            false
        }
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
