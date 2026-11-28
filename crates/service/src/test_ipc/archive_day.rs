//! Accelerated archive coverage. Each path is a playable, full-duration segment.

use super::*;
use chrono::TimeZone;
use cv_media::mp4::frag::{FragWriter, TrackRun};
use cv_media::mp4::reader::Mp4Reader;
use cv_media::mp4::{SampleRef, TrackSpec};
use cv_media::recorder::{parse_meta, SegmentMeta};
use std::sync::atomic::Ordering;

const HALF_HOUR_MS: i64 = 1_800_000;
const DAY_SEGMENTS: usize = 48;

/// Remux actual H.264 and AAC samples from the generated smoke fixture. Eight
/// seconds is a shared boundary for its 25 fps video and 48 kHz AAC frames.
fn write_native_segment(source: &Path, dest: &Path, start_ms: i64) -> Result<()> {
    const LOOP_SECONDS: u64 = 8;
    const SEGMENT_SECONDS: u64 = HALF_HOUR_MS as u64 / 1000;

    let reader = Mp4Reader::open(source)?;
    let meta = parse_meta(&reader.metas);
    let beginning = meta.iter().find(|meta| meta.at_s == 0.0 && !meta.tracks.is_empty())
        .context("Generated source lacks native ClipVault track metadata")?;
    let header = SegmentMeta { start_ms: Some(start_ms), ..beginning.clone() };
    let changes: Vec<_> = meta.iter().filter(|meta| meta.at_s > 0.0 && meta.at_s < LOOP_SECONDS as f64).collect();
    let mut tracks = Vec::new();
    let mut data = Vec::new();
    for (index, track) in reader.tracks.iter().enumerate() {
        let until = u64::from(track.timescale) * LOOP_SECONDS;
        let samples = reader.samples(index, 0, until)?;
        let first = samples.first().context("Generated source track has no samples")?;
        ensure!(
            first.dts == 0 && (!track.codec.is_video() || first.sync)
                && samples.last().is_some_and(|last| last.end() == until)
                && samples.windows(2).all(|pair| pair[0].end() == pair[1].dts),
            "Generated source track {} is not continuous through the 8-second sample boundary", track.id
        );
        tracks.push(TrackSpec {
            id: track.id,
            timescale: track.timescale,
            codec: track.codec.clone(),
            width: u16::try_from(track.width)?,
            height: u16::try_from(track.height)?,
            name: track.name.clone(),
            enabled: track.enabled,
            default_duration: first.duration,
        });
        let packets = samples.into_iter().map(|sample| {
            let bytes = reader.read(&sample)?;
            Ok((sample, bytes))
        }).collect::<Result<Vec<_>>>()?;
        data.push(packets);
    }
    let samples: Vec<Vec<_>> = data.iter().map(|packets| packets.iter().map(|(sample, bytes)| SampleRef {
        data: bytes, duration: sample.duration, sync: sample.sync, cts: sample.cts,
    }).collect()).collect();

    let mut writer = FragWriter::create(dest, &tracks, start_ms as u64 / 1000, Some(&serde_json::to_vec(&header)?))?;
    for loop_start in (0..SEGMENT_SECONDS).step_by(LOOP_SECONDS as usize) {
        if loop_start != 0 && !changes.is_empty() {
            writer.write_meta(&serde_json::to_vec(&SegmentMeta {
                at_s: loop_start as f64, slots: beginning.slots.clone(), ..Default::default()
            })?)?;
        }
        for change in &changes {
            writer.write_meta(&serde_json::to_vec(&SegmentMeta {
                at_s: loop_start as f64 + change.at_s, slots: change.slots.clone(), ..Default::default()
            })?)?;
        }
        let runs = tracks.iter().zip(&samples).map(|(track, frames)| TrackRun {
            track_id: track.id, base_time: loop_start * u64::from(track.timescale), samples: frames,
        }).collect::<Vec<_>>();
        writer.write_fragment(&runs)?;
    }
    writer.finish()?;

    let output = Mp4Reader::open(dest)?;
    let info = cv_media::probe::probe_reader(&output);
    ensure!(info.fragmented && info.start_ms == Some(start_ms) && (info.duration - 1800.0).abs() < 0.0001);
    for (index, track) in output.tracks.iter().enumerate() {
        ensure!(output.coverage(index) == [(0, SEGMENT_SECONDS * u64::from(track.timescale))]);
    }
    Ok(())
}

fn wait_for_index(db: &Db, starts: &[i64]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let mut rows = db.list_recordings()?;
        rows.sort_by_key(|row| row.start_ms);
        if rows.len() == starts.len()
            && rows.iter().zip(starts).all(|(r, t)| {
                r.probed && !r.is_live() && r.start_ms == *t && (r.duration - 1800.0).abs() < 0.1
            })
        {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "Archive index did not converge; {} segments indexed", rows.len());
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "Accelerated 24h archive with synthetic media, a fresh profile, NTFS hard links and loopback"]
fn a_full_day_of_archive_survives_rotation_and_reopening() -> Result<()> {
    let profile = TestProfile::new()?;
    // Pause archive waveform backfill while indexing, as during foreground gameplay.
    profile.shared.foreground_game.store(true, Ordering::Relaxed);
    let saved_clip = profile.cut_sample()?;
    let seed = Path::new(&profile.recording.path);
    let folder = seed.parent().context("Seed recording directory")?;
    let noon = chrono::Local.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).single().context("Local noon")?.timestamp_millis();
    let template = profile.dir.join("half-hour-template.mp4");
    write_native_segment(seed, &template, noon)?;
    let preview = profile.dir.join("last-second.jpg");
    cv_media::thumb::thumbnail(&template, 1799.5, &preview, 160)?;
    ensure!(preview.is_file(), "Could not decode the final second of generated media");
    // The seed is a copy owned by this freshly created profile. The input fixture is untouched.
    std::fs::remove_file(seed)?;
    let starts: Vec<_> = (0..=DAY_SEGMENTS).map(|i| noon + i as i64 * HALF_HOUR_MS).collect();
    let segment = |start| folder.join(cv_media::recorder::segment_name(start));
    // Each link represents a different archive time, which the library reads
    // from its filename. The shared data and native header are immutable.
    for &start in &starts[..DAY_SEGMENTS] {
        std::fs::hard_link(&template, segment(start))?;
    }
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let db = Db::open_default()?;
    wait_for_index(&db, &starts[..DAY_SEGMENTS])?;
    let midnight = noon + 12 * 3_600_000;
    let mut output_paths = Vec::new();
    for (name, from_ms, to_ms) in [
        ("Across midnight", midnight - 10_000, midnight + 10_000),
        ("Last thirty seconds", starts[DAY_SEGMENTS] - 30_000, starts[DAY_SEGMENTS]),
    ] {
        let job = profile.run_job(Request::CutRange {
            from_ms, to_ms, title: name.into(), game_id: None,
            category_ids: vec![], mix: None, audio_edits: vec![],
        })?;
        let path = job.output.context("Missing cut output")?;
        let info = cv_media::probe::probe(Path::new(&path))?;
        ensure!((info.duration - (to_ms-from_ms) as f64 / 1000.0).abs() < 0.1 && !info.audio.is_empty());
        ensure!(info.start_ms == Some(from_ms));
        println!("{name}: {:.3}s, {} audio tracks", info.duration, info.audio.len());
        output_paths.push(path);
    }
    let mut cfg = profile.shared.cfg();
    cfg.recording.retention_hours = 24;
    cfg.save()?;
    profile.client.call(Request::ReloadConfig { restart_recorder: false }, REQUEST_TIMEOUT)?;
    std::fs::hard_link(&template, segment(starts[DAY_SEGMENTS]))?;
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    wait_for_index(&db, &starts[1..])?;
    ensure!(!segment(noon).exists() && starts[1..].iter().all(|&t| segment(t).is_file()));
    let dir = profile.dir.clone();
    profile.shutdown()?;
    drop(db);
    drop(profile);
    let reopened = Db::open_default()?;
    wait_for_index(&reopened, &starts[1..])?;
    ensure!(reopened.clip(saved_clip.id)?.is_some() && Path::new(&saved_clip.path).is_file());
    ensure!(output_paths.iter().all(|path| Path::new(path).is_file()));
    println!("Verified 48 half-hour segments, midnight cut, rotation at hour 24.5 and persistent library in {}", dir.display());
    Ok(())
}
