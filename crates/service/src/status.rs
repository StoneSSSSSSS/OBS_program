//! Building the status snapshot (for the GUI and the tray icon).

use crate::shared::Shared;
use crate::win::ui::{TrayState, UiCmd};
use cv_core::ipc::{Event, Status};
use cv_core::util::{format_span_short, human_bytes, now_ms};
use std::sync::Arc;
use std::time::Duration;

pub fn build(sh: &Shared) -> Status {
    let cfg = sh.cfg();
    let mut s = sh.status.lock().clone();
    s.version = cv_core::VERSION.to_string();
    s.jobs = sh.jobs.lock().clone();
    let mut w = Vec::new();
    if !s.obs.running {
        if s.obs.closed_by_user {
            w.push("OBS was closed — recording and clipping are stopped".to_string());
        } else {
            w.push("OBS is not running".to_string());
        }
    } else if !s.obs.connected {
        w.push("Connecting to OBS…".to_string());
    }
    if !s.obs.last_error.is_empty() {
        w.push(s.obs.last_error.clone());
    }
    if s.obs.connected {
        if !s.recording.active && !s.recording.paused_by_user {
            w.push("24/7 recording is not running".to_string());
        }
        if !s.replay.active {
            w.push("Replay buffer is off — clips can't be saved".to_string());
        }
    }
    match s.mic.state.as_str() {
        "fallback" => w.push(format!("Preferred mic not connected — using {}", s.mic.device)),
        "missing" => w.push("No microphone is being recorded".to_string()),
        _ => {}
    }
    for e in sh.hotkey_errors.lock().iter() {
        w.push(e.clone());
    }
    let min_free = cfg.recording.min_free_space_gb as u64 * 1024 * 1024 * 1024;
    if s.disk.total_bytes > 0 && s.disk.free_bytes < min_free {
        w.push(format!("Low disk space on the recordings drive ({} free)", human_bytes(s.disk.free_bytes)));
    }
    if sh.tools.read().is_none() && cfg.advanced.ffmpeg_path.is_empty() && sh.tools().is_none() {
        w.push("ffmpeg not found — clipping from recordings and exports won't work".to_string());
    }
    s.warnings = w;
    s
}

/// Publishes status to GUIs every second and keeps the tray icon current.
pub fn run(sh: Arc<Shared>) {
    let mut last_tray: Option<(TrayState, String, bool)> = None;
    while !sh.is_shutting_down() {
        let s = build(&sh);
        if sh.has_clients() {
            sh.emit(Event::Status { status: Box::new(s.clone()) });
        }
        let state = if !s.warnings.is_empty() && (!s.obs.connected || !s.replay.active || s.mic.state == "missing" || s.mic.state == "fallback") {
            TrayState::Warning
        } else if s.recording.active {
            TrayState::Recording
        } else {
            TrayState::Idle
        };
        let mut tip = String::from("ClipVault — ");
        if s.recording.active {
            let secs = (now_ms() - s.recording.started_ms).max(0) / 1000;
            tip.push_str(&format!("recording ({})", format_span_short(secs)));
        } else if s.recording.paused_by_user {
            tip.push_str("24/7 recording paused");
        } else {
            tip.push_str("not recording");
        }
        if let Some(w) = s.warnings.first() {
            tip.push_str("\n");
            tip.push_str(w);
        }
        let tip: String = tip.chars().take(120).collect();
        let tray = (state, tip, s.recording.paused_by_user);
        if last_tray.as_ref() != Some(&tray) {
            sh.ui.send(UiCmd::Tray { state: tray.0, tooltip: tray.1.clone(), recording_paused: tray.2 });
            last_tray = Some(tray);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
