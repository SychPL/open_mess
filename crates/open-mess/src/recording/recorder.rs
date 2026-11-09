//! A recording's life (spec 2026-10-28-local-transcription, 4.1 and 4.2). The detection reports
//! the call every second; while one is on, Teams has an active session, the automatic mode is on,
//! the call was not stopped and everything allows it, the capture starts. The recording ends with
//! the call, a stop, a limit or an error, and goes to the transcription queue. Everything outside
//! (the settings, the disk, the capture, the toasts and the tray) comes in through [`Hooks`], so
//! the logic is tested without Windows.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::capture::{Control, Targets, Track};
use super::detect::Observation;
use super::queue::Queue;
use super::{MIN_FREE_DISK, Meta, Reason, Recordings, Refusal, State, UpdateLink, millis};
use crate::i18n;

/// The longest recording (4.2).
const LONGEST: Duration = Duration::from_secs(6 * 60 * 60);
/// The file that keeps a stopped call stopped across a restart of the daemon (4.1).
const SKIP: &str = "skip.json";

/// Who stopped a recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Owner,
    Agent,
}

/// The window's settings (4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub auto: bool,
    pub microphone: bool,
}

/// What the owner is shown (4.2): toasts under their own AppUserModelID, and the tray.
pub trait Notices: Send + Sync {
    /// A recording started, with or without the microphone; its buttons stop it.
    fn started(&self, id: &str, microphone: bool);
    /// A call is not recorded, and why; once a call.
    fn not_recording(&self, why: &str);
    /// A track of the recording stays silent: the microphone or the other side.
    fn track_lost(&self, microphone: bool);
    fn agent_stopped(&self);
    /// Takes the recording's toasts away, at every end of a recording.
    fn clear(&self);
}

/// Starts the capture of one recording into its tracks; it runs until the control's stop and
/// ends with the tracks finished.
pub type Capture = dyn Fn(Track, Option<Track>, Arc<Control>) -> io::Result<JoinHandle<io::Result<()>>> + Send + Sync;

/// Shows in the tray whether a recording is made.
pub type Indicator = Box<dyn Fn(bool) + Send + Sync>;

/// What the recorder reads and starts outside itself.
pub struct Hooks {
    pub settings: Box<dyn Fn() -> Settings + Send + Sync>,
    /// Free bytes on the disk of the recordings.
    pub free_disk: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The model's state for `status`: "missing", "downloading" or "ready".
    pub model: Box<dyn Fn() -> &'static str + Send + Sync>,
    pub capture: Box<Capture>,
    pub notices: Arc<dyn Notices>,
}

/// The recording being made.
struct Active {
    id: String,
    control: Arc<Control>,
    capture: JoinHandle<io::Result<()>>,
    started: Instant,
    microphone: bool,
    /// The tracks whose loss the owner was told of.
    told: (bool, bool),
}

#[derive(Default)]
struct Inner {
    active: Option<Active>,
    /// The call going on was stopped: not recorded again until it ends.
    skip: bool,
    /// "Nie nagrywam tej rozmowy" was shown for the call going on.
    told: bool,
    /// Why the call going on is not recorded.
    reason: Option<&'static str>,
    /// Detection works.
    works: bool,
    /// `skip.json` was looked at with the first read.
    checked: bool,
    /// The devices whose capture could not be opened in the call going on: no new recording
    /// starts while Teams stays on them (4.2).
    lost: Option<Targets>,
}

pub struct Recorder {
    recordings: Recordings,
    local: PathBuf,
    queue: Arc<Queue>,
    ui_ready: Arc<AtomicBool>,
    hooks: Hooks,
    inner: Mutex<Inner>,
    /// Shows in the tray whether a recording is made; set once the tray is up.
    indicator: Mutex<Option<Indicator>>,
    /// What the update sees of the recording, and the recording of the update (4.8).
    link: Arc<UpdateLink>,
}

impl Recorder {
    pub fn new(local: &Path, queue: Arc<Queue>, ui_ready: Arc<AtomicBool>, hooks: Hooks) -> Recorder {
        Recorder {
            recordings: Recordings::new(local),
            local: local.to_path_buf(),
            queue,
            ui_ready,
            hooks,
            inner: Mutex::default(),
            indicator: Mutex::default(),
            link: Arc::default(),
        }
    }

    /// What an update watches (4.8).
    pub fn link(&self) -> Arc<UpdateLink> {
        Arc::clone(&self.link)
    }

    fn locked(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn recordings(&self) -> &Recordings {
        &self.recordings
    }

    pub fn queue(&self) -> &Arc<Queue> {
        &self.queue
    }

    /// The tray's indicator, shown at once as things stand.
    pub fn set_indicator(&self, show: Indicator) {
        show(self.locked().active.is_some());
        *self.indicator.lock().unwrap_or_else(PoisonError::into_inner) = Some(show);
    }

    fn indicate(&self, recording: bool) {
        if let Some(show) = &*self.indicator.lock().unwrap_or_else(PoisonError::into_inner) {
            show(recording);
        }
    }

    fn skip_file(&self) -> PathBuf {
        self.local.join("recordings").join(SKIP)
    }

    /// Keeps the call stopped across a restart; written through a temporary file (4.1).
    fn set_skip(&self, inner: &mut Inner, skip: bool) {
        inner.skip = skip;
        let path = self.skip_file();
        // ponytail: a skip that cannot be written lives in memory only; reported once a log sink exists.
        if skip {
            let temporary = path.with_extension("tmp");
            let _ = fs::write(&temporary, b"{}").and_then(|()| fs::rename(&temporary, &path));
        } else {
            let _ = fs::remove_file(path);
        }
    }

    /// Takes one read of the detection, at `now`.
    pub fn observe(&self, seen: &Observation, now: Instant) {
        let mut inner = self.locked();
        inner.works = seen.works;
        let listed = seen.microphone.is_some() || seen.speaker.is_some();
        if !inner.checked && seen.works {
            inner.checked = true;
            // A skip.json, even one that cannot be read, keeps the call going on stopped.
            if self.skip_file().exists() {
                self.set_skip(&mut inner, listed);
            }
        }
        // A skip that could not be written is tried again, so a restart keeps it.
        if inner.skip && !self.skip_file().exists() {
            self.set_skip(&mut inner, true);
        }
        // Only a working read tells that the call is over: one lost detection is not its end.
        if seen.works && (seen.ended || (!seen.call && !listed)) {
            if inner.skip {
                self.set_skip(&mut inner, false);
            }
            (inner.told, inner.reason, inner.lost) = (false, None, None);
        }
        if inner.active.is_some() {
            if let Some(reason) = self.end_reason(&mut inner, seen, now) {
                let active = inner.active.take();
                drop(inner);
                if let Some(active) = active {
                    // No track left open: the recording ends without skipping the call (4.2), and
                    // starts again once Teams moves to another device.
                    let lost = (reason == Some(Reason::CaptureFailed)).then(|| active.control.targets());
                    let reason = self.end(active, reason, false, None);
                    // Only while the call goes on: one that ended with this read leaves nothing to skip.
                    let going_on = seen.call && !seen.ended;
                    let mut inner = self.locked();
                    if going_on && lost.is_some() {
                        inner.lost = lost;
                    } else if going_on && matches!(reason, Some(Reason::TooLong | Reason::CaptureFailed | Reason::WriteFailed)) {
                        // Six hours, a capture that panicked or a write that failed end it as a
                        // stop does: the rest of the call is not recorded, no new recording every
                        // few seconds (4.2).
                        self.set_skip(&mut inner, true);
                    }
                }
            }
            return;
        }
        if !(seen.call && seen.active) || inner.skip || !(self.hooks.settings)().auto {
            return;
        }
        if let Err(why) = self.start(&mut inner, seen, now) {
            inner.reason = Some(why);
            if !inner.told {
                inner.told = true;
                self.hooks.notices.not_recording(why);
            }
        }
    }

    /// Why the recording going on ends now (`Some(None)`: the call ended), as the read `seen`
    /// shows; meanwhile it follows Teams' devices and tells of a track lost.
    fn end_reason(&self, inner: &mut Inner, seen: &Observation, now: Instant) -> Option<Option<Reason>> {
        let active = inner.active.as_mut()?;
        if !seen.works {
            return Some(Some(Reason::DetectFailed));
        }
        if seen.ended || !seen.call {
            return Some(None);
        }
        if now.duration_since(active.started) >= LONGEST {
            return Some(Some(Reason::TooLong));
        }
        if (self.hooks.free_disk)() < MIN_FREE_DISK {
            return Some(Some(Reason::LowDisk));
        }
        if active.capture.is_finished() {
            // Ended by itself: a write failed or the capture panicked; `end` tells which.
            return Some(None);
        }
        let targets = targets(seen, active.microphone);
        let (others, microphone) = active.control.dead(&targets);
        *active.control.targets.lock().unwrap_or_else(PoisonError::into_inner) = targets;
        if others && (!active.microphone || microphone) {
            return Some(Some(Reason::CaptureFailed));
        }
        for (lost, told, is_microphone) in [(others, &mut active.told.0, false), (microphone, &mut active.told.1, true)] {
            if lost && !*told {
                *told = true;
                self.hooks.notices.track_lost(is_microphone);
            }
        }
        None
    }

    fn start(&self, inner: &mut Inner, seen: &Observation, now: Instant) -> Result<(), &'static str> {
        let microphone = (self.hooks.settings)().microphone;
        // Teams moved to no other device since the capture could not open these in this call.
        if inner.lost.as_ref() == Some(&targets(seen, microphone)) {
            return Err("cannot open the call's devices");
        }
        if !self.ui_ready.load(Ordering::SeqCst) {
            return Err("no tray icon");
        }
        if (self.hooks.model)() != "ready" {
            return Err("no model");
        }
        if let Some(refusal) = self.recordings.refusal((self.hooks.free_disk)()) {
            return Err(match refusal {
                Refusal::DiskFull => "low disk",
                Refusal::AudioLimit => "audio limit",
                Refusal::TranscriptLimit => "text limit",
            });
        }
        // The daemon is about to end for an update; the rest of the call is recorded after it (4.1).
        if !self.link.start_recording() {
            return Err("update in progress");
        }
        let id = match self.recordings.create(millis(SystemTime::now())) {
            Ok(id) => id,
            Err(_) => {
                self.link.end_recording();
                return Err("cannot create the recording");
            }
        };
        let started = self.recordings.path(&id).ok_or(io::ErrorKind::NotFound).map_err(io::Error::from).and_then(|path| {
            let others = Track::create(&path.join("others.wav"))?;
            let me = if microphone { Some(Track::create(&path.join("me.wav"))?) } else { None };
            let control = Arc::new(Control::default());
            *control.targets.lock().unwrap_or_else(PoisonError::into_inner) = targets(seen, microphone);
            Ok(((self.hooks.capture)(others, me, Arc::clone(&control))?, control))
        });
        let (capture, control) = match started {
            Ok(started) => started,
            Err(_) => {
                let _ = self.recordings.delete(&id);
                self.link.end_recording();
                return Err("cannot create the recording");
            }
        };
        self.queue.recording(Some(&id));
        self.hooks.notices.started(&id, microphone);
        self.indicate(true);
        inner.reason = None;
        inner.active = Some(Active { id, control, capture, started: now, microphone, told: (false, false) });
        Ok(())
    }

    /// Ends `active`: the capture stops, the recording waits for its transcription (or goes,
    /// `delete`), the toasts go and the tray shows it. The reason it ended with.
    fn end(&self, active: Active, reason: Option<Reason>, delete: bool, by: Option<By>) -> Option<Reason> {
        active.control.stop.store(true, Ordering::SeqCst);
        let reason = match active.capture.join() {
            Ok(Ok(())) => reason,
            Ok(Err(_)) => reason.or(Some(Reason::WriteFailed)),
            Err(_) => reason.or(Some(Reason::CaptureFailed)),
        };
        // ponytail: a meta.json that cannot be written leaves `recording`, which the next start
        // repairs as interrupted.
        if let Ok(meta) = self.recordings.meta(&active.id) {
            let _ = self.recordings.set_meta(&active.id, &Meta { state: State::Waiting, end_ms: Some(millis(SystemTime::now())), reason, ..meta });
        }
        self.link.end_recording();
        self.queue.recording(None);
        if delete {
            let _ = self.queue.delete(&self.recordings, &active.id);
        } else {
            self.queue.wake();
        }
        self.hooks.notices.clear();
        self.indicate(false);
        if by == Some(By::Agent) {
            self.hooks.notices.agent_stopped();
        }
        reason
    }

    /// Stops the recording going on, if any (only `id`'s, when given: a toast left in the action
    /// centre), and keeps the call stopped; `delete` deletes it too. The recording's id.
    pub fn stop(&self, by: By, delete: bool, id: Option<&str>) -> Option<String> {
        let mut inner = self.locked();
        if id.is_some_and(|id| inner.active.as_ref().is_none_or(|active| active.id != id)) {
            return None;
        }
        let active = inner.active.take()?;
        self.set_skip(&mut inner, true);
        drop(inner);
        let stopped = active.id.clone();
        let _ = self.end(active, None, delete, Some(by));
        Some(stopped)
    }

    /// The automatic mode was turned off: the recording going on ends, the call is not skipped.
    pub fn settings_changed(&self) {
        if (self.hooks.settings)().auto {
            return;
        }
        let active = self.locked().active.take();
        if let Some(active) = active {
            let _ = self.end(active, None, false, None);
        }
    }

    /// Deletes a recording from the window: the one going on stops first (and its call is not
    /// recorded again, as with Zatrzymaj i usuń). A folder that cannot go is an error.
    pub fn delete(&self, id: &str) -> io::Result<()> {
        let _ = self.stop(By::Owner, false, Some(id));
        self.queue.delete(&self.recordings, id)
    }

    /// Runs `work` (the model's removal) while nothing is recorded or transcribed, and holds new
    /// recordings back meanwhile (4.6); refused otherwise. A transcription that starts during it
    /// finds the model gone and waits.
    pub fn without_work<T>(&self, work: impl FnOnce() -> T) -> Result<T, &'static str> {
        let inner = self.locked();
        if inner.active.is_some() || self.queue.view().running.is_some() {
            return Err(i18n::pick("trwa nagrywanie albo transkrypcja", "a call is being recorded or transcribed"));
        }
        Ok(work())
    }

    #[cfg(test)]
    fn stop_capture_for_test(&self) {
        if let Some(active) = &self.locked().active {
            active.control.stop.store(true, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    fn capture_finished_for_test(&self) -> bool {
        self.locked().active.as_ref().is_none_or(|active| active.capture.is_finished())
    }

    /// The recording going on, if any.
    pub fn active(&self) -> Option<String> {
        self.locked().active.as_ref().map(|active| active.id.clone())
    }

    /// The `recording` object of `status` (4.5).
    pub fn status(&self) -> Value {
        let (active, works, reason) = {
            let inner = self.locked();
            (inner.active.as_ref().map(|active| active.id.clone()), inner.works, inner.reason)
        };
        let queue = self.queue.view();
        let waiting = self.recordings.list().iter().filter(|entry| entry.meta.state == State::Waiting).count();
        let model = if queue.reason == Some("model damaged") { "damaged" } else { (self.hooks.model)() };
        json!({
            "auto": (self.hooks.settings)().auto,
            "active": active,
            "queue": waiting,
            "model": model,
            "detect": if works { "ok" } else { "unavailable" },
            "reason": reason.or(queue.reason),
        })
    }
}

/// The devices of Teams' call that `seen` shows, the microphone only when it is recorded.
fn targets(seen: &Observation, microphone: bool) -> Targets {
    Targets { speaker: seen.speaker.clone(), microphone: seen.microphone.clone().filter(|_| microphone) }
}

/// Free bytes on the disk that holds `path`; unknown counts as enough.
pub fn free_disk(path: &Path) -> u64 {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let holding = disks.list().iter().filter(|disk| path.starts_with(disk.mount_point()));
    holding.max_by_key(|disk| disk.mount_point().as_os_str().len()).map_or(u64::MAX, sysinfo::Disk::available_space)
}

/// The recorder of this computer: Teams' audio sessions, WASAPI and `toasts`. Starts the
/// detection, which reads the sessions every second.
#[cfg(windows)]
pub fn start(
    local: &Path,
    queue: Arc<Queue>,
    ui_ready: Arc<AtomicBool>,
    settings: Box<dyn Fn() -> Settings + Send + Sync>,
    model: Box<dyn Fn() -> &'static str + Send + Sync>,
    toasts: Arc<super::notice::Toasts>,
) -> io::Result<Arc<Recorder>> {
    let free = local.to_path_buf();
    let hooks = Hooks {
        settings,
        free_disk: Box::new(move || free_disk(&free)),
        model,
        capture: Box::new(capture),
        notices: Arc::clone(&toasts) as Arc<dyn Notices>,
    };
    let recorder = Arc::new(Recorder::new(local, queue, ui_ready, hooks));
    toasts.attach(&recorder);
    let watched = Arc::clone(&recorder);
    std::thread::Builder::new().name("call detection".into()).spawn(move || {
        // Without COM every read fails, and `status` shows the detection unavailable.
        let _ = wasapi::initialize_mta();
        let mut detector = super::detect::Detector::default();
        loop {
            let now = Instant::now();
            let seen = detector.read(now, super::detect::sessions()).clone();
            watched.observe(&seen, now);
            std::thread::sleep(Duration::from_secs(1));
        }
    })?;
    Ok(recorder)
}

/// The capture of one recording through WASAPI, on a thread of its own with COM.
#[cfg(windows)]
fn capture(others: Track, me: Option<Track>, control: Arc<Control>) -> io::Result<JoinHandle<io::Result<()>>> {
    std::thread::Builder::new().name("call capture".into()).spawn(move || {
        let _ = wasapi::initialize_mta();
        // ponytail: devices that cannot be listed at all end the recording as "write failed".
        let mut audio = super::capture::Wasapi::new().map_err(io::Error::other)?;
        let started = Instant::now();
        super::capture::run(&mut audio, others, me, &control, &|| started.elapsed(), &mut || std::thread::sleep(Duration::from_millis(100)))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU8;

    use super::*;
    use crate::recording::Transcript;

    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);

    impl Told {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl Notices for Told {
        fn started(&self, _: &str, microphone: bool) {
            self.0.lock().unwrap().push(format!("started {microphone}"));
        }
        fn not_recording(&self, why: &str) {
            self.0.lock().unwrap().push(format!("not recording: {why}"));
        }
        fn track_lost(&self, microphone: bool) {
            self.0.lock().unwrap().push(format!("lost {microphone}"));
        }
        fn agent_stopped(&self) {
            self.0.lock().unwrap().push("agent stopped".into());
        }
        fn clear(&self) {
            self.0.lock().unwrap().push("clear".into());
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        recorder: Recorder,
        told: Arc<Told>,
        auto: Arc<AtomicBool>,
        model: Arc<AtomicBool>,
        /// Makes the capture thread panic once stopped.
        panics: Arc<AtomicBool>,
        /// Makes the capture find no device it can open.
        no_device: Arc<AtomicBool>,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        // A transcription that never ends, so the recordings stay as the recorder left them.
        let engine = Arc::new(|_: &Path, _: Option<&str>, cancel: &AtomicBool, _: &AtomicU8| -> Result<Transcript, _> {
            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(super::super::stt::SttError::Cancelled)
        });
        let queue = Arc::new(Queue::start(dir.path(), engine, Arc::new(|| Err("held in tests")), Arc::new(|| None)).unwrap());
        let (told, auto, model, panics) =
            (Arc::new(Told::default()), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
        let no_device = Arc::new(AtomicBool::new(false));
        let hooks = Hooks {
            settings: {
                let auto = Arc::clone(&auto);
                Box::new(move || Settings { auto: auto.load(Ordering::SeqCst), microphone: true })
            },
            free_disk: Box::new(|| u64::MAX),
            model: {
                let model = Arc::clone(&model);
                Box::new(move || if model.load(Ordering::SeqCst) { "ready" } else { "missing" })
            },
            capture: {
                let (panics, no_device) = (Arc::clone(&panics), Arc::clone(&no_device));
                Box::new(move |others, me, control| {
                    let panics = Arc::clone(&panics);
                    if no_device.load(Ordering::SeqCst) {
                        *control.dead.lock().unwrap() = ((true, true), control.targets());
                    }
                    std::thread::Builder::new().spawn(move || {
                        while !control.stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        assert!(!panics.load(Ordering::SeqCst), "capture failed");
                        others.finish(Duration::from_millis(100))?;
                        me.map_or(Ok(()), |me| me.finish(Duration::from_millis(100)))
                    })
                })
            },
            notices: Arc::clone(&told) as Arc<dyn Notices>,
        };
        let recorder = Recorder::new(dir.path(), queue, Arc::new(AtomicBool::new(true)), hooks);
        Setup { _dir: dir, recorder, told, auto, model, panics, no_device }
    }

    fn on(active: bool) -> Observation {
        Observation {
            works: true,
            call: true,
            active,
            ended: false,
            microphone: Some("mic".into()),
            microphone_on: active,
            speaker: Some("headset".into()),
        }
    }

    fn ended() -> Observation {
        Observation { works: true, ended: true, ..Observation::default() }
    }

    fn none() -> Observation {
        Observation { works: true, ..Observation::default() }
    }

    fn state(setup: &Setup, id: &str) -> Meta {
        setup.recorder.recordings.meta(id).unwrap()
    }

    #[test]
    fn a_call_is_recorded_from_start_to_end() {
        let setup = setup();
        let now = Instant::now();
        setup.recorder.observe(&none(), now);
        assert!(setup.recorder.active().is_none());
        setup.recorder.observe(&on(true), now);
        let id = setup.recorder.active().unwrap();
        assert_eq!(state(&setup, &id).state, State::Recording);
        assert_eq!(setup.told.take(), ["started true"]);
        // Muted: the recording goes on.
        setup.recorder.observe(&on(false), now);
        assert_eq!(setup.recorder.active().as_deref(), Some(id.as_str()));
        setup.recorder.observe(&ended(), now);
        assert!(setup.recorder.active().is_none());
        let meta = state(&setup, &id);
        assert!(meta.state == State::Waiting && meta.end_ms.is_some() && meta.reason.is_none());
        assert_eq!(setup.told.take(), ["clear"]);
        assert!(setup.recorder.recordings.has_audio(&id));
    }

    #[test]
    fn nothing_is_recorded_with_the_mode_off_and_it_starts_when_turned_on() {
        let setup = setup();
        setup.auto.store(false, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none() && setup.told.take().is_empty());
        setup.auto.store(true, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_some());
        // Turned off during the recording: it ends, the call is not skipped.
        setup.auto.store(false, Ordering::SeqCst);
        setup.recorder.settings_changed();
        assert!(setup.recorder.active().is_none());
        setup.auto.store(true, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_some());
    }

    #[test]
    fn a_stopped_call_stays_stopped_until_it_ends_also_across_a_restart() {
        let setup = setup();
        setup.recorder.observe(&on(true), Instant::now());
        let id = setup.recorder.stop(By::Owner, false, None).unwrap();
        assert_eq!(state(&setup, &id).state, State::Waiting);
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none());
        assert!(setup.recorder.skip_file().exists());

        // The daemon restarts during the call: still stopped.
        let restarted =
            Recorder::new(&setup.recorder.local, Arc::clone(&setup.recorder.queue), Arc::new(AtomicBool::new(true)), setup_hooks_like(&setup));
        restarted.observe(&on(true), Instant::now());
        assert!(restarted.active().is_none());
        // The call ends: the next one is recorded.
        restarted.observe(&ended(), Instant::now());
        assert!(!restarted.skip_file().exists());
        restarted.observe(&on(true), Instant::now());
        assert!(restarted.active().is_some());
        restarted.stop(By::Owner, true, None);
    }

    /// Hooks like `setup`'s, for a second recorder over the same folder.
    fn setup_hooks_like(setup: &Setup) -> Hooks {
        let _ = setup;
        Hooks {
            settings: Box::new(|| Settings { auto: true, microphone: false }),
            free_disk: Box::new(|| u64::MAX),
            model: Box::new(|| "ready"),
            capture: Box::new(|others, _, control| {
                std::thread::Builder::new().spawn(move || {
                    while !control.stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    others.finish(Duration::ZERO)
                })
            }),
            notices: Arc::new(Told::default()),
        }
    }

    #[test]
    fn a_skip_left_from_a_call_long_over_is_dropped() {
        let setup = setup();
        fs::create_dir_all(setup.recorder.skip_file().parent().unwrap()).unwrap();
        fs::write(setup.recorder.skip_file(), b"not json").unwrap();
        setup.recorder.observe(&none(), Instant::now());
        assert!(!setup.recorder.skip_file().exists());
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_some());
    }

    #[test]
    fn a_call_that_cannot_be_recorded_is_told_once() {
        let setup = setup();
        setup.model.store(false, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        setup.recorder.observe(&on(true), Instant::now());
        assert_eq!(setup.told.take(), ["not recording: no model"]);
        assert_eq!(setup.recorder.status()["reason"], "no model");
        // The next call is told again.
        setup.recorder.observe(&ended(), Instant::now());
        setup.recorder.observe(&on(true), Instant::now());
        assert_eq!(setup.told.take(), ["not recording: no model"]);
    }

    #[test]
    fn an_agent_stop_and_a_delete_are_shown() {
        let setup = setup();
        setup.recorder.observe(&on(true), Instant::now());
        let id = setup.recorder.active().unwrap();
        setup.told.take();
        // A toast of another recording does nothing.
        assert!(setup.recorder.stop(By::Owner, true, Some("0123456789abcdef0123456789abcdef")).is_none());
        assert_eq!(setup.recorder.stop(By::Agent, false, None).as_deref(), Some(id.as_str()));
        assert_eq!(setup.told.take(), ["clear", "agent stopped"]);
        // Deleted from the window.
        setup.recorder.delete(&id).unwrap();
        assert!(setup.recorder.recordings.meta(&id).is_err());
    }

    #[test]
    fn a_capture_that_panics_ends_the_recording_with_its_reason() {
        let setup = setup();
        setup.recorder.observe(&on(true), Instant::now());
        let id = setup.recorder.active().unwrap();
        setup.panics.store(true, Ordering::SeqCst);
        setup.recorder.observe(&ended(), Instant::now());
        assert_eq!(state(&setup, &id).reason, Some(Reason::CaptureFailed));
    }

    #[test]
    fn a_capture_that_fails_during_a_call_does_not_start_again_in_it() {
        let setup = setup();
        setup.recorder.observe(&on(true), Instant::now());
        let id = setup.recorder.active().unwrap();
        setup.panics.store(true, Ordering::SeqCst);
        // The capture stops on its own: the next read sees it finished.
        setup.recorder.stop_capture_for_test();
        while !setup.recorder.capture_finished_for_test() {
            std::thread::sleep(Duration::from_millis(2));
        }
        setup.recorder.observe(&on(true), Instant::now());
        assert_eq!(state(&setup, &id).reason, Some(Reason::CaptureFailed));
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none());
        // The next call is recorded.
        setup.panics.store(false, Ordering::SeqCst);
        setup.recorder.observe(&ended(), Instant::now());
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_some());
    }

    #[test]
    fn a_device_that_does_not_open_ends_the_recording_without_skipping_the_call() {
        let setup = setup();
        setup.no_device.store(true, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        let id = setup.recorder.active().unwrap();
        setup.recorder.observe(&on(true), Instant::now());
        assert_eq!(state(&setup, &id).reason, Some(Reason::CaptureFailed));
        // Teams stays on that device: nothing new starts.
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none() && !setup.recorder.skip_file().exists());
        // Teams moves to another microphone: the call is recorded again.
        setup.recorder.observe(&Observation { microphone: Some("mic 2".into()), ..on(true) }, Instant::now());
        assert!(setup.recorder.active().is_some());
    }

    #[test]
    fn a_device_lost_before_teams_moved_does_not_block_the_new_one() {
        let setup = setup();
        setup.no_device.store(true, Ordering::SeqCst);
        setup.recorder.observe(&on(true), Instant::now());
        // Teams moves to other speakers before the capture tried them: nothing is lost yet.
        setup.recorder.observe(&Observation { speaker: Some("speakers".into()), ..on(true) }, Instant::now());
        assert!(setup.recorder.active().is_some());
    }

    #[test]
    fn a_stopped_call_stays_stopped_when_the_detection_comes_back() {
        let setup = setup();
        setup.recorder.observe(&on(true), Instant::now());
        setup.recorder.stop(By::Owner, false, None).unwrap();
        // A minute of failed reads, then the same call again.
        setup.recorder.observe(&Observation { ended: true, ..Observation::default() }, Instant::now());
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none());
    }

    #[test]
    fn an_update_sees_the_recording_and_holds_new_ones_at_the_handover() {
        let setup = setup();
        let link = setup.recorder.link();
        setup.recorder.observe(&on(true), Instant::now());
        assert!(link.recording());
        // While a call is recorded the daemon does not hand over.
        assert!(!link.start_handover());
        setup.recorder.observe(&ended(), Instant::now());
        assert!(!link.recording());
        setup.told.take();
        assert!(link.start_handover());
        setup.recorder.observe(&on(true), Instant::now());
        assert!(setup.recorder.active().is_none());
        assert_eq!(setup.told.take(), ["not recording: update in progress"]);
    }

    #[test]
    fn the_disk_of_a_path_is_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_disk(dir.path()) > 0);
    }

    #[test]
    fn detection_lost_or_six_hours_end_the_recording() {
        let setup = setup();
        let now = Instant::now();
        setup.recorder.observe(&on(true), now);
        let id = setup.recorder.active().unwrap();
        setup.recorder.observe(&Observation::default(), now);
        assert_eq!(state(&setup, &id).reason, Some(Reason::DetectFailed));
        assert_eq!(setup.recorder.status()["detect"], "unavailable");

        setup.recorder.observe(&on(true), now);
        let long = setup.recorder.active().unwrap();
        setup.recorder.observe(&on(true), now + LONGEST);
        assert_eq!(state(&setup, &long).reason, Some(Reason::TooLong));
        // As after a stop: the rest of that call is not recorded.
        setup.recorder.observe(&on(true), now + LONGEST);
        assert!(setup.recorder.active().is_none());
    }
}
