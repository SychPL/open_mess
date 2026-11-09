//! The tools of the local recordings (spec 2026-10-28-local-transcription, 4.5): the list of
//! recordings and one recording's transcript, under the read permission of the reserved account
//! `local`; a recording of a call's live captions also under the read permission of its
//! conversation (spec 2026-11-04-captions-archive, 4.6). No tool starts a recording, changes a
//! setting or deletes one: only the configuration window does (spec 15: the CLI is not a trusted
//! channel).

use chrono::{DateTime, SecondsFormat};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolError, ToolInfo, failed, shared};
use crate::recording::archive::{self, Cue, Gaps};
use crate::recording::{Entry, Meta, Recordings, Source, State, valid_id};

pub const LIST: &str = "list_recordings";
pub const TRANSCRIPT: &str = "get_recording_transcript";
pub const STOP: &str = "stop_recording";
/// The most cues of one page, and the bounds of a page's size.
const MAX_PAGE_CUES: usize = 2000;
const MAX_BYTES: (u64, u64, u64) = (4096, 786_432, 131_072);
/// How an agent adds a meeting's chat to the transcript: by time, since a recording does not know
/// its meeting (spec 4.5, the owner's decision of 2026-11-03).
const CHAT: &str = "A transcript has no chat. For a Teams meeting, if teams_list_meetings is offered, it gives from \
                    an hour before the recording's start to its end (or now) the meeting that overlaps it most; \
                    read_messages of that meeting's conversation, newest first and paged with before, up to the \
                    recording's end, is its chat: all of it for a single meeting (an agenda sent before it too), \
                    and back to the end of the previous occurrence for a recurring one, whose series shares one chat. \
                    Without teams_list_meetings, list_conversations and search find the meeting's chat like any \
                    conversation.";
/// What a captions recording is (spec 2026-11-04-captions-archive, 4.6).
const CAPTIONS: &str = "A recording with source captions holds the call's Teams live captions as they came, with the \
                        speakers' names: speech recognition, so words may be wrong. Nothing was heard during its gaps, \
                        so a missing sentence means something only outside them. Its conversation, when it is a chat \
                        thread, is the call's chat, which read_messages reads. The same call may also have a recording \
                        with source audio.";

pub fn tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: LIST.into(),
            description: format!(
                "Calls recorded on this computer, newest first, with their state: transcribed locally (source audio) or \
                 kept from their live captions (source captions, with their coverage in percent and number of gaps). \
                 Their text comes from get_recording_transcript. {CAPTIONS} {CHAT}"
            ),
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
            description: format!(
                "The text of one recording, a page at a time: pieces of speech with times from the recording's start. \
                 Source audio: the side that spoke (me: the user's microphone, others: what the call played). Source \
                 captions: the speaker's name, own when the user spoke, the gaps and damaged_lines on the first page; \
                 readable while the call goes on. {CAPTIONS} {CHAT}"
            ),
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

/// Whether an agent may read: the recordings at all with None, one recording with its meta.
pub type Readable<'a> = &'a dyn Fn(Option<&Meta>) -> Result<bool, ToolError>;

/// Runs `name` (one of [`LIST`] and [`TRANSCRIPT`]); `readable` says whether an agent may read
/// the recordings now. It is asked before the files are read and again before the answer goes
/// out, so a permission taken back meanwhile lets nothing through (4.5). `progress` is the
/// transcription's progress of a recording, while it runs.
pub fn call(
    recordings: &Recordings,
    progress: &dyn Fn(&str) -> Option<u8>,
    readable: Readable<'_>,
    name: &str,
    arguments: Value,
) -> Result<Value, ToolError> {
    if !arguments.is_object() {
        return Err(ToolError::InvalidArgs("arguments must be a JSON object".into()));
    }
    if !readable(None)? {
        return Err(not_allowed());
    }
    let result = match name {
        LIST => list(recordings, progress, readable, args(arguments)?)?,
        TRANSCRIPT => transcript(recordings, readable, args(arguments)?)?,
        _ => return Err(ToolError::UnknownTool(name.into())),
    };
    if !readable(None)? {
        return Err(not_allowed());
    }
    Ok(shared::with_notice(result))
}

fn list(recordings: &Recordings, progress: &dyn Fn(&str) -> Option<u8>, readable: Readable<'_>, args: ListArgs) -> Result<Value, ToolError> {
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
    let unknown = || ToolError::InvalidArgs("unknown recording in before - list_recordings lists them".into());
    let first = match args.before {
        Some(before) => {
            let at = entries.iter().position(|Entry { id, .. }| *id == before).ok_or_else(unknown)?;
            // One the agent may not read was never on its page.
            if !entries.get(at).is_some_and(|entry| readable(Some(&entry.meta)).unwrap_or(false)) {
                return Err(unknown());
            }
            at + 1
        }
        None => 0,
    };
    // Filtered before the limit: a page ends only at its limit or at the end (4.6, R4).
    let mut kept = Vec::new();
    for entry in entries.into_iter().skip(first) {
        if kept.len() == limit as usize {
            break;
        }
        if since.is_none_or(|since| i64::try_from(entry.meta.start_ms).is_ok_and(|start| start >= since)) && readable(Some(&entry.meta))? {
            kept.push(entry);
        }
    }
    let mut items = Vec::new();
    for Entry { id, meta } in kept {
        // Asked again just before the answer.
        if !readable(Some(&meta))? {
            continue;
        }
        let percent = (meta.state == State::Transcribing).then(|| progress(&id)).flatten();
        let mut item = json!({
            "recording": id,
            "source": meta.source,
            "start": time(meta.start_ms),
            "end": meta.end_ms.and_then(time),
            "state": meta.state,
            "progress": percent,
            "language": language(&meta.language),
            "reason": meta.reason,
        });
        if meta.source == Source::Captions
            && let Some(item) = item.as_object_mut()
        {
            item.insert("account".into(), meta.account.into());
            item.insert("conversation".into(), meta.conversation.into());
            item.insert("coverage".into(), meta.coverage.into());
            item.insert("gaps".into(), meta.gaps.unwrap_or(0).into());
        }
        items.push(item);
    }
    Ok(json!({"items": items}))
}

fn transcript(recordings: &Recordings, readable: Readable<'_>, args: TranscriptArgs) -> Result<Value, ToolError> {
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
    if !readable(Some(&meta))? {
        return Err(not_allowed());
    }
    let page = match meta.source {
        Source::Audio => audio_page(recordings, &args, &meta, max_bytes)?,
        Source::Captions => captions_page(recordings, &args, &meta, max_bytes)?,
    };
    // A call's threads may have grown meanwhile: the latest meta is asked about.
    let meta = recordings.meta(&args.recording).unwrap_or(meta);
    if !readable(Some(&meta))? {
        return Err(not_allowed());
    }
    Ok(page)
}

fn audio_page(recordings: &Recordings, args: &TranscriptArgs, meta: &Meta, max_bytes: u64) -> Result<Value, ToolError> {
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

/// A page of a captions recording, readable in every state (4.6): its cues from `cursor` (the
/// count of whole cues before), and on the first page its gaps and the lines that could not be
/// read.
fn captions_page(recordings: &Recordings, args: &TranscriptArgs, meta: &Meta, max_bytes: u64) -> Result<Value, ToolError> {
    let dir = recordings.path(&args.recording).ok_or_else(|| ToolError::InvalidArgs("unknown recording".into()))?;
    let written = meta.state == State::Recording;
    let first = args.cursor.unwrap_or(0);
    let (mut cues, mut used, mut seen, mut more, mut damaged) = (Vec::new(), 0u64, 0u64, false, 0usize);
    let read = archive::read_lines(&dir.join(archive::CAPTIONS), written, |cue: Option<Cue>| {
        let Some(cue) = cue else {
            damaged += 1;
            return true;
        };
        // The first page goes on to the end, counting the damaged lines.
        if more {
            return args.cursor.is_none();
        }
        if seen < first {
            seen += 1;
            return true;
        }
        let item = json!({"start": offset(cue.at), "end": null, "speaker": cue.speaker, "own": cue.own, "text": cue.text});
        let size = item.to_string().len() as u64;
        if cues.len() == MAX_PAGE_CUES || (!cues.is_empty() && used + size > max_bytes) {
            more = true;
            return args.cursor.is_none();
        }
        used += size;
        seen += 1;
        cues.push(item);
        true
    });
    read.map_err(|_| ToolError::Failed("the captions cannot be read".into()))?;
    if seen < first {
        return Err(ToolError::InvalidArgs("cursor out of range".into()));
    }
    let next = first + cues.len() as u64;
    let mut page = json!({
        "recording": args.recording,
        "source": Source::Captions,
        "start": time(meta.start_ms),
        "end": meta.end_ms.and_then(time),
        "state": meta.state,
        "reason": meta.reason,
        "conversation": meta.conversation,
        "next_cursor": more.then_some(next),
        "untrusted": {"cues": cues},
    });
    if args.cursor.is_none()
        && let Some(page) = page.as_object_mut()
    {
        let (gaps, gap_lines) = Gaps::load(&dir, written).map_err(|_| ToolError::Failed("the gaps cannot be read".into()))?;
        let gaps: Vec<Value> = gaps.0.iter().map(|gap| json!({"from": offset(gap.from), "to": gap.to.map(offset), "reason": gap.reason})).collect();
        page.insert("gaps".into(), gaps.into());
        page.insert("damaged_lines".into(), (damaged + gap_lines).into());
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use super::*;
    use crate::recording::archive::{Gap, GapReason};
    use crate::recording::{Cue as AudioCue, Speaker, Transcript};

    fn done(recordings: &Recordings, cues: usize) -> String {
        let id = recordings.create(1_700_000_000_000).unwrap();
        let cues = (0..cues).map(|i| AudioCue {
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

    fn yes(_: Option<&Meta>) -> Result<bool, ToolError> {
        Ok(true)
    }

    fn captions(recordings: &Recordings, start_ms: u64, conversation: &str, cues: usize) -> String {
        let meta = Meta {
            source: Source::Captions,
            start_ms,
            account: Some("teams:a".into()),
            conversation: Some(conversation.into()),
            call: Some("call-1".into()),
            coverage: Some(90),
            gaps: Some(1),
            ..Meta::default()
        };
        let id = recordings.create_with(&meta).unwrap();
        let cues: Vec<Cue> = (0..cues).map(|i| Cue::capped(i as u64 * 1500, "Ann", i % 2 == 0, &format!("powiedziane {i}"))).collect();
        archive::append_cues(&recordings.path(&id).unwrap(), &cues).unwrap();
        id
    }

    #[test]
    fn the_descriptions_say_where_a_meetings_chat_is() {
        for tool in tools().iter().filter(|tool| tool.name != STOP) {
            let description = &tool.description;
            let ways = ["teams_list_meetings", "read_messages", "list_conversations", "search", "gaps"];
            assert!(ways.iter().all(|way| description.contains(way)), "{}", tool.name);
        }
    }

    #[test]
    fn a_transcript_comes_in_pages_marked_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = done(&recordings, 3000);
        // A small default page; the largest one takes the most cues a page has.
        let small = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id})).unwrap();
        let small = small["untrusted"]["cues"].as_array().unwrap();
        assert!(small.len() < MAX_PAGE_CUES && serde_json::to_vec(small).unwrap().len() as u64 <= MAX_BYTES.2 + small.len() as u64);
        let page = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id, "max_bytes": MAX_BYTES.1})).unwrap();
        assert_eq!(page["language"], "pl");
        assert_eq!(page["start"], "2023-11-14T22:13:20.000Z");
        let cues = page["untrusted"]["cues"].as_array().unwrap();
        assert_eq!(cues.len(), MAX_PAGE_CUES);
        assert_eq!(cues[1], json!({"start": "00:00:01.000", "end": "00:00:01.900", "speaker": "others", "text": "zdanie 1"}));
        assert!(page["untrusted_notice"].is_string());
        assert_eq!(page["next_cursor"], 2000);
        let last = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id, "cursor": 2000, "max_bytes": 4096})).unwrap();
        let cues = last["untrusted"]["cues"].as_array().unwrap();
        assert!(!cues.is_empty() && serde_json::to_vec(cues).unwrap().len() <= 4096 + 100);
        assert_eq!(last["next_cursor"], 2000 + cues.len());
        let end = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id, "cursor": 3000})).unwrap();
        assert_eq!((end["untrusted"]["cues"].as_array().unwrap().len(), &end["next_cursor"]), (0, &Value::Null));
    }

    #[test]
    fn bad_requests_are_refused_without_touching_files() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let waiting = recordings.create(0).unwrap();
        recordings.set_meta(&waiting, &Meta { state: State::Waiting, ..recordings.meta(&waiting).unwrap() }).unwrap();
        let id = done(&recordings, 1);
        let live = captions(&recordings, 0, "19:a@thread.v2", 2);
        for args in [
            json!({"recording": "..\\..\\x"}),
            json!({"recording": "0123456789abcdef0123456789abcdef"}),
            json!({"recording": id, "cursor": 5}),
            json!({"recording": live, "cursor": 3}),
            json!({"recording": id, "max_bytes": 10}),
            json!({"recording": id, "extra": 1}),
            json!([id]),
        ] {
            assert!(matches!(call(&recordings, &|_| None, &yes, TRANSCRIPT, args.clone()), Err(ToolError::InvalidArgs(_))), "{args}");
        }
        let err = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": waiting})).unwrap_err();
        assert_eq!(err.to_string(), "transcript not ready: waiting");
        assert!(matches!(call(&recordings, &|_| None, &yes, LIST, json!({"since": "yesterday"})), Err(ToolError::InvalidArgs(_))));
        assert!(matches!(call(&recordings, &|_| None, &yes, LIST, json!({"limit": 0})), Err(ToolError::InvalidArgs(_))));
    }

    #[test]
    fn the_list_is_newest_first_from_since() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let old = recordings.create(1_000).unwrap();
        let new = done(&recordings, 1);
        let all = call(&recordings, &|_| None, &yes, LIST, json!({})).unwrap();
        let ids: Vec<&str> = all["items"].as_array().unwrap().iter().map(|item| item["recording"].as_str().unwrap()).collect();
        assert_eq!(ids, [new.as_str(), old.as_str()]);
        assert_eq!(all["items"][0]["state"], "done");
        assert_eq!(all["items"][0]["source"], "audio");
        assert_eq!(all["items"][1]["end"], Value::Null);
        let recent = call(&recordings, &|_| None, &yes, LIST, json!({"since": "2023-01-01T00:00:00Z", "limit": 5})).unwrap();
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
        let list = call(&recordings, &percent, &yes, LIST, json!({})).unwrap();
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
            let page = call(&recordings, &|_| None, &yes, LIST, args).unwrap();
            page["items"].as_array().unwrap().iter().map(|item| item["recording"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(page(json!({"limit": 1})), [ids[2].clone()]);
        assert_eq!(page(json!({"limit": 1, "before": ids[2]})), [ids[1].clone()]);
        assert_eq!(page(json!({"before": ids[1]})), [ids[0].clone()]);
        assert!(page(json!({"before": ids[0]})).is_empty());
        for before in ["0123456789abcdef0123456789abcdef", "..\\x"] {
            assert!(matches!(call(&recordings, &|_| None, &yes, LIST, json!({"before": before})), Err(ToolError::InvalidArgs(_))), "{before}");
        }
    }

    #[test]
    fn a_permission_taken_back_during_a_call_lets_nothing_out() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = done(&recordings, 1);
        let no = |_: Option<&Meta>| Ok(false);
        assert!(matches!(call(&recordings, &|_| None, &no, LIST, json!({})), Err(ToolError::NotAllowed(_))));
        // Allowed when asked first, taken back before the answer.
        for id in [id, captions(&recordings, 0, "19:a@thread.v2", 1)] {
            let asked = Cell::new(0);
            let revoked = |_: Option<&Meta>| {
                asked.set(asked.get() + 1);
                Ok(asked.get() <= 2)
            };
            assert!(matches!(call(&recordings, &|_| None, &revoked, TRANSCRIPT, json!({"recording": id})), Err(ToolError::NotAllowed(_))));
            assert!(asked.get() > 2);
        }
    }

    #[test]
    fn a_captions_recording_reads_in_pages_while_it_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let id = captions(&recordings, 1_700_000_000_000, "19:a@thread.v2", 3000);
        let folder = recordings.path(&id).unwrap();
        Gaps(vec![Gap { from: 0, to: Some(12_000), reason: GapReason::LateStart }, Gap { from: 60_000, to: None, reason: GapReason::Rejoin }])
            .save(&folder)
            .unwrap();
        // A bad line in the middle and one being written at the end.
        let mut file = fs::OpenOptions::new().append(true).open(folder.join(archive::CAPTIONS)).unwrap();
        std::io::Write::write_all(&mut file, b"garbage\n{\"at\": 1, \"spea").unwrap();
        drop(file);
        let first = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id, "max_bytes": MAX_BYTES.1})).unwrap();
        assert_eq!((&first["source"], &first["state"], &first["conversation"]), (&json!("captions"), &json!("recording"), &json!("19:a@thread.v2")));
        let cues = first["untrusted"]["cues"].as_array().unwrap();
        assert_eq!(cues.len(), MAX_PAGE_CUES);
        assert_eq!(cues[1], json!({"start": "00:00:01.500", "end": null, "speaker": "Ann", "own": false, "text": "powiedziane 1"}));
        assert_eq!(
            first["gaps"],
            json!([{"from": "00:00:00.000", "to": "00:00:12.000", "reason": "late_start"}, {"from": "00:01:00.000", "to": null, "reason": "rejoin"}])
        );
        // The unfinished last line is not damaged while the recording goes on.
        assert_eq!(first["damaged_lines"], 1);
        let rest = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id, "cursor": first["next_cursor"]})).unwrap();
        assert!(rest.get("gaps").is_none() && rest.get("damaged_lines").is_none());
        assert_eq!(rest["untrusted"]["cues"][0]["text"], format!("powiedziane {MAX_PAGE_CUES}"));
        // Ended: the torn line counts.
        recordings.set_meta(&id, &Meta { state: State::Done, end_ms: Some(1_700_000_100_000), ..recordings.meta(&id).unwrap() }).unwrap();
        assert_eq!(call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": id})).unwrap()["damaged_lines"], 2);
        // A failed one without captions is an empty page with its reason.
        let failed = recordings
            .create_with(&Meta { source: Source::Captions, state: State::Failed, reason: Some(crate::recording::Reason::E2ee), ..Meta::default() })
            .unwrap();
        let page = call(&recordings, &|_| None, &yes, TRANSCRIPT, json!({"recording": failed})).unwrap();
        assert_eq!((page["untrusted"]["cues"].as_array().unwrap().len(), &page["reason"]), (0, &json!("e2ee")));
    }

    #[test]
    fn unreadable_captions_are_left_out_before_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path());
        let readable = captions(&recordings, 1_000, "19:open@thread.v2", 1);
        let hidden: Vec<String> = (0..60).map(|n| captions(&recordings, 2_000 + n, "19:secret@thread.v2", 1)).collect();
        let open = |meta: Option<&Meta>| Ok(meta.is_none_or(|meta| meta.conversation.as_deref() != Some("19:secret@thread.v2")));
        let page = call(&recordings, &|_| None, &open, LIST, json!({"limit": 50})).unwrap();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            (&items[0]["recording"], &items[0]["coverage"], &items[0]["gaps"], &items[0]["account"]),
            (&json!(readable), &json!(90), &json!(1), &json!("teams:a"))
        );
        // Neither its text nor a page after it.
        assert!(matches!(call(&recordings, &|_| None, &open, TRANSCRIPT, json!({"recording": hidden[0]})), Err(ToolError::NotAllowed(_))));
        assert!(matches!(call(&recordings, &|_| None, &open, LIST, json!({"before": hidden[0]})), Err(ToolError::InvalidArgs(_))));
    }
}
