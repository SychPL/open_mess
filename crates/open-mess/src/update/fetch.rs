//! Asking GitHub for the latest release and downloading it into a folder, verified (spec
//! 2026-10-27-self-update, section 5). Nothing here runs a downloaded file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::redirect::Policy;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use super::UpdateError;
use super::release::{Version, manifest_sha256, verify};

/// Where releases come from and which hosts a download may touch.
#[derive(Debug, Clone)]
pub struct Source {
    /// The "latest release" API URL.
    pub latest: Url,
    /// The only hosts the API URL, asset URLs and redirects may use.
    pub allowed_hosts: Vec<String>,
    /// Refuse anything but HTTPS (false only in tests, against a loopback server).
    pub https_only: bool,
}

impl Source {
    pub fn github() -> Result<Source, url::ParseError> {
        Ok(Source {
            latest: Url::parse("https://api.github.com/repos/SychPL/open_mess/releases/latest")?,
            allowed_hosts: ["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"]
                .map(String::from)
                .to_vec(),
            https_only: true,
        })
    }

    fn allows(&self, url: &Url) -> bool {
        let scheme_ok = if self.https_only { url.scheme() == "https" } else { matches!(url.scheme(), "http" | "https") };
        scheme_ok && url.host_str().is_some_and(|host| self.allowed_hosts.iter().any(|allowed| allowed == host))
    }
}

/// A release newer than the running `om`: its version and the assets it carries.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub version: Version,
    pub assets: Vec<(String, Url)>,
}

/// A fully verified release in `dir`.
#[derive(Debug, Clone)]
pub struct Downloaded {
    pub version: Version,
    pub manifest_sha256: String,
    pub dir: PathBuf,
}

/// The manifest and its signature are small; anything bigger is not one.
const MAX_MANIFEST: usize = 64 * 1024;
/// The release API answer.
const MAX_API: usize = 1024 * 1024;
const MAX_ASSETS: usize = 200;
const MAX_REDIRECTS: usize = 5;
/// Connecting, and a whole small request: the release answer, the manifest, its signature (spec 5).
const TIMEOUT: Duration = Duration::from_secs(30);
/// A file of the release gets [`TIMEOUT`] and a second for every 256 KiB it declares, at most this.
const MAX_ASSET_TIME: Duration = Duration::from_secs(30 * 60);

/// How long the download of a file of `size` bytes may take in all: a server that sends a byte now
/// and then never ends it otherwise.
fn asset_deadline(size: u64) -> Duration {
    TIMEOUT.saturating_add(Duration::from_secs(size / (256 * 1024))).min(MAX_ASSET_TIME)
}

fn network(err: reqwest::Error) -> UpdateError {
    if err.is_redirect() { UpdateError::Rejected(format!("redirect refused: {err}")) } else { UpdateError::Network(err.to_string()) }
}

/// The HTTP client: follows at most 5 redirects, each to an allowed host.
pub fn client(source: &Source) -> Result<reqwest::Client, UpdateError> {
    // reqwest is built without a bundled crypto provider: ring becomes the process default,
    // unless the program installed another one already.
    rustls::crypto::ring::default_provider().install_default().ok();
    let rules = source.clone();
    let policy = Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if rules.allows(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("redirect to a host that is not allowed")
        }
    });
    let mut builder = reqwest::Client::builder()
        .redirect(policy)
        .user_agent(format!("open_mess/{}", Version::current()))
        .connect_timeout(TIMEOUT)
        .read_timeout(TIMEOUT);
    if !source.https_only {
        // Loopback test servers must not go through a proxy.
        builder = builder.no_proxy();
    }
    builder.build().map_err(network)
}

/// `url`, the answer read to its end within `timeout`.
async fn get(client: &reqwest::Client, url: &Url, timeout: Duration) -> Result<reqwest::Response, UpdateError> {
    let response = client.get(url.clone()).timeout(timeout).send().await.map_err(network)?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!("HTTP {}", response.status())));
    }
    Ok(response)
}

/// The whole body, at most `cap` bytes.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, UpdateError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len().saturating_add(chunk.len()) > cap {
            return Err(UpdateError::Rejected(format!("answer is larger than {cap} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

/// The latest published release, if it is newer than `current`.
pub async fn latest(client: &reqwest::Client, source: &Source, current: Version) -> Result<Option<Candidate>, UpdateError> {
    let response =
        client.get(source.latest.clone()).header("Accept", "application/vnd.github+json").timeout(TIMEOUT).send().await.map_err(network)?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // no release yet
    }
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!("HTTP {}", response.status())));
    }
    let release: ReleaseJson =
        serde_json::from_slice(&read_capped(response, MAX_API).await?).map_err(|e| UpdateError::Rejected(format!("release answer: {e}")))?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::from_tag(&release.tag_name)?;
    if version <= current {
        return Ok(None);
    }
    let mut assets = Vec::new();
    for asset in release.assets.into_iter().take(MAX_ASSETS) {
        let url = Url::parse(&asset.browser_download_url).map_err(|e| UpdateError::Rejected(format!("asset URL: {e}")))?;
        if !source.allows(&url) {
            return Err(UpdateError::Rejected(format!("asset {:?} is not on an allowed host", asset.name)));
        }
        assets.push((asset.name, url));
    }
    Ok(Some(Candidate { version, assets }))
}

fn asset_url<'a>(candidate: &'a Candidate, name: &str) -> Result<&'a Url, UpdateError> {
    candidate
        .assets
        .iter()
        .find(|(asset, _)| asset == name)
        .map(|(_, url)| url)
        .ok_or_else(|| UpdateError::Rejected(format!("release has no {name}")))
}

/// Downloads the manifest, verifies it and then every file it lists into `<root>\<version>`.
/// On any error that folder is removed. A folder that holds an installation's backup is refused,
/// never removed: after a failed rollback it may be the only copy of the user's data.
pub async fn download(client: &reqwest::Client, candidate: &Candidate, public_key: &str, root: &Path) -> Result<Downloaded, UpdateError> {
    let dir = root.join(candidate.version.to_string());
    if dir.exists() {
        if holds_backup(&dir)? {
            return Err(UpdateError::Rejected(format!(
                "{} holds the backup of an earlier installation; remove it once open_mess works again",
                dir.display()
            )));
        }
        std::fs::remove_dir_all(&dir)?; // leftovers of an earlier attempt are never reused
    }
    let result = download_into(client, candidate, public_key, &dir).await;
    if result.is_err() {
        std::fs::remove_dir_all(&dir).ok();
    }
    result
}

/// Whether an update folder holds a whole backup, current (`backup`) or set aside (`backup-*`), as
/// update/apply.rs names them. A `backup.partial` was never whole and does not count.
fn holds_backup(dir: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name == "backup" || name.starts_with("backup-") {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn download_into(client: &reqwest::Client, candidate: &Candidate, public_key: &str, dir: &Path) -> Result<Downloaded, UpdateError> {
    std::fs::create_dir_all(dir)?;
    let manifest_bytes = read_capped(get(client, asset_url(candidate, "manifest.json")?, TIMEOUT).await?, MAX_MANIFEST).await?;
    let signature = read_capped(get(client, asset_url(candidate, "manifest.json.minisig")?, TIMEOUT).await?, MAX_MANIFEST).await?;
    let signature = String::from_utf8(signature).map_err(|_| UpdateError::Rejected("signature is not text".into()))?;
    let manifest = verify(&manifest_bytes, &signature, public_key)?;
    if Version::parse(&manifest.version)? != candidate.version {
        return Err(UpdateError::Rejected(format!("manifest is for {}, the release tag for {}", manifest.version, candidate.version)));
    }
    for file in &manifest.files {
        let mut response = get(client, asset_url(candidate, &file.name)?, asset_deadline(file.size)).await?;
        let part = dir.join(format!("{}.part", file.name));
        let mut out = std::fs::File::create(&part)?;
        let (mut hasher, mut size) = (Sha256::new(), 0u64);
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            size = size.saturating_add(chunk.len() as u64);
            if size > file.size {
                return Err(UpdateError::Rejected(format!("{} is larger than the manifest says", file.name)));
            }
            hasher.update(&chunk);
            out.write_all(&chunk)?;
        }
        out.flush()?;
        drop(out);
        let sum: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if size != file.size || sum != file.sha256 {
            return Err(UpdateError::Rejected(format!("{} does not match the manifest", file.name)));
        }
        std::fs::rename(&part, dir.join(&file.name))?;
    }
    std::fs::write(dir.join("manifest.json"), &manifest_bytes)?;
    std::fs::write(dir.join("manifest.json.minisig"), signature)?;
    Ok(Downloaded { version: candidate.version, manifest_sha256: manifest_sha256(&manifest_bytes), dir: dir.to_path_buf() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_gets_a_second_per_256_kib_and_at_most_30_minutes() {
        assert_eq!(asset_deadline(0), Duration::from_secs(30));
        assert_eq!(asset_deadline(20 * 1024 * 1024), Duration::from_secs(30 + 80));
        assert_eq!(asset_deadline(u64::MAX), Duration::from_secs(30 * 60));
    }
}
