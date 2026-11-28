//! Real-time Full history soak for only the synthetic window owned by this process.

use super::*;
use cv_core::util::now_ms;
use cv_media::mp4::reader::Mp4Reader;
use std::io::Write;

const BUDGET: f64 = 600.0;
const SEGMENT: f64 = 8.0; // The isolated owned-window recorder's rollover interval.

fn note(log: &mut std::fs::File, message: impl std::fmt::Display) -> Result<()> {
    let line = format!("{} {message}", chrono::Local::now().to_rfc3339());
    writeln!(log, "{line}")?;
    println!("{line}");
    Ok(())
}

fn status(profile: &TestProfile) -> Result<Status> {
    Ok(serde_json::from_value(profile.client.call(Request::Status, REQUEST_TIMEOUT)?)?)
}

#[test]
#[ignore = "Records an owned synthetic HWND for over ten real minutes in a fresh isolated QA profile"]
fn owned_window_full_history_retention_soak() -> Result<()> {
    ensure!(
        std::env::var("CLIPVAULT_QA_CAPTURE_WINDOW").as_deref() == Ok("1"),
        "Set CLIPVAULT_QA_CAPTURE_WINDOW=1 for the isolated owned-window test"
    );
    // Leave the normal regression at ten minutes. A day run keeps the same rolling disk budget.
    let requested = std::env::var("CLIPVAULT_QA_SOAK_SECONDS").ok()
        .map(|value| value.parse::<u64>()).transpose()?.unwrap_or(0);
    ensure!(requested == 0 || (600..=86_400).contains(&requested), "Choose 600 through 86400 wall-clock seconds");
    let target = requested as f64;
    let qa = std::env::current_dir()?.join(".clipvault-local").join("qa").canonicalize()?;
    let dir = cv_core::paths::data_dir();
    ensure!(dir.parent().context("QA profile parent")?.canonicalize()?.starts_with(&qa),
        "Use a new clipvault-ipc-* profile under the project's .clipvault-local/qa");
    let fixture = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("Synthetic fixture needed for IPC setup")?);
    ensure!(fixture.canonicalize()?.starts_with(&qa), "Use a synthetic fixture under .clipvault-local/qa");

    // Shut down the engine before the owned window is destroyed, including on error.
    let mut window = None;
    let mut profile = TestProfile::new()?;
    let mut log = std::fs::File::create(profile.dir.join("full-history-soak.txt"))?;
    let db = Db::open_default()?;
    // TestProfile uses a seed to validate initial IPC indexing. Empty the archive
    // before arming so only newly recorded footage contributes to the budget.
    std::fs::remove_file(&profile.recording.path)?;
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    while !db.list_recordings()?.is_empty() {
        ensure!(Instant::now() < deadline, "The setup fixture is still indexed");
        thread::sleep(Duration::from_millis(50));
    }

    let mut cfg = profile.shared.cfg();
    cfg.recording.enabled = true;
    cfg.recording.history_mode = HistoryMode::Archive;
    cfg.recording.set_history_minutes(10);
    cfg.recording.max_height = 360;
    cfg.recording.fps = 20;
    cfg.recording.keyframe_interval_secs = 1;
    cfg.recording.encoder = "software".into();
    cfg.recording.app_tracks = false;
    cfg.recording.show_cursor = false;
    cfg.audio.record_mic = false;
    cfg.audio.mic_guard = false;
    cfg.save()?;
    let folder = PathBuf::from(&cfg.recording.folder);
    *profile.shared.cfg.write() = cfg;

    let source = SyntheticWindow::open()?;
    let hwnd = source.hwnd;
    window.replace(source);
    let rx = profile.engine_rx.take().context("Owned-window engine channel")?;
    let shared = profile.shared.clone();
    let armed_wall = now_ms();
    let armed = Instant::now();
    note(&mut log, format_args!("Armed native WGC hwnd={hwnd:#x}; budget=600s, segment=8s, requested wall time={requested}s, mic and loopback disabled"))?;
    profile.engine = Some(spawn("retention-soak-engine", move || engine::run_test_window(shared, rx, hwnd)));

    let mut kept = None;
    let mut saved = None;
    let mut expiring = None;
    let mut following = None;
    let mut expired = None;
    let mut next_report = 0.0;
    let mut last_written = 0;
    let mut last_progress = armed;
    let mut last_rollover = 0;
    let mut rollovers = 0_u64;
    let mut peak_drops = 0;
    loop {
        let s = status(&profile)?;
        let rows = db.list_recordings()?;
        let elapsed = armed.elapsed().as_secs_f64();
        while profile.events.try_recv().is_ok() {}
        ensure!(elapsed < target.max(BUDGET) + 80.0, "Soak did not converge after {elapsed:.1}s; engine={:?}", s.engine);
        if !s.recording.active || s.recording.written_until_ms <= s.recording.started_ms {
            ensure!(elapsed < 25.0 || s.recording.active,
                "Native WGC recorder is inactive at {elapsed:.1}s; engine={:?}", s.engine);
            thread::sleep(Duration::from_millis(150));
            continue;
        }
        if s.recording.written_until_ms > last_written {
            last_progress = Instant::now();
            last_written = s.recording.written_until_ms;
        }
        ensure!(last_progress.elapsed() < Duration::from_secs(30), "Recorder stopped advancing its durable output at {elapsed:.2}s");
        if s.recording.started_ms != last_rollover {
            rollovers += u64::from(last_rollover != 0);
            last_rollover = s.recording.started_ms;
        }
        peak_drops = peak_drops.max(s.engine.dropped_frames);
        ensure!(s.engine.capture == "Windows Graphics Capture" && s.mic.state == "disabled",
            "Unexpected capture path or microphone state: {} / {}", s.engine.capture, s.mic.state);
        let ordinary: Vec<_> = rows.iter().filter(|r| r.probed && !r.is_live() && !r.kept).collect();
        let total: f64 = ordinary.iter().map(|r| r.duration).sum();

        if elapsed >= next_report {
            note(&mut log, format_args!(
                "Elapsed {elapsed:.2}s; {} ordinary finalized segments ({total:.3}s), live durable {:.3}s, written_until_ms={}, rollovers={rollovers}, peak reported drops={peak_drops}",
                ordinary.len(), (s.recording.written_until_ms - s.recording.started_ms) as f64 / 1000.0,
                s.recording.written_until_ms,
            ))?;
            if elapsed >= BUDGET + 60.0 {
                ensure!((BUDGET - 2.0 * SEGMENT..=BUDGET + 2.0 * SEGMENT).contains(&total),
                    "Rolling archive had {total:.3}s after {elapsed:.2}s");
            }
            next_report += 60.0;
        }

        if kept.is_none() {
            if let Some(first) = ordinary.first() {
                let original = std::fs::read(&first.path)?;
                profile.client.call(Request::KeepRecording { recording_id: first.id, keep: true }, REQUEST_TIMEOUT)?;
                let row = db.recording(first.id)?.context("Kept recording catalog row")?;
                let path = PathBuf::from(&row.path);
                ensure!(row.kept && path.parent() == Some(folder.join(crate::library::KEPT_DIR).as_path()));
                ensure!((first.duration - SEGMENT).abs() < 0.25, "First segment duration was {:.3}s", first.duration);
                ensure!(first.start_ms >= armed_wall, "Kept footage predates recorder launch");
                kept = Some((row, original));
                note(&mut log, format_args!("Kept first native recording, start_ms={}; elapsed={elapsed:.2}s", first.start_ms))?;
            }
        } else if saved.is_none() {
            if let Some(second) = ordinary.first() {
                let job = profile.run_job(Request::CutRange {
                    from_ms: second.start_ms + 1_000,
                    to_ms: second.start_ms + 5_000,
                    title: "Saved from the expiring live soak recording".into(),
                    game_id: None,
                    category_ids: Vec::new(),
                    mix: None,
                    audio_edits: Vec::new(),
                })?;
                let path = PathBuf::from(job.output.context("Saved clip path")?);
                let bytes = std::fs::read(&path)?;
                let id = job.clip_id.context("Saved clip ID")?;
                saved = Some((id, path, bytes));
                expiring = Some((*second).clone());
                note(&mut log, format_args!("Saved a 4s clip from oldest ordinary start_ms={}", second.start_ms))?;
            }
        } else if let Some(first) = &expiring {
            if following.is_none() {
                following = ordinary.iter().find(|r| r.start_ms > first.start_ms).map(|r| (**r).clone());
            }
            if expired.is_none() && db.recording(first.id)?.is_none() && !Path::new(&first.path).exists() {
                ensure!(elapsed >= BUDGET && elapsed <= BUDGET + 3.0 * SEGMENT,
                    "Oldest ordinary segment expired at {elapsed:.2}s outside the ten-minute budget and segment slack");
                ensure!(first.end_ms() + 600_000 <= s.recording.written_until_ms + 1_000,
                    "The oldest recording expired before the recorder made ten minutes newer footage durable");
                note(&mut log, format_args!(
                    "First ordinary expiry at {elapsed:.2}s; removed start_ms={}, end_ms={}; {} finalized ordinary segments ({total:.3}s)",
                    first.start_ms, first.end_ms(), ordinary.len(),
                ))?;
                expired = Some(s.recording.started_ms);
            }
            if let (Some(first_expiry_live_start), Some(next)) = (expired, &following) {
                if elapsed >= target && s.recording.started_ms > first_expiry_live_start
                    && db.recording(next.id)?.is_none() && !Path::new(&next.path).exists()
                {
                    ensure!(total >= BUDGET - SEGMENT - 0.25 && total < BUDGET + SEGMENT + 0.25,
                        "Finalized ordinary footage {total:.3}s exceeds the segment slack");
                    let durable = total + (s.recording.written_until_ms - s.recording.started_ms) as f64 / 1000.0;
                    ensure!((BUDGET - 0.5..BUDGET + SEGMENT + 0.5).contains(&durable),
                        "Regular footage with durable live tail was {durable:.3}s");
                    let (row, bytes) = kept.as_ref().context("Missing kept recording")?;
                    ensure!(db.recording(row.id)?.is_some_and(|r| r.kept && r.path == row.path)
                        && std::fs::read(&row.path)? == *bytes, "Kept recording changed");
                    let (id, path, bytes) = saved.as_ref().context("Missing saved clip")?;
                    ensure!(db.clip(*id)?.is_some_and(|clip| Path::new(&clip.path) == path)
                        && std::fs::read(path)? == *bytes, "Saved clip changed");
                    ensure!((cv_media::probe::probe(path)?.duration - 4.0).abs() < 0.1);
                    let reader = Mp4Reader::open(Path::new(&row.path))?;
                    ensure!(reader.is_fragmented() && reader.audio().len() == 2);
                    for track in reader.audio() {
                        let pcm = cv_media::audioread::read_range(&reader, track, 0.25, 1.25)?;
                        ensure!(!pcm.is_empty() && pcm.iter().all(|v| v.abs() < 0.001),
                            "Synthetic WGC recording contains unexpected audio");
                    }
                    let initial = cv_media::thumb::keyframe_at(&reader, 1.05)?;
                    let later = cv_media::thumb::keyframe_at(&reader, 3.05)?;
                    let changed = initial.nv12.iter().zip(later.nv12)
                        .filter(|(a, b)| a.abs_diff(*b) > 25).count();
                    ensure!(changed > initial.nv12.len() / 20, "The owned window did not visibly animate");
                    profile.shutdown()?;
                    note(&mut log, format_args!(
                        "PASS after {elapsed:.2}s with continued rollover and another ordinary expiry. {} finalized ordinary segments: {total:.3}s; durable total {durable:.3}s against 600s budget plus one 8s segment. Kept recording and 4s saved clip unchanged. Both audio tracks silent; {changed} image samples changed.",
                        ordinary.len(),
                    ))?;
                    return Ok(());
                }
            }
        }

        thread::sleep(Duration::from_millis(if elapsed > 650.0 { 750 } else { 150 }));
    }
}
