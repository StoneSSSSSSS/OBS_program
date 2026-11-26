//! Turning saved replays into organised clips, plus clip file operations
//! (rename / move to another game / delete) used by the GUI.

use crate::shared::{ClipTask, Shared};
use crate::win::ui::ToastLevel;
use anyhow::{bail, Context, Result};
use cv_core::db::Db;
use cv_core::games::{attribute, Span};
use cv_core::ipc::Event;
use cv_core::model::NewClip;
use cv_core::naming::{render_template, sanitize_component, unique_path};
use cv_core::timeline::CutPart;
use cv_core::util::{format_duration, now_ms};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DESKTOP_KEY: &str = "desktop";

pub fn run(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<ClipTask>) {
    let mut failed: HashSet<PathBuf> = HashSet::new();
    while let Ok(task) = rx.recv() {
        if sh.is_shutting_down() {
            break;
        }
        let (path, at_ms, short, stray) = match task {
            ClipTask::ReplaySaved { path, at_ms, short } => (path, at_ms, short, false),
            ClipTask::ImportStray { path } => {
                if failed.contains(&path) {
                    continue;
                }
                let at = cv_core::obs::filename::parse_timestamp_ms(&path)
                    .or_else(|| path.metadata().and_then(|m| m.modified()).ok().map(cv_core::util::system_time_ms))
                    .unwrap_or_else(now_ms);
                (path, at, false, true)
            }
        };
        match save_replay(&sh, &path, at_ms, short) {
            Ok((id, title, game, dur)) => {
                sh.log_event("info", format!("Saved clip \"{title}\" ({game}, {})", format_duration(dur)));
                if !stray {
                    sh.toast(ToastLevel::Success, format!("Clip saved: {game} · {}", format_duration(dur)));
                }
                sh.emit(Event::ClipAdded { clip_id: id });
                sh.emit(Event::LibraryChanged);
            }
            Err(e) => {
                sh.log_event("error", format!("Could not save clip {}: {e:#}", path.display()));
                sh.toast(ToastLevel::Error, format!("Clip could not be filed: {e}"));
                failed.insert(path);
            }
        }
    }
}

/// Wait for a file to exist and stop growing.
fn wait_stable(path: &Path, max: Duration) -> Result<u64> {
    let start = Instant::now();
    let mut last = None;
    loop {
        if let Ok(m) = path.metadata() {
            let len = m.len();
            if last == Some(len) && len > 0 {
                return Ok(len);
            }
            last = Some(len);
        }
        if start.elapsed() > max {
            if let Some(l) = last {
                return Ok(l);
            }
            bail!("{} never appeared", path.display());
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Move a file, retrying while it's locked; copies across drives.
pub fn move_file(src: &Path, dest: &Path, retry_for: Duration) -> Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    }
    let start = Instant::now();
    loop {
        match std::fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(17) => {
                // ERROR_NOT_SAME_DEVICE: copy, verify, delete.
                let tmp = dest.with_extension("partial");
                std::fs::copy(src, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
                let (a, b) = (src.metadata()?.len(), tmp.metadata()?.len());
                if a != b {
                    let _ = std::fs::remove_file(&tmp);
                    bail!("copy verification failed ({a} vs {b} bytes)");
                }
                std::fs::rename(&tmp, dest)?;
                let mut tries = 0;
                while std::fs::remove_file(src).is_err() && tries < 50 {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(200));
                }
                return Ok(());
            }
            Err(e) => {
                // 32 = sharing violation, 5 = access denied (still being written / scanned).
                let transient = matches!(e.raw_os_error(), Some(32) | Some(5) | Some(33));
                if !transient || start.elapsed() > retry_for {
                    return Err(e).with_context(|| format!("moving {} -> {}", src.display(), dest.display()));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Resolve the game a clip covering [start, end] belongs to.
pub fn attribute_game(db: &Db, start_ms: i64, end_ms: i64, unknown_name: &str) -> Result<(i64, String, String)> {
    let spans: Vec<Span> = db
        .activity_between(start_ms - 5000, end_ms)?
        .into_iter()
        .map(|a| Span { key: a.key, is_game: a.is_game, start_ms: a.start_ms, end_ms: a.end_ms.max(a.start_ms + 1000) })
        .collect();
    if let Some((key, _)) = attribute(&spans, start_ms, end_ms) {
        if let Some(g) = db.game_by_key(&key)? {
            let exe = cv_core::games::exe_name_of(&g.exe_path);
            return Ok((g.id, g.name, exe));
        }
    }
    let id = db.upsert_game(DESKTOP_KEY, unknown_name, "", false, now_ms())?;
    Ok((id, db.game(id)?.map(|g| g.name).unwrap_or_else(|| unknown_name.to_string()), String::new()))
}

pub fn clip_dir(cfg: &cv_core::config::Config, game_name: &str) -> PathBuf {
    let root = PathBuf::from(&cfg.clips.folder);
    if cfg.clips.organize_by_game {
        root.join(sanitize_component(game_name))
    } else {
        root
    }
}

fn save_replay(sh: &Shared, src: &Path, at_ms: i64, short: bool) -> Result<(i64, String, String, f64)> {
    let cfg = sh.cfg();
    wait_stable(src, Duration::from_secs(15))?;
    let tools = sh.tools();
    let info = tools.as_ref().and_then(|t| t.probe(src).ok());
    let dur = info.as_ref().map(|i| i.duration).filter(|d| *d > 0.0).unwrap_or(cfg.clips.clip_length_secs as f64);
    let end_ms = at_ms;
    let start_ms = end_ms - (dur * 1000.0) as i64;

    let db = Db::open_default()?;
    let (game_id, game_name, exe) = attribute_game(&db, start_ms, end_ms, &cfg.clips.unknown_game_name)?;
    let local = chrono::DateTime::from_timestamp_millis(end_ms).unwrap_or_default().with_timezone(&chrono::Local);
    let title = render_template(&cfg.clips.name_template, &game_name, &exe, local);
    let dir = clip_dir(&cfg, &game_name);

    let _lock = sh.library_lock.lock();
    let dest = unique_path(&dir, &title, "mp4", None);
    let final_dur;
    if short && dur > cfg.clips.short_clip_secs as f64 + 1.0 {
        let tools = tools.as_ref().context("ffmpeg is needed for short clips")?;
        std::fs::create_dir_all(&dir)?;
        let parts = [CutPart { path: src.to_path_buf(), in_s: dur - cfg.clips.short_clip_secs as f64, out_s: None }];
        let tmp = dest.with_extension("tmp.mp4");
        let args = cv_core::ffmpeg::cut_args(&parts, &tmp, Path::new("unused"), false, 0, &Default::default())?;
        tools.run_progress(&args, cfg.clips.short_clip_secs as f64, &mut |_| {}, &AtomicBool::new(false))?;
        std::fs::rename(&tmp, &dest)?;
        let _ = std::fs::remove_file(src);
        final_dur = cfg.clips.short_clip_secs as f64;
    } else {
        move_file(src, &dest, MOVE_RETRY)?;
        final_dur = dur;
    }
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
    let size = dest.metadata().map(|m| m.len() as i64).unwrap_or(0);
    let is_game = db.game(game_id)?.map(|g| g.is_game()).unwrap_or(false);
    let id = db.insert_clip(&NewClip {
        path: dest.to_string_lossy().to_string(),
        title: stem.clone(),
        game_id: Some(game_id),
        created_at: end_ms,
        source_start_ms: Some(end_ms - (final_dur * 1000.0) as i64),
        size,
        source: "replay".into(),
        favorite: cfg.clips.auto_favorite && is_game,
    })?;
    drop(_lock);
    if let Some(t) = &tools {
        probe_and_thumb(&db, t, id, &dest, cfg.clips.generate_thumbnails);
    }
    Ok((id, stem, game_name, final_dur))
}

/// Fill in duration/tracks and make a thumbnail.
pub fn probe_and_thumb(db: &Db, tools: &cv_core::ffmpeg::Tools, id: i64, path: &Path, thumbs: bool) {
    match tools.probe(path) {
        Ok(info) => {
            let names: Vec<String> = info
                .audio
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    if !a.title.is_empty() {
                        a.title.clone()
                    } else if info.audio.len() == cv_core::config::TRACK_NAMES.len() {
                        cv_core::config::TRACK_NAMES[i].to_string()
                    } else if i == 0 {
                        "Audio".to_string()
                    } else {
                        format!("Track {}", i + 1)
                    }
                })
                .collect();
            let _ = db.update_clip_probe(
                id,
                info.duration,
                info.size as i64,
                info.width as i64,
                info.height as i64,
                info.fps,
                info.audio.len() as i64,
                &names,
            );
            if thumbs {
                let out = cv_core::paths::thumbs_dir().join(format!("clip_{id}.jpg"));
                let at = (info.duration * 0.85).max(0.0);
                match tools.thumbnail(path, at, &out, 480) {
                    Ok(()) => {
                        let _ = db.set_clip_thumb(id, &out.to_string_lossy());
                    }
                    Err(e) => tracing::warn!("thumbnail for clip {id}: {e:#}"),
                }
            }
        }
        Err(e) => {
            tracing::warn!("probe failed for {}: {e:#}", path.display());
            // Mark as probed with what we know so we don't retry forever.
            let size = path.metadata().map(|m| m.len() as i64).unwrap_or(0);
            let _ = db.update_clip_probe(id, 0.0, size, 0, 0, 0.0, 0, &[]);
            let _ = db.set_clip_thumb(id, "-");
        }
    }
}

/// How long to keep retrying a move while OBS/antivirus still holds the file.
const MOVE_RETRY: Duration = Duration::from_secs(120);

// ------------------------------------------------------------ file ops

pub fn rename_clip(sh: &Shared, clip_id: i64, title: &str) -> Result<()> {
    let db = Db::open_default()?;
    let clip = db.clip(clip_id)?.context("clip not found")?;
    let title = sanitize_component(title);
    let src = PathBuf::from(&clip.path);
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp4".into());
    let _lock = sh.library_lock.lock();
    let dest = unique_path(&dir, &title, &ext, Some(&src));
    if dest.to_string_lossy() != clip.path {
        move_file(&src, &dest, Duration::from_secs(10))?;
    }
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
    db.set_clip_path_and_title(clip_id, &dest.to_string_lossy(), &stem)?;
    Ok(())
}

pub fn set_clip_game(sh: &Shared, clip_id: i64, game_id: Option<i64>) -> Result<()> {
    let cfg = sh.cfg();
    let db = Db::open_default()?;
    let clip = db.clip(clip_id)?.context("clip not found")?;
    let game_name = match game_id {
        Some(g) => db.game(g)?.context("game not found")?.name,
        None => cfg.clips.unknown_game_name.clone(),
    };
    let src = PathBuf::from(&clip.path);
    let _lock = sh.library_lock.lock();
    if cfg.clips.organize_by_game && src.is_file() {
        let dir = clip_dir(&cfg, &game_name);
        let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp4".into());
        if src.parent() != Some(dir.as_path()) {
            let dest = unique_path(&dir, &stem, &ext, None);
            move_file(&src, &dest, Duration::from_secs(10))?;
            let new_stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(stem);
            db.set_clip_path_and_title(clip_id, &dest.to_string_lossy(), &new_stem)?;
            remove_dir_if_empty(src.parent());
        }
    }
    db.set_clip_game(clip_id, game_id)?;
    Ok(())
}

pub fn delete_clips(sh: &Shared, ids: &[i64]) -> Result<usize> {
    let cfg = sh.cfg();
    let db = Db::open_default()?;
    let _lock = sh.library_lock.lock();
    let mut n = 0;
    for id in ids {
        let Some(clip) = db.clip(*id)? else { continue };
        let p = PathBuf::from(&clip.path);
        if p.exists() {
            if cfg.clips.delete_to_recycle_bin {
                crate::win::shell::recycle(&[p.as_path()])?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
        if !clip.thumb.is_empty() && clip.thumb != "-" {
            let _ = std::fs::remove_file(&clip.thumb);
        }
        db.delete_clip(*id)?;
        remove_dir_if_empty(p.parent());
        n += 1;
    }
    Ok(n)
}

pub fn remove_dir_if_empty(dir: Option<&Path>) {
    if let Some(d) = dir {
        if std::fs::read_dir(d).map(|mut r| r.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_within_volume_and_waits() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        std::fs::write(&a, b"data").unwrap();
        assert_eq!(wait_stable(&a, Duration::from_secs(3)).unwrap(), 4);
        let b = dir.path().join("sub").join("b.mp4");
        move_file(&a, &b, Duration::from_secs(1)).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"data");
        assert!(move_file(&a, &b, Duration::from_millis(100)).is_err());
        assert!(wait_stable(&a, Duration::from_millis(500)).is_err());
    }

    #[test]
    fn empty_dir_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("empty");
        std::fs::create_dir(&d).unwrap();
        remove_dir_if_empty(Some(&d));
        assert!(!d.exists());
        let d2 = dir.path().join("full");
        std::fs::create_dir(&d2).unwrap();
        std::fs::write(d2.join("x"), b"1").unwrap();
        remove_dir_if_empty(Some(&d2));
        assert!(d2.exists());
    }

    #[test]
    fn attribution_falls_back_to_desktop() {
        // Uses an in-memory-like temp DB through a direct Db handle.
        let db = Db::open_in_memory().unwrap();
        let (id, name, exe) = attribute_game(&db, 0, 10_000, "Desktop").unwrap();
        assert_eq!((name.as_str(), exe.as_str()), ("Desktop", ""));
        let g = db.upsert_game("cs2.exe", "Counter-Strike 2", r"C:\cs2.exe", true, 0).unwrap();
        let a = db.start_activity("cs2.exe", Some(g), true, "CS2", 0).unwrap();
        db.extend_activity(a, 10_000, None).unwrap();
        let (gid, name, exe) = attribute_game(&db, 0, 10_000, "Desktop").unwrap();
        assert_eq!((gid, name.as_str(), exe.as_str()), (g, "Counter-Strike 2", "cs2.exe"));
        assert_ne!(id, gid);
    }
}
