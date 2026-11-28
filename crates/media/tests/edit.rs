//! Record a few seconds (split into several files), then probe, cut across
//! files, and make a thumbnail, checking the results.

use cv_media::cut::{cut, CutSource};
use cv_media::probe::{probe, TrackRole};
use cv_media::recorder::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

struct Namer;
impl AppNamer for Namer {
    fn identify(&self, exe: &str, _pid: u32) -> AppIdent {
        AppIdent { key: exe.to_string(), name: format!("App {exe}"), category: "media".into() }
    }
}

fn tone_wav(path: &std::path::Path, secs: f32) {
    let n = (48_000.0 * secs) as usize;
    let data_len = n * 4;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&192_000u32.to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data_len as u32).to_le_bytes());
    for i in 0..n {
        let v = (0.05 * (i as f32 * 660.0 * std::f32::consts::TAU / 48_000.0).sin() * 32767.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

fn ffprobe(path: &std::path::Path) -> Option<String> {
    let exe = std::path::PathBuf::from(r"C:\ffmpeg-7.1-essentials_build\bin\ffprobe.exe");
    if !exe.is_file() {
        return None;
    }
    let out = std::process::Command::new(exe)
        .args(["-v", "error", "-count_frames", "-show_entries", "format=duration:stream=codec_name,nb_read_frames:stream_tags=handler_name", "-of", "compact"])
        .arg(path)
        .output()
        .unwrap();
    Some(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn record_probe_cut_thumbnail() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    tone_wav(&wav, 2.5);
    // 2-second segments so a cut has to span files.
    let cfg = RecorderConfig { folder: dir.path().join("rec"), fps: 30, max_height: 540, app_slots: 3, segment_secs: 2, keyframe_secs: 1.0, ..Default::default() };
    let (etx, erx) = crossbeam_channel::unbounded();
    let rec = Recorder::start(cfg, Arc::new(Namer), etx).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", wav.display())])
        .spawn()
        .unwrap();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(2500));
    rec.stop();
    let mut segs: Vec<(std::path::PathBuf, i64, f64)> = erx
        .try_iter()
        .filter_map(|e| match e {
            RecEvent::SegmentFinished { path, start_ms, duration_s, .. } => Some((path, start_ms, duration_s)),
            _ => None,
        })
        .collect();
    segs.sort_by_key(|s| s.1);
    eprintln!("segments: {segs:?}");
    assert!(segs.len() >= 3, "expected several segments");
    // Probing a recording.
    let info = probe(&segs[1].0).unwrap();
    eprintln!("probe: {}x{} {:.2} fps {:.2} s, audio {:?}", info.width, info.height, info.fps, info.duration, info.audio.iter().map(|a| (&a.name, &a.role)).collect::<Vec<_>>());
    assert!((info.fps - 30.0).abs() < 1.0);
    assert_eq!(info.audio[0].role, TrackRole::Full);
    assert_eq!(info.audio[1].role, TrackRole::Mic);
    assert!(matches!(info.audio[2].role, TrackRole::Slot { slot: 0 }));
    // Cut the whole thing (all files) into one clip.
    let sources: Vec<CutSource> = segs.iter().map(|(p, ..)| CutSource { path: p.clone(), from_s: 0.0, to_s: None }).collect();
    let total: f64 = segs.iter().map(|s| s.2).sum();
    let clip = dir.path().join("all.mp4");
    let out = cut(&sources, &clip, Some(segs[0].1), &mut |_| {}, &AtomicBool::new(false)).unwrap();
    eprintln!("whole clip: {:.2} s (recorded {:.2}), tracks {:?}", out.duration, total, out.audio.iter().map(|a| &a.name).collect::<Vec<_>>());
    assert!((out.duration - total).abs() < 0.2);
    assert!(out.audio.iter().any(|a| a.name == "App powershell.exe"), "app track by name");
    let cinfo = probe(&clip).unwrap();
    assert!((cinfo.duration - total).abs() < 0.3, "{} vs {}", cinfo.duration, total);
    assert!(cinfo.audio.iter().any(|a| matches!(&a.role, TrackRole::App { app } if app.key == "powershell.exe")));
    if let Some(r) = ffprobe(&clip) {
        eprintln!("ffprobe whole: {r}");
        assert!(!r.to_lowercase().contains("error"), "{r}");
    }
    // A cut that starts mid-file (needs the edit list) and spans a boundary.
    let mid = dir.path().join("mid.mp4");
    let s1 = &segs[0];
    let part = vec![CutSource { path: s1.0.clone(), from_s: s1.2 - 0.7, to_s: None }, CutSource { path: segs[1].0.clone(), from_s: 0.0, to_s: Some(1.1) }];
    let out = cut(&part, &mid, None, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let mi = probe(&mid).unwrap();
    eprintln!("mid clip: {:.3} s (expected {:.3}), edit start {:.3}", mi.duration, out.duration, mi.edit_start);
    assert!((out.duration - 1.8).abs() < 0.05);
    assert!((mi.duration - 1.8).abs() < 0.1, "{}", mi.duration);
    if let Some(r) = ffprobe(&mid) {
        eprintln!("ffprobe mid: {r}");
        assert!(!r.to_lowercase().contains("error"), "{r}");
    }
    // Thumbnail.
    let jpg = dir.path().join("t.jpg");
    cv_media::thumb::thumbnail(&clip, 1.0, &jpg, 320).unwrap();
    let img = image::open(&jpg).unwrap();
    assert_eq!(img.width(), 320);
    let _ = std::fs::copy(&jpg, std::env::temp_dir().join("cv_thumb_test.jpg"));

    // Waveform of the clip: the app track is loud only while the tone played.
    let wave = cv_media::wave::generate(&clip, 0.0, None, &AtomicBool::new(false)).unwrap();
    let app_i = cinfo.audio.iter().position(|a| matches!(a.role, TrackRole::App { .. })).unwrap();
    let loud = (0..wave.frames()).filter(|f| wave.get(*f, app_i) > 150).count();
    eprintln!("waveform: {} frames x {} tracks; app loud for {} frames", wave.frames(), wave.tracks, loud);
    assert!((wave.duration_s() - cinfo.duration).abs() < 0.5);
    assert!((18..=30).contains(&loud), "tone ~2.5 s = 25 frames, got {loud}");

    // Export, keeping the video and mixing only the app track, muted for 1 s.
    use cv_core::mixer::GainCurve;
    let mut gains: Vec<GainCurve> = cinfo.audio.iter().map(|_| GainCurve::constant(0.0)).collect();
    let tone_start = (0..wave.frames()).find(|f| wave.get(*f, app_i) > 150).unwrap() as f64 / 10.0;
    gains[app_i] = GainCurve { base: 1.0, edits: vec![(tone_start + 0.5, tone_start + 1.5, 0.0)], fade: 0.03 };
    let job = cv_media::export::ExportJob { source: clip.clone(), range: None, gains, video: cv_media::export::VideoOut::Copy, audio_kbps: 128, start_ms: None };
    let ex = dir.path().join("export.mp4");
    cv_media::export::export(&job, &ex, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let ei = probe(&ex).unwrap();
    assert_eq!(ei.audio.len(), 1);
    assert!((ei.duration - cinfo.duration).abs() < 0.2, "{} vs {}", ei.duration, cinfo.duration);
    let ew = cv_media::wave::generate(&ex, 0.0, None, &AtomicBool::new(false)).unwrap();
    let at = |t: f64| ew.get((t * 10.0) as usize, 0);
    eprintln!("export levels: before {} muted {} after {}", at(tone_start + 0.3), at(tone_start + 1.0), at(tone_start + 1.8));
    assert!(at(tone_start + 0.3) > 150 && at(tone_start + 1.8) > 150, "tone audible outside the edit");
    assert!(at(tone_start + 1.0) < 40, "muted inside the edit");

    // Size-limited re-encode of 2 s into ~0.5 MB.
    let job = cv_media::export::ExportJob { source: clip.clone(), range: Some((1.0, 3.0)), gains: vec![GainCurve::constant(1.0)], video: cv_media::export::VideoOut::Reencode { kbps: 1500, max_height: 360 }, audio_kbps: 96, start_ms: None };
    let small = dir.path().join("small.mp4");
    let size = cv_media::export::export(&job, &small, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let si = probe(&small).unwrap();
    eprintln!("re-encoded: {} bytes, {}x{}, {:.2} s, {:.1} fps", size, si.width, si.height, si.duration, si.fps);
    assert!(si.height <= 360);
    assert!((si.duration - 2.0).abs() < 0.15, "{}", si.duration);
    assert!(size < 700_000, "{size}");
    if let Some(r) = ffprobe(&small) {
        eprintln!("ffprobe small: {r}");
        assert!(!r.to_lowercase().contains("error"), "{r}");
    }
}
