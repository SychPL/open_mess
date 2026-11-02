//! Telling that a Teams call is on (spec 2026-10-28-local-transcription, 4.1), from the audio
//! sessions Windows lists for each device: what the Volume Mixer shows, read only. A call starts
//! when a session of `ms-teams.exe` is active, and lasts while Teams keeps any session listed:
//! muting stops its streams but keeps the sessions, and Teams removes them about 180 s after the
//! call. The logic here takes what was read and the time, so it is tested without Windows.

use std::time::{Duration, Instant};

/// The process whose sessions are a call (4.9). WebView2's sessions in the Teams tree are the
/// interface's sounds and do not count.
pub const PROCESS: &str = "ms-teams.exe";
/// No Teams session listed this long: the call has ended (a device change may drop them briefly).
const GONE: Duration = Duration::from_secs(15);
/// Reads failing this long: detection is unavailable and a recording ends.
const BROKEN: Duration = Duration::from_secs(60);
/// All Teams sessions inactive this long ends the call too, once step 0b confirmed that Teams
/// plays nothing then (4.1).
const QUIET: Duration = Duration::from_secs(30 * 60);
/// Step 0b showed that Teams plays the other side only with an active session: the end of a
/// recording is cut to the last active moment, and [`QUIET`] applies.
// ponytail: false until step 0b confirms it on a live call (4.1).
pub const CONFIRMED: bool = false;

/// A session of the Teams process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Capture (the microphone) or render (what Teams plays).
    pub capture: bool,
    /// The device's id.
    pub device: String,
    pub active: bool,
}

/// What a read tells the recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// Reads work, or failed for less than [`BROKEN`].
    pub works: bool,
    /// A call is on.
    pub call: bool,
    /// A Teams session is active now: a recording may start.
    pub active: bool,
    /// The call ended with this read.
    pub ended: bool,
    /// The device of Teams' microphone session (an active one first), and whether it is on.
    pub microphone: Option<String>,
    pub microphone_on: bool,
    /// The device Teams plays the call on: one with an active session first.
    pub speaker: Option<String>,
}

#[derive(Default)]
pub struct Detector {
    observation: Observation,
    gone_since: Option<Instant>,
    quiet_since: Option<Instant>,
    failing_since: Option<Instant>,
    /// The last read with an active Teams session.
    pub last_active: Option<Instant>,
}

impl Detector {
    /// Takes one read, the Teams sessions or the error, made at `now`.
    pub fn read(&mut self, now: Instant, read: Result<Vec<Session>, String>) -> &Observation {
        self.observation.ended = false;
        let sessions = match read {
            Ok(sessions) => {
                self.failing_since = None;
                sessions
            }
            Err(_) => {
                let since = *self.failing_since.get_or_insert(now);
                if now.duration_since(since) >= BROKEN {
                    let ended = self.observation.call;
                    self.observation = Observation { ended, ..Observation::default() };
                    self.gone_since = None;
                    self.quiet_since = None;
                } else {
                    // One failed read changes nothing.
                    self.observation.works = true;
                }
                return &self.observation;
            }
        };
        let active = sessions.iter().any(|session| session.active);
        let pick = |capture: bool| {
            let mine = || sessions.iter().filter(move |session| session.capture == capture);
            mine().find(|session| session.active).or_else(|| mine().next()).map(|session| session.device.clone())
        };
        let mut call = self.observation.call || active;
        let mut ended = false;
        if active {
            self.last_active = Some(now);
        }
        if sessions.is_empty() {
            let since = *self.gone_since.get_or_insert(now);
            ended |= call && now.duration_since(since) >= GONE;
        } else {
            self.gone_since = None;
        }
        if active {
            self.quiet_since = None;
        } else if call {
            let since = *self.quiet_since.get_or_insert(now);
            ended |= CONFIRMED && now.duration_since(since) >= QUIET;
        }
        if ended {
            call = false;
            self.gone_since = None;
            self.quiet_since = None;
        }
        self.observation = Observation {
            works: true,
            call,
            active: call && active,
            ended,
            microphone: pick(true),
            microphone_on: sessions.iter().any(|session| session.capture && session.active),
            speaker: pick(false),
        };
        &self.observation
    }
}

/// The Teams sessions of every active capture and render device. Needs COM on the thread.
#[cfg(windows)]
pub fn sessions() -> Result<Vec<Session>, String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use wasapi::{DeviceEnumerator, Direction, SessionState};

    let text = |err: wasapi::WasapiError| err.to_string();
    let enumerator = DeviceEnumerator::new().map_err(text)?;
    let mut found = Vec::new();
    for (direction, capture) in [(Direction::Capture, true), (Direction::Render, false)] {
        for device in &enumerator.get_device_collection(&direction).map_err(text)? {
            let device = device.map_err(text)?;
            let id = device.get_id().map_err(text)?;
            let list = device.get_iaudiosessionmanager().and_then(|manager| manager.get_audiosessionenumerator()).map_err(text)?;
            for index in 0..list.get_count().map_err(text)? {
                let session = list.get_session(index).map_err(text)?;
                let state = session.get_state().map_err(text)?;
                if state != SessionState::Expired {
                    found.push((
                        session.get_process_id().map_err(text)?,
                        Session { capture, device: id.clone(), active: state == SessionState::Active },
                    ));
                }
            }
        }
    }
    let pids: Vec<Pid> = found.iter().map(|(pid, _)| Pid::from_u32(*pid)).collect();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ProcessRefreshKind::nothing());
    let teams = |pid: u32| system.process(Pid::from_u32(pid)).is_some_and(|process| process.name().eq_ignore_ascii_case(PROCESS));
    Ok(found.into_iter().filter(|(pid, _)| teams(*pid)).map(|(_, session)| session).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(capture: bool, device: &str, active: bool) -> Session {
        Session { capture, device: device.into(), active }
    }

    /// A call as step 0 saw it: the microphone and two streams on the headset.
    fn call(active: bool) -> Result<Vec<Session>, String> {
        Ok(vec![session(true, "mic", active), session(false, "headset", active), session(false, "headset", active)])
    }

    #[test]
    fn a_call_starts_active_lasts_while_muted_and_ends_when_teams_drops_it() {
        let (mut detector, start) = (Detector::default(), Instant::now());
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        assert!(!detector.read(at(0), Ok(Vec::new())).call);
        let on = detector.read(at(1), call(true)).clone();
        assert!(on.call && on.active && on.microphone_on);
        assert_eq!((on.microphone.as_deref(), on.speaker.as_deref()), (Some("mic"), Some("headset")));
        // Muted: every stream stops, the sessions stay; the call goes on, the microphone is off.
        let muted = detector.read(at(30), call(false)).clone();
        assert!(muted.call && !muted.active && !muted.microphone_on && !muted.ended);
        assert_eq!(muted.microphone.as_deref(), Some("mic"));
        // Hung up: inactive for about 180 s, then gone; the end comes 15 s after that.
        assert!(detector.read(at(200), call(false)).call);
        assert!(detector.read(at(210), Ok(Vec::new())).call);
        let end = detector.read(at(225), Ok(Vec::new())).clone();
        assert!(end.ended && !end.call);
        assert!(!detector.read(at(226), Ok(Vec::new())).ended);
        assert_eq!(detector.last_active, Some(at(1)));
    }

    #[test]
    fn sessions_left_inactive_after_a_call_do_not_start_one() {
        // The daemon starts within the 180 s after a call: nothing is active, nothing starts.
        let (mut detector, start) = (Detector::default(), Instant::now());
        let seen = detector.read(start, call(false)).clone();
        assert!(!seen.call && !seen.active);
    }

    #[test]
    fn a_device_change_shorter_than_the_margin_keeps_the_call() {
        let (mut detector, start) = (Detector::default(), Instant::now());
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        detector.read(at(0), call(true));
        assert!(detector.read(at(5), Ok(Vec::new())).call);
        let moved = detector.read(at(10), Ok(vec![session(true, "usb", true), session(false, "speakers", true)])).clone();
        assert!(moved.call && !moved.ended);
        assert_eq!((moved.microphone.as_deref(), moved.speaker.as_deref()), (Some("usb"), Some("speakers")));
        assert!(!detector.read(at(30), Ok(Vec::new())).ended);
    }

    #[test]
    fn the_active_device_wins_over_one_left_listed() {
        let (mut detector, start) = (Detector::default(), Instant::now());
        let seen = detector.read(start, Ok(vec![session(false, "old", false), session(false, "new", true)])).clone();
        assert_eq!(seen.speaker.as_deref(), Some("new"));
        assert_eq!(seen.microphone, None);
    }

    /// Reads this computer's sessions: `cargo test -- --ignored live_sessions`. Only reads.
    #[cfg(windows)]
    #[test]
    #[ignore = "reads the audio devices of the computer it runs on"]
    fn live_sessions() {
        wasapi::initialize_mta().ok().unwrap();
        // Outside a call Teams has none; during one, the microphone and the headset.
        let found = sessions().unwrap();
        assert!(found.iter().all(|session| !session.device.is_empty()));
    }

    #[test]
    fn reads_failing_for_a_minute_end_the_call() {
        let (mut detector, start) = (Detector::default(), Instant::now());
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        detector.read(at(0), call(true));
        // One failed read changes nothing.
        let once = detector.read(at(1), Err("COM".into())).clone();
        assert!(once.works && once.call && !once.ended);
        assert!(detector.read(at(30), Err("COM".into())).call);
        let broken = detector.read(at(61), Err("COM".into())).clone();
        assert!(!broken.works && !broken.call && broken.ended);
        assert!(detector.read(at(62), call(true)).works);
    }
}
