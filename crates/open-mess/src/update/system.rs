//! The Windows side of an update (spec 2026-10-27-self-update, 7): the scheduled task
//! `open_mess-update` that runs the helper. Its own task gives the helper its own job object, so it
//! outlives the daemon and can start the daemon's task `open_mess` again. Tests never come here;
//! they use fakes.

use std::ffi::OsStr;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::UpdateError;
use super::quiesce::Launcher;
use crate::triggers::spawn::CREATE_NO_WINDOW;

/// The helper's scheduled task.
pub const TASK: &str = "open_mess-update";

/// Task Scheduler for real.
pub struct WindowsSystem;

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

fn schtasks(args: &[&OsStr]) -> Result<(), UpdateError> {
    let system = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let status = Command::new(system.join("System32").join("schtasks.exe"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() { Ok(()) } else { Err(UpdateError::Io(std::io::Error::other(format!("schtasks failed: {status}")))) }
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
