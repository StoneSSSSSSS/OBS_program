//! Background waveform generation for the timeline's audio lanes.
//!
//! Newest recordings first, then clips. The file OBS is writing is extended
//! incrementally about once a minute. Everything runs at below-normal
//! priority with pauses between files so it never competes with a game.

use crate::shared::Shared;
use cv_core::db::Db;
use cv_core::model::ClipFilter;
use cv_core::waveform::{generate, wave_path, Waveform};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Don't read the last few seconds of a growing file (partial fragment).
const LIVE_SAFETY_S: f64 = 4.0;
const LIVE_INTERVAL: Duration = Duration::from_secs(60);

pub fn run(sh: Arc<Shared>) {
    let _ = std::fs::create_dir_all(cv_core::paths::data_dir().join("waves"));
    // Let startup settle (indexing, OBS launch) first.
    sleep_unless_shutdown(&sh, Duration::from_secs(20));
    let mut live_done: Option<(String, Instant)> = None;
    let mut failed: HashSet<String> = HashSet::new();
    let mut last_cleanup = Instant::now() - Duration::from_secs(3600);
    let never = AtomicBool::new(false);
    while !sh.is_shutting_down() {
        let Some(tools) = sh.tools() else {
            sleep_unless_shutdown(&sh, Duration::from_secs(60));
            continue;
        };
        let Ok(db) = Db::open_default() else {
            sleep_unless_shutdown(&sh, Duration::from_secs(60));
            continue;
        };
        let mut recs = db.list_recordings().unwrap_or_default();
        recs.sort_by_key(|r| std::cmp::Reverse(r.start_ms));
        let live_tracks = sh.status.lock().recording.tracks as usize;
        let mut did_work = false;

        for r in &recs {
            if sh.is_shutting_down() {
                return;
            }
            if failed.contains(&r.path) {
                continue;
            }
            let src = PathBuf::from(&r.path);
            let wp = wave_path(&src);
            let existing = Waveform::load(&wp).ok();
            if r.is_live() {
                let due = live_done.as_ref().map(|(p, t)| p != &r.path || t.elapsed() >= LIVE_INTERVAL).unwrap_or(true);
                if !due {
                    continue;
                }
                let tracks = if r.audio_tracks > 0 { r.audio_tracks as usize } else { live_tracks.max(1) };
                let recorded = (cv_core::util::now_ms() - r.start_ms) as f64 / 1000.0 - LIVE_SAFETY_S;
                extend(&tools.ffmpeg, &src, &wp, existing, tracks, recorded, &never, &mut failed, &r.path);
                live_done = Some((r.path.clone(), Instant::now()));
                did_work = true;
                continue;
            }
            if r.audio_tracks <= 0 || r.duration <= 0.0 {
                continue; // not probed yet
            }
            let complete = existing.as_ref().map(|w| w.tracks == r.audio_tracks as usize && w.duration_s() >= r.duration - 2.0).unwrap_or(false);
            if complete {
                continue;
            }
            extend(&tools.ffmpeg, &src, &wp, existing, r.audio_tracks as usize, r.duration, &never, &mut failed, &r.path);
            did_work = true;
            // One full file per pass, then breathe (and re-check the live file).
            break;
        }

        if !did_work {
            // Clips (small files) once the recordings are done.
            let clips = db.list_clips(&ClipFilter::default()).unwrap_or_default();
            for c in clips.iter().filter(|c| c.audio_tracks > 0 && c.duration > 0.0).take(500) {
                if sh.is_shutting_down() {
                    return;
                }
                let src = PathBuf::from(&c.path);
                let wp = wave_path(&src);
                if wp.exists() || failed.contains(&c.path) || !src.is_file() {
                    continue;
                }
                extend(&tools.ffmpeg, &src, &wp, None, c.audio_tracks as usize, c.duration, &never, &mut failed, &c.path);
                did_work = true;
                break;
            }
        }

        if last_cleanup.elapsed() > Duration::from_secs(3600) {
            last_cleanup = Instant::now();
            cleanup(&db);
        }
        sleep_unless_shutdown(&sh, if did_work { Duration::from_secs(5) } else { Duration::from_secs(30) });
    }
}

#[allow(clippy::too_many_arguments)]
fn extend(
    ffmpeg: &Path,
    src: &Path,
    wp: &Path,
    existing: Option<Waveform>,
    tracks: usize,
    upto_s: f64,
    cancel: &AtomicBool,
    failed: &mut HashSet<String>,
    key: &str,
) {
    let mut base = existing.filter(|w| w.tracks == tracks).unwrap_or_default();
    let from = base.duration_s();
    if upto_s - from < 1.0 {
        return;
    }
    match generate(ffmpeg, src, tracks, from, cancel) {
        Ok(w) => {
            base.append(&w);
            if let Err(e) = base.save(wp) {
                tracing::warn!("saving waveform for {}: {e:#}", src.display());
            }
        }
        Err(e) => {
            tracing::warn!("waveform for {}: {e:#}", src.display());
            failed.insert(key.to_string());
        }
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
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if !keep.contains(&e.path()) {
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
