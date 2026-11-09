//! The files of a captions recording (spec 2026-11-04-captions-archive, 4.4 and 4.5): its cues in
//! `captions.jsonl`, one line each in the order they came, word for word; the times nothing was
//! heard in `gaps.jsonl`; and the archiver's own state of the calls, `captions-calls.json`, beside
//! the recordings. Every line is read as possibly damaged: a bad one is skipped and counted, never
//! a panic.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{read_capped, valid_id, write_atomic};

pub const CAPTIONS: &str = "captions.jsonl";
pub const GAPS: &str = "gaps.jsonl";
/// The archiver's state of the calls, in the recordings folder.
pub const CALLS: &str = "captions-calls.json";
/// The most lines of `gaps.jsonl`; later gaps lengthen the last line (4.5).
pub const MAX_GAPS: usize = 1000;
/// A gap that starts at most this long after the end of the last one of its reason lengthens it.
const MERGE_MS: u64 = 60_000;
/// A cue's speaker and text, in characters, as the captions spec caps them.
pub const MAX_SPEAKER: usize = 200;
pub const MAX_TEXT: usize = 4000;
/// The longest line read; a cue at its limits, every character escaped, stays well below.
const MAX_LINE: u64 = 64 * 1024;
/// The calls the state keeps, and how long after a call was last seen.
pub const MAX_CALLS: usize = 1000;
pub const CALL_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_CALLS_FILE: u64 = 4 * 1024 * 1024;

/// One cue of `captions.jsonl`: milliseconds from the recording's start, as the module sent it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    pub at: u64,
    pub speaker: String,
    pub own: bool,
    pub text: String,
}

impl Cue {
    /// The cue within the captions spec's limits.
    pub fn capped(at: u64, speaker: &str, own: bool, text: &str) -> Cue {
        Cue { at, speaker: speaker.chars().take(MAX_SPEAKER).collect(), own, text: text.chars().take(MAX_TEXT).collect() }
    }
}

/// Appends `cues` to the recording folder's `captions.jsonl`, after cutting a line a crash left
/// unfinished (4.4).
pub fn append_cues(dir: &Path, cues: &[Cue]) -> io::Result<()> {
    if cues.is_empty() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    for cue in cues {
        serde_json::to_writer(&mut bytes, cue).map_err(io::Error::other)?;
        bytes.push(b'\n');
    }
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(CAPTIONS))?;
    let whole = whole_lines(&mut file)?;
    file.set_len(whole)?;
    file.seek(SeekFrom::Start(whole))?;
    file.write_all(&bytes)?;
    file.flush()
}

/// The length of `file` up to its last newline.
fn whole_lines(file: &mut File) -> io::Result<u64> {
    let mut end = file.metadata()?.len();
    let mut chunk = vec![0u8; 8192];
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let len = usize::try_from(end - start).unwrap_or(0);
        let Some(chunk) = chunk.get_mut(..len) else { return Ok(0) };
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(at) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + at as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Reads a JSON Lines file a line at a time: `each` gets every line parsed, or None for a damaged
/// one, and returns false to stop. A last line without its newline is left out unseen while the
/// file is being `written` (it may be half written now), and is damaged otherwise (4.4). A missing
/// file has no lines.
pub fn read_lines<T: DeserializeOwned>(path: &Path, written: bool, mut each: impl FnMut(Option<T>) -> bool) -> io::Result<()> {
    let mut reader = match File::open(path) {
        Ok(file) => BufReader::new(file),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut line = Vec::new();
    loop {
        line.clear();
        if (&mut reader).take(MAX_LINE + 1).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let parsed = match line.strip_suffix(b"\n") {
            Some(whole) => serde_json::from_slice(whole).ok(),
            None if line.len() as u64 > MAX_LINE => {
                // Too long to be one of ours: the rest of it goes too.
                let mut rest = Vec::new();
                while (&mut reader).take(MAX_LINE).read_until(b'\n', &mut rest)? > 0 && rest.last() != Some(&b'\n') {
                    rest.clear();
                }
                None
            }
            None if written => return Ok(()),
            None => None,
        };
        if !each(parsed) {
            return Ok(());
        }
    }
}

/// Why nothing was heard for a while (4.5): a fixed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// From the recording's start to the first live state of its session.
    LateStart,
    /// Gaps of several reasons, past [`MAX_GAPS`].
    Mixed,
    /// The call had no captions.
    CaptionsOff,
    /// Cues lost: gone from the module's buffer unread, or the rest of a session never read.
    Lost,
    /// From the last read of a session that ended until the next one was live.
    Rejoin,
}

/// One line of `gaps.jsonl`, in milliseconds from the recording's start; `to` None while it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub from: u64,
    pub to: Option<u64>,
    pub reason: GapReason,
}

/// The gaps of a recording, as `gaps.jsonl` keeps them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gaps(pub Vec<Gap>);

impl Gaps {
    /// The gaps of the recording folder `dir` and the damaged lines of the file.
    pub fn load(dir: &Path, written: bool) -> io::Result<(Gaps, usize)> {
        let (mut gaps, mut damaged) = (Vec::new(), 0);
        read_lines(&dir.join(GAPS), written, |gap: Option<Gap>| {
            match gap {
                Some(gap) => gaps.push(gap),
                None => damaged += 1,
            }
            true
        })?;
        Ok((Gaps(gaps), damaged))
    }

    /// Written whole: at most [`MAX_GAPS`] short lines, never part of one after a crash.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let mut bytes = Vec::new();
        for gap in &self.0 {
            serde_json::to_writer(&mut bytes, gap).map_err(io::Error::other)?;
            bytes.push(b'\n');
        }
        write_atomic(&dir.join(GAPS), &bytes)
    }

    /// Adds a gap: the last one of its reason grows when this one starts at most a minute after
    /// it ends; past [`MAX_GAPS`] the last line grows over it, `mixed` when the reasons differ.
    pub fn add(&mut self, from: u64, to: Option<u64>, reason: GapReason) {
        let to = to.map(|to| to.max(from));
        if to == Some(from) {
            return;
        }
        let later = |old: Option<u64>| match (old, to) {
            (Some(old), Some(to)) => Some(old.max(to)),
            _ => None,
        };
        if let Some(last) = self.0.iter_mut().rev().find(|gap| gap.reason == reason)
            && last.to.is_none_or(|end| from <= end.saturating_add(MERGE_MS))
        {
            last.from = last.from.min(from);
            last.to = later(last.to);
            return;
        }
        if self.0.len() >= MAX_GAPS
            && let Some(last) = self.0.last_mut()
        {
            last.to = later(last.to);
            if last.reason != reason {
                last.reason = GapReason::Mixed;
            }
            return;
        }
        self.0.push(Gap { from, to, reason });
    }

    /// Ends the lasting gaps of `reason` (every reason when None) at `at`.
    pub fn end(&mut self, reason: Option<GapReason>, at: u64) {
        for gap in self.0.iter_mut().filter(|gap| gap.to.is_none() && reason.is_none_or(|reason| gap.reason == reason)) {
            gap.to = Some(at.max(gap.from));
        }
    }

    /// Whether a gap of `reason` lasts.
    pub fn open(&self, reason: GapReason) -> bool {
        self.0.iter().any(|gap| gap.reason == reason && gap.to.is_none())
    }

    /// The share of `total` milliseconds no gap covers, in whole percent; overlapping gaps count
    /// once and a lasting one to `total` (4.5).
    pub fn coverage(&self, total: u64) -> u8 {
        if total == 0 {
            return 100;
        }
        let mut spans: Vec<(u64, u64)> =
            self.0.iter().map(|gap| (gap.from.min(total), gap.to.unwrap_or(total).min(total))).filter(|(from, to)| from < to).collect();
        spans.sort_unstable();
        let (mut covered, mut reach) = (0u64, 0u64);
        for (from, to) in spans {
            let from = from.max(reach);
            if to > from {
                covered += to - from;
                reach = to;
            }
        }
        u8::try_from((total - covered.min(total)).saturating_mul(100) / total).unwrap_or(100)
    }
}

/// What the archiver does with a call (4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Active,
    /// The owner stopped it ("Zatrzymaj", or deleted its recording while it ran): never again.
    Blocked,
    /// It could not be joined for good.
    GaveUp,
}

/// One call of `captions-calls.json`: an account and a `call` of `teams_list_calls`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub account: String,
    pub call: String,
    #[serde(default)]
    pub recording: Option<String>,
    pub state: CallState,
    /// When the call was last listed, in milliseconds since 1970.
    pub seen_ms: u64,
    /// Its captions were on at some time: open_mess never turns them on again (4.3).
    #[serde(default)]
    pub captions_seen: bool,
    /// Joins that failed for a reason that may pass.
    #[serde(default)]
    pub attempts: u32,
}

/// A call id as `teams_list_calls` gives it (4.2): `^[A-Za-z0-9-]{1,64}$`.
pub fn valid_call(call: &str) -> bool {
    (1..=64).contains(&call.len()) && call.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn calls_path(dir: &Path) -> PathBuf {
    dir.join(CALLS)
}

fn bak_path(dir: &Path) -> PathBuf {
    dir.join(format!("{CALLS}.bak"))
}

fn parse_calls(path: &Path) -> io::Result<Vec<Call>> {
    let calls: Vec<Call> =
        serde_json::from_slice(&read_capped(path, MAX_CALLS_FILE)?).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(calls
        .into_iter()
        .filter(|call| valid_call(&call.call) && call.account.len() <= 256 && call.recording.as_deref().is_none_or(valid_id))
        .take(MAX_CALLS)
        .collect())
}

/// The calls of the recordings folder `dir`, and whether their state was lost: the file cannot be
/// read (then they are its previous version's, when that reads), whatever the previous version
/// holds (4.4). No file at all is no call, nothing lost.
pub fn load_calls(dir: &Path) -> (Vec<Call>, bool) {
    match parse_calls(&calls_path(dir)) {
        Ok(calls) => (calls, false),
        Err(err) if err.kind() == io::ErrorKind::NotFound && !bak_path(dir).exists() => (Vec::new(), false),
        Err(_) => (parse_calls(&bak_path(dir)).unwrap_or_default(), true),
    }
}

/// Writes the calls, without those not seen for [`CALL_TTL_MS`] and past [`MAX_CALLS`] (the
/// longest unseen first). The file it replaces stays as its previous version.
pub fn save_calls(dir: &Path, calls: &mut Vec<Call>, now_ms: u64) -> io::Result<()> {
    calls.retain(|call| call.seen_ms.saturating_add(CALL_TTL_MS) > now_ms);
    if calls.len() > MAX_CALLS {
        calls.sort_by_key(|call| std::cmp::Reverse(call.seen_ms));
        calls.truncate(MAX_CALLS);
    }
    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(calls).map_err(io::Error::other)?;
    // The previous version first, copied whole: after a crash in between the file is still there.
    if let Ok(previous) = fs::read(calls_path(dir)) {
        write_atomic(&bak_path(dir), &previous)?;
    }
    write_atomic(&calls_path(dir), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(at: u64, text: &str) -> Cue {
        Cue { at, speaker: "Ann".into(), own: false, text: text.into() }
    }

    fn cues(path: &Path, written: bool) -> (Vec<Cue>, usize) {
        let (mut found, mut damaged) = (Vec::new(), 0);
        read_lines(path, written, |cue: Option<Cue>| {
            match cue {
                Some(cue) => found.push(cue),
                None => damaged += 1,
            }
            true
        })
        .unwrap();
        (found, damaged)
    }

    #[test]
    fn cues_are_kept_word_for_word() {
        let dir = tempfile::tempdir().unwrap();
        let odd = "line one\nline \"two\"\t\\ </p><script>\u{0}\u{1F600} zażółć";
        append_cues(dir.path(), &[cue(1, odd), cue(2, "")]).unwrap();
        append_cues(dir.path(), &[cue(3, "next")]).unwrap();
        let (found, damaged) = cues(&dir.path().join(CAPTIONS), false);
        assert_eq!(found, [cue(1, odd), cue(2, ""), cue(3, "next")]);
        assert_eq!(damaged, 0);
    }

    #[test]
    fn a_torn_line_is_cut_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        append_cues(dir.path(), &[cue(1, "kept")]).unwrap();
        // The daemon stopped in the middle of a line.
        let path = dir.path().join(CAPTIONS);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"at\": 2, \"speak").unwrap();
        drop(file);
        // Being written: not counted; read after the recording ended: damaged.
        assert_eq!(cues(&path, true), (vec![cue(1, "kept")], 0));
        assert_eq!(cues(&path, false), (vec![cue(1, "kept")], 1));
        append_cues(dir.path(), &[cue(3, "after")]).unwrap();
        assert_eq!(cues(&path, false), (vec![cue(1, "kept"), cue(3, "after")], 0));
        // A file with no newline at all goes whole.
        fs::write(&path, vec![b'x'; 20_000]).unwrap();
        append_cues(dir.path(), &[cue(4, "alone")]).unwrap();
        assert_eq!(cues(&path, false), (vec![cue(4, "alone")], 0));
    }

    #[test]
    fn a_bad_line_in_the_middle_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CAPTIONS);
        let long = "x".repeat(MAX_LINE as usize * 2);
        fs::write(&path, format!("{}\nnot json\n{{\"at\": 1}}\n{long}\n\n{}\n", json(&cue(1, "a")), json(&cue(2, "b")))).unwrap();
        assert_eq!(cues(&path, false), (vec![cue(1, "a"), cue(2, "b")], 4));
        // A missing file has nothing.
        assert_eq!(cues(&dir.path().join("none.jsonl"), false), (Vec::new(), 0));
    }

    fn json(cue: &Cue) -> String {
        serde_json::to_string(cue).unwrap()
    }

    #[test]
    fn cues_are_capped_by_characters() {
        let cue = Cue::capped(0, &"ż".repeat(300), true, &"ą".repeat(5000));
        assert_eq!((cue.speaker.chars().count(), cue.text.chars().count()), (MAX_SPEAKER, MAX_TEXT));
    }

    fn gap(from: u64, to: Option<u64>, reason: GapReason) -> Gap {
        Gap { from, to, reason }
    }

    #[test]
    fn gaps_of_a_reason_merge_within_a_minute() {
        let mut gaps = Gaps::default();
        gaps.add(0, Some(10_000), GapReason::LateStart);
        gaps.add(20_000, Some(30_000), GapReason::Lost);
        gaps.add(80_000, Some(90_000), GapReason::Lost);
        gaps.add(200_000, Some(210_000), GapReason::Lost);
        gaps.add(205_000, None, GapReason::CaptionsOff);
        gaps.add(206_000, Some(207_000), GapReason::CaptionsOff);
        assert_eq!(
            gaps.0,
            [
                gap(0, Some(10_000), GapReason::LateStart),
                gap(20_000, Some(90_000), GapReason::Lost),
                gap(200_000, Some(210_000), GapReason::Lost),
                gap(205_000, None, GapReason::CaptionsOff),
            ]
        );
        assert!(gaps.open(GapReason::CaptionsOff));
        gaps.end(Some(GapReason::CaptionsOff), 260_000);
        assert_eq!(gaps.0.last(), Some(&gap(205_000, Some(260_000), GapReason::CaptionsOff)));
        assert!(!gaps.open(GapReason::CaptionsOff));
    }

    #[test]
    fn a_gap_every_30_s_for_12_h_stays_under_the_line_limit_and_loses_no_time() {
        let end = 12 * 60 * 60 * 1000;
        // One reason: they all merge.
        let mut same = Gaps::default();
        for at in (0..end).step_by(30_000) {
            same.add(at, Some(at + 1000), GapReason::Lost);
        }
        assert_eq!(same.0, [gap(0, Some(end - 29_000), GapReason::Lost)]);
        // Three reasons in turn: a reason's gaps are 90 s apart and never merge.
        let reasons = [GapReason::Lost, GapReason::CaptionsOff, GapReason::Rejoin];
        let mut gaps = Gaps::default();
        let mut at = 0;
        for reason in reasons.iter().cycle().take(usize::try_from(end / 30_000).unwrap()) {
            gaps.add(at, Some(at + 1000), *reason);
            at += 30_000;
        }
        assert_eq!(gaps.0.len(), MAX_GAPS);
        let dir = tempfile::tempdir().unwrap();
        gaps.save(dir.path()).unwrap();
        assert!(fs::metadata(dir.path().join(GAPS)).unwrap().len() < 80_000);
        // The last line covers every later gap.
        let last = gaps.0.last().unwrap();
        assert_eq!(last.reason, GapReason::Mixed);
        assert!(last.to.unwrap() >= at - 30_000);
        assert_eq!(Gaps::load(dir.path(), false).unwrap(), (gaps, 0));
    }

    #[test]
    fn coverage_counts_overlaps_once_and_a_lasting_gap_to_now() {
        let gaps =
            Gaps(vec![gap(0, Some(10_000), GapReason::LateStart), gap(5_000, Some(20_000), GapReason::Lost), gap(90_000, None, GapReason::Rejoin)]);
        // 20 s of the first 100 s, and the lasting one from 90 s.
        assert_eq!(gaps.coverage(100_000), 70);
        // A recording going on without new gaps covers more as time passes.
        let one = Gaps(vec![gap(0, Some(10_000), GapReason::LateStart)]);
        assert!(one.coverage(20_000) < one.coverage(200_000));
        assert_eq!(Gaps::default().coverage(0), 100);
        assert_eq!(Gaps(vec![gap(0, None, GapReason::Rejoin)]).coverage(5), 0);
    }

    #[test]
    fn a_damaged_gap_line_is_skipped_and_coverage_comes_from_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(GAPS), "{\"from\": 0, \"to\": 50000, \"reason\": \"lost\"}\n{\"from\": oops\n").unwrap();
        let (gaps, damaged) = Gaps::load(dir.path(), false).unwrap();
        assert_eq!((gaps.coverage(100_000), damaged), (50, 1));
    }

    fn call(call: &str, seen_ms: u64) -> Call {
        Call { account: "teams:a".into(), call: call.into(), recording: None, state: CallState::Active, seen_ms, captions_seen: false, attempts: 0 }
    }

    #[test]
    fn the_calls_keep_a_previous_version_and_forget_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_calls(dir.path()), (Vec::new(), false));
        let mut calls = vec![call("a", 1000), Call { state: CallState::Blocked, ..call("b", 2000) }];
        save_calls(dir.path(), &mut calls, 3000).unwrap();
        assert_eq!(load_calls(dir.path()), (calls.clone(), false));
        let mut later = vec![call("c", CALL_TTL_MS + 1500)];
        later.extend(calls.clone());
        save_calls(dir.path(), &mut later, CALL_TTL_MS + 1500).unwrap();
        // "a" was last seen a day ago.
        assert_eq!(later.iter().map(|call| call.call.as_str()).collect::<Vec<_>>(), ["c", "b"]);
        // Unreadable: the previous version, and the state counts as lost.
        fs::write(dir.path().join(CALLS), b"{broken").unwrap();
        assert_eq!(load_calls(dir.path()), (calls, true));
        fs::write(bak_path(dir.path()), b"also broken").unwrap();
        assert_eq!(load_calls(dir.path()), (Vec::new(), true));
    }

    #[test]
    fn calls_are_capped_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let mut calls: Vec<Call> = (0..MAX_CALLS as u64 + 5).map(|n| call(&format!("c{n}"), 10_000 + n)).collect();
        save_calls(dir.path(), &mut calls, 20_000).unwrap();
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(!calls.iter().any(|call| call.call == "c0"));
        let bad = r#"[{"account": "a", "call": "../x", "state": "active", "seen_ms": 1},
                      {"account": "a", "call": "ok", "recording": "..", "state": "active", "seen_ms": 1},
                      {"account": "a", "call": "fine", "state": "blocked", "seen_ms": 1}]"#;
        fs::write(dir.path().join(CALLS), bad).unwrap();
        let (calls, lost) = load_calls(dir.path());
        assert_eq!((calls.len(), calls[0].call.as_str(), lost), (1, "fine", false));
        assert!(valid_call("19ab-CD") && !valid_call("") && !valid_call(&"a".repeat(65)) && !valid_call("a b"));
    }
}
