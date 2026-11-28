//! Waveforms for the audio lanes.
//!
//! The recorder writes waveforms live while recording, and new clips get
//! theirs when they're made. This worker fills in the rest (recordings and
//! clips from before, or files you copied in) by decoding their audio slowly
//! in the background, and pauses while you're playing a game.

use crate::shared::Shared;
use cv_core::db::Db;
use cv_core::model::ClipFilter;
use cv_core::waveform::wave_path;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};

const MAX_TRACK_SPEED: u32 = 50;
const FOCUS_POLL: Duration = Duration::from_millis(250);

fn gaming(sh: &Shared) -> bool {
    sh.foreground_game.load(Ordering::Relaxed)
}

fn wait_for_archive_slot(
    span: Duration,
    mut flags: impl FnMut() -> (bool, bool),
    mut sleep: impl FnMut(Duration),
) -> anyhow::Result<()> {
    let mut throttle = span / MAX_TRACK_SPEED;
    loop {
        let (shutting_down, game_focused) = flags();
        anyhow::ensure!(!shutting_down, "waveform backfill stopped");
        if game_focused {
            #[cfg(test)]
            measure::waiting_for_focus();
            sleep(FOCUS_POLL);
        } else if !throttle.is_zero() {
            sleep(std::mem::take(&mut throttle));
        } else {
            return Ok(());
        }
    }
}

pub fn run(sh: Arc<Shared>) {
    // Limit CPU and I/O priority on this background worker only.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    let _ = std::fs::create_dir_all(cv_core::paths::data_dir().join("waves"));
    sleep_unless_shutdown(&sh, Duration::from_secs(20));
    let mut failed: HashSet<String> = HashSet::new();
    let mut last_cleanup = Instant::now() - Duration::from_secs(3600);
    while !sh.is_shutting_down() {
        let Ok(db) = Db::open_default() else {
            sleep_unless_shutdown(&sh, Duration::from_secs(60));
            continue;
        };
        let mut did_work = false;
        if !gaming(&sh) {
            let mut todo: Vec<String> = Vec::new();
            let mut recs = db.list_recordings().unwrap_or_default();
            recs.sort_by_key(|r| std::cmp::Reverse(r.start_ms));
            for r in recs.iter().filter(|r| !r.is_live() && r.duration > 0.0) {
                todo.push(r.path.clone());
            }
            for c in db.list_clips(&ClipFilter::default()).unwrap_or_default().iter().filter(|c| c.audio_tracks > 0).take(2000) {
                todo.push(c.path.clone());
            }
            for path in todo {
                if sh.is_shutting_down() || gaming(&sh) {
                    break;
                }
                if failed.contains(&path) || sh.is_live(&path) {
                    continue;
                }
                let src = PathBuf::from(&path);
                let wp = wave_path(&src);
                if wp.exists() || !crate::shared::media_path_allowed(&src) || !src.is_file() {
                    continue;
                }
                let archive_slot = |span| {
                    wait_for_archive_slot(span, || (sh.is_shutting_down(), gaming(&sh)), std::thread::sleep)?;
                    #[cfg(test)]
                    if !span.is_zero() {
                        measure::starting_chunk();
                    }
                    Ok(())
                };
                match cv_media::wave::generate_archive(&src, archive_slot) {
                    Ok(w) => {
                        if archive_slot(Duration::ZERO).is_ok() && src.is_file() && !wp.exists() {
                            if let Err(e) = w.save(&wp) {
                                tracing::warn!("saving waveform for {}: {e:#}", src.display());
                            }
                        }
                    }
                    Err(_) if sh.is_shutting_down() => break,
                    Err(e) => {
                        tracing::debug!("waveform for {}: {e:#}", src.display());
                        failed.insert(path.clone());
                    }
                }
                did_work = true;
                break; // one file per pass, then breathe
            }
        }
        if last_cleanup.elapsed() > Duration::from_secs(3600) {
            last_cleanup = Instant::now();
            cleanup(&db);
        }
        let delay = if gaming(&sh) { Duration::from_secs(1) } else if did_work { Duration::from_secs(2) } else { Duration::from_secs(30) };
        sleep_unless_shutdown(&sh, delay);
    }
}

/// Remove waveform files whose recording/clip no longer exists.
fn cleanup(db: &Db) {
    let mut keep: HashSet<PathBuf> = HashSet::new();
    for r in db.list_recordings().unwrap_or_default() {
        keep.insert(wave_path(Path::new(&r.path)));
    }
    for c in db.list_clips(&ClipFilter { include_missing: true, ..Default::default() }).unwrap_or_default() {
        keep.insert(wave_path(Path::new(&c.path)));
    }
    let dir = cv_core::paths::data_dir().join("waves");
    let buffer = crate::engine::buffer_dir();
    if let Ok(rd) = std::fs::read_dir(&buffer) {
        for e in rd.flatten() {
            keep.insert(wave_path(&e.path()));
        }
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let fresh = e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().map(|d| d < Duration::from_secs(600)).unwrap_or(true)).unwrap_or(true);
            if !keep.contains(&e.path()) && !fresh {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn sleep_unless_shutdown(sh: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !sh.is_shutting_down() {
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
#[path = "waves_measure.rs"]
pub(crate) mod measure;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_chunk_waits_for_focus_and_throttles_before_resuming() {
        let mut flags = [(false, true), (false, true), (false, false), (false, false)].into_iter();
        let mut waits = Vec::new();
        wait_for_archive_slot(Duration::from_millis(200), || flags.next().unwrap(), |delay| waits.push(delay)).unwrap();
        assert_eq!(waits, [FOCUS_POLL, FOCUS_POLL, Duration::from_millis(4)]);
        assert_eq!(flags.len(), 0);
    }

    #[test]
    fn archive_chunk_stops_while_waiting_for_game_focus_to_clear() {
        let mut flags = [(false, true), (true, true)].into_iter();
        let mut waits = Vec::new();
        assert!(wait_for_archive_slot(Duration::from_millis(200), || flags.next().unwrap(), |delay| waits.push(delay)).is_err());
        assert_eq!(waits, [FOCUS_POLL]);
    }
}
