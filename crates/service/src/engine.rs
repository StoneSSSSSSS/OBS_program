//! Runs ClipVault's own recorder (screen + every app's sound + microphone),
//! keeps it healthy, files its recordings in the library, and turns the clip
//! hotkey into clips.
//!
//! When the 24/7 recording is paused, the recorder keeps a short rolling
//! buffer instead (in the data folder) so the clip hotkey still works.

use crate::shared::{media_path_allowed, require_media_path, ClipTask, EngineCmd, Footage, LibCmd, Reply, Shared};
use crate::win::ui::{ToastLevel, UiCmd};
use crossbeam_channel::{select, Receiver};
use cv_core::audio_names::MicDecision;
use cv_core::config::{Config, ROLLING_SEGMENT_SECS};
use cv_core::db::Db;
use cv_core::ipc::{Event, Meter};
use cv_core::util::now_ms;
use cv_media::capture::CaptureMethod;
use cv_media::mf::h264::{EncoderChoice, RateControl};
use cv_media::recorder::{AppIdent, AppNamer, MonitorChoice, RecEvent, Recorder, RecorderConfig};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const META_RECORDING_WANTED: &str = "recording_wanted";
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const RESTART_WINDOW: Duration = Duration::from_secs(3600);
const MAX_STALL_RESTARTS: usize = 10;

fn stall_restart_allowed(starts: &mut VecDeque<Instant>, now: Instant) -> bool {
    starts.retain(|start| now.saturating_duration_since(*start) < RESTART_WINDOW);
    starts.len() < MAX_STALL_RESTARTS
}


pub fn buffer_dir() -> PathBuf {
    cv_core::paths::recorder_buffer_dir()
}

fn rolling_dir() -> PathBuf {
    cv_core::paths::rolling_buffer_dir()
}

fn internal_buffer_root_ok(dir: &Path) -> bool {
    if !media_path_allowed(dir) {
        return false;
    }
    let Some(parent) = dir.parent().and_then(|path| path.canonicalize().ok()) else { return false };
    let Some(name) = dir.file_name() else { return false };
    match dir.canonicalize() {
        Ok(actual) => crate::shared::same_path(&actual.to_string_lossy(), &parent.join(name).to_string_lossy()),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}


#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordingMode {
    Archive,
    Paused,
    Rolling,
}

impl RecordingMode {
    fn segment_secs(self, cfg: &Config) -> u32 {
        match self {
            Self::Archive => cfg.recording.archive_segment_seconds(),
            Self::Paused => 60,
            Self::Rolling => ROLLING_SEGMENT_SECS,
        }
    }
}

fn recording_mode(cfg: &Config, wanted: bool) -> RecordingMode {
    if cfg.recording.rolling_seconds().is_some() {
        RecordingMode::Rolling
    } else if wanted && cfg.recording.enabled {
        RecordingMode::Archive
    } else {
        RecordingMode::Paused
    }
}

fn clip_seconds(requested: u32, rolling_seconds: Option<u32>) -> u32 {
    requested.max(1).min(rolling_seconds.unwrap_or(u32::MAX))
}

/// Files are removed at the end of their last frame, so a partially retained
/// segment may overlap the expiry boundary without extending the saved clip.
fn rolling_expired(segment_end_ms: i64, cutoff_ms: Option<i64>) -> bool {
    cutoff_ms.is_none_or(|cutoff| segment_end_ms <= cutoff)
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
    mode: RecordingMode,
    clip_leases: Vec<(Weak<()>, Vec<PathBuf>)>,
    /// Recent files written by this engine, including footage across mode changes.
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

fn new_engine(sh: Arc<Shared>, wanted: bool) -> Engine {
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let namer = Arc::new(Namer { sh: sh.clone(), cache: Default::default() });
    Engine {
        sh,
        rec: None,
        events_tx,
        events_rx,
        namer,
        wanted,
        mode: RecordingMode::Paused,
        clip_leases: Vec::new(),
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
    }
}

pub fn run(sh: Arc<Shared>, rx: Receiver<EngineCmd>) {
    cv_media::util::disable_background_throttling();
    let wanted = Db::open_default().ok().and_then(|db| db.meta_get(META_RECORDING_WANTED).ok().flatten()).map(|v| v != "0").unwrap_or(true);
    let mut e = new_engine(sh, wanted);
    // Give the mic guard a moment to pick the microphone first.
    std::thread::sleep(Duration::from_millis(600));
    let mut pending = Vec::new();
    for cmd in rx.try_iter() {
        match cmd {
            EngineCmd::Mic { decision } => e.mic = Some(decision),
            EngineCmd::Shutdown => return,
            cmd => pending.push(cmd),
        }
    }
    if e.sh.is_shutting_down() {
        return;
    }
    // A fresh service session starts a fresh temporary replay buffer.
    e.clean_rolling(None);
    e.start();
    for cmd in pending {
        e.command(cmd);
    }
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
    e.clean_rolling(None);
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
        let folder = match self.mode {
            RecordingMode::Archive => PathBuf::from(&r.folder),
            RecordingMode::Paused => buffer_dir(),
            RecordingMode::Rolling => rolling_dir(),
        };
        RecorderConfig {
            folder,
            segment_secs: self.mode.segment_secs(cfg),
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
            keyframe_secs: if self.mode == RecordingMode::Rolling {
                r.keyframe_interval_secs.min(2) as f32
            } else {
                r.keyframe_interval_secs as f32
            },
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
        self.mode = recording_mode(&cfg, self.wanted);
        let rc = self.recorder_config(&cfg);
        let folder = rc.folder.clone();
        let capture = if let Err(error) = cfg.validate_media_folders() {
            Err(error)
        } else if self.mode != RecordingMode::Archive && !internal_buffer_root_ok(&folder) {
            Err(anyhow::anyhow!("ClipVault’s temporary buffer must stay in its profile directory"))
        } else {
            require_media_path(&folder).and_then(|_| Recorder::start(rc, self.namer.clone(), self.events_tx.clone()))
        };
        match capture {
            Ok(r) => {
                self.rec = Some(r);
                self.progress = (0, Instant::now());
                self.sh.update_status(|s| s.engine.last_error.clear());
                self.retry_at = None;
                let what = match self.mode {
                    RecordingMode::Archive => "24/7 recording",
                    RecordingMode::Paused => "clip buffer only (24/7 paused)",
                    RecordingMode::Rolling => "temporary replay only",
                };
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
            EngineCmd::SaveRecentRolling { seconds, reply: r } => {
                let result = if self.mode == RecordingMode::Rolling {
                    self.save_requested_clip(seconds, true)
                } else {
                    Err("Replay only is not active".into())
                };
                if let Err(error) = &result {
                    self.sh.toast(ToastLevel::Error, format!("Clip not saved: {error}"));
                }
                reply(r, result);
            }
            EngineCmd::SetRecording { enabled, reply: r } => self.set_recording(enabled, r),
            EngineCmd::ToggleRecording => {
                // Compute from the engine's state when this command is dequeued.
                let enabled = self.mode != RecordingMode::Archive;
                self.set_recording(enabled, None);
            }
            EngineCmd::Restart { reply: r } => {
                self.starts.clear();
                self.restart("requested");
                reply(r, if self.rec.is_some() { Ok(()) } else { Err(self.sh.status.lock().engine.last_error.clone()) });
            }
            EngineCmd::ConfigChanged { restart } => {
                let cfg = self.sh.cfg();
                if restart || self.mode != recording_mode(&cfg, self.wanted) {
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

    fn set_recording(&mut self, enabled: bool, r: Reply) {
        if self.mode == RecordingMode::Rolling {
            let message = "Replay only is active. Choose Full history in Settings to save continuous recordings.".to_string();
            self.sh.toast(ToastLevel::Info, &message);
            reply(r, Err(message));
            return;
        }
        if enabled && !self.sh.cfg().recording.enabled {
            let msg = "Turn on 24/7 recording in Settings before resuming".to_string();
            self.sh.toast(ToastLevel::Warning, &msg);
            reply(r, Err(msg));
            return;
        }
        let changed = self.wanted != enabled;
        let stored = Db::open_default().and_then(|db| {
            db.meta_set(META_RECORDING_WANTED, if enabled { "1" } else { "0" })
        });
        if let Err(error) = stored {
            let msg = format!("Could not save the recording preference: {error:#}");
            self.sh.toast(ToastLevel::Error, &msg);
            reply(r, Err(msg));
            return;
        }
        self.wanted = enabled;
        if changed || self.rec.is_none() {
            self.restart("24/7 toggled");
        }
        if self.rec.is_none() {
            reply(r, Err(self.sh.status.lock().engine.last_error.clone()));
            return;
        }
        let msg = if enabled { "24/7 recording resumed" } else { "24/7 recording paused (the clip hotkey still works)" };
        self.sh.log_event("info", msg);
        self.sh.toast(ToastLevel::Info, msg);
        reply(r, Ok(()));
    }

    fn save_clip(&mut self, short: bool) -> Result<(), String> {
        let cfg = self.sh.cfg();
        let requested = if short { cfg.clips.short_clip_secs } else { cfg.clips.clip_length_secs };
        self.save_requested_clip(requested, short)
    }

    fn save_requested_clip(&mut self, requested: u32, short: bool) -> Result<(), String> {
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
        self.queue_flushed_clip(requested, short, end_ms)
    }

    fn queue_flushed_clip(&mut self, requested: u32, short: bool, end_ms: i64) -> Result<(), String> {
        let cfg = self.sh.cfg();
        let rolling_seconds = if self.mode == RecordingMode::Rolling { cfg.recording.rolling_seconds() } else { None };
        let secs = clip_seconds(requested, rolling_seconds);
        let from_ms = end_ms - i64::from(secs) * 1000;
        // The flush can rotate the file. Consume those events before taking a snapshot.
        while let Ok(event) = self.events_rx.try_recv() {
            self.event(event);
        }
        let mut footage = self.footage.clone();
        if let Some(live) = self.sh.live_path.lock().as_ref() {
            if let Some(segment) = footage.iter_mut().find(|segment| &segment.path == live) {
                segment.duration_s = segment.duration_s.max((end_ms - segment.start_ms) as f64 / 1000.0);
            }
        }
        if self.mode == RecordingMode::Rolling {
            let rolling = rolling_dir();
            footage.retain(|segment| segment.path.starts_with(&rolling));
        } else {
            // Use the library for archived history, including earlier service runs.
            let rolling = rolling_dir();
            footage.retain(|segment| !segment.path.starts_with(&rolling));
            let db = Db::open_default().map_err(|error| format!("{error:#}"))?;
            let records = db.list_recordings().map_err(|error| format!("{error:#}"))?;
            for record in records.into_iter().filter(|record| record.probed && !record.is_live() && record.end_ms() > from_ms) {
                let path = PathBuf::from(record.path);
                if !footage.iter().any(|segment| crate::shared::same_path(&segment.path.to_string_lossy(), &path.to_string_lossy())) {
                    footage.push(Footage { path, start_ms: record.start_ms, duration_s: record.duration });
                }
            }
        }
        footage.retain(|segment| segment.end_ms() > from_ms && segment.start_ms < end_ms
            && media_path_allowed(&segment.path) && segment.path.is_file());
        if footage.is_empty() {
            return Err("There is no recent footage to save yet".into());
        }
        let lease = Arc::new(());
        if self.mode == RecordingMode::Rolling {
            self.clip_leases.push((Arc::downgrade(&lease), footage.iter().map(|segment| segment.path.clone()).collect()));
        }
        self.sh.clip_tx.send(ClipTask::SaveRange { footage, from_ms, to_ms: end_ms, short, _lease: lease })
            .map_err(|_| "Clip worker is unavailable".to_string())?;
        Ok(())
    }

    fn event(&mut self, ev: RecEvent) {
        match ev {
            RecEvent::SegmentStarted { path, start_ms } => {
                *self.sh.live_path.lock() = Some(path.clone());
                self.footage.push(Footage { path: path.clone(), start_ms, duration_s: 0.0 });
                if self.mode == RecordingMode::Archive {
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
                if self.mode == RecordingMode::Archive {
                    if let Ok(db) = Db::open_default() {
                        if let Ok(id) = db.upsert_recording(&path.to_string_lossy(), start_ms, bytes as i64, "complete") {
                            if let Ok(info) = cv_media::probe::probe(&path) {
                                let stamp = path.metadata().ok().map(|m| std::os::windows::fs::MetadataExt::last_write_time(&m) as i64);
                                let _ = db.set_recording_probe(id, start_ms, duration_s, info.audio.len() as i64, true, stamp);
                            }
                        }
                    }
                    self.sh.emit(Event::RecordingsChanged);
                    let _ = self.sh.lib_tx.send(LibCmd::RecordingsChanged);
                }
            }
            RecEvent::Slots { slots, .. } => {
                tracing::debug!("app tracks: {:?}", slots.iter().map(|s| (&s.app.name, s.slot)).collect::<Vec<_>>());
                if self.mode == RecordingMode::Archive {
                    self.sh.emit(Event::RecordingsChanged);
                }
            }
            RecEvent::Warning(m) => self.warn_once(&m),
        }
    }

    fn expire_replay(&mut self, cfg: &Config) {
        let cutoff = cfg.recording.rolling_seconds().map(|seconds| now_ms() - i64::from(seconds) * 1000);
        self.clean_rolling(cutoff);
    }

    fn tick(&mut self) {
        let cfg = self.sh.cfg();
        self.expire_replay(&cfg);
        if self.rec.is_none() {
            if self.retry_at.map(|t| Instant::now() >= t).unwrap_or(true) && !self.sh.is_shutting_down() {
                self.start();
            }
            self.publish();
            return;
        }
        let st = self.rec.as_ref().unwrap().status();
        let now = Instant::now();
        if st.written_until_ms != self.progress.0 {
            self.progress = (st.written_until_ms, now);
        } else if st.video_error.is_none()
            && now.saturating_duration_since(self.progress.1) > STALL_TIMEOUT
            && stall_restart_allowed(&mut self.starts, now)
        {
            self.starts.push_back(now);
            self.sh.log_event("warn", "The recording stopped making progress; restarting it");
            self.restart("stalled");
            return;
        }
        if let Some(last) = self.footage.last_mut() {
            if st.path.as_ref() == Some(&last.path) && st.written_until_ms > last.start_ms {
                last.duration_s = (st.written_until_ms - last.start_ms) as f64 / 1000.0;
            }
        }
        // Mic silence warning.
        if st.levels.mic > 0.003 {
            self.mic_last_sound = Instant::now();
            self.mic_warned = false;
        }
        let mins = cfg.audio.mic_silence_warning_mins;
        if cfg.audio.record_mic && self.mic_choice(&cfg).is_some() && mins > 0 && !self.mic_warned && self.mic_last_sound.elapsed() > Duration::from_secs(mins as u64 * 60) {
            self.mic_warned = true;
            self.sh.ui.send(UiCmd::Balloon { level: ToastLevel::Warning, title: "Your mic is silent".into(), text: format!("No sound from the microphone for {mins} minutes.") });
        }
        // Rolling clip buffer: keep only what the hotkey can use.
        if self.last_buffer_clean.elapsed() > Duration::from_secs(30) {
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
        let buffer = buffer_dir();
        if cfg.validate_media_folders().is_err() || !internal_buffer_root_ok(&buffer) {
            return;
        }
        self.footage.retain(|f| {
            let end = f.start_ms + (f.duration_s * 1000.0) as i64;
            if end >= cutoff || Some(&f.path) == live.as_ref() {
                return true;
            }
            if f.path.starts_with(&buffer) {
                let _ = std::fs::remove_file(&f.path);
                let _ = std::fs::remove_file(cv_core::waveform::wave_path(&f.path));
            }
            false
        });
        // The buffer is dedicated to rolling replay. Keep recent files across a mode
        // change and remove leftovers from earlier runs once they age out.
        if let Ok(rd) = std::fs::read_dir(&buffer) {
            for e in rd.flatten() {
                let p = e.path();
                if Some(&p) == live.as_ref() || self.footage.iter().any(|f| f.path == p)
                    || !native_segment_path(&p) {
                    continue;
                }
                let old = e.metadata().and_then(|m| m.modified()).ok()
                    .is_some_and(|t| cv_core::util::system_time_ms(t) < cutoff);
                if old {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }

    fn clean_rolling(&mut self, cutoff: Option<i64>) {
        let dir = rolling_dir();
        if !dir.is_dir() || !internal_buffer_root_ok(&dir) || self.sh.cfg().validate_media_folders().is_err() {
            return;
        }
        self.clip_leases.retain(|(lease, _)| lease.strong_count() > 0);
        let mut protected: HashSet<_> = self.clip_leases.iter().flat_map(|(_, paths)| paths.iter().cloned()).collect();
        protected.extend(self.sh.live_path.lock().clone());
        let mut ends = HashMap::new();
        self.footage.retain(|segment| {
            if !segment.path.starts_with(&dir) {
                return true;
            }
            ends.insert(segment.path.clone(), segment.end_ms());
            protected.contains(&segment.path) || !rolling_expired(segment.end_ms(), cutoff)
        });
        for path in prune_rolling_files(&dir, cutoff, &protected, &ends) {
            let _ = std::fs::remove_file(cv_core::waveform::wave_path(&path));
        }
    }

    fn publish(&self) {
        let cfg = self.sh.cfg();
        let rec = self.rec.as_ref().map(|r| r.status());
        let paused = self.mode != RecordingMode::Archive;
        let rolling = cfg.recording.rolling_seconds();
        let now = now_ms();
        let available_seconds = rolling.map_or(0, |seconds| {
            let from_ms = now - i64::from(seconds) * 1000;
            let dir = rolling_dir();
            let written = rec.as_ref().map_or(now, |rec| rec.written_until_ms);
            let available_ms: i64 = self.footage.iter()
                .filter(|segment| segment.path.starts_with(&dir))
                .map(|segment| (segment.end_ms().min(written) - segment.start_ms.max(from_ms)).max(0))
                .sum();
            ((available_ms / 1000) as u32).min(seconds)
        });
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
                    s.recording.written_until_ms = if paused { 0 } else { st.written_until_ms };
                    s.recording.bytes = st.bytes as i64;
                    s.recording.healthy = healthy;
                    s.recording.tracks = 2 + if cfg.recording.app_tracks { cfg.recording.app_track_count } else { 0 };
                    s.replay.active = healthy;
                    s.mic.muted = !st.mic_ok && cfg.audio.record_mic;
                }
                None => {
                    s.engine.running = false;
                    s.recording.active = false;
                    s.recording.healthy = false;
                    s.recording.path.clear();
                    s.recording.written_until_ms = 0;
                    s.recording.bytes = 0;
                    s.replay.active = false;
                }
            }
            s.recording.paused_by_user = paused;
            s.replay.seconds = clip_seconds(cfg.clips.clip_length_secs, rolling);
            s.replay.rolling_minutes = rolling.map(|seconds| seconds / 60);
            s.replay.available_seconds = available_seconds;
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

fn prune_rolling_files(dir: &Path, cutoff: Option<i64>, protected: &HashSet<PathBuf>, ends: &HashMap<PathBuf, i64>) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return removed };
    for entry in entries.flatten() {
        let path = entry.path();
        if protected.contains(&path) || !native_segment_path(&path) {
            continue;
        }
        let Some(start) = cv_core::filenames::parse_timestamp_ms(&path) else { continue };
        let end = ends.get(&path).copied().unwrap_or_else(|| {
            entry.metadata().and_then(|m| m.modified()).ok().map(cv_core::util::system_time_ms)
                .unwrap_or(start + i64::from(ROLLING_SEGMENT_SECS) * 1000)
        });
        if rolling_expired(end, cutoff) && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

fn native_segment_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("mp4")
        && cv_core::filenames::parse_timestamp_ms(path).is_some()
        && path.symlink_metadata().is_ok_and(|metadata| metadata.is_file())
}

/// Send a command and wait for its reply.
pub fn send_and_wait(sh: &Shared, make: impl FnOnce(crossbeam_channel::Sender<Result<(), String>>) -> EngineCmd, timeout: Duration) -> Result<(), String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    sh.engine_tx.send(make(tx)).map_err(|_| "service is shutting down".to_string())?;
    rx.recv_timeout(timeout).map_err(|_| "timed out".to_string())?
}

/// Exercise the same clip-source snapshot and expiry paths after a synthetic
/// flush. No capture worker is started by this test-only Engine owner.
#[cfg(test)]
pub(crate) fn run_rolling_fixture(sh: Arc<Shared>, rx: Receiver<EngineCmd>, footage: Vec<Footage>, written_until_ms: i64) {
    let mut engine = new_engine(sh, true);
    engine.footage = footage;
    while let Ok(command) = rx.recv() {
        match command {
            EngineCmd::ConfigChanged { .. } => {
                let cfg = engine.sh.cfg();
                engine.mode = recording_mode(&cfg, true);
                engine.expire_replay(&cfg);
                engine.publish();
            }
            EngineCmd::SaveRecentRolling { seconds, reply: response } => {
                let result = if engine.mode == RecordingMode::Rolling {
                    engine.queue_flushed_clip(seconds, true, written_until_ms)
                } else {
                    Err("Replay only is not active".into())
                };
                reply(response, result);
            }
            EngineCmd::Shutdown => break,
            _ => {}
        }
    }
    engine.clean_rolling(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_only_supersedes_archive_pause_and_caps_requested_seconds() {
        let mut cfg = Config::default();
        assert_eq!(cfg.recording.rolling_seconds(), None);
        assert!(recording_mode(&cfg, true) == RecordingMode::Archive);
        assert!(recording_mode(&cfg, false) == RecordingMode::Paused);
        cfg.recording.history_mode = cv_core::config::HistoryMode::Rolling;
        cfg.recording.rolling_minutes = 2;
        assert!(recording_mode(&cfg, false) == RecordingMode::Rolling);
        assert_eq!(RecordingMode::Rolling.segment_secs(&cfg), 10);
        assert_eq!(RecordingMode::Archive.segment_secs(&cfg), 3600);
        assert_eq!(clip_seconds(420, cfg.recording.rolling_seconds()), 120);
        assert_eq!(clip_seconds(30, cfg.recording.rolling_seconds()), 30);
    }

    #[test]
    fn rolling_cleanup_removes_only_expired_unleased_segments() {
        let dir = tempfile::tempdir().unwrap();
        let rolling = dir.path().join("rolling");
        std::fs::create_dir(&rolling).unwrap();
        let old = rolling.join("2026-11-28 09-00-00.mp4");
        let recent = rolling.join("2026-11-28 09-00-10.mp4");
        let leased = rolling.join("2026-11-28 09-00-20.mp4");
        let archive = dir.path().join("2026-11-28 09-00-30.mp4");
        let saved_clip = dir.path().join("saved clip.mp4");
        let unrelated = rolling.join("manual copy.mp4");
        for path in [&old, &recent, &leased, &archive, &saved_clip, &unrelated] {
            std::fs::write(path, b"synthetic").unwrap();
        }
        let ends = HashMap::from([(old.clone(), 10), (recent.clone(), 30), (leased.clone(), 15)]);
        let pins = HashSet::from([leased.clone()]);
        assert_eq!(prune_rolling_files(&rolling, Some(20), &pins, &ends), [old.clone()]);
        assert!(!old.exists());
        assert!(recent.exists() && leased.exists());
        // Leaving rolling mode or stopping discards temporary data outside active jobs.
        assert_eq!(prune_rolling_files(&rolling, None, &pins, &ends), [recent.clone()]);
        assert!(!recent.exists() && leased.exists());
        // Once the save job finishes, ordinary cleanup removes the source, not the clip.
        assert_eq!(prune_rolling_files(&rolling, None, &HashSet::new(), &ends), [leased.clone()]);
        assert!(!leased.exists() && archive.exists() && saved_clip.exists() && unrelated.exists());
    }

    #[test]
    fn exhausted_watchdog_budget_recovers_when_the_window_expires() {
        let now = Instant::now();
        let mut starts = VecDeque::from(vec![now; MAX_STALL_RESTARTS]);
        assert!(!stall_restart_allowed(&mut starts, now));
        assert!(stall_restart_allowed(&mut starts, now + RESTART_WINDOW));
        assert!(starts.is_empty());
    }
}
