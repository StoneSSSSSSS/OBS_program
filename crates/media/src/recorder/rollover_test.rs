//! Drive the actual recorder muxer across its hourly rollover without capture.

use super::*;
use crate::mp4::reader::Mp4Reader;
use anyhow::ensure;

const HOUR_S: u64 = 3600;
const SOURCE_BLOCK_S: u64 = 8;
const HNS_PER_S: u64 = 10_000_000;
const VIDEO_SCALE: u64 = 90_000;
const AUDIO_SCALE: u64 = 48_000;
const AUDIO_PHASE: u64 = 256;

#[test]
#[ignore = "Requires the project-local synthetic 40s MP4; writes a short-lived synthetic hour under QA"]
fn muxer_hourly_roll_preserves_video_and_straddling_audio() -> Result<()> {
    let qa = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    let fixture_path = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("Set CLIPVAULT_TEST_FIXTURE to the synthetic 40s MP4")?).canonicalize()?;
    ensure!(fixture_path.starts_with(&qa), "Fixture must be inside the project-local QA folder");
    let folder = tempfile::Builder::new().prefix("muxer-hourly-").tempdir_in(&qa)?;
    let source = Mp4Reader::open(&fixture_path)?;
    let video = source.video().context("Fixture has no video")?;
    let audio = *source.audio().first().context("Fixture has no audio")?;
    let track = &source.tracks[video];
    let Codec::H264 { avcc } = &track.codec else { anyhow::bail!("Fixture must be H.264"); };
    let (_, sps, pps) = crate::h264::parse_avcc(avcc).context("Invalid fixture H.264 configuration")?;
    let sps = sps.first().context("Fixture SPS is missing")?;
    let pps = pps.first().context("Fixture PPS is missing")?;
    ensure!(u64::from(track.timescale) == VIDEO_SCALE && u64::from(source.tracks[audio].timescale) == AUDIO_SCALE);

    let (send, receive) = unbounded();
    let epoch = Epoch::now();
    let mut muxer = Muxer::new(Arc::new(Shared {
        window_test: None,
        cfg: Mutex::new(RecorderConfig {
            folder: folder.path().to_path_buf(),
            segment_secs: HOUR_S as u32,
            fps: 25,
            mic: None,
            record_apps: false,
            ..Default::default()
        }),
        status: Mutex::new(RecorderStatus::default()),
        stop: AtomicBool::new(false),
        force_key: AtomicBool::new(false),
        epoch,
        events: send,
    }));

    // Write an entire hour with 8s blocks, using a distinct second block
    // after the rollover. AAC phase remains continuous through every block.
    for elapsed in (0..(HOUR_S + SOURCE_BLOCK_S)).step_by(SOURCE_BLOCK_S as usize) {
        let fixture_from = if elapsed < HOUR_S { 0 } else { SOURCE_BLOCK_S };
        let fixture_to = fixture_from + SOURCE_BLOCK_S;
        let video_samples = source.samples(video, fixture_from * VIDEO_SCALE, fixture_to * VIDEO_SCALE)?;
        let audio_samples = source.samples(audio, fixture_from * AUDIO_SCALE, fixture_to * AUDIO_SCALE)?;
        ensure!(video_samples.first().is_some_and(|first| first.sync && first.dts == fixture_from * VIDEO_SCALE));
        ensure!(video_samples.last().is_some_and(|last| last.end() == fixture_to * VIDEO_SCALE));
        ensure!(audio_samples.first().is_some_and(|first| first.dts == fixture_from * AUDIO_SCALE));
        ensure!(audio_samples.last().is_some_and(|last| last.end() == fixture_to * AUDIO_SCALE));
        let audio_queue = muxer.audio.entry(AudioTrack::Full).or_default();
        for sample in &audio_samples {
            audio_queue.push_back(AudioPacket {
                track: AudioTrack::Full,
                pos: (elapsed * AUDIO_SCALE + sample.dts - fixture_from * AUDIO_SCALE + AUDIO_PHASE) as i64,
                data: source.read(sample)?,
            });
        }

        let at_hns = |dts| epoch.hns + ((elapsed * VIDEO_SCALE + dts - fixture_from * VIDEO_SCALE) * HNS_PER_S / VIDEO_SCALE) as i64;
        let gop_starts: Vec<_> = video_samples.iter().enumerate().filter_map(|(index, sample)| sample.sync.then_some(index))
            .chain(std::iter::once(video_samples.len())).collect();
        for bounds in gop_starts.windows(2) {
            let (from, to) = (bounds[0], bounds[1]);
            let packets = video_samples[from..to].iter().map(|sample| Ok(VideoPacket {
                time_hns: at_hns(sample.dts),
                data: source.read(sample)?,
                key: sample.sync,
                sps: sample.sync.then(|| sps.clone()),
                pps: sample.sync.then(|| pps.clone()),
                width: track.width,
                height: track.height,
            })).collect::<Result<Vec<_>>>()?;
            let end = video_samples.get(to).map_or(fixture_to * VIDEO_SCALE, |next| next.dts);
            muxer.write_gop(Gop { start: at_hns(video_samples[from].dts), packets }, at_hns(end))?;
        }
    }
    let last = muxer.seg.take().context("No final recording segment")?;
    muxer.finish_segment(last);
    let segments: Vec<_> = receive.try_iter().filter_map(|event| match event {
        RecEvent::SegmentFinished { path, start_ms, duration_s, .. } => Some((path, start_ms, duration_s)),
        _ => None,
    }).collect();
    ensure!(segments.len() == 2, "Expected two segments, got {segments:?}");
    ensure!((segments[0].2 - HOUR_S as f64).abs() < 0.000_001, "Recorder did not roll after one hour");
    ensure!((segments[1].2 - SOURCE_BLOCK_S as f64).abs() < 0.000_001, "Recorder rolled at the wrong GOP");
    ensure!(segments[1].1 - segments[0].1 == (HOUR_S * 1000) as i64, "Embedded wall-clock rollover shifted: {segments:?}");

    let left = Mp4Reader::open(&segments[0].0)?;
    let right = Mp4Reader::open(&segments[1].0)?;
    ensure!(file_span(&segments[0].0)?.0 == segments[0].1);
    ensure!(file_span(&segments[1].0)?.0 == segments[1].1);
    for (source_index, id, scale, phase) in [(video, TRACK_VIDEO, VIDEO_SCALE, 0), (audio, TRACK_FULL, AUDIO_SCALE, AUDIO_PHASE)] {
        let left_index = left.track_index(id).context("Left track missing")?;
        let right_index = right.track_index(id).context("Right track missing")?;
        let seam = HOUR_S * scale;
        let phase_at_dts = |dts| dts + phase;
        // Compare original packets from both sides in a narrow range around
        // the split, mapping their decode timestamps to the native recorder.
        let expected = source.samples(source_index, 6 * scale, 10 * scale)?;
        let left_packets = left.samples(left_index, seam - 2 * scale + phase, seam)?;
        let right_packets = right.samples(right_index, 0, 2 * scale + phase)?;
        let actual: Vec<_> = left_packets.iter().map(|sample| (sample, &left, 0))
            .chain(right_packets.iter().map(|sample| (sample, &right, seam))).collect();
        ensure!(left_packets.len() > 1 && right_packets.len() > 1, "Expected packets on both sides");
        ensure!(left.read(left_packets.last().unwrap())? != right.read(&right_packets[0])?, "Fixture payloads must distinguish the seam");
        ensure!(left_packets.last().unwrap().end() == seam + right_packets[0].dts, "Tracks are discontinuous at the seam");
        if id == TRACK_FULL {
            ensure!(left_packets.last().unwrap().end() > seam && right_packets[0].dts == AUDIO_PHASE, "AAC packet did not straddle the split");
        }
        ensure!(expected.len() == actual.len(), "Track {id}: expected {} packets, got {}", expected.len(), actual.len());
        for (expected, (sample, reader, base)) in expected.iter().zip(actual) {
            let dts = phase_at_dts((HOUR_S - SOURCE_BLOCK_S) * scale + expected.dts);
            ensure!((sample.dts + base, sample.duration, sample.sync) == (dts, expected.duration, expected.sync),
                "Track {id}: timing diverged at output DTS {dts}");
            ensure!(reader.read(sample)? == source.read(expected)?, "Track {id}: payload diverged at output DTS {dts}");
        }
    }
    Ok(())
}
