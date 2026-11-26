//! State shared between the service's threads.

use crate::win::ui::{ToastLevel, UiHandle};
use crossbeam_channel::Sender;
use cv_core::audio_names::MicDecision;
use cv_core::config::Config;
use cv_core::db::Db;
use cv_core::ffmpeg::Tools;
use cv_core::ipc::{AudioDevices, Event, JobInfo, ServerMsg, Status};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What game audio should follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameTarget {
    pub exe: String,
    pub title: String,
    pub class: String,
    pub name: String,
}

/// Commands for the OBS supervisor thread.
pub enum SupCmd {
    SaveClip { short: bool, reply: Option<Sender<Result<(), String>>> },
    SetRecording { enabled: bool, reply: Option<Sender<Result<(), String>>> },
    RestartObs { reply: Option<Sender<Result<(), String>>> },
    ConfigChanged { restart_obs: bool },
    Mic { decision: MicDecision },
    DesktopDevice { device_id: String },
    /// exe (lower case) -> include that app's capture in the full mix.
    AppRouting(HashMap<String, bool>),
    GameTarget(GameTarget),
    Shutdown,
}

/// Work for the clip-saving thread.
pub enum ClipTask {
    ReplaySaved { path: PathBuf, at_ms: i64, short: bool },
    ImportStray { path: PathBuf },
}

/// Work for the library thread.
pub enum LibCmd {
    Rescan,
    RecordingsChanged,
}

pub struct Shared {
    pub cfg: Arc<RwLock<Config>>,
    pub status: Mutex<Status>,
    pub ui: UiHandle,
    pub sup_tx: Sender<SupCmd>,
    pub clip_tx: Sender<ClipTask>,
    pub job_tx: Sender<crate::jobs::JobRequest>,
    pub lib_tx: Sender<LibCmd>,
    pub audio_tx: Sender<()>,
    pub clients: Mutex<Vec<Sender<String>>>,
    pub tools: RwLock<Option<Tools>>,
    pub hotkey_errors: Arc<Mutex<Vec<String>>>,
    pub devices: Mutex<AudioDevices>,
    pub jobs: Mutex<Vec<JobInfo>>,
    pub event_db: Mutex<Option<Db>>,
    /// Held while moving/renaming clip files so the scanner never sees a
    /// half-finished operation.
    pub library_lock: Mutex<()>,
    pub shutdown: AtomicBool,
}

impl Shared {
    pub fn cfg(&self) -> Config {
        self.cfg.read().clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Send an event to every connected GUI.
    pub fn emit(&self, event: Event) {
        let Ok(line) = serde_json::to_string(&ServerMsg::Event { event }) else { return };
        let mut clients = self.clients.lock();
        clients.retain(|c| c.send(line.clone()).is_ok());
    }

    pub fn has_clients(&self) -> bool {
        !self.clients.lock().is_empty()
    }

    /// Show a toast on screen and in the GUI.
    pub fn toast(&self, level: ToastLevel, message: impl Into<String>) {
        let message = message.into();
        self.ui.toast(level, message.clone());
        self.emit(Event::Toast {
            level: match level {
                ToastLevel::Info => "info",
                ToastLevel::Success => "success",
                ToastLevel::Warning => "warning",
                ToastLevel::Error => "error",
            }
            .into(),
            message,
        });
    }

    /// Record something in the activity log shown on the GUI's home page.
    pub fn log_event(&self, level: &str, message: impl AsRef<str>) {
        let message = message.as_ref();
        match level {
            "error" => tracing::error!("{message}"),
            "warn" => tracing::warn!("{message}"),
            _ => tracing::info!("{message}"),
        }
        let mut db = self.event_db.lock();
        if db.is_none() {
            *db = Db::open_default().ok();
        }
        if let Some(db) = db.as_ref() {
            let _ = db.log_event(level, message);
            let _ = db.prune_events(500);
        }
    }

    pub fn tools(&self) -> Option<Tools> {
        if let Some(t) = self.tools.read().clone() {
            return Some(t);
        }
        let cfg = self.cfg();
        match cv_core::ffmpeg::locate(&cfg.advanced.ffmpeg_path, &cfg.advanced.ffprobe_path) {
            Ok(t) => {
                tracing::info!("using {} ({})", t.ffmpeg.display(), t.version);
                *self.tools.write() = Some(t.clone());
                Some(t)
            }
            Err(e) => {
                tracing::warn!("{e:#}");
                None
            }
        }
    }

    pub fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock());
    }
}

/// Normalise a path reported by OBS (forward slashes) to Windows form.
pub fn norm_path(p: &str) -> String {
    p.replace('/', "\\")
}

pub fn same_path(a: &str, b: &str) -> bool {
    norm_path(a).eq_ignore_ascii_case(&norm_path(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_normalisation() {
        assert_eq!(norm_path("I:/OBS/Recordings/a.mp4"), r"I:\OBS\Recordings\a.mp4");
        assert!(same_path("I:/OBS/a.mp4", r"i:\obs\A.MP4"));
        assert!(!same_path("I:/OBS/a.mp4", r"I:\OBS\b.mp4"));
    }
}
