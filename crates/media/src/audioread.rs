//! Reading an audio track as 48 kHz stereo PCM over time, with silence where
//! the track has no samples (unused app tracks) and resampling for files
//! recorded at other rates.

use crate::mf::aac::{AacDecoder, DecodedPcm, FRAME};
use crate::mf::HNS;
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
    timescale: u32,
    channels: usize,
    dec: AacDecoder,
    /// Decoded native-rate stereo frames, starting at `buf_pos` (native frames).
    buf: VecDeque<f32>,
    buf_pos: i64,
    /// Next media tick we will fetch packets from.
    fetch_pos: u64,
    pending: VecDeque<Sample>,
    /// Decode time of the last packet queued (avoids queueing one twice).
    last_dts: Option<u64>,
    /// End of the last packet accepted by the decoder for this continuous run.
    decoder_end: Option<u64>,
    end: u64,
}

impl TrackPcm {
    /// Reader for audio track `idx` (a reader track index).
    pub fn new(r: &Mp4Reader, idx: usize) -> Result<TrackPcm> {
        let t = &r.tracks[idx];
        let Codec::Aac { asc, .. } = &t.codec else { anyhow::bail!("not an AAC track") };
        let dec = AacDecoder::new(asc)?;
        let channels = dec.channels.max(1) as usize;
        Ok(TrackPcm {
            idx,
            rate: dec.rate,
            timescale: t.timescale.max(1),
            channels,
            dec,
            buf: VecDeque::new(),
            buf_pos: 0,
            fetch_pos: 0,
            pending: VecDeque::new(),
            last_dts: None,
            decoder_end: None,
            end: r.end_ticks(idx),
        })
    }

    fn to_native(&self, out_frames: i64) -> i64 {
        (out_frames as i128 * self.rate as i128 / OUT_RATE as i128) as i64
    }

    /// Jump to `pos` (output frames). Decodes a little before it so the
    /// codec has settled.
    pub fn seek(&mut self, r: &Mp4Reader, pos: i64) -> Result<()> {
        let native = self.to_native(pos.max(0));
        let preroll = (native - 2 * FRAME as i64).max(0);
        self.dec.reset();
        self.buf.clear();
        self.pending.clear();
        self.last_dts = None;
        self.decoder_end = None;
        self.fetch_pos = (i128::from(preroll) * i128::from(self.timescale)
            / i128::from(self.rate)) as u64;
        self.buf_pos = preroll;
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

    fn buf_end(&self) -> i64 {
        self.buf_pos + (self.buf.len() / 2) as i64
    }

    fn silence_until(&mut self, end: i64) {
        let frames = end.saturating_sub(self.buf_end()).max(0) as usize;
        self.buf.extend(std::iter::repeat(0.0).take(frames * 2));
    }

    fn media_frame(&self, tick: i64) -> i64 {
        Self::frame_at(tick, self.rate, i64::from(self.timescale))
    }

    fn frame_at(timestamp: i64, rate: u32, clock: i64) -> i64 {
        let scaled = i128::from(timestamp) * i128::from(rate);
        let clock = i128::from(clock);
        // MFT timestamps are truncated to 100 ns. Round back to the nearest PCM frame.
        ((scaled + scaled.signum() * (clock / 2)) / clock) as i64
    }

    fn append_decoded(&mut self, output: Vec<DecodedPcm>, end_ticks: u64) {
        let end = self.media_frame(end_ticks as i64);
        for block in output {
            let start = Self::frame_at(block.time_hns, self.rate, HNS);
            self.silence_until(start.min(end));
            let frames = (end - start)
                .max(0)
                .min((block.samples.len() / self.channels) as i64);
            let skip = (self.buf_end() - start).clamp(0, frames) as usize;
            for frame in block.samples.chunks_exact(self.channels).take(frames as usize).skip(skip) {
                self.buf.push_back(frame[0]);
                self.buf.push_back(frame[usize::from(self.channels > 1)]);
            }
        }
    }

    fn drain_decoder(&mut self) {
        if let Some(end) = self.decoder_end.take() {
            // A whole AAC frame can contain padding beyond the run's last packet duration.
            match self.dec.drain() {
                Ok(output) => self.append_decoded(output, end),
                Err(e) => tracing::debug!("AAC drain at {end}: {e:#}"),
            }
            self.dec.reset();
        }
    }

    /// Decode until the buffer reaches native frame `upto`.
    fn fill(&mut self, r: &Mp4Reader, upto: i64) -> Result<()> {
        while self.buf_end() <= upto {
            if self.pending.is_empty() && self.fetch_pos < self.end {
                let to = self.fetch_pos + u64::from(self.timescale) * 2;
                let samples = r.samples(self.idx, self.fetch_pos, to)?;
                self.fetch_pos = to;
                self.pending.extend(samples.into_iter().filter(|s| {
                    self.last_dts.is_none_or(|last| s.dts > last)
                }));
                if let Some(last) = self.pending.back() {
                    self.last_dts = Some(last.dts);
                }
            }

            let Some(&packet) = self.pending.front() else {
                self.drain_decoder();
                let silence_end = if self.fetch_pos >= self.end {
                    upto + 1
                } else {
                    self.media_frame(self.fetch_pos as i64).min(upto + 1)
                };
                self.silence_until(silence_end);
                continue;
            };
            if self.decoder_end.is_none_or(|end| packet.dts > end) {
                self.drain_decoder();
                self.silence_until(self.media_frame(packet.pts()).min(upto + 1));
                if self.buf_end() > upto {
                    break;
                }
            }

            self.pending.pop_front();
            let data = r.read(&packet)?;
            // The transform may return PCM for an earlier packet after accepting this one.
            let to_hns = |ticks: i64| {
                (i128::from(ticks) * i128::from(HNS) / i128::from(self.timescale)) as i64
            };
            let timestamp = to_hns(packet.pts());
            let duration = to_hns(i64::from(packet.duration)).max(1);
            match self.dec.decode_timed(&data, timestamp, duration) {
                Ok(output) => {
                    let end = packet.end();
                    self.decoder_end = Some(end);
                    self.append_decoded(output, end);
                }
                Err(e) => {
                    tracing::debug!("AAC packet at {}: {e:#}", packet.dts);
                    if self.decoder_end.is_some() {
                        self.drain_decoder();
                    } else {
                        self.dec.reset();
                    }
                }
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
