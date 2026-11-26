//! The OBS supervisor: keeps OBS running, connected, recording and buffering,
//! and applies audio routing decisions from the other threads.

use crate::obs_setup::{self, Managed};
use crate::shared::{norm_path, ClipTask, GameTarget, LibCmd, Shared, SupCmd};
use crate::win::process;
use crate::win::ui::{ToastLevel, UiCmd};
use crossbeam_channel::{never, select, Receiver, Sender};
use cv_core::audio_names::MicDecision;
use cv_core::ipc::{Event, Meter};
use cv_core::obs::files::ObsConfigFiles;
use cv_core::obs::protocol::subs;
use cv_core::obs::{client::ObsError, ObsClient, ObsEvent};
use cv_core::util::now_ms;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const META_RECORDING_WANTED: &str = "recording_wanted";
const META_MIC_INPUT: &str = "mic_input_name";

struct PendingSave {
    short: bool,
    at: Instant,
}

pub struct Supervisor {
    sh: Arc<Shared>,
    rx: Receiver<SupCmd>,
    obs: Option<Arc<ObsClient>>,
    events: Option<Receiver<ObsEvent>>,
    pid: Option<u32>,
    started: Instant,
    launches: VecDeque<Instant>,
    closed_by_user: bool,
    expecting_exit: bool,
    exit_started: bool,
    recording_wanted: bool,
    last_connect_try: Option<Instant>,
    connect_failures: u32,
    settings_restart_done: bool,
    // outputs
    rec_active: bool,
    rec_path: String,
    rec_started_ms: i64,
    rec_bytes: i64,
    rec_bytes_changed: Instant,
    stall_restarts: VecDeque<Instant>,
    last_start_attempt: Option<Instant>,
    start_failures: u32,
    replay_active: bool,
    last_replay_attempt: Option<Instant>,
    // audio
    managed: Option<Managed>,
    inputs_dirty: bool,
    mic: Option<MicDecision>,
    mic_applied: Option<String>,
    desktop_device: Option<String>,
    desktop_applied: Option<String>,
    routing: HashMap<String, bool>,
    game: Option<GameTarget>,
    game_applied: Option<String>,
    pending_save: Option<PendingSave>,
    last_save_request: Option<Instant>,
    meters_emitted: Instant,
    mic_last_sound: Instant,
    mic_silence_warned: bool,
    tick_count: u64,
    last_error: String,
}

impl Supervisor {
    pub fn new(sh: Arc<Shared>, rx: Receiver<SupCmd>) -> Self {
        let wanted = cv_core::db::Db::open_default()
            .ok()
            .and_then(|db| db.meta_get(META_RECORDING_WANTED).ok().flatten())
            .map(|v| v != "0")
            .unwrap_or(true);
        Self {
            sh,
            rx,
            obs: None,
            events: None,
            pid: None,
            started: Instant::now(),
            launches: VecDeque::new(),
            closed_by_user: false,
            expecting_exit: false,
            exit_started: false,
            recording_wanted: wanted,
            last_connect_try: None,
            connect_failures: 0,
            settings_restart_done: false,
            rec_active: false,
            rec_path: String::new(),
            rec_started_ms: 0,
            rec_bytes: 0,
            rec_bytes_changed: Instant::now(),
            stall_restarts: VecDeque::new(),
            last_start_attempt: None,
            start_failures: 0,
            replay_active: false,
            last_replay_attempt: None,
            managed: None,
            inputs_dirty: true,
            mic: None,
            mic_applied: None,
            desktop_device: None,
            desktop_applied: None,
            routing: HashMap::new(),
            game: None,
            game_applied: None,
            pending_save: None,
            last_save_request: None,
            meters_emitted: Instant::now(),
            mic_last_sound: Instant::now(),
            mic_silence_warned: false,
            tick_count: 0,
            last_error: String::new(),
        }
    }

    pub fn run(mut self) {
        tracing::info!("supervisor started (24/7 recording {})", if self.recording_wanted { "on" } else { "paused" });
        let mut next_tick = Instant::now();
        loop {
            if self.sh.is_shutting_down() {
                break;
            }
            let ev_rx = self.events.clone().unwrap_or_else(never);
            let timeout = next_tick.saturating_duration_since(Instant::now());
            select! {
                recv(self.rx) -> cmd => match cmd {
                    Ok(SupCmd::Shutdown) | Err(_) => break,
                    Ok(cmd) => self.command(cmd),
                },
                recv(ev_rx) -> ev => match ev {
                    Ok(ev) => self.obs_event(ev),
                    Err(_) => self.disconnected("event channel closed".into()),
                },
                default(timeout) => {}
            }
            if Instant::now() >= next_tick {
                self.tick();
                let secs = self.sh.cfg.read().advanced.health_check_secs.max(1) as u64;
                next_tick = Instant::now() + Duration::from_secs(secs);
            }
        }
        if let Some(obs) = &self.obs {
            obs.close();
        }
        tracing::info!("supervisor stopped");
    }

    fn obs_exe(&self) -> PathBuf {
        PathBuf::from(self.sh.cfg.read().obs.exe_path.trim())
    }

    fn files(&self) -> ObsConfigFiles {
        ObsConfigFiles::from_config(&self.sh.cfg.read())
    }

    fn connected(&self) -> Option<Arc<ObsClient>> {
        self.obs.as_ref().filter(|o| o.is_alive()).cloned()
    }

    // ------------------------------------------------------------ tick

    fn tick(&mut self) {
        self.tick_count += 1;
        let cfg = self.sh.cfg();

        // Process state.
        if let Some(pid) = self.pid {
            if !process::is_running(pid) {
                self.obs_process_exited();
            }
        }
        if self.pid.is_none() {
            let exe = self.obs_exe();
            if let Some(pid) = process::find_by_exe(&exe).into_iter().next() {
                tracing::info!("found running OBS (pid {pid})");
                self.pid = Some(pid);
            }
        }
        if self.pid.is_none() {
            if self.should_launch(&cfg) {
                self.launch();
            }
            self.publish_status();
            return;
        }

        // Connection.
        if self.connected().is_none() {
            let due = self.last_connect_try.map(|t| t.elapsed() >= Duration::from_secs(2)).unwrap_or(true);
            if due {
                self.try_connect();
            }
            // An OBS we didn't start, with the websocket switched off: restart it
            // once so the websocket (and our settings) take effect.
            if self.connected().is_none()
                && self.connect_failures >= 8
                && !self.settings_restart_done
                && !self.files().read_websocket().enabled
            {
                self.settings_restart_done = true;
                self.sh.log_event("warn", "OBS websocket is off; restarting OBS to enable it");
                self.restart_obs("enabling the OBS websocket");
            }
        } else {
            self.health_check(&cfg);
        }
        self.publish_status();
    }

    fn should_launch(&mut self, cfg: &cv_core::config::Config) -> bool {
        if !cfg.obs.launch_on_start || self.sh.is_shutting_down() {
            return false;
        }
        if self.closed_by_user && !cfg.obs.restart_if_closed {
            return false;
        }
        if self.launches.is_empty() && self.started.elapsed() < Duration::from_secs(cfg.obs.launch_delay_secs as u64) {
            return false;
        }
        while self.launches.front().map(|t| t.elapsed() > Duration::from_secs(3600)).unwrap_or(false) {
            self.launches.pop_front();
        }
        if self.launches.len() as u32 >= cfg.obs.max_restarts_per_hour {
            if self.last_error != "restart limit" {
                self.last_error = "restart limit".into();
                self.sh.log_event("error", "OBS keeps crashing; auto-restart paused for now");
                self.sh.ui.send(UiCmd::Balloon {
                    level: ToastLevel::Error,
                    title: "OBS keeps crashing".into(),
                    text: "ClipVault stopped restarting it. Open ClipVault to restart OBS.".into(),
                });
            }
            return false;
        }
        // Back off a little after a crash.
        if let Some(last) = self.launches.back() {
            if last.elapsed() < Duration::from_secs(5) {
                return false;
            }
        }
        true
    }

    fn launch(&mut self) {
        let cfg = self.sh.cfg();
        let exe = self.obs_exe();
        if !exe.is_file() {
            self.set_error(format!("OBS not found at {}", exe.display()));
            return;
        }
        let files = self.files();
        if cfg.obs.manage_settings {
            match files.apply_settings(&cfg, false) {
                Ok(true) => tracing::info!("updated OBS settings before launch"),
                Ok(false) => {}
                Err(e) => tracing::warn!("could not write OBS settings: {e:#}"),
            }
        }
        if let Err(e) = files.ensure_websocket(cfg.obs.websocket_port, &cfg.obs.websocket_password) {
            tracing::warn!("could not configure OBS websocket: {e:#}");
        }
        let start_rec = cfg.recording.enabled && self.recording_wanted;
        match process::launch_obs(&exe, cfg.obs.minimize_to_tray, start_rec, true) {
            Ok(pid) => {
                self.pid = Some(pid);
                self.launches.push_back(Instant::now());
                self.exit_started = false;
                self.expecting_exit = false;
                self.connect_failures = 0;
                self.last_connect_try = Some(Instant::now());
                self.inputs_dirty = true;
                self.mic_applied = None;
                self.desktop_applied = None;
                self.game_applied = None;
                self.last_error.clear();
                self.sh.log_event("info", format!("Started OBS (pid {pid})"));
                self.sh.update_status(|s| s.obs.restarts = self.launches.len() as u32);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    fn obs_process_exited(&mut self) {
        let expected = self.expecting_exit;
        let clean = self.exit_started;
        tracing::info!("OBS exited (expected={expected}, clean={clean})");
        self.pid = None;
        self.drop_connection();
        self.rec_active = false;
        self.replay_active = false;
        if expected {
            self.expecting_exit = false;
            return;
        }
        let cfg = self.sh.cfg();
        if clean {
            self.closed_by_user = !cfg.obs.restart_if_closed;
            self.sh.log_event("warn", "OBS was closed");
            if self.closed_by_user {
                self.sh.ui.send(UiCmd::Balloon {
                    level: ToastLevel::Warning,
                    title: "OBS was closed".into(),
                    text: "24/7 recording and clipping are stopped. Open ClipVault to start OBS again.".into(),
                });
            }
        } else {
            self.sh.log_event("error", "OBS crashed or was killed; restarting it");
            if !cfg.obs.restart_on_crash {
                self.closed_by_user = true;
            }
        }
        self.exit_started = false;
    }

    fn try_connect(&mut self) {
        self.last_connect_try = Some(Instant::now());
        let cfg = self.sh.cfg();
        let password = if cfg.obs.websocket_password.is_empty() {
            self.files().read_websocket().password
        } else {
            cfg.obs.websocket_password.clone()
        };
        let mask = subs::ALL | subs::INPUT_VOLUME_METERS;
        match ObsClient::connect(cfg.obs.websocket_port, &password, mask, Duration::from_secs(3)) {
            Ok((client, events)) => {
                self.obs = Some(Arc::new(client));
                self.events = Some(events);
                self.connect_failures = 0;
                self.on_connected();
            }
            Err(ObsError::Auth) => {
                self.connect_failures += 1;
                self.set_error("OBS websocket password is wrong (Settings > OBS)".into());
            }
            Err(e) => {
                self.connect_failures += 1;
                if self.connect_failures % 15 == 0 {
                    tracing::warn!("still can't reach OBS websocket: {e}");
                }
            }
        }
    }

    fn on_connected(&mut self) {
        let Some(obs) = self.connected() else { return };
        let cfg = self.sh.cfg();
        let version = obs.request("GetVersion", None).ok().and_then(|v| v["obsVersion"].as_str().map(str::to_string)).unwrap_or_default();
        tracing::info!("connected to OBS {version} (websocket {})", obs.ws_version);
        self.last_error.clear();
        self.sh.update_status(|s| {
            s.obs.connected = true;
            s.obs.version = version.clone();
            s.obs.last_error.clear();
        });

        // An OBS we didn't launch may be running with different settings.
        if cfg.obs.manage_settings && !self.settings_restart_done {
            if let Ok(true) = self.files().apply_settings(&cfg, true) {
                self.settings_restart_done = true;
                self.sh.log_event("info", "Restarting OBS once to apply ClipVault's recording settings");
                self.restart_obs("applying recording settings");
                return;
            }
        }
        self.settings_restart_done = true;

        self.inputs_dirty = true;
        self.mic_applied = None;
        self.desktop_applied = None;
        self.game_applied = None;
        self.ensure_inputs();

        // Output state.
        if let Ok(v) = obs.request("GetRecordStatus", None) {
            self.rec_active = v["outputActive"].as_bool().unwrap_or(false);
            self.rec_bytes = v["outputBytes"].as_i64().unwrap_or(0);
            self.rec_bytes_changed = Instant::now();
            if self.rec_active && self.rec_path.is_empty() {
                // Find the file OBS is writing: newest recording in the folder.
                self.rec_path = newest_recording(&cfg.recording.folder).unwrap_or_default();
                self.rec_started_ms = now_ms() - v["outputDuration"].as_i64().unwrap_or(0);
                if !self.rec_path.is_empty() {
                    let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
                }
            }
        }
        if let Ok(v) = obs.request("GetReplayBufferStatus", None) {
            self.replay_active = v["outputActive"].as_bool().unwrap_or(false);
        }
        self.ensure_outputs(&cfg);
        self.apply_audio();
    }

    fn drop_connection(&mut self) {
        if let Some(o) = self.obs.take() {
            o.close();
        }
        self.events = None;
        self.managed = None;
        self.sh.update_status(|s| s.obs.connected = false);
    }

    fn disconnected(&mut self, reason: String) {
        if self.obs.is_some() {
            tracing::warn!("lost OBS websocket: {reason}");
        }
        self.drop_connection();
    }

    fn set_error(&mut self, e: String) {
        if self.last_error != e {
            self.sh.log_event("error", &e);
            self.last_error = e.clone();
        }
        self.sh.update_status(|s| s.obs.last_error = e);
    }

    // ------------------------------------------------------ outputs

    fn ensure_outputs(&mut self, cfg: &cv_core::config::Config) {
        let Some(obs) = self.connected() else { return };
        if self.exit_started || self.expecting_exit {
            return;
        }
        let want_rec = cfg.recording.enabled && self.recording_wanted;
        if want_rec && !self.rec_active && cfg.recording.auto_restart {
            let due = self.last_start_attempt.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true);
            if due {
                self.last_start_attempt = Some(Instant::now());
                match obs.request("StartRecord", None) {
                    Ok(_) => {
                        tracing::info!("started recording");
                        self.start_failures = 0;
                    }
                    Err(ObsError::Request { code: 500, .. }) => {} // already active
                    Err(e) => {
                        self.start_failures += 1;
                        if self.start_failures == 3 {
                            self.sh.log_event("error", format!("OBS refuses to start recording: {e}"));
                            self.sh.ui.send(UiCmd::Balloon {
                                level: ToastLevel::Error,
                                title: "Recording won't start".into(),
                                text: format!("{e}"),
                            });
                        }
                    }
                }
            }
        }
        if !self.replay_active {
            let due = self.last_replay_attempt.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true);
            if due {
                self.last_replay_attempt = Some(Instant::now());
                match obs.request("StartReplayBuffer", None) {
                    Ok(_) => tracing::info!("started replay buffer"),
                    Err(ObsError::Request { code: 500, .. }) => self.replay_active = true,
                    Err(e) => tracing::warn!("could not start replay buffer: {e}"),
                }
            }
        }
    }

    fn health_check(&mut self, cfg: &cv_core::config::Config) {
        let Some(obs) = self.connected() else { return };
        if let Ok(v) = obs.request("GetRecordStatus", None) {
            let active = v["outputActive"].as_bool().unwrap_or(false);
            let bytes = v["outputBytes"].as_i64().unwrap_or(0);
            if active != self.rec_active {
                self.rec_active = active;
                self.rec_bytes_changed = Instant::now();
            }
            if active {
                if bytes != self.rec_bytes {
                    self.rec_bytes = bytes;
                    self.rec_bytes_changed = Instant::now();
                } else if self.rec_bytes_changed.elapsed() > Duration::from_secs(cfg.recording.stall_timeout_secs as u64) {
                    self.recording_stalled();
                }
            }
        }
        if let Ok(v) = obs.request("GetReplayBufferStatus", None) {
            self.replay_active = v["outputActive"].as_bool().unwrap_or(false);
        }
        self.ensure_outputs(cfg);
        if self.inputs_dirty || self.tick_count % 20 == 0 {
            self.ensure_inputs();
        }
        if self.pending_save.as_ref().map(|p| p.at.elapsed() > Duration::from_secs(60)).unwrap_or(false) {
            self.pending_save = None;
        }
        // Mic silence warning.
        let mins = cfg.audio.mic_silence_warning_mins;
        if mins > 0 && self.rec_active && !self.mic_silence_warned && self.mic_last_sound.elapsed() > Duration::from_secs(mins as u64 * 60) {
            self.mic_silence_warned = true;
            self.sh.ui.send(UiCmd::Balloon {
                level: ToastLevel::Warning,
                title: "Your mic is silent".into(),
                text: format!("No sound from the microphone for {mins} minutes."),
            });
        }
    }

    fn recording_stalled(&mut self) {
        let Some(obs) = self.connected() else { return };
        while self.stall_restarts.front().map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(false) {
            self.stall_restarts.pop_front();
        }
        self.stall_restarts.push_back(Instant::now());
        self.rec_bytes_changed = Instant::now();
        if self.stall_restarts.len() >= 3 {
            self.sh.log_event("error", "Recording keeps stalling; restarting OBS");
            self.stall_restarts.clear();
            self.restart_obs("recording stalled repeatedly");
            return;
        }
        self.sh.log_event("warn", "Recording stopped growing; restarting the recording");
        let _ = obs.request("StopRecord", None);
        std::thread::sleep(Duration::from_secs(2));
        let _ = obs.request("StartRecord", None);
    }

    // ------------------------------------------------------- inputs

    fn ensure_inputs(&mut self) {
        let Some(obs) = self.connected() else { return };
        let cfg = self.sh.cfg();
        let db = cv_core::db::Db::open_default().ok();
        let mic_meta = db.as_ref().and_then(|d| d.meta_get(META_MIC_INPUT).ok().flatten());
        let mut routing = self.routing.clone();
        if let Some(g) = &self.game {
            if let Some(v) = self.routing.get(&g.exe.to_lowercase()) {
                routing.insert(String::new(), *v);
            }
        }
        match obs_setup::ensure_inputs(&obs, &cfg, mic_meta.as_deref(), &routing) {
            Ok(m) => {
                if let (Some(db), Some(mic)) = (db.as_ref(), m.mic.as_ref()) {
                    let _ = db.meta_set(META_MIC_INPUT, mic);
                }
                let changed = self.managed.as_ref() != Some(&m);
                self.managed = Some(m);
                self.inputs_dirty = false;
                if changed {
                    self.mic_applied = None;
                    self.desktop_applied = None;
                    self.game_applied = None;
                    self.apply_audio();
                }
            }
            Err(e) => {
                tracing::warn!("could not set up OBS audio inputs: {e}");
                self.inputs_dirty = true;
            }
        }
    }

    fn apply_audio(&mut self) {
        let Some(obs) = self.connected() else { return };
        let Some(m) = self.managed.clone() else { return };
        let cfg = self.sh.cfg();
        // Mic.
        if let (Some(mic), Some(dec)) = (m.mic.as_ref(), self.mic.clone()) {
            if cfg.audio.mic_guard {
                if let Some(id) = dec.obs_device_id() {
                    if self.mic_applied.as_deref() != Some(id) {
                        match obs_setup::set_device(&obs, mic, id) {
                            Ok(changed) => {
                                self.mic_applied = Some(id.to_string());
                                if changed {
                                    let msg = match &dec {
                                        MicDecision::Preferred { endpoint, .. } => format!("Mic set to {}", cv_core::audio_names::clean_display_name(&endpoint.name)),
                                        MicDecision::WindowsDefault => "Preferred mic not connected; recording the Windows default mic".to_string(),
                                        MicDecision::Leave => String::new(),
                                    };
                                    if !msg.is_empty() {
                                        self.sh.log_event("info", &msg);
                                        self.sh.toast(ToastLevel::Info, msg);
                                    }
                                }
                            }
                            Err(e) => tracing::warn!("could not set mic: {e}"),
                        }
                    }
                }
            }
        }
        // Desktop audio device.
        if let (Some(desk), Some(dev)) = (m.desktop.as_ref(), self.desktop_device.clone()) {
            if self.desktop_applied.as_deref() != Some(dev.as_str()) {
                if obs_setup::set_device(&obs, desk, &dev).is_ok() {
                    self.desktop_applied = Some(dev);
                }
            }
        }
        // Game audio target.
        if cfg.audio.multitrack && cfg.audio.game_audio_capture {
            if let Some(g) = self.game.clone() {
                if self.game_applied.as_deref() != Some(g.exe.as_str()) {
                    match obs_setup::set_game_target(&obs, &g.exe, &g.title, &g.class) {
                        Ok(changed) => {
                            self.game_applied = Some(g.exe.clone());
                            if changed {
                                tracing::info!("game audio now follows {} ({})", g.name, g.exe);
                            }
                        }
                        Err(e) => tracing::warn!("could not retarget game audio: {e}"),
                    }
                }
            }
        }
        // Full-mix routing for apps on other devices.
        if cfg.audio.multitrack {
            let game_exe = self.game.as_ref().map(|g| g.exe.clone());
            if let Err(e) = obs_setup::apply_routing(&obs, &m, &self.routing, game_exe.as_deref()) {
                tracing::warn!("routing: {e}");
            }
        }
    }

    // ------------------------------------------------------- events

    fn obs_event(&mut self, ev: ObsEvent) {
        match ev {
            ObsEvent::Disconnected(reason) => self.disconnected(reason),
            ObsEvent::Event { kind, data } => self.obs_event_kind(&kind, &data),
        }
    }

    fn obs_event_kind(&mut self, kind: &str, data: &Value) {
        match kind {
            "ReplayBufferSaved" => {
                let path = data["savedReplayPath"].as_str().unwrap_or("").to_string();
                let pending = self.pending_save.take();
                if pending.is_none() {
                    // Saved from OBS itself: still give the usual feedback.
                    self.sh.ui.send(UiCmd::ShowOverlay);
                }
                tracing::info!("replay saved: {path}");
                if !path.is_empty() {
                    let _ = self.sh.clip_tx.send(ClipTask::ReplaySaved {
                        path: PathBuf::from(norm_path(&path)),
                        at_ms: now_ms(),
                        short: pending.map(|p| p.short).unwrap_or(false),
                    });
                }
            }
            "RecordStateChanged" => {
                let state = data["outputState"].as_str().unwrap_or("");
                let path = data["outputPath"].as_str().map(norm_path).unwrap_or_default();
                match state {
                    "OBS_WEBSOCKET_OUTPUT_STARTED" => {
                        self.rec_active = true;
                        if !path.is_empty() {
                            self.rec_path = path;
                        }
                        self.rec_started_ms = now_ms();
                        self.rec_bytes_changed = Instant::now();
                        self.sh.log_event("info", "Recording started");
                        let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
                    }
                    "OBS_WEBSOCKET_OUTPUT_STOPPED" => {
                        self.rec_active = false;
                        self.rec_path.clear();
                        if !self.expecting_exit && !self.exit_started {
                            self.sh.log_event("warn", "Recording stopped");
                        }
                        let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
                    }
                    _ => {}
                }
            }
            "RecordFileChanged" => {
                let path = data["newOutputPath"].as_str().map(norm_path).unwrap_or_default();
                tracing::info!("recording split -> {path}");
                self.rec_path = path;
                self.rec_started_ms = now_ms();
                let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
            }
            "ReplayBufferStateChanged" => {
                self.replay_active = data["outputActive"].as_bool().unwrap_or(false);
            }
            "ExitStarted" => {
                self.exit_started = true;
                tracing::info!("OBS is exiting");
            }
            "CurrentProgramSceneChanged" | "InputRemoved" | "InputNameChanged" | "SceneItemRemoved" => {
                self.inputs_dirty = true;
            }
            "InputVolumeMeters" => self.meters(data),
            _ => {}
        }
    }

    fn meters(&mut self, data: &Value) {
        let mic_name = self.managed.as_ref().and_then(|m| m.mic.clone());
        let mut out = Vec::new();
        if let Some(inputs) = data["inputs"].as_array() {
            for i in inputs {
                let name = i["inputName"].as_str().unwrap_or("").to_string();
                let peak = i["inputLevelsMul"]
                    .as_array()
                    .map(|chs| chs.iter().filter_map(|c| c.get(1).and_then(Value::as_f64)).fold(0.0f64, f64::max))
                    .unwrap_or(0.0) as f32;
                if Some(&name) == mic_name.as_ref() && peak > 0.003 {
                    self.mic_last_sound = Instant::now();
                    self.mic_silence_warned = false;
                }
                out.push(Meter { name, peak });
            }
        }
        if self.sh.has_clients() && self.meters_emitted.elapsed() >= Duration::from_millis(100) {
            self.meters_emitted = Instant::now();
            self.sh.emit(Event::Meters { meters: out });
        }
    }

    // ----------------------------------------------------- commands

    fn command(&mut self, cmd: SupCmd) {
        match cmd {
            SupCmd::SaveClip { short, reply } => {
                let r = self.save_clip(short);
                if let Err(e) = &r {
                    self.sh.toast(ToastLevel::Error, format!("Clip not saved: {e}"));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(r);
                }
            }
            SupCmd::SetRecording { enabled, reply } => {
                let r = self.set_recording(enabled);
                if let Some(reply) = reply {
                    let _ = reply.send(r);
                }
            }
            SupCmd::RestartObs { reply } => {
                self.closed_by_user = false;
                self.launches.clear();
                if self.pid.is_some() {
                    self.restart_obs("requested");
                }
                self.tick();
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            SupCmd::ConfigChanged { restart_obs } => {
                self.inputs_dirty = true;
                *self.sh.tools.write() = None;
                if restart_obs && self.pid.is_some() {
                    self.restart_obs("settings changed");
                } else {
                    self.ensure_inputs();
                }
            }
            SupCmd::Mic { decision } => {
                if self.mic.as_ref() != Some(&decision) {
                    tracing::info!("mic decision: {decision:?}");
                    self.mic = Some(decision);
                    self.apply_audio();
                }
            }
            SupCmd::DesktopDevice { device_id } => {
                if self.desktop_device.as_deref() != Some(&device_id) {
                    self.desktop_device = Some(device_id);
                    self.apply_audio();
                }
            }
            SupCmd::AppRouting(r) => {
                if r != self.routing {
                    self.routing = r;
                    self.apply_audio();
                }
            }
            SupCmd::GameTarget(g) => {
                if self.game.as_ref() != Some(&g) {
                    self.game = Some(g);
                    self.apply_audio();
                }
            }
            SupCmd::Shutdown => {}
        }
    }

    fn save_clip(&mut self, short: bool) -> Result<(), String> {
        let cfg = self.sh.cfg();
        if let Some(t) = self.last_save_request {
            if t.elapsed() < Duration::from_millis(cfg.clips.save_cooldown_ms as u64) {
                return Ok(()); // double press: ignore silently
            }
        }
        let Some(obs) = self.connected() else {
            return Err(if self.pid.is_none() { "OBS is not running".into() } else { "not connected to OBS yet".into() });
        };
        if !self.replay_active {
            let _ = obs.request("StartReplayBuffer", None);
            return Err("the replay buffer was off; it's starting now, try again in a moment".into());
        }
        obs.request("SaveReplayBuffer", None).map_err(|e| e.to_string())?;
        self.last_save_request = Some(Instant::now());
        self.pending_save = Some(PendingSave { short, at: Instant::now() });
        self.sh.ui.send(UiCmd::ShowOverlay);
        Ok(())
    }

    fn set_recording(&mut self, enabled: bool) -> Result<(), String> {
        self.recording_wanted = enabled;
        if let Ok(db) = cv_core::db::Db::open_default() {
            let _ = db.meta_set(META_RECORDING_WANTED, if enabled { "1" } else { "0" });
        }
        let msg = if enabled { "24/7 recording resumed" } else { "24/7 recording paused" };
        self.sh.log_event("info", msg);
        self.sh.toast(ToastLevel::Info, msg);
        if let Some(obs) = self.connected() {
            if enabled {
                self.last_start_attempt = None;
                let cfg = self.sh.cfg();
                self.ensure_outputs(&cfg);
            } else if self.rec_active {
                obs.request("StopRecord", None).map_err(|e| e.to_string())?;
            }
        }
        self.publish_status();
        Ok(())
    }

    /// Stop outputs, close OBS gracefully (or end it), and let the next tick relaunch.
    fn restart_obs(&mut self, why: &str) {
        let Some(pid) = self.pid else { return };
        tracing::info!("restarting OBS ({why})");
        self.expecting_exit = true;
        if let Some(obs) = self.connected() {
            let _ = obs.request("StopReplayBuffer", None);
            if self.rec_active {
                let _ = obs.request("StopRecord", None);
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(200));
                    match obs.request("GetRecordStatus", None) {
                        Ok(v) if v["outputActive"].as_bool() == Some(false) => break,
                        Err(_) => break,
                        _ => {}
                    }
                }
            }
        }
        self.drop_connection();
        process::request_close_obs(pid);
        let timeout = Duration::from_secs(self.sh.cfg.read().obs.shutdown_timeout_secs as u64);
        let t = Instant::now();
        while process::is_running(pid) && t.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(250));
        }
        if process::is_running(pid) {
            tracing::warn!("OBS did not close in time; ending it");
            process::terminate(pid);
            std::thread::sleep(Duration::from_millis(1500));
        }
        self.pid = None;
        self.expecting_exit = false;
        self.exit_started = false;
        self.rec_active = false;
        self.replay_active = false;
        self.closed_by_user = false;
        // Relaunch right away (settings are written in launch()).
        if self.should_launch(&self.sh.cfg()) || self.launches.is_empty() {
            self.launch();
        }
    }

    // ------------------------------------------------------- status

    fn publish_status(&mut self) {
        let cfg = self.sh.cfg();
        let running = self.pid.is_some();
        let connected = self.connected().is_some();
        let mic_state;
        let mic_device;
        match (&self.mic, cfg.audio.mic_guard) {
            (_, false) => {
                mic_state = "disabled";
                mic_device = String::new();
            }
            (Some(MicDecision::Preferred { rank, endpoint }), _) => {
                mic_state = if *rank == 0 { "ok" } else { "fallback" };
                mic_device = cv_core::audio_names::clean_display_name(&endpoint.name);
            }
            (Some(MicDecision::WindowsDefault), _) => {
                mic_state = if cfg.audio.mic_priority.is_empty() { "ok" } else { "fallback" };
                mic_device = "Windows default".into();
            }
            (Some(MicDecision::Leave), _) => {
                mic_state = "missing";
                mic_device = String::new();
            }
            (None, _) => {
                mic_state = "unknown";
                mic_device = String::new();
            }
        }
        let tracks = if cfg.audio.multitrack { 6 } else { 1 };
        let healthy = self.rec_active && self.rec_bytes_changed.elapsed() < Duration::from_secs(cfg.recording.stall_timeout_secs as u64);
        self.sh.update_status(|s| {
            s.obs.running = running;
            s.obs.connected = connected;
            s.obs.closed_by_user = self.closed_by_user;
            if self.last_error.is_empty() {
                s.obs.last_error.clear();
            }
            s.recording.active = self.rec_active;
            s.recording.paused_by_user = !self.recording_wanted || !cfg.recording.enabled;
            s.recording.path = self.rec_path.clone();
            s.recording.started_ms = self.rec_started_ms;
            s.recording.bytes = self.rec_bytes;
            s.recording.healthy = healthy;
            s.recording.tracks = tracks;
            s.replay.active = self.replay_active;
            s.replay.seconds = cfg.clips.clip_length_secs;
            s.mic.state = mic_state.into();
            s.mic.device = mic_device;
            s.mic.preferred = cfg.audio.mic_priority.first().cloned().unwrap_or_default();
            s.game.audio_target = self.game.as_ref().map(|g| g.name.clone());
        });
    }
}

/// Newest non-replay video in the recordings folder.
fn newest_recording(folder: &str) -> Option<String> {
    let rd = std::fs::read_dir(folder).ok()?;
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| cv_core::obs::filename::is_video_file(p) && !cv_core::obs::filename::is_replay_file(p))
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
        .map(|p| p.to_string_lossy().to_string())
}

pub fn send_and_wait(tx: &Sender<SupCmd>, make: impl FnOnce(Sender<Result<(), String>>) -> SupCmd, timeout: Duration) -> Result<(), String> {
    let (rtx, rrx) = crossbeam_channel::bounded(1);
    tx.send(make(rtx)).map_err(|_| "service is shutting down".to_string())?;
    rrx.recv_timeout(timeout).map_err(|_| "timed out".to_string())?
}

#[allow(dead_code)]
fn _unused(_: Value) -> Value {
    json!(null)
}
