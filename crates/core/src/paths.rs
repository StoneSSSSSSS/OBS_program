//! Well-known file locations.
//!
//! All mutable app state lives in `%LOCALAPPDATA%\ClipVault` (override with the
//! `CLIPVAULT_DATA_DIR` environment variable, used by tests and portable setups).

use std::path::{Path, PathBuf};

pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLIPVAULT_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(crate::APP_NAME)
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn db_path() -> PathBuf {
    data_dir().join("library.db")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

pub fn temp_dir() -> PathBuf {
    data_dir().join("tmp")
}

/// Written by the service; tells the GUI which port/token to use.
pub fn ipc_info_path() -> PathBuf {
    data_dir().join("ipc.json")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [data_dir(), logs_dir(), thumbs_dir(), temp_dir()] {
        std::fs::create_dir_all(d)?;
    }
    Ok(())
}

/// Directory containing the running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Look for a resource shipped next to the executable (or in `assets/` beside it,
/// or — during development — in the workspace `assets/` folder).
pub fn find_resource(name: &str) -> Option<PathBuf> {
    let exe = exe_dir();
    let mut candidates = vec![exe.join(name), exe.join("assets").join(name)];
    // target/{debug,release}/ -> workspace root
    if let Some(root) = exe.parent().and_then(Path::parent) {
        candidates.push(root.join("assets").join(name));
        candidates.push(root.join("third_party").join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// OBS's configuration directory (`%APPDATA%\obs-studio` unless overridden).
pub fn obs_config_dir(override_dir: &str) -> PathBuf {
    if !override_dir.trim().is_empty() {
        return PathBuf::from(override_dir.trim());
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("obs-studio")
}

/// The user's Startup folder (for the "start with Windows" shortcut).
pub fn startup_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    })
}

pub fn start_menu_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
}

pub const SERVICE_EXE: &str = "clipvault-service.exe";
pub const GUI_EXE: &str = "clipvault.exe";
pub const STARTUP_LINK_NAME: &str = "ClipVault Service.lnk";
