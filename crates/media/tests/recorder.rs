//! End-to-end: record the real screen + a program's sound for a few seconds.

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

fn tone_wav(path: &std::path::Path, secs: f32) {
    let n = (48_000.0 * secs) as usize;
    let data_len = n * 4;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32] { b.extend_from_slice(&v.to_le_bytes()); }
    for v in [1u16, 2] { b.extend_from_slice(&v.to_le_bytes()); }
    for v in [48_000u32, 192_000] { b.extend_from_slice(&v.to_le_bytes()); }
    for v in [4u16, 16] { b.extend_from_slice(&v.to_le_bytes()); }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data_len as u32).to_le_bytes());
    for i in 0..n {
        let v = (0.06 * (i as f32 * 523.0 * std::f32::consts::TAU / 48_000.0).sin() * 32767.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn records_screen_and_app_sound() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    cv_media::util::disable_background_throttling();
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    tone_wav(&wav, 2.0);
    let cfg = RecorderConfig { folder: dir.path().join("rec"), fps: 30, max_height: 720, app_slots: 4, ..Default::default() };
    let (etx, erx) = crossbeam_channel::unbounded();
    let rec = Recorder::start(cfg, Arc::new(Namer), etx).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", wav.display())])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    let flushed = rec.flush(Duration::from_secs(5)).expect("flush");
    let st = rec.status();
    eprintln!("status: {}x{} fps {:.1} via {} / {}; written until {} (flush {})", st.width, st.height, st.fps, st.capture_method, st.encoder, st.written_until_ms, flushed);
    assert!(st.video_error.is_none(), "{:?}", st.video_error);
    let now = chrono::Utc::now().timestamp_millis();
    assert!(now - flushed < 1500, "flush left {} ms unwritten", now - flushed);
    let _ = child.wait();
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
    let loud = pcm.chunks(960).filter(|c| c.iter().any(|x| x.abs() > 0.02)).count();
    eprintln!("app track: {} blocks of 10 ms with tone", loud);
    assert!((150..=230).contains(&loud), "tone length off: {loud}");
}
