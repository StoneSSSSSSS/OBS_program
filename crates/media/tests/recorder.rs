//! End-to-end: record the real screen + a program's sound for a few seconds.

mod common;
use common::{write_tone, TonePlayer, SIGNAL_FLOOR};
use cv_core::waveform::level_u8;
use cv_media::mp4::reader::Mp4Reader;
use cv_media::recorder::*;
use std::sync::Arc;
use std::time::Duration;

struct Namer;
impl AppNamer for Namer {
    fn identify(&self, exe: &str, _pid: u32) -> AppIdent {
        AppIdent { key: exe.to_string(), name: exe.trim_end_matches(".exe").to_string(), category: "other".into() }
    }
}

#[test]
#[ignore = "requires an interactive Windows display and render audio endpoint"]
fn records_screen_and_app_sound() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    cv_media::util::disable_background_throttling();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLIPVAULT_DATA_DIR", dir.path().join("data"));
    let wav = dir.path().join("tone.wav");
    write_tone(&wav, 2.0);
    let cfg = RecorderConfig { folder: dir.path().join("rec"), mic: None, fps: 30, max_height: 720, app_slots: 4, ..Default::default() };
    let (etx, erx) = crossbeam_channel::unbounded();
    let rec = Recorder::start(cfg, Arc::new(Namer), etx).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let child = TonePlayer::start(&wav);
    std::thread::sleep(Duration::from_millis(2500));
    let flushed = rec.flush(Duration::from_secs(5)).expect("flush");
    let st = rec.status();
    eprintln!("status: {}x{} fps {:.1} via {} / {}; written until {} (flush {})", st.width, st.height, st.fps, st.capture_method, st.encoder, st.written_until_ms, flushed);
    assert!(st.video_error.is_none(), "{:?}", st.video_error);
    let now = chrono::Utc::now().timestamp_millis();
    assert!(now - flushed < 1500, "flush left {} ms unwritten", now - flushed);
    child.finish();
    std::thread::sleep(Duration::from_millis(1500));
    rec.stop();
    let events: Vec<RecEvent> = erx.try_iter().collect();
    for e in &events {
        eprintln!("event: {e:?}");
    }
    let path = events.iter().find_map(|e| match e { RecEvent::SegmentFinished { path, .. } => Some(path.clone()), _ => None }).expect("a finished segment");
    let r = Mp4Reader::open(&path).unwrap();
    let dur = r.duration_secs();
    eprintln!("{}: {:.2} s, {} tracks", path.display(), dur, r.tracks.len());
    assert!((5.0..7.5).contains(&dur), "duration {dur}");
    let v = r.samples(0, 0, u64::MAX).unwrap();
    // Frame durations add up exactly: one continuous video range, no gaps.
    assert_eq!(r.coverage(0).len(), 1, "video has gaps: {:?}", r.coverage(0));
    eprintln!("video frames: {}", v.len());
    assert!(v.len() as f64 > dur * 30.0 * 0.85, "too few frames: {}", v.len());
    let full = r.samples(1, 0, u64::MAX).unwrap();
    let full_s = full.len() as f64 * 1024.0 / 48_000.0;
    assert!((full_s - dur).abs() < 0.6, "full mix {full_s} vs video {dur}");
    // The tone's program got an app track with ~2 s of sound.
    let meta = parse_meta(&r.metas);
    let slots: Vec<&SlotInfo> = meta.iter().flat_map(|m| m.slots.iter()).collect();
    eprintln!("slot meta: {slots:?}");
    assert!(slots.iter().any(|s| s.app.key == "powershell.exe"), "no app track for the tone");
    let app_track = r.track_index(FIRST_SLOT_TRACK).unwrap();
    let cov = r.coverage(app_track);
    eprintln!("app track coverage: {cov:?}");
    assert!(!cov.is_empty());
    // Decode the app track and check the tone is there.
    let asc = match &r.tracks[app_track].codec { cv_media::mp4::Codec::Aac { asc, .. } => asc.clone(), _ => panic!() };
    let mut dec = cv_media::mf::aac::AacDecoder::new(&asc).unwrap();
    let mut pcm = Vec::new();
    for s in r.samples(app_track, 0, u64::MAX).unwrap() {
        pcm.extend(dec.decode(&r.read(&s).unwrap()).unwrap());
    }
    let loud = pcm.chunks(960).filter(|c| c.iter().any(|x| x.abs() >= SIGNAL_FLOOR)).count();
    eprintln!("app track: {} blocks of 10 ms with tone", loud);
    assert!((150..=230).contains(&loud), "tone length off: {loud}");
}

#[test]
#[ignore = "requires an interactive Windows display and render audio endpoint"]
fn live_waveform_is_written() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLIPVAULT_DATA_DIR", dir.path().join("data"));
    let wav = dir.path().join("tone.wav");
    write_tone(&wav, 1.5);
    let cfg = RecorderConfig { folder: dir.path().join("rec"), mic: None, fps: 30, max_height: 360, app_slots: 2, ..Default::default() };
    let (etx, erx) = crossbeam_channel::unbounded();
    let rec = Recorder::start(cfg, Arc::new(Namer), etx).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    TonePlayer::start(&wav).finish();
    std::thread::sleep(Duration::from_millis(1200));
    rec.stop();
    let path = erx.try_iter().find_map(|e| match e { RecEvent::SegmentFinished { path, .. } => Some(path), _ => None }).unwrap();
    let w = cv_core::waveform::Waveform::load(&cv_core::waveform::wave_path(&path)).expect("waveform file");
    let audible = level_u8(SIGNAL_FLOOR);
    let loud_full = (0..w.frames()).filter(|f| w.get(*f, 0) >= audible).count();
    let loud_app = (0..w.frames()).filter(|f| w.get(*f, 2) >= audible).count();
    eprintln!("waveform {} tracks x {} frames; full loud {loud_full}, app loud {loud_app}", w.tracks, w.frames());
    assert_eq!(w.tracks, 4);
    assert!((10..=17).contains(&loud_app), "{loud_app}");
    assert!(loud_full >= loud_app);
}
