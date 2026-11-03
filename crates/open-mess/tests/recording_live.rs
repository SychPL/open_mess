//! The live check of the call recording (spec 2026-10-28-local-transcription, steps 0b and 7),
//! with the owner in a Teams call: the real detection, capture and toasts, in a temporary folder
//! instead of the daemon's, so a running daemon is not touched. It prints only, every second, the
//! Teams sessions' states and the two tracks' levels; no audio, text, name or device id. The
//! recording is deleted at the end, and a run killed before it is cleaned up by the next one.
//! Run it while the daemon does not record calls: the toasts share its app id, so the end of this
//! run clears the daemon's toasts too.
//!
//! `OPEN_MESS_LIVE_SECONDS=600 cargo test -p open-mess --test recording_live -- --ignored --nocapture`

#![cfg(windows)]
// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::time::{Duration, Instant};

use open_mess::recording::queue::Queue;
use open_mess::recording::recorder::{self, By, Settings};
use open_mess::recording::stt::SttError;
use open_mess::recording::{Transcript, detect, wav};

/// The folder under `%TEMP%` that holds this check's temporary folder.
const FOLDER: &str = "open_mess-live-call";

/// RMS of each second of a track.
fn levels(path: &Path) -> Vec<u32> {
    let Ok(mut reader) = wav::Reader::open(path) else { return Vec::new() };
    let mut out = Vec::new();
    let mut second = Vec::new();
    loop {
        second.clear();
        if reader.read(&mut second, wav::SAMPLE_RATE as usize).unwrap() == 0 {
            return out;
        }
        let energy: f64 = second.iter().map(|sample| f64::from(*sample).powi(2)).sum();
        out.push((energy / second.len() as f64).sqrt() as u32);
    }
}

#[test]
#[ignore = "records a real Teams call on the computer it runs on"]
#[allow(clippy::print_stdout)]
fn live_call() {
    let seconds: u64 = std::env::var("OPEN_MESS_LIVE_SECONDS").ok().and_then(|value| value.parse().ok()).unwrap_or(300);
    // What a killed run left goes with its folder, found by its exact name only.
    let parent = std::env::temp_dir().join(FOLDER);
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).unwrap();
    let local = tempfile::tempdir_in(&parent).unwrap();
    // Nothing is transcribed here: the queue is held.
    let engine = Arc::new(|_: &Path, _: Option<&str>, _: &AtomicBool, _: &AtomicU8| -> Result<Transcript, SttError> { Err(SttError::Cancelled) });
    let queue = Arc::new(Queue::start(local.path(), engine, Arc::new(|| Err("live check")), Arc::new(|| None)).unwrap());
    let settings = Box::new(|| Settings { auto: true, microphone: true });
    let recorder = recorder::start(local.path(), queue, Arc::new(AtomicBool::new(true)), settings, Box::new(|| "ready")).unwrap();

    wasapi::initialize_mta().ok().unwrap();
    let started = Instant::now();
    let mut states = Vec::new();
    println!("second | Teams capture active/listed | Teams render active/listed | recording");
    while started.elapsed() < Duration::from_secs(seconds) {
        let sessions = detect::sessions().unwrap_or_default();
        let count = |capture: bool, active: bool| sessions.iter().filter(|session| session.capture == capture && (session.active || !active)).count();
        let line = format!(
            "{}/{} | {}/{} | {}",
            count(true, true),
            count(true, false),
            count(false, true),
            count(false, false),
            recorder.active().is_some()
        );
        if states.last() != Some(&line) {
            println!("{:>5} | {line}", started.elapsed().as_secs());
            states.push(line);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let id = recorder.stop(By::Owner, false, None).or_else(|| recorder.recordings().list().first().map(|entry| entry.id.clone()));
    let Some(id) = id else {
        println!("nothing was recorded");
        return;
    };
    let folder = recorder.recordings().path(&id).unwrap();
    let meta = recorder.recordings().meta(&id).unwrap();
    println!("recording: {:?}, reason {:?}", meta.state, meta.reason);
    let (others, me) = (levels(&folder.join("others.wav")), levels(&folder.join("me.wav")));
    println!("second | others rms | me rms");
    for second in 0..others.len().max(me.len()) {
        println!("{second:>5} | {:>6} | {:>6}", others.get(second).copied().unwrap_or(0), me.get(second).copied().unwrap_or(0));
    }
    recorder.recordings().delete(&id).unwrap();
}
