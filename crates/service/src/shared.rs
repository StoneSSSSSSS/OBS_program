//! State shared between the service's threads.

use crate::win::ui::{ToastLevel, UiHandle};
use crossbeam_channel::Sender;
use cv_core::audio_names::MicDecision;
use cv_core::config::Config;
use cv_core::db::Db;
use cv_core::ipc::{AudioDevices, Event, JobInfo, ServerMsg, Status};
use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type Reply = Option<Sender<Result<(), String>>>;

/// Commands for the recording engine thread.
pub enum EngineCmd {
    SaveClip { short: bool, reply: Reply },
    SaveRecentRolling { seconds: u32, reply: Reply },
    SetRecording { enabled: bool, reply: Reply },
    ToggleRecording,
    Restart { reply: Reply },
    ConfigChanged { restart: bool },
    Mic { decision: MicDecision },
    Shutdown,
}

/// A stretch of footage, for cutting clips.
#[derive(Debug, Clone)]
pub struct Footage {
    pub path: PathBuf,
    pub start_ms: i64,
    /// Seconds (for the file being written: recorded so far).
    pub duration_s: f64,
}

impl Footage {
    pub fn end_ms(&self) -> i64 {
        self.start_ms + (self.duration_s.max(0.0) * 1000.0) as i64
    }
}

/// Work for the clip-saving thread.
pub enum ClipTask {
    /// Save `[from_ms, to_ms)` (wall clock) from these files as a new clip.
    SaveRange { footage: Vec<Footage>, from_ms: i64, to_ms: i64, short: bool, _lease: Arc<()> },
}

/// Work for the library thread.
pub enum LibCmd {
    Rescan,
    RecordingsChanged,
}

pub struct Shared {
    pub cfg: Arc<RwLock<Config>>,
    #[cfg(test)]
    pub isolated_test: bool,
    pub status: Mutex<Status>,
    pub ui: UiHandle,
    pub engine_tx: Sender<EngineCmd>,
    pub clip_tx: Sender<ClipTask>,
    pub job_tx: Sender<crate::jobs::JobRequest>,
    pub lib_tx: Sender<LibCmd>,
    pub audio_tx: Sender<()>,
    pub clients: Mutex<Vec<Sender<String>>>,
    pub hotkey_errors: Arc<Mutex<Vec<String>>>,
    pub devices: Mutex<AudioDevices>,
    pub jobs: Mutex<Vec<JobInfo>>,
    pub event_db: Mutex<Option<Db>>,
    pub foreground_game: AtomicBool,
    /// Held while moving/renaming clip files so the scanner never sees a
    /// half-finished operation.
    pub library_lock: Mutex<()>,
    /// The file the recorder is writing right now (never deleted or moved).
    pub live_path: Mutex<Option<PathBuf>>,
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

    pub fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock());
    }

    /// Is `path` the file being recorded right now?
    pub fn is_live(&self, path: &str) -> bool {
        self.live_path.lock().as_ref().map(|p| same_path(&p.to_string_lossy(), path)).unwrap_or(false)
    }
}

/// Normalise a path (forward slashes) to Windows form.
pub fn norm_path(p: &str) -> String {
    p.replace('/', "\\")
}

pub fn same_path(a: &str, b: &str) -> bool {
    norm_path(a).eq_ignore_ascii_case(&norm_path(b))
}

pub fn media_path_allowed(path: &Path) -> bool {
    !cv_core::paths::uses_local_storage() || cv_core::paths::owns_local_path(path)
}

pub fn require_media_path(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(media_path_allowed(path), "Media path is outside this project profile: {}", path.display());
    Ok(())
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
