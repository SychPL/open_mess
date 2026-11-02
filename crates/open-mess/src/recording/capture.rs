//! Capturing a call (spec 2026-10-28-local-transcription, 4.2): the device Teams plays the call
//! on, by loopback, and the microphone Teams uses, each into its own 16 kHz mono track. A track
//! is written by the clock, not by the packets: what did not arrive is silence and what came past
//! the clock is cut, so both tracks keep the recording's length and their times agree. The
//! microphone is written only while Teams has it on; muted in Teams, its track gets silence and
//! what the microphone heard is dropped in memory.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::wav;

/// Samples per millisecond.
const PER_MS: u64 = wav::SAMPLE_RATE as u64 / 1000;
/// How far a track may run ahead of or behind the clock: packets come in bursts.
const SLACK: u64 = 200 * PER_MS;
/// Silence is written in pieces of this many samples.
const SILENCE: [i16; 1600] = [0; 1600];
/// A track that failed to open this many times in a row stays silent (at most 3 tries a minute).
const TRIES: u32 = 3;
const RETRY: Duration = Duration::from_secs(20);

/// One track of a recording, kept on the clock.
pub struct Track {
    writer: wav::Writer,
    written: u64,
}

impl Track {
    pub fn create(path: &Path) -> io::Result<Track> {
        Ok(Track { writer: wav::Writer::create(path)?, written: 0 })
    }

    fn due(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX).saturating_mul(PER_MS)
    }

    fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        self.writer.write(samples)?;
        self.written += samples.len() as u64;
        Ok(())
    }

    fn silence(&mut self, mut count: u64) -> io::Result<()> {
        while count > 0 {
            let piece = count.min(SILENCE.len() as u64);
            self.write(SILENCE.get(..piece as usize).unwrap_or_default())?;
            count -= piece;
        }
        Ok(())
    }

    /// Writes `samples`, which arrived by `elapsed` from the recording's start: up to the clock
    /// and [`SLACK`] past it, as silence when `open` is false. Then silence up to [`SLACK`] behind
    /// the clock, for what never came.
    pub fn feed(&mut self, elapsed: Duration, samples: &[i16], open: bool) -> io::Result<()> {
        let due = Self::due(elapsed);
        let room = (due + SLACK).saturating_sub(self.written);
        let take = samples.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        if open {
            self.write(samples.get(..take).unwrap_or_default())?;
        } else {
            self.silence(take as u64)?;
        }
        self.silence(due.saturating_sub(SLACK).saturating_sub(self.written))
    }

    /// Ends the track at `elapsed`, with silence up to it, and writes its header.
    pub fn finish(mut self, elapsed: Duration) -> io::Result<()> {
        self.silence(Self::due(elapsed).saturating_sub(self.written))?;
        self.writer.finish()
    }
}

/// Where the tracks come from: WASAPI on Windows ([`Wasapi`]), a fake in tests.
pub trait Audio {
    /// Opens the loopback of the render device `device`, or the capture device `device`.
    fn open(&mut self, device: &str, loopback: bool) -> Result<Box<dyn Source>, String>;
    /// Whether Teams' microphone session on `device` is active: Teams has the microphone on.
    fn microphone_on(&mut self, device: &str) -> bool;
}

/// One opened device.
pub trait Source {
    /// Appends the samples that arrived since the last read.
    fn read(&mut self, into: &mut Vec<i16>) -> Result<(), String>;
}

/// The devices to capture, set by the detection as Teams moves (4.1); `None` gives silence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Targets {
    pub speaker: Option<String>,
    pub microphone: Option<String>,
}

/// What the recording and its capture share.
#[derive(Default)]
pub struct Control {
    pub targets: Mutex<Targets>,
    pub stop: AtomicBool,
    /// Tracks that could not be opened and stay silent: the other side, the microphone.
    pub dead: Mutex<(bool, bool)>,
}

impl Control {
    pub fn targets(&self) -> Targets {
        self.targets.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn dead(&self) -> (bool, bool) {
        *self.dead.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One track's device as it is captured.
struct Feed {
    track: Track,
    loopback: bool,
    device: Option<String>,
    source: Option<Box<dyn Source>>,
    failures: u32,
    last_try: Option<Duration>,
    dead: bool,
}

impl Feed {
    fn new(track: Track, loopback: bool) -> Feed {
        Feed { track, loopback, device: None, source: None, failures: 0, last_try: None, dead: false }
    }

    /// Follows `wanted`, opening its device when it changed (not a failure) or after a failure
    /// (at most [`TRIES`] in a row, [`RETRY`] apart), and reads what came.
    fn read(&mut self, audio: &mut dyn Audio, wanted: Option<String>, now: Duration, into: &mut Vec<i16>) {
        if wanted != self.device {
            (self.device, self.source, self.failures, self.last_try) = (wanted, None, 0, None);
        }
        let Some(device) = self.device.clone() else { return };
        if self.source.is_none() && !self.dead && self.last_try.is_none_or(|at| now.saturating_sub(at) >= RETRY) {
            self.last_try = Some(now);
            match audio.open(&device, self.loopback) {
                Ok(source) => (self.source, self.failures) = (Some(source), 0),
                Err(_) => {
                    self.failures += 1;
                    self.dead = self.failures >= TRIES;
                }
            }
        }
        if let Some(source) = &mut self.source
            && source.read(into).is_err()
        {
            // Unplugged, or the device went away: opened again by the next reads.
            self.source = None;
            into.clear();
        }
    }
}

/// Captures into `others` (the loopback of the speaker) and `me` (the microphone, if recorded)
/// until `control.stop`, reading every time `pause` returns; `clock` is the time since the
/// recording started. A write error ends it as an error ("write failed", 4.2).
pub fn run(
    audio: &mut dyn Audio,
    others: Track,
    me: Option<Track>,
    control: &Control,
    clock: &dyn Fn() -> Duration,
    pause: &mut dyn FnMut(),
) -> io::Result<()> {
    let mut feeds = [Some(Feed::new(others, true)), me.map(|track| Feed::new(track, false))];
    let mut samples = Vec::new();
    while !control.stop.load(Ordering::SeqCst) {
        let targets = control.targets();
        let now = clock();
        for (feed, wanted) in feeds.iter_mut().zip([targets.speaker, targets.microphone]) {
            let Some(feed) = feed else { continue };
            samples.clear();
            feed.read(audio, wanted, now, &mut samples);
            let open = feed.loopback || feed.device.as_deref().is_some_and(|device| audio.microphone_on(device));
            feed.track.feed(now, &samples, open)?;
        }
        if let [Some(others), me] = &feeds {
            *control.dead.lock().unwrap_or_else(PoisonError::into_inner) = (others.dead, me.as_ref().is_some_and(|me| me.dead));
        }
        pause();
    }
    let end = clock();
    for feed in feeds.into_iter().flatten() {
        feed.track.finish(end)?;
    }
    Ok(())
}

/// The devices through WASAPI, in shared mode only: nothing about the devices or Teams changes.
/// Needs COM on the thread.
#[cfg(windows)]
pub struct Wasapi {
    enumerator: wasapi::DeviceEnumerator,
    /// Whether a process is Teams, by its id.
    teams: std::collections::HashMap<u32, bool>,
}

#[cfg(windows)]
impl Wasapi {
    pub fn new() -> Result<Wasapi, String> {
        Ok(Wasapi { enumerator: wasapi::DeviceEnumerator::new().map_err(|err| err.to_string())?, teams: Default::default() })
    }
}

#[cfg(windows)]
struct WasapiSource {
    // Kept for the stream's life: dropping it stops the stream.
    _client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    bytes: Vec<u8>,
}

#[cfg(windows)]
impl Audio for Wasapi {
    fn open(&mut self, device: &str, _loopback: bool) -> Result<Box<dyn Source>, String> {
        use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
        let text = |err: wasapi::WasapiError| err.to_string();
        let mut client = self.enumerator.get_device(device).and_then(|device| device.get_iaudioclient()).map_err(text)?;
        let format = WaveFormat::new(16, 16, &SampleType::Int, wav::SAMPLE_RATE as usize, 1, None);
        // On a render device a capture stream is its loopback (wasapi adds the flag). Windows
        // converts to 16 kHz mono; 200 ms of buffer for reads every 100 ms.
        client
            .initialize_client(&format, &Direction::Capture, &StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 2_000_000 })
            .map_err(text)?;
        let capture = client.get_audiocaptureclient().map_err(text)?;
        client.start_stream().map_err(text)?;
        Ok(Box::new(WasapiSource { _client: client, capture, bytes: vec![0; 64 * 1024] }))
    }

    fn microphone_on(&mut self, device: &str) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let Ok(list) = self
            .enumerator
            .get_device(device)
            .and_then(|device| device.get_iaudiosessionmanager())
            .and_then(|manager| manager.get_audiosessionenumerator())
        else {
            return false;
        };
        for index in 0..list.get_count().unwrap_or(0) {
            let Ok(session) = list.get_session(index) else { continue };
            let Ok(pid) = session.get_process_id() else { continue };
            let teams = *self.teams.entry(pid).or_insert_with(|| {
                let mut system = System::new();
                system.refresh_processes_specifics(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true, ProcessRefreshKind::nothing());
                system.process(Pid::from_u32(pid)).is_some_and(|process| process.name().eq_ignore_ascii_case(super::detect::PROCESS))
            });
            if teams && session.get_state().is_ok_and(|state| state == wasapi::SessionState::Active) {
                return true;
            }
        }
        false
    }
}

#[cfg(windows)]
impl Source for WasapiSource {
    fn read(&mut self, into: &mut Vec<i16>) -> Result<(), String> {
        let text = |err: wasapi::WasapiError| err.to_string();
        while let Some(frames) = self.capture.get_next_packet_size().map_err(text)?.filter(|frames| *frames > 0) {
            let bytes = (frames as usize * 2).min(self.bytes.len());
            let buffer = self.bytes.get_mut(..bytes).unwrap_or_default();
            let (read, info) = self.capture.read_from_device(buffer).map_err(text)?;
            let samples = buffer.get(..read as usize * 2).unwrap_or_default().as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair));
            // A packet marked silent holds no sound, whatever its bytes.
            if info.flags.silent { into.extend(samples.map(|_| 0)) } else { into.extend(samples) }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use super::*;

    fn read(path: &Path) -> Vec<i16> {
        let mut reader = wav::Reader::open(path).unwrap();
        let mut out = Vec::new();
        while reader.read(&mut out, 100_000).unwrap() > 0 {}
        out
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// Opens the loopback of this computer's default render device for a moment and reads it:
    /// `cargo test -- --ignored live_loopback`. Nothing is written anywhere.
    #[cfg(windows)]
    #[test]
    #[ignore = "opens an audio device of the computer it runs on"]
    fn live_loopback() {
        wasapi::initialize_mta().ok().unwrap();
        let device = wasapi::DeviceEnumerator::new().unwrap().get_default_device(&wasapi::Direction::Render).unwrap().get_id().unwrap();
        let mut source = Wasapi::new().unwrap().open(&device, true).unwrap();
        let mut samples = Vec::new();
        for _ in 0..5 {
            std::thread::sleep(ms(100));
            source.read(&mut samples).unwrap();
        }
    }

    #[test]
    fn a_track_keeps_to_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let mut track = Track::create(&path).unwrap();
        // Nothing for a second: silence up to 200 ms behind the clock.
        track.feed(ms(1000), &[], true).unwrap();
        assert_eq!(track.written, 800 * PER_MS);
        // A burst of 2 s at 1.1 s: cut at 200 ms ahead.
        track.feed(ms(1100), &[5; 32_000], true).unwrap();
        assert_eq!(track.written, 1300 * PER_MS);
        // The gate closed: silence instead of the samples.
        track.feed(ms(1400), &[7; 1600], false).unwrap();
        track.finish(ms(2000)).unwrap();
        let samples = read(&path);
        assert_eq!(samples.len() as u64, 2000 * PER_MS);
        assert!(samples.iter().all(|sample| *sample != 7));
        assert_eq!(samples.iter().filter(|sample| **sample == 5).count() as u64, 500 * PER_MS);
    }

    /// A fake device: every read brings 100 ms of `value`, or nothing while `quiet`.
    struct Fake {
        value: i16,
        quiet: Rc<Cell<bool>>,
    }

    impl Source for Fake {
        fn read(&mut self, into: &mut Vec<i16>) -> Result<(), String> {
            if !self.quiet.get() {
                into.extend([self.value; 1600]);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Devices {
        /// Devices that open, with the value they bring.
        values: HashMap<String, i16>,
        opened: Vec<String>,
        quiet: Rc<Cell<bool>>,
        microphone_on: Rc<Cell<bool>>,
    }

    impl Audio for Devices {
        fn open(&mut self, device: &str, _: bool) -> Result<Box<dyn Source>, String> {
            self.opened.push(device.into());
            let value = *self.values.get(device).ok_or("no such device")?;
            Ok(Box::new(Fake { value, quiet: Rc::clone(&self.quiet) }))
        }

        fn microphone_on(&mut self, _: &str) -> bool {
            self.microphone_on.get()
        }
    }

    /// Runs the capture for `ticks` reads of 100 ms, calling `at` before each.
    fn capture(audio: &mut Devices, me: bool, control: &Control, ticks: u64, mut at: impl FnMut(u64, &Control)) -> (Vec<i16>, Vec<i16>) {
        let dir = tempfile::tempdir().unwrap();
        let (others, mic) = (dir.path().join("others.wav"), dir.path().join("me.wav"));
        let tick = Rc::new(Cell::new(0u64));
        let clock = {
            let tick = Rc::clone(&tick);
            move || ms(tick.get() * 100)
        };
        at(0, control);
        let mut pause = || {
            tick.set(tick.get() + 1);
            at(tick.get(), control);
            if tick.get() == ticks {
                control.stop.store(true, Ordering::SeqCst);
            }
        };
        let me = me.then(|| Track::create(&mic).unwrap());
        run(audio, Track::create(&others).unwrap(), me, control, &clock, &mut pause).unwrap();
        (read(&others), if mic.exists() { read(&mic) } else { Vec::new() })
    }

    fn targets(speaker: &str, microphone: &str) -> Targets {
        Targets { speaker: Some(speaker.into()), microphone: Some(microphone.into()) }
    }

    #[test]
    fn the_microphone_follows_the_mute_in_teams() {
        let mut audio = Devices { values: HashMap::from([("headset".into(), 100), ("mic".into(), 200)]), ..Devices::default() };
        let on = Rc::clone(&audio.microphone_on);
        let control = Control::default();
        let (others, me) = capture(&mut audio, true, &control, 50, |tick, control| {
            *control.targets.lock().unwrap() = targets("headset", "mic");
            // Muted in Teams from 2 s to 3 s.
            on.set(!(20..30).contains(&tick));
        });
        assert_eq!((others.len(), me.len()), (80_000, 80_000));
        // The other side goes on through the mute; the microphone is silent in it.
        assert!(others.get(32_000..48_000).unwrap().iter().all(|sample| *sample == 100));
        assert!(me.get(35_200..44_800).unwrap().iter().all(|sample| *sample == 0));
        assert!(me.get(52_800..60_000).unwrap().iter().all(|sample| *sample == 200));
    }

    #[test]
    fn a_device_change_in_teams_moves_the_capture() {
        let mut audio = Devices { values: HashMap::from([("a".into(), 1), ("b".into(), 2)]), ..Devices::default() };
        let control = Control::default();
        let (others, _) = capture(&mut audio, false, &control, 40, |tick, control| {
            let speaker = if tick < 20 { "a" } else { "b" };
            control.targets.lock().unwrap().speaker = Some(speaker.into());
        });
        assert_eq!(audio.opened, ["a", "b"]);
        assert_eq!(others.len(), 64_000);
        assert_eq!(others.last(), Some(&2));
        assert_eq!(control.dead(), (false, false));
    }

    #[test]
    fn a_source_quiet_for_ten_minutes_keeps_its_length() {
        let mut audio = Devices { values: HashMap::from([("headset".into(), 100), ("mic".into(), 200)]), ..Devices::default() };
        let (quiet, on) = (Rc::clone(&audio.quiet), Rc::clone(&audio.microphone_on));
        on.set(true);
        let control = Control::default();
        // 12 minutes; the devices bring nothing from minute 1 to minute 11.
        let (others, me) = capture(&mut audio, true, &control, 7200, |tick, control| {
            *control.targets.lock().unwrap() = targets("headset", "mic");
            quiet.set((600..6600).contains(&tick));
        });
        assert_eq!((others.len(), me.len()), (11_520_000, 11_520_000));
        // After the gap the samples sit at their own time, not shifted by it.
        assert!(others.get(10_640_000..11_000_000).unwrap().iter().all(|sample| *sample == 100));
        assert!(others.get(1_000_000..10_000_000).unwrap().iter().all(|sample| *sample == 0));
    }

    #[test]
    fn a_device_that_does_not_open_leaves_its_track_silent() {
        let mut audio = Devices { values: HashMap::from([("mic".into(), 200)]), ..Devices::default() };
        audio.microphone_on.set(true);
        let control = Control::default();
        // 2 minutes: tries at 0 s, 20 s and 40 s, then no more.
        let (others, me) = capture(&mut audio, true, &control, 1200, |_, control| *control.targets.lock().unwrap() = targets("gone", "mic"));
        assert_eq!(audio.opened.iter().filter(|device| *device == "gone").count(), 3);
        assert_eq!(control.dead(), (true, false));
        assert!(others.iter().all(|sample| *sample == 0) && others.len() == me.len());
    }
}
