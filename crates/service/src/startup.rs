//! First-run configuration, migration from the old Python program, and the
//! "start with Windows" shortcut.

use crate::win::coreaudio::{com_init_mta, AudioSystem};
use anyhow::Result;
use cv_core::audio_names::clean_display_name;
use cv_core::config::{parse_legacy_configs, Config};
use cv_core::obs::files::ObsConfigFiles;
use cv_core::paths;
use std::path::{Path, PathBuf};

/// Load config.toml, or build one from the current OBS setup on first run.
pub fn load_or_first_run() -> Config {
    let path = paths::config_path();
    if path.exists() {
        return Config::load_or_default();
    }
    let mut cfg = Config::default();
    seed_from_obs(&mut cfg);
    if let Err(e) = cfg.save() {
        tracing::error!("could not save initial config: {e:#}");
    }
    cfg
}

/// Copy the user's existing OBS output settings into our config so nothing
/// changes unexpectedly (quality, replay length, recordings folder...).
pub fn seed_from_obs(cfg: &mut Config) {
    let files = ObsConfigFiles::from_config(cfg);
    if let Ok(s) = files.read_profile_summary() {
        if let Some(p) = s.rec_path.filter(|p| !p.is_empty()) {
            cfg.recording.folder = p.clone();
            if let Some(parent) = Path::new(&p).parent() {
                cfg.clips.folder = parent.join("Clips").to_string_lossy().to_string();
            }
        }
        if let Some(v) = s.replay_secs {
            cfg.clips.clip_length_secs = v;
        }
        if let Some(v) = s.replay_mb {
            cfg.clips.replay_max_memory_mb = v;
        }
        if let Some(v) = s.fps {
            cfg.recording.fps = v;
        }
        if let Some(v) = s.encoder {
            cfg.recording.encoder = v;
        }
        if let Some(v) = s.rate_control {
            cfg.recording.rate_control = v.to_uppercase();
        }
        if let Some(v) = s.cqp {
            cfg.recording.cqp = v;
        }
        if let Some(v) = s.bitrate {
            cfg.recording.bitrate_kbps = v;
        }
        if let Some(v) = s.preset {
            cfg.recording.preset = v;
        }
        if let Some(v) = s.audio_bitrate {
            cfg.recording.audio_bitrate_kbps = v;
        }
        if let Some(f) = s.format {
            if cv_core::config::RECORDING_FORMATS.iter().any(|(k, _)| *k == f) {
                cfg.recording.format = f;
            }
        }
    }
    if let Some(mic) = current_obs_mic_name(&files) {
        tracing::info!("preferred mic from OBS: {mic}");
        cfg.audio.mic_priority = vec![mic];
    }
    cfg.sanitize();
}

/// Name of the microphone OBS currently records (resolved through Core Audio).
fn current_obs_mic_name(files: &ObsConfigFiles) -> Option<String> {
    let name = files.scene_collection_name().unwrap_or_else(|| "Untitled".into());
    let file = files.dir.join("basic").join("scenes").join(format!("{name}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let mut ids = Vec::new();
    for k in ["AuxAudioDevice1", "AuxAudioDevice2"] {
        if let Some(id) = v[k]["settings"]["device_id"].as_str() {
            ids.push(id.to_string());
        }
    }
    if let Some(sources) = v["sources"].as_array() {
        for s in sources {
            if s["id"].as_str() == Some("wasapi_input_capture") {
                if let Some(id) = s["settings"]["device_id"].as_str() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    let ids: Vec<String> = ids.into_iter().filter(|i| i != "default" && !i.is_empty()).collect();
    if ids.is_empty() {
        return None;
    }
    // Core Audio on a throwaway MTA thread.
    std::thread::spawn(move || {
        com_init_mta();
        let sys = AudioSystem::new(None).ok()?;
        let inputs = sys.inputs();
        ids.iter()
            .find_map(|id| inputs.iter().find(|e| e.id.eq_ignore_ascii_case(id)))
            .map(|e| clean_display_name(&e.name))
    })
    .join()
    .ok()
    .flatten()
}

/// Import settings from the old Python program's folder (configs.txt).
pub fn migrate_legacy(legacy_dir: &Path) -> Result<Config> {
    let mut cfg = if paths::config_path().exists() { Config::load()? } else { Config::default() };
    seed_from_obs(&mut cfg);
    let configs = legacy_dir.join("configs.txt");
    if configs.is_file() {
        let legacy = parse_legacy_configs(&std::fs::read_to_string(&configs)?);
        // The old program used OBS's split length (60 min) to count recordings.
        let split = ObsConfigFiles::from_config(&cfg)
            .read_profile_summary()
            .ok()
            .and_then(|s| s.split_minutes)
            .unwrap_or(60);
        cfg.apply_legacy(&legacy, split);
    }
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
            crate::win::shell::create_shortcut(&link, &exe, "", &paths::exe_dir(), "ClipVault background service (keeps OBS recording)")?;
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
