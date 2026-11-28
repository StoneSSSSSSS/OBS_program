//! Runs ClipVault's own recorder (screen + every app's sound + microphone),
//! keeps it healthy, files its recordings in the library, and turns the clip
//! hotkey into clips.
//!
//! When the 24/7 recording is paused, the recorder keeps a short rolling
//! buffer instead (in the data folder) so the clip hotkey still works.

use crate::shared::{ClipTask, EngineCmd, Footage, LibCmd, Reply, Shared};
use crate::win::ui::{ToastLevel, UiCmd};
use crossbeam_channel::{select, Receiver};
use cv_core::audio_names::MicDecision;
use cv_core::config::Config;
use cv_core::db::Db;
use cv_core::ipc::{Event, Meter};
use cv_core::util::now_ms;
use cv_media::capture::CaptureMethod;
use cv_media::mf::h264::{EncoderChoice, RateControl};
use cv_media::recorder::{AppIdent, AppNamer, MonitorChoice, RecEvent, Recorder, RecorderConfig};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const META_RECORDING_WANTED: &str = "recording_wanted";

pub fn buffer_dir() -> PathBuf {
    cv_core::paths::data_dir().join("buffer")
}

/// Names and categorises apps for the recorder's per-app tracks.
pub struct Namer {
    sh: Arc<Shared>,
    cache: parking_lot::Mutex<HashMap<String, (AppIdent, Instant)>>,
}

impl AppNamer for Namer {
    fn identify(&self, exe: &str, pid: u32) -> AppIdent {
        let key = exe.to_lowercase();
        if let Some((a, t)) = self.cache.lock().get(&key) {
            if t.elapsed() < Duration::from_secs(60) {
                return a.clone();
            }
        }
        let cfg = self.sh.cfg();
        let path = crate::win::process::exe_path_of(pid).unwrap_or_default();
        let db = Db::open_default().ok();
        let game = db.as_ref().and_then(|d| d.game_by_key(&key).ok().flatten());
        let category = match cfg.audio.category_of(&key) {
            Some(c) => c.to_string(),
            None => {
                let is_game = match &game {
                    Some(g) => g.is_game(),
                    None => {
                        let overrides = db.as_ref().and_then(|d| d.game_overrides().ok()).unwrap_or_default();
                        let info = cv_core::games::WindowInfo { exe_path: path.clone(), exe_name: key.clone(), ..Default::default() };
                        cv_core::games::Classifier::new(&cfg.games, overrides).classify(&info).is_game()
                    }
                };
                if is_game { "game" } else { "other" }.to_string()
            }
        };
        let name = match &game {
            Some(g) => g.name.clone(),
            None => {
                let steam = cv_core::games::steam_name_for_exe(&path);
                let (product, desc) = crate::win::foreground::version_strings(&path);
                cv_core::games::friendly_name(&key, &path, steam.as_deref(), product.as_deref(), desc.as_deref())
            }
        };
        let a = AppIdent { key: key.clone(), name, category };
        self.cache.lock().insert(key, (a.clone(), Instant::now()));
        a
    }
}

struct Engine {
    sh: Arc<Shared>,
    rec: Option<Recorder>,
    events_tx: crossbeam_channel::Sender<RecEvent>,
    events_rx: Receiver<RecEvent>,
    namer: Arc<Namer>,
    wanted: bool,
    buffer_mode: bool,
    /// Files written by the current recorder (newest last).
    footage: Vec<Footage>,
    mic: Option<MicDecision>,
    last_save: Option<Instant>,
    starts: VecDeque<Instant>,
    retry_at: Option<Instant>,
    progress: (i64, Instant),
    mic_last_sound: Instant,
    mic_warned: bool,
    warned: HashMap<String, Instant>,
    last_buffer_clean: Instant,
}

pub fn run(sh: Arc<Shared>, rx: Receiver<EngineCmd>) {
    cv_media::util::disable_background_throttling();
    let wanted = Db::open_default().ok().and_then(|db| db.meta_get(META_RECORDING_WANTED).ok().flatten()).map(|v| v != "0").unwrap_or(true);
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let namer = Arc::new(Namer { sh: sh.clone(), cache: Default::default() });
    let mut e = Engine {
        sh,
        rec: None,
        events_tx,
        events_rx,
        namer,
        wanted,
        buffer_mode: false,
        footage: Vec::new(),
        mic: None,
        last_save: None,
        starts: VecDeque::new(),
        retry_at: None,
        progress: (0, Instant::now()),
        mic_last_sound: Instant::now(),
        mic_warned: false,
        warned: HashMap::new(),
        last_buffer_clean: Instant::now(),
    };
    // Give the mic guard a moment to pick the microphone first.
    std::thread::sleep(Duration::from_millis(600));
    while let Ok(cmd) = rx.try_recv() {
        if let EngineCmd::Mic { decision } = cmd {
            e.mic = Some(decision);
        }
    }
    e.start();
    let tick = crossbeam_channel::tick(Duration::from_secs(1));
    loop {
        let ev_rx = e.events_rx.clone();
        select! {
            recv(rx) -> cmd => match cmd {
                Ok(EngineCmd::Shutdown) | Err(_) => break,
                Ok(cmd) => e.command(cmd),
            },
            recv(ev_rx) -> ev => if let Ok(ev) = ev { e.event(ev) },
            recv(tick) -> _ => e.tick(),
        }
        if e.sh.is_shutting_down() {
            break;
        }
    }
    e.stop();
    // File the last segment.
    while let Ok(ev) = e.events_rx.try_recv() {
        e.event(ev);
    }
    tracing::info!("recording engine stopped");
}

fn reply(r: Reply, v: Result<(), String>) {
    if let Some(r) = r {
        let _ = r.send(v);
    }
}

impl Engine {
    fn recorder_config(&self, cfg: &Config) -> RecorderConfig {
        let r = &cfg.recording;
        let monitor = if r.monitor.eq_ignore_ascii_case("primary") { MonitorChoice::Primary } else { MonitorChoice::Device(r.monitor.clone()) };
        // Estimate the encoded size to pick the bitrate.
        let outs = cv_media::d3d::list_outputs();
        let out = outs.iter().find(|o| o.device_name.eq_ignore_ascii_case(&r.monitor)).or_else(|| outs.iter().find(|o| o.primary)).or(outs.first());
        let (w, h) = out.map(|o| cv_media::d3d::fit_size((o.width(), o.height()), r.max_height)).unwrap_or((1920, 1080));
        let kbps = r.video_kbps(w, h);
        let mic = if !cfg.audio.record_mic {
            None
        } else {
            match &self.mic {
                Some(MicDecision::Preferred { endpoint, .. }) => Some(Some(endpoint.id.clone())),
                Some(MicDecision::Leave) => None,
                Some(MicDecision::WindowsDefault) | None => Some(None),
            }
        };
        let (folder, segment_secs) = if self.wanted && r.enabled { (PathBuf::from(&r.folder), r.segment_minutes * 60) } else { (buffer_dir(), 60) };
        RecorderConfig {
            folder,
            segment_secs,
            fps: r.fps,
            max_height: r.max_height,
            monitor,
            capture_method: match r.capture_method.as_str() {
                "duplication" => CaptureMethod::Duplication,
                "graphics_capture" => CaptureMethod::GraphicsCapture,
                _ => CaptureMethod::Auto,
            },
            encoder: if r.encoder == "software" { EncoderChoice::Software } else { EncoderChoice::Auto },
            rate: RateControl::Vbr { kbps, max_kbps: kbps * 3 / 2 },
            keyframe_secs: r.keyframe_interval_secs as f32,
            cursor: r.show_cursor,
            audio_kbps: r.audio_bitrate_kbps,
            app_kbps: r.app_bitrate_kbps,
            app_slots: r.app_track_count,
            record_apps: r.app_tracks,
            mic,
            mic_gain: cfg.audio.mic_gain,
            exclude_pids: vec![std::process::id()],
        }
    }

    fn start(&mut self) {
        if self.rec.is_some() {
            return;
        }
        let cfg = self.sh.cfg();
        self.buffer_mode = !(self.wanted && cfg.recording.enabled);
        let rc = self.recorder_config(&cfg);
        let folder = rc.folder.clone();
        match Recorder::start(rc, self.namer.clone(), self.events_tx.clone()) {
            Ok(r) => {
                self.rec = Some(r);
                self.footage.clear();
                self.progress = (0, Instant::now());
                self.starts.push_back(Instant::now());
                while self.starts.front().map(|t| t.elapsed() > Duration::from_secs(3600)).unwrap_or(false) {
                    self.starts.pop_front();
                }
                self.retry_at = None;
                let what = if self.buffer_mode { "clip buffer only (24/7 paused)" } else { "24/7 recording" };
                self.sh.log_event("info", format!("Recording started: {what} to {}", folder.display()));
            }
            Err(e) => {
                let msg = format!("Recording couldn't start: {e:#}");
                self.sh.update_status(|s| s.engine.last_error = msg.clone());
                self.warn_once(&msg);
                self.retry_at = Some(Instant::now() + Duration::from_secs(10));
            }
        }
        self.publish();
    }

    fn stop(&mut self) {
        if let Some(r) = self.rec.take() {
            r.stop();
        }
        *self.sh.live_path.lock() = None;
        self.sh.update_status(|s| {
            s.engine.running = false;
            s.recording.active = false;
            s.replay.active = false;
        });
    }

    fn restart(&mut self, why: &str) {
        tracing::info!("restarting the recorder ({why})");
        self.stop();
        // Collect the finished segment events before starting again.
        while let Ok(ev) = self.events_rx.try_recv() {
            self.event(ev);
        }
        self.start();
    }

    fn warn_once(&mut self, msg: &str) {
        let due = self.warned.get(msg).map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(true);
        if due {
            self.warned.insert(msg.to_string(), Instant::now());
            self.sh.log_event("warn", msg);
            self.sh.toast(ToastLevel::Warning, msg);
        }
    }

    fn mic_choice(&self, cfg: &Config) -> Option<Option<String>> {
        if !cfg.audio.record_mic {
            return None;
        }
        match &self.mic {
            Some(MicDecision::Preferred { endpoint, .. }) => Some(Some(endpoint.id.clone())),
            Some(MicDecision::Leave) => None,
            _ => Some(None),
        }
    }

    fn command(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::SaveClip { short, reply: r } => {
                let res = self.save_clip(short);
                if let Err(e) = &res {
                    self.sh.toast(ToastLevel::Error, format!("Clip not saved: {e}"));
                }
                reply(r, res);
            }
            EngineCmd::SetRecording { enabled, reply: r } => {
                self.wanted = enabled;
                if let Ok(db) = Db::open_default() {
                    let _ = db.meta_set(META_RECORDING_WANTED, if enabled { "1" } else { "0" });
                }
                let msg = if enabled { "24/7 recording resumed" } else { "24/7 recording paused (the clip hotkey still works)" };
                self.sh.log_event("info", msg);
                self.sh.toast(ToastLevel::Info, msg);
                self.restart("24/7 toggled");
                reply(r, Ok(()));
            }
            EngineCmd::Restart { reply: r } => {
                self.starts.clear();
                self.restart("requested");
                reply(r, if self.rec.is_some() { Ok(()) } else { Err(self.sh.status.lock().engine.last_error.clone()) });
            }
            EngineCmd::ConfigChanged { restart } => {
                let cfg = self.sh.cfg();
                if restart || (self.buffer_mode != !(self.wanted && cfg.recording.enabled)) {
                    self.restart("settings changed");
                } else if let Some(r) = &self.rec {
                    r.set_mic(self.mic_choice(&cfg), cfg.audio.mic_gain);
                }
            }
            EngineCmd::Mic { decision } => {
                if self.mic.as_ref() != Some(&decision) {
                    tracing::info!("mic decision: {decision:?}");
                    let announce = match &decision {
                        MicDecision::Preferred { endpoint, .. } if self.mic.is_some() => Some(format!("Recording mic: {}", cv_core::audio_names::clean_display_name(&endpoint.name))),
                        MicDecision::WindowsDefault if self.mic.is_some() => Some("Preferred mic not connected; recording the Windows default mic".to_string()),
                        _ => None,
                    };
                    self.mic = Some(decision);
                    let cfg = self.sh.cfg();
                    if let Some(r) = &self.rec {
                        r.set_mic(self.mic_choice(&cfg), cfg.audio.mic_gain);
                    }
                    if let Some(m) = announce {
                        self.sh.log_event("info", &m);
                        self.sh.toast(ToastLevel::Info, m);
                    }
                }
            }
            EngineCmd::Shutdown => {}
        }
    }

    fn save_clip(&mut self, short: bool) -> Result<(), String> {
        let cfg = self.sh.cfg();
        if let Some(t) = self.last_save {
            if t.elapsed() < Duration::from_millis(cfg.clips.save_cooldown_ms as u64) {
                return Ok(()); // double press
            }
        }
        let Some(rec) = &self.rec else {
            return Err("recording isn't running".into());
        };
        self.last_save = Some(Instant::now());
        self.sh.ui.send(UiCmd::ShowOverlay);
        let end_ms = rec.flush(Duration::from_secs(6)).map_err(|e| format!("{e:#}"))?;
        let secs = if short { cfg.clips.short_clip_secs } else { cfg.clips.clip_length_secs };
        let from_ms = end_ms - secs as i64 * 1000;
        let footage = if self.buffer_mode {
            let mut f = self.footage.clone();
            if let Some(last) = f.last_mut() {
                last.duration_s = last.duration_s.max((end_ms - last.start_ms) as f64 / 1000.0);
            }
            f
        } else {
            let db = Db::open_default().map_err(|e| format!("{e:#}"))?;
            db.list_recordings()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.end_ms() > from_ms - 1000 || r.is_live())
                .map(|r| Footage { duration_s: if r.is_live() { (end_ms - r.start_ms) as f64 / 1000.0 } else { r.duration }, path: PathBuf::from(r.path), start_ms: r.start_ms })
                .collect()
        };
        let _ = self.sh.clip_tx.send(ClipTask::SaveRange { footage, from_ms, to_ms: end_ms, short });
        Ok(())
    }

    fn event(&mut self, ev: RecEvent) {
        match ev {
            RecEvent::SegmentStarted { path, start_ms } => {
                *self.sh.live_path.lock() = Some(path.clone());
                self.footage.push(Footage { path: path.clone(), start_ms, duration_s: 0.0 });
                if !self.buffer_mode {
                    if let Ok(db) = Db::open_default() {
                        let _ = db.upsert_recording(&path.to_string_lossy(), start_ms, 0, "recording");
                    }
                    self.sh.emit(Event::RecordingsChanged);
                }
            }
            RecEvent::SegmentFinished { path, start_ms, duration_s, bytes } => {
                {
                    let mut live = self.sh.live_path.lock();
                    if live.as_ref() == Some(&path) {
                        *live = None;
                    }
                }
                if let Some(f) = self.footage.iter_mut().find(|f| f.path == path) {
                    f.duration_s = duration_s;
                }
                if !self.buffer_mode && path.starts_with(&self.sh.cfg().recording.folder) {
                    if let Ok(db) = Db::open_default() {
                        if let Ok(id) = db.upsert_recording(&path.to_string_lossy(), start_ms, bytes as i64, "complete") {
                            let tracks = cv_media::probe::probe(&path).map(|i| i.audio.len() as i64).unwrap_or(0);
                            let _ = db.set_recording_probe(id, duration_s, tracks, true);
                        }
                    }
                    self.sh.emit(Event::RecordingsChanged);
                    let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
                }
            }
            RecEvent::Slots { slots, .. } => {
                tracing::debug!("app tracks: {:?}", slots.iter().map(|s| (&s.app.name, s.slot)).collect::<Vec<_>>());
                if !self.buffer_mode {
                    self.sh.emit(Event::RecordingsChanged);
                }
            }
            RecEvent::Warning(m) => self.warn_once(&m),
        }
    }

    fn tick(&mut self) {
        if self.rec.is_none() {
            if self.retry_at.map(|t| Instant::now() >= t).unwrap_or(true) && !self.sh.is_shutting_down() {
                self.start();
            }
            self.publish();
            return;
        }
        let st = self.rec.as_ref().unwrap().status();
        // Stalled? (nothing written for a minute although the screen is fine)
        if st.written_until_ms != self.progress.0 {
            self.progress = (st.written_until_ms, Instant::now());
        } else if st.video_error.is_none() && self.progress.1.elapsed() > Duration::from_secs(60) && self.starts.len() < 10 {
            self.sh.log_event("warn", "The recording stopped making progress; restarting it");
            self.restart("stalled");
            return;
        }
        if let Some(last) = self.footage.last_mut() {
            if st.written_until_ms > last.start_ms {
                last.duration_s = (st.written_until_ms - last.start_ms) as f64 / 1000.0;
            }
        }
        // Mic silence warning.
        let cfg = self.sh.cfg();
        if st.levels.mic > 0.003 {
            self.mic_last_sound = Instant::now();
            self.mic_warned = false;
        }
        let mins = cfg.audio.mic_silence_warning_mins;
        if mins > 0 && !self.mic_warned && self.mic_last_sound.elapsed() > Duration::from_secs(mins as u64 * 60) {
            self.mic_warned = true;
            self.sh.ui.send(UiCmd::Balloon { level: ToastLevel::Warning, title: "Your mic is silent".into(), text: format!("No sound from the microphone for {mins} minutes.") });
        }
        // Rolling clip buffer: keep only what the hotkey can use.
        if self.buffer_mode && self.last_buffer_clean.elapsed() > Duration::from_secs(30) {
            self.last_buffer_clean = Instant::now();
            self.clean_buffer(&cfg);
        }
        // Meters for the GUI.
        if self.sh.has_clients() {
            let mut meters = vec![Meter { name: "Full mix".into(), peak: st.levels.full }, Meter { name: "Microphone".into(), peak: st.levels.mic }];
            for (app, p) in &st.levels.apps {
                meters.push(Meter { name: app.name.clone(), peak: *p });
            }
            self.sh.emit(Event::Meters { meters });
        }
        self.publish();
    }

    fn clean_buffer(&mut self, cfg: &Config) {
        let keep_ms = (cfg.clips.clip_length_secs.max(cfg.clips.short_clip_secs) as i64 + 180) * 1000;
        let cutoff = now_ms() - keep_ms;
        let live = self.sh.live_path.lock().clone();
        self.footage.retain(|f| {
            let end = f.start_ms + (f.duration_s * 1000.0) as i64;
            let old = end < cutoff;
            if old && Some(&f.path) != live.as_ref() {
                let _ = std::fs::remove_file(&f.path);
                let _ = std::fs::remove_file(cv_core::waveform::wave_path(&f.path));
                false
            } else {
                true
            }
        });
        // Leftovers from earlier runs.
        if let Ok(rd) = std::fs::read_dir(buffer_dir()) {
            for e in rd.flatten() {
                let p = e.path();
                if Some(&p) == live.as_ref() || self.footage.iter().any(|f| f.path == p) {
                    continue;
                }
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    fn publish(&self) {
        let cfg = self.sh.cfg();
        let rec = self.rec.as_ref().map(|r| r.status());
        let paused = !self.wanted || !cfg.recording.enabled;
        self.sh.update_status(|s| {
            match &rec {
                Some(st) => {
                    let healthy = st.video_error.is_none() && now_ms() - st.written_until_ms < 10_000;
                    s.engine.running = true;
                    s.engine.encoder = st.encoder.clone();
                    s.engine.capture = st.capture_method.clone();
                    s.engine.width = st.width;
                    s.engine.height = st.height;
                    s.engine.fps = st.fps;
                    s.engine.dropped_frames = st.dropped_frames;
                    s.engine.last_error = st.video_error.clone().unwrap_or_default();
                    s.recording.active = !paused && st.path.is_some();
                    s.recording.path = if paused { String::new() } else { st.path.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default() };
                    s.recording.started_ms = st.segment_start_ms;
                    s.recording.bytes = st.bytes as i64;
                    s.recording.healthy = healthy;
                    s.recording.tracks = 2 + if cfg.recording.app_tracks { cfg.recording.app_track_count } else { 0 };
                    s.replay.active = healthy;
                    s.mic.muted = !st.mic_ok && cfg.audio.record_mic;
                }
                None => {
                    s.engine.running = false;
                    s.recording.active = false;
                    s.replay.active = false;
                }
            }
            s.recording.paused_by_user = paused;
            s.replay.seconds = cfg.clips.clip_length_secs;
            let (state, device) = match (&self.mic, cfg.audio.record_mic) {
                (_, false) => ("disabled", String::new()),
                (Some(MicDecision::Preferred { rank, endpoint }), _) => (if *rank == 0 { "ok" } else { "fallback" }, cv_core::audio_names::clean_display_name(&endpoint.name)),
                (Some(MicDecision::WindowsDefault), _) => (if cfg.audio.mic_priority.is_empty() { "ok" } else { "fallback" }, "Windows default".to_string()),
                (Some(MicDecision::Leave), _) => ("missing", String::new()),
                (None, _) => ("ok", "Windows default".to_string()),
            };
            s.mic.state = state.into();
            s.mic.device = device;
            s.mic.preferred = cfg.audio.mic_priority.first().cloned().unwrap_or_default();
        });
    }
}

/// Send a command and wait for its reply.
pub fn send_and_wait(sh: &Shared, make: impl FnOnce(crossbeam_channel::Sender<Result<(), String>>) -> EngineCmd, timeout: Duration) -> Result<(), String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    sh.engine_tx.send(make(tx)).map_err(|_| "service is shutting down".to_string())?;
    rx.recv_timeout(timeout).map_err(|_| "timed out".to_string())?
}
