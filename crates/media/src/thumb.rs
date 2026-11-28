//! Thumbnails: decode the keyframe nearest a moment and save a small JPEG.

use crate::mf::h264dec::{nv12_to_rgb, DecodedFrame, VideoDecoder};
use crate::mp4::reader::Mp4Reader;
use crate::mp4::Codec;
use anyhow::{bail, ensure, Context, Result};
use std::path::Path;

// Converting a decimal time to binary can put an exact track tick just below its integer.
fn presentation_tick(seconds: f64, timescale: u32) -> i64 {
    let tick = seconds * f64::from(timescale);
    let nearest = tick.round();
    if (tick - nearest).abs() <= (tick.abs() * f64::EPSILON).min(0.25) {
        nearest as i64
    } else {
        tick.floor() as i64
    }
}

/// Decode the keyframe at or before `at_s` (presentation seconds).
pub fn keyframe_at(r: &Mp4Reader, at_s: f64) -> Result<DecodedFrame> {
    let vi = r.video().context("no playable video")?;
    let t = &r.tracks[vi];
    let Codec::H264 { avcc } = &t.codec else { anyhow::bail!("unsupported video") };
    let media = presentation_tick(at_s.max(0.0), t.timescale).saturating_add(t.edit.map_or(0, |(start, _)| start.max(0)));
    let key = r.sync_at_or_before(vi, media as u64)?.context("no keyframe")?;
    let mut dec = VideoDecoder::new(avcc, None, true)?;
    let data = r.read(&key)?;
    let mut frames = dec.decode(&data, 0)?;
    if frames.is_empty() {
        frames = dec.drain()?;
    }
    frames.into_iter().next().context("the decoder returned no picture")
}

/// Decode the picture whose presentation sample covers `at`, including reordered H.264 frames.
pub fn frame_at_time(reader: &Mp4Reader, video: usize, at: f64) -> Result<(DecodedFrame, f64)> {
    ensure!(at.is_finite() && at >= 0.0, "Choose a nonnegative presentation time");
    let track = reader.tracks.get(video).context("Unknown video track")?;
    let Codec::H264 { avcc } = &track.codec else { bail!("Frame inspection requires H.264") };
    let scale = i64::from(track.timescale);
    ensure!(scale > 0, "Video track has no timebase");
    let edit = track.edit.map(|(start, _)| start.max(0)).unwrap_or(0);
    let media_at = presentation_tick(at, track.timescale).saturating_add(edit);
    let from = media_at.saturating_sub(scale * 3).max(0) as u64;
    let to = media_at.saturating_add(scale * 3) as u64;
    let candidates = reader.samples(video, from, to)?;
    let selected = candidates.iter().filter(|sample| sample.pts() <= media_at)
        .max_by_key(|sample| sample.pts()).context("No video sample at this time")?;
    let seek = reader.sync_at_or_before(video, selected.dts)?.context("No sync sample before this frame")?;
    let samples = reader.samples(video, seek.dts, selected.dts.saturating_add(scale as u64 * 2))?;
    let hns = |pts: i64| (i128::from(pts) * i128::from(cv_core::editor::TICKS_PER_SECOND) / i128::from(scale)) as i64;
    let target = hns(selected.pts());
    let mut decoder = VideoDecoder::new(avcc, None, false)?;
    let presented = (selected.pts() - edit).max(0) as f64 / scale as f64;
    for sample in samples {
        for frame in decoder.decode(&reader.read(&sample)?, hns(sample.pts()))? {
            if frame.pts_hns == target {
                return Ok((frame, presented));
            }
        }
    }
    let frame = decoder.drain()?.into_iter().find(|frame| frame.pts_hns == target)
        .context("Decoder did not return the selected presentation sample")?;
    Ok((frame, presented))
}

/// Decode a downscaled picture at `at_s` without writing a file.
pub fn picture(path: &Path, at_s: f64, width: u32) -> Result<image::RgbImage> {
    let r = Mp4Reader::open(path)?;
    let dur = r.duration_secs();
    let f = keyframe_at(&r, at_s.min((dur - 0.5).max(0.0))).or_else(|_| keyframe_at(&r, 0.0))?;
    Ok(nv12_to_rgb(&f, width))
}

/// Save a JPEG `width` pixels wide of the picture at `at_s`.
pub fn thumbnail(path: &Path, at_s: f64, out: &Path, width: u32) -> Result<()> {
    let img = picture(path, at_s, width)?;
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = out.with_extension("tmp.jpg");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 82).encode_image(&img)?;
    drop(file);
    std::fs::rename(&tmp, out)?;
    Ok(())
}
