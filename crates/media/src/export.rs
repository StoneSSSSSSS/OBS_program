//! Exports for sharing: one mixed audio track (your volumes and edits baked
//! in) and either the original video (lossless, instant) or video re-encoded
//! on the GPU to fit a size limit.

use crate::audioread::{TrackPcm, OUT_RATE};
use crate::d3d::{fit_size, Converter, Gpu};
use crate::mf::aac::{AacEncoder, FRAME};
use crate::mf::h264::{EncoderChoice, EncoderConfig, RateControl, VideoEncoder};
use crate::mf::h264dec::VideoDecoder;
use crate::mp4::flat::FlatWriter;
use crate::mp4::reader::{Mp4Reader, Sample};
use crate::mp4::{Codec, SampleRef, TrackSpec};
use anyhow::{bail, Context, Result};
use cv_core::mixer::{limit, GainCurve};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum VideoOut {
    /// Keep the original video (no quality loss, very fast).
    Copy,
    /// Re-encode at this bitrate, scaled to fit `max_height` (0 = keep).
    Reencode { kbps: u32, max_height: u32 },
}

#[derive(Debug, Clone)]
pub struct ExportJob {
    pub source: PathBuf,
    /// Presentation seconds; None = whole file.
    pub range: Option<(f64, f64)>,
    /// Gain curve per audio track (in `probe` order), in source seconds.
    pub gains: Vec<GainCurve>,
    pub video: VideoOut,
    pub audio_kbps: u32,
    pub start_ms: Option<i64>,
}

/// Video bitrate (kbit/s) that fits `target_mb` for `duration_s`, or None
/// if it can't be done at a watchable quality.
pub fn bitrate_for_size(target_mb: u32, duration_s: f64, audio_kbps: u32) -> Option<u32> {
    if duration_s <= 0.0 {
        return None;
    }
    let total_kbits = target_mb as f64 * 8.0 * 1024.0 * 0.95; // container overhead
    let video = total_kbits / duration_s - audio_kbps as f64;
    (video >= 150.0).then(|| video.floor() as u32)
}

/// Height to scale to for a bitrate so the picture doesn't fall apart.
pub fn height_for_bitrate(kbps: u32, source_h: u32, max_h: u32) -> u32 {
    let cap = if kbps < 1200 {
        480
    } else if kbps < 2500 {
        720
    } else if kbps < 6000 {
        1080
    } else {
        source_h
    };
    let mut h = source_h.min(cap);
    if max_h > 0 {
        h = h.min(max_h);
    }
    h - h % 2
}

struct Out {
    t: f64,
    track: usize,
    data: OutData,
    duration: u32,
    sync: bool,
}

enum OutData {
    Owned(Vec<u8>),
    Copy(Sample),
}

pub fn export(job: &ExportJob, dest: &Path, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<u64> {
    let r = Mp4Reader::open(&job.source)?;
    let info = crate::probe::probe_reader(&r);
    let vi = r.video().context("no playable video in this clip")?;
    let vt = r.tracks[vi].clone();
    let Codec::H264 { avcc } = &vt.codec else { bail!("unsupported video") };
    let vedit = vt.edit_start_s();
    let full = info.duration;
    let (a, b) = job.range.map(|(a, b)| (a.max(0.0), b.min(full))).unwrap_or((0.0, full));
    if b <= a + 0.05 {
        bail!("the export range is empty");
    }
    let dur = b - a;
    let vts = vt.timescale as f64;
    let mut outs: Vec<Out> = Vec::new();
    let mut specs: Vec<TrackSpec> = Vec::new();
    let mut edit: Option<(u64, u64)> = None;

    // ---- video
    match &job.video {
        VideoOut::Copy => {
            let key = r.sync_at_or_before(vi, ((a + vedit) * vts) as u64)?.context("no keyframe")?;
            let pre = (a + vedit) - key.dts as f64 / vts;
            for s in r.samples(vi, key.dts, ((b + vedit) * vts).ceil() as u64)? {
                outs.push(Out { t: s.dts as f64 / vts - vedit - a, track: 0, data: OutData::Copy(s), duration: s.duration, sync: s.sync });
            }
            specs.push(TrackSpec { id: 1, timescale: vt.timescale, codec: vt.codec.clone(), width: vt.width as u16, height: vt.height as u16, name: "Video".into(), enabled: true, default_duration: 0 });
            edit = Some(((pre * vts).round() as u64, (dur * vts).round() as u64));
        }
        VideoOut::Reencode { kbps, max_height } => {
            let (frames, spec) = reencode(&r, vi, avcc, a + vedit, b + vedit, *kbps, *max_height, info.fps, progress, cancel)?;
            for (t, data, sync) in frames {
                outs.push(Out { t: t - a - vedit, track: 0, data: OutData::Owned(data), duration: spec.default_duration, sync });
            }
            specs.push(spec);
        }
    }
    // Durations from timestamps (re-encoded frames), last frame to the end.
    fix_durations(&mut outs, 0, dur, specs[0].timescale as f64);

    // ---- audio: mix the tracks into one
    let tracks: Vec<(usize, &GainCurve)> = info.audio.iter().filter_map(|t| job.gains.get(t.index).filter(|g| !g.is_silent()).map(|g| (r.track_index(t.track_id).unwrap(), g))).collect();
    if !tracks.is_empty() {
        let mut readers: Vec<(TrackPcm, &GainCurve)> = tracks.iter().map(|(ti, g)| Ok((TrackPcm::new(&r, *ti)?, *g))).collect::<Result<_>>()?;
        let mut enc = AacEncoder::new(OUT_RATE, 2, job.audio_kbps)?;
        let total = (dur * OUT_RATE as f64).round() as i64;
        let start = (a * OUT_RATE as f64).round() as i64;
        for (t, _) in readers.iter_mut() {
            t.seek(&r, start)?;
        }
        let chunk = OUT_RATE as i64 / 2;
        let mut pos = 0i64;
        let mut frames_out = 0u64;
        let mut tmp = vec![0.0f32; chunk as usize * 2];
        let mut gains = vec![0.0f32; chunk as usize];
        while pos < total {
            if cancel.load(Ordering::SeqCst) {
                bail!("cancelled");
            }
            let n = (total - pos).min(chunk) as usize;
            let mut mix = vec![0.0f32; n * 2];
            for (t, g) in readers.iter_mut() {
                t.read(&r, start + pos, &mut tmp[..n * 2])?;
                g.fill((start + pos) as f64 / OUT_RATE as f64, 1.0 / OUT_RATE as f64, &mut gains[..n]);
                for i in 0..n {
                    mix[i * 2] += tmp[i * 2] * gains[i];
                    mix[i * 2 + 1] += tmp[i * 2 + 1] * gains[i];
                }
            }
            for v in mix.iter_mut() {
                *v = limit(*v);
            }
            for f in enc.encode(&mix)? {
                outs.push(Out { t: frames_out as f64 * FRAME as f64 / OUT_RATE as f64, track: 1, data: OutData::Owned(f), duration: FRAME as u32, sync: true });
                frames_out += 1;
            }
            pos += n as i64;
            if !matches!(job.video, VideoOut::Reencode { .. }) {
                progress((pos as f32 / total as f32) * 0.8);
            }
        }
        for f in enc.finish()? {
            outs.push(Out { t: frames_out as f64 * FRAME as f64 / OUT_RATE as f64, track: 1, data: OutData::Owned(f), duration: FRAME as u32, sync: true });
            frames_out += 1;
        }
        specs.push(TrackSpec {
            id: 2,
            timescale: OUT_RATE,
            codec: Codec::Aac { asc: enc.asc.clone(), channels: 2, sample_rate: OUT_RATE, avg_bitrate: enc.bitrate_kbps * 1000 },
            width: 0,
            height: 0,
            name: "Audio".into(),
            enabled: true,
            default_duration: FRAME as u32,
        });
    }

    // ---- write
    outs.sort_by(|x, y| x.t.total_cmp(&y.t).then(x.track.cmp(&y.track)));
    let mut w = FlatWriter::new(dest, &specs, (job.start_ms.unwrap_or(0) / 1000).max(0) as u64)?;
    for o in &outs {
        if cancel.load(Ordering::SeqCst) {
            w.abort();
            bail!("cancelled");
        }
        let data = match &o.data {
            OutData::Owned(d) => std::borrow::Cow::Borrowed(d.as_slice()),
            OutData::Copy(s) => std::borrow::Cow::Owned(r.read(s)?),
        };
        w.add(o.track, &SampleRef { data: &data, duration: o.duration.max(1), sync: o.sync, cts: 0 })?;
    }
    if let Some((m, d)) = edit {
        let total = w.track_ticks(0);
        w.set_edit(0, m, d.min(total.saturating_sub(m)));
    }
    let size = w.finish(dest, None)?;
    progress(1.0);
    Ok(size)
}

/// Set each sample's duration from the next sample's time on the same track.
fn fix_durations(outs: &mut [Out], track: usize, end_t: f64, ts: f64) {
    let idx: Vec<usize> = (0..outs.len()).filter(|i| outs[*i].track == track).collect();
    for k in 0..idx.len() {
        let t = outs[idx[k]].t;
        let next = idx.get(k + 1).map(|j| outs[*j].t).unwrap_or(end_t.max(t + 1.0 / 60.0));
        if matches!(outs[idx[k]].data, OutData::Owned(_)) {
            outs[idx[k]].duration = (((next - t) * ts).round() as u32).max(1);
        }
    }
}

/// Decode [a, b) (media seconds) and re-encode it. Returns (media time s, data, keyframe).
#[allow(clippy::too_many_arguments)]
fn reencode(r: &Mp4Reader, vi: usize, avcc: &[u8], a: f64, b: f64, kbps: u32, max_height: u32, fps: f64, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<(Vec<(f64, Vec<u8>, bool)>, TrackSpec)> {
    let t = &r.tracks[vi];
    let vts = t.timescale as f64;
    let mut dec = VideoDecoder::new(avcc, None, true)?;
    let src = (dec.width, dec.height);
    let target_h = height_for_bitrate(kbps, src.1, max_height);
    let (w, h) = fit_size(src, target_h);
    let fps_i = fps.round().clamp(10.0, 240.0) as u32;
    let gpu = Gpu::primary().ok().map(Arc::new);
    let mut enc = VideoEncoder::new(gpu.clone(), EncoderConfig { width: w, height: h, fps: fps_i, rate: RateControl::Cbr { kbps }, gop: fps_i * 2, choice: EncoderChoice::Auto, realtime: false })?;
    let mut conv = match (&gpu, (w, h) != src) {
        (Some(g), true) => Some(Converter::new(g, src, (w, h), false)?),
        _ => None,
    };
    let pool: Vec<_> = match &gpu {
        Some(g) => (0..6).map(|_| g.nv12_target(w, h)).collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let key = r.sync_at_or_before(vi, (a * vts) as u64)?.context("no keyframe")?;
    let samples = r.samples(vi, key.dts, (b * vts).ceil() as u64 + 1)?;
    let mut out: Vec<(f64, Vec<u8>, bool)> = Vec::new();
    let mut params: (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut n = 0usize;
    let total = samples.len().max(1);
    let handle = |frames: Vec<crate::mf::h264::EncodedFrame>, out: &mut Vec<(f64, Vec<u8>, bool)>, params: &mut (Option<Vec<u8>>, Option<Vec<u8>>)| {
        for f in frames {
            let c = crate::h264::annexb_to_avcc(&f.data);
            if c.sps.is_some() {
                params.0 = c.sps.clone();
                params.1 = c.pps.clone();
            }
            out.push((f.time_hns as f64 / 1e7, c.data, f.key || c.idr));
        }
    };
    let mut encode_frame = |frame: crate::mf::h264dec::DecodedFrame, enc: &mut VideoEncoder, n: &mut usize| -> Result<Vec<crate::mf::h264::EncodedFrame>> {
        let time = frame.pts_hns;
        if let (Some(g), Some(c)) = (&gpu, conv.as_mut()) {
            if !enc.wants_cpu_frames() {
                use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE};
                let up = g.upload_nv12(&frame.nv12, src.0, src.1, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
                let dst = &pool[*n % pool.len()];
                c.blit(&up, dst)?;
                *n += 1;
                return enc.encode_texture(dst, time, false);
            }
        }
        if (w, h) != src {
            let scaled = scale_nv12_cpu(&frame.nv12, src.0, src.1, w, h);
            enc.encode_cpu(&scaled, time, false)
        } else {
            enc.encode_cpu(&frame.nv12, time, false)
        }
    };
    for (i, s) in samples.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let pts = s.pts() as f64 / vts;
        let data = r.read(s)?;
        for f in dec.decode(&data, (pts * 1e7) as i64)? {
            let ft = f.pts_hns as f64 / 1e7;
            if ft + 1e-4 < a || ft >= b {
                continue;
            }
            let frames = encode_frame(f, &mut enc, &mut n)?;
            handle(frames, &mut out, &mut params);
        }
        if i % 8 == 0 {
            progress(i as f32 / total as f32 * 0.85);
        }
    }
    for f in dec.drain()? {
        let ft = f.pts_hns as f64 / 1e7;
        if ft + 1e-4 >= a && ft < b {
            let frames = encode_frame(f, &mut enc, &mut n)?;
            handle(frames, &mut out, &mut params);
        }
    }
    let rest = enc.finish()?;
    handle(rest, &mut out, &mut params);
    if out.is_empty() {
        bail!("no video frames in that range");
    }
    out.sort_by(|x, y| x.0.total_cmp(&y.0));
    let sps = params.0.context("encoder produced no SPS")?;
    let pps = params.1.context("encoder produced no PPS")?;
    let spec = TrackSpec {
        id: 1,
        timescale: 90_000,
        codec: Codec::H264 { avcc: crate::h264::build_avcc(&sps, &pps) },
        width: w as u16,
        height: h as u16,
        name: "Video".into(),
        enabled: true,
        default_duration: (90_000.0 / fps.max(1.0)) as u32,
    };
    Ok((out, spec))
}

/// Bilinear NV12 scaling on the CPU (fallback when there's no GPU).
pub fn scale_nv12_cpu(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let mut out = vec![0u8; dw * dh * 3 / 2];
    let sample = |plane: &[u8], pw: usize, ph: usize, stride: usize, comps: usize, c: usize, x: f32, y: f32| -> u8 {
        let x0 = (x.floor() as usize).min(pw - 1);
        let y0 = (y.floor() as usize).min(ph - 1);
        let x1 = (x0 + 1).min(pw - 1);
        let y1 = (y0 + 1).min(ph - 1);
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let p = |xx: usize, yy: usize| plane[yy * stride + xx * comps + c] as f32;
        let v = p(x0, y0) * (1.0 - fx) * (1.0 - fy) + p(x1, y0) * fx * (1.0 - fy) + p(x0, y1) * (1.0 - fx) * fy + p(x1, y1) * fx * fy;
        v.round().clamp(0.0, 255.0) as u8
    };
    let (ys, uvs) = src.split_at(sw * sh);
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).max(0.0);
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).max(0.0);
            out[y * dw + x] = sample(ys, sw, sh, sw, 1, 0, sx, sy);
        }
    }
    let (cw, ch, dcw, dch) = (sw / 2, sh / 2, dw / 2, dh / 2);
    for y in 0..dch {
        let sy = ((y as f32 + 0.5) * ch as f32 / dch as f32 - 0.5).max(0.0);
        for x in 0..dcw {
            let sx = ((x as f32 + 0.5) * cw as f32 / dcw as f32 - 0.5).max(0.0);
            let o = dw * dh + y * dw + x * 2;
            out[o] = sample(uvs, cw, ch, sw, 2, 0, sx, sy);
            out[o + 1] = sample(uvs, cw, ch, sw, 2, 1, sx, sy);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_targets() {
        let k = bitrate_for_size(10, 30.0, 128).unwrap();
        assert!((2400..2600).contains(&k), "{k}");
        assert_eq!(bitrate_for_size(1, 600.0, 128), None);
        assert_eq!(height_for_bitrate(k, 1440, 1080), 720, "10 MB for 30 s is a 720p job");
        assert_eq!(height_for_bitrate(2000, 1440, 1080), 720);
        assert_eq!(height_for_bitrate(8000, 1440, 0), 1440);
        assert_eq!(height_for_bitrate(900, 1080, 0), 480);
    }

    #[test]
    fn cpu_scaler_keeps_flat_colour() {
        let (w, h) = (8u32, 4u32);
        let mut src = vec![100u8; (w * h) as usize];
        src.extend(std::iter::repeat([90u8, 160u8]).take((w * h / 4) as usize).flatten());
        let out = scale_nv12_cpu(&src, w, h, 4, 2);
        assert_eq!(out.len(), 4 * 2 * 3 / 2);
        assert!(out[..8].iter().all(|v| *v == 100));
        assert_eq!(&out[8..10], &[90, 160]);
    }
}
