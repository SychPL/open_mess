//! The tools of the local recordings (spec 2026-10-28-local-transcription, 4.5): the list of
//! recordings and one recording's transcript, under the read permission of the reserved account
//! `local`. No tool starts a recording, changes a setting or deletes one: only the configuration
//! window does (spec 15: the CLI is not a trusted channel).

use chrono::{DateTime, SecondsFormat};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolError, ToolInfo, failed, shared};
use crate::recording::{Entry, Recordings, State, valid_id};

pub const LIST: &str = "list_recordings";
pub const TRANSCRIPT: &str = "get_recording_transcript";
pub const STOP: &str = "stop_recording";
/// The most cues of one page, and the bounds of a page's size.
const MAX_PAGE_CUES: usize = 2000;
const MAX_BYTES: (u64, u64, u64) = (4096, 786_432, 131_072);

pub fn tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: LIST.into(),
            description: "Calls recorded on this computer and transcribed locally, newest first, with their state. Their \
                          transcripts come from get_recording_transcript."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
                    "since": {"type": "string", "description": "Only recordings that started after this time (RFC 3339)."},
                    "before": {"type": "string", "description": "The last recording of the previous page: the next page, older recordings."},
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: TRANSCRIPT.into(),
            description: "The local transcript of one recording, a page at a time: pieces of speech with times from the \
                          recording's start and the side that spoke (me: the user's microphone, others: what the call played)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "recording": {"type": "string", "description": "Recording id from list_recordings."},
                    "cursor": {"type": "integer", "minimum": 0, "description": "next_cursor of the previous page."},
                    "max_bytes": {"type": "integer", "minimum": MAX_BYTES.0, "maximum": MAX_BYTES.1, "default": MAX_BYTES.2},
                },
                "required": ["recording"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: STOP.into(),
            description: "Stops the recording of the call going on, if one is made; that call is then not recorded again. \
                          The owner is shown that an agent stopped it. Starting a recording is up to the owner."
                .into(),
            input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        },
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    limit: Option<u32>,
    since: Option<String>,
    before: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptArgs {
    recording: String,
    cursor: Option<u64>,
    max_bytes: Option<u64>,
}

fn args<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs(err.to_string()))
}

fn time(ms: u64) -> Option<String> {
    DateTime::from_timestamp_millis(i64::try_from(ms).ok()?).map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// `HH:MM:SS.mmm` from a recording's start.
fn offset(ms: u64) -> String {
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// A language code as the model's tokenizer names it; anything else is left out.
fn language(code: &Option<String>) -> Option<&str> {
    code.as_deref().filter(|code| (2..=8).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase()))
}

fn not_allowed() -> ToolError {
    ToolError::NotAllowed("reading the local recordings is not allowed".into())
}

/// Runs `name` (one of [`LIST`] and [`TRANSCRIPT`]); `readable` says whether an agent may read
/// the recordings now. It is asked before the files are read and again before the answer goes
/// out, so a permission taken back meanwhile lets nothing through (4.5). `progress` is the
/// transcription's progress of a recording, while it runs.
pub fn call(
    recordings: &Recordings,
    progress: &dyn Fn(&str) -> Option<u8>,
    readable: impl Fn() -> Result<bool, ToolError>,
    name: &str,
    arguments: Value,
) -> Result<Value, ToolError> {
    if !arguments.is_object() {
        return Err(ToolError::InvalidArgs("arguments must be a JSON object".into()));
    }
    if !readable()? {
        return Err(not_allowed());
    }
    let result = match name {
        LIST => list(recordings, progress, args(arguments)?)?,
        TRANSCRIPT => transcript(recordings, args(arguments)?)?,
        _ => return Err(ToolError::UnknownTool(name.into())),
    };
    if !readable()? {
        return Err(not_allowed());
    }
    Ok(shared::with_notice(result))
}

fn list(recordings: &Recordings, progress: &dyn Fn(&str) -> Option<u8>, args: ListArgs) -> Result<Value, ToolError> {
    let limit = args.limit.unwrap_or(50);
    if !(1..=500).contains(&limit) {
        return Err(ToolError::InvalidArgs("limit must be from 1 to 500".into()));
    }
    let since = match args.since {
        Some(since) => Some(
            DateTime::parse_from_rfc3339(&since).map_err(|_| ToolError::InvalidArgs("since must be an RFC 3339 time".into()))?.timestamp_millis(),
        ),
        None => None,
    };
    let entries = recordings.list();
    let first = match args.before {
        Some(before) => entries
            .iter()
            .position(|Entry { id, .. }| *id == before)
            .map(|at| at + 1)
            .ok_or_else(|| ToolError::InvalidArgs("unknown recording in before - list_recordings lists them".into()))?,
        None => 0,
    };
    let items: Vec<Value> = entries
        .into_iter()
        .skip(first)
        .filter(|Entry { meta, .. }| since.is_none_or(|since| i64::try_from(meta.start_ms).is_ok_and(|start| start >= since)))
        .take(limit as usize)
        .map(|Entry { id, meta }| {
            let percent = (meta.state == State::Transcribing).then(|| progress(&id)).flatten();
            json!({
                "recording": id,
                "start": time(meta.start_ms),
                "end": meta.end_ms.and_then(time),
                "state": meta.state,
                "progress": percent,
                "language": language(&meta.language),
                "reason": meta.reason,
            })
        })
        .collect();
    Ok(json!({"items": items}))
}

fn transcript(recordings: &Recordings, args: TranscriptArgs) -> Result<Value, ToolError> {
    let unknown = || ToolError::InvalidArgs("unknown recording - list_recordings lists them".into());
    if !valid_id(&args.recording) {
        return Err(unknown());
    }
    let max_bytes = args.max_bytes.unwrap_or(MAX_BYTES.2);
    if !(MAX_BYTES.0..=MAX_BYTES.1).contains(&max_bytes) {
        return Err(ToolError::InvalidArgs(format!("max_bytes must be from {} to {}", MAX_BYTES.0, MAX_BYTES.1)));
    }
    let meta = recordings.meta(&args.recording).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => unknown(),
        _ => ToolError::Failed("this recording is damaged".into()),
    })?;
    if meta.state != State::Done {
        return Err(ToolError::Failed(format!("transcript not ready: {}", meta.state.name())));
    }
    let transcript = recordings.transcript(&args.recording).map_err(|_| ToolError::Failed("the transcript is damaged".into()))?;
    let first = usize::try_from(args.cursor.unwrap_or(0)).map_err(|_| ToolError::InvalidArgs("cursor out of range".into()))?;
    if first > transcript.cues.len() {
        return Err(ToolError::InvalidArgs("cursor out of range".into()));
    }
    let (mut cues, mut used) = (Vec::new(), 0u64);
    for cue in transcript.cues.iter().skip(first).take(MAX_PAGE_CUES) {
        let cue = json!({"start": offset(cue.start_ms), "end": offset(cue.end_ms), "speaker": cue.speaker, "text": cue.text});
        let size = serde_json::to_vec(&cue).map_err(failed)?.len() as u64;
        if !cues.is_empty() && used + size > max_bytes {
            break;
        }
        used += size;
        cues.push(cue);
    }
    let next = first + cues.len();
    Ok(json!({
        "recording": args.recording,
        "start": time(meta.start_ms),
        "end": meta.end_ms.and_then(time),
        "language": language(&meta.language),
        "next_cursor": (next < transcript.cues.len()).then_some(next),
        "untrusted": {"cues": cues},
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::recording::{Cue, Meta, Speaker, Transcript};

    fn done(recordings: &Recordings, cues: usize) -> String {
        let id = recordings.create(1_700_000_000_000).unwrap();
        let cues = (0..cues).map(|i| Cue {
            start_ms: i as u64 * 1000,
            end_ms: i as u64 * 1000 + 900,
            speaker: Speaker::Others,
            text: format!("zdanie {i}"),
        });
        recordings.finish_transcript(&id, &Transcript { language: Some("pl".into()), cues: cues.collect() }).unwrap();
        let meta = recordings.meta(&id).unwrap();
        recordings.set_meta(&id, &Meta { end_ms: Some(1_700_000_060_000), ..meta }).unwrap();
        id
    }

    fn yes() -> Result<bool, ToolError> {
        Ok(true)
    }

    #[test]
    fn a_transcript_comes_in_pages_marked_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = done(&recordings, 3000);
        // A small default page; the largest one takes the most cues a page has.
        let small = call(&recordings, &|_| None, yes, TRANSCRIPT, json!({"recording": id})).unwrap();
        let small = small["untrusted"]["cues"].as_array().unwrap();
        assert!(small.len() < MAX_PAGE_CUES && serde_json::to_vec(small).unwrap().len() as u64 <= MAX_BYTES.2 + small.len() as u64);
        let page = call(&recordings, &|_| None, yes, TRANSCRIPT, json!({"recording": id, "max_bytes": MAX_BYTES.1})).unwrap();
        assert_eq!(page["language"], "pl");
        assert_eq!(page["start"], "2023-11-14T22:13:20.000Z");
        let cues = page["untrusted"]["cues"].as_array().unwrap();
        assert_eq!(cues.len(), MAX_PAGE_CUES);
        assert_eq!(cues[1], json!({"start": "00:00:01.000", "end": "00:00:01.900", "speaker": "others", "text": "zdanie 1"}));
        assert!(page["untrusted_notice"].is_string());
        assert_eq!(page["next_cursor"], 2000);
        let last = call(&recordings, &|_| None, yes, TRANSCRIPT, json!({"recording": id, "cursor": 2000, "max_bytes": 4096})).unwrap();
        let cues = last["untrusted"]["cues"].as_array().unwrap();
        assert!(!cues.is_empty() && serde_json::to_vec(cues).unwrap().len() <= 4096 + 100);
        assert_eq!(last["next_cursor"], 2000 + cues.len());
        let end = call(&recordings, &|_| None, yes, TRANSCRIPT, json!({"recording": id, "cursor": 3000})).unwrap();
        assert_eq!((end["untrusted"]["cues"].as_array().unwrap().len(), &end["next_cursor"]), (0, &Value::Null));
    }

    #[test]
    fn bad_requests_are_refused_without_touching_files() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let waiting = recordings.create(0).unwrap();
        recordings.set_meta(&waiting, &Meta { state: State::Waiting, ..recordings.meta(&waiting).unwrap() }).unwrap();
        let id = done(&recordings, 1);
        for args in [
            json!({"recording": "..\\..\\x"}),
            json!({"recording": "0123456789abcdef0123456789abcdef"}),
            json!({"recording": id, "cursor": 5}),
            json!({"recording": id, "max_bytes": 10}),
            json!({"recording": id, "extra": 1}),
            json!([id]),
        ] {
            assert!(matches!(call(&recordings, &|_| None, yes, TRANSCRIPT, args.clone()), Err(ToolError::InvalidArgs(_))), "{args}");
        }
        let err = call(&recordings, &|_| None, yes, TRANSCRIPT, json!({"recording": waiting})).unwrap_err();
        assert_eq!(err.to_string(), "transcript not ready: waiting");
        assert!(matches!(call(&recordings, &|_| None, yes, LIST, json!({"since": "yesterday"})), Err(ToolError::InvalidArgs(_))));
        assert!(matches!(call(&recordings, &|_| None, yes, LIST, json!({"limit": 0})), Err(ToolError::InvalidArgs(_))));
    }

    #[test]
    fn the_list_is_newest_first_from_since() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let old = recordings.create(1_000).unwrap();
        let new = done(&recordings, 1);
        let all = call(&recordings, &|_| None, yes, LIST, json!({})).unwrap();
        let ids: Vec<&str> = all["items"].as_array().unwrap().iter().map(|item| item["recording"].as_str().unwrap()).collect();
        assert_eq!(ids, [new.as_str(), old.as_str()]);
        assert_eq!(all["items"][0]["state"], "done");
        assert_eq!(all["items"][1]["end"], Value::Null);
        let recent = call(&recordings, &|_| None, yes, LIST, json!({"since": "2023-01-01T00:00:00Z", "limit": 5})).unwrap();
        assert_eq!(recent["items"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_transcription_running_shows_its_progress() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let running = recordings.create(2_000).unwrap();
        recordings.set_meta(&running, &Meta { state: State::Transcribing, ..recordings.meta(&running).unwrap() }).unwrap();
        let done = done(&recordings, 1);
        let percent = |id: &str| (id == running).then_some(42);
        let list = call(&recordings, &percent, yes, LIST, json!({})).unwrap();
        let progress: Vec<(&str, &Value)> =
            list["items"].as_array().unwrap().iter().map(|item| (item["recording"].as_str().unwrap(), &item["progress"])).collect();
        assert_eq!(progress, [(done.as_str(), &Value::Null), (running.as_str(), &json!(42))]);
    }

    #[test]
    fn before_pages_past_the_newest_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let ids: Vec<String> = (1..=3).map(|start| recordings.create(start * 1000).unwrap()).collect();
        let page = |args: Value| -> Vec<String> {
            let page = call(&recordings, &|_| None, yes, LIST, args).unwrap();
            page["items"].as_array().unwrap().iter().map(|item| item["recording"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(page(json!({"limit": 1})), [ids[2].clone()]);
        assert_eq!(page(json!({"limit": 1, "before": ids[2]})), [ids[1].clone()]);
        assert_eq!(page(json!({"before": ids[1]})), [ids[0].clone()]);
        assert!(page(json!({"before": ids[0]})).is_empty());
        for before in ["0123456789abcdef0123456789abcdef", "..\\x"] {
            assert!(matches!(call(&recordings, &|_| None, yes, LIST, json!({"before": before})), Err(ToolError::InvalidArgs(_))), "{before}");
        }
    }

    #[test]
    fn a_permission_taken_back_during_a_call_lets_nothing_out() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = done(&recordings, 1);
        let no = || Ok(false);
        assert!(matches!(call(&recordings, &|_| None, no, LIST, json!({})), Err(ToolError::NotAllowed(_))));
        // Allowed when asked first, taken back before the answer.
        let asked = Cell::new(0);
        let revoked = || {
            asked.set(asked.get() + 1);
            Ok(asked.get() == 1)
        };
        assert!(matches!(call(&recordings, &|_| None, revoked, TRANSCRIPT, json!({"recording": id})), Err(ToolError::NotAllowed(_))));
        assert_eq!(asked.get(), 2);
    }
}
