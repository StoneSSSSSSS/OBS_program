//! Thumbnails: decode the keyframe nearest a moment and save a small JPEG.

use crate::mf::h264dec::{nv12_to_rgb, DecodedFrame, VideoDecoder};
use crate::mp4::reader::Mp4Reader;
use crate::mp4::Codec;
use anyhow::{Context, Result};
use std::path::Path;

/// Decode the keyframe at or before `at_s` (presentation seconds).
pub fn keyframe_at(r: &Mp4Reader, at_s: f64) -> Result<DecodedFrame> {
    let vi = r.video().context("no playable video")?;
    let t = &r.tracks[vi];
    let Codec::H264 { avcc } = &t.codec else { anyhow::bail!("unsupported video") };
    let media = (at_s.max(0.0) + t.edit_start_s()) * t.timescale as f64;
    let key = r.sync_at_or_before(vi, media as u64)?.context("no keyframe")?;
    let mut dec = VideoDecoder::new(avcc, None, true)?;
    let data = r.read(&key)?;
    let mut frames = dec.decode(&data, 0)?;
    if frames.is_empty() {
        frames = dec.drain()?;
    }
    frames.into_iter().next().context("the decoder returned no picture")
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
