//! Record a few seconds (split into several files), then probe, cut across
//! files, and make a thumbnail, checking the results.

mod common;
use common::{write_tone, TonePlayer, SIGNAL_FLOOR};
use cv_core::waveform::level_u8;
use cv_media::cut::{cut, CutSource};
use cv_media::probe::{probe, TrackRole};
use cv_media::recorder::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

const SILENCE_CEILING: f32 = 0.005;

struct Namer;
impl AppNamer for Namer {
    fn identify(&self, exe: &str, _pid: u32) -> AppIdent {
        AppIdent { key: exe.to_string(), name: format!("App {exe}"), category: "media".into() }
    }
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
#[ignore = "requires an interactive Windows display and render audio endpoint"]
fn record_probe_cut_thumbnail() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    cv_media::util::disable_background_throttling();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLIPVAULT_DATA_DIR", dir.path().join("data"));
    let wav = dir.path().join("tone.wav");
    write_tone(&wav, 2.5);
    // 2-second segments so a cut has to span files.
    let cfg = RecorderConfig { folder: dir.path().join("rec"), mic: None, fps: 30, max_height: 540, app_slots: 3, segment_secs: 2, keyframe_secs: 1.0, ..Default::default() };
    let (etx, erx) = crossbeam_channel::unbounded();
    let rec = Recorder::start(cfg, Arc::new(Namer), etx).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    TonePlayer::start(&wav).finish();
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
    // Validate the fragmented source independently of the cutter and use
    // slot history so activity in an unrelated app cannot satisfy this check.
    let audible = level_u8(SIGNAL_FLOOR);
    let mut raw_peak = 0;
    let mut raw_loud = 0;
    for (path, _, _) in &segs {
        let info = probe(path).unwrap();
        let wave = cv_media::wave::generate(path, 0.0, None, &AtomicBool::new(false)).unwrap();
        for (track, audio) in info.audio.iter().enumerate() {
            let TrackRole::Slot { slot } = audio.role else { continue };
            for frame in 0..wave.frames() {
                let time = (frame as f64 + 0.5) / wave.rate as f64;
                let app = cv_media::probe::slot_app(&info.slot_history, slot, time);
                if app.is_some_and(|app| app.key == "powershell.exe") {
                    let level = wave.get(frame, track);
                    raw_peak = raw_peak.max(level);
                    raw_loud += usize::from(level >= audible);
                }
            }
        }
    }
    eprintln!("recorded app: maximum peak {raw_peak}/255, {raw_loud} audible windows");
    assert!(raw_loud >= 18, "the fragmented recording has insufficient app signal: peak {raw_peak}/255, threshold {audible}/255, audible windows {raw_loud}");
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

    // Waveform of the clip: the app track is loud only while the tone played.
    let wave = cv_media::wave::generate(&clip, 0.0, None, &AtomicBool::new(false)).unwrap();
    let app_i = cinfo.audio.iter().position(|a| matches!(&a.role, TrackRole::App { app } if app.key=="powershell.exe")).unwrap();
    let peak = (0..wave.frames()).map(|f| wave.get(f, app_i)).max().unwrap_or(0);
    let loud = (0..wave.frames()).filter(|f| wave.get(*f, app_i) >= audible).count();
    eprintln!("cut waveform: {} frames x {} tracks; app peak {peak}/255, {loud} audible windows", wave.frames(), wave.tracks);
    assert!((wave.duration_s() - cinfo.duration).abs() < 0.5);
    assert!((18..=30).contains(&loud), "tone ~2.5 s = 25 frames, got {loud}");

    // Export, keeping the video and mixing only the app track, muted for 1 s.
    use cv_core::mixer::GainCurve;
    let mut gains: Vec<GainCurve> = cinfo.audio.iter().map(|_| GainCurve::constant(0.0)).collect();
    let tone_start = (0..wave.frames()).find(|f| wave.get(*f, app_i) >= audible).unwrap() as f64 / 10.0;
    gains[app_i] = GainCurve { base: 1.0, edits: vec![(tone_start + 0.5, tone_start + 1.5, 0.0)], fade: 0.03, slots: Vec::new() };
    let job = cv_media::export::ExportJob { source: clip.clone(), range: None, gains, video: cv_media::export::VideoOut::Copy, audio_kbps: 128, start_ms: None };
    let ex = dir.path().join("export.mp4");
    cv_media::export::export(&job, &ex, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let ei = probe(&ex).unwrap();
    assert_eq!(ei.audio.len(), 1);
    assert!((ei.duration - cinfo.duration).abs() < 0.2, "{} vs {}", ei.duration, cinfo.duration);
    let ew = cv_media::wave::generate(&ex, 0.0, None, &AtomicBool::new(false)).unwrap();
    let at = |t: f64| ew.get((t * 10.0) as usize, 0);
    eprintln!("export levels: before {} muted {} after {}", at(tone_start + 0.3), at(tone_start + 1.0), at(tone_start + 1.8));
    assert!(at(tone_start + 0.3) >= audible && at(tone_start + 1.8) >= audible, "tone audible outside the edit");
    assert!(at(tone_start + 1.0) <= level_u8(SILENCE_CEILING), "muted inside the edit");

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
