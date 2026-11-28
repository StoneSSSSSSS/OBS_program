//! Mic guard.
//!
//! Watches Windows audio devices. Whenever something changes (headset
//! unplugged/replugged, default device switched, new endpoint registered) it
//! works out which microphone should be recorded and tells the recording
//! engine, which switches within a second or two.

use crate::shared::{EngineCmd, Shared};
use crate::win::coreaudio::{com_init_mta, AudioSystem};
use cv_core::audio_names::{decide_mic, MicDecision};
use cv_core::ipc::AudioDevices;
use std::sync::Arc;
use std::time::Duration;

pub fn run(sh: Arc<Shared>, trigger_rx: crossbeam_channel::Receiver<()>) {
    com_init_mta();
    let (change_tx, change_rx) = crossbeam_channel::bounded::<()>(64);
    let sys = match AudioSystem::new(Some(change_tx)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("mic guard: Core Audio unavailable: {e}");
            return;
        }
    };
    let mut last_mic: Option<MicDecision> = None;
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
            outputs,
            default_input: def_in.as_ref().map(|e| e.name.clone()),
            default_output: def_out.as_ref().map(|e| e.name.clone()),
        };
        let d = if cfg.audio.mic_guard { decide_mic(&cfg.audio.mic_priority, &inputs, &cfg.audio.mic_fallback) } else { MicDecision::WindowsDefault };
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
            let _ = sh.engine_tx.send(EngineCmd::Mic { decision: d });
        }
    }
}
