//! First-run configuration, migration from the old Python program, and the
//! "start with Windows" shortcut.

use anyhow::Result;
use cv_core::config::{parse_legacy_configs, Config};
use cv_core::paths;
use std::path::{Path, PathBuf};

/// Load config.toml, preserving selected folders if their drives are offline.
pub fn load_or_first_run() -> Config {
    let path = paths::config_path();
    let cfg = Config::load_or_default();
    if !path.exists() {
        if let Err(e) = cfg.save() {
            tracing::error!("could not save config: {e:#}");
        }
    }
    let _ = std::fs::create_dir_all(&cfg.recording.folder);
    let _ = std::fs::create_dir_all(&cfg.clips.folder);
    cfg
}

/// Import settings from the old Python program's folder (configs.txt).
pub fn migrate_legacy(legacy_dir: &Path) -> Result<Config> {
    let mut cfg = if paths::config_path().exists() { Config::load()? } else { Config::default() };
    let configs = legacy_dir.join("configs.txt");
    if configs.is_file() {
        let legacy = parse_legacy_configs(&std::fs::read_to_string(&configs)?);
        // The old program counted 60-minute recordings.
        cfg.apply_legacy(&legacy, 60);
    }
    cfg.fix_missing_drives();
    cfg.save()?;
    Ok(cfg)
}

fn service_exe() -> PathBuf {
    paths::exe_dir().join(paths::SERVICE_EXE)
}

/// Create or remove the Startup-folder shortcut for the service.
pub fn sync_startup_shortcut(cfg: &Config) -> Result<()> {
    let Some(dir) = paths::startup_dir() else { return Ok(()) };
    let link = dir.join(paths::STARTUP_LINK_NAME);
    // Never register a development build (cargo's target dir) at login.
    let exe_dir = paths::exe_dir().to_string_lossy().to_lowercase();
    if exe_dir.contains("\\target\\debug") || exe_dir.contains("\\target\\release") {
        tracing::debug!("dev build: not touching the startup shortcut");
        return Ok(());
    }
    if cfg.general.start_with_windows {
        let exe = service_exe();
        if exe.is_file() {
            crate::win::shell::create_shortcut(&link, &exe, "", &paths::exe_dir(), "ClipVault background recorder")?;
        }
    } else if link.exists() {
        std::fs::remove_file(&link)?;
    }
    Ok(())
}

/// Create Start Menu + Desktop shortcuts for the GUI.
pub fn create_app_shortcuts(desktop: bool) -> Result<()> {
    let gui = paths::exe_dir().join(paths::GUI_EXE);
    if let Some(sm) = paths::start_menu_dir() {
        crate::win::shell::create_shortcut(&sm.join("ClipVault.lnk"), &gui, "", &paths::exe_dir(), "ClipVault — clips and 24/7 recordings")?;
    }
    if desktop {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let candidates = [PathBuf::from(&home).join("OneDrive").join("Desktop"), PathBuf::from(&home).join("Desktop")];
            if let Some(d) = candidates.iter().find(|d| d.is_dir()) {
                crate::win::shell::create_shortcut(&d.join("ClipVault.lnk"), &gui, "", &paths::exe_dir(), "ClipVault — clips and 24/7 recordings")?;
            }
        }
    }
    Ok(())
}
