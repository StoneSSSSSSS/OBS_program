//! Data model shared by the database, IPC and GUI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Game {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub exe_path: String,
    /// User override: Some(true)/Some(false), None = automatic.
    pub is_game_override: Option<bool>,
    pub auto_is_game: bool,
    pub name_locked: bool,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Filled by list queries.
    pub clip_count: i64,
}

impl Game {
    pub fn is_game(&self) -> bool {
        self.is_game_override.unwrap_or(self.auto_is_game)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub sort: i64,
    pub clip_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub game_id: Option<i64>,
    pub game_name: Option<String>,
    /// When the clip was saved (Unix ms).
    pub created_at: i64,
    /// Wall-clock time the clip's content starts, if known.
    pub source_start_ms: Option<i64>,
    pub duration: f64,
    pub size: i64,
    pub width: i64,
    pub height: i64,
    pub fps: f64,
    pub audio_tracks: i64,
    pub track_names: Vec<String>,
    pub favorite: bool,
    pub notes: String,
    pub source: String,
    /// JSON array of per-track gains the user last used.
    pub mixer: Option<Vec<f32>>,
    pub thumb: String,
    pub probed: bool,
    pub missing: bool,
    pub last_position: f64,
    pub views: i64,
    pub category_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NewClip {
    pub path: String,
    pub title: String,
    pub game_id: Option<i64>,
    pub created_at: i64,
    pub source_start_ms: Option<i64>,
    pub size: i64,
    pub source: String,
    pub favorite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ClipSort {
    #[default]
    Newest,
    Oldest,
    Longest,
    Largest,
    Title,
    Game,
}

impl ClipSort {
    pub const ALL: [ClipSort; 6] =
        [ClipSort::Newest, ClipSort::Oldest, ClipSort::Longest, ClipSort::Largest, ClipSort::Title, ClipSort::Game];
    pub fn label(self) -> &'static str {
        match self {
            ClipSort::Newest => "Newest first",
            ClipSort::Oldest => "Oldest first",
            ClipSort::Longest => "Longest",
            ClipSort::Largest => "Largest",
            ClipSort::Title => "Title A-Z",
            ClipSort::Game => "Game A-Z",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClipFilter {
    pub game_id: Option<i64>,
    pub category_id: Option<i64>,
    /// Clips with no category.
    pub uncategorized: bool,
    pub favorites_only: bool,
    pub search: String,
    pub sort: ClipSort,
    pub include_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub id: i64,
    pub path: String,
    pub start_ms: i64,
    /// Seconds (0 = unknown yet).
    pub duration: f64,
    pub size: i64,
    /// "recording" while OBS is writing it, otherwise "complete".
    pub status: String,
    pub audio_tracks: i64,
    pub kept: bool,
    pub probed: bool,
}

impl Recording {
    pub fn is_live(&self) -> bool {
        self.status == "recording"
    }
    pub fn end_ms(&self) -> i64 {
        self.start_ms + (self.duration * 1000.0) as i64
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivitySpan {
    pub id: i64,
    pub key: String,
    pub game_id: Option<i64>,
    pub name: String,
    pub is_game: bool,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: i64,
    pub at_ms: i64,
    pub label: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEntry {
    pub id: i64,
    pub at_ms: i64,
    pub level: String,
    pub message: String,
}
