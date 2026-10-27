//! Checking GitHub releases and downloading verified updates, against a fake release server
//! (spec 2026-10-27-self-update, section 5).
// clippy.toml allows expect in #[test] functions only; the server and fixture helpers are test code too.
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use open_mess::update::UpdateError;
use open_mess::update::fetch::{Downloaded, Source, client, download, latest};
use open_mess::update::release::{Version, manifest_sha256};
use sha2::{Digest, Sha256};

const OM: &[u8] = b"fake om.exe bytes";
const TEAMS: &[u8] = b"fake teams module bytes";

type Keys = (String, minisign::SecretKey);
type Log = Arc<Mutex<Vec<String>>>;

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn keys() -> Keys {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("key pair");
    (pair.pk.to_box().expect("pk box").to_string(), pair.sk)
}

fn current() -> Version {
    Version::parse("0.1.0").expect("version")
}

struct Fake {
    /// The release as the API lists it.
    tag: String,
    draft: bool,
    prerelease: bool,
    /// Asset name -> bytes served.
    assets: Vec<(String, Vec<u8>)>,
}

impl Fake {
    /// A signed release 9.9.9 of `om.exe` and `teams.wasm`.
    fn release(keys: &Keys) -> Fake {
        Fake::with_manifest(keys, "9.9.9", &[("om.exe", "exe", OM, sha(OM)), ("teams.wasm", "module", TEAMS, sha(TEAMS))])
    }

    /// A release tagged v9.9.9 whose signed manifest says `version` and lists `files`
    /// (name, kind, bytes served, sha256 declared).
    fn with_manifest(keys: &Keys, version: &str, files: &[(&str, &str, &[u8], String)]) -> Fake {
        let entries: Vec<String> = files
            .iter()
            .map(|(name, kind, bytes, hash)| format!(r#"{{"name":"{name}","kind":"{kind}","size":{},"sha256":"{hash}"}}"#, bytes.len()))
            .collect();
        let manifest = format!(r#"{{"version":"{version}","files":[{}]}}"#, entries.join(",")).into_bytes();
        let signature = minisign::sign(None, &keys.1, Cursor::new(&manifest), None, None).expect("sign").to_string().into_bytes();
        let mut assets = vec![("manifest.json".to_string(), manifest), ("manifest.json.minisig".to_string(), signature)];
        assets.extend(files.iter().map(|(name, _, bytes, _)| (name.to_string(), bytes.to_vec())));
        Fake { tag: "v9.9.9".into(), draft: false, prerelease: false, assets }
    }

    fn without(mut self, name: &str) -> Fake {
        self.assets.retain(|(asset, _)| asset != name);
        self
    }

    /// Starts the server; the log collects the name of every asset requested.
    async fn serve(self) -> (Source, Log) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let listed: Vec<serde_json::Value> = self
            .assets
            .iter()
            .map(|(name, _)| serde_json::json!({"name": name, "browser_download_url": format!("http://{addr}/asset/{name}")}))
            .collect();
        let release = serde_json::json!({"tag_name": self.tag, "draft": self.draft, "prerelease": self.prerelease, "assets": listed});
        let log = Log::default();
        let state = Shared { assets: Arc::new(self.assets.into_iter().collect()), log: log.clone(), addr };
        let app = Router::new()
            .route("/latest", get(move || async move { axum::Json(release) }))
            .route("/asset/{name}", get(asset))
            .route("/elsewhere", get(|| async { "elsewhere" }))
            .with_state(state);
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let source =
            Source { latest: format!("http://{addr}/latest").parse().expect("url"), allowed_hosts: vec!["127.0.0.1".into()], https_only: false };
        (source, log)
    }
}

#[derive(Clone)]
struct Shared {
    assets: Arc<HashMap<String, Vec<u8>>>,
    log: Log,
    addr: std::net::SocketAddr,
}

async fn asset(State(state): State<Shared>, Path(name): Path<String>) -> Response {
    state.log.lock().expect("log").push(name.clone());
    let Some(bytes) = state.assets.get(&name) else { return StatusCode::NOT_FOUND.into_response() };
    if name == "drop.wasm" {
        // Promises more than it sends: the connection breaks mid-file.
        return ([(header::CONTENT_LENGTH, "1000")], bytes.clone()).into_response();
    }
    if name == "redirect.wasm" {
        // The same machine, but not a host on the list.
        return (StatusCode::FOUND, [(header::LOCATION, format!("http://localhost:{}/elsewhere", state.addr.port()))]).into_response();
    }
    bytes.clone().into_response()
}

/// Runs the whole flow against `fake`; the folder is the update root.
async fn run(fake: Fake, public_key: &str) -> (Result<Downloaded, UpdateError>, tempfile::TempDir, Log) {
    let (source, log) = fake.serve().await;
    let client = client(&source).expect("client");
    let candidate = latest(&client, &source, current()).await.expect("latest").expect("candidate");
    let root = tempfile::tempdir().expect("tempdir");
    let result = download(&client, &candidate, public_key, root.path()).await;
    (result, root, log)
}

fn assert_removed(root: &tempfile::TempDir) {
    assert!(!root.path().join("9.9.9").exists(), "partial download left behind");
}

#[tokio::test]
async fn newer_release_downloads_and_verifies() {
    let keys = keys();
    let (result, root, _) = run(Fake::release(&keys), &keys.0).await;
    let done = result.expect("download");
    assert_eq!(done.version, Version(9, 9, 9));
    assert_eq!(done.dir, root.path().join("9.9.9"));
    assert_eq!(std::fs::read(done.dir.join("om.exe")).expect("om.exe"), OM);
    assert_eq!(std::fs::read(done.dir.join("teams.wasm")).expect("teams"), TEAMS);
    assert_eq!(done.manifest_sha256, manifest_sha256(&std::fs::read(done.dir.join("manifest.json")).expect("manifest")));
    assert!(!done.dir.join("om.exe.part").exists());
}

#[tokio::test]
async fn equal_or_lower_version_is_ignored() {
    let keys = keys();
    let (source, _) = Fake::release(&keys).serve().await;
    let client = client(&source).expect("client");
    for current in ["9.9.9", "10.0.0"] {
        let current = Version::parse(current).expect("version");
        assert!(latest(&client, &source, current).await.expect("latest").is_none());
    }
}

#[tokio::test]
async fn draft_and_prerelease_are_ignored() {
    let keys = keys();
    for (draft, prerelease) in [(true, false), (false, true)] {
        let (source, _) = Fake { draft, prerelease, ..Fake::release(&keys) }.serve().await;
        let client = client(&source).expect("client");
        assert!(latest(&client, &source, current()).await.expect("latest").is_none());
    }
}

#[tokio::test]
async fn tag_and_manifest_version_must_match() {
    let keys = keys();
    let fake = Fake::with_manifest(&keys, "9.9.8", &[("om.exe", "exe", OM, sha(OM))]);
    let (result, root, _) = run(fake, &keys.0).await;
    assert!(matches!(result, Err(UpdateError::Rejected(_))));
    assert_removed(&root);
}

#[tokio::test]
async fn missing_signature_is_rejected() {
    let keys = keys();
    let (result, root, _) = run(Fake::release(&keys).without("manifest.json.minisig"), &keys.0).await;
    assert!(matches!(result, Err(UpdateError::Rejected(_))));
    assert_removed(&root);
    // A signature from another key is no better.
    let (result, root, _) = run(Fake::release(&keys), &self::keys().0).await;
    assert!(matches!(result, Err(UpdateError::Rejected(_))));
    assert_removed(&root);
}

#[tokio::test]
async fn wrong_sum_is_rejected() {
    let keys = keys();
    let fake = Fake::with_manifest(&keys, "9.9.9", &[("om.exe", "exe", OM, sha(b"something else"))]);
    let (result, root, _) = run(fake, &keys.0).await;
    assert!(matches!(result, Err(UpdateError::Rejected(_))));
    assert_removed(&root);
}

#[tokio::test]
async fn file_larger_than_manifest_is_cut_off() {
    let keys = keys();
    let mut fake = Fake::release(&keys);
    // The manifest says 17 bytes; the server sends a megabyte.
    for (name, bytes) in &mut fake.assets {
        if name == "om.exe" {
            *bytes = vec![b'x'; 1024 * 1024];
        }
    }
    let (result, root, _) = run(fake, &keys.0).await;
    assert!(matches!(result, Err(UpdateError::Rejected(_))));
    assert_removed(&root);
}

#[tokio::test]
async fn redirect_to_other_host_is_refused() {
    let keys = keys();
    let fake = Fake::with_manifest(&keys, "9.9.9", &[("om.exe", "exe", OM, sha(OM)), ("redirect.wasm", "module", b"x", sha(b"x"))]);
    let (result, root, log) = run(fake, &keys.0).await;
    assert!(result.is_err());
    assert!(!log.lock().expect("log").contains(&"elsewhere".to_string()));
    assert_removed(&root);
}

#[tokio::test]
async fn partial_download_is_removed() {
    let keys = keys();
    let fake = Fake::with_manifest(&keys, "9.9.9", &[("om.exe", "exe", OM, sha(OM)), ("drop.wasm", "module", b"0123456789", sha(b"0123456789"))]);
    let (result, root, _) = run(fake, &keys.0).await;
    assert!(matches!(result, Err(UpdateError::Network(_))), "{result:?}");
    assert_removed(&root);
}

#[tokio::test]
async fn folder_with_a_backup_is_never_replaced() {
    let keys = keys();
    let (source, _) = Fake::release(&keys).serve().await;
    let client = client(&source).expect("client");
    let candidate = latest(&client, &source, current()).await.expect("latest").expect("candidate");
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("9.9.9");
    for backup in ["backup", "backup-1700000000000"] {
        std::fs::create_dir_all(dir.join(backup)).expect("backup");
        std::fs::write(dir.join(backup).join("state.sqlite3"), b"the user's data").expect("backup file");
        let result = download(&client, &candidate, &keys.0, root.path()).await;
        let Err(UpdateError::Rejected(why)) = result else { panic!("downloaded over {backup}: {result:?}") };
        assert!(why.contains(&dir.display().to_string()) && why.contains("remove it"), "{why}");
        assert_eq!(std::fs::read(dir.join(backup).join("state.sqlite3")).expect("kept"), b"the user's data");
        std::fs::rename(dir.join(backup), root.path().join(backup)).expect("move away");
    }
    // A partial backup was never whole: the folder is an ordinary leftover.
    std::fs::create_dir_all(dir.join("backup.partial")).expect("partial");
    download(&client, &candidate, &keys.0, root.path()).await.expect("download");
}

#[tokio::test]
async fn asset_not_in_manifest_is_not_downloaded() {
    let keys = keys();
    let mut fake = Fake::release(&keys);
    fake.assets.push(("extra.wasm".into(), b"not signed for".to_vec()));
    let (result, _root, log) = run(fake, &keys.0).await;
    let done = result.expect("download");
    assert!(!log.lock().expect("log").contains(&"extra.wasm".to_string()));
    assert!(!done.dir.join("extra.wasm").exists());
}
