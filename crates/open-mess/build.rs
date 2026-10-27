//! On Windows, tauri-build embeds the icon and the application manifest into `om.exe`, and turns
//! `capabilities/main.json` into the list of commands the configuration window may call.

/// The configuration window's commands (src/ui/window.rs); each gets an `allow-<name>` permission.
#[cfg(windows)]
const COMMANDS: &[&str] = &[];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)))?;
    Ok(())
}
