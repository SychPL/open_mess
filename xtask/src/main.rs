//! Repository tasks.
//!
//! `cargo xtask ci` runs the same gates as CI (fmt, clippy, the wasm32-wasip2 module build, tests);
//! run it before every commit.
//! `cargo xtask hooks` points git at `.githooks/`, which sends every commit to the Codex and GLM reviewers.

use std::path::Path;
use std::process::{Command, ExitCode};

fn run(program: &str, args: &[&str]) -> bool {
    println!("> {program} {}", args.join(" "));
    Command::new(program).args(args).status().is_ok_and(|status| status.success())
}

/// Every `modules/<dir>` is the package `open-mess-<dir>`.
fn modules() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../modules");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("Cargo.toml").exists())
        .map(|entry| format!("open-mess-{}", entry.file_name().to_string_lossy()))
        .collect();
    names.sort();
    names
}

fn ci() -> bool {
    // The components the core loads; their glue only exists for wasm32 (needs `rustup target add
    // wasm32-wasip2`), so they get their own clippy pass. They are built before the tests, which load them.
    let mut wasm = vec!["--target", "wasm32-wasip2", "--locked"];
    let modules = modules();
    for module in &modules {
        wasm.extend(["-p", module]);
    }
    let steps: [Vec<&str>; 5] = [
        vec!["fmt", "--all", "--check"],
        vec!["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"],
        [&["clippy"][..], &wasm, &["--", "-D", "warnings"]].concat(),
        [&["build"][..], &wasm].concat(),
        vec!["test", "--workspace", "--locked"],
    ];
    // `all` stops at the first failing step, like CI.
    !modules.is_empty() && steps.iter().all(|args| run("cargo", args))
}

fn main() -> ExitCode {
    let ok = match std::env::args().nth(1).as_deref() {
        Some("ci") => ci(),
        Some("hooks") => run("git", &["config", "core.hooksPath", ".githooks"]),
        _ => {
            eprintln!("usage: cargo xtask <ci|hooks>");
            false
        }
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
