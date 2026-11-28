//! What's in a video file: length, picture size, and what each audio track
//! is (full mix, microphone, or a particular app's sound).

use crate::mp4::reader::{Mp4Reader, TrackKind};
use crate::mp4::Codec;
use crate::recorder::{parse_meta, AppIdent, SegmentMeta};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What an audio track holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackRole {
    /// Everything you heard plus your mic.
    Full,
    Mic,
    /// One app's sound (clips; or a recording slot at a given time).
    App { app: AppIdent },
    /// A 24/7 recording's reusable app track; see the slot history.
    Slot { slot: u32 },
    /// A track from another program (OBS layout or unknown).
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrackInfo {
    /// Position among the file's decodable audio tracks.
    pub index: usize,
    pub track_id: u32,
    pub name: String,
    pub role: TrackRole,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub duration: f64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio: Vec<AudioTrackInfo>,
    /// Wall-clock start (ClipVault recordings and clips).
    pub start_ms: Option<i64>,
    pub fragmented: bool,
    /// For recordings: which app was on which slot, over time.
    pub slot_history: Vec<SegmentMeta>,
    /// Seconds hidden at the start by an edit list (keyframe pre-roll).
    pub edit_start: f64,
}

/// The legacy 6-track layout ClipVault configured in OBS.
pub const LEGACY_NAMES: [&str; 6] = ["Full mix", "Game", "Voice chat", "Music", "Microphone", "Desktop"];

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let r = Mp4Reader::open(path)?;
    Ok(probe_reader(&r))
}

pub fn probe_reader(r: &Mp4Reader) -> MediaInfo {
    let mut info = MediaInfo { duration: r.duration_secs(), size: std::fs::metadata(r.path()).map(|m| m.len()).unwrap_or(0), fragmented: r.is_fragmented(), ..Default::default() };
    let metas = parse_meta(&r.metas);
    info.start_ms = metas.iter().find_map(|m| m.start_ms);
    let track_meta = metas.iter().find(|m| !m.tracks.is_empty()).map(|m| m.tracks.clone()).unwrap_or_default();
    if let Some(vi) = r.video() {
        let t = &r.tracks[vi];
        info.width = t.width;
        info.height = t.height;
        info.video_codec = match &t.codec {
            Codec::H264 { avcc } => {
                if let Some((_, sps, _)) = crate::h264::parse_avcc(avcc) {
                    if let Some((w, h)) = sps.first().and_then(|s| crate::h264::sps_dimensions(s)) {
                        info.width = w;
                        info.height = h;
                    }
                }
                "h264".into()
            }
            _ => String::new(),
        };
        info.edit_start = t.edit_start_s();
        // Frame rate from the first second of samples.
        let ts = t.timescale as u64;
        if let Ok(s) = r.samples(vi, 0, ts * 2) {
            let durs: Vec<u32> = s.iter().take(120).map(|x| x.duration).filter(|d| *d > 0).collect();
            if !durs.is_empty() {
                let mut sorted = durs.clone();
                sorted.sort_unstable();
                info.fps = ts as f64 / sorted[sorted.len() / 2] as f64;
            }
        }
    } else if let Some(t) = r.tracks.iter().find(|t| t.kind == TrackKind::Video) {
        info.width = t.width;
        info.height = t.height;
        info.video_codec = match &t.codec {
            Codec::Other { fourcc } => String::from_utf8_lossy(fourcc).to_string(),
            _ => String::new(),
        };
    }
    let audio = r.audio();
    let n = audio.len();
    for (i, &ti) in audio.iter().enumerate() {
        let t = &r.tracks[ti];
        let tm = track_meta.iter().find(|m| m.id == t.id);
        let role = match tm.map(|m| m.role.as_str()) {
            Some("full") => TrackRole::Full,
            Some("mic") => TrackRole::Mic,
            Some("slot") => TrackRole::Slot { slot: tm.and_then(|m| m.slot).unwrap_or(0) },
            Some("app") => match tm.and_then(|m| m.app.clone()) {
                Some(app) => TrackRole::App { app },
                None => TrackRole::Other,
            },
            _ => TrackRole::Other,
        };
        let name = match &role {
            TrackRole::Full => "Full mix".to_string(),
            TrackRole::Mic => "Microphone".to_string(),
            TrackRole::App { app } => app.name.clone(),
            TrackRole::Slot { slot } => format!("App {}", slot + 1),
            TrackRole::Other => {
                let h = t.name.trim();
                if !h.is_empty() && !h.contains("Handler") && !h.eq_ignore_ascii_case("SoundHandler") {
                    h.to_string()
                } else if n == LEGACY_NAMES.len() {
                    LEGACY_NAMES[i].to_string()
                } else if n == 1 {
                    "Audio".to_string()
                } else {
                    format!("Track {}", i + 1)
                }
            }
        };
        info.audio.push(AudioTrackInfo { index: i, track_id: t.id, name, role, enabled: t.enabled });
    }
    info.slot_history = metas;
    info
}

/// Which app was on recording slot `slot` at `at_s` seconds into the file.
pub fn slot_app(history: &[SegmentMeta], slot: u32, at_s: f64) -> Option<AppIdent> {
    let mut cur: Option<AppIdent> = None;
    for m in history {
        if m.at_s > at_s + 1e-6 {
            break;
        }
        cur = m.slots.iter().find(|s| s.slot == slot).map(|s| s.app.clone());
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::SlotInfo;

    fn app(k: &str) -> AppIdent {
        AppIdent { key: k.into(), name: k.into(), category: "other".into() }
    }

    #[test]
    fn slot_history_lookup() {
        let h = vec![
            SegmentMeta { at_s: 0.0, slots: vec![], ..Default::default() },
            SegmentMeta { at_s: 10.0, slots: vec![SlotInfo { slot: 0, app: app("discord.exe") }], ..Default::default() },
            SegmentMeta { at_s: 50.0, slots: vec![SlotInfo { slot: 0, app: app("chrome.exe") }, SlotInfo { slot: 1, app: app("cs2.exe") }], ..Default::default() },
        ];
        assert_eq!(slot_app(&h, 0, 5.0), None);
        assert_eq!(slot_app(&h, 0, 10.0).unwrap().key, "discord.exe");
        assert_eq!(slot_app(&h, 0, 49.9).unwrap().key, "discord.exe");
        assert_eq!(slot_app(&h, 0, 60.0).unwrap().key, "chrome.exe");
        assert_eq!(slot_app(&h, 1, 60.0).unwrap().key, "cs2.exe");
        assert_eq!(slot_app(&h, 1, 20.0), None);
    }
}
