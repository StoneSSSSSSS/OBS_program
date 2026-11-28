//! Audio waveforms for the timeline lanes.
//!
//! Every audio track becomes a loudness envelope (10 values per second per
//! track, 0-255 on a dB scale), stored in
//! `%LOCALAPPDATA%\ClipVault\waves\<hash>.cvw`. The recorder writes them
//! live while recording; other files are decoded in the background.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub const PEAKS_PER_SEC: u32 = 10;
/// Envelope sample rate produced by ffmpeg (decimated mean |x|).
const MAGIC: &[u8; 4] = b"CVW1";
/// dB range mapped onto 0..255.
const FLOOR_DB: f32 = -54.0;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Waveform {
    pub tracks: usize,
    pub rate: u32,
    /// Interleaved: frame-major, `tracks` values per frame.
    pub peaks: Vec<u8>,
}

impl Waveform {
    pub fn frames(&self) -> usize {
        if self.tracks == 0 {
            0
        } else {
            self.peaks.len() / self.tracks
        }
    }

    pub fn duration_s(&self) -> f64 {
        self.frames() as f64 / self.rate.max(1) as f64
    }

    pub fn get(&self, frame: usize, track: usize) -> u8 {
        self.peaks.get(frame * self.tracks + track).copied().unwrap_or(0)
    }

    /// Loudest value for `track` within `[from_s, to_s)`, looking at no more
    /// than `max_samples` frames (strided) so zoomed-out views stay cheap.
    pub fn max_in(&self, track: usize, from_s: f64, to_s: f64, max_samples: usize) -> u8 {
        if track >= self.tracks || to_s <= 0.0 {
            return 0;
        }
        let r = self.rate as f64;
        let a = (from_s.max(0.0) * r).floor() as usize;
        let b = ((to_s * r).ceil() as usize).min(self.frames());
        if b <= a {
            return if a < self.frames() { self.get(a, track) } else { 0 };
        }
        let step = ((b - a) / max_samples.max(1)).max(1);
        (a..b).step_by(step).map(|f| self.get(f, track)).max().unwrap_or(0)
    }

    pub fn append(&mut self, other: &Waveform) {
        if self.tracks == 0 {
            *self = other.clone();
        } else if other.tracks == self.tracks {
            self.peaks.extend_from_slice(&other.peaks);
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(16 + self.peaks.len());
        v.extend_from_slice(MAGIC);
        v.extend_from_slice(&(self.tracks as u32).to_le_bytes());
        v.extend_from_slice(&self.rate.to_le_bytes());
        v.extend_from_slice(&(self.frames() as u32).to_le_bytes());
        v.extend_from_slice(&self.peaks[..self.frames() * self.tracks]);
        v
    }

    pub fn from_bytes(b: &[u8]) -> Result<Waveform> {
        if b.len() < 16 || &b[..4] != MAGIC {
            bail!("not a waveform file");
        }
        let rd = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (tracks, rate, frames) = (rd(4) as usize, rd(8), rd(12) as usize);
        let need = 16 + tracks * frames;
        if tracks == 0 || tracks > 16 || rate == 0 || b.len() < need {
            bail!("corrupt waveform file");
        }
        Ok(Waveform { tracks, rate, peaks: b[16..need].to_vec() })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::util::write_atomic(path, &self.to_bytes())?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Waveform> {
        Self::from_bytes(&std::fs::read(path)?)
    }
}

/// Where the waveform for `media` is cached.
pub fn wave_path(media: &Path) -> PathBuf {
    // FNV-1a over the lower-cased path: stable across runs and processes.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in media.to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    crate::paths::data_dir().join("waves").join(format!("{h:016x}.cvw"))
}

/// Mean |amplitude| (0..1) -> 0..255 on a dB scale.
pub fn level_u8(amp: f32) -> u8 {
    if !(amp > 0.0) {
        return 0;
    }
    let db = 20.0 * amp.log10();
    (((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Turns PCM into 10/s peak values, one track at a time.
#[derive(Debug, Clone)]
pub struct PeakMeter {
    per: usize,
    count: usize,
    max: f32,
}

impl PeakMeter {
    /// `rate` = sample frames per second of the input.
    pub fn new(rate: u32) -> PeakMeter {
        PeakMeter { per: (rate / PEAKS_PER_SEC).max(1) as usize, count: 0, max: 0.0 }
    }

    /// Feed interleaved samples (`channels` per frame); returns completed peaks.
    pub fn push(&mut self, pcm: &[f32], channels: usize, out: &mut Vec<u8>) {
        for frame in pcm.chunks(channels.max(1)) {
            for v in frame {
                self.max = self.max.max(v.abs());
            }
            self.count += 1;
            if self.count >= self.per {
                out.push(level_u8(self.max));
                self.count = 0;
                self.max = 0.0;
            }
        }
    }

    /// Feed `frames` frames of silence.
    pub fn silence(&mut self, mut frames: usize, out: &mut Vec<u8>) {
        while frames > 0 {
            let n = (self.per - self.count).min(frames);
            self.count += n;
            frames -= n;
            if self.count >= self.per {
                out.push(level_u8(self.max));
                self.count = 0;
                self.max = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_u8(0.0), 0);
        assert_eq!(level_u8(1.0), 255);
        assert_eq!(level_u8(0.0001), 0); // -80 dB is below the floor
        let half = level_u8(0.5); // -6 dB
        assert!((220..235).contains(&half), "{half}");
        assert_eq!(level_u8(f32::NAN), 0);
    }

    #[test]
    fn file_roundtrip_and_queries() {
        let w = Waveform { tracks: 2, rate: 10, peaks: vec![0, 10, 50, 20, 100, 30, 0, 40] };
        assert_eq!(w.frames(), 4);
        assert_eq!(w.max_in(0, 0.0, 0.4, 100), 100);
        assert_eq!(w.max_in(1, 0.15, 0.4, 100), 40);
        assert_eq!(w.max_in(5, 0.0, 1.0, 10), 0);
        let back = Waveform::from_bytes(&w.to_bytes()).unwrap();
        assert_eq!(back, w);
        assert!(Waveform::from_bytes(b"nope").is_err());
        let mut truncated = w.to_bytes();
        truncated.truncate(18);
        assert!(Waveform::from_bytes(&truncated).is_err());
        let mut a = w.clone();
        a.append(&w);
        assert_eq!(a.frames(), 8);
    }

    #[test]
    fn peak_meter() {
        let mut m = PeakMeter::new(100);
        let mut out = Vec::new();
        let mut pcm = vec![0.0f32; 25 * 2];
        pcm[3] = 0.5;
        m.push(&pcm, 2, &mut out);
        assert_eq!(out.len(), 2, "10 frames per peak at 100 Hz");
        assert!(out[0] > 200);
        assert_eq!(out[1], 0);
        m.silence(15, &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn wave_paths_are_stable_and_case_insensitive() {
        assert_eq!(wave_path(Path::new(r"I:\OBS\a.mp4")), wave_path(Path::new(r"i:\obs\A.MP4")));
        assert_ne!(wave_path(Path::new(r"I:\OBS\a.mp4")), wave_path(Path::new(r"I:\OBS\b.mp4")));
    }
}
