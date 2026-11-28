use super::flat::FlatWriter;
use super::frag::{FragWriter, TrackRun};
use super::reader::{Mp4Reader, TrackKind};
use super::*;
use std::process::Command;

const SPS: [u8; 27] = [0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58];
const PPS: [u8; 6] = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

fn tracks() -> Vec<TrackSpec> {
    vec![
        TrackSpec {
            id: 1,
            timescale: 90_000,
            codec: Codec::H264 { avcc: crate::h264::build_avcc(&SPS, &PPS) },
            width: 1920,
            height: 1080,
            name: "Video".into(),
            enabled: true,
            default_duration: 3000,
        },
        TrackSpec {
            id: 2,
            timescale: 48_000,
            codec: Codec::Aac { asc: crate::aac::asc_lc(48000, 2), channels: 2, sample_rate: 48000, avg_bitrate: 160_000 },
            width: 0,
            height: 0,
            name: "Full mix".into(),
            enabled: true,
            default_duration: 1024,
        },
        TrackSpec {
            id: 3,
            timescale: 48_000,
            codec: Codec::Aac { asc: crate::aac::asc_lc(48000, 2), channels: 2, sample_rate: 48000, avg_bitrate: 128_000 },
            width: 0,
            height: 0,
            name: "App slot 1".into(),
            enabled: false,
            default_duration: 1024,
        },
    ]
}

/// Fake video sample: a length-prefixed NAL with a recognisable payload.
fn vsample(i: u32) -> Vec<u8> {
    let nal_type = if i % 30 == 0 { 0x65 } else { 0x41 };
    let mut v = vec![0, 0, 0, 5, nal_type];
    v.extend_from_slice(&i.to_be_bytes());
    v
}

fn asample(i: u32, tag: u8) -> Vec<u8> {
    vec![tag, (i >> 8) as u8, i as u8, 0x21]
}

#[test]
fn fragmented_write_read_with_gaps_and_live_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.mp4");
    let mut w = FragWriter::create(&path, &tracks(), 1_700_000_000, Some(br#"{"start":1}"#)).unwrap();
    // 3 fragments of 1 s: 30 frames of video, ~47 AAC frames of audio.
    let mut reader: Option<Mp4Reader> = None;
    for f in 0..3u32 {
        let vs: Vec<Vec<u8>> = (f * 30..f * 30 + 30).map(vsample).collect();
        let v: Vec<SampleRef> = vs.iter().enumerate().map(|(k, d)| SampleRef { data: d, duration: 3000, sync: k == 0, cts: 0 }).collect();
        let a0 = f * 47;
        let as_: Vec<Vec<u8>> = (a0..a0 + 47).map(|i| asample(i, 0xA0)).collect();
        let a: Vec<SampleRef> = as_.iter().map(|d| SampleRef { data: d, duration: 1024, sync: true, cts: 0 }).collect();
        // The app slot only has audio in the middle fragment.
        let bs: Vec<Vec<u8>> = (0..20).map(|i| asample(i, 0xB0)).collect();
        let bsr: Vec<SampleRef> = bs.iter().map(|d| SampleRef { data: d, duration: 1024, sync: true, cts: 0 }).collect();
        let mut runs = vec![TrackRun { track_id: 1, base_time: f as u64 * 90_000, samples: &v }, TrackRun { track_id: 2, base_time: a0 as u64 * 1024, samples: &a }];
        if f == 1 {
            w.write_meta(br#"{"slots":[1]}"#).unwrap();
            runs.push(TrackRun { track_id: 3, base_time: 60_000, samples: &bsr });
        }
        w.write_fragment(&runs).unwrap();
        w.flush().unwrap();
        // Follow the file while it's being written.
        match &mut reader {
            None => reader = Some(Mp4Reader::open(&path).unwrap()),
            Some(r) => assert!(r.refresh().unwrap()),
        }
        let r = reader.as_ref().unwrap();
        let vi = r.video().unwrap();
        assert!((r.end_secs(vi) - (f + 1) as f64).abs() < 1e-9);
    }
    w.finish().unwrap();
    let r = Mp4Reader::open(&path).unwrap();
    assert!(r.is_fragmented());
    assert_eq!(r.tracks.len(), 3);
    assert_eq!(r.tracks[0].kind, TrackKind::Video);
    assert_eq!((r.tracks[0].width, r.tracks[0].height), (1920, 1080));
    assert_eq!(r.tracks[1].name, "Full mix");
    assert!(!r.tracks[2].enabled);
    assert_eq!(r.metas.len(), 2);
    assert_eq!(r.metas[1].1, br#"{"slots":[1]}"#.to_vec());
    // Video: 90 frames, keyframes every 30.
    let v = r.samples(0, 0, u64::MAX).unwrap();
    assert_eq!(v.len(), 90);
    assert_eq!(r.read(&v[31]).unwrap(), vsample(31));
    assert!(v[30].sync && !v[31].sync);
    let k = r.sync_at_or_before(0, 59 * 3000).unwrap().unwrap();
    assert_eq!(k.dts, 30 * 3000);
    assert_eq!(r.sync_times(0).unwrap(), vec![0.0, 1.0, 2.0]);
    // Range query across a fragment boundary.
    let mid = r.samples(0, 85_000, 95_000).unwrap();
    assert_eq!(mid.iter().map(|s| s.dts).collect::<Vec<_>>(), vec![84_000, 87_000, 90_000, 93_000]);
    // Audio.
    let a = r.samples(1, 0, u64::MAX).unwrap();
    assert_eq!(a.len(), 141);
    assert_eq!(r.read(&a[100]).unwrap(), asample(100, 0xA0));
    // The slot track only covers its fragment.
    assert_eq!(r.coverage(2), vec![(60_000, 60_000 + 20 * 1024)]);
    let b = r.samples(2, 0, u64::MAX).unwrap();
    assert_eq!(b.len(), 20);
    assert_eq!(r.read(&b[3]).unwrap(), asample(3, 0xB0));
    assert!((r.duration_secs() - 3.0).abs() < 0.01);
}

#[test]
fn partial_fragment_is_ignored_until_complete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.mp4");
    let mut w = FragWriter::create(&path, &tracks()[..1], 0, None).unwrap();
    let vs: Vec<Vec<u8>> = (0..30).map(vsample).collect();
    let v: Vec<SampleRef> = vs.iter().enumerate().map(|(k, d)| SampleRef { data: d, duration: 3000, sync: k == 0, cts: 0 }).collect();
    w.write_fragment(&[TrackRun { track_id: 1, base_time: 0, samples: &v }]).unwrap();
    w.write_fragment(&[TrackRun { track_id: 1, base_time: 90_000, samples: &v }]).unwrap();
    w.finish().unwrap();
    // Chop the file in the middle of the second mdat.
    let full = std::fs::read(&path).unwrap();
    std::fs::write(&path, &full[..full.len() - 20]).unwrap();
    let mut r = Mp4Reader::open(&path).unwrap();
    assert_eq!(r.samples(0, 0, u64::MAX).unwrap().len(), 30);
    std::fs::write(&path, &full).unwrap();
    assert!(r.refresh().unwrap());
    assert_eq!(r.samples(0, 0, u64::MAX).unwrap().len(), 60);
}

#[test]
fn flat_write_read_with_edit_list() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("clip.mp4");
    let t = tracks();
    let mut w = FlatWriter::new(&dest, &t, 1_700_000_000).unwrap();
    // Interleave ~0.5 s of video and audio at a time.
    for block in 0..4u32 {
        for i in block * 15..block * 15 + 15 {
            let d = vsample(i);
            w.add(0, &SampleRef { data: &d, duration: 3000, sync: i % 30 == 0, cts: 0 }).unwrap();
        }
        for i in block * 24..block * 24 + 24 {
            let d = asample(i, 0xA0);
            w.add(1, &SampleRef { data: &d, duration: 1024, sync: true, cts: 0 }).unwrap();
        }
    }
    // Present from 0.4 s (12 frames of pre-roll hidden).
    w.set_edit(0, 36_000, 60 * 3000 - 36_000);
    w.finish(&dest, Some(br#"{"tracks":[]}"#)).unwrap();
    assert!(!dest.with_extension("cvpart").exists());
    let r = Mp4Reader::open(&dest).unwrap();
    assert!(!r.is_fragmented());
    assert_eq!(r.tracks.len(), 2, "empty slot track dropped");
    let v = r.samples(0, 0, u64::MAX).unwrap();
    assert_eq!(v.len(), 60);
    for (i, s) in v.iter().enumerate() {
        assert_eq!(r.read(s).unwrap(), vsample(i as u32));
        assert_eq!(s.sync, i % 30 == 0);
    }
    let a = r.samples(1, 0, u64::MAX).unwrap();
    assert_eq!(a.len(), 96);
    assert_eq!(r.read(&a[95]).unwrap(), asample(95, 0xA0));
    assert_eq!(r.tracks[0].edit, Some((36_000, 144_000)));
    assert!((r.tracks[0].edit_start_s() - 0.4).abs() < 1e-9);
    assert_eq!(r.metas.len(), 1);
    assert_eq!(r.sync_at_or_before(0, 50 * 3000).unwrap().unwrap().dts, 30 * 3000);
}

fn ffprobe() -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(r"C:\ffmpeg-7.1-essentials_build\bin\ffprobe.exe");
    if p.is_file() {
        return Some(p);
    }
    let out = Command::new("where").arg("ffprobe").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(|l| std::path::PathBuf::from(l.trim())).filter(|p| p.is_file())
}

/// Cross-check our files with ffprobe when it happens to be installed (dev
/// machines only; ClipVault itself never uses it).
#[test]
fn ffprobe_accepts_our_files() {
    let Some(probe) = ffprobe() else {
        eprintln!("ffprobe not installed; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let frag = dir.path().join("frag.mp4");
    let mut w = FragWriter::create(&frag, &tracks(), 0, Some(b"{}")).unwrap();
    let vs: Vec<Vec<u8>> = (0..30).map(vsample).collect();
    let v: Vec<SampleRef> = vs.iter().enumerate().map(|(k, d)| SampleRef { data: d, duration: 3000, sync: k == 0, cts: 0 }).collect();
    let as_: Vec<Vec<u8>> = (0..47).map(|i| asample(i, 0xA0)).collect();
    let a: Vec<SampleRef> = as_.iter().map(|d| SampleRef { data: d, duration: 1024, sync: true, cts: 0 }).collect();
    w.write_fragment(&[TrackRun { track_id: 1, base_time: 0, samples: &v }, TrackRun { track_id: 2, base_time: 0, samples: &a }]).unwrap();
    w.finish().unwrap();
    let out = Command::new(&probe)
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height,nb_read_packets:stream_tags=handler_name", "-count_packets", "-of", "compact"])
        .arg(&frag)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{s} {}", String::from_utf8_lossy(&out.stderr));
    assert!(s.contains("codec_name=h264|width=1920|height=1080"), "{s}");
    assert!(s.contains("nb_read_packets=30"), "{s}");
    assert!(s.contains("codec_name=aac"), "{s}");
    assert!(s.contains("handler_name=Full mix"), "{s}");

    let flat = dir.path().join("flat.mp4");
    let mut w = FlatWriter::new(&flat, &tracks(), 0).unwrap();
    for (i, d) in vs.iter().enumerate() {
        w.add(0, &SampleRef { data: d, duration: 3000, sync: i == 0, cts: 0 }).unwrap();
    }
    for d in &as_ {
        w.add(1, &SampleRef { data: d, duration: 1024, sync: true, cts: 0 }).unwrap();
    }
    w.set_edit(0, 9000, 81_000);
    w.finish(&flat, None).unwrap();
    let out = Command::new(&probe)
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_name,nb_read_packets", "-count_packets", "-of", "compact"])
        .arg(&flat)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{s}");
    assert!(s.contains("nb_read_packets=30"), "{s}");
    assert!(s.contains("nb_read_packets=47"), "{s}");
}
