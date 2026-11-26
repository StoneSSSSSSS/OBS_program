//! ClipVault background service.
//!
//! Started at login (Startup folder). Launches and supervises OBS, keeps the
//! 24/7 recording and replay buffer running, owns the clip hotkeys, keeps the
//! microphone pointed at the right device, files clips by game and enforces
//! recording retention. Shows only a tray icon; the GUI (clipvault.exe) talks
//! to it over a local socket.

#![windows_subsystem = "windows"]

mod activity;
mod audio_guard;
mod clips;
mod ipc_server;
mod jobs;
mod library;
mod obs_setup;
mod shared;
mod startup;
mod status;
mod supervisor;
mod win;

use crate::shared::{LibCmd, Shared, SupCmd};
use crate::win::ui::{self, Action, ToastLevel, UiCmd};
use anyhow::Result;
use cv_core::config::Config;
use cv_core::ipc::Event;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        attach_console();
        let code = match cli(&args[1..]) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        };
        std::process::exit(code);
    }
    if let Err(e) = run_service() {
        tracing::error!("service failed: {e:#}");
        std::process::exit(1);
    }
}

fn attach_console() {
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

/// Command-line maintenance commands (used by the installer and for diagnostics).
fn cli(args: &[String]) -> Result<()> {
    cv_core::paths::ensure_dirs()?;
    match args[0].as_str() {
        "--version" => println!("ClipVault service {}", cv_core::VERSION),
        "--migrate" => {
            let dir = args.get(1).map(std::path::PathBuf::from).ok_or_else(|| anyhow::anyhow!("--migrate <old program folder>"))?;
            let cfg = startup::migrate_legacy(&dir)?;
            println!("config written to {}", cv_core::paths::config_path().display());
            println!("recordings: {}  clips: {}  retention: {}h  replay: {}s  mic: {:?}", cfg.recording.folder, cfg.clips.folder, cfg.recording.retention_hours, cfg.clips.clip_length_secs, cfg.audio.mic_priority);
        }
        "--install-shortcuts" => {
            let cfg = startup::load_or_first_run();
            startup::sync_startup_shortcut(&cfg)?;
            startup::create_app_shortcuts(args.iter().any(|a| a == "--desktop"))?;
            println!("shortcuts created");
        }
        "--remove-startup" => {
            let mut cfg = Config::load_or_default();
            cfg.general.start_with_windows = false;
            startup::sync_startup_shortcut(&cfg)?;
        }
        "--diagnose" => diagnose()?,
        "--stop" => {
            let (client, _) = cv_core::ipc::IpcClient::connect(Duration::from_secs(3))?;
            client.call(cv_core::ipc::Request::Shutdown, Duration::from_secs(5))?;
            println!("service stopping");
        }
        other => anyhow::bail!("unknown option {other}"),
    }
    Ok(())
}

fn diagnose() -> Result<()> {
    let cfg = startup::load_or_first_run();
    let devices = std::thread::spawn(|| {
        win::coreaudio::com_init_mta();
        let sys = win::coreaudio::AudioSystem::new(None).ok()?;
        Some((sys.inputs(), sys.outputs(), sys.default_endpoint(true), sys.default_endpoint(false), sys.sessions_by_output()))
    })
    .join()
    .ok()
    .flatten();
    let files = cv_core::obs::files::ObsConfigFiles::from_config(&cfg);
    let ws = files.read_websocket();
    let procs = win::process::list_processes();
    let names: std::collections::HashMap<u32, String> = procs.iter().map(|p| (p.pid, p.exe_name.clone())).collect();
    let out = serde_json::json!({
        "config": cv_core::paths::config_path(),
        "obs_running": win::process::find_by_exe(std::path::Path::new(&cfg.obs.exe_path)),
        "websocket": {"enabled": ws.enabled, "port": ws.port, "has_password": !ws.password.is_empty()},
        "profile": format!("{:?}", files.read_profile_summary().ok()),
        "settings_pending": files.apply_settings(&cfg, true).ok(),
        "ffmpeg": cv_core::ffmpeg::locate(&cfg.advanced.ffmpeg_path, &cfg.advanced.ffprobe_path).map(|t| t.ffmpeg.display().to_string()).unwrap_or_else(|e| e.to_string()),
        "mic_priority": cfg.audio.mic_priority,
        "inputs": devices.as_ref().map(|d| &d.0),
        "outputs": devices.as_ref().map(|d| &d.1),
        "default_input": devices.as_ref().and_then(|d| d.2.clone()),
        "default_output": devices.as_ref().and_then(|d| d.3.clone()),
        "sessions": devices.as_ref().map(|d| d.4.iter().map(|(k, v)| (k.clone(), v.iter().map(|p| names.get(p).cloned().unwrap_or_default()).collect::<Vec<_>>())).collect::<std::collections::HashMap<_, _>>()),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn run_service() -> Result<()> {
    cv_core::paths::ensure_dirs()?;
    let cfg = startup::load_or_first_run();
    let _log = cv_core::logging::init("service", &cfg.advanced.log_level, cfg.advanced.log_retention_days);
    let Some(_instance) = win::process::SingleInstance::acquire("Local\\ClipVaultService") else {
        tracing::info!("another ClipVault service is already running; exiting");
        return Ok(());
    };
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    tracing::info!("ClipVault service {} starting", cv_core::VERSION);
    if let Err(e) = startup::sync_startup_shortcut(&cfg) {
        tracing::warn!("startup shortcut: {e:#}");
    }

    let cfg = Arc::new(RwLock::new(cfg));
    let (ui_handle, ui_rx) = ui::channel();
    let (sup_tx, sup_rx) = crossbeam_channel::unbounded();
    let (clip_tx, clip_rx) = crossbeam_channel::unbounded();
    let (job_tx, job_rx) = crossbeam_channel::unbounded();
    let (lib_tx, lib_rx) = crossbeam_channel::unbounded();
    let (audio_tx, audio_rx) = crossbeam_channel::bounded(4);
    let (action_tx, action_rx) = crossbeam_channel::unbounded();
    let hotkey_errors = Arc::new(Mutex::new(Vec::new()));
    let sh = Arc::new(Shared {
        cfg: cfg.clone(),
        status: Mutex::new(cv_core::ipc::Status { service_started_ms: cv_core::util::now_ms(), ..Default::default() }),
        ui: ui_handle.clone(),
        sup_tx,
        clip_tx,
        job_tx,
        lib_tx,
        audio_tx,
        clients: Mutex::new(Vec::new()),
        tools: RwLock::new(None),
        hotkey_errors: hotkey_errors.clone(),
        devices: Mutex::new(Default::default()),
        jobs: Mutex::new(Vec::new()),
        event_db: Mutex::new(None),
        library_lock: Mutex::new(()),
        shutdown: std::sync::atomic::AtomicBool::new(false),
    });
    sh.log_event("info", format!("ClipVault service {} started", cv_core::VERSION));

    spawn("supervisor", { let sh = sh.clone(); move || supervisor::Supervisor::new(sh, sup_rx).run() });
    spawn("activity", { let sh = sh.clone(); move || activity::run(sh) });
    spawn("audio-guard", { let sh = sh.clone(); move || audio_guard::run(sh, audio_rx) });
    spawn("library", { let sh = sh.clone(); move || library::run(sh, lib_rx) });
    spawn("clips", { let sh = sh.clone(); move || clips::run(sh, clip_rx) });
    spawn("jobs", { let sh = sh.clone(); move || jobs::run(sh, job_rx) });
    spawn("status", { let sh = sh.clone(); move || status::run(sh) });
    spawn("actions", { let sh = sh.clone(); move || actions(sh, action_rx) });
    spawn("tools", { let sh = sh.clone(); move || { let _ = sh.tools(); } });
    if let Err(e) = ipc_server::start(sh.clone()) {
        tracing::error!("GUI connection unavailable: {e:#}");
    }

    // The UI message loop runs on the main thread until shutdown.
    ui::run(ui_handle, ui_rx, action_tx, cfg, hotkey_errors)?;
    request_shutdown(&sh);
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_file(cv_core::paths::ipc_info_path());
    tracing::info!("ClipVault service stopped");
    Ok(())
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let n = name.to_string();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            if r.is_err() {
                tracing::error!("thread {n} panicked");
            }
        })
        .expect("spawn thread");
}

pub fn request_shutdown(sh: &Shared) {
    if sh.shutdown.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = sh.sup_tx.send(SupCmd::Shutdown);
    sh.ui.send(UiCmd::Quit);
}

/// Handle hotkeys and tray menu actions.
fn actions(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<Action>) {
    for a in rx {
        match a {
            Action::SaveClip => {
                let _ = sh.sup_tx.send(SupCmd::SaveClip { short: false, reply: None });
            }
            Action::SaveShortClip => {
                let _ = sh.sup_tx.send(SupCmd::SaveClip { short: true, reply: None });
            }
            Action::AddMarker => match add_marker(&sh, "") {
                Ok(_) => sh.toast(ToastLevel::Info, "Marker added to the 24/7 timeline"),
                Err(e) => sh.toast(ToastLevel::Error, format!("Marker failed: {e}")),
            },
            Action::ToggleRecording => {
                let paused = sh.status.lock().recording.paused_by_user;
                let _ = sh.sup_tx.send(SupCmd::SetRecording { enabled: paused, reply: None });
            }
            Action::OpenGui => open_gui(&sh),
            Action::RestartObs => {
                let _ = sh.sup_tx.send(SupCmd::RestartObs { reply: None });
            }
            Action::OpenClipsFolder => win::shell::open_in_explorer(std::path::Path::new(&sh.cfg.read().clips.folder)),
            Action::OpenRecordingsFolder => win::shell::open_in_explorer(std::path::Path::new(&sh.cfg.read().recording.folder)),
            Action::QuitService | Action::SessionEnding => {
                tracing::info!("shutdown requested ({a:?})");
                request_shutdown(&sh);
                break;
            }
        }
    }
}

pub fn add_marker(sh: &Shared, label: &str) -> Result<i64> {
    let db = cv_core::db::Db::open_default()?;
    let id = db.add_marker(cv_core::util::now_ms(), label)?;
    sh.emit(Event::RecordingsChanged);
    Ok(id)
}

fn open_gui(sh: &Shared) {
    let exe = cv_core::paths::exe_dir().join(cv_core::paths::GUI_EXE);
    if let Err(e) = std::process::Command::new(&exe).current_dir(cv_core::paths::exe_dir()).spawn() {
        sh.toast(ToastLevel::Error, format!("Couldn't open ClipVault: {e}"));
    }
}

/// Re-read config.toml after the GUI saved it.
pub fn reload_config(sh: &Shared, restart_obs: bool) -> Result<()> {
    let new = Config::load()?;
    let old = std::mem::replace(&mut *sh.cfg.write(), new.clone());
    tracing::info!("config reloaded (restart OBS: {restart_obs})");
    sh.ui.send(UiCmd::ApplyConfig);
    let _ = sh.audio_tx.try_send(());
    if old.clips.folder != new.clips.folder || old.recording.folder != new.recording.folder {
        let _ = sh.lib_tx.send(LibCmd::Rescan);
    }
    if old.general.start_with_windows != new.general.start_with_windows {
        startup::sync_startup_shortcut(&new)?;
    }
    if old.advanced.ffmpeg_path != new.advanced.ffmpeg_path || old.advanced.ffprobe_path != new.advanced.ffprobe_path {
        *sh.tools.write() = None;
    }
    let _ = sh.sup_tx.send(SupCmd::ConfigChanged { restart_obs });
    sh.log_event("info", "Settings updated");
    Ok(())
}
