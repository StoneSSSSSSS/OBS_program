//! Lossless cutting: copy the video and audio samples of a time range (from
//! one or more files) into a new fast-start MP4, without re-encoding.
//!
//! * Video starts at the keyframe before the in-point; an edit list hides the
//!   extra frames, so playback starts exactly at the in-point.
//! * Audio is copied frame by frame. A 24/7 recording's reusable app tracks
//!   are split by app, so the clip gets one track per app that played in the
//!   range (with silence where that app was quiet).

use crate::mp4::flat::FlatWriter;
use crate::mp4::reader::{Mp4Reader, Sample};
use crate::mp4::{Codec, SampleRef, TrackSpec};
use crate::probe::{probe_reader, slot_app, AudioTrackInfo, MediaInfo, TrackRole};
use crate::recorder::{AppIdent, SegmentMeta, TrackMeta};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// A piece of a source file: `[from_s, to_s)` in the file's presentation time.
#[derive(Debug, Clone, PartialEq)]
pub struct CutSource {
    pub path: PathBuf,
    pub from_s: f64,
    /// None = to the end.
    pub to_s: Option<f64>,
}

/// A retained source interval and where it appears in the joined cut.
#[derive(Debug, Clone, PartialEq)]
pub struct CutSpan {
    pub source_index: usize,
    pub from_s: f64,
    pub to_s: f64,
    pub output_start_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CutOutput {
    pub duration: f64,
    pub size: u64,
    pub audio: Vec<AudioTrackInfo>,
    pub spans: Vec<CutSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Full,
    Mic,
    App(String),
    Other(usize),
}

enum Data {
    From { src: usize, sample: Sample },
    Silence,
}

struct OutSample {
    /// Clip seconds (for interleaving).
    t: f64,
    track: usize,
    data: Data,
    duration: u32,
    sync: bool,
}

/// Silent AAC frame for a given configuration.
fn silent_frame(asc: &[u8]) -> Result<Vec<u8>> {
    let (_, rate, ch) = crate::aac::parse_asc(asc).context("bad audio config")?;
    let mut e = crate::mf::aac::AacEncoder::new(rate, ch.max(1), 96)?;
    let mut frames = e.encode(&vec![0.0; crate::mf::aac::FRAME * ch.max(1) as usize * 4])?;
    frames.extend(e.finish()?);
    frames.into_iter().nth(1).context("no silent frame")
}

pub fn cut(sources: &[CutSource], dest: &Path, start_ms: Option<i64>, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<CutOutput> {
    if sources.is_empty() {
        bail!("nothing to cut");
    }
    let readers: Vec<Mp4Reader> = sources.iter().map(|s| Mp4Reader::open(&s.path)).collect::<Result<_>>()?;
    let infos: Vec<MediaInfo> = readers.iter().map(probe_reader).collect();
    let r0 = &readers[0];
    let vi0 = r0.video().context("the source has no playable video")?;
    let vt = r0.tracks[vi0].clone();
    if !matches!(vt.codec, Codec::H264 { .. }) {
        bail!("unsupported video codec");
    }
    for r in &readers[1..] {
        let vi = r.video().context("a source has no video")?;
        if r.tracks[vi].codec != vt.codec {
            bail!("that range spans recordings with different video settings; clip each part separately");
        }
    }
    let vts = vt.timescale as f64;

    let mut out: Vec<OutSample> = Vec::new();
    let mut keys: Vec<(Key, f64, AppIdent, Vec<u8>, String)> = Vec::new(); // key, first time, app, asc, name
    let mut audio_samples: HashMap<Key, Vec<(f64, usize, Sample)>> = HashMap::new();
    let mut spans = Vec::new();
    let mut offset = 0.0f64;
    let mut preroll = 0.0f64;
    let mut video_dts_total: u64 = 0;
    for (k, (src, r)) in sources.iter().zip(&readers).enumerate() {
        let info = &infos[k];
        let vi = r.video().unwrap();
        let edit = r.tracks[vi].edit_start_s();
        let file_end = r.end_secs(vi) - edit;
        let from = src.from_s.max(0.0);
        let to = src.to_s.unwrap_or(file_end).min(file_end);
        if to <= from + 0.001 {
            continue;
        }
        let (mf, mt) = (from + edit, to + edit);
        let key = r.sync_at_or_before(vi, (mf * vts) as u64)?.context("no keyframe in range")?;
        if k == 0 || out.is_empty() {
            preroll = (mf - key.dts as f64 / vts).max(0.0);
        }
        for s in r.samples(vi, key.dts, (mt * vts).ceil() as u64)? {
            let t = offset + (s.dts as f64 / vts - mf);
            video_dts_total += s.duration as u64;
            out.push(OutSample { t, track: 0, data: Data::From { src: k, sample: s }, duration: s.duration, sync: s.sync });
        }
        // Audio.
        for a in &info.audio {
            let ti = r.track_index(a.track_id).unwrap();
            let t = &r.tracks[ti];
            let ts = t.timescale as f64;
            let Codec::Aac { asc, .. } = &t.codec else { continue };
            let half = 512.0 / ts;
            for s in r.samples(ti, ((mf - half).max(0.0) * ts) as u64, (mt * ts) as u64)? {
                let st = s.dts as f64 / ts;
                if st < mf - half || st >= mt {
                    continue;
                }
                let key = match &a.role {
                    TrackRole::Full => Key::Full,
                    TrackRole::Mic => Key::Mic,
                    TrackRole::App { app } => Key::App(app.key.clone()),
                    TrackRole::Slot { slot } => match slot_app(&info.slot_history, *slot, st - edit) {
                        Some(app) => Key::App(app.key),
                        None => continue,
                    },
                    TrackRole::Other => Key::Other(a.index),
                };
                let clip_t = offset + st - mf;
                if !keys.iter().any(|(kk, ..)| *kk == key) {
                    let app = match (&a.role, &key) {
                        (TrackRole::App { app }, _) => app.clone(),
                        (TrackRole::Slot { slot }, _) => slot_app(&info.slot_history, *slot, st - edit).unwrap(),
                        _ => AppIdent { key: String::new(), name: a.name.clone(), category: String::new() },
                    };
                    let name = match &key {
                        Key::App(_) => app.name.clone(),
                        _ => a.name.clone(),
                    };
                    keys.push((key.clone(), clip_t, app, asc.clone(), name));
                }
                audio_samples.entry(key).or_default().push((clip_t, k, s));
            }
        }
        spans.push(CutSpan { source_index: k, from_s: from, to_s: to, output_start_s: offset });
        offset += to - from;
    }
    if out.is_empty() {
        bail!("nothing was recorded in that range");
    }
    let duration = offset;
    // Output track order: video, full, mic, apps by first appearance, others.
    keys.sort_by(|a, b| {
        let rank = |k: &Key| match k {
            Key::Full => 0,
            Key::Mic => 1,
            Key::App(_) => 2,
            Key::Other(_) => 3,
        };
        rank(&a.0).cmp(&rank(&b.0)).then(a.1.total_cmp(&b.1)).then(a.0.cmp(&b.0))
    });
    let mut specs = vec![TrackSpec {
        id: 1,
        timescale: vt.timescale,
        codec: vt.codec.clone(),
        width: vt.width as u16,
        height: vt.height as u16,
        name: "Video".into(),
        enabled: true,
        default_duration: 0,
    }];
    let mut metas = vec![TrackMeta { id: 1, role: "video".into(), slot: None, app: None, name: "Video".into() }];
    let mut audio_info = Vec::new();
    let first_other = keys.iter().position(|(k, ..)| matches!(k, Key::Other(_)));
    for (i, (key, _, app, asc, name)) in keys.iter().enumerate() {
        let id = i as u32 + 2;
        let (_, rate, ch) = crate::aac::parse_asc(asc).unwrap_or((2, 48_000, 2));
        let enabled = matches!(key, Key::Full) || (Some(i) == first_other && !keys.iter().any(|(k, ..)| *k == Key::Full));
        specs.push(TrackSpec {
            id,
            timescale: rate,
            codec: Codec::Aac { asc: asc.clone(), channels: ch, sample_rate: rate, avg_bitrate: 160_000 },
            width: 0,
            height: 0,
            name: name.clone(),
            enabled,
            default_duration: 1024,
        });
        let (role, app_meta, rrole) = match key {
            Key::Full => ("full", None, TrackRole::Full),
            Key::Mic => ("mic", None, TrackRole::Mic),
            Key::App(_) => ("app", Some(app.clone()), TrackRole::App { app: app.clone() }),
            Key::Other(_) => ("other", None, TrackRole::Other),
        };
        metas.push(TrackMeta { id, role: role.into(), slot: None, app: app_meta, name: name.clone() });
        audio_info.push(AudioTrackInfo { index: i, track_id: id, name: name.clone(), role: rrole, enabled });
        // Lay the frames on a continuous grid starting at clip time 0,
        // filling gaps with silence.
        let mut list = audio_samples.remove(key).unwrap_or_default();
        list.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ts = rate as f64;
        let mut pos: i64 = 0;
        for (t, src, s) in list {
            let p = (t * ts).round() as i64;
            if p < pos - 512 {
                continue; // overlaps what's already there
            }
            while p - pos > 512 {
                out.push(OutSample { t: pos as f64 / ts, track: i + 1, data: Data::Silence, duration: 1024, sync: true });
                pos += 1024;
            }
            out.push(OutSample { t: pos as f64 / ts, track: i + 1, data: Data::From { src, sample: s }, duration: s.duration.max(1), sync: true });
            pos += s.duration.max(1) as i64;
        }
    }
    // Interleave by time.
    out.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.track.cmp(&b.track)));
    let mut silences: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut w = FlatWriter::new(dest, &specs, (start_ms.unwrap_or(0) / 1000).max(0) as u64)?;
    let total = out.len().max(1);
    for (i, o) in out.iter().enumerate() {
        if i % 256 == 0 {
            if cancel.load(Ordering::SeqCst) {
                w.abort();
                bail!("cancelled");
            }
            progress(i as f32 / total as f32 * 0.97);
        }
        let data = match &o.data {
            Data::From { src, sample } => readers[*src].read(sample)?,
            Data::Silence => {
                if !silences.contains_key(&o.track) {
                    let asc = &keys[o.track - 1].3;
                    silences.insert(o.track, silent_frame(asc)?);
                }
                silences[&o.track].clone()
            }
        };
        w.add(o.track, &SampleRef { data: &data, duration: o.duration, sync: o.sync, cts: 0 })?;
    }
    let pre_ticks = (preroll * vts).round() as u64;
    let shown = ((duration * vts).round() as u64).min(video_dts_total.saturating_sub(pre_ticks));
    w.set_edit(0, pre_ticks, shown);
    let meta = SegmentMeta { at_s: 0.0, start_ms, tracks: metas, slots: Vec::new() };
    let size = w.finish(dest, Some(&serde_json::to_vec(&meta)?))?;
    progress(1.0);
    Ok(CutOutput { duration, size, audio: audio_info, spans })
}
