//! Action 2 (spec 8.7): the rule's program started for a batch. It is started directly with the
//! rule's fixed arguments, never through a shell, and nothing from a message reaches its command
//! line (R1): the batch goes to its stdin as JSON and to a file in `<data_dir>/runs/` whose path is
//! in `OPEN_MESS_EVENT_FILE`. The file is deleted when the run ends, and leftovers of a daemon that
//! stopped are deleted at start (R5, R6). The program reaches open_mess through MCP or the CLI like
//! any other caller, under the same permissions (spec 8.6).
//!
//! Limits (spec 15): one run per rule and [`MAX_RUNS`] in all; more batches wait, [`MAX_QUEUE`]
//! per rule, the oldest dropped first; a run is stopped after the rule's run timeout.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::process::{Child, Command};

use super::engine::{Batch, DeliveryResult};
use super::payload;
use super::rules::Spawn;

/// Programs running at once, over all rules.
pub const MAX_RUNS: usize = 2;
/// Batches waiting per rule.
pub const MAX_QUEUE: usize = 10;
/// A run still going after this is stopped, with the processes it started.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The environment variable with the event file's path.
pub const EVENT_FILE_VAR: &str = "OPEN_MESS_EVENT_FILE";

/// The runs of the daemon: what runs and what waits.
pub struct Runs {
    dir: PathBuf,
    timeout: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Rules with a program running.
    running: HashSet<String>,
    /// Batches waiting, oldest first.
    queued: VecDeque<Queued>,
}

struct Queued {
    rule: String,
    spawn: Spawn,
    input: Vec<u8>,
}

impl Runs {
    /// Runs with their event files in `dir` (`<data_dir>/runs`), stopped after `timeout`. Files
    /// left there by a daemon that stopped are deleted.
    pub fn new(dir: PathBuf, timeout: Duration) -> Arc<Runs> {
        // Best effort: a file that can't be deleted now is tried again at the next start.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Arc::new(Runs { dir, timeout, state: Mutex::new(State::default()) })
    }

    /// Programs running now.
    pub fn running(&self) -> usize {
        self.lock().running.len()
    }

    /// Batches waiting for a run.
    pub fn queued(&self) -> usize {
        self.lock().queued.len()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends a run of `rule` and takes the oldest waiting batch that may run now, if any.
    fn finished(&self, rule: &str) -> Option<Queued> {
        let mut state = self.lock();
        state.running.remove(rule);
        let next = state.queued.iter().position(|queued| !state.running.contains(&queued.rule))?;
        let next = state.queued.remove(next)?;
        state.running.insert(next.rule.clone());
        Some(next)
    }
}

/// Starts `spawn` for the batch of `rule`, or queues the batch when the rule's program is already
/// running or [`MAX_RUNS`] are.
pub fn deliver(runs: &Arc<Runs>, rule: &str, spawn: &Spawn, batch: &Batch) -> DeliveryResult {
    let input = serde_json::to_vec(&payload(rule, batch)).unwrap_or_default();
    let queued = Queued { rule: rule.to_owned(), spawn: spawn.clone(), input };
    let mut state = runs.lock();
    if state.running.contains(rule) || state.running.len() >= MAX_RUNS {
        let mut result = DeliveryResult::Delivered;
        if state.queued.iter().filter(|waiting| waiting.rule == rule).count() >= MAX_QUEUE
            && let Some(oldest) = state.queued.iter().position(|waiting| waiting.rule == rule)
        {
            state.queued.remove(oldest);
            result = DeliveryResult::DeliveredDroppingOldest;
        }
        state.queued.push_back(queued);
        return result;
    }
    state.running.insert(rule.to_owned());
    drop(state);
    tokio::spawn(run_all(Arc::clone(runs), queued));
    DeliveryResult::Delivered
}

/// Runs the batch, then whatever waited for the slot it frees.
async fn run_all(runs: Arc<Runs>, first: Queued) {
    let mut next = Some(first);
    while let Some(run) = next {
        // ponytail: a program that fails to start is not reported anywhere until a log sink exists.
        let _ = run_one(&runs, &run.spawn, &run.input).await;
        next = runs.finished(&run.rule);
    }
}

async fn run_one(runs: &Runs, spawn: &Spawn, input: &[u8]) -> std::io::Result<()> {
    let event_file = write_event_file(&runs.dir, input)?;
    let result = async {
        let mut command = Command::new(&spawn.program);
        command.args(&spawn.args).env(EVENT_FILE_VAR, &event_file).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        command.kill_on_drop(true);
        if let Some(dir) = &spawn.working_dir {
            command.current_dir(dir);
        }
        // The daemon has no console; without this, a console program would open a window.
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let write = async {
            if let Some(mut stdin) = stdin {
                // A program that exits without reading its input is fine.
                let _ = stdin.write_all(input).await;
            }
        };
        match tokio::time::timeout(runs.timeout, async { tokio::join!(write, child.wait()).1 }).await {
            Ok(status) => status.map(drop),
            Err(_) => {
                kill_tree(&mut child).await;
                Ok(())
            }
        }
    }
    .await;
    let _ = std::fs::remove_file(&event_file);
    result
}

/// Writes the batch to a new file with a random name in `dir`.
fn write_event_file(dir: &Path, input: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut name = [0u8; 16];
    getrandom::fill(&mut name).map_err(|err| std::io::Error::other(err.to_string()))?;
    let name: String = name.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = dir.join(format!("{name}.json"));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    if let Err(err) = std::io::Write::write_all(&mut file, input) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    Ok(path)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Stops the program and the processes it started: `claude -p` and the like run children, which
/// `Child::kill` alone would leave running.
#[cfg(windows)]
async fn kill_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        // ponytail: taskkill instead of a job object, which needs unsafe Windows calls; children whose
        // parent already exited are not found by it.
        let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let _ = Command::new(system.join("System32").join("taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .await;
    }
    let _ = child.kill().await;
}

/// Stops the program.
// ponytail: the process only; a process group kill when trigger runs matter off Windows.
#[cfg(not(windows))]
async fn kill_tree(child: &mut Child) {
    let _ = child.kill().await;
}
