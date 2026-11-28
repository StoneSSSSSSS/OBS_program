//! Reading an audio track as 48 kHz stereo PCM over time, with silence where
//! the track has no samples (unused app tracks) and resampling for files
//! recorded at other rates.

use crate::mf::aac::AacDecoder;
use crate::mp4::reader::{Mp4Reader, Sample};
use crate::mp4::Codec;
use anyhow::{Context, Result};
use std::collections::VecDeque;

pub const OUT_RATE: u32 = 48_000;

/// Sequential reader of one audio track. Positions are output frames
/// (48 kHz) on the track's own media timeline.
pub struct TrackPcm {
    idx: usize,
    rate: u32,
    channels: usize,
    dec: AacDecoder,
    /// Decoded native-rate stereo frames, starting at `buf_pos` (native frames).
    buf: VecDeque<f32>,
    buf_pos: i64,
    /// Next native frame position we will fetch samples from.
    fetch_pos: i64,
    pending: VecDeque<Sample>,
    /// Decode time of the last sample queued (avoids queueing one twice).
    last_dts: i64,
    end: i64,
}

impl TrackPcm {
    /// Reader for audio track `idx` (a reader track index).
    pub fn new(r: &Mp4Reader, idx: usize) -> Result<TrackPcm> {
        let t = &r.tracks[idx];
        let Codec::Aac { asc, .. } = &t.codec else { anyhow::bail!("not an AAC track") };
        let dec = AacDecoder::new(asc)?;
        let channels = dec.channels.max(1) as usize;
        Ok(TrackPcm { idx, rate: t.timescale.max(1), channels, dec, buf: VecDeque::new(), buf_pos: 0, fetch_pos: 0, pending: VecDeque::new(), last_dts: -1, end: r.end_ticks(idx) as i64 })
    }

    fn to_native(&self, out_frames: i64) -> i64 {
        (out_frames as i128 * self.rate as i128 / OUT_RATE as i128) as i64
    }

    /// Jump to `pos` (output frames). Decodes a little before it so the
    /// codec has settled.
    pub fn seek(&mut self, r: &Mp4Reader, pos: i64) -> Result<()> {
        let native = self.to_native(pos.max(0));
        let warm = 2048;
        self.dec.reset();
        self.buf.clear();
        self.pending.clear();
        self.last_dts = -1;
        self.fetch_pos = (native - warm).max(0);
        self.buf_pos = self.fetch_pos;
        self.fill(r, native)?;
        // Drop everything before the target.
        let drop = ((native - self.buf_pos).max(0) as usize * 2).min(self.buf.len());
        self.buf.drain(..drop);
        self.buf_pos += drop as i64 / 2;
        if self.buf_pos < native && self.buf.is_empty() {
            self.buf_pos = native;
        }
        Ok(())
    }

    /// Decode until the buffer reaches native frame `upto`.
    fn fill(&mut self, r: &Mp4Reader, upto: i64) -> Result<()> {
        while self.buf_pos + (self.buf.len() / 2) as i64 <= upto {
            if self.pending.is_empty() {
                if self.fetch_pos >= self.end {
                    // Past the end: silence.
                    let n = (upto - (self.buf_pos + (self.buf.len() / 2) as i64) + 1).max(1) as usize;
                    self.buf.extend(std::iter::repeat(0.0).take(n * 2));
                    break;
                }
                let to = self.fetch_pos + self.rate as i64 * 2;
                let s = r.samples(self.idx, self.fetch_pos as u64, to as u64)?;
                self.fetch_pos = to;
                let have_until = self.buf_pos + (self.buf.len() / 2) as i64;
                let last = self.last_dts;
                self.pending.extend(s.into_iter().filter(|x| x.end() as i64 > have_until && x.dts as i64 > last));
                if let Some(l) = self.pending.back() {
                    self.last_dts = l.dts as i64;
                }
                if self.pending.is_empty() {
                    // A gap in the track: silence until the next window.
                    let gap_end = to.min(upto + 1);
                    let n = (gap_end - have_until).max(0) as usize;
                    self.buf.extend(std::iter::repeat(0.0).take(n * 2));
                    continue;
                }
            }
            let s = self.pending.pop_front().unwrap();
            let have_until = self.buf_pos + (self.buf.len() / 2) as i64;
            let start = s.dts as i64;
            if start > have_until {
                // Silence before this frame, and a fresh decoder state.
                let n = (start - have_until) as usize;
                self.buf.extend(std::iter::repeat(0.0).take(n * 2));
            }
            let data = r.read(&s)?;
            let pcm = self.dec.decode(&data).unwrap_or_default();
            let frames = pcm.len() / self.channels;
            // Place the decoded frame at its timestamp, skipping overlap.
            let have_until = self.buf_pos + (self.buf.len() / 2) as i64;
            let skip = (have_until - start).clamp(0, frames as i64) as usize;
            for f in skip..frames {
                let (l, rr) = if self.channels == 1 { (pcm[f], pcm[f]) } else { (pcm[f * self.channels], pcm[f * self.channels + 1]) };
                self.buf.push_back(l);
                self.buf.push_back(rr);
            }
        }
        Ok(())
    }

    /// Read `out.len() / 2` stereo frames starting at output frame `pos`.
    /// Calls should be sequential; a jump triggers a seek.
    pub fn read(&mut self, r: &Mp4Reader, pos: i64, out: &mut [f32]) -> Result<()> {
        let n = out.len() / 2;
        if self.rate == OUT_RATE {
            if pos < self.buf_pos || pos > self.buf_pos + (self.buf.len() / 2) as i64 + 4800 {
                self.seek(r, pos)?;
            }
            self.fill(r, pos + n as i64)?;
            let off = ((pos - self.buf_pos).max(0) as usize) * 2;
            for (i, o) in out.iter_mut().enumerate() {
                *o = self.buf.get(off + i).copied().unwrap_or(0.0);
            }
            // Drop consumed frames.
            let consumed = (off + n * 2).min(self.buf.len());
            self.buf.drain(..consumed);
            self.buf_pos += consumed as i64 / 2;
            return Ok(());
        }
        // Linear resampling from the native rate.
        let ratio = self.rate as f64 / OUT_RATE as f64;
        let first = (pos as f64 * ratio).floor() as i64;
        if first < self.buf_pos || first > self.buf_pos + (self.buf.len() / 2) as i64 + 4800 {
            self.seek(r, pos)?;
        }
        let last = ((pos + n as i64) as f64 * ratio).ceil() as i64 + 1;
        self.fill(r, last)?;
        for i in 0..n {
            let x = (pos + i as i64) as f64 * ratio;
            let k = x.floor() as i64;
            let frac = (x - k as f64) as f32;
            let at = |f: i64, c: usize| -> f32 { self.buf.get(((f - self.buf_pos).max(0) as usize) * 2 + c).copied().unwrap_or(0.0) };
            out[i * 2] = at(k, 0) * (1.0 - frac) + at(k + 1, 0) * frac;
            out[i * 2 + 1] = at(k, 1) * (1.0 - frac) + at(k + 1, 1) * frac;
        }
        let keep_from = first.max(self.buf_pos);
        let consumed = (((keep_from - self.buf_pos) as usize) * 2).min(self.buf.len());
        self.buf.drain(..consumed);
        self.buf_pos += consumed as i64 / 2;
        Ok(())
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
}

/// Convenience: decode `[from, to)` seconds of a track into a buffer.
pub fn read_range(r: &Mp4Reader, idx: usize, from_s: f64, to_s: f64) -> Result<Vec<f32>> {
    let mut t = TrackPcm::new(r, idx).context("audio track")?;
    let a = (from_s * OUT_RATE as f64) as i64;
    let n = ((to_s - from_s).max(0.0) * OUT_RATE as f64) as usize;
    let mut out = vec![0.0; n * 2];
    t.seek(r, a)?;
    t.read(r, a, &mut out)?;
    Ok(out)
}
