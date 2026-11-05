//! The signed release manifest and the checks every release must pass before anything is
//! downloaded (spec 2026-10-27-self-update, sections 4 and 5).

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::UpdateError;

/// The release public key (minisign), compiled in. The placeholder in a fresh checkout rejects
/// every release.
pub const RELEASE_KEY: &str = include_str!("../../../../keys/release.pub");

/// Largest sum of file sizes a release may declare.
const MAX_TOTAL_SIZE: u64 = 256 * 1024 * 1024;

/// A release version `major.minor.patch`; no pre-release or build parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(text: &str) -> Result<Version, UpdateError> {
        let bad = || UpdateError::Rejected(format!("bad version {text:?}"));
        let mut parts = text.split('.');
        let mut next = || -> Result<u64, UpdateError> {
            let part = parts.next().ok_or_else(bad)?;
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let version = Version(next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(version)
    }

    /// A release tag: `v` plus a version.
    pub fn from_tag(tag: &str) -> Result<Version, UpdateError> {
        let rest = tag.strip_prefix('v').ok_or_else(|| UpdateError::Rejected(format!("bad release tag {tag:?}")))?;
        Version::parse(rest)
    }

    /// The version of the running `om`.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Exe,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<FileEntry>,
}

/// SHA-256 of the manifest bytes, lowercase hex: the identity of a release.
pub fn manifest_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `om.exe` or `<name>.wasm` with `<name>` in `[a-z0-9-]{1,32}`.
fn name_allowed(name: &str) -> bool {
    if name == "om.exe" {
        return true;
    }
    match name.strip_suffix(".wasm") {
        Some(stem) => (1..=32).contains(&stem.len()) && stem.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        None => false,
    }
}

/// Checks the signature first, then parses the manifest and checks every entry.
pub fn verify(manifest: &[u8], signature: &str, public_key: &str) -> Result<Manifest, UpdateError> {
    let reject = |what: &str, err: &dyn fmt::Display| UpdateError::Rejected(format!("{what}: {err}"));
    let key = minisign_verify::PublicKey::decode(public_key).map_err(|e| reject("release key", &e))?;
    let signature = minisign_verify::Signature::decode(signature).map_err(|e| reject("signature", &e))?;
    key.verify(manifest, &signature, false).map_err(|e| reject("signature", &e))?;
    let parsed: Manifest = serde_json::from_slice(manifest).map_err(|e| reject("manifest", &e))?;

    let mut total: u64 = 0;
    for (index, file) in parsed.files.iter().enumerate() {
        if !name_allowed(&file.name) {
            return Err(UpdateError::Rejected(format!("file name {:?} is not allowed", file.name)));
        }
        if parsed.files.iter().take(index).any(|earlier| earlier.name == file.name) {
            return Err(UpdateError::Rejected(format!("file {:?} listed twice", file.name)));
        }
        if (file.name == "om.exe") != (file.kind == Kind::Exe) {
            return Err(UpdateError::Rejected(format!("file {:?} has the wrong kind", file.name)));
        }
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(UpdateError::Rejected(format!("file {:?} has a bad sha256", file.name)));
        }
        total = total.saturating_add(file.size);
    }
    // A later step copies the release `om.exe` to start the installer helper.
    if !parsed.files.iter().any(|file| file.name == "om.exe") {
        return Err(UpdateError::Rejected("release has no om.exe".into()));
    }
    if total > MAX_TOTAL_SIZE {
        return Err(UpdateError::Rejected("release is larger than 256 MiB".into()));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A fresh key pair: (public key file text, secret key).
    fn key_pair() -> (String, minisign::SecretKey) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("key pair");
        (pair.pk.to_box().expect("pk box").to_string(), pair.sk)
    }

    fn sign(sk: &minisign::SecretKey, bytes: &[u8]) -> String {
        minisign::sign(None, sk, Cursor::new(bytes), None, None).expect("sign").to_string()
    }

    fn manifest_json(files: &[(&str, &str, u64)]) -> Vec<u8> {
        let files: Vec<String> =
            files.iter().map(|(name, kind, size)| format!(r#"{{"name":{name:?},"kind":"{kind}","size":{size},"sha256":"{HASH}"}}"#)).collect();
        format!(r#"{{"version":"1.2.3","files":[{}]}}"#, files.join(",")).into_bytes()
    }

    /// Signs `json` with a fresh key and verifies it.
    fn check(json: &[u8]) -> Result<Manifest, UpdateError> {
        let (public, secret) = key_pair();
        verify(json, &sign(&secret, json), &public)
    }

    #[test]
    fn good_signature_parses() {
        let json = manifest_json(&[("om.exe", "exe", 10), ("teams.wasm", "module", 20)]);
        let manifest = check(&json).expect("valid");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(manifest.files[1].kind, Kind::Module);
        assert_eq!(manifest_sha256(&json).len(), 64);
    }

    #[test]
    fn bad_signature_is_rejected() {
        let json = manifest_json(&[("om.exe", "exe", 10)]);
        let (public, secret) = key_pair();
        let (_, other) = key_pair();
        assert!(matches!(verify(&json, &sign(&other, &json), &public), Err(UpdateError::Rejected(_))));
        let altered = manifest_json(&[("om.exe", "exe", 11)]);
        assert!(matches!(verify(&altered, &sign(&secret, &json), &public), Err(UpdateError::Rejected(_))));
        assert!(matches!(verify(&json, "not a signature", &public), Err(UpdateError::Rejected(_))));
    }

    #[test]
    fn foreign_signature_is_rejected_by_the_release_key() {
        let json = manifest_json(&[("om.exe", "exe", 10)]);
        let (_, secret) = key_pair();
        assert!(matches!(verify(&json, &sign(&secret, &json), RELEASE_KEY), Err(UpdateError::Rejected(_))));
    }

    #[test]
    fn release_key_decodes() {
        // A damaged keys/release.pub would reject every release in the field; CI catches it here.
        minisign_verify::PublicKey::decode(RELEASE_KEY).expect("keys/release.pub is a minisign public key");
    }

    #[test]
    fn name_whitelist() {
        let long = format!("{}.wasm", "a".repeat(33));
        for bad in ["../om.exe", "Teams.wasm", "a\\b.wasm", long.as_str(), "om.dll", ".wasm", "a/b.wasm"] {
            let result = check(&manifest_json(&[(bad, "module", 1)]));
            assert!(matches!(result, Err(UpdateError::Rejected(_))), "{bad} accepted");
        }
        for good in ["teams.wasm", "test-x9.wasm"] {
            check(&manifest_json(&[("om.exe", "exe", 1), (good, "module", 1)])).expect(good);
        }
        // `om.exe` must be the exe and the exe must be `om.exe`.
        assert!(check(&manifest_json(&[("om.exe", "module", 1)])).is_err());
        assert!(check(&manifest_json(&[("teams.wasm", "exe", 1)])).is_err());
    }

    #[test]
    fn manifest_without_om_exe_is_rejected() {
        let json = manifest_json(&[("teams.wasm", "module", 1)]);
        assert!(matches!(check(&json), Err(UpdateError::Rejected(_))));
    }

    #[test]
    fn bad_hash_is_rejected() {
        let json = String::from_utf8(manifest_json(&[("om.exe", "exe", 1)])).expect("utf8");
        for hash in ["abc", &HASH.to_uppercase()] {
            let result = check(json.replace(HASH, hash).as_bytes());
            assert!(matches!(result, Err(UpdateError::Rejected(_))), "{hash} accepted");
        }
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let json = manifest_json(&[("teams.wasm", "module", 1), ("teams.wasm", "module", 1)]);
        assert!(matches!(check(&json), Err(UpdateError::Rejected(_))));
    }

    #[test]
    fn total_over_256_mib_is_rejected() {
        let mib = 1024 * 1024;
        check(&manifest_json(&[("om.exe", "exe", 128 * mib), ("a.wasm", "module", 128 * mib)])).expect("exactly the cap");
        let json = manifest_json(&[("om.exe", "exe", 128 * mib), ("a.wasm", "module", 128 * mib + 1)]);
        assert!(matches!(check(&json), Err(UpdateError::Rejected(_))));
        let json = manifest_json(&[("om.exe", "exe", u64::MAX), ("a.wasm", "module", u64::MAX)]);
        assert!(matches!(check(&json), Err(UpdateError::Rejected(_))));
    }

    #[test]
    fn version_order() {
        let v = |s: &str| Version::parse(s).expect(s);
        assert!(v("0.10.0") > v("0.9.9"));
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        assert_eq!(Version::from_tag("v1.2.3").expect("tag"), v("1.2.3"));
        for bad in ["v1.2", "1.2", "1.2.3-rc1", "1.2.3.4", "1.2.x", "+1.2.3", "", "1..3"] {
            assert!(Version::parse(bad).is_err(), "{bad} accepted");
        }
        assert!(Version::from_tag("1.2.3").is_err());
        assert!(Version::from_tag("v1.2.3-rc1").is_err());
    }
}
