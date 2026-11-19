//! The daemon's log file (spec 2026-11-18-daemon-log-file): open_mess's own `info`, `warn` and
//! `error` events, one cleaned line each, in `<local>\logs\open_mess.log`, the previous file kept
//! as `open_mess.old.log`.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{Filtered, LevelFilter, Targets};
use tracing_subscriber::layer::{Context, Layer};

/// The current file, in the log folder.
pub const FILE: &str = "open_mess.log";
/// The file before it, replaced at each rotation.
pub const OLD_FILE: &str = "open_mess.old.log";
/// Each file's limit: two files, at most 8 MiB together (4.3).
pub const MAX_FILE: u64 = 4 * 1024 * 1024;
/// One event's message and fields, after cleaning (4.5).
const MAX_LINE: usize = 4096;
/// Local time with its offset: the owner reports times as the clock showed them.
const TIME: &str = "%Y-%m-%d %H:%M:%S%.3f %:z";

/// open_mess's own targets only (`open_mess` and `open_mess_host`, by prefix), up to `level`. A
/// dependency's debug log, like rmcp's, prints whole tool results (Codex review of 0cff6c1), and no
/// dependency's text is checked against spec 8.4.
pub fn targets(level: LevelFilter) -> Targets {
    Targets::new().with_target("open_mess", level)
}

/// The file layer as the daemon installs it: `info` and above of open_mess's own targets, in `dir`.
/// Each of `folders`, the user's profile folders and the name each is written as (`%USERPROFILE%`),
/// is written as its name: the folders' names are often the user's.
pub fn layer<S: Subscriber>(dir: &Path, max: u64, mut folders: Vec<(String, String)>) -> Filtered<FileLayer, Targets, S> {
    folders.retain(|(folder, _)| folder.len() > 3);
    // The longest first: `%APPDATA%` is inside `%USERPROFILE%`.
    folders.sort_by_key(|(folder, _)| std::cmp::Reverse(folder.len()));
    FileLayer { file: Mutex::new(LogFile::new(dir, max)), folders }.with_filter(targets(LevelFilter::INFO))
}

/// Writes each event as one line of [`LogFile`]. Never panics, never fails, never prints.
pub struct FileLayer {
    /// One line at a time: two threads never rotate the file at once.
    file: Mutex<LogFile>,
    folders: Vec<(String, String)>,
}

impl<S: Subscriber> Layer<S> for FileLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        // A module without a name is the manifest call of a module not loaded yet, maybe one still
        // waiting for approval (runtime.rs): nothing of it reaches the file.
        if fields.nameless {
            return;
        }
        fields.message.push_str(&fields.rest);
        let mut text = fields.message;
        for (folder, name) in &self.folders {
            // A share also as a verbatim path (`\\?\UNC\server\...`); `C:\...` is inside `\\?\C:\...`.
            let verbatim = folder.strip_prefix(r"\\").map(|share| format!(r"\\?\UNC\{share}"));
            for path in verbatim.iter().chain([folder]) {
                // As Debug writes it (`{e:?}` of a path in a string: `\\`, `\u{a0}`), with forward
                // slashes, as it is.
                let debug = format!("{path:?}");
                let debug = debug.strip_prefix('"').and_then(|debug| debug.strip_suffix('"')).unwrap_or(path).to_owned();
                for form in [debug, path.replace('\\', "/"), path.clone()] {
                    text = without(&text, &form, name);
                }
            }
        }
        let meta = event.metadata();
        let mut line = String::new();
        // write! into a String, not to_string(), which panics when a Display fails. The process id
        // tells apart two daemons with their own data folders and one local folder.
        let now = local_now().format(TIME);
        let _ = writeln!(line, "{now} [{}] {:<5} {}: {}", std::process::id(), meta.level(), meta.target(), clean(&text));
        self.file.lock().unwrap_or_else(PoisonError::into_inner).write_line(&line);
    }
}

/// The local time, or UTC once Windows could not give the time zone: chrono unwraps that failure,
/// and the panic's message goes to stderr once, not with every line.
fn local_now() -> chrono::DateTime<chrono::FixedOffset> {
    static NO_ZONE: AtomicBool = AtomicBool::new(false);
    if !NO_ZONE.load(Ordering::Relaxed) {
        match std::panic::catch_unwind(chrono::Local::now) {
            Ok(now) => return now.fixed_offset(),
            Err(_) => NO_ZONE.store(true, Ordering::Relaxed),
        }
    }
    chrono::Utc::now().fixed_offset()
}

/// `text` with every `what`, in any case like a Windows path, written as `with`.
fn without(text: &str, what: &str, with: &str) -> String {
    if what.is_empty() {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(len) = starts_with_any_case(rest, what) {
            out.push_str(with);
            rest = rest.get(len..).unwrap_or_default();
            continue;
        }
        let mut chars = rest.chars();
        out.extend(chars.next());
        rest = chars.as_str();
    }
    out
}

/// The length of `text`'s start that is `what` in any case (`Ł` and `ł` alike), if it is.
fn starts_with_any_case(text: &str, what: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    for expected in what.chars() {
        let (_, c) = chars.next()?;
        if !c.to_lowercase().eq(expected.to_lowercase()) {
            return None;
        }
    }
    Some(chars.next().map_or(text.len(), |(at, _)| at))
}

/// An event's message, and its other fields as ` name=value`.
#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
    /// A `module` field that is empty.
    nameless: bool,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.add(field, format_args!("{value:?}"));
    }
}

impl Fields {
    fn add(&mut self, field: &Field, value: fmt::Arguments<'_>) {
        if field.name() == "message" {
            let _ = self.message.write_fmt(value);
            return;
        }
        let _ = write!(self.rest, " {}=", field.name());
        let start = self.rest.len();
        let _ = self.rest.write_fmt(value);
        self.nameless |= field.name() == "module" && self.rest.len() == start;
    }
}

/// The log file, rotated at `max` bytes and opened by name for each line: a handle kept open would
/// go on writing to a file that another daemon (its own data folder, the same local folder) rotated
/// away, and rotate the wrong one. A line that cannot be written is lost (4.4).
pub struct LogFile {
    dir: PathBuf,
    max: u64,
}

impl LogFile {
    pub fn new(dir: &Path, max: u64) -> LogFile {
        LogFile { dir: dir.to_path_buf(), max }
    }

    pub fn write_line(&self, line: &str) {
        let len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        let Some(mut file) = self.open() else { return };
        // The file's own size, not a count of this process's lines; an empty file takes any line.
        let full = |file: &File| match file.metadata() {
            Ok(meta) => meta.len() > 0 && meta.len().saturating_add(len) > self.max,
            Err(_) => true,
        };
        if full(&file) {
            // Closed first: Windows renames an open file only if every handle on it allows that.
            drop(file);
            if fs::rename(self.dir.join(FILE), self.dir.join(OLD_FILE)).is_err() {
                // Not renamed: the file starts again empty rather than grow past the limit, and is
                // appended to like always, so another daemon's line is never written over.
                let _ = File::create(self.dir.join(FILE));
            }
            file = match self.open() {
                // Neither renamed nor emptied (rights that allow appending only): no more lines.
                Some(fresh) if !full(&fresh) => fresh,
                _ => return,
            };
        }
        // A line cut short (the power went mid-write) is ended first, so this one starts its own.
        let mut last = [0_u8; 1];
        let cut = file.seek(SeekFrom::End(-1)).is_ok() && file.read_exact(&mut last).is_ok() && last != *b"\n";
        let _ = if cut { file.write_all(format!("\n{line}").as_bytes()) } else { file.write_all(line.as_bytes()) };
    }

    fn open(&self) -> Option<File> {
        fs::create_dir_all(&self.dir).ok()?;
        OpenOptions::new().create(true).read(true).append(true).open(self.dir.join(FILE)).ok()
    }
}

/// `text` as one line, with the shapes of secrets and ids taken out and at most [`MAX_LINE`]
/// bytes (4.5). A second line of defence: events never carry them in the first place (4.6).
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut word: Option<usize> = None;
    for (at, c) in text.char_indices() {
        if is_word(c) {
            word.get_or_insert(at);
            continue;
        }
        if let Some(start) = word.take() {
            push_word(&mut out, text.get(start..at).unwrap_or_default());
        }
        out.push(if is_hidden(c) { ' ' } else { c });
        if out.len() > MAX_LINE {
            break;
        }
    }
    if let Some(start) = word {
        push_word(&mut out, text.get(start..).unwrap_or_default());
    }
    if out.len() > MAX_LINE {
        out.truncate(out.floor_char_boundary(MAX_LINE));
        out.push_str("...");
    }
    out
}

/// Ids, tokens, addresses and URLs are words of these.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || "-_.:@/+%~?&#".contains(c)
}

/// Line breaks, terminal escapes, text direction and invisible characters.
fn is_hidden(c: char) -> bool {
    c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}')
}

/// An e-mail address, a Teams id (`19:...@thread.v2`), a GUID, a token, a URL or a phone number:
/// never a code, a count or a state's name.
fn push_word(out: &mut String, word: &str) {
    let chars = word.chars().count();
    let digits = word.chars().filter(char::is_ascii_digit).count();
    // A Teams MRI however short (`8:live:jan`, `28:bot`): a number, a colon, a letter; a time
    // (`10:40`) has a digit after its colon.
    let mri = word.split_once(':').is_some_and(|(kind, rest)| {
        (1..=3).contains(&kind.len()) && kind.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with(|c: char| c.is_ascii_alphabetic())
    });
    let secret = mri || word.contains('@') || digits >= 9 || chars >= 32 || (chars >= 24 && digits > 0);
    out.push_str(if secret { "<redacted>" } else { word });
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    fn read(dir: &Path, file: &str) -> String {
        fs::read_to_string(dir.join(file)).unwrap_or_default()
    }

    #[test]
    fn rotates_at_the_limit_and_keeps_one_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = LogFile::new(dir.path(), 100);
        for n in 0..20 {
            log.write_line(&format!("line {n:02} of the rotation test\n"));
        }
        // 29 bytes a line: three fit in 100.
        let lines = |range: std::ops::Range<u32>| range.map(|n| format!("line {n:02} of the rotation test\n")).collect::<String>();
        assert_eq!(read(dir.path(), FILE), lines(18..20));
        assert_eq!(read(dir.path(), OLD_FILE), lines(15..18));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_large_file_from_before_is_rotated_on_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), "x".repeat(90)).unwrap();
        LogFile::new(dir.path(), 100).write_line("a line that does not fit\n");
        assert_eq!(read(dir.path(), FILE), "a line that does not fit\n");
        assert_eq!(read(dir.path(), OLD_FILE), "x".repeat(90));
    }

    #[test]
    fn a_line_cut_short_is_ended_before_the_next() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), "a line the power cut").unwrap();
        LogFile::new(dir.path(), 100).write_line("the daemon started\n");
        assert_eq!(read(dir.path(), FILE), "a line the power cut\nthe daemon started\n");
    }

    #[test]
    fn a_failed_rename_empties_the_file_instead_of_growing_it() {
        let dir = tempfile::tempdir().unwrap();
        // A folder in the old file's place: the rename fails on Windows and Linux alike.
        fs::create_dir(dir.path().join(OLD_FILE)).unwrap();
        let log = LogFile::new(dir.path(), 100);
        for n in 0..20 {
            log.write_line(&format!("line {n:02} of the rename test\n"));
        }
        let current = read(dir.path(), FILE);
        assert!(current.len() <= 100, "{current:?}");
        assert!(current.ends_with("line 19 of the rename test\n"), "{current:?}");
        assert!(dir.path().join(OLD_FILE).is_dir());
    }

    #[test]
    fn a_folder_that_cannot_be_made_loses_lines_until_it_can() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("blocker");
        fs::write(&blocker, "a file where the log folder should be").unwrap();
        let dir = blocker.join("logs");
        let log = LogFile::new(&dir, 100);
        log.write_line("lost\n");
        // Through the layer too: nothing panics and nothing is returned.
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(&dir, 100, Vec::new())), || tracing::warn!("lost as well"));
        fs::remove_file(&blocker).unwrap();
        log.write_line("written\n");
        assert_eq!(read(&dir, FILE), "written\n");
    }

    #[test]
    fn only_info_and_above_of_open_mess_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(dir.path(), MAX_FILE, Vec::new())), || {
            tracing::info!(target: "open_mess::captions", reason = ?Some(3), cues = 12, "a recording ended");
            tracing::warn!(target: "open_mess_host::bindings", module = "teams", "polling failed");
            tracing::error!(target: "open_mess::daemon", "an error");
            tracing::debug!(target: "open_mess::daemon", "a debug line");
            tracing::warn!(target: "rmcp::service", "a dependency");
            // A module's crash, with its kind (actor.rs).
            tracing::warn!(target: "open_mess_host::actor", module = %"teams", crashes = 6, "module stopped: module call timed out");
        });
        let file = read(dir.path(), FILE);
        let lines: Vec<&str> = file.lines().collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[3].ends_with(" WARN  open_mess_host::actor: module stopped: module call timed out module=teams crashes=6"), "{lines:?}");
        // Time, level, target, message, fields.
        assert!(lines[0].ends_with(" INFO  open_mess::captions: a recording ended reason=Some(3) cues=12"), "{lines:?}");
        assert!(lines[1].ends_with(" WARN  open_mess_host::bindings: polling failed module=teams"), "{lines:?}");
        assert!(lines[2].ends_with(" ERROR open_mess::daemon: an error"), "{lines:?}");
    }

    #[test]
    fn a_module_without_a_name_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(dir.path(), MAX_FILE, Vec::new())), || {
            // A module's manifest call, before it is approved (bindings.rs, actor.rs).
            tracing::warn!(target: "open_mess_host::bindings", module = %"", "storage read failed: Failed");
            tracing::warn!(target: "open_mess_host::actor", module = "", crashes = 1, "module stopped");
            tracing::warn!(target: "open_mess_host::bindings", module = %"teams", "storage read failed: Failed");
        });
        let file = read(dir.path(), FILE);
        assert_eq!(file.lines().count(), 1, "{file}");
        assert!(file.trim_end().ends_with("storage read failed: Failed module=teams"), "{file}");
    }

    #[test]
    fn a_line_starts_with_the_local_time_and_the_process() {
        let dir = tempfile::tempdir().unwrap();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(dir.path(), MAX_FILE, Vec::new())), || tracing::info!("timed"));
        let line = read(dir.path(), FILE);
        let (time, rest) = line.split_once(&format!(" [{}] INFO ", std::process::id())).unwrap();
        chrono::DateTime::parse_from_str(time, TIME).unwrap();
        assert!(rest.ends_with(": timed\n"), "{line:?}");
    }

    #[test]
    fn the_profile_folder_is_written_as_userprofile() {
        let dir = tempfile::tempdir().unwrap();
        // `to_lowercase` below makes `Ł` an `ł`: Windows paths ignore case beyond ASCII too.
        let path = r"C:\Users\Łukasz Kowalski\AppData\Local\open_mess\recordings";
        // Roaming redirected to a share by a domain policy.
        let roaming = r"\\server\profiles\Łukasz Kowalski\AppData\Roaming";
        let folders = vec![(r"C:\Users\Łukasz Kowalski".to_owned(), "%USERPROFILE%".to_owned()), (roaming.to_owned(), "%APPDATA%".to_owned())];
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(dir.path(), MAX_FILE, folders)), || {
            tracing::warn!("unable to open database file: {}", path.to_lowercase());
            // As `{e:?}` of an error holding the path writes it, and with forward slashes.
            tracing::warn!("storage read failed: Failed({path:?})");
            tracing::warn!("{}", path.replace('\\', "/"));
            tracing::warn!(r"unable to open database file: {roaming}\open_mess\modules\teams.sqlite3");
            // The same share as a verbatim path (`OPEN_MESS_DATA_DIR` written so).
            tracing::warn!(r"unable to open database file: \\?\UNC\server\profiles\Łukasz Kowalski\AppData\Roaming\open_mess");
        });
        let file = read(dir.path(), FILE);
        let lines: Vec<&str> = file.lines().collect();
        assert!(lines[0].ends_with(r": unable to open database file: %USERPROFILE%\appdata\local\open_mess\recordings"), "{file}");
        assert!(lines[1].ends_with(r#": storage read failed: Failed("%USERPROFILE%\\AppData\\Local\\open_mess\\recordings")"#), "{file}");
        assert!(lines[3].ends_with(r": unable to open database file: %APPDATA%\open_mess\modules\teams.sqlite3"), "{file}");
        assert!(lines[4].ends_with(r": unable to open database file: %APPDATA%\open_mess"), "{file}");
        assert_eq!(lines.len(), 5, "{file}");
        assert!(!file.to_lowercase().contains("łukasz"), "{file}");
    }

    #[test]
    fn a_profile_debug_escapes_is_written_as_userprofile_too() {
        let dir = tempfile::tempdir().unwrap();
        // A no-break space, which Debug writes as `\u{a0}`.
        let home = "C:\\Users\\Jan\u{a0}Kowalski".to_owned();
        let path = format!(r"{home}\AppData\Roaming\open_mess\modules\teams.sqlite3");
        let folders = vec![(home, "%USERPROFILE%".to_owned())];
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer(dir.path(), MAX_FILE, folders)), || {
            tracing::warn!("storage read failed: Failed({path:?})");
        });
        let file = read(dir.path(), FILE);
        assert!(file.contains(r#"Failed("%USERPROFILE%\\AppData"#) && !file.contains("Kowalski"), "{file}");
    }

    #[test]
    fn two_writers_of_one_file_share_one_history() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (LogFile::new(dir.path(), 100), LogFile::new(dir.path(), 100));
        let line = |n: u32| format!("{} {n:02} of two writers\n", if n.is_multiple_of(2) { "first " } else { "second" });
        for n in 0..40_u32 {
            (if n.is_multiple_of(2) { &first } else { &second }).write_line(&line(n));
        }
        // 25 bytes a line: four fit in 100. Neither writer rotates the other's lines away.
        assert_eq!(read(dir.path(), FILE), (36..40).map(line).collect::<String>());
        assert_eq!(read(dir.path(), OLD_FILE), (32..36).map(line).collect::<String>());
    }

    #[test]
    fn module_text_is_one_line_without_escapes() {
        assert_eq!(clean("first\nsecond\r\n\tthird \u{1b}[31mred\u{202e}txt\u{2028}x"), "first second   third  [31mred txt x");
    }

    #[test]
    fn ids_tokens_and_addresses_are_redacted() {
        for secret in [
            "72f988bf-86f1-41af-91ab-2d7cd011db47",
            "abcdefab-cdef-abcd-efab-cdefabcdefab",
            "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.eyJhdWQiOiJodHRwczovL2FwaS5zcGFjZXMuc2t5cGUuY29tIn0.c2ln",
            "19:0a1b2c3d@thread.v2",
            "19:meeting_NjJkZmQ1@thread.v2",
            "8:orgid:72f988bf-86f1-41af-91ab-2d7cd011db47",
            "someone@example.com",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations",
            "0.AXoAv4j5cvGGr0GRqy180BHbR1sO3sGQ1",
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
            "+12025550123",
            "8:live.com:jkowalski",
            "28:bot",
            "1700000000123",
        ] {
            assert_eq!(clean(&format!("before {secret} after")), "before <redacted> after", "{secret}");
        }
        assert_eq!(clean("token=eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9"), "token=<redacted>");
    }

    #[test]
    fn codes_counts_and_states_stay() {
        for text in [
            "live captions: join HTTP 403",
            "Teams ended the call (code 4097, sub-code 5012)",
            "a join failed outcome=Transient(ModuleFailed)",
            "polling failed module=teams crashes=2",
            "nie udało się połączyć: odmowa dostępu (os error 5)",
            "the daemon started version=0.2.0",
            "the call ended at 10:40 (line 3: x)",
        ] {
            assert_eq!(clean(text), text);
        }
    }

    #[test]
    fn a_long_line_is_cut_on_a_character() {
        let line = clean(&"zażółć ".repeat(1000));
        assert!(line.len() <= MAX_LINE + 3, "{}", line.len());
        assert!(line.ends_with("..."));
        assert!(line.starts_with("zażółć zażółć "));
    }
}
