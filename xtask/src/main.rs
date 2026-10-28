//! Repository tasks.
//!
//! `cargo xtask ci` runs the same gates as CI (fmt, clippy, the wasm32-wasip2 module build, tests);
//! run it before every commit.
//! `cargo xtask hooks` points git at `.githooks/`, which sends every commit to the Codex and GLM reviewers.
//! `cargo xtask release-keygen` creates the release signing key once; `cargo xtask release <x.y.z>`
//! tags, builds, signs and publishes a release (spec 2026-10-27-self-update, section 4).

// A developer tool: printing here may panic, it never takes the daemon down.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

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
    if modules.is_empty() {
        eprintln!("no packages found in modules/");
        return false;
    }
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
    steps.iter().all(|args| run("cargo", args))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Runs a command in the repository and fails with its name when it does not succeed.
fn step(program: &str, args: &[&str]) -> Result<(), String> {
    if run(program, args) { Ok(()) } else { Err(format!("`{program} {}` failed", args.join(" "))) }
}

/// Runs a command in the repository and returns its trimmed stdout.
fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program).args(args).output().map_err(|e| format!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!("`{program} {}` failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn valid_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The commits (full shas) that have no `<first 7 chars>.processed` file in `reviews_dir`.
fn unprocessed_commits(shas: &[String], reviews_dir: &Path) -> Vec<String> {
    shas.iter().filter(|sha| !reviews_dir.join(format!("{}.processed", sha.get(..7).unwrap_or(sha))).exists()).cloned().collect()
}

/// True when `diff` (git unified diff) changes only `version =` lines of `Cargo.toml` and anything in `Cargo.lock`.
fn only_version_changed(diff: &str) -> bool {
    let mut file = "";
    let mut seen = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            file = rest.split(" b/").nth(1).unwrap_or("");
            if file != "Cargo.toml" && file != "Cargo.lock" {
                return false;
            }
            seen = true;
        } else if line.starts_with("+++") || line.starts_with("---") {
            continue;
        } else if let Some(content) = line.strip_prefix(['+', '-'])
            && file == "Cargo.toml"
            && !content.trim_start().starts_with("version =")
        {
            return false;
        }
    }
    seen
}

/// Sets the `version` of `[workspace.package]`, keeping every line ending as it was; `None` when the
/// section or the line is missing.
fn set_workspace_version(toml: &str, version: &str) -> Option<String> {
    let mut in_section = false;
    let mut done = false;
    let out: String = toml
        .split_inclusive('\n')
        .map(|line| {
            if line.starts_with('[') {
                in_section = line.trim() == "[workspace.package]";
            } else if in_section && !done && line.starts_with("version =") {
                done = true;
                let end = &line[line.trim_end().len()..];
                return format!("version = \"{version}\"{end}");
            }
            line.to_string()
        })
        .collect();
    done.then_some(out)
}

/// What the owner must run to undo or finish a release that failed after `stage`.
/// `files` are the names in `target/release-dist`, listed one by one: `gh release` takes file paths,
/// not patterns, and PowerShell does not expand a `*`.
fn recovery(stage: Stage, tag: &str, files: &[String]) -> String {
    match stage {
        Stage::Clean => String::new(),
        Stage::Edited => "git checkout HEAD -- Cargo.toml Cargo.lock".to_string(),
        Stage::Committed => format!("git tag -d {tag}\ngit reset --hard HEAD~1"),
        Stage::Pushed => {
            let list: String = files.iter().map(|name| format!(" target/release-dist/{name}")).collect();
            format!(
                "# no release {tag} on GitHub yet:\ngh release create {tag} --title {tag} --notes \"\"{list}\n\
                 # the release is there, with some files missing (a failed create can leave a draft):\n\
                 gh release upload {tag}{list} --clobber\ngh release edit {tag} --draft=false"
            )
        }
    }
}

/// Signs a test message with `secret` and verifies it with `public`: a secret key that is not the
/// pair of keys/release.pub fails here, before any commit or tag.
fn check_key_pair(public: &minisign::PublicKey, secret: &minisign::SecretKey) -> Result<(), String> {
    const MESSAGE: &[u8] = b"open_mess release key check";
    let signature = minisign::sign(None, secret, MESSAGE, None, None).map_err(|e| format!("cannot sign with the secret key: {e}"))?;
    minisign::verify(public, &signature, std::io::Cursor::new(MESSAGE), true, false, false)
        .map_err(|e| format!("the secret key is not the pair of keys/release.pub: {e}"))
}

/// How far `release` got; decides which recovery commands are printed on failure.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Clean,
    /// Cargo.toml / Cargo.lock modified, nothing committed.
    Edited,
    /// The Release commit and the tag exist locally.
    Committed,
    /// Commit and tag are on origin; the GitHub release is missing or lacks files.
    Pushed,
}

/// The published name of a module's wasm output: `open_mess_<name>.wasm` -> `<name>.wasm` with `_` as `-`.
/// `Ok(None)` for test guests; `Err` when the result would fail the daemon's name whitelist.
fn module_release_name(file: &str) -> Result<Option<String>, String> {
    let Some(name) = file.strip_prefix("open_mess_").and_then(|rest| rest.strip_suffix(".wasm")) else {
        return Err(format!("{file} is not an open_mess_<name>.wasm output"));
    };
    if name.starts_with("test_") {
        return Ok(None);
    }
    let name = name.replace('_', "-");
    let allowed = (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if allowed { Ok(Some(format!("{name}.wasm"))) } else { Err(format!("{name}.wasm is not an allowed release file name")) }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The release manifest JSON (spec section 4): `files` are `(published name, contents)`.
fn build_manifest(version: &str, files: &[(String, Vec<u8>)]) -> String {
    let files: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, bytes)| {
            let kind = if name == "om.exe" { "exe" } else { "module" };
            serde_json::json!({ "name": name, "kind": kind, "size": bytes.len(), "sha256": hex_sha256(bytes) })
        })
        .collect();
    serde_json::json!({ "version": version, "files": files }).to_string()
}

fn secret_key_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("OPEN_MESS_RELEASE_KEY") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set; set OPEN_MESS_RELEASE_KEY")?;
    Ok(PathBuf::from(home).join(".open_mess").join("release.key"))
}

/// Writes a new file; fails when it exists.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Creates the release key pair once. Never overwrites a key and never writes the secret into the repository.
fn release_keygen() -> Result<(), String> {
    let public = repo_root().join("keys").join("release.pub");
    let secret = secret_key_path()?;
    if secret.exists() {
        return Err(format!("{} already exists; not overwriting it", secret.display()));
    }
    // A fresh checkout holds a placeholder whose comment says there is no key yet.
    let placeholder = std::fs::read_to_string(&public).is_ok_and(|text| text.contains("no release key yet"));
    if public.exists() && !placeholder {
        return Err(format!("{} already holds a key; not overwriting it", public.display()));
    }
    let dir = secret.parent().ok_or("the secret key path has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let repo = repo_root().canonicalize().map_err(|e| e.to_string())?;
    if dir.canonicalize().map_err(|e| e.to_string())?.starts_with(&repo) {
        return Err("the secret key must not live inside the repository".into());
    }
    let pair = minisign::KeyPair::generate_encrypted_keypair(None).map_err(|e| format!("key generation failed: {e}"))?;
    let secret_bytes = pair.sk.to_box(None).map_err(|e| e.to_string())?.to_bytes();
    let public_bytes = pair.pk.to_box().map_err(|e| e.to_string())?.to_bytes();
    write_new(&secret, &secret_bytes)?;
    std::fs::write(&public, public_bytes).map_err(|e| format!("cannot write {}: {e}", public.display()))?;
    println!("secret key: {} (back it up; losing it means no more updates)", secret.display());
    println!("public key: {} (commit it; `om` compiles it in)", public.display());
    Ok(())
}

/// `cargo xtask release <version>`: spec section 4, steps 1-6, stopping at the first failure.
fn release(version: &str) -> Result<(), String> {
    if !valid_version(version) {
        return Err(format!("{version:?} is not a version like 1.2.3"));
    }
    let tag = format!("v{version}");
    let mut stage = Stage::Clean;
    let mut files = Vec::new();
    let result = release_steps(version, &tag, &mut stage, &mut files);
    if result.is_err() {
        let commands = recovery(stage, &tag, &files);
        if !commands.is_empty() {
            eprintln!("the release stopped half way; run:\n{commands}");
        }
    }
    result
}

/// `assets` gets the names of the files in `target/release-dist`, for the recovery commands.
fn release_steps(version: &str, tag: &str, stage: &mut Stage, assets: &mut Vec<String>) -> Result<(), String> {
    let tag = tag.to_string();
    let root = repo_root();
    std::env::set_current_dir(&root).map_err(|e| e.to_string())?;

    // 1. Preconditions. The keys are checked first so a missing key cannot strand a tagged release.
    let key_path = secret_key_path()?;
    if !key_path.exists() {
        return Err(format!("no secret key at {}; run `cargo xtask release-keygen`", key_path.display()));
    }
    let public =
        minisign::PublicKey::from_file(root.join("keys").join("release.pub")).map_err(|e| format!("keys/release.pub is not a release key: {e}"))?;
    // Unlock now (the password is asked here): a wrong password must not strand a commit and tag.
    let secret = minisign::SecretKey::from_file(&key_path, None).map_err(|e| format!("cannot unlock the secret key: {e}"))?;
    check_key_pair(&public, &secret)?;
    if !capture("git", &["status", "--porcelain"])?.is_empty() {
        return Err("the working tree is not clean".into());
    }
    if capture("git", &["rev-parse", "--abbrev-ref", "HEAD"])? != "main" {
        return Err("not on main".into());
    }
    step("git", &["fetch", "origin", "main", "--tags"])?;
    if capture("git", &["rev-parse", "HEAD"])? != capture("git", &["rev-parse", "origin/main"])? {
        return Err("main differs from origin/main".into());
    }
    if !capture("git", &["tag", "--list", &tag])?.is_empty() {
        return Err(format!("tag {tag} already exists"));
    }
    if !ci() {
        return Err("cargo xtask ci failed".into());
    }
    let range = match capture("git", &["describe", "--tags", "--abbrev=0", "--match", "v*"]) {
        Ok(previous) => format!("{previous}..HEAD"),
        Err(_) => "HEAD".to_string(),
    };
    let shas: Vec<String> = capture("git", &["rev-list", &range])?.lines().map(str::to_string).collect();
    let open = unprocessed_commits(&shas, &root.join(".reviews"));
    if !open.is_empty() {
        return Err(format!("commits without a .reviews/<sha>.processed file: {}", open.join(", ")));
    }

    // 2. Version commit and tag; the diff must be only the version and the lockfile.
    let manifest_path = root.join("Cargo.toml");
    let toml = std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
    let updated = set_workspace_version(&toml, version).ok_or("no version in [workspace.package] of Cargo.toml")?;
    std::fs::write(&manifest_path, updated).map_err(|e| e.to_string())?;
    *stage = Stage::Edited;
    step("cargo", &["update", "--workspace"])?;
    if !only_version_changed(&capture("git", &["diff", "--unified=0"])?) {
        return Err("the version bump changed more than the version and Cargo.lock; reset and look".into());
    }
    step("git", &["add", "Cargo.toml", "Cargo.lock"])?;
    step("git", &["commit", "-m", &format!("Release {version}")])?;
    *stage = Stage::Committed;
    step("git", &["tag", &tag])?;

    // 3. Build om.exe and the platform modules.
    step("cargo", &["build", "--release", "--locked", "-p", "open-mess"])?;
    let mut files = vec![("om.exe".to_string(), std::fs::read(root.join("target/release/om.exe")).map_err(|e| format!("om.exe: {e}"))?)];
    for module in modules() {
        let dir = module.strip_prefix("open-mess-").unwrap_or(&module);
        if dir.starts_with("test-") {
            continue;
        }
        step("cargo", &["build", "--release", "--locked", "--target", "wasm32-wasip2", "-p", &module])?;
        let output = format!("open_mess_{}.wasm", dir.replace('-', "_"));
        let Some(name) = module_release_name(&output)? else { continue };
        files.push((name, std::fs::read(root.join("target/wasm32-wasip2/release").join(&output)).map_err(|e| format!("{output}: {e}"))?));
    }

    // 4-5. Manifest and its minisign signature (the password is asked here).
    let manifest = build_manifest(version, &files);
    let signature = minisign::sign(Some(&public), &secret, manifest.as_bytes(), None, None).map_err(|e| format!("signing failed: {e}"))?;
    let dist = root.join("target/release-dist");
    std::fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    for (name, bytes) in &files {
        std::fs::write(dist.join(name), bytes).map_err(|e| e.to_string())?;
        assets.push(name.clone());
    }
    std::fs::write(dist.join("manifest.json"), &manifest).map_err(|e| e.to_string())?;
    std::fs::write(dist.join("manifest.json.minisig"), signature.to_string()).map_err(|e| e.to_string())?;
    assets.extend(["manifest.json".to_string(), "manifest.json.minisig".to_string()]);

    // 6. Publish. Atomic: the commit and the tag land on origin together, or neither does.
    step("git", &["push", "--atomic", "origin", "main", &tag])?;
    *stage = Stage::Pushed;
    let paths: Vec<String> = assets.iter().map(|name| dist.join(name).to_string_lossy().into_owned()).collect();
    let mut args = vec!["release", "create", &tag, "--title", &tag, "--notes", ""];
    args.extend(paths.iter().map(String::as_str));
    step("gh", &args)
}

fn report(result: Result<(), String>) -> bool {
    result.inspect_err(|e| eprintln!("error: {e}")).is_ok()
}

fn main() -> ExitCode {
    let ok = match std::env::args().nth(1).as_deref() {
        Some("ci") => ci(),
        Some("hooks") => run("git", &["config", "core.hooksPath", ".githooks"]),
        Some("release-keygen") => report(release_keygen()),
        Some("release") => match std::env::args().nth(2) {
            Some(version) => report(release(&version)),
            None => {
                eprintln!("usage: cargo xtask release <x.y.z>");
                false
            }
        },
        _ => {
            eprintln!("usage: cargo xtask <ci|hooks|release-keygen|release>");
            false
        }
    };
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprocessed_commits_are_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("abcdef1.processed"), "ok").expect("write");
        let shas = ["abcdef1234567890".to_string(), "1234567aaaaaaaaa".to_string()];
        assert_eq!(unprocessed_commits(&shas, dir.path()), vec!["1234567aaaaaaaaa".to_string()]);
        // A missing folder means nothing is processed.
        assert_eq!(unprocessed_commits(&shas, &dir.path().join("none")).len(), 2);
    }

    #[test]
    fn release_diff_with_other_changes_is_refused() {
        let ok = "diff --git a/Cargo.toml b/Cargo.toml\n--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -5 +5 @@\n-version = \"0.0.0\"\n+version = \"0.1.0\"\ndiff --git a/Cargo.lock b/Cargo.lock\n--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n-version = \"0.0.0\"\n+version = \"0.1.0\"\n";
        assert!(only_version_changed(ok));
        assert!(!only_version_changed(""));
        let other_file = ok.replace("a/Cargo.lock b/Cargo.lock", "a/src/lib.rs b/src/lib.rs");
        assert!(!only_version_changed(&other_file));
        let other_line = ok.replace("+version = \"0.1.0\"\ndiff", "+edition = \"2024\"\ndiff");
        assert!(!only_version_changed(&other_line));
    }

    #[test]
    fn manifest_lists_sizes_and_sums() {
        let files = [("om.exe".to_string(), b"abc".to_vec()), ("teams.wasm".to_string(), Vec::new())];
        let json: serde_json::Value = serde_json::from_str(&build_manifest("1.2.3", &files)).expect("json");
        assert_eq!(json["version"], "1.2.3");
        assert_eq!(json["files"][0]["name"], "om.exe");
        assert_eq!(json["files"][0]["kind"], "exe");
        assert_eq!(json["files"][0]["size"], 3);
        assert_eq!(json["files"][0]["sha256"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(json["files"][1]["kind"], "module");
        assert_eq!(json["files"][1]["size"], 0);
    }

    #[test]
    fn test_guests_are_not_released() {
        assert_eq!(module_release_name("open_mess_test_echo.wasm"), Ok(None));
        assert_eq!(module_release_name("open_mess_teams.wasm"), Ok(Some("teams.wasm".to_string())));
        assert_eq!(module_release_name("open_mess_my_chat.wasm"), Ok(Some("my-chat.wasm".to_string())));
        // A name the daemon's whitelist would reject stops the release.
        assert!(module_release_name(&format!("open_mess_{}.wasm", "a".repeat(40))).is_err());
        assert!(module_release_name("open_mess_Teams.wasm").is_err());
    }

    #[test]
    fn line_endings_survive_the_version_bump() {
        let toml = "[workspace.package]\r\nversion = \"0.0.0\"\r\nedition = \"2024\"";
        let expected = "[workspace.package]\r\nversion = \"1.0.0\"\r\nedition = \"2024\"";
        assert_eq!(set_workspace_version(toml, "1.0.0").expect("set"), expected);
    }

    #[test]
    fn failed_release_prints_the_way_back() {
        assert_eq!(recovery(Stage::Clean, "v1.0.0", &[]), "");
        assert!(recovery(Stage::Edited, "v1.0.0", &[]).contains("git checkout HEAD -- Cargo.toml Cargo.lock"));
        let committed = recovery(Stage::Committed, "v1.0.0", &[]);
        assert!(committed.contains("git tag -d v1.0.0") && committed.contains("git reset --hard HEAD~1"));
        // The release may be missing, or there with only some of its files.
        let files = ["om.exe".to_string(), "manifest.json".to_string()];
        let pushed = recovery(Stage::Pushed, "v1.0.0", &files);
        let list = " target/release-dist/om.exe target/release-dist/manifest.json";
        assert!(pushed.contains(&format!("gh release create v1.0.0 --title v1.0.0 --notes \"\"{list}")), "{pushed}");
        assert!(pushed.contains(&format!("gh release upload v1.0.0{list} --clobber")), "{pushed}");
        assert!(pushed.contains("gh release edit v1.0.0 --draft=false"), "{pushed}");
        assert!(!pushed.contains('*'), "{pushed}");
    }

    #[test]
    fn a_secret_key_that_does_not_match_the_public_one_is_refused() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("pair");
        let other = minisign::KeyPair::generate_unencrypted_keypair().expect("other pair");
        check_key_pair(&pair.pk, &pair.sk).expect("a matching pair");
        assert!(check_key_pair(&other.pk, &pair.sk).is_err());
    }

    #[test]
    fn version_is_set_in_the_workspace_package() {
        let toml = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.0.0\"\nedition = \"2024\"\n";
        let out = set_workspace_version(toml, "0.1.0").expect("set");
        assert!(out.contains("version = \"0.1.0\"\nedition"));
        assert!(set_workspace_version("[workspace]\n", "0.1.0").is_none());
        assert!(valid_version("0.1.0") && !valid_version("1.2") && !valid_version("1.2.3-rc1") && !valid_version("v1.2.3"));
    }
}
