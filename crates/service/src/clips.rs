//! Saving clips (from the clip hotkey) and clip file operations (rename /
//! move to another game / delete) used by the GUI.

use crate::shared::{ClipTask, Footage, Shared};
use crate::win::ui::ToastLevel;
use anyhow::{bail, Context, Result};
use cv_core::db::Db;
use cv_core::games::{attribute, Span};
use cv_core::ipc::Event;
use cv_core::model::NewClip;
use cv_core::naming::{render_template, sanitize_component, unique_path};
use cv_core::util::{format_duration, now_ms};
use cv_media::cut::{cut, CutSource};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DESKTOP_KEY: &str = "desktop";

pub fn run(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<ClipTask>) {
    while let Ok(task) = rx.recv() {
        if sh.is_shutting_down() {
            break;
        }
        match task {
            ClipTask::SaveRange { footage, from_ms, to_ms, short } => match save_range(&sh, &footage, from_ms, to_ms, short) {
                Ok((id, title, game, dur)) => {
                    sh.log_event("info", format!("Saved clip \"{title}\" ({game}, {})", format_duration(dur)));
                    sh.toast(ToastLevel::Success, format!("Clip saved: {game} · {}", format_duration(dur)));
                    sh.emit(Event::ClipAdded { clip_id: id });
                    sh.emit(Event::LibraryChanged);
                }
                Err(e) => {
                    sh.log_event("error", format!("Could not save the clip: {e:#}"));
                    sh.toast(ToastLevel::Error, format!("Clip could not be saved: {e}"));
                }
            },
        }
    }
}

/// Cut plan: the parts of `footage` covering `[from_ms, to_ms)`.
pub fn plan(footage: &[Footage], from_ms: i64, to_ms: i64) -> Vec<CutSource> {
    let mut v: Vec<&Footage> = footage.iter().collect();
    v.sort_by_key(|f| f.start_ms);
    v.into_iter()
        .filter_map(|f| {
            let end = f.start_ms + (f.duration_s * 1000.0) as i64;
            let a = from_ms.max(f.start_ms);
            let b = to_ms.min(end);
            (b > a + 100).then(|| CutSource { path: f.path.clone(), from_s: (a - f.start_ms) as f64 / 1000.0, to_s: Some((b - f.start_ms) as f64 / 1000.0) })
        })
        .collect()
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
                // 32 = sharing violation, 5 = access denied (antivirus scanning).
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

/// A temporary output path next to where the clip will end up.
pub fn temp_beside(dir: &Path) -> PathBuf {
    dir.join(format!(".cv_{}.mp4", now_ms()))
}

fn save_range(sh: &Shared, footage: &[Footage], from_ms: i64, to_ms: i64, short: bool) -> Result<(i64, String, String, f64)> {
    let cfg = sh.cfg();
    let parts = plan(footage, from_ms, to_ms);
    if parts.is_empty() {
        bail!("nothing has been recorded yet");
    }
    let db = Db::open_default()?;
    let (game_id, game_name, exe) = attribute_game(&db, from_ms, to_ms, &cfg.clips.unknown_game_name)?;
    let local = chrono::DateTime::from_timestamp_millis(to_ms).unwrap_or_default().with_timezone(&chrono::Local);
    let title = render_template(&cfg.clips.name_template, &game_name, &exe, local);
    let dir = clip_dir(&cfg, &game_name);
    std::fs::create_dir_all(&dir)?;
    let tmp = temp_beside(&dir);
    // The clip starts where the first part really starts (may be later than
    // asked if the recording began recently).
    let first = footage.iter().find(|f| f.path == parts[0].path).map(|f| f.start_ms).unwrap_or(from_ms);
    let start_ms = first + (parts[0].from_s * 1000.0) as i64;
    let out = match cut(&parts, &tmp, Some(start_ms), &mut |_| {}, &AtomicBool::new(false)) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let _lock = sh.library_lock.lock();
    let dest = unique_path(&dir, &title, "mp4", None);
    move_file(&tmp, &dest, Duration::from_secs(10))?;
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
    let is_game = db.game(game_id)?.map(|g| g.is_game()).unwrap_or(false);
    let id = db.insert_clip(&NewClip {
        path: dest.to_string_lossy().to_string(),
        title: stem.clone(),
        game_id: Some(game_id),
        created_at: to_ms,
        source_start_ms: Some(start_ms),
        size: dest.metadata().map(|m| m.len() as i64).unwrap_or(0),
        source: "replay".into(),
        favorite: cfg.clips.auto_favorite && is_game,
    })?;
    drop(_lock);
    let _ = short;
    probe_and_thumb(&db, id, &dest, cfg.clips.generate_thumbnails);
    copy_wave(footage, &parts, &dest);
    Ok((id, stem, game_name, out.duration))
}

/// Give a new clip a waveform right away by decoding it (fast for clips).
fn copy_wave(_footage: &[Footage], _parts: &[CutSource], dest: &Path) {
    let dest = dest.to_path_buf();
    std::thread::spawn(move || {
        if let Ok(w) = cv_media::wave::generate(&dest, 0.0, None, &AtomicBool::new(false)) {
            let _ = w.save(&cv_core::waveform::wave_path(&dest));
        }
    });
}

/// Fill in duration/tracks and make a thumbnail.
pub fn probe_and_thumb(db: &Db, id: i64, path: &Path, thumbs: bool) {
    match cv_media::probe::probe(path) {
        Ok(info) => {
            let names: Vec<String> = info.audio.iter().map(|a| a.name.clone()).collect();
            let _ = db.update_clip_probe(id, info.duration, info.size as i64, info.width as i64, info.height as i64, info.fps, info.audio.len() as i64, &names);
            if thumbs {
                let out = cv_core::paths::thumbs_dir().join(format!("clip_{id}.jpg"));
                let at = (info.duration * 0.85).max(0.0);
                match cv_media::thumb::thumbnail(path, at, &out, 480) {
                    Ok(()) => {
                        let _ = db.set_clip_thumb(id, &out.to_string_lossy());
                    }
                    Err(e) => {
                        tracing::warn!("thumbnail for clip {id}: {e:#}");
                        let _ = db.set_clip_thumb(id, "-");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("can't read {}: {e:#}", path.display());
            // Mark as probed with what we know so we don't retry forever.
            let size = path.metadata().map(|m| m.len() as i64).unwrap_or(0);
            let _ = db.update_clip_probe(id, 0.0, size, 0, 0, 0.0, 0, &[]);
            let _ = db.set_clip_thumb(id, "-");
        }
    }
}

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
        move_wave(&src, &dest);
    }
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
    db.set_clip_path_and_title(clip_id, &dest.to_string_lossy(), &stem)?;
    Ok(())
}

/// Keep a clip's waveform when its file moves.
fn move_wave(src: &Path, dest: &Path) {
    let (a, b) = (cv_core::waveform::wave_path(src), cv_core::waveform::wave_path(dest));
    if a.exists() {
        let _ = std::fs::rename(a, b);
    }
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
            move_wave(&src, &dest);
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
        let _ = std::fs::remove_file(cv_core::waveform::wave_path(&p));
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
    fn move_within_volume() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        std::fs::write(&a, b"data").unwrap();
        let b = dir.path().join("sub").join("b.mp4");
        move_file(&a, &b, Duration::from_secs(1)).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"data");
        assert!(move_file(&a, &b, Duration::from_millis(100)).is_err());
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
    fn cut_plans_across_files() {
        let f = |p: &str, s: i64, d: f64| Footage { path: PathBuf::from(p), start_ms: s, duration_s: d };
        let footage = vec![f("b", 60_000, 60.0), f("a", 0, 60.0), f("c", 200_000, 30.0)];
        let p = plan(&footage, 50_000, 210_000);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].path, PathBuf::from("a"));
        assert_eq!((p[0].from_s, p[0].to_s), (50.0, Some(60.0)));
        assert_eq!((p[1].from_s, p[1].to_s), (0.0, Some(60.0)));
        assert_eq!((p[2].from_s, p[2].to_s), (0.0, Some(10.0)));
        assert!(plan(&footage, 125_000, 199_000).is_empty(), "gaps have nothing");
    }

    #[test]
    fn attribution_falls_back_to_desktop() {
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
