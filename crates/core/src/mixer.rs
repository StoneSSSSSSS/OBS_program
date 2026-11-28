//! The audio mixing model shared by the player and exports.
//!
//! Sound is organised in *lanes*:
//! * `full` - the full mix track (everything you heard plus your mic);
//! * one lane per category: `cat:game`, `cat:voice`, `cat:media`,
//!   `cat:other` ("Desktop": everything else) and `mic`;
//! * one lane per app inside a category: `app:<exe>` (the expanded view).
//!
//! A file's audio tracks map onto lanes (see [`TrackSource`]). In "separate"
//! mode a track's gain is its category lane x its app lane, times every
//! non-destructive edit on either lane; in "full" mode only the full mix plays.
//! Edits can ramp in/out smoothly (optional) instead of switching instantly.

use crate::model::AudioEdit;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// (id, label) of every category, in display order.
pub const CATEGORIES: [(&str, &str); 5] = [("game", "Game"), ("voice", "Voice chat"), ("media", "Music & media"), ("other", "Desktop"), ("mic", "Microphone")];

pub const LANE_FULL: &str = "full";
pub const LANE_MIC: &str = "mic";

pub fn category_lane(cat: &str) -> String {
    if cat == "mic" {
        LANE_MIC.to_string()
    } else {
        format!("cat:{}", normalize_category(cat))
    }
}

pub fn app_lane(key: &str) -> String {
    format!("app:{}", key.to_lowercase())
}

pub fn category_label(cat: &str) -> &'static str {
    CATEGORIES.iter().find(|(id, _)| *id == cat).map(|(_, l)| *l).unwrap_or("Desktop")
}

/// Unknown categories count as "other".
pub fn normalize_category(cat: &str) -> &'static str {
    match cat {
        "game" => "game",
        "voice" => "voice",
        "media" | "music" => "media",
        "mic" => "mic",
        _ => "other",
    }
}

/// Lane ids of the legacy 6-track OBS layout, by track index.
pub const LEGACY_LANES: [&str; 6] = [LANE_FULL, "cat:game", "cat:voice", "cat:media", LANE_MIC, "cat:other"];

/// What one audio track of a file holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackSource {
    Full,
    Mic,
    App { key: String, name: String, category: String },
    /// A whole category in one track (legacy OBS recordings).
    Category(String),
    /// Unknown track (its own lane `track:<n>`).
    Other(usize),
}

impl TrackSource {
    /// The lanes whose settings affect this track.
    pub fn lanes(&self, category_override: Option<&str>) -> Vec<String> {
        match self {
            TrackSource::Full => vec![LANE_FULL.into()],
            TrackSource::Mic => vec![LANE_MIC.into()],
            TrackSource::App { key, category, .. } => {
                let cat = category_override.unwrap_or(category);
                vec![category_lane(cat), app_lane(key)]
            }
            TrackSource::Category(c) => vec![category_lane(c)],
            TrackSource::Other(i) => vec![format!("track:{i}")],
        }
    }

    pub fn category<'a>(&'a self, category_override: Option<&'a str>) -> &'a str {
        match self {
            TrackSource::Full => "full",
            TrackSource::Mic => "mic",
            TrackSource::App { category, .. } => normalize_category(category_override.unwrap_or(category)),
            TrackSource::Category(c) => normalize_category(c),
            TrackSource::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MixMode {
    /// Play only the full mix track.
    Full,
    /// Play categories/apps separately (adjustable).
    #[default]
    Separate,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LaneGain {
    pub gain: f32,
    #[serde(default)]
    pub muted: bool,
}

impl Default for LaneGain {
    fn default() -> Self {
        LaneGain { gain: 1.0, muted: false }
    }
}

impl LaneGain {
    pub fn effective(&self) -> f32 {
        if self.muted || !self.gain.is_finite() {
            0.0
        } else {
            self.gain.clamp(0.0, 4.0)
        }
    }
}

/// Volume settings for one clip (or the 24/7 view).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MixSettings {
    pub mode: MixMode,
    /// Lane id -> gain. Missing lanes are at 100%.
    #[serde(default)]
    pub lanes: BTreeMap<String, LaneGain>,
}

impl MixSettings {
    pub fn lane(&self, id: &str) -> LaneGain {
        self.lanes.get(id).copied().unwrap_or_default()
    }

    pub fn set(&mut self, id: &str, g: LaneGain) {
        if g == LaneGain::default() {
            self.lanes.remove(id);
        } else {
            self.lanes.insert(id.to_string(), g);
        }
    }

    /// From the old per-track gain list (legacy clips).
    pub fn from_legacy_gains(gains: &[f32]) -> MixSettings {
        let mut m = MixSettings::default();
        if gains.len() > 1 && gains[0] > 0.0 && gains[1..].iter().all(|g| *g == 0.0) {
            m.mode = MixMode::Full;
            m.set(LANE_FULL, LaneGain { gain: gains[0], muted: false });
            return m;
        }
        m.mode = MixMode::Separate;
        for (i, g) in gains.iter().enumerate() {
            let lane = if gains.len() == LEGACY_LANES.len() { LEGACY_LANES[i].to_string() } else { format!("track:{i}") };
            m.set(&lane, LaneGain { gain: *g, muted: *g <= 0.0 });
        }
        m
    }
}

/// Seconds an edit's volume change ramps in/out when smoothing is on.
pub const DEFAULT_FADE_S: f64 = 0.03;

/// A track's gain over time.
#[derive(Debug, Clone, PartialEq)]
pub struct GainCurve {
    pub base: f32,
    /// (start, end, gain) - multiplied together where they overlap.
    pub edits: Vec<(f64, f64, f32)>,
    /// Ramp length in seconds at each edit boundary (0 = instant).
    pub fade: f64,
}

fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

impl GainCurve {
    pub fn constant(g: f32) -> GainCurve {
        GainCurve { base: g, edits: Vec::new(), fade: 0.0 }
    }

    /// Nothing audible ever (skip decoding the track).
    pub fn is_silent(&self) -> bool {
        self.base <= 1e-5
    }

    fn edit_factor(&self, e: &(f64, f64, f32), t: f64) -> f32 {
        let (a, b, g) = *e;
        if t < a || t >= b {
            return 1.0;
        }
        if self.fade <= 0.0 {
            return g;
        }
        let f = self.fade.min((b - a) / 2.0).max(1e-6);
        let w = smoothstep(((t - a) / f).min((b - t) / f));
        (1.0 + (g as f64 - 1.0) * w) as f32
    }

    pub fn at(&self, t: f64) -> f32 {
        let mut g = self.base;
        for e in &self.edits {
            g *= self.edit_factor(e, t);
        }
        g
    }

    /// `out[i] = at(t0 + i * dt)`, fast when no edit boundary is nearby.
    pub fn fill(&self, t0: f64, dt: f64, out: &mut [f32]) {
        let t1 = t0 + dt * out.len() as f64;
        let relevant: Vec<&(f64, f64, f32)> = self.edits.iter().filter(|e| e.0 < t1 && e.1 > t0).collect();
        if relevant.is_empty() {
            out.fill(self.base);
            return;
        }
        let fully_inside_flat = relevant.iter().all(|e| {
            let f = if self.fade > 0.0 { self.fade.min((e.1 - e.0) / 2.0) } else { 0.0 };
            e.0 + f <= t0 && e.1 - f >= t1
        });
        if fully_inside_flat {
            let g = relevant.iter().fold(self.base, |g, e| g * e.2);
            out.fill(g);
            return;
        }
        for (i, o) in out.iter_mut().enumerate() {
            let t = t0 + dt * i as f64;
            let mut g = self.base;
            for e in &relevant {
                g *= self.edit_factor(e, t);
            }
            *o = g;
        }
    }
}

/// Edits on `lane` as (start, end, gain), in file time. `edit_offset` is
/// subtracted from edit times (e.g. wall-clock edits -> file seconds).
pub fn lane_edits(edits: &[AudioEdit], lane: &str, edit_offset: f64) -> Vec<(f64, f64, f32)> {
    edits.iter().filter(|e| e.lane_id() == lane).map(|e| (e.start - edit_offset, e.end - edit_offset, e.gain)).collect()
}

/// Gain curve for every track of a file.
///
/// `overrides`: the user's category for an app key (lower case).
pub fn compose(tracks: &[TrackSource], mix: &MixSettings, edits: &[AudioEdit], edit_offset: f64, fade: f64, overrides: &BTreeMap<String, String>) -> Vec<GainCurve> {
    let has_full = tracks.iter().any(|t| matches!(t, TrackSource::Full));
    let separable = tracks.iter().any(|t| !matches!(t, TrackSource::Full | TrackSource::Other(_)));
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let full_mode = mix.mode == MixMode::Full || !separable;
            let on = match t {
                TrackSource::Full => full_mode,
                TrackSource::Other(_) if !has_full && !separable => i == tracks.iter().position(|x| matches!(x, TrackSource::Other(_))).unwrap_or(0) || mix.lanes.contains_key(&format!("track:{i}")),
                _ => !full_mode,
            };
            if !on {
                return GainCurve::constant(0.0);
            }
            let ov = match t {
                TrackSource::App { key, .. } => overrides.get(&key.to_lowercase()).map(String::as_str),
                _ => None,
            };
            let lanes = t.lanes(ov);
            let base = lanes.iter().fold(1.0f32, |g, l| g * mix.lane(l).effective());
            let mut e = Vec::new();
            for l in &lanes {
                e.extend(lane_edits(edits, l, edit_offset));
            }
            e.sort_by(|a, b| a.0.total_cmp(&b.0));
            GainCurve { base, edits: e, fade }
        })
        .collect()
}

/// Soft limiter used on every mix so boosted tracks don't clip harshly.
pub fn limit(v: f32) -> f32 {
    let a = v.abs();
    if a <= 0.8 {
        v
    } else {
        v.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(key: &str, cat: &str) -> TrackSource {
        TrackSource::App { key: key.into(), name: key.into(), category: cat.into() }
    }

    fn edit(lane: &str, a: f64, b: f64, g: f32) -> AudioEdit {
        AudioEdit { track: 0, start: a, end: b, gain: g, lane: lane.into() }
    }

    #[test]
    fn separate_mode_multiplies_category_and_app() {
        let tracks = vec![TrackSource::Full, TrackSource::Mic, app("discord.exe", "voice"), app("cs2.exe", "game")];
        let mut mix = MixSettings::default();
        mix.set("cat:voice", LaneGain { gain: 0.5, muted: false });
        mix.set("app:discord.exe", LaneGain { gain: 0.5, muted: false });
        mix.set("mic", LaneGain { gain: 1.0, muted: true });
        let c = compose(&tracks, &mix, &[], 0.0, 0.0, &BTreeMap::new());
        assert_eq!(c[0].base, 0.0, "full mix off in separate mode");
        assert_eq!(c[1].base, 0.0, "muted mic");
        assert_eq!(c[2].base, 0.25);
        assert_eq!(c[3].base, 1.0);
        // Full mode plays only the full mix.
        mix.mode = MixMode::Full;
        let c = compose(&tracks, &mix, &[], 0.0, 0.0, &BTreeMap::new());
        assert_eq!(c.iter().map(|g| g.base).collect::<Vec<_>>(), vec![1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn overrides_move_apps_between_categories() {
        let tracks = vec![app("chrome.exe", "other")];
        let mut mix = MixSettings::default();
        mix.set("cat:media", LaneGain { gain: 0.0, muted: true });
        let mut ov = BTreeMap::new();
        assert_eq!(compose(&tracks, &mix, &[], 0.0, 0.0, &ov)[0].base, 1.0);
        ov.insert("chrome.exe".to_string(), "media".to_string());
        assert_eq!(compose(&tracks, &mix, &[], 0.0, 0.0, &ov)[0].base, 0.0);
    }

    #[test]
    fn edits_apply_to_category_and_app_lanes() {
        let tracks = vec![app("discord.exe", "voice")];
        let edits = vec![edit("cat:voice", 10.0, 20.0, 0.0), edit("app:discord.exe", 15.0, 30.0, 2.0), edit("cat:game", 0.0, 100.0, 0.0)];
        let c = compose(&tracks, &MixSettings::default(), &edits, 0.0, 0.0, &BTreeMap::new());
        assert_eq!(c[0].edits.len(), 2);
        assert_eq!(c[0].at(5.0), 1.0);
        assert_eq!(c[0].at(12.0), 0.0);
        assert_eq!(c[0].at(17.0), 0.0);
        assert_eq!(c[0].at(25.0), 2.0);
        // Wall-clock edits shifted onto a file starting at t=100.
        let c = compose(&tracks, &MixSettings::default(), &[edit("cat:voice", 110.0, 120.0, 0.0)], 100.0, 0.0, &BTreeMap::new());
        assert_eq!(c[0].at(15.0), 0.0);
    }

    #[test]
    fn smooth_fades() {
        let g = GainCurve { base: 1.0, edits: vec![(1.0, 2.0, 0.0)], fade: 0.1 };
        assert_eq!(g.at(0.99), 1.0);
        assert!((g.at(1.05) - 0.5).abs() < 0.01, "halfway through the ramp");
        assert_eq!(g.at(1.5), 0.0);
        assert!(g.at(1.97) > 0.1 && g.at(1.97) < 0.9);
        assert_eq!(g.at(2.0), 1.0);
        // Short edits ramp over half their length each way.
        let s = GainCurve { base: 1.0, edits: vec![(1.0, 1.04, 0.0)], fade: 0.1 };
        assert!((s.at(1.02) - 0.0).abs() < 1e-3);
        // Hard edits switch instantly.
        let h = GainCurve { base: 0.8, edits: vec![(1.0, 2.0, 2.0)], fade: 0.0 };
        assert_eq!(h.at(1.0), 1.6);
        assert_eq!(h.at(0.999), 0.8);
    }

    #[test]
    fn fill_matches_at() {
        let g = GainCurve { base: 0.9, edits: vec![(0.010, 0.020, 0.0), (0.015, 0.050, 2.0)], fade: 0.003 };
        let mut out = vec![0.0; 3000];
        g.fill(0.0, 1.0 / 48_000.0, &mut out);
        for (i, v) in out.iter().enumerate() {
            assert!((v - g.at(i as f64 / 48_000.0)).abs() < 1e-6, "{i}");
        }
        // Constant fast paths.
        let mut out = vec![0.0; 10];
        g.fill(1.0, 0.001, &mut out);
        assert!(out.iter().all(|v| *v == 0.9));
        g.fill(0.03, 0.0001, &mut out);
        assert!(out.iter().all(|v| (*v - 1.8).abs() < 1e-6));
    }

    #[test]
    fn legacy_layout_and_gains() {
        let tracks: Vec<TrackSource> = vec![TrackSource::Full, TrackSource::Category("game".into()), TrackSource::Category("voice".into()), TrackSource::Category("media".into()), TrackSource::Mic, TrackSource::Category("other".into())];
        let m = MixSettings::from_legacy_gains(&[0.0, 1.0, 0.5, 1.0, 1.0, 0.0]);
        let c = compose(&tracks, &m, &[], 0.0, 0.0, &BTreeMap::new());
        assert_eq!(c.iter().map(|g| g.base).collect::<Vec<_>>(), vec![0.0, 1.0, 0.5, 1.0, 1.0, 0.0]);
        let full = MixSettings::from_legacy_gains(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(full.mode, MixMode::Full);
        // Legacy edits by track index map to lanes.
        let e = AudioEdit { track: 2, start: 0.0, end: 1.0, gain: 0.0, lane: String::new() };
        assert_eq!(e.lane_id(), "cat:voice");
    }

    #[test]
    fn single_unknown_track_plays() {
        let tracks = vec![TrackSource::Other(0)];
        let c = compose(&tracks, &MixSettings::default(), &[], 0.0, 0.0, &BTreeMap::new());
        assert_eq!(c[0].base, 1.0);
    }

    #[test]
    fn limiter() {
        assert_eq!(limit(0.5), 0.5);
        assert!(limit(3.0) <= 1.0 && limit(3.0) > 0.95);
        assert!(limit(-3.0) >= -1.0);
        assert!(limit(0.9) < 0.9 && limit(0.9) > 0.85);
    }
}
