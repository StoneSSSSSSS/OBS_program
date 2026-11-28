use super::*;
use cv_core::mixer::{app_lane, LaneGain, MixMode, LANE_MIC};
use cv_media::audioread::read_range;
use cv_media::cut::{cut, CutSource};
use cv_media::export::{export, ExportJob, VideoOut};
use cv_media::h264::{annexb_to_avcc, build_avcc, Converted};
use cv_media::mf::aac::{AacEncoder, FRAME};
use cv_media::mf::h264::{EncoderChoice, EncoderConfig, RateControl, VideoEncoder};
use cv_media::mp4::flat::FlatWriter;
use cv_media::mp4::frag::{FragWriter, TrackRun};
use cv_media::mp4::{SampleRef, TrackSpec};
use cv_media::probe::{track_sources, TrackRole};
use cv_media::recorder::{AppIdent, SegmentMeta, SlotInfo, TrackMeta};
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[path = "review_footage.rs"]
mod review_footage;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 192;
const FPS: u32 = 25;
const GOP: usize = 20;
const FRAGMENTS: usize = 4;
const FRAGMENT_SECONDS: f64 = GOP as f64 / FPS as f64;
const SWITCH_SECONDS: f64 = 2.0 * FRAGMENT_SECONDS;
const DURATION: f64 = FRAGMENTS as f64 * FRAGMENT_SECONDS;
const START_MS: i64 = 1_800_000_000_000;
const FIRST_APP: &str = "fixture-game.exe";
const SECOND_APP: &str = "fixture-chat.exe";
const GAME_HZ: f64 = 470.0;
const VOICE_HZ: f64 = 830.0;
const MIC_HZ: f64 = 270.0;

struct Fixture {
    tracks: Vec<TrackSpec>,
    video: Vec<Converted>,
    audio: Vec<Vec<Vec<u8>>>,
}

impl Fixture {
    fn make() -> Result<Self> {
        Self::make_fragments(FRAGMENTS)
    }

    fn make_fragments(fragments: usize) -> Result<Self> {
        Self::make_with_audio(fragments, &["Full mix", "Microphone", "App 1"], |track, t| {
            let app = if t < SWITCH_SECONDS { GAME_HZ } else { VOICE_HZ };
            let app = tone(app, t) * 0.24;
            let mic = tone(MIC_HZ, t) * 0.11;
            match track {
                0 => app + mic,
                1 => mic,
                _ => app,
            }
        })
    }

    fn make_with_audio(
        fragments: usize,
        names: &[&str],
        signal: impl Fn(usize, f64) -> f32,
    ) -> Result<Self> {
        Self::make_with_images(fragments, names, signal, moving_image)
    }

    fn make_with_images(
        fragments: usize,
        names: &[&str],
        signal: impl Fn(usize, f64) -> f32,
        image: impl Fn(usize) -> Vec<u8>,
    ) -> Result<Self> {
        let mut encoder = VideoEncoder::new(None, EncoderConfig {
            width: WIDTH, height: HEIGHT, fps: FPS,
            rate: RateControl::Cbr { kbps: 1000 },
            gop: GOP as u32, choice: EncoderChoice::Software, realtime: false,
        })?;
        let mut frames = Vec::new();
        for i in 0..fragments * GOP {
            let frame = image(i);
            frames.extend(encoder.encode_cpu(&frame, (i as f64 * VIDEO_CLOCK / FPS as f64) as i64, i % GOP == 0)?);
        }
        frames.extend(encoder.finish()?);
        assert_eq!(frames.len(), fragments * GOP);
        let video: Vec<_> = frames.iter().map(|frame| annexb_to_avcc(&frame.data)).collect();
        let avcc = build_avcc(
            video[0].sps.as_deref().context("fixture H.264 SPS")?,
            video[0].pps.as_deref().context("fixture H.264 PPS")?,
        );
        let mut tracks = vec![TrackSpec {
            id: 1, timescale: 90_000, codec: Codec::H264 { avcc },
            width: WIDTH as u16, height: HEIGHT as u16, name: "Video".into(),
            enabled: true, default_duration: 90_000 / FPS,
        }];
        let mut audio = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let mut encoder = AacEncoder::new(OUT_RATE, 2, 128)?;
            let mut packets = Vec::new();
            for p in 0..packet_boundary(fragments) {
                let mut pcm = Vec::with_capacity(FRAME * 2);
                for n in p * FRAME..(p + 1) * FRAME {
                    let t = n as f64 / RATE;
                    let value = signal(i, t);
                    pcm.extend([value, value]);
                }
                packets.extend(encoder.encode(&pcm)?);
            }
            packets.extend(encoder.finish()?);
            assert!(packets.len() >= packet_boundary(fragments));
            tracks.push(TrackSpec {
                id: i as u32 + 2, timescale: OUT_RATE, codec: Codec::Aac {
                    asc: encoder.asc.clone(), channels: 2, sample_rate: OUT_RATE,
                    avg_bitrate: 128_000,
                },
                width: 0, height: 0, name: (*name).into(), enabled: i == 0,
                default_duration: FRAME as u32,
            });
            audio.push(packets);
        }
        Ok(Self { tracks, video, audio })
    }

    fn write_flat(&self, path: &Path) -> Result<()> {
        let mut writer = FlatWriter::new(path, &self.tracks, (START_MS / 1000) as u64)?;
        for frame in &self.video {
            writer.add(0, &SampleRef {
                data: &frame.data, duration: 90_000 / FPS, sync: frame.idr, cts: 0,
            })?;
        }
        let packets = packet_boundary(self.video.len() / GOP);
        for (track, samples) in self.audio.iter().enumerate() {
            for data in &samples[..packets] {
                writer.add(track + 1, &SampleRef {
                    data, duration: FRAME as u32, sync: true, cts: 0,
                })?;
            }
        }
        writer.finish(path, None)?;
        Ok(())
    }

    fn writer(&self, path: &Path) -> Result<FragWriter> {
        let roles = ["video", "full", "mic", "slot"];
        let tracks = self.tracks.iter().zip(roles).map(|(t, role)| TrackMeta {
            id: t.id,
            role: role.into(),
            slot: (role == "slot").then_some(0),
            app: None,
            name: t.name.clone(),
        }).collect();
        let initial = SegmentMeta {
            at_s: 0.0, start_ms: Some(START_MS), tracks, slots: vec![slot(0)],
        };
        Ok(FragWriter::create(path, &self.tracks, (START_MS / 1000) as u64,
            Some(&serde_json::to_vec(&initial)?))?)
    }

    fn append(&self, writer: &mut FragWriter, fragment: usize) -> Result<()> {
        if fragment == 2 {
            writer.write_meta(&serde_json::to_vec(&SegmentMeta {
                at_s: SWITCH_SECONDS, slots: vec![slot(2)], ..Default::default()
            })?)?;
        }
        let video: Vec<_> = self.video[fragment * GOP..(fragment + 1) * GOP].iter()
            .map(|frame| SampleRef {
                data: &frame.data, duration: 90_000 / FPS, sync: frame.idr, cts: 0,
            }).collect();
        assert!(video[0].sync, "each fragment starts on a keyframe");
        let a = packet_boundary(fragment);
        let b = packet_boundary(fragment + 1);
        let audio: Vec<Vec<_>> = self.audio.iter().map(|track| track[a..b].iter()
            .map(|data| SampleRef { data, duration: FRAME as u32, sync: true, cts: 0 }).collect()).collect();
        let mut runs = vec![TrackRun {
            track_id: 1, base_time: (fragment * GOP) as u64 * (90_000 / FPS as u64),
            samples: &video,
        }];
        for (i, samples) in audio.iter().enumerate() {
            runs.push(TrackRun { track_id: i as u32 + 2, base_time: (a * FRAME) as u64, samples });
        }
        writer.write_fragment(&runs)?;
        writer.flush()?;
        Ok(())
    }

    fn write(&self, path: &Path) -> Result<()> {
        let mut writer = self.writer(path)?;
        for i in 0..self.video.len() / GOP {
            self.append(&mut writer, i)?;
        }
        writer.finish()?;
        Ok(())
    }
}

fn packet_boundary(fragment: usize) -> usize {
    (fragment as f64 * FRAGMENT_SECONDS * RATE / FRAME as f64).ceil() as usize
}

fn slot(fragment: usize) -> SlotInfo {
    let (key, name, category) = if fragment < 2 {
        (FIRST_APP, "Fixture Game", "game")
    } else {
        (SECOND_APP, "Fixture Chat", "voice")
    };
    SlotInfo { slot: 0, app: AppIdent { key: key.into(), name: name.into(), category: category.into() } }
}

fn moving_image(index: usize) -> Vec<u8> {
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let mut nv12 = vec![128; w * h * 3 / 2];
    // Two high-contrast pieces of motion plus three independently colored sections.
    let bar = (index * 7) % (w - 32);
    let row = (index * 3) % (h - 26);
    for (y, line) in nv12[..w * h].chunks_exact_mut(w).enumerate() {
        for (x, pixel) in line.iter_mut().enumerate() {
            *pixel = if (bar..bar + 32).contains(&x) || (row..row + 26).contains(&y) {
                218
            } else if (x / 20 + y / 20) % 2 == 0 {
                86
            } else {
                116
            };
        }
    }
    // Write the frame index into the image so tests can measure displayed
    // time independently of the player's published clock.
    for line in nv12[..w * 10].chunks_exact_mut(w) {
        for bit in 0..8 {
            let x = 8 + bit * 16;
            line[x..x + 16].fill(if index & (1 << bit) == 0 { 20 } else { 234 });
        }
    }
    for line in nv12[w * h..].chunks_exact_mut(w) {
        for (x, uv) in line.chunks_exact_mut(2).enumerate() {
            uv.copy_from_slice(match x * 6 / w {
                0 => &[110, 166],
                1 => &[102, 101],
                _ => &[175, 118],
            });
        }
    }
    nv12
}

fn tone(hz: f64, t: f64) -> f32 {
    (std::f64::consts::TAU * hz * t).sin() as f32
}

fn amplitude(pcm: &[f32], hz: f64) -> f64 {
    let mut sum = [0.0f64; 2];
    for (i, x) in pcm.chunks_exact(2).enumerate() {
        let phase = std::f64::consts::TAU * hz * i as f64 / RATE;
        sum[0] += f64::from(x[0]) * phase.cos();
        sum[1] += f64::from(x[0]) * phase.sin();
    }
    4.0 * sum[0].hypot(sum[1]) / pcm.len() as f64
}

fn assert_bar_at(frame: &VideoFrame, expected_index: usize) {
    let bar = (expected_index * 7) % (WIDTH as usize - 32);
    let red_at = |x| i16::from(frame.rgba[(15 * frame.width + x) * 4]);
    assert!(red_at(bar + 16) > red_at((bar + 88) % frame.width) + 60);
}

fn displayed_frame(frame: &VideoFrame) -> usize {
    (0..8).fold(0, |index, bit| {
        let x = 16 + 16 * bit;
        let px = (5 * frame.width + x) * 4;
        let bright = frame.rgba[px..px + 3].iter().map(|&v| u16::from(v)).sum::<u16>() > 384;
        index | (usize::from(bright) << bit)
    })
}

const LEGACY_TONES: [f64; 6] = [330.0, GAME_HZ, VOICE_HZ, 610.0, MIC_HZ, 970.0];

fn legacy_fixture(path: &Path) -> Result<()> {
    Fixture::make_with_audio(FRAGMENTS, &["SoundHandler"; 6], |i, time| {
        tone(LEGACY_TONES[i], time) * 0.2
    })?.write_flat(path)
}

fn muted_mic() -> PreviewMix {
    let mut preview = PreviewMix::default();
    preview.mix.set(LANE_MIC, LaneGain { gain: 1.0, muted: true });
    preview
}

fn wait_for(player: &NativePlayer, condition: impl Fn(&NativeState, Option<&VideoFrame>) -> bool) -> NativeState {
    let start = Instant::now();
    loop {
        let state = player.state();
        let frame = player.frame();
        if condition(&state, frame.as_deref()) {
            return state;
        }
        assert!(start.elapsed() < Duration::from_secs(8),
            "timed out: loaded={}, paused={}, position={:.3}, duration={:.3}, ended={}, frame={:?}, error={:?}",
            state.loaded, state.paused, state.position, state.duration, state.ended,
            frame.as_ref().map(|f| f.serial), state.error);
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) static WORKER_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[test]
#[ignore = "Measures native player resources with a project-local synthetic file"]
fn measure_player_resources() -> Result<()> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    fn sample() -> Result<(f64, usize, u64)> {
        let (mut created, mut exit, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let mut mem = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of_val(&mem) as u32;
        unsafe {
            let process = GetCurrentProcess();
            GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)?;
            GetProcessMemoryInfo(process, &mut mem, size)?;
        }
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Ok(((ticks(kernel) + ticks(user)) as f64 / 10_000_000.0, mem.WorkingSetSize, WORKER_TICKS.load(std::sync::atomic::Ordering::Relaxed)))
    }
    let path = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("synthetic fixture required")?);
    let player = NativePlayer::new(egui::Context::default());
    player.set_volume(0.0);
    player.load(path, 0.0, false, true);
    wait_for(&player, |s, f| s.loaded && f.is_some());
    for (name, playing) in [("paused", false), ("playing", true), ("paused after playback", false)] {
        player.set_paused(!playing);
        wait_for(&player, |s, _| s.paused != playing);
        let before_frame = player.frame().unwrap().serial;
        let before = sample()?;
        let then = Instant::now();
        std::thread::sleep(Duration::from_secs(5));
        let after = sample()?;
        let seconds = then.elapsed().as_secs_f64();
        let frames = player.frame().unwrap().serial - before_frame;
        println!("{name}: {:.2}% of one CPU core, {:.1} MiB resident, {} worker ticks, {frames} new frames in {seconds:.2}s",
            (after.0 - before.0) * 100.0 / seconds, after.1 as f64 / 1048576.0, after.2 - before.2);
        if playing { assert!(frames > 90 && player.state().position > 4.5); } else { assert_eq!(frames, 0); }
    }
    Ok(())
}

#[test]
fn frame_steps_follow_visible_variable_rate_frames_and_clamp_at_trim() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("variable-rate.mp4");
    let fixture = Fixture::make()?;
    let mut file = FlatWriter::new(&path, &fixture.tracks[..1], 0)?;
    for (i, frame) in fixture.video.iter().take(12).enumerate() {
        // Pausing at any time inside the long frame must step to its neighbor.
        file.add(0, &SampleRef { data: &frame.data, duration: if i == 4 { 14_400 } else { 3600 }, sync: frame.idr, cts: 0 })?;
    }
    // Frame 1 is partially visible at time zero. Frames 0 and 11 are outside the edit.
    file.set_edit(0, 4_500, 45_900);
    file.finish(&path, None)?;
    let player = NativePlayer::new(egui::Context::default());
    player.load(path, 0.20, false, true);
    let shown = |index: usize| wait_for(&player, |s, f| s.paused && f.is_some_and(|f| displayed_frame(f) == index));
    shown(4);
    player.step(true);
    let stepped = shown(5);
    assert!((stepped.position - 0.27).abs() < 0.0001);
    for _ in 0..4 { player.step(false); }
    assert!(shown(1).first_frame);
    player.step(false);
    player.step(true);
    shown(2);
    player.seek_latest();
    assert!(shown(10).last_frame);
    player.seek(0.509, true);
    player.step(false);
    shown(9);
    player.seek_latest();
    assert!(shown(10).last_frame);
    player.step(true);
    player.step(false);
    shown(9);
    Ok(())
}

#[test]
fn converts_nv12_directly_to_rgba() {
    let frame = DecodedFrame { pts_hns: 0, width: 4, height: 2,
        nv12: vec![16, 235, 81, 145, 145, 81, 235, 16, 128, 128, 90, 240] };
    let direct = rgba_from_nv12(&frame);
    let reference = cv_media::mf::h264dec::nv12_to_rgb(&frame, 0).into_raw();
    assert_eq!(direct.len(), 32);
    for (actual, expected) in direct.chunks_exact(4).zip(reference.chunks_exact(3)) {
        assert_eq!(actual[3], 255);
        for (actual, expected) in actual[..3].iter().zip(expected) {
            assert!(actual.abs_diff(*expected) <= 1);
        }
    }
}

#[test]
fn plays_pauses_seeks_changes_speed_and_restarts_at_end() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("preview.mp4");
    Fixture::make()?.write(&path)?;
    let player = NativePlayer::new(egui::Context::default());
    // Exercise the real Windows output if available, without making test noise.
    player.set_volume(0.0);
    player.set_mix(muted_mic());
    player.load(path, 0.0, false, true);
    let initial = wait_for(&player, |s, f| s.loaded && s.paused && f.is_some());
    assert_eq!(initial.audio_tracks, 3);
    assert_eq!(initial.info.unwrap().slot_history.len(), 2);
    let first = player.frame().unwrap();
    assert_eq!((first.width, first.height), (WIDTH as usize, HEIGHT as usize));

    player.set_paused(false);
    wait_for(&player, |s, f| s.position > 0.27 && f.is_some_and(|f| f.serial > first.serial));
    let motion = player.frame().unwrap();
    assert_ne!(motion.rgba, first.rgba);

    player.set_paused(true);
    let stopped = wait_for(&player, |s, _| s.paused);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(player.state().position, stopped.position);

    player.seek(1.17, true);
    wait_for(&player, |s, f| (s.position - 1.17).abs() < 0.001 && f.is_some_and(|f| f.serial > motion.serial));
    let exact = player.frame().unwrap();
    assert_ne!(exact.rgba, first.rgba);
    assert_bar_at(&exact, 29);
    player.seek(1.43, false);
    wait_for(&player, |s, f| (s.position - 0.8).abs() < 0.06 && f.is_some_and(|f| f.serial > exact.serial));
    assert_bar_at(&player.frame().unwrap(), 20);

    player.seek(2.62, true);
    wait_for(&player, |s, _| (s.position - 2.62).abs() < 0.001);
    player.set_speed(2.0);
    player.set_paused(false);
    let ended = wait_for(&player, |s, _| s.ended && s.paused);
    assert!((ended.duration - DURATION).abs() < 0.04);
    assert_eq!(ended.position, ended.duration);

    player.set_paused(false);
    wait_for(&player, |s, _| !s.ended && !s.paused && s.position < 0.7);
    player.clear();
    wait_for(&player, |s, f| !s.loaded && f.is_none());
    Ok(())
}

#[test]
fn follows_complete_live_fragments_and_slot_metadata() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("growing.mp4");
    let fixture = Fixture::make()?;
    let mut writer = fixture.writer(&path)?;
    fixture.append(&mut writer, 0)?;
    let player = NativePlayer::new(egui::Context::default());
    player.set_volume(0.0);
    player.load(path, 0.0, true, true);
    let first = wait_for(&player, |s, frame| s.loaded && frame.is_some());
    assert!((first.duration - 0.8).abs() < 0.04);
    let first_info = first.info.unwrap();
    assert!(Arc::ptr_eq(&first_info, &player.state().info.unwrap()));

    player.set_speed(2.0);
    player.set_paused(false);
    let edge = wait_for(&player, |s, _| s.position >= s.duration - 0.02);
    assert!(!edge.ended && !edge.paused);
    fixture.append(&mut writer, 1)?;
    fixture.append(&mut writer, 2)?;
    let grown = wait_for(&player, |s, _| s.duration > 2.3 && s.position > 1.2);
    let updated = grown.info.unwrap();
    assert!(!Arc::ptr_eq(&first_info, &updated));
    assert_eq!(updated.slot_history[1].slots[0].app.key, SECOND_APP);
    player.set_paused(true);
    wait_for(&player, |s, _| s.paused);
    let first_serial = player.frame().unwrap().serial;
    player.seek(2.13, true);
    wait_for(&player, |s, f| (s.position - 2.13).abs() < 0.001 && f.is_some_and(|f| f.serial > first_serial));
    writer.finish()?;
    Ok(())
}

#[test]
fn preview_and_exports_follow_reused_app_slot_edits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("source.mp4");
    Fixture::make()?.write(&path)?;
    let mut preview = muted_mic();
    preview.mix.set(&app_lane(FIRST_APP), LaneGain { gain: 0.5, muted: false });
    preview.edits.push(AudioEdit::new(&app_lane(SECOND_APP), 1.9, 2.4, 0.0));
    let mut media = Media::open(path.clone(), false, &preview)?;
    let read = |media: &mut Media, secs: f64| media.pcm(secs * RATE, 1.0, 4800, 1.0);
    let game = read(&mut media, 0.6)?;
    assert!((0.08..0.16).contains(&amplitude(&game, GAME_HZ)));
    assert!(amplitude(&game, MIC_HZ) < 0.01);
    let silence = read(&mut media, 2.05)?;
    assert!(amplitude(&silence, VOICE_HZ) < 0.01);
    let voice = read(&mut media, 2.65)?;
    assert!(amplitude(&voice, VOICE_HZ) > 0.17);
    assert!(amplitude(&voice, GAME_HZ) < 0.02);

    let joined = (0..10).map(|i| media.pcm(2.67 * RATE + (i * 480) as f64, 1.0, 480, 1.0))
        .collect::<Result<Vec<_>>>()?.concat();
    assert!(amplitude(&joined, VOICE_HZ) > 0.17);
    assert!((amplitude(&joined, VOICE_HZ) - amplitude(&voice, VOICE_HZ)).abs() < 0.02);

    let keep = AtomicBool::new(false);
    let clip_path = dir.path().join("cut.mp4");
    let clip = cut(&[CutSource { path: path.clone(), from_s: 0.5, to_s: Some(2.9) }],
        &clip_path, Some(START_MS + 500), &mut |_| {}, &keep)?;
    assert!((clip.duration - 2.4).abs() < 0.01);
    let clip_media = Media::open(clip_path, false, &PreviewMix::default())?;
    let apps: Vec<_> = clip_media.info.audio.iter().filter_map(|t| match &t.role {
        TrackRole::App { app } => Some(app.key.as_str()),
        _ => None,
    }).collect();
    assert_eq!(apps, [FIRST_APP, SECOND_APP]);

    let export_path = dir.path().join("mix.mp4");
    let mut job = ExportJob {
        source: path, range: None, gains: media.curves.clone(), video: VideoOut::Copy,
        audio_kbps: 128, start_ms: Some(START_MS),
    };
    export(&job, &export_path, &mut |_| {}, &keep)?;
    let reader = Mp4Reader::open(&export_path)?;
    assert_eq!(reader.audio().len(), 1);
    let ti = reader.audio()[0];
    let game = read_range(&reader, ti, 0.65, 0.75)?;
    let mute = read_range(&reader, ti, 2.08, 2.18)?;
    let voice = read_range(&reader, ti, 2.68, 2.78)?;
    assert!((0.07..0.17).contains(&amplitude(&game, GAME_HZ)));
    assert!(amplitude(&game, MIC_HZ) < 0.02);
    assert!(amplitude(&mute, VOICE_HZ) < 0.02);
    assert!(amplitude(&voice, VOICE_HZ) > 0.15);

    job.range = Some((0.3, 1.4));
    job.video = VideoOut::Reencode { kbps: 320, max_height: 144 };
    let small = dir.path().join("smaller.mp4");
    export(&job, &small, &mut |_| {}, &keep)?;
    let info = cv_media::probe::probe(&small)?;
    assert!((info.duration - 1.1).abs() < 0.06);
    assert_eq!(info.height, 144);
    assert_eq!(info.audio.len(), 1);

    let mut hard_mute = muted_mic();
    hard_mute.mix.set(&app_lane(FIRST_APP), LaneGain { gain: 1.0, muted: true });
    media.update_curves(&hard_mute);
    let boundary = media.pcm(SWITCH_SECONDS * RATE - 6.0, 1.0, 12, 1.0)?;
    assert!(boundary[..12].iter().all(|&sample| sample == 0.0));
    assert!(boundary[12..].iter().any(|&sample| sample.abs() > 0.01));

    preview.mix.mode = MixMode::Full;
    media.update_curves(&preview);
    let full = read(&mut media, 2.05)?;
    assert!(amplitude(&full, VOICE_HZ) > 0.17);
    assert!(amplitude(&full, MIC_HZ) > 0.07);
    Ok(())
}

#[test]
fn legacy_six_track_labels_and_audio_edits_match_export() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("legacy.mp4");
    legacy_fixture(&source)?;

    // Old clips saved positional gains and edits, with no lane string.
    let mut preview = PreviewMix::default();
    preview.mix = MixSettings::from_legacy_gains(&[0.0, 0.5, 0.25, 1.0, 0.0, 1.0]);
    preview.edits.push(serde_json::from_str(r#"{"track":2,"start":0.9,"end":1.4,"gain":0.0}"#)?);
    assert_eq!(preview.edits[0].lane_id(), "cat:voice");

    let mut media = Media::open(source.clone(), false, &preview)?;
    let names: Vec<_> = media.info.audio.iter().map(|track| track.name.as_str()).collect();
    assert_eq!(names, ["Full mix", "Game", "Voice chat", "Music", "Microphone", "Desktop"]);
    let lanes: Vec<_> = track_sources(&media.info).iter().map(|source| source.lanes(None)).collect();
    assert_eq!(lanes.into_iter().flatten().collect::<Vec<_>>(),
        ["full", "cat:game", "cat:voice", "cat:media", "mic", "cat:other"]);
    assert!(media.info.slot_history.is_empty());
    assert!(!media.info.fragmented);

    let mut player = crate::player::Player::new(egui::Context::default(), &cv_core::config::Config::default());
    player.open(source.to_str().unwrap(), 0.0, 6, &[], Some(&preview.mix), false, false,
        &cv_core::config::PlayerConfig::default());
    let started = Instant::now();
    while player.track_count != 6 && started.elapsed() < Duration::from_secs(5) {
        player.after_load_check();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.track_count, 6);
    let visible: BTreeMap<_, _> = player.lane_ids.iter().zip(&player.track_names)
        .map(|(id, name)| (id.as_str(), name.as_str())).collect();
    for (id, name) in [("full", "Full mix"), ("cat:game", "Game"), ("cat:voice", "Voice chat"),
        ("cat:media", "Music & media"), ("mic", "Microphone"), ("cat:other", "Desktop")]
    {
        assert_eq!(visible.get(id), Some(&name));
    }

    let mut job = ExportJob {
        source: source.clone(), range: None,
        gains: mix_gains(&cv_media::probe::probe(&source)?, &preview.mix, &preview.edits,
            preview.edit_offset, preview.fade, &preview.overrides),
        video: VideoOut::Copy, audio_kbps: 160, start_ms: None,
    };
    let check = |media: &mut Media, job: &ExportJob, path: &Path, expectations: &[(f64, [f64; 6])]| -> Result<()> {
        export(job, path, &mut |_| {}, &AtomicBool::new(false))?;
        let reader = Mp4Reader::open(path)?;
        assert_eq!(reader.audio().len(), 1);
        for (time, expected) in expectations {
            let live = media.pcm(*time * RATE, 1.0, 4800, 1.0)?;
            let exported = read_range(&reader, reader.audio()[0], *time, time + 0.1)?;
            for (i, hz) in LEGACY_TONES.into_iter().enumerate() {
                let (a, b) = (amplitude(&live, hz), amplitude(&exported, hz));
                println!("{time:.2}s {hz:.0} Hz: preview={a:.3}, export={b:.3}, expected={:.3}", expected[i]);
                assert!((a - expected[i]).abs() < 0.024);
                assert!((b - expected[i]).abs() < 0.034);
                assert!((a - b).abs() < 0.025);
            }
        }
        Ok(())
    };
    check(&mut media, &job, &dir.path().join("separate.mp4"), &[
        (0.55, [0.0, 0.1, 0.05, 0.2, 0.0, 0.2]),
        (1.10, [0.0, 0.1, 0.0, 0.2, 0.0, 0.2]),
        (1.80, [0.0, 0.1, 0.05, 0.2, 0.0, 0.2]),
    ])?;

    preview.mix = MixSettings::from_legacy_gains(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    media.update_curves(&preview);
    job.gains = mix_gains(&cv_media::probe::probe(&source)?, &preview.mix, &preview.edits,
        preview.edit_offset, preview.fade, &preview.overrides);
    check(&mut media, &job, &dir.path().join("full.mp4"), &[
        (1.10, [0.2, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ])?;
    Ok(())
}

#[test]
#[ignore = "measures the default Windows render endpoint; writes only silent samples"]
fn legacy_video_tracks_wasapi_clock_through_play_pause_and_seek() -> Result<()> {
    cv_media::mf::init();
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("legacy-timing.mp4");
    legacy_fixture(&source)?;
    let mut playback = Playback::new();
    playback.command(Command::Volume(0.0))?;
    playback.command(Command::Load(1, source, 0.0, false, true))?;

    let next_frame = |player: &mut Playback| -> Result<VideoFrame> {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Some(frame) = player.tick()? {
                return Ok(frame);
            }
            std::thread::sleep(TICK);
        }
        anyhow::bail!("timed out waiting for a picture");
    };
    assert_eq!(displayed_frame(&next_frame(&mut playback)?), 0);
    playback.command(Command::Pause(false))?;
    let clock = |player: &Playback| -> Result<f64> {
        let output = player.output.as_ref().context("WASAPI did not open")?;
        anyhow::ensure!(output.started, "Windows endpoint never started");
        Ok((player.next_audio - f64::from(output.padding()?) * player.speed) / RATE)
    };
    let mut worst: f64 = 0.0;
    for _ in 0..7 {
        let frame = next_frame(&mut playback)?;
        let expected = clock(&playback)?;
        let video = displayed_frame(&frame) as f64 / FPS as f64;
        worst = worst.max((video - expected).abs());
        println!("play: WASAPI {expected:.3}s, frame {video:.3}s");
    }

    playback.command(Command::Pause(true))?;
    let paused = playback.state.position;
    std::thread::sleep(Duration::from_millis(100));
    assert!(playback.tick()?.is_none());
    assert_eq!(playback.state.position, paused);
    assert!(!playback.output.as_ref().unwrap().started);
    assert_eq!(playback.output.as_ref().unwrap().padding()?, 0);

    playback.command(Command::Seek(1.17, true))?;
    let exact = next_frame(&mut playback)?;
    assert!((playback.state.position - 1.17).abs() < 0.001);
    assert_eq!(displayed_frame(&exact), 29);
    assert!((playback.next_audio / RATE - 1.17).abs() < 0.001);
    println!("paused exact seek: requested 1.170s, frame {:.3}s, audio cursor {:.3}s",
        displayed_frame(&exact) as f64 / FPS as f64, playback.next_audio / RATE);

    playback.command(Command::Pause(false))?;
    for _ in 0..7 {
        let frame = next_frame(&mut playback)?;
        let expected = clock(&playback)?;
        let video = displayed_frame(&frame) as f64 / FPS as f64;
        worst = worst.max((video - expected).abs());
        println!("resume: WASAPI {expected:.3}s, frame {video:.3}s");
    }

    playback.command(Command::Seek(2.33, true))?;
    let changed = next_frame(&mut playback)?;
    assert_eq!(displayed_frame(&changed), 58);
    let resumed = next_frame(&mut playback)?;
    let expected = clock(&playback)?;
    let video = displayed_frame(&resumed) as f64 / FPS as f64;
    worst = worst.max((video - expected).abs());
    println!("playing seek: WASAPI {expected:.3}s, frame {video:.3}s, largest measured separation {worst:.3}s");
    assert!(worst < 0.100, "A/V clock and decoded display frame separated by {worst:.3}s");
    Ok(())
}

#[test]
#[ignore = "writes an opt-in fixture to CLIPVAULT_FIXTURE_PATH"]
fn write_synthetic_fixture() -> Result<()> {
    let path = PathBuf::from(std::env::var_os("CLIPVAULT_FIXTURE_PATH").context("set CLIPVAULT_FIXTURE_PATH")?);
    anyhow::ensure!(path.is_absolute() && !path.exists(), "choose a new absolute output path");
    let fragments = match std::env::var("CLIPVAULT_FIXTURE_FRAGMENTS") {
        Ok(value) => value.parse::<usize>().context("parse CLIPVAULT_FIXTURE_FRAGMENTS")?,
        Err(std::env::VarError::NotPresent) => FRAGMENTS,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!((3..=75).contains(&fragments), "choose 3 to 75 fixture fragments");
    Fixture::make_fragments(fragments)?.write(&path)?;
    println!("Synthetic media fixture: {}", path.display());
    Ok(())
}

#[test]
fn new_load_invalidates_the_previous_segment_before_the_worker_opens_it() {
    let (tx, rx) = unbounded();
    let old = NativeState { loaded: true, position: 7.95, duration: 8.0, ..Default::default() };
    let view = Arc::new(Mutex::new(View { generation: 1, ..Default::default() }));
    assert!(view.lock().publish(1, &old, Some(VideoFrame {
        serial: 8, width: 2, height: 2, rgba: vec![255; 16],
    })));
    let player = NativePlayer { tx, view: view.clone() };
    player.load(PathBuf::from("next.mp4"), 0.0, true, false);
    assert!(matches!(rx.try_recv().unwrap(), Command::Load(2, ..)));
    assert!(!view.lock().publish(1, &old, None));
    assert!(!player.state().loaded);
    assert_eq!(player.state().position, 0.0);
    assert!(player.frame().is_none());
}
