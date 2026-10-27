//! A test helper the trigger tests start as a rule's program (tests/spawn.rs): writes what it got
//! (its arguments, its stdin, the path in `OPEN_MESS_EVENT_FILE` and that file's content) as JSON
//! to the path in its first argument. With the arguments `--sleep <ms>` it then sleeps, and writes
//! `<path>.done` when it wakes.

use std::io::Read as _;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = args.first().ok_or_else(|| std::io::Error::other("usage: open-mess-record-run <output path> [--sleep <ms>]"))?;
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let event_file = std::env::var("OPEN_MESS_EVENT_FILE").ok();
    let event = event_file.as_ref().and_then(|path| std::fs::read_to_string(path).ok());
    let record = serde_json::json!({
        "args": args,
        "stdin": stdin,
        "event_file": event_file,
        "event": event,
        "cwd": std::env::current_dir()?,
    });
    // Renamed into place, so a test never reads half a file.
    let partial = format!("{out}.partial");
    std::fs::write(&partial, record.to_string())?;
    std::fs::rename(&partial, out)?;
    let sleep = args.iter().skip_while(|arg| *arg != "--sleep").nth(1).and_then(|ms| ms.parse().ok());
    if let Some(ms) = sleep {
        std::thread::sleep(Duration::from_millis(ms));
        std::fs::write(format!("{out}.done"), "")?;
    }
    Ok(())
}
