//! ffmpeg/ffprobe: locating binaries, probing, thumbnails, cutting, exporting.

use crate::timeline::CutPart;
use crate::util::hide_console;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub version: String,
}

/// Parse `ffmpeg version X ...` and decide whether it's recent enough.
/// Accepts release builds >= 5 and git builds `N-xxxxx` from 2022 onwards.
pub fn version_ok(first_line: &str) -> bool {
    let v = first_line
        .split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)
        .unwrap_or("");
    if let Some(n) = v.strip_prefix("N-") {
        return n.split('-').next().and_then(|n| n.parse::<u64>().ok()).map(|n| n >= 105_000).unwrap_or(false);
    }
    let v = v.trim_start_matches('n');
    v.split(['.', '-']).next().and_then(|m| m.parse::<u32>().ok()).map(|m| m >= 5).unwrap_or(false)
}

fn probe_version(exe: &Path) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.arg("-hide_banner").arg("-version").stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
    hide_console(&mut cmd);
    let out = run_with_timeout(cmd, Duration::from_secs(10)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let first = text.lines().next()?.to_string();
    version_ok(&first).then_some(first)
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::paths::exe_dir(), crate::paths::exe_dir().join("bin")];
    for root in [r"C:\Program Files", r"C:\Program Files (x86)", r"C:\ffmpeg"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut found: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase().starts_with("ffmpeg"))
                        .unwrap_or(false)
                })
                .collect();
            found.sort();
            found.reverse(); // newest version folder first
            for f in found {
                dirs.push(f.join("bin"));
                dirs.push(f);
            }
        }
        dirs.push(PathBuf::from(root).join("bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs
}

/// Find a working ffmpeg + ffprobe pair. Explicit paths win if they work.
pub fn locate(ffmpeg_cfg: &str, ffprobe_cfg: &str) -> Result<Tools> {
    let mut tried = Vec::new();
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    if !ffmpeg_cfg.trim().is_empty() {
        let f = PathBuf::from(ffmpeg_cfg.trim());
        let p = if ffprobe_cfg.trim().is_empty() {
            f.with_file_name("ffprobe.exe")
        } else {
            PathBuf::from(ffprobe_cfg.trim())
        };
        pairs.push((f, p));
    }
    for d in candidate_dirs() {
        pairs.push((d.join("ffmpeg.exe"), d.join("ffprobe.exe")));
    }
    for (f, p) in pairs {
        if !f.is_file() || !p.is_file() {
            continue;
        }
        tried.push(f.display().to_string());
        if let Some(version) = probe_version(&f) {
            if probe_version(&p).is_some() {
                return Ok(Tools { ffmpeg: f, ffprobe: p, version });
            }
        }
    }
    bail!(
        "no usable ffmpeg 5+ with ffprobe found (checked {}). Set the path in Settings > Advanced.",
        if tried.is_empty() { "nothing".to_string() } else { tried.join(", ") }
    )
}

/// Run a command, killing it if it exceeds `timeout`.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<std::process::Output> {
    let mut child = cmd.spawn().context("starting process")?;
    let start = Instant::now();
    // Drain pipes on threads so a chatty process can't deadlock on a full pipe.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut s) = stdout {
            let _ = std::io::Read::read_to_end(&mut s, &mut v);
        }
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut s) = stderr {
            let _ = std::io::Read::read_to_end(&mut s, &mut v);
        }
        v
    });
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("process timed out after {:?}", timeout);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output { status, stdout: out_t.join().unwrap_or_default(), stderr: err_t.join().unwrap_or_default() })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub index: usize,
    pub title: String,
    pub channels: u32,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub duration: f64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio: Vec<AudioTrack>,
}

fn parse_rate(s: &str) -> f64 {
    match s.split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.parse().unwrap_or(0.0);
            let d: f64 = d.parse().unwrap_or(0.0);
            if d > 0.0 {
                n / d
            } else {
                0.0
            }
        }
        None => s.parse().unwrap_or(0.0),
    }
}

/// Parse ffprobe `-show_format -show_streams` JSON.
pub fn parse_probe_json(v: &Value) -> ProbeInfo {
    let mut info = ProbeInfo::default();
    let f = &v["format"];
    info.duration = f["duration"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    info.size = f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut audio_idx = 0;
    if let Some(streams) = v["streams"].as_array() {
        for s in streams {
            match s["codec_type"].as_str() {
                Some("video") if info.width == 0 => {
                    // Skip attached cover art.
                    if s["disposition"]["attached_pic"].as_i64() == Some(1) {
                        continue;
                    }
                    info.width = s["width"].as_u64().unwrap_or(0) as u32;
                    info.height = s["height"].as_u64().unwrap_or(0) as u32;
                    info.video_codec = s["codec_name"].as_str().unwrap_or("").to_string();
                    let avg = parse_rate(s["avg_frame_rate"].as_str().unwrap_or("0/0"));
                    let r = parse_rate(s["r_frame_rate"].as_str().unwrap_or("0/0"));
                    info.fps = if avg > 0.0 && avg < 1000.0 { avg } else { r };
                    if info.duration <= 0.0 {
                        info.duration = s["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
                    }
                }
                Some("audio") => {
                    let tags = &s["tags"];
                    let mut title = tags["title"].as_str().unwrap_or("").to_string();
                    if title.is_empty() {
                        let h = tags["handler_name"].as_str().unwrap_or("");
                        if !h.is_empty() && !h.eq_ignore_ascii_case("SoundHandler") && !h.contains("Handler") {
                            title = h.to_string();
                        }
                    }
                    info.audio.push(AudioTrack {
                        index: audio_idx,
                        title,
                        channels: s["channels"].as_u64().unwrap_or(0) as u32,
                        sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()).unwrap_or(0),
                    });
                    audio_idx += 1;
                }
                _ => {}
            }
        }
    }
    info
}

impl Tools {
    pub fn probe(&self, path: &Path) -> Result<ProbeInfo> {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut cmd);
        let out = run_with_timeout(cmd, Duration::from_secs(60))?;
        if !out.status.success() {
            bail!("ffprobe failed on {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        let v: Value = serde_json::from_slice(&out.stdout).context("ffprobe output")?;
        let mut info = parse_probe_json(&v);
        if info.size == 0 {
            info.size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
        Ok(info)
    }

    /// Grab one frame at `at_s` as a JPEG `width` pixels wide.
    pub fn thumbnail(&self, input: &Path, at_s: f64, out: &Path, width: u32) -> Result<()> {
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = out.with_extension("tmp.jpg");
        let attempt = |t: f64| -> Result<bool> {
            let mut cmd = Command::new(&self.ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-ss"])
                .arg(format!("{t:.3}"))
                .arg("-i")
                .arg(input)
                .args(["-frames:v", "1", "-an", "-vf"])
                .arg(format!("scale={width}:-2:flags=bicubic"))
                .args(["-q:v", "4", "-f", "image2"])
                .arg(&tmp)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            hide_console(&mut cmd);
            let out = run_with_timeout(cmd, Duration::from_secs(60))?;
            Ok(out.status.success() && tmp.metadata().map(|m| m.len() > 0).unwrap_or(false))
        };
        if attempt(at_s.max(0.0))? || attempt(0.0)? {
            std::fs::rename(&tmp, out)?;
            Ok(())
        } else {
            let _ = std::fs::remove_file(&tmp);
            bail!("could not extract a frame from {}", input.display())
        }
    }

    /// Run ffmpeg with `-progress`, reporting 0..1 and honouring `cancel`.
    pub fn run_progress(
        &self,
        args: &[String],
        total_s: f64,
        progress: &mut dyn FnMut(f32),
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_console(&mut cmd);
        tracing::debug!("ffmpeg {}", args.join(" "));
        let mut child = cmd.spawn().context("starting ffmpeg")?;
        let stderr = child.stderr.take();
        let err_t = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = std::io::Read::read_to_string(&mut e, &mut s);
            }
            s
        });
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let (line_tx, line_rx) = crossbeam_channel::unbounded::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("cancelled");
            }
            match line_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    if let Some(us) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
                        if let Ok(us) = us.trim().parse::<i64>() {
                            if total_s > 0.0 {
                                progress(((us as f64 / 1e6) / total_s).clamp(0.0, 1.0) as f32);
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if child.try_wait()?.is_some() {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = child.wait()?;
        let err = err_t.join().unwrap_or_default();
        if !status.success() {
            bail!("ffmpeg failed: {}", last_lines(&err, 4));
        }
        progress(1.0);
        Ok(())
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

fn secs(v: f64) -> String {
    format!("{:.3}", v.max(0.0))
}

/// Quote a path for an ffconcat list.
pub fn concat_quote(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn concat_list(parts: &[CutPart]) -> String {
    let mut s = String::from("ffconcat version 1.0\n");
    for p in parts {
        s.push_str(&format!("file {}\n", concat_quote(&p.path)));
        if p.in_s > 0.0 {
            s.push_str(&format!("inpoint {}\n", secs(p.in_s)));
        }
        if let Some(o) = p.out_s {
            s.push_str(&format!("outpoint {}\n", secs(o)));
        }
    }
    s
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodeOptions {
    /// "h264_nvenc", "hevc_nvenc", "libx264".
    pub video_encoder: String,
    /// CQ/CRF.
    pub quality: u32,
    pub audio_bitrate_kbps: u32,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self { video_encoder: "h264_nvenc".into(), quality: 23, audio_bitrate_kbps: 192 }
    }
}

fn video_quality_args(o: &EncodeOptions) -> Vec<String> {
    let mut a: Vec<String> = vec!["-c:v".into(), o.video_encoder.clone()];
    if o.video_encoder.contains("nvenc") {
        a.extend(["-preset", "p5", "-rc", "vbr", "-cq"].iter().map(|s| s.to_string()));
        a.push(o.quality.to_string());
        a.extend(["-b:v", "0"].iter().map(|s| s.to_string()));
    } else {
        a.extend(["-preset", "veryfast", "-crf"].iter().map(|s| s.to_string()));
        a.push(o.quality.to_string());
    }
    a.extend(["-pix_fmt", "yuv420p"].iter().map(|s| s.to_string()));
    a
}

/// Build ffmpeg args for cutting `parts` into `out`.
///
/// * fast (`precise == false`): stream copy, starts at the keyframe before the
///   in-point (an MP4 edit list hides the extra frames in most players).
/// * precise: re-encodes video, frame exact. Audio tracks are re-encoded too.
pub fn cut_args(parts: &[CutPart], out: &Path, list_file: &Path, precise: bool, audio_tracks: usize, enc: &EncodeOptions) -> Result<Vec<String>> {
    if parts.is_empty() {
        bail!("nothing to cut");
    }
    let mut a: Vec<String> = Vec::new();
    if parts.len() == 1 || !precise {
        if parts.len() == 1 {
            let p = &parts[0];
            a.push("-ss".into());
            a.push(secs(p.in_s));
            if let Some(o) = p.out_s {
                a.push("-t".into());
                a.push(secs(o - p.in_s));
            }
            a.push("-i".into());
            a.push(p.path.to_string_lossy().to_string());
        } else {
            a.extend(["-f", "concat", "-safe", "0", "-i"].iter().map(|s| s.to_string()));
            a.push(list_file.to_string_lossy().to_string());
        }
        a.extend(["-map", "0:v:0", "-map", "0:a?"].iter().map(|s| s.to_string()));
        if precise {
            a.extend(video_quality_args(enc));
            a.extend(["-c:a", "aac", "-b:a"].iter().map(|s| s.to_string()));
            a.push(format!("{}k", enc.audio_bitrate_kbps));
        } else {
            a.extend(["-c", "copy"].iter().map(|s| s.to_string()));
        }
    } else {
        // Precise multi-file: trim the first/last inputs and join with the
        // concat filter, carrying every audio track through.
        let n = parts.len();
        for p in parts {
            if p.in_s > 0.0 {
                a.push("-ss".into());
                a.push(secs(p.in_s));
            }
            if let Some(o) = p.out_s {
                a.push("-t".into());
                a.push(secs(o - p.in_s));
            }
            a.push("-i".into());
            a.push(p.path.to_string_lossy().to_string());
        }
        let mut f = String::new();
        for i in 0..n {
            f.push_str(&format!("[{i}:v:0]"));
            for t in 0..audio_tracks {
                f.push_str(&format!("[{i}:a:{t}]"));
            }
        }
        f.push_str(&format!("concat=n={n}:v=1:a={audio_tracks}[v]"));
        for t in 0..audio_tracks {
            f.push_str(&format!("[a{t}]"));
        }
        a.push("-filter_complex".into());
        a.push(f);
        a.push("-map".into());
        a.push("[v]".into());
        for t in 0..audio_tracks {
            a.push("-map".into());
            a.push(format!("[a{t}]"));
        }
        a.extend(video_quality_args(enc));
        if audio_tracks > 0 {
            a.extend(["-c:a", "aac", "-b:a"].iter().map(|s| s.to_string()));
            a.push(format!("{}k", enc.audio_bitrate_kbps));
        }
    }
    a.extend(["-movflags", "+faststart", "-f", "mp4"].iter().map(|s| s.to_string()));
    a.push(out.to_string_lossy().to_string());
    Ok(a)
}

/// Settings for exporting a shareable file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExportSpec {
    /// (audio track index, gain). Empty = keep all tracks untouched.
    pub gains: Vec<(usize, f32)>,
    /// Optional (start, end) seconds within the input.
    pub range: Option<(f64, f64)>,
    /// Re-encode to fit this many MB (None = keep video as is).
    pub target_size_mb: Option<u32>,
    pub max_height: u32,
    /// Re-encode video even without a size target (frame-exact range).
    pub reencode_video: bool,
}

/// Video bitrate (kbps) that fits `target_mb` for `duration_s`, or None if impossible.
pub fn bitrate_for_size(target_mb: u32, duration_s: f64, audio_kbps: u32) -> Option<u32> {
    if duration_s <= 0.0 {
        return None;
    }
    let total_kbits = target_mb as f64 * 8.0 * 1024.0 * 0.94; // container overhead margin
    let video = total_kbits / duration_s - audio_kbps as f64;
    (video >= 150.0).then(|| video.floor() as u32)
}

/// Height to scale to for a given bitrate so quality stays reasonable.
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

pub fn export_args(input: &Path, out: &Path, spec: &ExportSpec, enc: &EncodeOptions, info: &ProbeInfo, video_kbps: Option<u32>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let reencode = video_kbps.is_some() || spec.reencode_video;
    if let Some((s, e)) = spec.range {
        a.push("-ss".into());
        a.push(secs(s));
        a.push("-t".into());
        a.push(secs(e - s));
    }
    a.push("-i".into());
    a.push(input.to_string_lossy().to_string());
    let mix = if spec.gains.is_empty() { None } else { crate::mixer::ffmpeg_mix_filter(&spec.gains) };
    let mut filters: Vec<String> = Vec::new();
    let target_h = video_kbps.map(|k| height_for_bitrate(k, info.height, spec.max_height)).unwrap_or(info.height);
    let scale = reencode && target_h > 0 && target_h < info.height;
    if scale {
        filters.push(format!("[0:v:0]scale=-2:{target_h}:flags=lanczos[vout]"));
    }
    if let Some(m) = &mix {
        filters.push(m.clone());
    }
    if !filters.is_empty() {
        a.push("-filter_complex".into());
        a.push(filters.join(";"));
    }
    a.push("-map".into());
    a.push(if scale { "[vout]".into() } else { "0:v:0".into() });
    match (&mix, spec.gains.is_empty()) {
        (Some(_), _) => {
            a.push("-map".into());
            a.push("[aout]".into());
        }
        (None, true) => {
            a.push("-map".into());
            a.push("0:a?".into());
        }
        (None, false) => {} // everything muted -> no audio
    }
    if let Some(k) = video_kbps {
        a.extend(["-c:v".to_string(), enc.video_encoder.clone()]);
        if enc.video_encoder.contains("nvenc") {
            a.extend(["-preset", "p6", "-rc", "vbr", "-multipass", "fullres"].iter().map(|s| s.to_string()));
        } else {
            a.extend(["-preset", "medium"].iter().map(|s| s.to_string()));
        }
        a.extend([
            "-b:v".to_string(),
            format!("{k}k"),
            "-maxrate".to_string(),
            format!("{}k", (k as f64 * 1.15) as u32),
            "-bufsize".to_string(),
            format!("{}k", k * 2),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ]);
    } else if spec.reencode_video {
        a.extend(video_quality_args(enc));
    } else {
        a.extend(["-c:v", "copy"].iter().map(|s| s.to_string()));
    }
    if mix.is_some() {
        a.extend(["-c:a".to_string(), "aac".to_string(), "-b:a".to_string(), format!("{}k", enc.audio_bitrate_kbps)]);
    } else if spec.gains.is_empty() {
        a.extend(["-c:a", "copy"].iter().map(|s| s.to_string()));
    }
    a.extend(["-movflags", "+faststart", "-f", "mp4"].iter().map(|s| s.to_string()));
    a.push(out.to_string_lossy().to_string());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_checks() {
        assert!(version_ok("ffmpeg version 6.0-essentials_build-www.gyan.dev Copyright (c) 2000-2023"));
        assert!(version_ok("ffmpeg version 7.1 Copyright"));
        assert!(version_ok("ffmpeg version n6.1.1 Copyright"));
        assert!(!version_ok("ffmpeg version N-55702-g920046a"));
        assert!(version_ok("ffmpeg version N-118000-gabc Copyright"));
        assert!(!version_ok("ffmpeg version 4.4.2"));
        assert!(!version_ok("garbage"));
    }

    #[test]
    fn probe_json_parsing() {
        let v: Value = serde_json::from_str(
            r#"{"streams":[
                {"codec_type":"video","codec_name":"h264","width":2560,"height":1440,"avg_frame_rate":"60/1","r_frame_rate":"60/1"},
                {"codec_type":"audio","channels":2,"sample_rate":"48000","tags":{"handler_name":"SoundHandler"}},
                {"codec_type":"audio","channels":2,"sample_rate":"48000","tags":{"title":"Game"}}],
               "format":{"duration":"3600.000000","size":"13503844027"}}"#,
        )
        .unwrap();
        let p = parse_probe_json(&v);
        assert_eq!((p.width, p.height), (2560, 1440));
        assert!((p.fps - 60.0).abs() < 1e-9);
        assert_eq!(p.duration, 3600.0);
        assert_eq!(p.size, 13_503_844_027);
        assert_eq!(p.audio.len(), 2);
        assert_eq!(p.audio[0].title, "");
        assert_eq!(p.audio[1].title, "Game");
        assert_eq!(p.audio[1].index, 1);
    }

    #[test]
    fn concat_list_quoting() {
        let parts = vec![
            CutPart { path: PathBuf::from(r"I:\OBS\it's.mp4"), in_s: 12.5, out_s: None },
            CutPart { path: PathBuf::from(r"I:\OBS\b.mp4"), in_s: 0.0, out_s: Some(30.0) },
        ];
        let s = concat_list(&parts);
        assert_eq!(
            s,
            "ffconcat version 1.0\nfile 'I:/OBS/it'\\''s.mp4'\ninpoint 12.500\nfile 'I:/OBS/b.mp4'\noutpoint 30.000\n"
        );
    }

    #[test]
    fn cut_arg_shapes() {
        let one = vec![CutPart { path: "a.mp4".into(), in_s: 10.0, out_s: Some(40.0) }];
        let a = cut_args(&one, Path::new("o.mp4"), Path::new("l.txt"), false, 6, &EncodeOptions::default()).unwrap();
        assert_eq!(&a[..6], &["-ss", "10.000", "-t", "30.000", "-i", "a.mp4"]);
        assert!(a.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(a.windows(2).any(|w| w == ["-map", "0:a?"]));

        let two = vec![
            CutPart { path: "a.mp4".into(), in_s: 50.0, out_s: None },
            CutPart { path: "b.mp4".into(), in_s: 0.0, out_s: Some(10.0) },
        ];
        let fast = cut_args(&two, Path::new("o.mp4"), Path::new("l.txt"), false, 6, &EncodeOptions::default()).unwrap();
        assert!(fast.windows(2).any(|w| w == ["-f", "concat"]));
        let precise = cut_args(&two, Path::new("o.mp4"), Path::new("l.txt"), true, 2, &EncodeOptions::default()).unwrap();
        let fc = precise.iter().position(|s| s == "-filter_complex").unwrap();
        assert_eq!(precise[fc + 1], "[0:v:0][0:a:0][0:a:1][1:v:0][1:a:0][1:a:1]concat=n=2:v=1:a=2[v][a0][a1]");
        assert_eq!(&precise[..6], &["-ss", "50.000", "-i", "a.mp4", "-t", "10.000"]);
        assert!(cut_args(&[], Path::new("o"), Path::new("l"), false, 0, &EncodeOptions::default()).is_err());
    }

    #[test]
    fn size_targets() {
        // 30s into 10MB with 128k audio: ~2.4 Mbit/s video.
        let k = bitrate_for_size(10, 30.0, 128).unwrap();
        assert!((2300..2500).contains(&k), "{k}");
        assert_eq!(bitrate_for_size(1, 600.0, 128), None);
        assert_eq!(height_for_bitrate(k, 1440, 1080), 720);
        assert_eq!(height_for_bitrate(8000, 1440, 1080), 1080);
        assert_eq!(height_for_bitrate(8000, 1440, 0), 1440);
        assert_eq!(height_for_bitrate(900, 1080, 0), 480);
    }

    #[test]
    fn export_arg_shapes() {
        let info = ProbeInfo { width: 2560, height: 1440, duration: 30.0, ..Default::default() };
        let spec = ExportSpec { gains: vec![(1, 1.0), (2, 0.5)], range: Some((5.0, 15.0)), target_size_mb: Some(10), max_height: 1080, reencode_video: false };
        let a = export_args(Path::new("in.mp4"), Path::new("out.mp4"), &spec, &EncodeOptions::default(), &info, Some(2000));
        let fc = a.iter().position(|s| s == "-filter_complex").unwrap();
        assert!(a[fc + 1].starts_with("[0:v:0]scale=-2:720"));
        assert!(a[fc + 1].contains("amix=inputs=2"));
        assert!(a.windows(2).any(|w| w == ["-map", "[vout]"]));
        assert!(a.windows(2).any(|w| w == ["-map", "[aout]"]));
        assert!(a.windows(2).any(|w| w == ["-b:v", "2000k"]));

        let keep = ExportSpec::default();
        let b = export_args(Path::new("in.mp4"), Path::new("out.mp4"), &keep, &EncodeOptions::default(), &info, None);
        assert!(b.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(b.windows(2).any(|w| w == ["-c:a", "copy"]));
        assert!(!b.contains(&"-filter_complex".to_string()));
    }

    /// End-to-end against a real ffmpeg, if one is installed.
    #[test]
    fn real_ffmpeg_cut_probe_thumb_export() {
        let Ok(tools) = locate("", "") else {
            eprintln!("ffmpeg not found; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mp4");
        // 12s test video, 2 audio tracks, keyframe every 1s, fragmented like OBS.
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30",
                  "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                  "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000",
                  "-t", "12", "-map", "0:v", "-map", "1:a", "-map", "2:a",
                  "-c:v", "libx264", "-preset", "ultrafast", "-g", "30",
                  "-c:a", "aac", "-metadata:s:a:1", "handler_name=Game",
                  "-movflags", "frag_keyframe+empty_moov+delay_moov"])
            .arg(&src);
        hide_console(&mut cmd);
        let st = cmd.status().unwrap();
        assert!(st.success());
        let info = tools.probe(&src).unwrap();
        assert!((info.duration - 12.0).abs() < 0.2, "{}", info.duration);
        assert_eq!(info.audio.len(), 2);
        assert_eq!(info.audio[1].title, "Game");

        let thumb = dir.path().join("t.jpg");
        tools.thumbnail(&src, 3.0, &thumb, 160).unwrap();
        assert!(thumb.metadata().unwrap().len() > 100);

        // Fast cut, precise cut, multi-part concat.
        let cancel = AtomicBool::new(false);
        for (precise, parts, expect) in [
            (false, vec![CutPart { path: src.clone(), in_s: 2.0, out_s: Some(6.0) }], 4.0),
            (true, vec![CutPart { path: src.clone(), in_s: 2.5, out_s: Some(6.5) }], 4.0),
            (false, vec![CutPart { path: src.clone(), in_s: 8.0, out_s: None }, CutPart { path: src.clone(), in_s: 0.0, out_s: Some(2.0) }], 6.0),
            (true, vec![CutPart { path: src.clone(), in_s: 8.0, out_s: None }, CutPart { path: src.clone(), in_s: 0.0, out_s: Some(2.0) }], 6.0),
        ] {
            let out = dir.path().join(format!("cut_{precise}_{}.mp4", parts.len()));
            let list = dir.path().join("list.ffconcat");
            std::fs::write(&list, concat_list(&parts)).unwrap();
            let enc = EncodeOptions { video_encoder: "libx264".into(), quality: 30, audio_bitrate_kbps: 96 };
            let args = cut_args(&parts, &out, &list, precise, 2, &enc).unwrap();
            let mut last = 0.0f32;
            tools.run_progress(&args, 4.0, &mut |p| last = p, &cancel).unwrap();
            assert_eq!(last, 1.0);
            let ci = tools.probe(&out).unwrap();
            assert_eq!(ci.audio.len(), 2, "all audio tracks kept ({precise})");
            // Precise cuts are frame exact; fast cuts may start up to one keyframe early.
            let tol = if precise { 0.15 } else { 1.1 };
            assert!((ci.duration - expect).abs() < tol, "duration {} expected {expect} precise={precise}", ci.duration);
        }

        // Export with a custom mix to a size target.
        let out = dir.path().join("share.mp4");
        let spec = ExportSpec { gains: vec![(0, 1.0), (1, 0.3)], range: None, target_size_mb: Some(1), max_height: 0, reencode_video: false };
        let enc = EncodeOptions { video_encoder: "libx264".into(), quality: 30, audio_bitrate_kbps: 96 };
        let k = bitrate_for_size(1, info.duration, 96).unwrap();
        let args = export_args(&src, &out, &spec, &enc, &info, Some(k));
        tools.run_progress(&args, info.duration, &mut |_| {}, &cancel).unwrap();
        let ei = tools.probe(&out).unwrap();
        assert_eq!(ei.audio.len(), 1, "mixed down to one track");
        assert!(ei.size <= 1024 * 1024 + 64 * 1024, "size {}", ei.size);

        // Cancellation stops promptly.
        let cancel = AtomicBool::new(true);
        let r = tools.run_progress(&args, info.duration, &mut |_| {}, &cancel);
        assert!(r.is_err());
    }
}
