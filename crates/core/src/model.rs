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

/// A non-destructive volume change on one audio lane over a time range.
/// `gain` 0 = muted, 1 = unchanged, 2 = twice as loud. Times are seconds on
/// the media's own timeline (clip time), except on the 24/7 timeline where
/// they are wall-clock seconds (Unix time).
///
/// `lane` names what the edit applies to (`cat:game`, `app:discord.exe`,
/// `mic`, `full`...; see [`crate::mixer`]). Older edits only have `track`,
/// an index into the legacy 6-track layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioEdit {
    #[serde(default)]
    pub track: usize,
    pub start: f64,
    pub end: f64,
    pub gain: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lane: String,
}

impl AudioEdit {
    pub fn new(lane: &str, start: f64, end: f64, gain: f32) -> AudioEdit {
        AudioEdit { track: 0, start, end, gain, lane: lane.to_string() }
    }

    pub fn overlaps(&self, a: f64, b: f64) -> bool {
        self.start < b && self.end > a
    }

    /// The lane this edit applies to.
    pub fn lane_id(&self) -> String {
        if !self.lane.is_empty() {
            self.lane.clone()
        } else {
            crate::mixer::LEGACY_LANES.get(self.track).map(|s| s.to_string()).unwrap_or_else(|| format!("track:{}", self.track))
        }
    }
}

/// Apply a gain to `[a, b)` on `lane`, replacing whatever edits covered that
/// range on that lane. `gain == 1.0` simply clears the range.
pub fn apply_edit(edits: &mut Vec<AudioEdit>, lane: &str, a: f64, b: f64, gain: f32) {
    let (a, b) = (a.min(b), a.max(b));
    if b - a < 0.01 {
        return;
    }
    let mut out = Vec::with_capacity(edits.len() + 2);
    for e in edits.drain(..) {
        if e.lane_id() != lane || !e.overlaps(a, b) {
            out.push(e);
            continue;
        }
        // Keep the parts of the old edit outside [a, b).
        if e.start < a {
            out.push(AudioEdit { end: a, ..e.clone() });
        }
        if e.end > b {
            out.push(AudioEdit { start: b, ..e.clone() });
        }
    }
    if (gain - 1.0).abs() > 0.001 {
        out.push(AudioEdit::new(lane, a, b, gain));
    }
    // Normalise legacy edits to lane ids so they compare cleanly.
    for e in out.iter_mut() {
        if e.lane.is_empty() {
            e.lane = e.lane_id();
            e.track = 0;
        }
    }
    out.sort_by(|x, y| x.lane.cmp(&y.lane).then(x.start.total_cmp(&y.start)));
    *edits = out;
}

/// Edits re-based onto a sub-range `[from, to)` of the timeline (e.g. when a
/// clip is cut from the 24/7 recording or trimmed): clipped to the range and
/// shifted so `from` becomes 0.
pub fn rebase_edits(edits: &[AudioEdit], from: f64, to: f64) -> Vec<AudioEdit> {
    edits
        .iter()
        .filter(|e| e.overlaps(from, to))
        .map(|e| AudioEdit { track: 0, start: e.start.max(from) - from, end: e.end.min(to) - from, gain: e.gain, lane: e.lane_id() })
        .collect()
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
    /// Non-destructive per-track volume edits (clip time).
    #[serde(default)]
    pub audio_edits: Vec<AudioEdit>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn e(lane: &str, start: f64, end: f64, gain: f32) -> AudioEdit {
        AudioEdit::new(lane, start, end, gain)
    }

    #[test]
    fn applying_edits_replaces_overlaps() {
        let mut v = vec![e("cat:game", 0.0, 10.0, 0.0)];
        apply_edit(&mut v, "cat:game", 4.0, 6.0, 2.0);
        assert_eq!(v, vec![e("cat:game", 0.0, 4.0, 0.0), e("cat:game", 4.0, 6.0, 2.0), e("cat:game", 6.0, 10.0, 0.0)]);
        // Other lanes untouched; gain 1 clears.
        apply_edit(&mut v, "app:discord.exe", 0.0, 5.0, 0.5);
        apply_edit(&mut v, "cat:game", 3.0, 7.0, 1.0);
        assert_eq!(v, vec![e("app:discord.exe", 0.0, 5.0, 0.5), e("cat:game", 0.0, 3.0, 0.0), e("cat:game", 7.0, 10.0, 0.0)]);
        // Reversed ranges are normalised; tiny ranges ignored.
        apply_edit(&mut v, "mic", 9.0, 8.0, 0.0);
        assert!(v.contains(&e("mic", 8.0, 9.0, 0.0)));
        let before = v.clone();
        apply_edit(&mut v, "mic", 1.0, 1.001, 0.0);
        assert_eq!(v, before);
    }

    #[test]
    fn legacy_edits_convert_to_lanes() {
        let legacy: AudioEdit = serde_json::from_str(r#"{"track":1,"start":0.0,"end":5.0,"gain":0.0}"#).unwrap();
        assert_eq!(legacy.lane_id(), "cat:game");
        let mut v = vec![legacy];
        apply_edit(&mut v, "cat:game", 2.0, 3.0, 1.0);
        assert_eq!(v, vec![e("cat:game", 0.0, 2.0, 0.0), e("cat:game", 3.0, 5.0, 0.0)]);
    }

    #[test]
    fn rebasing() {
        let v = vec![e("full", 100.0, 110.0, 0.0), e("cat:game", 95.0, 102.0, 2.0), e("mic", 200.0, 210.0, 0.0)];
        let r = rebase_edits(&v, 98.0, 108.0);
        assert_eq!(r, vec![e("full", 2.0, 10.0, 0.0), e("cat:game", 0.0, 4.0, 2.0)]);
    }
}
