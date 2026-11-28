//! Muted synthetic load for the service backfill benchmark. The shared NativePlayer
//! performs real decode, WASAPI render and frame publication in a headless egui context.

use super::*;
use std::fs;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS};

fn sample() -> Result<(u64, IO_COUNTERS)> {
    let (mut created, mut exit, mut kernel, mut user) = Default::default();
    let mut io = IO_COUNTERS::default();
    unsafe {
        let process = GetCurrentProcess();
        GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)?;
        GetProcessIoCounters(process, &mut io)?;
    }
    let ticks = |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    Ok((ticks(kernel) + ticks(user), io))
}

#[test]
#[ignore = "Launch from the service backfill test with a synthetic archive and isolated profile"]
fn play_synthetic_archive() -> Result<()> {
    let local_root = std::env::current_dir()?.join(".clipvault-local").canonicalize()?;
    let qa_root = local_root.join("qa").canonicalize()?;
    anyhow::ensure!(qa_root.starts_with(&local_root), "QA directory escapes .clipvault-local");
    let dir = PathBuf::from(std::env::var_os("CLIPVAULT_QA_BACKFILL_DIR").context("Benchmark profile required")?).canonicalize()?;
    let path = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("Synthetic archive required")?).canonicalize()?;
    anyhow::ensure!(dir.starts_with(&qa_root) && path.starts_with(&qa_root)
        && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("clipvault-ipc-"))
        && !dir.join("player-ready").exists());
    let player = NativePlayer::new(egui::Context::default());
    player.set_volume(0.0);
    player.load(path, 0.0, false, false);
    wait_for(&player, |s, f| s.loaded && !s.paused && s.position > 0.25 && f.is_some());
    fs::write(dir.join("player-ready"), [])?;
    let await_marker = |name: &str| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !dir.join(name).exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        anyhow::ensure!(dir.join(name).exists(), "Missing benchmark marker {name}");
        Ok(())
    };
    for phase in ["play-only", "play-and-backfill"] {
        await_marker(&format!("{phase}-begin"))?;
        let (begin, start, frames, position) = (
            Instant::now(), sample()?, player.frame().context("Initial player frame")?.serial, player.state().position
        );
        await_marker(&format!("{phase}-end"))?;
        let (end, secs) = (sample()?, begin.elapsed().as_secs_f64());
        let state = player.state();
        let frames = player.frame().context("Final player frame")?.serial - frames;
        let played = state.position - position;
        let cpu = (end.0 - start.0) as f64 / 100_000.0 / secs;
        let read_bytes = end.1.ReadTransferCount - start.1.ReadTransferCount;
        let write_bytes = end.1.WriteTransferCount - start.1.WriteTransferCount;
        println!("{phase}: NativePlayer {cpu:.2}% of one core, process I/O {read_bytes} B read / {write_bytes} B written, {frames} frames published, {played:.3} s playback in {secs:.2} s");
        anyhow::ensure!(state.error.is_none() && !state.paused && frames > 90 && played > 6.0, "Native playback failed during {phase}");
    }
    Ok(())
}
