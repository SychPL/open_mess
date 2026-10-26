//! `om daemon`: the background process that serves the local API.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use crate::instance::InstanceLock;
use crate::store::Store;
use crate::tools::Registry;
use crate::{Error, api, paths, secrets};

/// Why the last `om daemon` failed to start, one line, in the data folder. The CLI shows it when
/// it can't reach the daemon; a successful start deletes it.
pub const LAST_START_ERROR_FILE: &str = "last-start-error.txt";

/// Runs the daemon until Ctrl+C.
pub fn run() -> Result<(), Error> {
    let started = Instant::now();
    let dir = paths::data_dir()?;
    // A second daemon fails here and leaves the running one's error file alone.
    let _lock = InstanceLock::acquire(&dir)?;
    let error_file = dir.join(LAST_START_ERROR_FILE);
    let (runtime, listener, token) = start(&dir).inspect_err(|err| {
        let _ = std::fs::write(&error_file, format!("{}\n", err.to_string().replace(['\r', '\n'], " ")));
    })?;
    let _ = std::fs::remove_file(&error_file);
    // Never println!: it panics when the launcher has closed stdout.
    let _ = writeln!(std::io::stdout(), "open_mess daemon listening on http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    runtime.block_on(api::serve(listener, token, Arc::new(Registry::with_builtin(started)), ctrl_c()))
}

fn start(dir: &Path) -> Result<(Runtime, TcpListener, String), Error> {
    let store = Store::open(&dir.join("state.sqlite3"))?;
    let port = store.port()?;
    let token = secrets::local_api_token()?;
    let runtime = Runtime::new()?;
    let listener = runtime.block_on(api::bind(port))?;
    Ok((runtime, listener, token))
}

async fn ctrl_c() {
    // Without a Ctrl+C handler the daemon runs until it is killed, rather than stopping at once.
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}
