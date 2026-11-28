//! Opt-in timing of production backfill against owned-window WGC and the GUI's player.
//! Use a synthetic half-hour archive and a new profile under .clipvault-local/qa.
//! The foreground classification input is simulated to avoid activating other windows.

use super::*;
use crate::waves::measure::{self, Monitor};
use cv_core::waveform::{wave_path, Waveform, PEAKS_PER_SEC};
use cv_media::mp4::reader::Mp4Reader;
use std::fs;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, CREATE_NO_WINDOW, IO_COUNTERS,
};

const SAMPLE_TIME: Duration = Duration::from_secs(8);
const PLAYER_TEST: &str = "native_player::tests::backfill_bench::play_synthetic_archive";

fn wait_for<T>(timeout: Duration, description: &str, mut check: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(value) = check() {
            return Ok(value);
        }
        thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("Timed out waiting for {description}");
}

fn set_focus(sh: &Shared, monitor: &Monitor, focused: bool, phase: &str) -> Result<()> {
    let began = Instant::now();
    sh.foreground_game.store(focused, Ordering::Relaxed);
    let observed = wait_for(Duration::from_secs(5), phase, || {
        let sample = monitor.lock();
        let stamp = if focused { sample.paused } else { sample.resumed };
        stamp.filter(|&time| time >= began)
    })?;
    println!("{phase}: {:.1} ms at chunk boundary", (observed - began).as_secs_f64() * 1000.0);
    Ok(())
}

struct ProcessCounters {
    cpu_100ns: u64,
    io: IO_COUNTERS,
}

impl ProcessCounters {
    fn read() -> Result<Self> {
        let (mut created, mut exit, mut kernel, mut user) = Default::default();
        let mut io = IO_COUNTERS::default();
        unsafe {
            let process = GetCurrentProcess();
            GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)?;
            GetProcessIoCounters(process, &mut io)?;
        }
        let ticks = |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
        Ok(Self { cpu_100ns: ticks(kernel) + ticks(user), io })
    }
}

fn sample_phase(
    profile: &TestProfile, monitor: &Monitor, recording: bool, backfill: bool, name: &str,
) -> Result<()> {
    let count = || monitor.lock().chunks;
    let drops = || profile.shared.status.lock().engine.dropped_frames;
    let begin_marker = profile.dir.join(format!("{name}-begin"));
    let end_marker = profile.dir.join(format!("{name}-end"));
    fs::write(&begin_marker, [])?;
    let utc_start = chrono::Utc::now();
    let (begin, before, chunks, dropped) = (Instant::now(), ProcessCounters::read()?, count(), drops());
    thread::sleep(SAMPLE_TIME);
    let (after, secs, chunks, dropped) = (ProcessCounters::read()?, begin.elapsed().as_secs_f64(), count() - chunks, drops() - dropped);
    let utc_end = chrono::Utc::now();
    fs::write(&end_marker, [])?;
    let cpu = (after.cpu_100ns - before.cpu_100ns) as f64 / 100_000.0 / secs;
    let read_bytes = after.io.ReadTransferCount - before.io.ReadTransferCount;
    let write_bytes = after.io.WriteTransferCount - before.io.WriteTransferCount;
    let capture = if recording { format!("{dropped}") } else { "n/a".into() };
    println!(
        "{name}: {}..{} {secs:.2} s, service {cpu:.2}% of one core, process I/O {read_bytes} B read / {write_bytes} B written, {chunks} archive chunks, recorder dropped {capture}",
        utc_start.to_rfc3339(), utc_end.to_rfc3339()
    );
    if recording {
        let status = profile.shared.status.lock();
        ensure!(status.recording.active && status.engine.capture == "Windows Graphics Capture");
        ensure!(status.mic.state == "disabled");
    }
    ensure!((chunks > 0) == backfill, "{name}: unexpected measured backfill activity");
    Ok(())
}

struct Player(Child);

impl Player {
    fn start(profile: &TestProfile, qa_root: &Path, executable: &Path, fixture: &Path) -> Result<Self> {
        let child = Command::new(executable)
            .args(["--exact", PLAYER_TEST, "--ignored", "--nocapture", "--test-threads=1"])
            .env("CLIPVAULT_QA_BACKFILL_DIR", &profile.dir)
            .env("CLIPVAULT_TEST_FIXTURE", fixture)
            .env("TEMP", &profile.dir)
            .env("TMP", &profile.dir)
            .current_dir(qa_root.parent().and_then(Path::parent).context("Project directory")?)
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdout(fs::File::create(profile.dir.join("player.log"))?)
            .stderr(fs::File::create(profile.dir.join("player.err"))?)
            .spawn()?;
        Ok(Self(child))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn verify_recording(profile: &TestProfile) -> Result<()> {
    let config = profile.shared.cfg();
    let mut segments = fs::read_dir(&config.recording.folder)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp4")) && p != Path::new(&profile.recording.path))
        .collect::<Vec<_>>();
    segments.sort();
    ensure!(segments.len() >= 2, "Owned capture did not produce two finalized files");
    let mut seconds = 0.0;
    let mut frames = 0;
    let mut encoded_gaps = 0;
    for (index, path) in segments.iter().enumerate() {
        let reader = Mp4Reader::open(path)?;
        let video = reader.video().context("QA window video")?;
        ensure!(reader.is_fragmented() && reader.audio().len() == 2);
        seconds += reader.duration_secs();
        let samples = reader.samples(video, 0, u64::MAX)?;
        frames += samples.len();
        for pair in samples.windows(2) {
            let frame_ticks = u64::from(reader.tracks[video].timescale) / u64::from(config.recording.fps);
            encoded_gaps += ((pair[1].dts - pair[0].dts + frame_ticks / 2) / frame_ticks).saturating_sub(1);
        }
        if index == 0 {
            for track in reader.audio() {
                let pcm = cv_media::audioread::read_range(&reader, track, 0.25, 1.25)?;
                ensure!(pcm.iter().all(|x| x.abs() < 0.001), "QA capture contained audible sound");
            }
            let first = cv_media::thumb::keyframe_at(&reader, 1.05)?;
            let later = cv_media::thumb::keyframe_at(&reader, 3.05)?;
            let changed = first.nv12.iter().zip(later.nv12).filter(|(a, b)| a.abs_diff(*b) > 25).count();
            ensure!(changed > first.nv12.len() / 20, "Capture did not preserve the animation");
        }
    }
    println!("Capture output: {} finalized MP4s, {seconds:.3} s, {frames} encoded video frames, {encoded_gaps} inferred missing frame intervals within files, first file audio is silent", segments.len());
    Ok(())
}

#[test]
#[ignore = "Windows WGC for an owned animated window, muted NativePlayer, isolated synthetic archive and optional complete waveform"]
fn measure_archive_backfill() -> Result<()> {
    ensure!(std::env::var("CLIPVAULT_QA_CAPTURE_WINDOW").as_deref() == Ok("1"));
    let local_root = std::env::current_dir()?.join(".clipvault-local").canonicalize()?;
    let qa_root = local_root.join("qa").canonicalize()?;
    ensure!(qa_root.starts_with(&local_root), "QA directory escapes .clipvault-local");
    let profile_dir = cv_core::paths::data_dir();
    ensure!(profile_dir.parent().context("QA profile parent")?.canonicalize()?.starts_with(&qa_root));
    let fixture = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("Synthetic archive fixture required")?).canonicalize()?;
    ensure!(fixture.starts_with(&qa_root));
    let gui_test = PathBuf::from(std::env::var_os("CLIPVAULT_QA_PLAYER_BIN").context("A GUI test executable is required")?).canonicalize()?;
    ensure!(gui_test.starts_with(qa_root.parent().context("QA parent")?) && gui_test.is_file());
    let media = cv_media::probe::probe(&fixture)?;
    println!("Input: {:.3} s, {} tracks, {} B, {}", media.duration, media.audio.len(), fs::metadata(&fixture)?.len(), fixture.display());
    ensure!(media.fragmented && media.duration >= 1799.9 && media.audio.len() >= 3,
        "Expected at least 30 minutes of native synthetic archive with 3 or more AAC tracks");
    let completion_seconds = std::env::var("CLIPVAULT_QA_COMPLETION_SECS")
        .unwrap_or_else(|_| "0".into()).parse::<u64>()?;
    ensure!(completion_seconds <= 1800, "QA completion deadline is at most 30 minutes");

    let monitor = Monitor::default();
    // Declaration order stops the recorder before destroying its window on error.
    let window = SyntheticWindow::open()?;
    let mut profile = TestProfile::with_wave_worker(true, {
        let monitor = monitor.clone();
        move |shared| measure::run(shared, monitor)
    })?;
    // The service intentionally delays backfill at startup. Keep capture bounded.
    thread::sleep(Duration::from_secs(20));
    start_owned_recorder(&mut profile, window.hwnd)?;
    wait_for(Duration::from_secs(12), "owned-window capture", || {
        let status = profile.shared.status.lock();
        (status.recording.active && status.engine.fps > 0.0 && status.recording.written_until_ms > status.recording.started_ms).then_some(())
    })?;
    sample_phase(&profile, &monitor, true, false, "record-only")?;
    set_focus(&profile.shared, &monitor, false, "initial backfill admission")?;
    sample_phase(&profile, &monitor, true, true, "record-and-backfill")?;
    set_focus(&profile.shared, &monitor, true, "capture focus pause")?;
    profile.shared.engine_tx.send(EngineCmd::Shutdown)?;
    profile.engine.take().context("owned capture thread")?.join().map_err(|_| anyhow::anyhow!("capture thread failed"))?;
    drop(window);
    verify_recording(&profile)?;

    let mut player = Player::start(&profile, &qa_root, &gui_test, &fixture)?;
    wait_for(Duration::from_secs(12), "muted NativePlayer", || profile.dir.join("player-ready").is_file().then_some(()))?;
    sample_phase(&profile, &monitor, false, false, "play-only")?;
    set_focus(&profile.shared, &monitor, false, "playback focus resume")?;
    sample_phase(&profile, &monitor, false, true, "play-and-backfill")?;
    set_focus(&profile.shared, &monitor, true, "playback focus pause")?;
    let exit = wait_for(Duration::from_secs(5), "GUI player receipt", || player.0.try_wait().ok().flatten())?;
    print!("{}", fs::read_to_string(profile.dir.join("player.log"))?);
    ensure!(exit.success(), "NativePlayer benchmark failed (see player.err)");
    let output = wave_path(Path::new(&profile.recording.path));
    if completion_seconds > 0 {
        set_focus(&profile.shared, &monitor, false, "completion focus resume")?;
        let began = Instant::now();
        let deadline = began + Duration::from_secs(completion_seconds);
        while !output.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_secs(1));
            let elapsed = began.elapsed().as_secs();
            if elapsed % 60 == 0 {
                println!("Completing archive: {elapsed}s, {} chunks", monitor.lock().chunks);
            }
        }
        ensure!(output.is_file(), "Archive did not finish by the deadline ({} chunks)", monitor.lock().chunks);
        let wave = Waveform::load(&output)?;
        ensure!(wave.rate == PEAKS_PER_SEC && wave.tracks == media.audio.len());
        ensure!((wave.duration_s() - media.duration).abs() <= 0.1 && wave.peaks.iter().any(|&v| v != 0),
            "Archive waveform duration or peaks do not match the input");
        println!("Waveform output: {:.3} s, {} tracks, {} frames, {} B, {:.2}s after final resume, {}", wave.duration_s(), wave.tracks, wave.frames(), fs::metadata(&output)?.len(), began.elapsed().as_secs_f64(), output.display());
    } else {
        ensure!(!output.exists(), "An incomplete waveform became visible");
        println!("Partial run: {} chunks admitted (200 ms per chunk per track), no waveform committed", monitor.lock().chunks);
    }
    profile.shutdown()?;
    Ok(())
}
