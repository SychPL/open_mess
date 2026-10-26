//! Where open_mess keeps its files.

use std::path::PathBuf;

use crate::Error;

/// The data folder: `OPEN_MESS_DATA_DIR` if set, else `%APPDATA%\open_mess`. Created if missing.
pub fn data_dir() -> Result<PathBuf, Error> {
    let dir = match std::env::var_os("OPEN_MESS_DATA_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("APPDATA").filter(|dir| !dir.is_empty()).ok_or(Error::NoDataDir)?).join("open_mess"),
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
