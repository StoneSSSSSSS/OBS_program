//! Opt-in IPC checks against the actual library and background jobs. Both tests
//! require a new ClipVault-specific data directory and explicit synthetic media.

use super::*;
use crate::shared::Footage;
use anyhow::{ensure, Context};
use cv_core::config::HistoryMode;
use cv_core::db::Db;
use cv_core::ipc::{Event, ExportSource, IpcClient, JobInfo, Request, Status};
use cv_core::mixer::MixSettings;
use cv_core::model::{AudioEdit, Clip, NewClip, Recording};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

#[path = "test_ipc/archive_day.rs"]
mod archive_day;

#[path = "test_ipc/hourly_split.rs"]
mod hourly_split;

#[path = "test_ipc/short_history.rs"]
mod short_history;

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
    waves: Option<thread::JoinHandle<()>>,
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
    fn new() -> Result<Self> {
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
        std::fs::create_dir_all(&config.recording.folder)?;
        std::fs::create_dir_all(&config.clips.folder)?;
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
        let waves = spawn("isolated-waves", {
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
            engine_rx: Some(engine_rx),
            engine: None,
            library: Some(library),
            status: Some(status),
            waves: Some(waves),
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
        if let Some(worker) = self.waves.take() {
            let _ = worker.join();
        }
        // Detached IPC/job threads can retain Shared until the test process
        // exits. Close the idle event-log connection before TempDir cleanup.
        drop(self.shared.event_db.lock().take());
        let _ = std::fs::remove_file(cv_core::paths::ipc_info_path());
    }
}

#[test]
#[ignore = "Requires an explicit synthetic MP4 and fresh test profile directory"]
fn cut_trim_and_export_over_ipc() -> Result<()> {
    let profile = TestProfile::new()?;
    let mut settings = profile.shared.cfg();
    settings.general.ui_scale = 1.125;
    settings.save()?;
    profile.client.call(Request::ReloadConfig { restart_recorder: false }, REQUEST_TIMEOUT)?;
    ensure!(profile.shared.cfg().general.ui_scale == settings.general.ui_scale, "isolated settings did not reload");
    ensure!(matches!(
        profile.engine_rx.as_ref().context("isolated engine receiver")?.try_recv(),
        Ok(EngineCmd::ConfigChanged { restart: false })
    ), "capture-free reload did not queue its recorder notification");
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
    ensure!(
        exported.source_clip_id == Some(clip.id) && exported.clip_id.is_none(),
        "completed export must identify its source without becoming a saved clip"
    );
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
    let source = Path::new(&profile.recording.path);
    let record_dir = source.parent().context("Recording source folder")?;
    let old = chrono::Local::now() - chrono::Duration::days(3);
    let expired_recording = record_dir.join(format!("{}.mp4", old.format("%Y-%m-%d %H-%M-%S")));
    let misplaced_export = record_dir.join(format!("{}.mp4", (old + chrono::Duration::seconds(1)).format("%Y-%m-%d %H-%M-%S")));
    let registered_clip = record_dir.join(format!("{}.mp4", (old + chrono::Duration::seconds(2)).format("%Y-%m-%d %H-%M-%S")));
    std::fs::copy(source, &expired_recording)?;
    std::fs::copy(&output, &misplaced_export)?;
    std::fs::copy(source, &registered_clip)?;
    Db::open_default()?.insert_clip(&NewClip {
        path: registered_clip.to_string_lossy().into_owned(), title: "Previously saved native clip".into(),
        game_id: None, created_at: 0, source_start_ms: None, size: registered_clip.metadata()?.len() as i64,
        source: "import".into(), favorite: false,
    })?;
    profile.shared.cfg.write().recording.max_age_days = 1;
    profile.client.call(Request::RescanLibrary, REQUEST_TIMEOUT)?;
    let expiry_deadline = Instant::now() + REQUEST_TIMEOUT;
    while expired_recording.exists() {
        ensure!(Instant::now() < expiry_deadline, "Retention failed to remove its expired native recording");
        thread::sleep(Duration::from_millis(25));
    }
    ensure!(misplaced_export.metadata()?.len() == size && registered_clip.is_file(),
        "Retention removed an export or saved clip from the archive folder");
    let kept_dir = source.parent().context("Recording source folder")?.join("Kept");
    std::fs::create_dir_all(&kept_dir)?;
    let collision = kept_dir.join(source.file_name().context("Recording filename")?);
    std::fs::write(&collision, b"existing kept file")?;
    profile.client.call(Request::KeepRecording { recording_id: profile.recording.id, keep: true }, REQUEST_TIMEOUT)?;
    let kept = Db::open_default()?.recording(profile.recording.id)?.context("Kept recording missing")?;
    ensure!(kept.kept && Path::new(&kept.path) != collision && Path::new(&kept.path).is_file(), "Keep did not choose a vacant filename");
    ensure!(std::fs::read(&collision)? == b"existing kept file", "Keep overwrote an existing file");
    let sibling = tempfile::tempdir_in(profile.dir.parent().context("QA profile parent")?)?;
    let outside = sibling.path().join("outside-profile.mp4");
    std::fs::write(&outside, b"isolated outside-profile fixture")?;
    let local = profile.dir.join("clips").join("trash-fixture.mp4");
    std::fs::write(&local, b"isolated project trash fixture")?;
    let db = Db::open_default()?;
    let register = |path: &Path| db.insert_clip(&NewClip {
        path: path.to_string_lossy().into_owned(), title: "Ownership test fixture".into(),
        game_id: None, created_at: 0, source_start_ms: None, size: path.metadata()?.len() as i64,
        source: "import".into(), favorite: false,
    });
    let outside_id = register(&outside)?;
    ensure!(profile.client.call(Request::DeleteClips { clip_ids: vec![outside_id] }, REQUEST_TIMEOUT).is_err(), "Delete accepted media outside the selected project profile");
    ensure!(db.clip(outside_id)?.is_some() && outside.is_file(), "Rejected delete altered the outside fixture");
    let local_id = register(&local)?;
    profile.shared.cfg.write().clips.delete_to_recycle_bin = true;
    profile.client.call(Request::DeleteClips { clip_ids: vec![local_id] }, REQUEST_TIMEOUT)?;
    let trash = std::fs::read_dir(profile.dir.join("trash"))?.next().context("Project Trash is empty")??.path();
    ensure!(!local.exists() && std::fs::read(trash.join("trash-fixture.mp4"))? == b"isolated project trash fixture", "Local Trash did not keep the deleted media inside the project");
    profile.shutdown()?;
    println!("Isolated IPC checks passed in {}", profile.dir.display());
    Ok(())
}

#[test]
#[ignore = "Opt-in service for GUI testing, stops on IPC Shutdown or after ten minutes"]
fn serve_isolated_profile() -> Result<()> {
    let profile = TestProfile::new()?;
    profile.cut_sample()?;
    println!(
        "Ready for isolated GUI testing in {}",
        profile.dir.display()
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut armed_live = false;
    let observe_interactions = std::env::var_os("CLIPVAULT_INTERACTION_CHECK").is_some();
    while !profile.shared.is_shutting_down() && Instant::now() < deadline {
        if observe_interactions && !armed_live && profile.dir.join("qa-arm-live").exists() {
            arm_synthetic_live(&profile)?;
            armed_live = true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn arm_synthetic_live(profile: &TestProfile) -> Result<()> {
    let db = Db::open_default()?;
    let source = Path::new(&profile.recording.path);
    let duration = profile.recording.duration;
    let start = chrono::Local::now() - chrono::Duration::milliseconds((duration * 1000.0) as i64);
    let path = source.with_file_name(format!("{}.mp4", start.format("%Y-%m-%d %H-%M-%S")));
    std::fs::copy(source, &path)?;
    let start_ms = cv_core::filenames::parse_timestamp_ms(&path).context("synthetic live timestamp")?;
    let bytes = path.metadata()?.len() as i64;
    let path_text = path.to_string_lossy().into_owned();
    *profile.shared.live_path.lock() = Some(path.clone());
    let id = db.upsert_recording(&path_text, start_ms, bytes, "recording")?;
    db.set_recording_probe(id, start_ms, duration, profile.recording.audio_tracks, false, None)?;
    profile.shared.update_status(|status| {
        status.recording = cv_core::ipc::RecStatus {
            active: true,
            path: path_text,
            started_ms: start_ms,
            written_until_ms: start_ms + (duration * 1000.0) as i64,
            bytes,
            healthy: true,
            tracks: profile.recording.audio_tracks as u32,
            ..Default::default()
        };
    });
    profile.shared.emit(Event::LibraryChanged);
    Ok(())
}

#[test]
#[ignore = "Requires synthetic media, fresh workspace profile, and loopback access"]
fn save_rolling_fixture_over_ipc() -> Result<()> {
    let mut profile = TestProfile::new()?;
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

#[test]
#[ignore = "captures the Windows display into a disposable profile for about 2 minutes; microphone off"]
fn real_rolling_recorder_over_ipc() -> Result<()> {
    ensure!(
        std::env::var("CLIPVAULT_TEST_LIVE_ROLLING").as_deref() == Ok("1"),
        "Set CLIPVAULT_TEST_LIVE_ROLLING=1 and use a non-private test display"
    );
    let temp = tempfile::Builder::new().prefix("clipvault-replay-hardware-").tempdir()?;
    let data = temp.path().join("clipvault-ipc-live-rolling");
    std::env::set_var("CLIPVAULT_DATA_DIR", &data);
    let mut profile = TestProfile::new()?;
    let archived_clip = profile.cut_sample()?;
    let archive = Path::new(&profile.recording.path);
    let old_clip = Path::new(&archived_clip.path);
    let archive_size = archive.metadata()?.len();
    let old_clip_size = old_clip.metadata()?.len();

    let mut config = profile.shared.cfg();
    config.recording.history_mode = HistoryMode::Rolling;
    config.recording.rolling_minutes = 2;
    config.recording.max_height = 360;
    config.recording.fps = 15;
    config.recording.quality = "custom".into();
    config.recording.bitrate_kbps = 1200;
    config.recording.encoder = "software".into();
    config.recording.show_cursor = false;
    config.recording.app_tracks = false;
    config.audio.record_mic = false;
    config.audio.mic_guard = false;
    config.clips.save_cooldown_ms = 0;
    config.clips.generate_thumbnails = false;
    config.save()?;
    *profile.shared.cfg.write() = config;

    let rx = profile.engine_rx.take().context("isolated engine channel")?;
    let shared = profile.shared.clone();
    profile.engine = Some(spawn("real-isolated-rolling-engine", move || engine::run(shared, rx)));
    let rolling = data.join("rolling");
    let pending = || -> Result<Vec<PathBuf>> {
        if !rolling.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = std::fs::read_dir(&rolling)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
            .collect::<Vec<_>>();
        files.sort();
        Ok(files)
    };
    let status = || -> Result<Status> {
        Ok(serde_json::from_value(profile.client.call(Request::Status, REQUEST_TIMEOUT)?)?)
    };
    let deadline = Instant::now() + Duration::from_secs(25);
    let first = loop {
        let files = pending()?;
        if files.len() >= 2 {
            let s = status()?;
            if s.replay.active {
                ensure!(s.mic.state == "disabled", "microphone unexpectedly active: {}", s.mic.state);
                ensure!(!s.recording.active && s.replay.rolling_minutes == Some(2), "expected replay-only status");
                println!("Live recorder: {}x{} {:.1} fps, {}, {}", s.engine.width, s.engine.height, s.engine.fps, s.engine.capture, s.engine.encoder);
                break files[0].clone();
            }
        }
        ensure!(Instant::now() < deadline, "real recorder did not create two segments: {:?}", status()?.engine);
        thread::sleep(Duration::from_millis(250));
    };
    let first_info = cv_media::probe::probe(&first)?;
    ensure!((first_info.duration - 10.0).abs() < 0.7, "first segment duration: {:.3}s", first_info.duration);
    let first_wall = first_info.start_ms.context("real segment wall-clock timestamp")?;
    let first_wave = cv_core::waveform::wave_path(&first);
    ensure!(first_wave.exists(), "first segment waveform sidecar is missing");
    println!("First real segment completed at {:.2}s", first_info.duration);

    let deadline = Instant::now() + Duration::from_secs(150);
    let mut max_segments = 0;
    let mut next_report = 30;
    loop {
        let files = pending()?;
        max_segments = max_segments.max(files.len());
        let s = status()?;
        if s.replay.available_seconds >= next_report {
            println!("Rolling buffer: {}s available, {} files", s.replay.available_seconds, files.len());
            next_report += 30;
        }
        if !first.exists() {
            ensure!(!first_wave.exists(), "expired waveform sidecar remained");
            let age_ms = cv_core::util::now_ms() - first_wall;
            ensure!(age_ms >= 120_000, "rotated out before the 2-minute window");
            ensure!(s.replay.available_seconds >= 108, "buffer stopped advancing: {s:?}");
            ensure!(max_segments >= 11 && files.len() <= 14, "unexpected rotation counts, {max_segments} peak and {} current", files.len());
            println!("Expired first segment and its waveform at {age_ms}ms; {} buffered files", files.len());
            break;
        }
        ensure!(Instant::now() < deadline, "2-minute replay retention never expired the first real segment: {s:?}");
        thread::sleep(Duration::from_millis(500));
    }

    let db = Db::open_default()?;
    let mut saved = Vec::new();
    for (seconds, wanted, tolerance) in [(30, 30.0, 0.8), (300, 120.0, 1.5)] {
        while profile.events.try_recv().is_ok() {}
        profile.client.call(Request::SaveRecentRolling { seconds }, Duration::from_secs(15))?;
        let deadline = Instant::now() + Duration::from_secs(25);
        let id = loop {
            match profile.events.recv_deadline(deadline).context("real replay save timed out")? {
                Event::ClipAdded { clip_id } => break clip_id,
                Event::Toast { level, message } if level == "error" => anyhow::bail!("{message}"),
                _ => {}
            }
        };
        let clip = db.clip(id)?.context("replay clip missing from library")?;
        let path = PathBuf::from(clip.path);
        let info = cv_media::probe::probe(&path)?;
        ensure!((info.duration - wanted).abs() < tolerance && info.duration < wanted + 0.2,
            "requested {seconds}s, expected {wanted}s cap, actual {:.3}s", info.duration);
        ensure!(info.start_ms.is_some_and(|at| at >= first_wall), "replay unexpectedly used archive footage");
        println!("SaveRecentRolling({seconds}) -> {:.3}s, {}x{}", info.duration, info.width, info.height);
        saved.push((path.clone(), path.metadata()?.len()));
    }
    ensure!(db.list_recordings()?.len() == 1, "rolling files appeared in archive");
    let wave_paths = pending()?.iter().map(|path| cv_core::waveform::wave_path(path)).collect::<Vec<_>>();
    profile.shutdown()?;
    profile.engine.take().context("engine worker")?.join().map_err(|_| anyhow::anyhow!("real engine thread panicked"))?;
    ensure!(pending()?.is_empty() && wave_paths.iter().all(|p| !p.exists()), "shutdown left temporary replay media");
    ensure!(archive.metadata()?.len() == archive_size && old_clip.metadata()?.len() == old_clip_size, "shutdown changed preexisting archive/clip media");
    ensure!(saved.iter().all(|(path, size)| path.metadata().is_ok_and(|m| m.len() == *size)), "shutdown changed a saved replay");
    ensure!(db.list_clips(&Default::default())?.len() == 3, "saved clip count changed after shutdown");
    println!("Shutdown cleared real rolling footage; archive and all three saved clips remained intact until fixture teardown");
    drop(db);
    drop(profile);
    temp.close().context("remove disposable hardware profile and all captured content")?;
    ensure!(!data.exists(), "disposable profile still exists");
    println!("Disposed of the entire hardware test profile");
    Ok(())
}

#[test]
#[ignore = "Requires a fresh profile and the generated synthetic recording fixture"]
fn long_clip_titles_and_safe_filenames_roundtrip_over_ipc() -> Result<()> {
    let profile = TestProfile::new()?;
    profile.shared.cfg.write().clips.organize_by_game = true;
    let db = Db::open_default()?;
    let full_title = format!("First: ⚔️ / {} — “公園?” 👩‍💻", "🧪".repeat(100));
    let start = profile.recording.start_ms + 1_000;
    let create = || {
        profile.run_job(Request::CutRange {
            from_ms: start,
            to_ms: start + 4_000,
            title: format!("  {full_title}  "),
            game_id: None,
            category_ids: Vec::new(),
            mix: None,
            audio_edits: Vec::new(),
        })
    };
    let first = create()?;
    let second = create()?;
    let load = |job: &JobInfo| -> Result<Clip> {
        db.clip(job.clip_id.context("clip job lacked an ID")?)?
            .context("saved clip is missing from the database")
    };
    let clip = load(&first)?;
    let duplicate = load(&second)?;
    ensure!(clip.title == full_title && duplicate.title == full_title && first.title == full_title,
        "CutRange lost or altered the display title");
    ensure!(clip.path != duplicate.path && Path::new(&clip.path).is_file() && Path::new(&duplicate.path).is_file(),
        "two colliding titles did not save to different files");
    ensure!(duplicate.path.ends_with(" (2).mp4"), "collision suffix missing");
    let safe_filename = |path: &str| -> Result<()> {
        let name = Path::new(path).file_name().and_then(|name| name.to_str()).context("missing filename")?;
        ensure!(name.encode_utf16().count() <= 104, "filename was not bounded in UTF-16");
        ensure!(!name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']), "filename contains a Windows-special character");
        Ok(())
    };
    safe_filename(&clip.path)?;
    safe_filename(&duplicate.path)?;

    let renamed = format!("Rename: {} — final / v2?", "長".repeat(160));
    profile.client.call(Request::RenameClip { clip_id: clip.id, title: renamed.clone() }, REQUEST_TIMEOUT)?;
    let after = load(&first)?;
    ensure!(after.title == renamed && after.path != clip.path && Path::new(&after.path).is_file(), "rename did not preserve full title and safe updated file");
    safe_filename(&after.path)?;

    let game = db.upsert_game("regroup.exe", "Regroup fixture", "", true, start)?;
    profile.client.call(Request::SetClipGame { clip_id: clip.id, game_id: Some(game) }, REQUEST_TIMEOUT)?;
    let grouped = load(&first)?;
    ensure!(grouped.title == renamed && grouped.path != after.path && Path::new(&grouped.path).is_file(),
        "moving clips between game folders altered the display title or lost the file");

    let trim = profile.run_job(Request::TrimClip { clip_id: clip.id, start_s: 0.2, end_s: 2.0, replace: false })?;
    let trimmed = load(&trim)?;
    ensure!(trimmed.title == format!("{renamed} (trim)") && trimmed.path.ends_with(" (trim).mp4"),
        "trim copy lost the full display title or trim filename suffix");
    safe_filename(&trimmed.path)?;

    let export = profile.run_job(Request::Export {
        source: ExportSource::Clip { clip_id: clip.id },
        range: None,
        mix: MixSettings::default(),
        edits: Some(Vec::new()),
        target_size_mb: None,
        title: full_title.clone(),
    })?;
    let output = export.output.context("export did not produce a file")?;
    ensure!(Path::new(&output).is_file() && output.ends_with(" (export).mp4"), "export lost its filename suffix");
    safe_filename(&output)?;
    profile.shutdown()?;
    println!("Preserved {} UTF-16 units at cut and {} after rename; verified trim and export suffixes", full_title.encode_utf16().count(), renamed.encode_utf16().count());
    Ok(())
}
