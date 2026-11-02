//! The transcription queue (spec 2026-10-28-local-transcription, 4.3): one recording at a time,
//! the oldest waiting first, and only while nothing records. A call that starts cancels the
//! transcription, which waits again and starts over after the call. The model is loaded for each
//! recording and dropped after it, so the daemon's memory comes back between them. The same
//! thread runs the retention pass every hour (4.4).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::stt::{self, SttError};
use super::{Meta, Reason, Recordings, State, Transcript, millis, model};

/// Transcribes the tracks of a recording's folder, in the language given (None: the model finds
/// it), stopping once the flag is set, with its progress in percent.
pub type Engine = dyn Fn(&Path, Option<&str>, &AtomicBool, &AtomicU8) -> Result<Transcript, SttError> + Send + Sync;
/// Whether a transcription may start now; the reason it may not.
pub type Ready = dyn Fn() -> Result<(), &'static str> + Send + Sync;
/// The language setting: a Whisper code, or None for `auto`.
pub type Language = dyn Fn() -> Option<String> + Send + Sync;

/// How often the retention pass runs (4.4), and how long the queue sleeps with nothing to do.
const PASS: Duration = Duration::from_secs(60 * 60);
const IDLE: Duration = Duration::from_secs(60);
/// Free memory a transcription needs: its peak in step 0e (4.54 GB) and 2 GB to spare (4.3).
const MEMORY: u64 = 6_600 * 1024 * 1024;

/// The transcription running now.
struct Running {
    id: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU8>,
}

#[derive(Default)]
struct Inner {
    /// The recording being made: nothing is transcribed meanwhile.
    recording: Option<String>,
    running: Option<Running>,
    /// Why recordings wait although nothing records.
    reason: Option<&'static str>,
    /// Recordings being deleted: not started again meanwhile.
    deleting: Vec<String>,
    /// Something changed since the queue last looked.
    changed: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Shared {
    fn locked(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self, edit: impl FnOnce(&mut Inner)) {
        let mut inner = self.locked();
        edit(&mut inner);
        inner.changed = true;
        self.changed.notify_all();
    }
}

/// What `status` and the window show of the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueView {
    pub running: Option<String>,
    pub reason: Option<&'static str>,
}

pub struct Queue {
    shared: Arc<Shared>,
}

impl Queue {
    /// Starts the queue's thread over the recordings in `local`.
    pub fn start(local: &Path, engine: Arc<Engine>, ready: Arc<Ready>, language: Arc<Language>) -> std::io::Result<Queue> {
        let shared = Arc::new(Shared { inner: Mutex::default(), changed: Condvar::new() });
        let worker = Worker { shared: Arc::clone(&shared), recordings: Recordings::new(local), engine, ready, language };
        std::thread::Builder::new().name("transcription queue".into()).spawn(move || worker.run())?;
        Ok(Queue { shared })
    }

    /// A recording started (`Some`): the running transcription stops and nothing starts until it
    /// ends (`None`).
    pub fn recording(&self, id: Option<&str>) {
        self.shared.change(|inner| {
            inner.recording = id.map(str::to_owned);
            if let Some(running) = inner.running.as_ref().filter(|_| id.is_some()) {
                running.cancel.store(true, Ordering::SeqCst);
            }
        });
    }

    /// Something a transcription waits for may have come: a recording waits, a failed one is
    /// tried again.
    pub fn wake(&self) {
        self.shared.change(|_| ());
    }

    /// Deletes the recording `id` from `recordings`. Its transcription, if it runs, stops first and
    /// is not started again, so no file of the folder is open when it goes (4.3, 4.6).
    pub fn delete(&self, recordings: &Recordings, id: &str) -> std::io::Result<()> {
        {
            let mut inner = self.shared.locked();
            inner.deleting.push(id.to_owned());
            while let Some(running) = inner.running.as_ref().filter(|running| running.id == id) {
                running.cancel.store(true, Ordering::SeqCst);
                inner = self.shared.changed.wait(inner).unwrap_or_else(PoisonError::into_inner);
            }
        }
        let deleted = recordings.delete(id);
        self.shared.change(|inner| inner.deleting.retain(|deleting| deleting != id));
        deleted
    }

    /// The progress of `id` in percent, while it is transcribed.
    pub fn progress(&self, id: &str) -> Option<u8> {
        let inner = self.shared.locked();
        inner.running.as_ref().filter(|running| running.id == id).map(|running| running.progress.load(Ordering::SeqCst))
    }

    pub fn view(&self) -> QueueView {
        let inner = self.shared.locked();
        QueueView { running: inner.running.as_ref().map(|running| running.id.clone()), reason: inner.reason }
    }
}

struct Worker {
    shared: Arc<Shared>,
    recordings: Recordings,
    engine: Arc<Engine>,
    ready: Arc<Ready>,
    language: Arc<Language>,
}

impl Worker {
    fn run(self) {
        let mut pass: Option<Instant> = None;
        loop {
            if pass.is_none_or(|at| at.elapsed() >= PASS) {
                let busy = self.shared.locked().recording.clone();
                // ponytail: what a pass cannot put right is tried in the next one; reported once a
                // log sink exists.
                let _ = self.recordings.recover(&busy.iter().map(String::as_str).collect::<Vec<_>>());
                let _ = self.recordings.expire(millis(SystemTime::now()));
                pass = Some(Instant::now());
            }
            // A transcription that came to nothing (stopped, could not start) waits for a change,
            // as an empty queue does, rather than starting again at once.
            let moved = self.next().is_some_and(|id| self.transcribe(id));
            if !moved {
                // Only for the reason, as things stand after a transcription that came to nothing.
                let _ = self.next();
                {
                    let inner = self.shared.locked();
                    let (mut inner, _) =
                        self.shared.changed.wait_timeout_while(inner, IDLE, |inner| !inner.changed).unwrap_or_else(PoisonError::into_inner);
                    inner.changed = false;
                }
            }
        }
    }

    /// The oldest waiting recording, when one may start now.
    fn next(&self) -> Option<String> {
        let waiting: Vec<String> =
            self.recordings.list().into_iter().rev().filter(|entry| entry.meta.state == State::Waiting).map(|entry| entry.id).collect();
        let mut inner = self.shared.locked();
        let oldest = waiting.into_iter().find(|id| !inner.deleting.contains(id))?;
        if inner.recording.is_some() {
            inner.reason = Some("recording");
            return None;
        }
        let ready = (self.ready)();
        inner.reason = ready.err();
        ready.ok().map(|()| oldest)
    }

    /// Runs the transcription of `id`; true when it ended done or failed.
    fn transcribe(&self, id: String) -> bool {
        let (cancel, progress) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU8::new(0)));
        {
            let mut inner = self.shared.locked();
            if inner.recording.is_some() || inner.deleting.contains(&id) {
                return false;
            }
            inner.running = Some(Running { id: id.clone(), cancel: Arc::clone(&cancel), progress: Arc::clone(&progress) });
        }
        let outcome = match (self.recordings.meta(&id), self.recordings.path(&id)) {
            (Ok(meta), Some(folder)) if self.recordings.set_meta(&id, &Meta { state: State::Transcribing, ..meta.clone() }).is_ok() => {
                let (engine, language) = (Arc::clone(&self.engine), (self.language)());
                let result = stt::isolated(move || engine(&folder, language.as_deref(), &cancel, &progress));
                self.finish(&id, meta, result)
            }
            _ => false,
        };
        // Not a change for the queue itself (that would start it again at once); it wakes those
        // waiting for this transcription to end.
        self.shared.locked().running = None;
        self.shared.changed.notify_all();
        outcome
    }

    /// Writes what a transcription came to; true when the recording is done or failed.
    fn finish(&self, id: &str, meta: Meta, result: Result<Transcript, SttError>) -> bool {
        // ponytail: a write that fails leaves `transcribing`, which the next pass puts back to waiting.
        match result {
            Ok(transcript) => self.recordings.finish_transcript(id, &transcript).is_ok(),
            // Stopped by a call or a deletion, or the model is damaged (4.7): the recording is not
            // at fault and waits again, if it is still there.
            Err(SttError::Cancelled | SttError::Damaged) => {
                let _ = self.recordings.set_meta(id, &Meta { state: State::Waiting, ..meta });
                false
            }
            // Its own tracks, among others a bad WAV file (4.4): this recording fails, the queue goes on.
            Err(_) => self.recordings.set_meta(id, &Meta { state: State::Failed, reason: Some(Reason::Failed), ..meta }).is_ok(),
        }
    }
}

/// When the model's weights in `local` were last written, if there is a model.
fn written(local: &Path) -> Option<SystemTime> {
    let revision = model::installed(local)?;
    let [.., weights] = &revision.files;
    fs::metadata(model::folder(local, revision).join(weights.name)).and_then(|found| found.modified()).ok()
}

/// The pinned Whisper model in `local` (4.3): what transcribes, and the gate a transcription
/// waits at. It starts with the model on disk and [`MEMORY`] free: Rust ends the process on a
/// failed allocation, so the gate is what keeps the daemon up. A model that failed its checksum
/// holds the gate ("model damaged") until its files change, as a new download changes them.
pub fn whisper(local: PathBuf) -> (Arc<Engine>, Arc<Ready>) {
    let damaged: Arc<Mutex<Option<Option<SystemTime>>>> = Arc::default();
    let engine = {
        let (local, damaged) = (local.clone(), Arc::clone(&damaged));
        Arc::new(move |folder: &Path, language: Option<&str>, cancel: &AtomicBool, progress: &AtomicU8| {
            // Deleted since the gate looked: the recording waits, like with a damaged model.
            let revision = model::installed(&local).ok_or(SttError::Damaged)?;
            let model = stt::Model::load(&local, revision, cancel);
            if matches!(model, Err(SttError::Damaged)) {
                *damaged.lock().unwrap_or_else(PoisonError::into_inner) = Some(written(&local));
            }
            model?.transcribe(folder, language, cancel, progress)
        })
    };
    let ready = Arc::new(move || {
        if model::installed(&local).is_none() {
            return Err("no model");
        }
        if damaged.lock().unwrap_or_else(PoisonError::into_inner).is_some_and(|when| when == written(&local)) {
            return Err("model damaged");
        }
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        if system.available_memory() < MEMORY {
            return Err("low memory");
        }
        Ok(())
    });
    (engine, ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{Cue, Speaker};

    fn transcript() -> Transcript {
        Transcript { language: Some("pl".into()), cues: vec![Cue { start_ms: 0, end_ms: 900, speaker: Speaker::Others, text: "dzień".into() }] }
    }

    /// A recording waiting for its transcription, `start` ms after now (recent: the retention
    /// pass at the queue's start would otherwise expire it).
    fn waiting(recordings: &Recordings, start: u64) -> String {
        let start = millis(SystemTime::now()) + start;
        let id = recordings.create(start).unwrap();
        recordings.set_meta(&id, &Meta { state: State::Waiting, end_ms: Some(start + 1), ..recordings.meta(&id).unwrap() }).unwrap();
        id
    }

    fn state(recordings: &Recordings, id: &str) -> State {
        recordings.meta(id).unwrap().state
    }

    fn eventually(what: impl Fn() -> bool) {
        let started = Instant::now();
        while !what() {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(local: &Path, engine: impl Fn(&Path, &AtomicBool) -> Result<Transcript, SttError> + Send + Sync + 'static) -> Queue {
        Queue::start(local, Arc::new(move |folder, _, cancel, _| engine(folder, cancel)), Arc::new(|| Ok(())), Arc::new(|| None)).unwrap()
    }

    /// Runs until it is cancelled.
    fn until_cancelled(cancel: &AtomicBool) -> Result<Transcript, SttError> {
        while !cancel.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(SttError::Cancelled)
    }

    #[test]
    fn waiting_recordings_are_transcribed_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let (new, old) = (waiting(&recordings, 2_000), waiting(&recordings, 1_000));
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&order);
        let _queue = start(dir.path(), move |folder, _| {
            seen.lock().unwrap().push(folder.file_name().unwrap().to_string_lossy().into_owned());
            Ok(transcript())
        });
        eventually(|| state(&recordings, &new) == State::Done);
        assert_eq!(*order.lock().unwrap(), [old.clone(), new.clone()]);
        assert_eq!(recordings.meta(&old).unwrap().language.as_deref(), Some("pl"));
    }

    #[test]
    fn a_call_stops_the_transcription_which_starts_over_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = waiting(&recordings, 1_000);
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let queue = start(dir.path(), move |_, cancel| {
            *counted.lock().unwrap() += 1;
            if *counted.lock().unwrap() == 1 { until_cancelled(cancel) } else { Ok(transcript()) }
        });
        eventually(|| queue.progress(&id).is_some());
        queue.recording(Some("0123456789abcdef0123456789abcdef"));
        // Back to waiting, held while the call lasts.
        eventually(|| state(&recordings, &id) == State::Waiting && queue.view() == QueueView { running: None, reason: Some("recording") });
        queue.recording(None);
        eventually(|| state(&recordings, &id) == State::Done);
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn a_panic_fails_its_recording_and_the_queue_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let (first, second) = (waiting(&recordings, 1_000), waiting(&recordings, 2_000));
        let bad = first.clone();
        let _queue = start(dir.path(), move |folder, _| {
            if folder.ends_with(&bad) {
                panic!("boom");
            }
            Ok(transcript())
        });
        eventually(|| state(&recordings, &second) == State::Done);
        let meta = recordings.meta(&first).unwrap();
        assert_eq!((meta.state, meta.reason), (State::Failed, Some(Reason::Failed)));
    }

    #[test]
    fn nothing_starts_until_it_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = waiting(&recordings, 1_000);
        let present = Arc::new(AtomicBool::new(false));
        let model = Arc::clone(&present);
        let ready = Arc::new(move || if model.load(Ordering::SeqCst) { Ok(()) } else { Err("no model") });
        let engine = Arc::new(|_: &Path, _: Option<&str>, _: &AtomicBool, _: &AtomicU8| Ok(transcript()));
        let queue = Queue::start(dir.path(), engine, ready, Arc::new(|| None)).unwrap();
        eventually(|| queue.view().reason == Some("no model"));
        assert_eq!(state(&recordings, &id), State::Waiting);
        present.store(true, Ordering::SeqCst);
        queue.wake();
        eventually(|| state(&recordings, &id) == State::Done);
    }

    #[test]
    fn a_damaged_model_leaves_the_recording_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = waiting(&recordings, 1_000);
        // As whisper() pairs them: the engine finds the damage, the gate holds until it is fixed.
        let (damaged, fixed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (found, model) = (Arc::clone(&damaged), Arc::clone(&fixed));
        let engine = Arc::new(move |_: &Path, _: Option<&str>, _: &AtomicBool, _: &AtomicU8| {
            if model.load(Ordering::SeqCst) {
                return Ok(transcript());
            }
            found.store(true, Ordering::SeqCst);
            Err(SttError::Damaged)
        });
        let gate = (Arc::clone(&damaged), Arc::clone(&fixed));
        let ready = Arc::new(move || if gate.0.load(Ordering::SeqCst) && !gate.1.load(Ordering::SeqCst) { Err("model damaged") } else { Ok(()) });
        let queue = Queue::start(dir.path(), engine, ready, Arc::new(|| None)).unwrap();
        eventually(|| queue.view().reason == Some("model damaged"));
        assert_eq!(state(&recordings, &id), State::Waiting);
        fixed.store(true, Ordering::SeqCst);
        queue.wake();
        eventually(|| state(&recordings, &id) == State::Done);
    }

    #[test]
    fn a_bad_track_fails_its_recording_not_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let (bad, good) = (waiting(&recordings, 1_000), waiting(&recordings, 2_000));
        let broken = bad.clone();
        let _queue = start(dir.path(), move |folder, _| {
            if folder.ends_with(&broken) { Err(std::io::Error::from(std::io::ErrorKind::InvalidData).into()) } else { Ok(transcript()) }
        });
        eventually(|| state(&recordings, &good) == State::Done);
        assert_eq!(state(&recordings, &bad), State::Failed);
    }

    #[test]
    fn a_transcription_that_comes_to_nothing_is_not_started_again_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = waiting(&recordings, 1_000);
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let queue = start(dir.path(), move |_, _| {
            *counted.lock().unwrap() += 1;
            Err(SttError::Cancelled)
        });
        eventually(|| *calls.lock().unwrap() == 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(*calls.lock().unwrap(), 1);
        // A change lets it try again.
        queue.wake();
        eventually(|| *calls.lock().unwrap() == 2 && state(&recordings, &id) == State::Waiting);
    }

    #[test]
    fn deleting_a_recording_stops_its_transcription_first() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = waiting(&recordings, 1_000);
        // Holds a file of the folder open, as a real transcription holds its tracks.
        let queue = start(dir.path(), |folder, cancel| {
            let _open = std::fs::File::open(folder.join("meta.json")).unwrap();
            until_cancelled(cancel)
        });
        eventually(|| queue.progress(&id).is_some());
        queue.delete(&recordings, &id).unwrap();
        assert!(recordings.path(&id).is_some_and(|path| !path.exists()));
        assert!(queue.view().running.is_none());
    }
}
