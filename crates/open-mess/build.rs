//! On Windows, tauri-build embeds the icon and the application manifest into `om.exe`, and turns
//! `capabilities/main.json` into the list of commands the configuration window may call.

/// The configuration window's commands (src/ui/window.rs); each gets an `allow-<name>` permission.
#[cfg(windows)]
const COMMANDS: &[&str] = &[
    "status",
    "accounts",
    "remove_account",
    "modules",
    "approve_module",
    "set_module_enabled",
    "rescan_modules",
    "mcp_command",
    "permissions",
    "set_permissions",
    "clear_permission_override",
    "waiting",
    "decide",
    "sign_in_start",
    "sign_in_request",
    "sign_in_answer",
    "sign_in_open_link",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)))?;
    Ok(())
}
