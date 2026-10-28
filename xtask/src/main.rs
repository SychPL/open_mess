//! Repository tasks.
//!
//! `cargo xtask ci` runs the same gates as CI (fmt, clippy, the wasm32-wasip2 module build, tests);
//! run it before every commit.
//! `cargo xtask hooks` points git at `.githooks/`, which sends every commit to the Codex and GLM reviewers.
//! `cargo xtask release-keygen` creates the release signing key once; `cargo xtask release <x.y.z>`
//! tags, builds, signs and publishes a release (spec 2026-10-27-self-update, section 4), and
//! `--pipeline` does the same from the GitHub Actions "Release" workflow (spec 2026-10-28-release-pipeline).

// A developer tool: printing here may panic, it never takes the daemon down.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The repository whose pull requests `release --pipeline` checks.
const REPO: &str = "SychPL/open_mess";

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

/// True when `new` is a higher `x.y.z` than `previous`, compared number by number (`0.10.0` is above `0.9.9`).
fn version_above(previous: &str, new: &str) -> bool {
    let parse = |text: &str| valid_version(text).then(|| text.split('.').map(str::parse::<u64>).collect::<Result<Vec<u64>, _>>().ok()).flatten();
    match (parse(previous), parse(new)) {
        (Some(previous), Some(new)) => new > previous,
        _ => false,
    }
}

/// `release <x.y.z> [--pipeline]`: the version and whether it is the pipeline mode. That mode only runs
/// inside GitHub Actions, so it cannot skip the local review check by accident, and needs the key password
/// in `OPEN_MESS_RELEASE_KEY_PASSWORD`, as there is no console to ask.
fn release_args(args: &[String], github_actions: Option<&str>, has_password: bool) -> Result<(String, bool), String> {
    let (version, pipeline) = match args {
        [version] => (version, false),
        [version, flag] if flag == "--pipeline" => (version, true),
        _ => return Err("usage: cargo xtask release <x.y.z> [--pipeline]".into()),
    };
    if pipeline && github_actions != Some("true") {
        return Err("--pipeline only runs in GitHub Actions".into());
    }
    if pipeline && !has_password {
        return Err("--pipeline needs the key password in OPEN_MESS_RELEASE_KEY_PASSWORD".into());
    }
    Ok((version.clone(), pipeline))
}

/// The number of the pull request that brought `sha` onto main, read from `gh api
/// repos/<repo>/commits/<sha>/pulls`: merged, into `main`, with `sha` as its merge commit (squash merging
/// makes every commit on main one). `None` for anything else, broken JSON included.
fn merged_through_pr(sha: &str, json: &str) -> Option<u64> {
    let pulls: Vec<Value> = serde_json::from_str(json).ok()?;
    pulls
        .iter()
        .find(|pull| {
            pull.pointer("/base/ref").and_then(Value::as_str) == Some("main")
                && pull.get("merged_at").is_some_and(Value::is_string)
                && pull.get("merge_commit_sha").and_then(Value::as_str) == Some(sha)
        })
        .and_then(|pull| pull.get("number").and_then(Value::as_u64))
}

/// The public key installs trust: `keys/release.pub` of the previous release (`previous`, its text), which
/// the versions users update from compiled in, or the working tree's (`tree`) for the first release. So the
/// release that brings a new keys/release.pub is still signed with the old key, and the next with the new one.
fn trusted_key(previous: Option<&str>, tree: &str) -> Result<minisign::PublicKey, String> {
    let (text, source) = match previous {
        Some(text) => (text, "keys/release.pub of the previous release"),
        None => (tree, "keys/release.pub"),
    };
    minisign::PublicKey::from_box(text.to_string().into()).map_err(|e| format!("{source} is not a release key: {e}"))
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
/// not patterns, and PowerShell does not expand a `*`. `run` is the GitHub Actions run of a `--pipeline`
/// release: there the stages before the push print nothing (they left nothing on origin, and the runner
/// with the local commit is gone), and the files come back from the run's artifact.
fn recovery(stage: Stage, version: &str, files: &[String], run: Option<&str>) -> String {
    let tag = format!("v{version}");
    match stage {
        _ if run.is_some() && stage != Stage::Pushed => String::new(),
        Stage::Clean => String::new(),
        Stage::Edited => "git checkout HEAD -- Cargo.toml Cargo.lock".to_string(),
        Stage::Committed => format!("git tag -d {tag}\ngit reset --hard HEAD~1"),
        Stage::Pushed => {
            let list: String = files.iter().map(|name| format!(" target/release-dist/{name}")).collect();
            let download = run
                .map(|run| {
                    format!("# in a clone of the repository at {tag}, get the files of the release:\ngh run download {run} -n release-dist-{version} -D target/release-dist\n")
                })
                .unwrap_or_default();
            // A published release is never uploaded to: only a draft is invisible to installs.
            format!(
                "{download}# see what GitHub holds:\ngh release view {tag} --json isDraft,assets\n\
                 # published, with every file of target/release-dist at its size: nothing to do.\n\
                 # a draft: upload the files again, check them with `gh release view` as above, then publish:\n\
                 gh release upload {tag}{list} --clobber\ngh release edit {tag} --draft=false\n\
                 # no release: create the draft, check it the same way, then publish:\n\
                 gh release create {tag} --draft --title {tag} --notes \"\"{list}\ngh release edit {tag} --draft=false"
            )
        }
    }
}

/// Signs a test message with `secret` and verifies it with `public`: a secret key that is not the
/// pair of the trusted keys/release.pub fails here, before any commit or tag.
fn check_key_pair(public: &minisign::PublicKey, secret: &minisign::SecretKey) -> Result<(), String> {
    const MESSAGE: &[u8] = b"open_mess release key check";
    let signature = minisign::sign(None, secret, MESSAGE, None, None).map_err(|e| format!("cannot sign with the secret key: {e}"))?;
    minisign::verify(public, &signature, std::io::Cursor::new(MESSAGE), true, false, false)
        .map_err(|e| format!("the secret key is not the pair of the trusted keys/release.pub: {e}"))
}

fn is_draft(release_view: &str) -> bool {
    serde_json::from_str::<Value>(release_view).ok().and_then(|release| release.get("isDraft")?.as_bool()) == Some(true)
}

/// Checks `gh release view <tag> --json isDraft,assets` output: the release is a draft (or published, as
/// `draft` says) and holds exactly the `expected` files (name, size on disk), all uploaded.
fn check_release(release_view: &str, draft: bool, expected: &[(String, u64)]) -> Result<(), String> {
    let release: Value = serde_json::from_str(release_view).map_err(|e| format!("unreadable `gh release view` output: {e}"))?;
    if release.get("isDraft").and_then(Value::as_bool) != Some(draft) {
        return Err(if draft { "the release is not a draft" } else { "the release is still a draft" }.into());
    }
    let assets = release.get("assets").and_then(Value::as_array).ok_or("`gh release view` lists no assets")?;
    // An asset whose upload did not finish counts as a different file.
    let found: Vec<(&str, Option<u64>)> = assets
        .iter()
        .map(|asset| {
            let uploaded = asset.get("state").and_then(Value::as_str).is_none_or(|state| state == "uploaded");
            (asset.get("name").and_then(Value::as_str).unwrap_or_default(), asset.get("size").and_then(Value::as_u64).filter(|_| uploaded))
        })
        .collect();
    let wanted: Vec<(&str, Option<u64>)> = expected.iter().map(|(name, size)| (name.as_str(), Some(*size))).collect();
    let missing: Vec<&str> = wanted.iter().filter(|file| !found.contains(file)).map(|(name, _)| *name).collect();
    let extra: Vec<&str> = found.iter().filter(|file| !wanted.contains(file)).map(|(name, _)| *name).collect();
    if missing.is_empty() && extra.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the release does not hold exactly its files: missing or wrong size [{}], not of this release [{}]",
            missing.join(", "),
            extra.join(", ")
        ))
    }
}

fn release_view(tag: &str) -> Result<String, String> {
    capture("gh", &["release", "view", tag, "--json", "isDraft,assets"])
}

/// Publishes through a draft (spec 2026-10-28-release-pipeline, 4.2): creates the draft with every file,
/// or uploads them again into a draft that is already there; checks it holds exactly `expected`; only then
/// publishes it and checks again. A draft is invisible to installs, so a half-uploaded release never
/// reaches one. Up to 3 attempts, 30 s apart; a release that is already published is only checked.
fn publish(tag: &str, paths: &[String], expected: &[(String, u64)]) -> Result<(), String> {
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let mut error = String::new();
    for attempt in 1..=3 {
        if attempt > 1 {
            eprintln!("{error}; attempt {attempt} of 3 in 30 s");
            std::thread::sleep(Duration::from_secs(30));
        }
        let result = match release_view(tag) {
            // No release yet, or GitHub did not answer: then `create` fails and the next attempt looks again.
            Err(_) => step("gh", &[&["release", "create", tag, "--draft", "--title", tag, "--notes", ""][..], &paths].concat()),
            Ok(view) if is_draft(&view) => step("gh", &[&["release", "upload", tag][..], &paths, &["--clobber"]].concat()),
            Ok(view) => return check_release(&view, false, expected),
        }
        .and_then(|()| check_release(&release_view(tag)?, true, expected))
        .and_then(|()| step("gh", &["release", "edit", tag, "--draft=false"]))
        .and_then(|()| check_release(&release_view(tag)?, false, expected));
        match result {
            Ok(()) => return Ok(()),
            Err(e) => error = e,
        }
    }
    Err(error)
}

/// Appends Markdown to the GitHub Actions run summary, when there is one.
fn append_summary(text: &str) {
    use std::io::Write;
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else { return };
    let written = std::fs::OpenOptions::new().append(true).create(true).open(&path).and_then(|mut file| file.write_all(text.as_bytes()));
    if let Err(e) = written {
        eprintln!("cannot write the run summary: {e}");
    }
}

/// `--pipeline` instead of the local .reviews files, which a runner does not have: every commit since
/// `previous` came through a merged pull request into main. The list goes to the run summary.
fn check_pull_requests(shas: &[String], previous: &str) -> Result<(), String> {
    const MAX_COMMITS: usize = 500;
    const MAX_ANSWER: usize = 64 * 1024;
    // Only the fields `merged_through_pr` reads, so the size cap is about them and not about pull request texts.
    const FIELDS: &str = "[.[] | {number, merged_at, merge_commit_sha, base: {ref: .base.ref}}]";
    if shas.len() > MAX_COMMITS {
        return Err(format!("{} commits since {previous}; the pipeline checks at most {MAX_COMMITS}", shas.len()));
    }
    let mut summary = format!("Commits since {previous}:\n\n");
    let mut refused = Vec::new();
    for sha in shas {
        let answer = capture("gh", &["api", &format!("repos/{REPO}/commits/{sha}/pulls"), "--jq", FIELDS])?;
        if answer.len() > MAX_ANSWER {
            return Err(format!("the pull requests of {sha} take more than 64 KiB"));
        }
        match merged_through_pr(sha, &answer) {
            Some(number) => summary.push_str(&format!("- {sha} #{number}\n")),
            None => refused.push(sha.as_str()),
        }
    }
    if !refused.is_empty() {
        return Err(format!("commits that did not come through a merged pull request into main: {}", refused.join(", ")));
    }
    append_summary(&summary);
    Ok(())
}

/// How far `release` got; decides which recovery commands are printed on failure.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Clean,
    /// Cargo.toml / Cargo.lock modified, nothing committed.
    Edited,
    /// The Release commit and the tag exist locally.
    Committed,
    /// Commit and tag are on origin; the GitHub release is missing, or still a draft.
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

/// `cargo xtask release <version> [--pipeline]`: spec 2026-10-27-self-update section 4, steps 1-6, with
/// the changes of spec 2026-10-28-release-pipeline section 4, stopping at the first failure. `password`
/// unlocks the secret key; without it the console asks.
fn release(version: &str, pipeline: bool, password: Option<String>) -> Result<(), String> {
    if !valid_version(version) {
        return Err(format!("{version:?} is not a version like 1.2.3"));
    }
    let tag = format!("v{version}");
    let mut stage = Stage::Clean;
    let mut files = Vec::new();
    let result = release_steps(version, &tag, pipeline, password, &mut stage, &mut files);
    if result.is_err() {
        let run = pipeline.then(|| std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "<run id>".to_string()));
        let commands = recovery(stage, version, &files, run.as_deref());
        if !commands.is_empty() {
            eprintln!("the release stopped half way; run:\n{commands}");
            append_summary(&format!("The release stopped after the push; finish it with:\n\n```\n{commands}\n```\n"));
        }
    }
    result
}

/// `assets` gets the names of the files in `target/release-dist`, for the recovery commands.
fn release_steps(
    version: &str,
    tag: &str,
    pipeline: bool,
    password: Option<String>,
    stage: &mut Stage,
    assets: &mut Vec<String>,
) -> Result<(), String> {
    let tag = tag.to_string();
    let root = repo_root();
    std::env::set_current_dir(&root).map_err(|e| e.to_string())?;

    // 1. Preconditions, all before any commit or tag. The previous release decides which key installs
    // trust and which versions may follow, so it is looked up first, on fresh tags.
    step("git", &["fetch", "origin", "main", "--tags"])?;
    let previous = capture("git", &["describe", "--tags", "--abbrev=0", "--match", "v*"]).ok();
    match &previous {
        // A running open_mess ignores a release that is not above its own version.
        Some(previous) if !version_above(previous.strip_prefix('v').unwrap_or_default(), version) => {
            return Err(format!("{version} is not above {previous}"));
        }
        // The first release covers commits from before pull requests; only the local review check fits it.
        None if pipeline => return Err("no earlier v* tag: the first release is local".into()),
        _ => {}
    }
    let tree_key = std::fs::read_to_string(root.join("keys").join("release.pub")).map_err(|e| format!("keys/release.pub: {e}"))?;
    let previous_key = previous.as_ref().map(|previous| capture("git", &["show", &format!("{previous}:keys/release.pub")])).transpose()?;
    let trusted = trusted_key(previous_key.as_deref(), &tree_key)?;
    let key_path = secret_key_path()?;
    if !key_path.exists() {
        return Err(format!("no secret key at {}; run `cargo xtask release-keygen`", key_path.display()));
    }
    // Unlock now (the console asks unless the password is set): a wrong password must not strand a commit and tag.
    let secret = minisign::SecretKey::from_file(&key_path, password).map_err(|e| format!("cannot unlock the secret key: {e}"))?;
    check_key_pair(&trusted, &secret)?;
    if !capture("git", &["status", "--porcelain"])?.is_empty() {
        return Err("the working tree is not clean".into());
    }
    if capture("git", &["rev-parse", "--abbrev-ref", "HEAD"])? != "main" {
        return Err("not on main".into());
    }
    let head = capture("git", &["rev-parse", "HEAD"])?;
    if head != capture("git", &["rev-parse", "origin/main"])? {
        return Err("main differs from origin/main".into());
    }
    // The release is of exactly the commit the run was started and approved for.
    if pipeline && std::env::var("GITHUB_SHA").ok().as_deref() != Some(head.as_str()) {
        return Err(format!("HEAD {head} is not GITHUB_SHA, the commit this run was started for"));
    }
    if !capture("git", &["tag", "--list", &tag])?.is_empty() {
        return Err(format!("tag {tag} already exists"));
    }
    if !ci() {
        return Err("cargo xtask ci failed".into());
    }
    let range = match &previous {
        Some(previous) => format!("{previous}..HEAD"),
        None => "HEAD".to_string(),
    };
    let shas: Vec<String> = capture("git", &["rev-list", &range])?.lines().map(str::to_string).collect();
    match &previous {
        Some(previous) if pipeline => check_pull_requests(&shas, previous)?,
        _ => {
            let open = unprocessed_commits(&shas, &root.join(".reviews"));
            if !open.is_empty() {
                return Err(format!("commits without a .reviews/<sha>.processed file: {}", open.join(", ")));
            }
        }
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

    // 4-5. Manifest and its minisign signature, checked against the trusted key.
    let manifest = build_manifest(version, &files);
    let signature = minisign::sign(Some(&trusted), &secret, manifest.as_bytes(), None, None).map_err(|e| format!("signing failed: {e}"))?;
    let dist = root.join("target/release-dist");
    // Files of an earlier release must neither be uploaded nor counted as this release's.
    match std::fs::remove_dir_all(&dist) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("cannot clear {}: {e}", dist.display())),
        _ => {}
    }
    std::fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    for (name, bytes) in &files {
        std::fs::write(dist.join(name), bytes).map_err(|e| e.to_string())?;
        assets.push(name.clone());
    }
    std::fs::write(dist.join("manifest.json"), &manifest).map_err(|e| e.to_string())?;
    std::fs::write(dist.join("manifest.json.minisig"), signature.to_string()).map_err(|e| e.to_string())?;
    assets.extend(["manifest.json".to_string(), "manifest.json.minisig".to_string()]);
    let expected = assets
        .iter()
        .map(|name| std::fs::metadata(dist.join(name)).map(|meta| (name.clone(), meta.len())).map_err(|e| format!("{name}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;

    // 6. Publish. Atomic: the commit and the tag land on origin together, or neither does.
    if let Err(error) = step("git", &["push", "--atomic", "origin", "main", &tag]) {
        // The push may have landed with only its acknowledgement lost: then the tag is on origin.
        let remote = capture("git", &["ls-remote", "origin", &format!("refs/tags/{tag}")]).unwrap_or_default();
        let release_commit = capture("git", &["rev-parse", "HEAD"])?;
        if remote.split_whitespace().next() != Some(release_commit.as_str()) {
            return Err(error);
        }
        eprintln!("git push reported an error, but {tag} is on origin at the release commit; going on");
    }
    *stage = Stage::Pushed;
    let paths: Vec<String> = assets.iter().map(|name| dist.join(name).to_string_lossy().into_owned()).collect();
    publish(&tag, &paths, &expected)
}

fn report(result: Result<(), String>) -> bool {
    result.inspect_err(|e| eprintln!("error: {e}")).is_ok()
}

fn main() -> ExitCode {
    let ok = match std::env::args().nth(1).as_deref() {
        Some("ci") => ci(),
        Some("hooks") => run("git", &["config", "core.hooksPath", ".githooks"]),
        Some("release-keygen") => report(release_keygen()),
        Some("release") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            // Never printed; an empty variable counts as unset.
            let password = std::env::var("OPEN_MESS_RELEASE_KEY_PASSWORD").ok().filter(|password| !password.is_empty());
            let github_actions = std::env::var("GITHUB_ACTIONS").ok();
            report(
                release_args(&args, github_actions.as_deref(), password.is_some())
                    .and_then(|(version, pipeline)| release(&version, pipeline, password)),
            )
        }
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
        assert_eq!(recovery(Stage::Clean, "1.0.0", &[], None), "");
        assert!(recovery(Stage::Edited, "1.0.0", &[], None).contains("git checkout HEAD -- Cargo.toml Cargo.lock"));
        let committed = recovery(Stage::Committed, "1.0.0", &[], None);
        assert!(committed.contains("git tag -d v1.0.0") && committed.contains("git reset --hard HEAD~1"));
        // The release may be missing, a draft, or published: the commands start by looking.
        let files = ["om.exe".to_string(), "manifest.json".to_string()];
        let pushed = recovery(Stage::Pushed, "1.0.0", &files, None);
        let list = " target/release-dist/om.exe target/release-dist/manifest.json";
        assert!(pushed.starts_with("# see what GitHub holds:\ngh release view v1.0.0 --json isDraft,assets\n"), "{pushed}");
        assert!(pushed.contains(&format!("gh release create v1.0.0 --draft --title v1.0.0 --notes \"\"{list}")), "{pushed}");
        assert!(pushed.contains(&format!("gh release upload v1.0.0{list} --clobber")), "{pushed}");
        assert!(pushed.contains("gh release edit v1.0.0 --draft=false"), "{pushed}");
        assert!(!pushed.contains('*'), "{pushed}");
        assert!(!pushed.contains("gh run download"), "{pushed}");
        // A published release is left alone: only the draft gets files uploaded again.
        let published = pushed.split("\n# ").find(|block| block.starts_with("published")).expect("published");
        assert!(published.ends_with("nothing to do."), "{published}");
        let draft = pushed.split("\n# ").find(|block| block.starts_with("a draft")).expect("draft");
        assert_eq!(pushed.matches("--clobber").count(), draft.matches("--clobber").count(), "{pushed}");
    }

    #[test]
    fn pipeline_recovery_starts_after_the_push_from_the_artifact() {
        for stage in [Stage::Clean, Stage::Edited, Stage::Committed] {
            assert_eq!(recovery(stage, "1.0.0", &["om.exe".to_string()], Some("42")), "", "{stage:?}");
        }
        let pushed = recovery(Stage::Pushed, "1.0.0", &["om.exe".to_string()], Some("42"));
        assert!(pushed.contains("gh run download 42 -n release-dist-1.0.0 -D target/release-dist\n"), "{pushed}");
        assert!(pushed.contains("gh release view v1.0.0 --json isDraft,assets"), "{pushed}");
    }

    #[test]
    fn only_a_squash_merged_pull_request_into_main_lets_a_commit_through() {
        let sha = "ff4fa9fcadeb8f8133455b3e837c2efa8861a485";
        let pull = |base: &str, merged_at: &str, merge_commit: &str| {
            format!(r#"[{{"number":2,"base":{{"ref":"{base}"}},"merged_at":{merged_at},"merge_commit_sha":"{merge_commit}"}}]"#)
        };
        assert_eq!(merged_through_pr(sha, &pull("main", "\"2026-10-28T10:31:25Z\"", sha)), Some(2));
        // Not merged, into another branch, merged as another commit (a merge commit's parent, a rebase).
        assert_eq!(merged_through_pr(sha, &pull("main", "null", sha)), None);
        assert_eq!(merged_through_pr(sha, &pull("release", "\"2026-10-28T10:31:25Z\"", sha)), None);
        assert_eq!(merged_through_pr(sha, &pull("main", "\"2026-10-28T10:31:25Z\"", "0123456789abcdef0123456789abcdef01234567")), None);
        assert_eq!(merged_through_pr(sha, "[]"), None);
        assert_eq!(merged_through_pr(sha, "{\"message\":\"Not Found\"}"), None);
        assert_eq!(merged_through_pr(sha, "[{\"number\":"), None);
        // Another pull request in the list does not hide the one that merged the commit.
        let two = format!(
            "[{},{}]",
            pull("main", "null", sha).trim_matches(['[', ']']),
            pull("main", "\"2026-10-28T10:31:25Z\"", sha).trim_matches(['[', ']'])
        );
        assert_eq!(merged_through_pr(sha, &two), Some(2));
    }

    fn public_text(pair: &minisign::KeyPair) -> String {
        String::from_utf8(pair.pk.to_box().expect("box").to_bytes()).expect("utf-8")
    }

    #[test]
    fn the_previous_release_decides_the_trusted_key() {
        let old = minisign::KeyPair::generate_unencrypted_keypair().expect("old pair");
        let new = minisign::KeyPair::generate_unencrypted_keypair().expect("new pair");
        // A rotation: the tree holds the new public key, the previous tag the old one.
        let trusted = trusted_key(Some(&public_text(&old)), &public_text(&new)).expect("previous key");
        check_key_pair(&trusted, &old.sk).expect("signed with the old key");
        assert!(check_key_pair(&trusted, &new.sk).is_err(), "a key that only matches the new keys/release.pub");
        // No previous release: the working tree's key.
        let first = trusted_key(None, &public_text(&new)).expect("tree key");
        check_key_pair(&first, &new.sk).expect("first release");
        assert!(trusted_key(Some("no key here"), &public_text(&new)).is_err());
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(version_above("1.2.3", "1.2.4"));
        assert!(version_above("0.9.9", "0.10.0"));
        assert!(version_above("1.9.9", "2.0.0"));
        assert!(!version_above("1.2.3", "1.2.3"));
        assert!(!version_above("1.2.4", "1.2.3"));
        assert!(!version_above("0.10.0", "0.9.9"));
        assert!(!version_above("foo", "1.0.0"));
        assert!(!version_above("1.0.0", "99999999999999999999.0.0"));
    }

    #[test]
    fn pipeline_mode_only_runs_in_actions_with_the_password() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(release_args(&args(&["1.2.3"]), None, false), Ok(("1.2.3".to_string(), false)));
        assert_eq!(release_args(&args(&["1.2.3", "--pipeline"]), Some("true"), true), Ok(("1.2.3".to_string(), true)));
        let outside = release_args(&args(&["1.2.3", "--pipeline"]), None, true);
        assert_eq!(outside, Err("--pipeline only runs in GitHub Actions".to_string()));
        assert!(release_args(&args(&["1.2.3", "--pipeline"]), Some("false"), true).is_err());
        let no_password = release_args(&args(&["1.2.3", "--pipeline"]), Some("true"), false).expect_err("no password");
        assert!(no_password.contains("OPEN_MESS_RELEASE_KEY_PASSWORD"), "{no_password}");
        assert!(release_args(&args(&[]), None, false).is_err());
        assert!(release_args(&args(&["1.2.3", "--pipelin"]), Some("true"), true).is_err());
        assert!(release_args(&args(&["1.2.3", "--pipeline", "x"]), Some("true"), true).is_err());
    }

    #[test]
    fn a_release_is_published_only_with_exactly_its_files() {
        let expected = [("om.exe".to_string(), 3), ("manifest.json".to_string(), 10)];
        let view = |draft: bool, assets: &str| format!(r#"{{"isDraft":{draft},"assets":[{assets}]}}"#);
        let om = r#"{"name":"om.exe","size":3,"state":"uploaded"}"#;
        let manifest = r#"{"name":"manifest.json","size":10,"state":"uploaded"}"#;
        let complete = format!("{om},{manifest}");
        check_release(&view(true, &complete), true, &expected).expect("complete draft");
        check_release(&view(false, &complete), false, &expected).expect("complete release");
        // Draft or published, as expected.
        assert!(check_release(&view(false, &complete), true, &expected).is_err());
        assert!(check_release(&view(true, &complete), false, &expected).is_err());
        assert!(is_draft(&view(true, "")) && !is_draft(&view(false, "")) && !is_draft("not json"));
        // A missing file, a wrong size, an unfinished upload, a file of another release.
        assert!(check_release(&view(true, om), true, &expected).is_err());
        let short = complete.replace("\"size\":10", "\"size\":9");
        let error = check_release(&view(true, &short), true, &expected).expect_err("wrong size");
        assert!(error.contains("manifest.json"), "{error}");
        assert!(check_release(&view(true, &complete.replace("\"uploaded\"}", "\"starter\"}")), true, &expected).is_err());
        let extra = format!("{complete},{}", r#"{"name":"old.wasm","size":1,"state":"uploaded"}"#);
        let error = check_release(&view(true, &extra), true, &expected).expect_err("extra file");
        assert!(error.contains("old.wasm"), "{error}");
        assert!(check_release("{", true, &expected).is_err());
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
