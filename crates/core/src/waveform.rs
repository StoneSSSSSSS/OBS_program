//! Audio waveforms for the timeline lanes.
//!
//! One low-priority ffmpeg pass per media file turns every audio track into a
//! loudness envelope (10 values per second per track, 0-255 on a dB scale),
//! stored in `%LOCALAPPDATA%\ClipVault\waves\<hash>.cvw`. The file being
//! recorded is extended incrementally.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

pub const PEAKS_PER_SEC: u32 = 10;
/// Envelope sample rate produced by ffmpeg (decimated mean |x|).
const ENVELOPE_HZ: u32 = 200;
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

/// ffmpeg arguments producing interleaved s16 envelopes for `tracks` audio
/// tracks on stdout, starting `from_s` seconds into the file.
/// `readrate` (x realtime) throttles disk reads for background backfill.
pub fn ffmpeg_args(input: &Path, tracks: usize, from_s: f64, readrate: Option<f64>) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into()];
    if let Some(r) = readrate {
        a.push("-readrate".into());
        a.push(format!("{r:.0}"));
    }
    if from_s > 0.0 {
        a.push("-ss".into());
        a.push(format!("{from_s:.3}"));
    }
    a.push("-i".into());
    a.push(input.to_string_lossy().to_string());
    let n = tracks.max(1);
    let abs: Vec<String> = (0..n).map(|i| format!("abs(val({i}))")).collect();
    let graph = if n == 1 {
        format!("[0:a:0]aformat=sample_fmts=fltp:channel_layouts=mono,aeval={}:c=same,aresample={ENVELOPE_HZ}[w]", abs[0])
    } else {
        let mut g = String::new();
        for i in 0..n {
            g.push_str(&format!("[0:a:{i}]aformat=sample_fmts=fltp:channel_layouts=mono[m{i}];"));
        }
        for i in 0..n {
            g.push_str(&format!("[m{i}]"));
        }
        g.push_str(&format!("amerge=inputs={n},aeval={}:c=same,aresample={ENVELOPE_HZ}[w]", abs.join("|")));
        g
    };
    a.extend(["-vn", "-sn", "-dn", "-filter_complex"].iter().map(|s| s.to_string()));
    a.push(graph);
    a.extend(["-map", "[w]", "-f", "s16le", "pipe:1"].iter().map(|s| s.to_string()));
    a
}

/// Turn the interleaved s16 envelope into 10/s peaks.
pub fn peaks_from_envelope(samples: &[i16], tracks: usize) -> Waveform {
    let per = (ENVELOPE_HZ / PEAKS_PER_SEC) as usize;
    let frames = samples.len() / tracks.max(1);
    let out_frames = frames / per;
    let mut peaks = Vec::with_capacity(out_frames * tracks);
    for f in 0..out_frames {
        for t in 0..tracks {
            let mut m = 0i32;
            for k in 0..per {
                let v = (samples[(f * per + k) * tracks + t] as i32).abs();
                m = m.max(v);
            }
            peaks.push(level_u8(m as f32 / 32768.0));
        }
    }
    Waveform { tracks, rate: PEAKS_PER_SEC, peaks }
}

/// Run ffmpeg and build the waveform. Runs at below-normal priority so it
/// never competes with a game.
pub fn generate(ffmpeg: &Path, input: &Path, tracks: usize, from_s: f64, readrate: Option<f64>, cancel: &AtomicBool) -> Result<Waveform> {
    let tracks = tracks.max(1);
    let mut cmd = Command::new(ffmpeg);
    cmd.args(ffmpeg_args(input, tracks, from_s, readrate)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    let mut child = cmd.spawn().context("starting ffmpeg")?;
    let stderr = child.stderr.take();
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let mut out = child.stdout.take().context("ffmpeg stdout")?;
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        let n = out.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let status = child.wait()?;
    let err = err_t.join().unwrap_or_default();
    if !status.success() {
        bail!("ffmpeg waveform failed: {}", err.lines().last().unwrap_or(""));
    }
    let samples: Vec<i16> = bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    Ok(peaks_from_envelope(&samples, tracks))
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
    fn envelope_to_peaks() {
        // 2 tracks, 40 envelope samples (= 2 output frames).
        let mut s = vec![0i16; 80];
        s[2 * 5 + 1] = 16384; // track 1 loud in frame 0
        let w = peaks_from_envelope(&s, 2);
        assert_eq!(w.frames(), 2);
        assert_eq!(w.get(0, 0), 0);
        assert!(w.get(0, 1) > 200);
        assert_eq!(w.get(1, 1), 0);
    }

    #[test]
    fn wave_paths_are_stable_and_case_insensitive() {
        assert_eq!(wave_path(Path::new(r"I:\OBS\a.mp4")), wave_path(Path::new(r"i:\obs\A.MP4")));
        assert_ne!(wave_path(Path::new(r"I:\OBS\a.mp4")), wave_path(Path::new(r"I:\OBS\b.mp4")));
    }

    #[test]
    fn real_ffmpeg_waveform() {
        let Ok(tools) = crate::ffmpeg::locate("", "") else {
            eprintln!("ffmpeg not found; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("w.mp4");
        // Track 0 silent, track 1 a loud tone for the first 2 s then silence.
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y",
                  "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo",
                  "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2,apad",
                  "-t", "4", "-map", "0:a", "-map", "1:a", "-c:a", "aac", "-ac", "2"])
            .arg(&src);
        crate::util::hide_console(&mut cmd);
        assert!(cmd.status().unwrap().success());
        let w = generate(&tools.ffmpeg, &src, 2, 0.0, Some(200.0), &AtomicBool::new(false)).unwrap();
        assert!((w.duration_s() - 4.0).abs() < 0.3, "{}", w.duration_s());
        assert_eq!(w.max_in(0, 0.0, 4.0, 1000), 0, "silent track stays flat");
        assert!(w.max_in(1, 0.2, 1.8, 1000) > 150, "tone is loud");
        assert!(w.max_in(1, 2.6, 3.8, 1000) < 40, "silence after the tone");
        // Incremental from 2 s.
        let tail = generate(&tools.ffmpeg, &src, 2, 2.0, None, &AtomicBool::new(false)).unwrap();
        assert!((tail.duration_s() - 2.0).abs() < 0.3);
    }
}
