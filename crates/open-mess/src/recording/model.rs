//! The Whisper model's files (spec 2026-10-28-local-transcription, 4.7): one folder per allowed
//! revision under `<local>\models`, each file checked against the SHA-256 compiled in here before
//! it is used.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// One file of a revision.
#[derive(Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub sha256: &'static str,
    /// A larger file is refused unread past this.
    pub max_size: u64,
}

/// A commit of `openai/whisper-large-v3-turbo` on Hugging Face (MIT licence): `config.json`,
/// `tokenizer.json` and `model.safetensors`, in that order.
#[derive(Debug)]
pub struct Revision {
    pub commit: &'static str,
    pub files: [ModelFile; 3],
}

const MIB: u64 = 1024 * 1024;

/// The allowed revisions, newest first. The list only grows: a release never drops a revision,
/// so an update never makes a downloaded model useless (4.7).
pub const REVISIONS: &[Revision] = &[Revision {
    commit: "41f01f3fe87f28c78e2fbf8b568835947dd65ed9",
    files: [
        ModelFile { name: "config.json", sha256: "c5b526b3e3cd64cd8940dabb45e8ba726629e22d8ed389c29b552f9140daf04a", max_size: 10 * MIB },
        ModelFile { name: "tokenizer.json", sha256: "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd", max_size: 10 * MIB },
        ModelFile { name: "model.safetensors", sha256: "542566a422ae4f3fd23f1ba11add198fca01bbf82e66e6a2857b3f608b1eb9d1", max_size: 2048 * MIB },
    ],
}];

/// A revision's folder; `local` is `Paths::local` (update/state.rs).
pub fn folder(local: &Path, revision: &Revision) -> PathBuf {
    local.join("models").join(format!("whisper-large-v3-turbo-{}", revision.commit))
}

/// The newest allowed revision with all its files on disk; their sums are checked as they are read.
pub fn installed(local: &Path) -> Option<&'static Revision> {
    REVISIONS.iter().find(|revision| revision.files.iter().all(|file| folder(local, revision).join(file.name).is_file()))
}

/// Reads a model file whole. One larger than its limit, or whose SHA-256 differs, is `InvalidData`
/// (the model is damaged).
pub fn read_verified(dir: &Path, file: &ModelFile) -> io::Result<Vec<u8>> {
    let found = File::open(dir.join(file.name))?;
    // Sized up front: growing by doubling would hold up to twice the weights' 1.6 GB.
    let mut bytes = Vec::with_capacity(found.metadata()?.len().min(file.max_size + 1) as usize);
    found.take(file.max_size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > file.max_size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is larger than expected", file.name)));
    }
    let sum: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if sum != file.sha256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} does not match its checksum", file.name)));
    }
    Ok(bytes)
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
        assert_eq!(read_verified(dir.path(), &ModelFile { name: "abc", sha256: sum, max_size: 3 }).unwrap(), b"abc");
        let wrong = ModelFile { name: "abc", sha256: "00", max_size: 3 };
        assert_eq!(read_verified(dir.path(), &wrong).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let small = ModelFile { name: "abc", sha256: sum, max_size: 2 };
        assert_eq!(read_verified(dir.path(), &small).unwrap_err().kind(), io::ErrorKind::InvalidData);
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
}
