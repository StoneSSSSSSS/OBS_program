//! Exercise the minute-based Full history budget through the archive worker.

use super::*;
use chrono::TimeZone;
use cv_media::mp4::reader::Mp4Reader;
use std::sync::atomic::Ordering;

const SEGMENT_MS: i64 = 40_000;

fn wait_for_archive(db: &Db, starts: &[i64]) -> Result<Vec<Recording>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = db.list_recordings()?;
        if rows.len() == starts.len()
            && rows.iter().zip(starts).all(|(row, start)| {
                row.start_ms == *start && row.probed && !row.is_live()
                    && (row.duration - 40.0).abs() < 0.001
            })
        {
            return Ok(rows);
        }
        ensure!(
            Instant::now() < deadline,
            "Archive did not converge to the expected recording starts {starts:?}: {:?}",
            rows.iter().map(|row| (row.start_ms, row.duration, row.kept)).collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "Requires the native 40s synthetic MP4 and a fresh project-local clipvault-ipc-* profile"]
fn ten_minute_full_history_rotates_after_reload() -> Result<()> {
    let profile = TestProfile::new()?;
    profile.shared.foreground_game.store(true, Ordering::Relaxed);
    let seed = Path::new(&profile.recording.path);
    let folder = seed.parent().context("Seed recording directory")?;
    let probe = cv_media::probe::probe(seed)?;
    ensure!(
        probe.fragmented && probe.start_ms.is_some() && (probe.duration - 40.0).abs() < 0.001,
        "This test needs a native fragmented ClipVault fixture of exactly 40 seconds"
    );
    let template = profile.dir.join("short-history-template.mp4");
    std::fs::rename(seed, &template)?;
    let fixture = Mp4Reader::open(&template)?;

    // With one kept recording and fifteen standard ones, the regular archive
    // totals exactly 10 minutes. The next 40-second recording tips it over.
    let start = chrono::Local.with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
        .single().context("Local noon")?.timestamp_millis();
    let starts: Vec<_> = (0..17).map(|i| start + i * SEGMENT_MS).collect();
    let segment = |start| folder.join(cv_media::recorder::segment_name(start));
    for &start in &starts[..16] {
        super::hourly_split::write_fragment(&fixture, &segment(start), start, 0, 40, 0, 1)?;
    }
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let db = Db::open_default()?;
    let rows = wait_for_archive(&db, &starts[..16])?;
    let kept_id = rows[0].id;
    let expiring_id = rows[1].id;
    let kept_bytes = std::fs::read(segment(starts[0]))?;
    profile.client.call(Request::KeepRecording { recording_id: kept_id, keep: true }, REQUEST_TIMEOUT)?;
    let kept = PathBuf::from(db.recording(kept_id)?.context("missing kept recording")?.path);
    ensure!(kept.parent() == Some(folder.join(crate::library::KEPT_DIR).as_path()));

    // Save a clip from the recording about to expire, then verify that archive
    // rotation does not change the saved clip's catalog entry or file content.
    let job = profile.run_job(Request::CutRange {
        from_ms: starts[1] + 4_000,
        to_ms: starts[1] + 14_000,
        title: "Saved from an expiring archive recording".into(),
        game_id: None,
        category_ids: Vec::new(),
        mix: None,
        audio_edits: Vec::new(),
    })?;
    let clip_id = job.clip_id.context("missing saved clip ID")?;
    let saved_path = PathBuf::from(job.output.context("missing saved clip path")?);
    let saved_bytes = std::fs::read(&saved_path)?;

    let mut cfg = profile.shared.cfg();
    ensure!(cfg.recording.history_minutes() > 10);
    cfg.recording.history_mode = HistoryMode::Archive;
    cfg.recording.set_history_minutes(10);
    cfg.save()?;
    profile.client.call(Request::ReloadConfig { restart_recorder: false }, REQUEST_TIMEOUT)?;
    ensure!(profile.shared.cfg().recording.history_minutes() == 10);
    ensure!(profile.shared.cfg().recording.rolling_seconds().is_none());
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let exact_budget = wait_for_archive(&db, &starts[..16])?;
    ensure!(exact_budget[0].kept && exact_budget[1..].iter().all(|row| !row.kept));

    super::hourly_split::write_fragment(&fixture, &segment(starts[16]), starts[16], 0, 40, 0, 1)?;
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let retained_starts = [&starts[..1], &starts[2..]].concat();
    let remaining = wait_for_archive(&db, &retained_starts)?;
    ensure!(remaining[0].kept && remaining[0].id == kept_id);
    ensure!(remaining[1..].iter().all(|row| !row.kept));
    ensure!(db.recording(expiring_id)?.is_none() && !segment(starts[1]).exists());
    ensure!(starts[2..].iter().all(|&start| segment(start).is_file()));
    ensure!(std::fs::read(&kept)? == kept_bytes);
    ensure!(std::fs::read(&saved_path)? == saved_bytes);
    ensure!((cv_media::probe::probe(&saved_path)?.duration - 10.0).abs() < 0.1);

    profile.shutdown()?;
    drop(db);
    drop(profile);
    let reopened = Db::open_default()?;
    ensure!(Config::load()?.recording.history_minutes() == 10);
    let rows = wait_for_archive(&reopened, &retained_starts)?;
    ensure!(rows[0].kept && rows[0].id == kept_id);
    ensure!(reopened.clip(clip_id)?.is_some_and(|clip| Path::new(&clip.path) == saved_path));
    println!("Full history reload kept 600 seconds of regular archive, one kept recording and the saved clip");
    Ok(())
}
