//! Mic guard + audio routing.
//!
//! Watches Windows audio devices. Whenever something changes (headset
//! unplugged/replugged, default device switched, new endpoint registered) it
//! recomputes which microphone OBS should record and tells the supervisor,
//! which re-points OBS within a second or two.

use crate::shared::{Shared, SupCmd};
use crate::win::coreaudio::{com_init_mta, AudioSystem};
use crate::win::process;
use cv_core::audio_names::{decide_mic, pick_preferred, MicDecision};
use cv_core::ipc::AudioDevices;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub fn run(sh: Arc<Shared>, trigger_rx: crossbeam_channel::Receiver<()>) {
    com_init_mta();
    let (change_tx, change_rx) = crossbeam_channel::bounded::<()>(64);
    let sys = match AudioSystem::new(Some(change_tx)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("audio guard: Core Audio unavailable: {e}");
            return;
        }
    };
    let mut last_mic: Option<MicDecision> = None;
    let mut last_desktop: Option<String> = None;
    let mut last_routing: Option<HashMap<String, bool>> = None;
    let mut first = true;
    while !sh.is_shutting_down() {
        if !first {
            crossbeam_channel::select! {
                recv(change_rx) -> _ => {
                    // Devices often change in bursts; let things settle.
                    std::thread::sleep(Duration::from_millis(1200));
                    while change_rx.try_recv().is_ok() {}
                }
                recv(trigger_rx) -> _ => { while trigger_rx.try_recv().is_ok() {} }
                default(Duration::from_secs(5)) => {}
            }
        }
        first = false;
        let cfg = sh.cfg();
        let inputs = sys.inputs();
        let outputs = sys.outputs();
        let def_in = sys.default_endpoint(true);
        let def_out = sys.default_endpoint(false);
        *sh.devices.lock() = AudioDevices {
            inputs: inputs.clone(),
            outputs: outputs.clone(),
            default_input: def_in.as_ref().map(|e| e.name.clone()),
            default_output: def_out.as_ref().map(|e| e.name.clone()),
        };

        // Microphone.
        if cfg.audio.mic_guard {
            let d = decide_mic(&cfg.audio.mic_priority, &inputs, &cfg.audio.mic_fallback);
            if last_mic.as_ref() != Some(&d) {
                if let Some(prev) = &last_mic {
                    let was_ok = matches!(prev, MicDecision::Preferred { rank: 0, .. });
                    let now_ok = matches!(d, MicDecision::Preferred { rank: 0, .. });
                    if was_ok && !now_ok && !cfg.audio.mic_priority.is_empty() {
                        sh.log_event("warn", "Preferred microphone disconnected");
                    } else if !was_ok && now_ok {
                        sh.log_event("info", "Preferred microphone reconnected");
                    }
                }
                last_mic = Some(d.clone());
                let _ = sh.sup_tx.send(SupCmd::Mic { decision: d });
            }
        }

        // Desktop audio device.
        let desktop_id = if cfg.audio.desktop_device.eq_ignore_ascii_case("default") {
            "default".to_string()
        } else {
            pick_preferred(&[cfg.audio.desktop_device.clone()], &outputs)
                .map(|(_, e)| e.id.clone())
                .unwrap_or_else(|| "default".to_string())
        };
        if last_desktop.as_deref() != Some(desktop_id.as_str()) {
            last_desktop = Some(desktop_id.clone());
            let _ = sh.sup_tx.send(SupCmd::DesktopDevice { device_id: desktop_id.clone() });
        }

        // Apps playing on a device other than the desktop-audio one must be
        // added to the full mix, or they'd be missing from it.
        if cfg.audio.multitrack && cfg.audio.auto_mix_other_devices {
            let desktop_endpoint = if desktop_id == "default" { def_out.as_ref().map(|e| e.id.clone()) } else { Some(desktop_id.clone()) };
            let sessions = sys.sessions_by_output();
            let names: HashMap<u32, String> = process::list_processes().into_iter().map(|p| (p.pid, p.exe_name.to_lowercase())).collect();
            let mut apps: HashSet<String> = cfg.audio.voice_apps.iter().chain(cfg.audio.music_apps.iter()).map(|s| s.to_lowercase()).collect();
            if let Some(exe) = current_game_exe(&sh) {
                apps.insert(exe);
            }
            let mut routing = HashMap::new();
            for app in apps {
                let mut on_desktop = false;
                let mut elsewhere = false;
                for (ep, pids) in &sessions {
                    let has = pids.iter().any(|p| names.get(p).map(|n| *n == app).unwrap_or(false));
                    if !has {
                        continue;
                    }
                    if Some(ep) == desktop_endpoint.as_ref() {
                        on_desktop = true;
                    } else {
                        elsewhere = true;
                    }
                }
                routing.insert(app, elsewhere && !on_desktop);
            }
            if last_routing.as_ref() != Some(&routing) {
                if routing.values().any(|v| *v) {
                    tracing::info!("apps on other devices added to full mix: {:?}", routing.iter().filter(|(_, v)| **v).map(|(k, _)| k).collect::<Vec<_>>());
                }
                last_routing = Some(routing.clone());
                let _ = sh.sup_tx.send(SupCmd::AppRouting(routing));
            }
        }
    }
}

/// Exe of the game the "Game" track currently follows (looked up by name).
fn current_game_exe(sh: &Shared) -> Option<String> {
    let name = sh.status.lock().game.audio_target.clone()?;
    let db = cv_core::db::Db::open_default().ok()?;
    let g = db.game_by_name(&name).ok().flatten()?;
    let exe = cv_core::games::exe_name_of(&g.exe_path);
    (!exe.is_empty()).then_some(exe)
}
