//! The Windows side of an update (spec 2026-10-27-self-update, 7): the scheduled task
//! `open_mess-update` that runs the helper, and what the helper does to the daemon: stop it, start
//! it and ask how it is. Its own task gives the helper its own job object, so it outlives the
//! daemon and can start the daemon's task `open_mess` again. Tests never come here; they use fakes.

use std::ffi::OsStr;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::UpdateError;
use super::apply::{Health, Judgement, System, judge};
use super::quiesce::Launcher;
use crate::instance::InstanceLock;
use crate::triggers::spawn::CREATE_NO_WINDOW;
use crate::{cli, secrets, store};

/// The helper's scheduled task.
pub const TASK: &str = "open_mess-update";
/// The daemon's scheduled task (`scripts/install.ps1`).
const DAEMON_TASK: &str = "open_mess";
/// How often the new daemon is asked for its `status`.
const POLL: Duration = Duration::from_secs(1);
/// One `status` answer; the daemon gives it at once.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// What the new daemon gets to come up, its account restore aside (spec 7.2).
const READY_WITHIN: Duration = Duration::from_secs(60);
/// The account restore's own limit, from the start of the check: fixed, since the modules' timers
/// run between the accounts (spec 7.2).
const ACCOUNTS_WITHIN: Duration = Duration::from_secs(10 * 60);
/// Ends what `schtasks /End` leaves: every `om.exe` of this user whose command line says `daemon`,
/// from any folder, with the filter of `scripts/install.ps1` (`Stop-Daemon`): `daemon` must be the
/// first argument after the program, so a bridge whose path has the word survives. Fixed text:
/// nothing from outside goes into it, and no double quote (`"` in the pattern) that the command
/// line would have to escape.
const STOP_DAEMONS: &str = r"Get-CimInstance Win32_Process -Filter 'Name=''om.exe''' | Where-Object { $_.CommandLine -match '^\s*(\x22[^\x22]*\x22|\S+)\s+daemon(\s|$)' -and (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq $env:USERNAME } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";

/// Task Scheduler and the daemon for real.
pub struct WindowsSystem {
    /// The daemon's data folder, for its instance lock, port and `status`: the helper's
    /// `--data-dir`. The launcher gets its own.
    pub data: PathBuf,
}

impl Launcher for WindowsSystem {
    /// Registers the task again, with this helper and data folder, and runs it.
    fn launch_helper(&self, helper: &Path, data: &Path) -> Result<(), UpdateError> {
        let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            (Ok(domain), Ok(name)) => format!("{domain}\\{name}"),
            _ => return Err(UpdateError::Io(std::io::Error::other("USERDOMAIN or USERNAME is not set"))),
        };
        let start = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let file = data.join("update-task.xml");
        std::fs::write(&file, utf16_with_bom(&task_xml(helper, data, &user, &start)))?;
        let created = schtasks(&[OsStr::new("/Create"), OsStr::new("/XML"), file.as_os_str(), OsStr::new("/TN"), OsStr::new(TASK), OsStr::new("/F")]);
        let _ = std::fs::remove_file(&file);
        created?;
        schtasks(&[OsStr::new("/Run"), OsStr::new("/TN"), OsStr::new(TASK)])
    }
}

impl System for WindowsSystem {
    fn wait_daemon_lock_free(&self, wait: Duration) -> bool {
        let started = Instant::now();
        loop {
            // Taken for an instant and let go; a daemon starting meanwhile retries (daemon.rs).
            if InstanceLock::acquire(&self.data).is_ok() {
                return true;
            }
            if started.elapsed() >= wait {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop_daemon(&self) -> Result<(), UpdateError> {
        // Fails when the task does not run; its processes are looked for all the same.
        let _ = schtasks(&[OsStr::new("/End"), OsStr::new("/TN"), OsStr::new(DAEMON_TASK)]);
        let powershell = system32().join("WindowsPowerShell").join("v1.0").join("powershell.exe");
        hidden(&powershell, &["-NoProfile", "-NonInteractive", "-Command", STOP_DAEMONS].map(OsStr::new))
    }

    fn start_daemon(&self) -> Result<(), UpdateError> {
        schtasks(&[OsStr::new("/Run"), OsStr::new("/TN"), OsStr::new(DAEMON_TASK)])
    }

    fn health(&self, version: &str, active_modules: &[String]) -> Health {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime.block_on(self.watch(version, active_modules)),
            Err(err) => Health::Failed(format!("cannot ask the new daemon: {err}")),
        }
    }
}

impl WindowsSystem {
    /// Asks the new daemon for its `status` every second until [`judge`] decides or a limit passes.
    async fn watch(&self, version: &str, active_modules: &[String]) -> Health {
        let started = Instant::now();
        loop {
            let (limit, why) = match self.status().await {
                Ok(status) => match judge(&status, version, active_modules) {
                    Judgement::Healthy => return Health::Healthy,
                    Judgement::Failed(why) => return Health::Failed(why),
                    Judgement::Pending { accounts_only: true } => (ACCOUNTS_WITHIN, "the accounts were not restored within 10 minutes".to_owned()),
                    Judgement::Pending { accounts_only: false } => (READY_WITHIN, "the window or a module was not ready within 60 s".to_owned()),
                    Judgement::OtherVersion(seen) => (READY_WITHIN, format!("version {seen} answered instead of {version} for 60 s")),
                },
                Err(err) => (READY_WITHIN, format!("the new daemon did not answer within 60 s: {err}")),
            };
            if started.elapsed() >= limit {
                return Health::Failed(why);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The daemon's `status` over its local API, asked as the CLI asks it.
    async fn status(&self) -> Result<Value, String> {
        let port = store::read_port(&self.data.join("state.sqlite3")).map_err(|err| err.to_string())?;
        let token = secrets::stored_local_api_token().map_err(|err| err.to_string())?.ok_or_else(|| "no local API token".to_owned())?;
        let response = cli::local_client()
            .map_err(|err| err.to_string())?
            .post(format!("http://127.0.0.1:{port}/api/call/status"))
            .bearer_auth(token)
            .json(&json!({}))
            .timeout(STATUS_TIMEOUT)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response.json().await.map_err(|err| err.to_string())
    }
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("System32")
}

fn schtasks(args: &[&OsStr]) -> Result<(), UpdateError> {
    hidden(&system32().join("schtasks.exe"), args)
}

/// Runs a program without a window and waits for it.
fn hidden(program: &Path, args: &[&OsStr]) -> Result<(), UpdateError> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() { Ok(()) } else { Err(UpdateError::Io(std::io::Error::other(format!("{} failed: {status}", program.display())))) }
}

/// The task, Task Scheduler schema 1.2: the helper hidden behind `conhost --headless` (as
/// `scripts/install.ps1` starts the daemon), at logon and every 5 minutes (recovery, spec 7.6),
/// never two at once, with no time limit.
fn task_xml(helper: &Path, data: &Path, user: &str, start: &str) -> String {
    let arguments = escape(&format!("--headless {} apply-update --data-dir {}", quote(helper), quote(data)));
    let working = escape(&helper.parent().map(|dir| dir.display().to_string()).unwrap_or_default());
    let user = escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>open_mess: installs an update the user accepted, or finishes an interrupted one.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
    <TimeTrigger>
      <Repetition>
        <Interval>PT5M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>{start}</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>conhost.exe</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// A path as one command-line argument. Backslashes before the closing quote are doubled, or the
/// last one would escape the quote (`"D:\data\"`).
fn quote(path: &Path) -> String {
    let text = path.display().to_string();
    let trailing = text.len() - text.trim_end_matches('\\').len();
    format!("\"{text}{}\"", "\\".repeat(trailing))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// What `schtasks /XML` reads: UTF-16 little-endian with a byte order mark.
fn utf16_with_bom(text: &str) -> Vec<u8> {
    [0xFEFF].into_iter().chain(text.encode_utf16()).flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_the_helper_hidden_with_the_data_folder() {
        let xml = task_xml(
            Path::new(r"C:\Users\A & B\AppData\Local\open_mess\bin\om.update.exe"),
            Path::new(r"D:\data\"),
            r"PC\a&b",
            "2026-10-27T10:00:00",
        );
        assert!(xml.contains(
            r#"<Arguments>--headless "C:\Users\A &amp; B\AppData\Local\open_mess\bin\om.update.exe" apply-update --data-dir "D:\data\\"</Arguments>"#
        ));
        assert!(xml.contains(r"<WorkingDirectory>C:\Users\A &amp; B\AppData\Local\open_mess\bin</WorkingDirectory>"));
        assert_eq!(xml.matches(r"<UserId>PC\a&amp;b</UserId>").count(), 2);
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>") && xml.contains("<Interval>PT5M</Interval>"));
        let bytes = utf16_with_bom("<T>");
        assert_eq!(bytes, [0xFF, 0xFE, b'<', 0, b'T', 0, b'>', 0]);
    }
}
