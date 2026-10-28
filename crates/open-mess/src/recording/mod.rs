//! Local recordings of Teams calls (spec 2026-10-28-local-transcription). Each recording is a
//! folder under `<local>\recordings` (`%LOCALAPPDATA%\open_mess`, never the data folder, which
//! may roam to a server): `meta.json` with its state, the tracks' WAV files until the transcription
//! is done, and `transcript.json`. The folder is the record; nothing of it is in the database.

pub mod model;
pub mod stt;
pub mod wav;

use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The tracks: what Teams plays, and the owner's microphone (4.2).
pub const TRACKS: [&str; 2] = ["others.wav", "me.wav"];
const META: &str = "meta.json";
const TRANSCRIPT: &str = "transcript.json";
const MAX_META: u64 = 64 * 1024;
/// One transcript, written and read (4.3).
pub const MAX_TRANSCRIPT: u64 = 16 * 1024 * 1024;
/// Audio not transcribed this long after its recording ended is deleted (4.4).
pub const AUDIO_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// All audio on disk, waiting or failed (4.4).
pub const MAX_AUDIO_TOTAL: u64 = 10 * 1024 * 1024 * 1024;
/// All transcripts (4.4).
pub const MAX_TRANSCRIPTS_TOTAL: u64 = 1024 * 1024 * 1024;
/// Free disk space below which no recording starts and a running one ends (4.4).
pub const MIN_FREE_DISK: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Recording,
    Waiting,
    Transcribing,
    Done,
    Failed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Recording => "recording",
            State::Waiting => "waiting",
            State::Transcribing => "transcribing",
            State::Done => "done",
            State::Failed => "failed",
        }
    }
}

/// Why a recording was cut short, failed or lost its audio: a fixed set, so nothing from outside
/// reaches an agent through it (4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// The daemon stopped while it recorded.
    #[serde(rename = "interrupted")]
    Interrupted,
    /// Not transcribed within [`AUDIO_RETENTION`]; the audio is gone.
    #[serde(rename = "expired")]
    Expired,
    /// `meta.json` cannot be read.
    #[serde(rename = "damaged")]
    Damaged,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "capture failed")]
    CaptureFailed,
    #[serde(rename = "write failed")]
    WriteFailed,
}

/// `meta.json`. Unknown fields are ignored, not refused: a rolled-back daemon still reads the
/// recordings of a newer one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub state: State,
    /// Wall-clock start and end, in milliseconds since 1970.
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub language: Option<String>,
    pub reason: Option<Reason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Me,
    Others,
}

/// One piece of speech, its times from the recording's start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: Speaker,
    pub text: String,
}

/// `transcript.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    pub cues: Vec<Cue>,
}

/// A recording as the list shows it; one whose `meta.json` cannot be read is `failed`, "damaged".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub meta: Meta,
}

/// Why a recording cannot start (4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    DiskFull,
    AudioLimit,
    TranscriptLimit,
}

/// A recording's id: 32 lowercase hex digits. Anything else never reaches a path.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|since| since.as_millis() as u64).unwrap_or(0)
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no such recording")
}

/// Writes the whole file next to `path` and moves it over `path`: after a crash the old file or
/// the new one is there, never part of one.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut out = File::create(&temp)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    fs::rename(&temp, path)
}

fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(bytes)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The recordings folder.
#[derive(Clone, Debug)]
pub struct Recordings {
    dir: PathBuf,
}

impl Recordings {
    /// `local` is `Paths::local` (update/state.rs).
    pub fn new(local: &Path) -> Recordings {
        Recordings { dir: local.join("recordings") }
    }

    /// A recording's folder; None for an id that is not one.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        valid_id(id).then(|| self.dir.join(id))
    }

    /// A new recording in state `recording`, started at `now_ms`; returns its id.
    pub fn create(&self, now_ms: u64) -> io::Result<String> {
        fs::create_dir_all(&self.dir)?;
        let meta = Meta { state: State::Recording, start_ms: now_ms, end_ms: None, language: None, reason: None };
        for _ in 0..8 {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            match fs::create_dir(self.dir.join(&id)) {
                Ok(()) => {
                    self.set_meta(&id, &meta)?;
                    return Ok(id);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::other("no free recording id"))
    }

    pub fn meta(&self, id: &str) -> io::Result<Meta> {
        let path = self.path(id).ok_or_else(not_found)?;
        serde_json::from_slice(&read_capped(&path.join(META), MAX_META)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub fn set_meta(&self, id: &str, meta: &Meta) -> io::Result<()> {
        let path = self.path(id).ok_or_else(not_found)?;
        write_atomic(&path.join(META), &serde_json::to_vec(meta).map_err(io::Error::other)?)
    }

    /// The transcript of a recording, up to [`MAX_TRANSCRIPT`]; a larger or malformed file is
    /// `InvalidData`.
    pub fn transcript(&self, id: &str) -> io::Result<Transcript> {
        let path = self.path(id).ok_or_else(not_found)?;
        serde_json::from_slice(&read_capped(&path.join(TRANSCRIPT), MAX_TRANSCRIPT)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// Ends a transcription in the order that survives a crash anywhere (4.3): the transcript,
    /// then the state `done`, then the audio is deleted.
    pub fn finish_transcript(&self, id: &str, transcript: &Transcript, language: Option<String>) -> io::Result<()> {
        let path = self.path(id).ok_or_else(not_found)?;
        let bytes = serde_json::to_vec(transcript).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_TRANSCRIPT {
            return Err(io::Error::other("the transcript is over its size limit"));
        }
        write_atomic(&path.join(TRANSCRIPT), &bytes)?;
        let meta = self.meta(id)?;
        self.set_meta(id, &Meta { state: State::Done, language, ..meta })?;
        delete_audio(&path)
    }

    /// Every recording, newest first.
    pub fn list(&self) -> Vec<Entry> {
        let Ok(dirs) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut entries: Vec<Entry> = dirs
            .flatten()
            .filter_map(|dir| {
                let id = dir.file_name().into_string().ok().filter(|id| valid_id(id))?;
                let meta = self.meta(&id).unwrap_or_else(|_| Meta {
                    state: State::Failed,
                    start_ms: dir.metadata().and_then(|meta| meta.modified()).map(millis).unwrap_or(0),
                    end_ms: None,
                    language: None,
                    reason: Some(Reason::Damaged),
                });
                Some(Entry { id, meta })
            })
            .collect();
        entries.sort_by(|a, b| b.meta.start_ms.cmp(&a.meta.start_ms).then_with(|| a.id.cmp(&b.id)));
        entries
    }

    pub fn delete(&self, id: &str) -> io::Result<()> {
        fs::remove_dir_all(self.path(id).ok_or_else(not_found)?)
    }

    /// At daemon start and in the daily pass with [`Recordings::expire`] (4.4): a recording the
    /// daemon left gets valid tracks and waits for its transcription, marked interrupted; a
    /// transcription that wrote its transcript is done; a done recording loses leftover audio.
    /// `busy` are the recording and the transcription running now, which are left alone. What
    /// could not be put right (a track another program holds) is returned and tried again in the
    /// next pass.
    pub fn recover(&self, busy: &[&str]) -> Vec<(String, io::Error)> {
        let mut failures = Vec::new();
        for Entry { id, meta } in self.list() {
            if busy.contains(&id.as_str()) {
                continue;
            }
            let Some(path) = self.path(&id) else { continue };
            let result = match meta.state {
                State::Recording => self.recover_recording(&id, &path, meta),
                State::Transcribing if self.transcript(&id).is_ok() => {
                    self.set_meta(&id, &Meta { state: State::Done, ..meta }).and_then(|()| delete_audio(&path))
                }
                State::Transcribing => self.set_meta(&id, &Meta { state: State::Waiting, ..meta }),
                State::Done => delete_audio(&path),
                State::Waiting | State::Failed => Ok(()),
            };
            if let Err(err) = result {
                failures.push((id, err));
            }
        }
        failures
    }

    /// A track shorter than its header (the daemon stopped as it was created) holds no audio and
    /// is deleted, and a recording with no track left is `failed`. Any other error (a file another
    /// program holds) deletes nothing: the recording stays as it is for the next pass.
    fn recover_recording(&self, id: &str, path: &Path, meta: Meta) -> io::Result<()> {
        let mut end_ms = meta.start_ms;
        let mut tracks = 0;
        for track in TRACKS {
            let file = path.join(track);
            // The last write, read before the fix writes the header: the retention counts from it.
            let Ok(found) = fs::metadata(&file) else { continue };
            end_ms = end_ms.max(found.modified().map(millis).unwrap_or(0));
            if found.len() < wav::HEADER {
                fs::remove_file(&file)?;
            } else {
                wav::fix_header(&file)?;
                tracks += 1;
            }
        }
        let state = if tracks == 0 { State::Failed } else { State::Waiting };
        self.set_meta(id, &Meta { state, end_ms: Some(end_ms), reason: Some(Reason::Interrupted), ..meta })
    }

    /// Deletes the audio of recordings not transcribed [`AUDIO_RETENTION`] after they ended
    /// (4.4); a waiting one becomes `failed`, "expired". Recording and transcribing ones are left.
    pub fn expire(&self, now_ms: u64) -> Vec<(String, io::Error)> {
        let retention = AUDIO_RETENTION.as_millis() as u64;
        let mut failures = Vec::new();
        for Entry { id, meta } in self.list() {
            let Some(path) = self.path(&id) else { continue };
            let old = meta.end_ms.unwrap_or(meta.start_ms).saturating_add(retention) <= now_ms;
            let result = match meta.state {
                State::Waiting if old => {
                    delete_audio(&path).and_then(|()| self.set_meta(&id, &Meta { state: State::Failed, reason: Some(Reason::Expired), ..meta }))
                }
                State::Failed if old => delete_audio(&path),
                _ => Ok(()),
            };
            if let Err(err) = result {
                failures.push((id, err));
            }
        }
        failures
    }

    /// Whether the audio of a recording is still there (for "Ponów", 4.6).
    pub fn has_audio(&self, id: &str) -> bool {
        self.path(id).is_some_and(|path| TRACKS.iter().any(|track| path.join(track).exists()))
    }

    /// The bytes of all audio and of all transcripts.
    pub fn usage(&self) -> (u64, u64) {
        let Ok(dirs) = fs::read_dir(&self.dir) else { return (0, 0) };
        dirs.flatten().fold((0, 0), |(audio, text), dir| {
            let path = dir.path();
            (audio + TRACKS.iter().map(|track| file_len(&path.join(track))).sum::<u64>(), text + file_len(&path.join(TRANSCRIPT)))
        })
    }

    /// Why a new recording may not start, given the free space on the disk (4.4).
    pub fn refusal(&self, free_disk: u64) -> Option<Refusal> {
        refuse(free_disk, self.usage(), MAX_AUDIO_TOTAL, MAX_TRANSCRIPTS_TOTAL)
    }
}

fn refuse(free_disk: u64, (audio, text): (u64, u64), max_audio: u64, max_text: u64) -> Option<Refusal> {
    if free_disk < MIN_FREE_DISK {
        Some(Refusal::DiskFull)
    } else if audio >= max_audio {
        Some(Refusal::AudioLimit)
    } else if text >= max_text {
        Some(Refusal::TranscriptLimit)
    } else {
        None
    }
}

fn delete_audio(path: &Path) -> io::Result<()> {
    for track in TRACKS {
        match fs::remove_file(path.join(track)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn recordings() -> (tempfile::TempDir, Recordings) {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        (dir, recordings)
    }

    fn with_track(recordings: &Recordings, id: &str, samples: &[i16]) {
        let mut writer = wav::Writer::create(&recordings.path(id).unwrap().join("others.wav")).unwrap();
        writer.write(samples).unwrap();
        writer.finish().unwrap();
    }

    fn transcript() -> Transcript {
        Transcript { cues: vec![Cue { start_ms: 0, end_ms: 900, speaker: Speaker::Others, text: "dzień dobry".into() }] }
    }

    #[test]
    fn ids_are_checked_before_any_path() {
        let (_dir, recordings) = recordings();
        assert!(valid_id("0123456789abcdef0123456789abcdef"));
        for bad in ["", "..", "..\\x", "0123456789ABCDEF0123456789ABCDEF", "0123456789abcdef0123456789abcde", "0123456789abcdef0123456789abcdef0"] {
            assert!(!valid_id(bad), "{bad}");
            assert!(recordings.path(bad).is_none());
            assert_eq!(recordings.meta(bad).unwrap_err().kind(), io::ErrorKind::NotFound);
            assert!(recordings.delete(bad).is_err());
        }
    }

    #[test]
    fn a_new_recording_is_listed_newest_first() {
        let (_dir, recordings) = recordings();
        let old = recordings.create(1_000).unwrap();
        let new = recordings.create(2_000).unwrap();
        assert!(valid_id(&old) && old != new);
        let list = recordings.list();
        assert_eq!(list.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), vec![new.as_str(), old.as_str()]);
        assert_eq!(list.first().unwrap().meta.state, State::Recording);
        recordings.delete(&old).unwrap();
        assert_eq!(recordings.list().len(), 1);
    }

    #[test]
    fn an_unreadable_meta_is_listed_as_damaged() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(1_000).unwrap();
        fs::write(recordings.path(&id).unwrap().join(META), b"{not json").unwrap();
        let meta = recordings.list().first().unwrap().meta.clone();
        assert_eq!((meta.state, meta.reason), (State::Failed, Some(Reason::Damaged)));
        // A folder that is not a recording's is not listed.
        fs::create_dir_all(recordings.dir.join("not-an-id")).unwrap();
        assert_eq!(recordings.list().len(), 1);
    }

    #[test]
    fn a_recording_the_daemon_left_waits_marked_interrupted() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(1_000).unwrap();
        // The writer never finished: the header still says no samples.
        let mut writer = wav::Writer::create(&recordings.path(&id).unwrap().join("me.wav")).unwrap();
        writer.write(&[1, 2, 3]).unwrap();
        drop(writer);
        assert!(recordings.recover(&[]).is_empty());
        let meta = recordings.meta(&id).unwrap();
        assert_eq!((meta.state, meta.reason), (State::Waiting, Some(Reason::Interrupted)));
        assert!(meta.end_ms.is_some());
        let mut reader = wav::Reader::open(&recordings.path(&id).unwrap().join("me.wav")).unwrap();
        assert_eq!(reader.remaining(), 3);
        let mut samples = Vec::new();
        reader.read(&mut samples, 10).unwrap();
        assert_eq!(samples, vec![1, 2, 3]);
    }

    #[test]
    fn a_left_recording_ends_at_its_last_write_not_at_the_fix() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(0).unwrap();
        let file = recordings.path(&id).unwrap().join("others.wav");
        drop(wav::Writer::create(&file).unwrap());
        // Last written long ago: the daemon comes back days later.
        File::options().write(true).open(&file).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(1_000)).unwrap();
        assert!(recordings.recover(&[]).is_empty());
        assert_eq!(recordings.meta(&id).unwrap().end_ms, Some(1_000_000));
    }

    #[test]
    fn a_track_shorter_than_its_header_is_dropped_not_left_recording() {
        let (_dir, recordings) = recordings();
        let both = recordings.create(0).unwrap();
        let empty = recordings.create(0).unwrap();
        // The daemon stopped as the tracks were created: nothing of them reached the disk.
        fs::write(recordings.path(&both).unwrap().join("me.wav"), b"").unwrap();
        with_track(&recordings, &both, &[1]);
        fs::write(recordings.path(&empty).unwrap().join("others.wav"), b"RIFF").unwrap();
        assert!(recordings.recover(&[]).is_empty());
        let meta = recordings.meta(&both).unwrap();
        assert_eq!((meta.state, meta.reason), (State::Waiting, Some(Reason::Interrupted)));
        assert!(!recordings.path(&both).unwrap().join("me.wav").exists());
        let meta = recordings.meta(&empty).unwrap();
        assert_eq!((meta.state, meta.reason), (State::Failed, Some(Reason::Interrupted)));
        assert!(!recordings.has_audio(&empty));
    }

    #[test]
    fn the_daily_pass_leaves_the_running_recording_and_transcription_alone() {
        let (_dir, recordings) = recordings();
        let recording = recordings.create(0).unwrap();
        let transcribing = recordings.create(0).unwrap();
        with_track(&recordings, &transcribing, &[1]);
        recordings.set_meta(&transcribing, &Meta { state: State::Transcribing, ..recordings.meta(&transcribing).unwrap() }).unwrap();
        assert!(recordings.recover(&[recording.as_str(), transcribing.as_str()]).is_empty());
        assert_eq!(recordings.meta(&recording).unwrap().state, State::Recording);
        assert_eq!(recordings.meta(&transcribing).unwrap().state, State::Transcribing);
    }

    #[test]
    fn a_track_that_cannot_be_fixed_now_is_kept_for_the_next_pass() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(0).unwrap();
        let file = recordings.path(&id).unwrap().join("others.wav");
        let mut writer = wav::Writer::create(&file).unwrap();
        writer.write(&[1, 2]).unwrap();
        drop(writer);
        // Not writable now, as when another program holds it.
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions.clone()).unwrap();
        assert_eq!(recordings.recover(&[]).len(), 1);
        assert!(file.exists());
        assert_eq!(recordings.meta(&id).unwrap().state, State::Recording);
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&file, permissions).unwrap();
        assert!(recordings.recover(&[]).is_empty());
        assert_eq!(recordings.meta(&id).unwrap().state, State::Waiting);
        assert!(recordings.has_audio(&id));
    }

    #[test]
    fn a_crash_anywhere_in_finishing_leaves_the_transcript_or_the_audio() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(1_000).unwrap();
        with_track(&recordings, &id, &[1, 2]);
        let path = recordings.path(&id).unwrap();
        let transcribing = Meta { state: State::Transcribing, end_ms: Some(2_000), ..recordings.meta(&id).unwrap() };
        recordings.set_meta(&id, &transcribing).unwrap();

        // Stopped before the transcript was written: back to waiting, audio kept.
        assert!(recordings.recover(&[]).is_empty());
        assert_eq!(recordings.meta(&id).unwrap().state, State::Waiting);
        assert!(recordings.has_audio(&id));

        // Stopped after the transcript, before the state: done, audio gone.
        recordings.set_meta(&id, &transcribing).unwrap();
        write_atomic(&path.join(TRANSCRIPT), &serde_json::to_vec(&transcript()).unwrap()).unwrap();
        assert!(recordings.recover(&[]).is_empty());
        assert_eq!(recordings.meta(&id).unwrap().state, State::Done);
        assert!(!recordings.has_audio(&id));

        // Stopped after the state, before the audio was deleted: the audio goes at start.
        with_track(&recordings, &id, &[1]);
        assert!(recordings.recover(&[]).is_empty());
        assert!(!recordings.has_audio(&id));
        assert_eq!(recordings.transcript(&id).unwrap(), transcript());
    }

    #[test]
    fn finishing_writes_transcript_state_then_deletes_audio() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(1_000).unwrap();
        with_track(&recordings, &id, &[1, 2]);
        recordings.finish_transcript(&id, &transcript(), Some("pl".into())).unwrap();
        let meta = recordings.meta(&id).unwrap();
        assert_eq!((meta.state, meta.language.as_deref()), (State::Done, Some("pl")));
        assert!(!recordings.has_audio(&id));
        assert_eq!(recordings.transcript(&id).unwrap(), transcript());
    }

    #[test]
    fn an_oversized_or_malformed_transcript_is_refused() {
        let (_dir, recordings) = recordings();
        let id = recordings.create(1_000).unwrap();
        let path = recordings.path(&id).unwrap().join(TRANSCRIPT);
        fs::write(&path, b"[1, 2]").unwrap();
        assert_eq!(recordings.transcript(&id).unwrap_err().kind(), io::ErrorKind::InvalidData);
        fs::write(&path, vec![b' '; MAX_TRANSCRIPT as usize + 1]).unwrap();
        assert_eq!(recordings.transcript(&id).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn audio_not_transcribed_in_seven_days_goes() {
        let (_dir, recordings) = recordings();
        let waiting = recordings.create(0).unwrap();
        let failed = recordings.create(0).unwrap();
        let fresh = recordings.create(0).unwrap();
        let transcribing = recordings.create(0).unwrap();
        for (id, state, end) in
            [(&waiting, State::Waiting, 0), (&failed, State::Failed, 0), (&fresh, State::Waiting, 5 * DAY), (&transcribing, State::Transcribing, 0)]
        {
            with_track(&recordings, id, &[1]);
            let reason = (state == State::Failed).then_some(Reason::Failed);
            recordings.set_meta(id, &Meta { state, end_ms: Some(end), reason, ..recordings.meta(id).unwrap() }).unwrap();
        }
        assert!(recordings.expire(8 * DAY).is_empty());
        let meta = recordings.meta(&waiting).unwrap();
        assert_eq!((meta.state, meta.reason), (State::Failed, Some(Reason::Expired)));
        assert!(!recordings.has_audio(&waiting));
        // A failed one keeps its reason, without its audio.
        assert_eq!(recordings.meta(&failed).unwrap().reason, Some(Reason::Failed));
        assert!(!recordings.has_audio(&failed));
        assert!(recordings.has_audio(&fresh) && recordings.has_audio(&transcribing));
    }

    #[test]
    fn limits_refuse_a_new_recording() {
        let (_dir, recordings) = recordings();
        assert_eq!(recordings.refusal(MIN_FREE_DISK), None);
        assert_eq!(recordings.refusal(MIN_FREE_DISK - 1), Some(Refusal::DiskFull));
        let done = recordings.create(0).unwrap();
        with_track(&recordings, &done, &[1, 2, 3]);
        recordings.finish_transcript(&done, &transcript(), None).unwrap();
        let waiting = recordings.create(0).unwrap();
        with_track(&recordings, &waiting, &[1, 2, 3]);
        let usage = recordings.usage();
        // Only the waiting recording still has audio: its header and three samples.
        assert_eq!(usage.0, 44 + 6);
        let text = usage.1;
        assert!(text > 0);
        assert_eq!(refuse(MIN_FREE_DISK, usage, 50, text + 1), Some(Refusal::AudioLimit));
        assert_eq!(refuse(MIN_FREE_DISK, usage, 51, text), Some(Refusal::TranscriptLimit));
        assert_eq!(refuse(MIN_FREE_DISK, usage, 51, text + 1), None);
    }
}
