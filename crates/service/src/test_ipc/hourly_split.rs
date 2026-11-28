//! Synthetic native MP4 packet continuity through a wall-clock hour boundary.

use super::*;
use chrono::TimeZone;
use cv_media::cut::{cut, CutSource};
use cv_media::mp4::frag::{FragWriter, TrackRun};
use cv_media::mp4::reader::Mp4Reader;
use cv_media::mp4::{SampleRef, TrackSpec};
use cv_media::recorder::{SegmentMeta, TrackMeta};
use std::sync::atomic::{AtomicBool, Ordering};

const SPLIT_S: u64 = 8;
const FIXTURE_S: u64 = 16;
const HOUR_S: u64 = 3600;
const IN_MS: i64 = 6_137;
const OUT_MS: i64 = 10_513;

/// Remux the same encoded packets in each fixture. The split happens at a
/// video keyframe. An AAC packet straddling the seam stays in the earlier
/// file, matching the recorder's rule of assigning packets by their start.
pub(super) fn write_fragment(
    reader: &Mp4Reader,
    dest: &Path,
    start_ms: i64,
    from_s: u64,
    to_s: u64,
    audio_phase: u64,
    repetitions: u64,
) -> Result<()> {
    let video = reader.video().context("fixture has no H.264 track")?;
    let audio = *reader.audio().first().context("fixture has no AAC track")?;
    let mut tracks = Vec::new();
    let mut data = Vec::new();
    let mut starts = Vec::new();
    for (index, phase) in [(video, 0), (audio, audio_phase)] {
        let track = &reader.tracks[index];
        let scale = u64::from(track.timescale);
        let (from, to) = (from_s * scale, to_s * scale);
        let packets = reader.samples(index, from.saturating_sub(phase), to - phase)?
            .into_iter()
            .filter(|sample| (from..to).contains(&(sample.dts + phase)))
            .map(|sample| {
                let bytes = reader.read(&sample)?;
                Ok((sample, bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        let first = &packets.first().context("fixture range has no packets")?.0;
        ensure!(
            packets.windows(2).all(|pair| pair[0].0.end() == pair[1].0.dts),
            "fixture track {} is discontinuous",
            track.id
        );
        if index == video {
            ensure!(
                first.dts == from && first.sync
                    && packets.last().is_some_and(|packet| packet.0.end() == to),
                "fixture must have video keyframes at 0s and 8s, and end on a frame boundary at 16s"
            );
        }
        starts.push(first.dts + phase - from);
        tracks.push(TrackSpec {
            id: track.id,
            timescale: track.timescale,
            codec: track.codec.clone(),
            width: u16::try_from(track.width)?,
            height: u16::try_from(track.height)?,
            name: track.name.clone(),
            enabled: true,
            default_duration: first.duration,
        });
        data.push(packets);
    }
    let header = SegmentMeta {
        start_ms: Some(start_ms),
        tracks: tracks.iter().zip(["video", "full"]).map(|(track, role)| TrackMeta {
            id: track.id,
            role: role.into(),
            slot: None,
            app: None,
            name: track.name.clone(),
        }).collect(),
        ..Default::default()
    };
    let samples: Vec<Vec<_>> = data.iter().map(|packets| packets.iter().map(|(packet, bytes)| SampleRef {
        data: bytes,
        duration: packet.duration,
        sync: packet.sync,
        cts: packet.cts,
    }).collect()).collect();
    let mut writer = FragWriter::create(dest, &tracks, (start_ms / 1000) as u64, Some(&serde_json::to_vec(&header)?))?;
    for repetition in 0..repetitions {
        let runs: Vec<_> = tracks.iter().zip(&samples).zip(&starts).map(|((track, samples), base_time)| TrackRun {
            track_id: track.id,
            base_time: base_time + repetition * (to_s - from_s) * u64::from(track.timescale),
            samples,
        }).collect();
        writer.write_fragment(&runs)?;
    }
    writer.finish()?;
    Ok(())
}

fn assert_same_packets(reference: &Path, actual: &Path, seam_s: f64) -> Result<()> {
    let reference = Mp4Reader::open(reference)?;
    let actual = Mp4Reader::open(actual)?;
    ensure!(reference.tracks.len() == 2 && actual.tracks.len() == 2, "expected video and full audio");
    for (index, track) in reference.tracks.iter().enumerate() {
        let actual_track = &actual.tracks[index];
        ensure!(track.timescale == actual_track.timescale && track.codec == actual_track.codec);
        ensure!((track.edit_start_s() - actual_track.edit_start_s()).abs() < 0.000_001);
        let expected = reference.samples(index, 0, reference.end_ticks(index))?;
        let found = actual.samples(index, 0, actual.end_ticks(index))?;
        ensure!(expected.len() == found.len(), "track {}: expected {} packets, found {}", track.id, expected.len(), found.len());
        let seam = ((seam_s + track.edit_start_s()) * f64::from(track.timescale)).round() as u64;
        let at_seam = expected.partition_point(|sample| sample.dts < seam);
        ensure!(at_seam > 0 && at_seam < expected.len(), "track {} does not cross the seam", track.id);
        ensure!(
            reference.read(&expected[at_seam - 1])? != reference.read(&expected[at_seam])?,
            "track {} has indistinguishable payloads at the seam",
            track.id
        );
        ensure!(
            expected.windows(2).all(|pair| pair[0].end() == pair[1].dts),
            "continuous reference has a gap on track {}",
            track.id
        );
        for (i, (expected, found)) in expected.iter().zip(&found).enumerate() {
            ensure!(
                (expected.dts, expected.pts(), expected.duration, expected.sync)
                    == (found.dts, found.pts(), found.duration, found.sync),
                "track {} packet {i}: timing or keyframe state changed at output DTS {}",
                track.id, expected.dts
            );
            ensure!(
                reference.read(expected)? == actual.read(found)?,
                "track {} packet {i}: payload changed at output DTS {} (seam at {seam})",
                track.id, expected.dts
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "Requires the synthetic 40s MP4 and a fresh project-local clipvault-ipc-* profile"]
fn native_hourly_split_matches_continuous_capture() -> Result<()> {
    let profile = TestProfile::new()?;
    profile.shared.foreground_game.store(true, Ordering::Relaxed);
    let fixture = Mp4Reader::open(Path::new(&profile.recording.path))?;
    let folder = Path::new(&profile.recording.path).parent().context("seed recording directory")?;
    let hour = chrono::Local.with_ymd_and_hms(2026, 1, 15, 13, 0, 0).single().context("local hour")? + chrono::Duration::milliseconds(357);
    let db = Db::open_default()?;
    for (phase, seam) in [(0, hour), (256, hour + chrono::Duration::hours(1))] {
        let start = seam - chrono::Duration::seconds(SPLIT_S as i64);
        let path_at = |at: chrono::DateTime<chrono::Local>| folder.join(format!("{}.mp4", at.format("%Y-%m-%d %H-%M-%S")));
        // The aligned case spans an actual hour on disk. Only the final block
        // is in the selection, and the second file uses different packets.
        let left_duration = if phase == 0 { HOUR_S } else { SPLIT_S };
        let left_start = seam - chrono::Duration::seconds(left_duration as i64);
        let left = path_at(left_start);
        let right = path_at(seam);
        write_fragment(&fixture, &left, left_start.timestamp_millis(), 0, SPLIT_S, phase, left_duration / SPLIT_S)?;
        write_fragment(&fixture, &right, seam.timestamp_millis(), SPLIT_S, FIXTURE_S, phase, 1)?;

        let continuous = profile.dir.join(format!("hourly-continuous-{phase}.mp4"));
        write_fragment(&fixture, &continuous, start.timestamp_millis(), 0, FIXTURE_S, phase, 1)?;
        let source = Mp4Reader::open(&continuous)?;
        let video = source.video().context("continuous source has no video")?;
        for boundary in [IN_MS, OUT_MS] {
            let seconds = boundary as f64 / 1000.0;
            ensure!(source.sync_times(video)?.iter().all(|key| (key - seconds).abs() > 0.03));
        }
        let reference = profile.dir.join(format!("hourly-reference-{phase}.mp4"));
        cut(&[CutSource {
            path: continuous,
            from_s: IN_MS as f64 / 1000.0,
            to_s: Some(OUT_MS as f64 / 1000.0),
        }], &reference, Some(start.timestamp_millis() + IN_MS), &mut |_| {}, &AtomicBool::new(false))?;

        profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let rows = db.list_recordings()?;
            let indexed = [(left_start.timestamp_millis(), &left, left_duration), (seam.timestamp_millis(), &right, SPLIT_S)]
                .into_iter().all(|(ms, path, duration)| rows.iter().any(|row| {
                    Path::new(&row.path) == path && row.probed && row.start_ms == ms
                        && (row.duration - duration as f64).abs() < 0.03
                }));
            if indexed { break; }
            ensure!(Instant::now() < deadline, "hourly recordings did not index");
            thread::sleep(Duration::from_millis(25));
        }
        let job = profile.run_job(Request::CutRange {
            from_ms: start.timestamp_millis() + IN_MS,
            to_ms: start.timestamp_millis() + OUT_MS,
            title: format!("Native hourly seam, AAC phase {phase}"),
            game_id: None,
            category_ids: Vec::new(),
            mix: None,
            audio_edits: Vec::new(),
        })?;
        let output = PathBuf::from(job.output.context("missing cut output")?);
        assert_same_packets(&reference, &output, SPLIT_S as f64 - IN_MS as f64 / 1000.0)
            .with_context(|| format!("hourly seam with AAC phase {phase}"))?;
        println!("Hourly seam with AAC phase {phase}: video and audio match continuous packet payloads and timing");
    }
    profile.shutdown()
}
