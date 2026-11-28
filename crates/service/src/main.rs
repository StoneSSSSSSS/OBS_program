//! ClipVault background service.
//!
//! Started at login (Startup folder). Runs ClipVault's own recorder (screen,
//! every app's sound and the microphone) for the 24/7 recording and the clip
//! hotkey, keeps the microphone pointed at the right device, files clips by
//! game and enforces recording retention. Shows only a tray icon; the GUI
//! (clipvault.exe) talks to it over a local socket.

#![windows_subsystem = "windows"]

mod activity;
mod audio_guard;
mod clips;
mod engine;
mod ipc_server;
mod jobs;
mod library;
mod shared;
mod startup;
mod status;
mod trim_recovery;
mod waves;
mod win;
#[cfg(test)]
mod test_ipc;

use crate::shared::{EngineCmd, LibCmd, Shared};
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
            println!("recordings: {}  clips: {}  retention: {}h  clip length: {}s", cfg.recording.folder, cfg.clips.folder, cfg.recording.retention_hours, cfg.clips.clip_length_secs);
        }
        "--install-shortcuts" => {
            anyhow::ensure!(!cv_core::paths::uses_local_storage(), "Project-local ClipVault does not create Windows shortcuts");
            let cfg = startup::load_or_first_run();
            startup::sync_startup_shortcut(&cfg)?;
            startup::create_app_shortcuts(args.iter().any(|a| a == "--desktop"))?;
            println!("shortcuts created");
        }
        "--remove-startup" => {
            anyhow::ensure!(!cv_core::paths::uses_local_storage(), "Project-local ClipVault does not manage Windows startup");
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
    cv_media::mf::init();
    let devices = std::thread::spawn(|| {
        win::coreaudio::com_init_mta();
        let sys = win::coreaudio::AudioSystem::new(None).ok()?;
        Some((sys.inputs(), sys.outputs(), sys.default_endpoint(true), sys.default_endpoint(false)))
    })
    .join()
    .ok()
    .flatten();
    let procs = cv_media::capture::sessions::processes();
    let sessions = cv_media::capture::sessions::sound_sessions();
    let pids: Vec<u32> = sessions.iter().map(|s| s.pid).collect();
    let targets = cv_media::capture::sessions::choose_targets(&pids, &procs, &[std::process::id()], cv_media::capture::sessions::NEVER_CAPTURE, &cv_media::capture::sessions::process_created);
    let out = serde_json::json!({
        "config": cv_core::paths::config_path(),
        "recordings_folder": cfg.recording.folder,
        "clips_folder": cfg.clips.folder,
        "monitors": cv_media::d3d::list_outputs().iter().map(|o| format!("{} {}x{} on {}", o.device_name, o.width(), o.height(), o.adapter_name)).collect::<Vec<_>>(),
        "video_encoders": cv_media::mf::h264::list_encoders().iter().map(|e| format!("{}{}", e.name, if e.hardware { " (hardware)" } else { "" })).collect::<Vec<_>>(),
        "mic_priority": cfg.audio.mic_priority,
        "inputs": devices.as_ref().map(|d| &d.0),
        "outputs": devices.as_ref().map(|d| &d.1),
        "default_input": devices.as_ref().and_then(|d| d.2.clone()),
        "default_output": devices.as_ref().and_then(|d| d.3.clone()),
        "apps_with_sound": sessions.iter().map(|s| format!("{} (pid {}){}", procs.get(&s.pid).map(|p| p.exe.clone()).unwrap_or_default(), s.pid, if s.active { " playing" } else { "" })).collect::<Vec<_>>(),
        "app_captures": targets.iter().map(|t| format!("{} (pid {})", t.exe, t.root)).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn run_service() -> Result<()> {
    cv_core::paths::ensure_dirs()?;
    let cfg = startup::load_or_first_run();
    let _log = cv_core::logging::init("service", &cfg.advanced.log_level, cfg.advanced.log_retention_days);
    let Some(_instance) = win::process::SingleInstance::acquire(&cv_core::paths::instance_name("Service")) else {
        tracing::info!("another ClipVault service is already running; exiting");
        return Ok(());
    };
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    tracing::info!("ClipVault service {} starting", cv_core::VERSION);
    trim_recovery::recover_on_start();
    if let Err(e) = startup::sync_startup_shortcut(&cfg) {
        tracing::warn!("startup shortcut: {e:#}");
    }

    let cfg = Arc::new(RwLock::new(cfg));
    let (ui_handle, ui_rx) = ui::channel();
    let (engine_tx, engine_rx) = crossbeam_channel::unbounded();
    let (clip_tx, clip_rx) = crossbeam_channel::unbounded();
    let (job_tx, job_rx) = crossbeam_channel::unbounded();
    let (lib_tx, lib_rx) = crossbeam_channel::unbounded();
    let (audio_tx, audio_rx) = crossbeam_channel::bounded(4);
    let (action_tx, action_rx) = crossbeam_channel::unbounded();
    let hotkey_errors = Arc::new(Mutex::new(Vec::new()));
    let sh = Arc::new(Shared {
        cfg: cfg.clone(),
        #[cfg(test)]
        isolated_test: false,
        status: Mutex::new(cv_core::ipc::Status { service_started_ms: cv_core::util::now_ms(), ..Default::default() }),
        ui: ui_handle.clone(),
        engine_tx,
        clip_tx,
        job_tx,
        lib_tx,
        audio_tx,
        clients: Mutex::new(Vec::new()),
        hotkey_errors: hotkey_errors.clone(),
        devices: Mutex::new(Default::default()),
        jobs: Mutex::new(Vec::new()),
        event_db: Mutex::new(None),
        foreground_game: Default::default(),
        library_lock: Mutex::new(()),
        live_path: Mutex::new(None),
        shutdown: std::sync::atomic::AtomicBool::new(false),
    });
    sh.log_event("info", format!("ClipVault service {} started", cv_core::VERSION));

    let engine = spawn("engine", { let sh = sh.clone(); move || engine::run(sh, engine_rx) });
    spawn("activity", { let sh = sh.clone(); move || activity::run(sh) });
    spawn("mic-guard", { let sh = sh.clone(); move || audio_guard::run(sh, audio_rx) });
    spawn("library", { let sh = sh.clone(); move || library::run(sh, lib_rx) });
    spawn("clips", { let sh = sh.clone(); move || clips::run(sh, clip_rx) });
    spawn("jobs", { let sh = sh.clone(); move || jobs::run(sh, job_rx) });
    spawn("status", { let sh = sh.clone(); move || status::run(sh) });
    spawn("actions", { let sh = sh.clone(); move || actions(sh, action_rx) });
    spawn("waves", { let sh = sh.clone(); move || waves::run(sh) });
    if let Err(e) = ipc_server::start(sh.clone()) {
        tracing::error!("GUI connection unavailable: {e:#}");
    }

    // The UI message loop runs on the main thread until shutdown.
    ui::run(ui_handle, ui_rx, action_tx, cfg, hotkey_errors)?;
    request_shutdown(&sh);
    // Let the recorder finish its file.
    let _ = engine.join();
    let _ = std::fs::remove_file(cv_core::paths::ipc_info_path());
    tracing::info!("ClipVault service stopped");
    Ok(())
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    let n = name.to_string();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            if r.is_err() {
                tracing::error!("thread {n} panicked");
            }
        })
        .expect("spawn thread")
}

pub fn request_shutdown(sh: &Shared) {
    if sh.shutdown.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = sh.engine_tx.send(EngineCmd::Shutdown);
    sh.ui.send(UiCmd::Quit);
}

/// Handle hotkeys and tray menu actions.
fn actions(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<Action>) {
    for a in rx {
        match a {
            Action::SaveClip => {
                let _ = sh.engine_tx.send(EngineCmd::SaveClip { short: false, reply: None });
            }
            Action::SaveShortClip => {
                let _ = sh.engine_tx.send(EngineCmd::SaveClip { short: true, reply: None });
            }
            Action::AddMarker => match add_marker(&sh, "") {
                Ok(_) => sh.toast(ToastLevel::Info, "Marker added to the 24/7 timeline"),
                Err(e) => sh.toast(ToastLevel::Error, format!("Marker failed: {e}")),
            },
            Action::ToggleRecording => {
                let _ = sh.engine_tx.send(EngineCmd::ToggleRecording);
            }
            Action::OpenGui => open_gui(&sh),
            Action::RestartRecorder => {
                let _ = sh.engine_tx.send(EngineCmd::Restart { reply: None });
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
pub fn reload_config(sh: &Shared, restart_recorder: bool) -> Result<()> {
    let new = Config::load()?;
    #[cfg(test)]
    let new = if sh.isolated_test { test_ipc::isolate_config(new, &cv_core::paths::data_dir()) } else { new };
    new.validate_media_folders()?;
    let old = std::mem::replace(&mut *sh.cfg.write(), new.clone());
    let restart = restart_recorder || old.recorder_restart_needed(&new);
    tracing::info!("config reloaded (restart recorder: {restart})");
    sh.ui.send(UiCmd::ApplyConfig);
    let _ = sh.audio_tx.try_send(());
    if old.clips.folder != new.clips.folder || old.recording.folder != new.recording.folder {
        let _ = sh.lib_tx.send(LibCmd::Rescan);
    }
    sh.engine_tx.send(EngineCmd::ConfigChanged { restart })?;
    if old.general.start_with_windows != new.general.start_with_windows {
        startup::sync_startup_shortcut(&new)?;
    }
    sh.log_event("info", "Settings updated");
    Ok(())
}
