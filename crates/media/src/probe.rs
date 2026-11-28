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


/// How each audio track maps onto the mixing lanes. 24/7 app tracks
/// ("slots") hold different apps over time; see [`slot_intervals`].
pub fn track_sources(info: &MediaInfo) -> Vec<cv_core::mixer::TrackSource> {
    use cv_core::mixer::TrackSource;
    let n = info.audio.len();
    let legacy = n == LEGACY_NAMES.len() && info.audio.iter().all(|a| a.role == TrackRole::Other);
    info.audio
        .iter()
        .enumerate()
        .map(|(i, a)| match &a.role {
            TrackRole::Full => TrackSource::Full,
            TrackRole::Mic => TrackSource::Mic,
            TrackRole::App { app } => TrackSource::App { key: app.key.clone(), name: app.name.clone(), category: app.category.clone() },
            TrackRole::Slot { .. } => TrackSource::Other(i),
            TrackRole::Other if legacy => match i {
                0 => TrackSource::Full,
                4 => TrackSource::Mic,
                1 => TrackSource::Category("game".into()),
                2 => TrackSource::Category("voice".into()),
                3 => TrackSource::Category("media".into()),
                _ => TrackSource::Category("other".into()),
            },
            TrackRole::Other => TrackSource::Other(i),
        })
        .collect()
}

/// When each app occupied recording slot `slot`: (start s, end s, app), in
/// file seconds. The last interval runs to `end_s`.
pub fn slot_intervals(info: &MediaInfo, slot: u32, end_s: f64) -> Vec<(f64, f64, AppIdent)> {
    let mut out: Vec<(f64, f64, AppIdent)> = Vec::new();
    let mut cur: Option<(f64, AppIdent)> = None;
    for m in &info.slot_history {
        let now = m.slots.iter().find(|s| s.slot == slot).map(|s| s.app.clone());
        let same = match (&cur, &now) {
            (Some((_, a)), Some(b)) => a.key == b.key,
            (None, None) => true,
            _ => false,
        };
        if same {
            continue;
        }
        if let Some((start, app)) = cur.take() {
            if m.at_s > start {
                out.push((start, m.at_s, app));
            }
        }
        cur = now.map(|a| (m.at_s, a));
    }
    if let Some((start, app)) = cur {
        if end_s > start {
            out.push((start, end_s, app));
        }
    }
    out
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
        let info = MediaInfo { slot_history: h, ..Default::default() };
        let iv = slot_intervals(&info, 0, 80.0);
        assert_eq!(iv.iter().map(|(a, b, x)| (*a, *b, x.key.as_str())).collect::<Vec<_>>(), vec![(10.0, 50.0, "discord.exe"), (50.0, 80.0, "chrome.exe")]);
        assert_eq!(slot_intervals(&info, 1, 80.0).len(), 1);
    }
}
