//! The Whisper model's files (spec 2026-10-28-local-transcription, 4.7): one folder per allowed
//! revision under `<local>\models`, each file checked against the size and SHA-256 compiled in
//! here, when it is downloaded and again before it is used. Only the configuration window starts a
//! download; nothing else of the recording reaches the network.

use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use url::Url;

use crate::update::UpdateError;
use crate::update::fetch::{self, Source};

/// One file of a revision.
#[derive(Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub sha256: &'static str,
    /// The exact size; a file of another size is refused, unread past it.
    pub size: u64,
}

/// A commit of `openai/whisper-large-v3-turbo` on Hugging Face (MIT licence): `config.json`,
/// `tokenizer.json` and `model.safetensors`, in that order.
#[derive(Debug)]
pub struct Revision {
    pub commit: &'static str,
    pub files: [ModelFile; 3],
}

/// The allowed revisions, newest first. The list only grows: a release never drops a revision,
/// so an update never makes a downloaded model useless (4.7).
pub const REVISIONS: &[Revision] = &[Revision {
    commit: "41f01f3fe87f28c78e2fbf8b568835947dd65ed9",
    files: [
        ModelFile { name: "config.json", sha256: "c5b526b3e3cd64cd8940dabb45e8ba726629e22d8ed389c29b552f9140daf04a", size: 1256 },
        ModelFile { name: "tokenizer.json", sha256: "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd", size: 2_710_337 },
        ModelFile { name: "model.safetensors", sha256: "542566a422ae4f3fd23f1ba11add198fca01bbf82e66e6a2857b3f608b1eb9d1", size: 1_617_824_864 },
    ],
}];

/// Where the revisions' files are: `<BASE><commit>/<file>`.
pub const BASE: &str = "https://huggingface.co/openai/whisper-large-v3-turbo/resolve/";
/// The weights may take this long to arrive (4.7: 1.6 GB in 30 minutes would need about 7 Mb/s);
/// the small files, a few minutes.
const WEIGHTS_TIME: Duration = Duration::from_secs(3 * 60 * 60);
const SMALL_TIME: Duration = Duration::from_secs(5 * 60);

/// A revision's folder; `local` is `Paths::local` (update/state.rs).
pub fn folder(local: &Path, revision: &Revision) -> PathBuf {
    local.join("models").join(format!("whisper-large-v3-turbo-{}", revision.commit))
}

/// The newest allowed revision with all its files on disk; their sums are checked as they are read.
pub fn installed(local: &Path) -> Option<&'static Revision> {
    REVISIONS.iter().find(|revision| revision.files.iter().all(|file| folder(local, revision).join(file.name).is_file()))
}

/// Reads a model file whole. One of another size, or whose SHA-256 differs, is `InvalidData`
/// (the model is damaged).
pub fn read_verified(dir: &Path, file: &ModelFile) -> io::Result<Vec<u8>> {
    let found = File::open(dir.join(file.name))?;
    // Sized up front: growing by doubling would hold up to twice the weights' 1.6 GB.
    let mut bytes = Vec::with_capacity(found.metadata()?.len().min(file.size + 1) as usize);
    found.take(file.size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != file.size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} has another size than expected", file.name)));
    }
    let sum: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if sum != file.sha256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} does not match its checksum", file.name)));
    }
    Ok(bytes)
}

/// Hugging Face and the CDN hosts its downloads redirect to (`us.aws.cdn.hf.co` and the like).
pub fn source() -> Result<Source, url::ParseError> {
    Ok(Source { latest: Url::parse(BASE)?, allowed_hosts: vec!["huggingface.co".into(), "*.hf.co".into()], https_only: true })
}

/// Downloads a revision into its folder, file by file, each checked as it arrives; `progress` hears
/// the bytes so far of all of them. Leftovers of an earlier attempt are never reused, and a failed
/// download leaves no folder. Once whole, the folders of the other listed revisions go (4.7).
pub async fn download(
    client: &reqwest::Client,
    base: &Url,
    local: &Path,
    revision: &Revision,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), UpdateError> {
    let dir = folder(local, revision);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    let total: u64 = revision.files.iter().map(|file| file.size).sum();
    let mut before = 0;
    for file in &revision.files {
        let result = match base.join(&format!("{}/{}", revision.commit, file.name)) {
            Ok(url) => {
                let deadline = if file.size > 100 * 1024 * 1024 { WEIGHTS_TIME } else { SMALL_TIME };
                let done_before = before;
                fetch::download_file(client, &url, &dir.join(file.name), file.size, file.sha256, deadline, &|done| {
                    progress(done_before + done, total)
                })
                .await
            }
            Err(err) => Err(UpdateError::Rejected(err.to_string())),
        };
        if let Err(err) = result {
            fs::remove_dir_all(&dir).ok();
            return Err(err);
        }
        before += file.size;
    }
    for other in REVISIONS.iter().filter(|other| other.commit != revision.commit) {
        fs::remove_dir_all(folder(local, other)).ok();
    }
    Ok(())
}

/// A download of the model, as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Download {
    Idle,
    Running { done: u64, total: u64 },
    Failed { reason: String },
}

/// What the window's Nagrywanie rozmów section shows of the model (4.6).
#[derive(Clone, Debug, Serialize)]
pub struct ModelView {
    /// The revision on disk, if any.
    pub installed: Option<&'static str>,
    /// A newer allowed revision than the one on disk: "Dostępny nowszy model".
    pub newer: bool,
    /// The bytes of the newest revision, for the download button.
    pub size: u64,
    pub download: Download,
}

/// The model's files and their download, for the window.
pub struct Models {
    local: PathBuf,
    base: Url,
    runtime: Handle,
    download: Arc<Mutex<Download>>,
}

fn locked(download: &Mutex<Download>) -> MutexGuard<'_, Download> {
    download.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Models {
    /// Downloads run on `runtime`, from `base` ([`BASE`], but in tests).
    pub fn new(local: &Path, base: Url, runtime: Handle) -> Models {
        Models { local: local.to_path_buf(), base, runtime, download: Arc::new(Mutex::new(Download::Idle)) }
    }

    pub fn view(&self) -> ModelView {
        let installed = installed(&self.local);
        let newest = REVISIONS.first();
        ModelView {
            installed: installed.map(|revision| revision.commit),
            newer: installed.is_some_and(|found| newest.is_some_and(|newest| newest.commit != found.commit)),
            size: newest.map_or(0, |revision| revision.files.iter().map(|file| file.size).sum()),
            download: locked(&self.download).clone(),
        }
    }

    /// "Pobierz model": the newest revision, in the background, from `source`'s hosts; one
    /// download at a time.
    pub fn start_download(&self, source: Source) -> Result<(), String> {
        self.start(REVISIONS.first().ok_or("brak modelu do pobrania")?, source)
    }

    fn start(&self, revision: &'static Revision, source: Source) -> Result<(), String> {
        {
            let mut state = locked(&self.download);
            if matches!(*state, Download::Running { .. }) {
                return Err("model już się pobiera".into());
            }
            *state = Download::Running { done: 0, total: revision.files.iter().map(|file| file.size).sum() };
        }
        let (local, base, download) = (self.local.clone(), self.base.clone(), Arc::clone(&self.download));
        self.runtime.spawn(async move {
            let set = |next: Download| *locked(&download) = next;
            let result = match fetch::client(&source) {
                Ok(client) => self::download(&client, &base, &local, revision, &|done, total| set(Download::Running { done, total })).await,
                Err(err) => Err(err),
            };
            set(match result {
                Ok(()) => Download::Idle,
                Err(err) => Download::Failed { reason: err.to_string() },
            });
        });
        Ok(())
    }

    /// "Usuń model": every revision's folder. Refused while a download runs.
    // ponytail: refused also while recording or transcribing, once those run (4.6, spec step 5).
    pub fn delete(&self) -> Result<(), String> {
        if matches!(*locked(&self.download), Download::Running { .. }) {
            return Err("model się pobiera".into());
        }
        match fs::remove_dir_all(self.local.join("models")) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err.to_string()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_revisions_name_full_sums() {
        assert!(!REVISIONS.is_empty());
        for revision in REVISIONS {
            assert_eq!(revision.commit.len(), 40);
            for file in &revision.files {
                assert!(file.sha256.len() == 64 && file.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{}", file.name);
            }
        }
    }

    #[test]
    fn a_file_is_read_only_with_its_sum_and_size() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("abc"), b"abc").unwrap();
        let sum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(read_verified(dir.path(), &ModelFile { name: "abc", sha256: sum, size: 3 }).unwrap(), b"abc");
        let wrong = ModelFile { name: "abc", sha256: "00", size: 3 };
        assert_eq!(read_verified(dir.path(), &wrong).unwrap_err().kind(), io::ErrorKind::InvalidData);
        for size in [2, 4] {
            let other = ModelFile { name: "abc", sha256: sum, size };
            assert_eq!(read_verified(dir.path(), &other).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn a_revision_is_installed_only_with_all_its_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed(dir.path()).is_none());
        let revision = REVISIONS.first().unwrap();
        let path = folder(dir.path(), revision);
        std::fs::create_dir_all(&path).unwrap();
        for file in &revision.files[..2] {
            std::fs::write(path.join(file.name), b"").unwrap();
        }
        assert!(installed(dir.path()).is_none());
        std::fs::write(path.join("model.safetensors"), b"").unwrap();
        assert_eq!(installed(dir.path()).map(|found| found.commit), Some(revision.commit));
    }

    /// Three one-byte files: "a", "b" and "c".
    const TEST: Revision = Revision {
        commit: "0000000000000000000000000000000000000000",
        files: [
            ModelFile { name: "config.json", sha256: "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb", size: 1 },
            ModelFile { name: "tokenizer.json", sha256: "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d", size: 1 },
            ModelFile { name: "model.safetensors", sha256: "2e7d2c03a9507ae265ecf5b5356885a53393a2029d241394997265a1a25aefc6", size: 1 },
        ],
    };

    /// Serves `files` (path, body) on loopback; a body starting with `->` redirects there.
    async fn serve(files: Vec<(&'static str, &'static str)>) -> (Url, Source) {
        use axum::response::{IntoResponse, Redirect};
        let mut router = axum::Router::new();
        for (path, body) in files {
            router = router.route(
                path,
                axum::routing::get(move || async move {
                    match body.strip_prefix("->") {
                        Some(target) => Redirect::temporary(target).into_response(),
                        None => body.into_response(),
                    }
                }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        let base = Url::parse(&format!("http://{addr}/")).unwrap();
        (base.clone(), Source { latest: base, allowed_hosts: vec!["127.0.0.1".into()], https_only: false })
    }

    fn path(name: &str) -> &'static str {
        Box::leak(format!("/{}/{name}", TEST.commit).into_boxed_str())
    }

    #[tokio::test]
    async fn a_revision_downloads_checked_and_replaces_the_others() {
        let (base, source) = serve(vec![(path("config.json"), "a"), (path("tokenizer.json"), "b"), (path("model.safetensors"), "c")]).await;
        let local = tempfile::tempdir().unwrap();
        // An older revision on disk goes once the new one is whole.
        let older = folder(local.path(), REVISIONS.first().unwrap());
        std::fs::create_dir_all(&older).unwrap();
        let seen = Mutex::new(Vec::new());
        let client = fetch::client(&source).unwrap();
        download(&client, &base, local.path(), &TEST, &|done, total| seen.lock().unwrap().push((done, total))).await.unwrap();
        let dir = folder(local.path(), &TEST);
        assert_eq!(std::fs::read(dir.join("model.safetensors")).unwrap(), b"c");
        assert!(!older.exists());
        assert_eq!(seen.lock().unwrap().last(), Some(&(3, 3)));
    }

    #[tokio::test]
    async fn a_wrong_file_or_a_foreign_redirect_leaves_nothing() {
        let local = tempfile::tempdir().unwrap();
        for weights in ["x", "cc", "->http://localhost:9/elsewhere"] {
            let (base, source) = serve(vec![(path("config.json"), "a"), (path("tokenizer.json"), "b"), (path("model.safetensors"), weights)]).await;
            let client = fetch::client(&source).unwrap();
            assert!(download(&client, &base, local.path(), &TEST, &|_, _| {}).await.is_err(), "{weights}");
            assert!(!folder(local.path(), &TEST).exists(), "{weights}");
        }
    }

    #[test]
    fn only_hugging_face_hosts_are_allowed() {
        let source = source().unwrap();
        let client = fetch::client(&source);
        assert!(client.is_ok());
        assert_eq!(source.allowed_hosts, ["huggingface.co", "*.hf.co"]);
        assert!(source.https_only);
    }

    #[tokio::test]
    async fn the_window_sees_the_download_and_cannot_delete_meanwhile() {
        let (base, source) = serve(vec![(path("config.json"), "a"), (path("tokenizer.json"), "b"), (path("model.safetensors"), "c")]).await;
        let local = tempfile::tempdir().unwrap();
        let models = Models::new(local.path(), base, Handle::current());
        assert_eq!((models.view().installed, models.view().download), (None, Download::Idle));
        models.start(&TEST, source.clone()).unwrap();
        // The test's runtime has one thread: the download has not run a step yet.
        assert!(models.start(&TEST, source).is_err());
        assert!(models.delete().is_err());
        while matches!(models.view().download, Download::Running { .. }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(models.view().download, Download::Idle);
        assert!(folder(local.path(), &TEST).join("config.json").exists());
        models.delete().unwrap();
        assert!(!local.path().join("models").exists());
    }
}
