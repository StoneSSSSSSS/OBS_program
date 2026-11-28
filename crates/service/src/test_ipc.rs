//! Opt-in IPC checks against the actual library and background jobs. Both tests
//! require a new ClipVault-specific data directory and explicit synthetic media.

use super::*;
use crate::shared::Footage;
use anyhow::{ensure, Context};
use cv_core::config::HistoryMode;
use cv_core::db::Db;
use cv_core::ipc::{Event, ExportSource, IpcClient, JobInfo, Request, Status};
use cv_core::mixer::MixSettings;
use cv_core::model::{AudioEdit, Clip, Recording};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const JOB_TIMEOUT: Duration = Duration::from_secs(120);

struct TestProfile {
    dir: PathBuf,
    shared: Arc<Shared>,
    client: IpcClient,
    events: crossbeam_channel::Receiver<Event>,
    recording: Recording,
    engine_rx: Option<crossbeam_channel::Receiver<EngineCmd>>,
    engine: Option<thread::JoinHandle<()>>,
    library: Option<thread::JoinHandle<()>>,
    status: Option<thread::JoinHandle<()>>,
}

pub(crate) fn isolate_config(mut config: Config, dir: &Path) -> Config {
    config.general.start_with_windows = false;
    config.general.show_tray_icon = false;
    config.general.notifications = false;
    config.recording.enabled = false;
    config.recording.folder = dir.join("recordings").to_string_lossy().into_owned();
    config.recording.min_free_space_gb = 0;
    config.recording.max_storage_gb = 0;
    config.recording.max_age_days = 0;
    config.clips.folder = dir.join("clips").to_string_lossy().into_owned();
    config.clips.delete_to_recycle_bin = false;
    config.export.folder = dir.join("exports").to_string_lossy().into_owned();
    config.export.open_folder_after = false;
    config.export.copy_to_clipboard = false;
    config.audio.record_mic = false;
    config
}

impl TestProfile {
    fn new(rolling_test: bool) -> Result<Self> {
        let dir = cv_core::paths::data_dir();
        let explicit = std::env::var_os("CLIPVAULT_DATA_DIR").map(PathBuf::from);
        ensure!(
            explicit.as_ref() == Some(&dir)
                && dir.is_absolute()
                && dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("clipvault-ipc-")),
            "Set CLIPVAULT_DATA_DIR to a new absolute directory named clipvault-ipc-*"
        );
        let fixture = std::env::var_os("CLIPVAULT_TEST_FIXTURE")
            .map(PathBuf::from)
            .context("Set CLIPVAULT_TEST_FIXTURE to the synthetic 40-second MP4")?;
        let fixture_info =
            cv_media::probe::probe(&fixture).context("Opening the synthetic fixture")?;
        ensure!(
            fixture_info.duration >= 30.0 && !fixture_info.audio.is_empty(),
            "The fixture must contain at least 30 seconds of video with audio"
        );
        // Exclusive creation prevents overwriting anything in an existing profile.
        std::fs::create_dir(&dir)
            .context("The isolated profile directory must be new and its parent must exist")?;
        cv_core::paths::ensure_dirs()?;

        let config = isolate_config(Config::default(), &dir);
        config.save()?;
        std::fs::create_dir(&config.recording.folder)?;
        std::fs::create_dir(&config.clips.folder)?;
        let start = chrono::Local::now() - chrono::Duration::minutes(2);
        let seed = PathBuf::from(&config.recording.folder)
            .join(format!("{}.mp4", start.format("%Y-%m-%d %H-%M-%S")));
        std::fs::copy(&fixture, &seed)?;
        let start_ms =
            cv_core::filenames::parse_timestamp_ms(&seed).context("fixture timestamp")?;

        // Let the real library worker discover and index the file.
        let db = Db::open_default()?;
        let game = db.upsert_game("synthetic.exe", "Synthetic test game", "", true, start_ms)?;
        let activity = db.start_activity(
            "synthetic.exe",
            Some(game),
            true,
            "Synthetic gameplay",
            start_ms,
        )?;
        db.extend_activity(
            activity,
            start_ms + (fixture_info.duration * 1000.0) as i64,
            None,
        )?;
        db.add_marker(start_ms + 9_000, "Synthetic highlight")?;
        let (ui_handle, _ui_rx) = ui::channel();
        let (engine_tx, engine_rx) = crossbeam_channel::unbounded();
        let (clip_tx, clip_rx) = crossbeam_channel::unbounded();
        let (job_tx, job_rx) = crossbeam_channel::unbounded();
        let (lib_tx, lib_rx) = crossbeam_channel::unbounded();
        let (audio_tx, _audio_rx) = crossbeam_channel::bounded(1);
        let shared = Arc::new(Shared {
            cfg: Arc::new(RwLock::new(config)),
            isolated_test: true,
            status: Mutex::new(Status {
                service_started_ms: cv_core::util::now_ms(),
                engine: cv_core::ipc::EngineStatus {
                    last_error: "Capture disabled in the isolated test profile".into(),
                    ..Default::default()
                },
                recording: cv_core::ipc::RecStatus {
                    paused_by_user: true,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ui: ui_handle,
            engine_tx,
            clip_tx,
            job_tx,
            lib_tx,
            audio_tx,
            clients: Mutex::new(Vec::new()),
            hotkey_errors: Arc::new(Mutex::new(Vec::new())),
            devices: Mutex::new(Default::default()),
            jobs: Mutex::new(Vec::new()),
            event_db: Mutex::new(None),
            foreground_game: Default::default(),
            library_lock: Mutex::new(()),
            live_path: Mutex::new(None),
            shutdown: std::sync::atomic::AtomicBool::new(false),
        });
        let info = ipc_server::start(shared.clone())?;
        ensure!(
            IpcClient::connect_to(info.port, "wrong-token", REQUEST_TIMEOUT).is_err(),
            "loopback service accepted an invalid token"
        );
        let (client, events) = IpcClient::connect_to(info.port, &info.token, REQUEST_TIMEOUT)?;
        let library = spawn("isolated-library", {
            let shared = shared.clone();
            move || library::run(shared, lib_rx)
        });
        let status = spawn("isolated-status", {
            let shared = shared.clone();
            move || status::run(shared)
        });
        spawn("isolated-jobs", {
            let shared = shared.clone();
            move || jobs::run(shared, job_rx)
        });
        spawn("isolated-clips", {
            let shared = shared.clone();
            move || clips::run(shared, clip_rx)
        });
        spawn("isolated-waves", {
            let shared = shared.clone();
            move || waves::run(shared)
        });

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let recording = loop {
            if let Some(recording) = db.list_recordings()?.into_iter().find(|r| r.probed) {
                break recording;
            }
            ensure!(
                Instant::now() < deadline,
                "Service library did not probe the seed recording"
            );
            thread::sleep(Duration::from_millis(25));
        };
        ensure!(
            (recording.duration - fixture_info.duration).abs() < 0.25,
            "Indexed duration differs from the source"
        );
        ensure!(
            !recording.is_live(),
            "Synthetic footage must be fully written"
        );
        Ok(Self {
            dir,
            shared,
            client,
            events,
            recording,
            engine_rx: rolling_test.then_some(engine_rx),
            engine: None,
            library: Some(library),
            status: Some(status),
        })
    }

    fn run_job(&self, request: Request) -> Result<JobInfo> {
        let id = self.client.call(request, REQUEST_TIMEOUT)?["job_id"]
            .as_u64()
            .context("missing job ID")?;
        let deadline = Instant::now() + JOB_TIMEOUT;
        loop {
            let event = self
                .events
                .recv_deadline(deadline)
                .context("job did not finish in time")?;
            if let Event::Job { job } = event {
                if job.id == id && job.state != "queued" && job.state != "running" {
                    ensure!(
                        job.state == "done",
                        "Job {} {}: {}",
                        job.kind,
                        job.state,
                        job.message
                    );
                    return Ok(job);
                }
            }
        }
    }

    fn cut_sample(&self) -> Result<Clip> {
        let source_start = self.recording.start_ms;
        let from_ms = source_start + 4_000;
        let to_ms = source_start + 24_000;
        let db = Db::open_default()?;
        let category = db.create_category("Highlights", "#7298ed")?;
        let job = self.run_job(Request::CutRange {
            from_ms,
            to_ms,
            title: "Synthetic highlight".into(),
            game_id: None,
            category_ids: vec![category],
            mix: Some(MixSettings::default()),
            audio_edits: vec![AudioEdit::new(
                "mic",
                (from_ms + 3_000) as f64 / 1000.0,
                (from_ms + 6_000) as f64 / 1000.0,
                0.0,
            )],
        })?;
        let id = job.clip_id.context("cut returned no clip ID")?;
        let clip = db.clip(id)?.context("cut is missing from the library")?;
        let path = Path::new(&clip.path);
        ensure!(
            path.is_file() && job.output.as_deref() == Some(clip.path.as_str()),
            "cut file and library path differ"
        );
        let info = cv_media::probe::probe(path)?;
        ensure!(
            (info.duration - 20.0).abs() < 0.3 && clip.duration > 19.7,
            "cut duration: {}",
            info.duration
        );
        ensure!(info.audio.len() > 1, "cut lost separate audio tracks");
        ensure!(
            clip.source_start_ms == Some(from_ms) && info.start_ms == Some(from_ms),
            "cut lost source timestamp"
        );
        ensure!(
            clip.category_ids == [category] && clip.audio_edits.len() == 1 && clip.mixer.is_some(),
            "cut lost edit metadata"
        );
        println!(
            "CutRange {}: {:.2}s, {} audio tracks, clip id {}",
            path.display(),
            info.duration,
            info.audio.len(),
            id
        );
        Ok(clip)
    }

    fn shutdown(&self) -> Result<()> {
        self.client.call(Request::Shutdown, REQUEST_TIMEOUT)?;
        Ok(())
    }
}

impl Drop for TestProfile {
    fn drop(&mut self) {
        request_shutdown(&self.shared);
        let _ = self.shared.lib_tx.send(LibCmd::Rescan);
        if let Some(worker) = self.engine.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.library.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.status.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(cv_core::paths::ipc_info_path());
    }
}

#[test]
#[ignore = "Requires an explicit synthetic MP4 and fresh test profile directory"]
fn cut_trim_and_export_over_ipc() -> Result<()> {
    let profile = TestProfile::new(false)?;
    let clip = profile.cut_sample()?;
    let replacement = profile.run_job(Request::TrimClip {
        clip_id: clip.id,
        start_s: 2.0,
        end_s: 16.0,
        replace: true,
    })?;
    ensure!(
        replacement.clip_id == Some(clip.id) && replacement.output.as_deref() == Some(&clip.path),
        "trim replaced the clip ID or path"
    );
    let trimmed = Db::open_default()?
        .clip(clip.id)?
        .context("trim removed the clip")?;
    ensure!(
        (trimmed.duration - 14.0).abs() < 0.3,
        "trim duration: {}",
        trimmed.duration
    );
    ensure!(
        trimmed.source_start_ms == clip.source_start_ms.map(|ms| ms + 2_000),
        "trim lost source timestamp"
    );
    ensure!(
        trimmed.audio_edits.len() == 1 && (trimmed.audio_edits[0].start - 1.0).abs() < 0.25,
        "trim did not rebase the edit"
    );
    let exported = profile.run_job(Request::Export {
        source: ExportSource::Clip { clip_id: clip.id },
        range: Some((1.0, 9.0)),
        mix: MixSettings::default(),
        edits: Some(Vec::new()),
        target_size_mb: Some(1),
        title: "Synthetic share".into(),
    })?;
    let output = PathBuf::from(exported.output.context("export returned no output path")?);
    ensure!(
        output.starts_with(profile.dir.join("exports")),
        "export location escaped the test profile"
    );
    let size = output.metadata()?.len();
    let info = cv_media::probe::probe(&output)?;
    ensure!(size <= 1024 * 1024 && size > 0, "export size: {size} bytes");
    ensure!(
        (info.duration - 8.0).abs() < 0.3 && info.audio.len() == 1,
        "export should have an 8-second picture and one mixed audio track"
    );
    println!(
        "Export {}: {:.2}s, {} mixed audio track, {size} bytes",
        output.display(),
        info.duration,
        info.audio.len()
    );
    profile.shutdown()?;
    println!("Isolated IPC checks passed in {}", profile.dir.display());
    Ok(())
}

#[test]
#[ignore = "Opt-in service for GUI testing, stops on IPC Shutdown or after ten minutes"]
fn serve_isolated_profile() -> Result<()> {
    let profile = TestProfile::new(false)?;
    profile.cut_sample()?;
    println!(
        "Ready for isolated GUI testing in {}",
        profile.dir.display()
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    while !profile.shared.is_shutting_down() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[test]
#[ignore = "Requires synthetic media, fresh workspace profile, and loopback access"]
fn save_rolling_fixture_over_ipc() -> Result<()> {
    let mut profile = TestProfile::new(true)?;
    let archived_clip = profile.cut_sample()?;
    let mut config = profile.shared.cfg();
    config.recording.history_mode = HistoryMode::Rolling;
    config.recording.rolling_minutes = 1;
    config.clips.save_cooldown_ms = 0;

    let rolling = profile.dir.join("rolling");
    std::fs::create_dir(&rolling)?;
    let template = profile.dir.join("segment-template.mp4");
    let source = cv_media::cut::CutSource {
        path: PathBuf::from(&profile.recording.path),
        from_s: 0.0,
        to_s: Some(10.0),
    };
    cv_media::cut::cut(
        &[source],
        &template,
        None,
        &mut |_| {},
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    let end_ms = cv_core::util::now_ms();
    let mut footage = Vec::new();
    for seconds_before_end in (1..=9).rev().map(|index| index * 10) {
        let start_ms = end_ms - seconds_before_end * 1000;
        let path = rolling.join(cv_media::recorder::segment_name(start_ms));
        std::fs::copy(&template, &path)?;
        let wave = cv_core::waveform::wave_path(&path);
        std::fs::create_dir_all(wave.parent().context("wave directory")?)?;
        std::fs::write(&wave, b"wave sidecar fixture")?;
        footage.push(Footage {
            path,
            start_ms,
            duration_s: 10.0,
        });
    }
    let expired = footage[0].path.clone();
    let recent = footage.last().unwrap().path.clone();
    let rx = profile
        .engine_rx
        .take()
        .context("isolated engine channel")?;
    let shared = profile.shared.clone();
    profile.engine = Some(spawn("fixture-replay-engine", move || {
        engine::run_rolling_fixture(shared, rx, footage, end_ms)
    }));

    config.save()?;
    profile.client.call(
        Request::ReloadConfig {
            restart_recorder: false,
        },
        REQUEST_TIMEOUT,
    )?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        let status: Status =
            serde_json::from_value(profile.client.call(Request::Status, REQUEST_TIMEOUT)?)?;
        if status.replay.rolling_minutes == Some(1) {
            break;
        }
        ensure!(Instant::now() < deadline, "Replay mode did not reload");
        thread::sleep(Duration::from_millis(25));
    }
    ensure!(
        !expired.exists() && !cv_core::waveform::wave_path(&expired).exists(),
        "Expired segment or its sidecar still exists"
    );
    ensure!(
        recent.exists() && cv_core::waveform::wave_path(&recent).exists(),
        "Recent segment was expired early"
    );

    for (seconds, expected) in [(30, 30.0), (120, 60.0)] {
        // Earlier jobs may have published a clip event before this request.
        while profile.events.try_recv().is_ok() {}
        profile
            .client
            .call(Request::SaveRecentRolling { seconds }, REQUEST_TIMEOUT)?;
        let deadline = Instant::now() + JOB_TIMEOUT;
        let id = loop {
            match profile
                .events
                .recv_deadline(deadline)
                .context("Replay save did not finish")?
            {
                Event::ClipAdded { clip_id } => break clip_id,
                Event::Toast { level, message } if level == "error" => anyhow::bail!("{message}"),
                _ => {}
            }
        };
        let clip = Db::open_default()?
            .clip(id)?
            .context("Replay clip was not indexed")?;
        ensure!(
            (clip.duration - expected).abs() < 0.3,
            "Requested {seconds}s, expected {expected}s, got {:.2}s",
            clip.duration
        );
        ensure!(
            clip.source_start_ms == Some(end_ms - (expected * 1000.0) as i64),
            "Replay incorrectly used a different source range"
        );
        println!(
            "SaveRecentRolling {seconds}s: {:.2}s saved to {}",
            clip.duration, clip.path
        );
    }

    config.recording.history_mode = HistoryMode::Archive;
    config.save()?;
    profile.client.call(
        Request::ReloadConfig {
            restart_recorder: false,
        },
        REQUEST_TIMEOUT,
    )?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    while recent.exists() {
        ensure!(
            Instant::now() < deadline,
            "Replay-only footage was not cleared on mode exit"
        );
        thread::sleep(Duration::from_millis(25));
    }
    ensure!(
        !cv_core::waveform::wave_path(&recent).exists(),
        "Mode exit left a sidecar"
    );
    ensure!(
        Path::new(&profile.recording.path).exists() && Path::new(&archived_clip.path).exists(),
        "Mode changes removed existing recordings or saved clips"
    );
    let db = Db::open_default()?;
    ensure!(
        db.list_recordings()?.len() == 1,
        "Replay files appeared in the archive"
    );
    ensure!(
        db.list_clips(&Default::default())?.len() == 3,
        "Saved replay clips were not preserved"
    );
    profile.shutdown()?;
    println!(
        "Replay-only IPC verification passed in {}",
        profile.dir.display()
    );
    Ok(())
}
