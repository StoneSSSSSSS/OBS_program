//! Library maintenance: index 24/7 recordings, enforce retention, scan the
//! clips folder, probe/thumbnail new files, and (once) tidy the folders the
//! old program created.

use crate::clips::{move_file, probe_and_thumb, remove_dir_if_empty};
use crate::shared::{media_path_allowed, require_media_path, LibCmd, Shared};
use cv_core::db::Db;
use cv_core::ipc::Event;
use cv_core::model::NewClip;
use cv_core::naming::{legacy_folder_to_game, merge_key, sanitize_component, unique_path};
use cv_core::filenames::{is_video_file, parse_timestamp_ms};
use cv_core::retention::{select_for_deletion, Policy, Reason, SegmentInfo};
use cv_core::util::{human_bytes, now_ms, system_time_ms};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const KEPT_DIR: &str = "Kept";
pub const EXPORTS_DIR: &str = "_Exports";
const META_LEGACY_REORG: &str = "legacy_reorg_v1";

pub fn run(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<LibCmd>) {
    let db = match Db::open_default() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("library: cannot open database: {e:#}");
            return;
        }
    };
    if !cv_core::paths::uses_local_storage() && sh.cfg.read().validate_media_folders().is_ok() {
        if let Err(e) = legacy_reorganize(&sh, &db) {
            tracing::warn!("folder tidy-up failed: {e:#}");
        }
    }
    let mut last_clip_scan: Option<Instant> = None;
    let mut force_scan = true;
    loop {
        if sh.is_shutting_down() {
            break;
        }
        let rec_changed = index_recordings(&sh, &db);
        if rec_changed {
            sh.emit(Event::RecordingsChanged);
        }
        retention(&sh, &db);
        let scan_due = last_clip_scan.map(|t| t.elapsed() > Duration::from_secs(300)).unwrap_or(true);
        if force_scan || scan_due {
            if scan_clips(&sh, &db) {
                sh.emit(Event::LibraryChanged);
            }
            last_clip_scan = Some(Instant::now());
            force_scan = false;
        }
        probe_pending(&sh, &db);
        update_disk_status(&sh, &db);

        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(LibCmd::Rescan) => force_scan = true,
            Ok(LibCmd::RecordingsChanged) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        for cmd in rx.try_iter() {
            force_scan |= matches!(cmd, LibCmd::Rescan);
        }
    }
}

fn list_videos(dir: &Path) -> Vec<PathBuf> {
    if !media_path_allowed(dir) {
        return Vec::new();
    }
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            media_path_allowed(p) && p.is_file() && is_video_file(p)
                && !name.starts_with(".cv_")
        }).collect())
        .unwrap_or_default()
}

/// Sync the recordings table with the folder. Returns true if anything changed.
fn index_recordings(sh: &Shared, db: &Db) -> bool {
    let cfg = sh.cfg();
    if cfg.validate_media_folders().is_err() {
        return false;
    }
    let folder = PathBuf::from(&cfg.recording.folder);
    if !media_path_allowed(&folder) || !folder.is_dir() {
        return false;
    }
    let _lock = sh.library_lock.lock();
    let mut changed = false;
    let mut present: HashSet<String> = HashSet::new();
    let existing: HashMap<String, cv_core::model::Recording> =
        db.list_recordings().unwrap_or_default().into_iter().map(|r| (r.path.to_lowercase(), r)).collect();
    let now = now_ms();

    let mut files: Vec<(PathBuf, bool)> = list_videos(&folder).into_iter().map(|p| (p, false)).collect();
    files.extend(list_videos(&folder.join(KEPT_DIR)).into_iter().map(|p| (p, true)));
    for (p, kept) in files {
        let path = p.to_string_lossy().to_string();
        present.insert(path.to_lowercase());
        let meta = p.metadata().ok();
        let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
        let prev = existing.get(&path.to_lowercase());
        let named_start = parse_timestamp_ms(&p);
        let start = prev.map(|r| r.start_ms).or(named_start)
            .or_else(|| meta.as_ref().and_then(|m| m.created().ok()).map(system_time_ms))
            .unwrap_or(now);
        let filetime = meta.as_ref().map(|m| std::os::windows::fs::MetadataExt::last_write_time(m) as i64);
        let is_live = sh.is_live(&path);
        let status = if is_live { "recording" } else { "complete" };
        let needs_upsert = prev.map(|r| r.size != size || r.status != status).unwrap_or(true);
        let id = if needs_upsert {
            changed = true;
            match db.upsert_recording(&path, start, size, status) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("index {path}: {e:#}");
                    continue;
                }
            }
        } else {
            prev.unwrap().id
        };
        if kept && !prev.map(|r| r.kept).unwrap_or(false) {
            let _ = db.set_recording_kept(id, true, &path);
            changed = true;
        }
        // Marked "keep" while it was still being written: move it now.
        if !kept && !is_live && prev.map(|r| r.kept).unwrap_or(false) {
            if let Ok(dest) = kept_destination(&p, &folder.join(KEPT_DIR)) {
                if move_file(&p, &dest, Duration::from_secs(5)).is_ok() {
                    let _ = db.set_recording_kept(id, true, &dest.to_string_lossy());
                    changed = true;
                    continue;
                }
            }
        }
        if is_live {
            let _ = db.set_recording_duration(id, ((now - start) as f64 / 1000.0).max(0.0));
        } else if prev.is_none_or(|r| !r.probed || r.size != size || r.probe_filetime != filetime || r.is_live()) {
            let info = cv_media::probe::probe(&p).ok().filter(|i| i.width > 0 && i.height > 0 && i.duration.is_finite() && i.duration > 0.0);
            // Native headers carry milliseconds. Honor explicitly renamed archive copies,
            // and use the header to disambiguate repeated local times during a DST fold.
            let native_start = info.as_ref().and_then(|i| i.start_ms).filter(|ms| {
                named_start.is_none_or(|named| {
                    named.div_euclid(1000) == ms.div_euclid(1000)
                        || cv_media::recorder::segment_name(named) == cv_media::recorder::segment_name(*ms)
                })
            });
            let start = native_start.or(named_start).unwrap_or(start);
            let (duration, tracks, valid) = info.map_or((0.0, 0, false), |i| (i.duration, i.audio.len() as i64, true));
            // Partial copies and locked files remain retryable. Filesystem age
            // is unrelated to the amount of playable media in an archive.
            if needs_upsert || prev.is_none_or(|r| r.start_ms != start || r.duration != duration || r.audio_tracks != tracks || r.probed != valid || r.probe_filetime != filetime) {
                changed |= db.set_recording_probe(id, start, duration, tracks, valid, filetime).is_ok();
            }
        }
    }
    // Forget files that are gone.
    for (k, r) in &existing {
        if Path::new(&r.path).starts_with(&folder) && !present.contains(k)
            && std::fs::metadata(&r.path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
            let _ = db.delete_recording_by_path(&r.path);
            changed = true;
        }
    }
    changed
}

fn retention(sh: &Shared, db: &Db) {
    let cfg = sh.cfg();
    if cfg.recording.rolling_seconds().is_some() || cfg.validate_media_folders().is_err() {
        return;
    }
    let folder = PathBuf::from(&cfg.recording.folder);
    if !media_path_allowed(&folder) {
        return;
    }
    let Some((free, _)) = crate::win::shell::disk_space(&folder) else { return };
    let _lock = sh.library_lock.lock();
    let Ok(archive) = cv_core::paths::media_path_key(&folder) else { return };
    let Ok(registered_clips) = db.clip_paths() else { return };
    let saved: HashSet<_> = registered_clips.iter().filter_map(|(_, path, _)| {
        cv_core::paths::media_path_key(Path::new(path)).ok()
    }).collect();
    let recs = db.list_recordings().unwrap_or_default();
    let segments: Vec<SegmentInfo> = recs
        .iter()
        .filter(|recording| recording.is_live() || (recording.probed && recording.duration > 0.0))
        .filter(|recording| retention_path_allowed(Path::new(&recording.path), &archive, &saved))
        .map(|r| SegmentInfo {
            path: PathBuf::from(&r.path),
            start_ms: r.start_ms,
            end_ms: if r.duration > 0.0 { r.end_ms() } else { r.start_ms },
            size: r.size.max(0) as u64,
            in_progress: r.is_live(),
            kept: r.kept,
        })
        .collect();
    let plan = select_for_deletion(&segments, now_ms(), free, Policy::from_config(&cfg.recording));
    if plan.is_empty() {
        return;
    }
    let mut deleted = 0u64;
    let mut count = 0;
    let mut reasons = HashSet::new();
    for (p, reason) in plan {
        // Re-check: never delete the file being recorded right now.
        let current = sh.cfg.read();
        if current.recording.rolling_seconds().is_some() || current.recording.folder != cfg.recording.folder
            || current.validate_media_folders().is_err() {
            break;
        }
        if sh.is_live(&p.to_string_lossy()) || !retention_path_allowed(&p, &archive, &saved) || !native_archive_media(&p) {
            continue;
        }
        let size = p.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&p) {
            Ok(()) => {
                let _ = db.delete_recording_by_path(&p.to_string_lossy());
                deleted += size;
                count += 1;
                reasons.insert(reason);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = db.delete_recording_by_path(&p.to_string_lossy());
            }
            Err(e) => tracing::warn!("could not delete old recording {}: {e}", p.display()),
        }
    }
    if count > 0 {
        let why = if reasons.contains(&Reason::LowDisk) {
            "to keep free disk space"
        } else if reasons.contains(&Reason::TotalSize) {
            "to stay under the storage cap"
        } else {
            "older than the retention window"
        };
        sh.log_event("info", format!("Deleted {count} old recording(s), {} ({why})", human_bytes(deleted)));
        sh.emit(Event::RecordingsChanged);
    }
}

fn retention_path_allowed(path: &Path, archive: &Path, saved: &HashSet<PathBuf>) -> bool {
    media_path_allowed(path) && cv_core::paths::media_path_key(path).is_ok_and(|key| key.starts_with(archive) && !saved.contains(&key))
}

fn native_archive_media(path: &Path) -> bool {
    // Native recordings have ClipVault timing metadata and use fragmented
    // MP4. Saved outputs are flat MP4 even when they have a source timestamp.
    cv_media::probe::probe(path).is_ok_and(|info| info.fragmented && info.start_ms.is_some())
}

/// Find clips added/removed outside the app. Returns true if anything changed.
fn scan_clips(sh: &Shared, db: &Db) -> bool {
    let cfg = sh.cfg();
    if cfg.validate_media_folders().is_err() {
        return false;
    }
    let root = PathBuf::from(&cfg.clips.folder);
    if !media_path_allowed(&root) {
        return false;
    }
    if !root.is_dir() {
        let _ = std::fs::create_dir_all(&root);
        if !root.is_dir() {
            return false; // drive missing: don't mark everything as missing
        }
    }
    let _lock = sh.library_lock.lock();
    let mut changed = false;
    let known: HashMap<String, (i64, bool)> =
        db.clip_paths().unwrap_or_default().into_iter().map(|(id, p, m)| (p.to_lowercase(), (id, m))).collect();
    let mut found: HashSet<String> = HashSet::new();
    let Ok(clips_key) = cv_core::paths::media_path_key(&root) else { return false };
    let Ok(exports_key) = cv_core::paths::media_path_key(&cfg.exports_dir()) else { return false };
    let mut files: Vec<(PathBuf, Option<String>)> = list_videos(&root).into_iter().map(|p| (p, None)).collect();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.eq_ignore_ascii_case(EXPORTS_DIR) || name.starts_with('.') {
                continue;
            }
            if !clips_path_allowed(&p, &clips_key, &exports_key) {
                continue;
            }
            for f in list_videos(&p) {
                files.push((f, Some(name.clone())));
            }
        }
    }
    let now = now_ms();
    for (p, folder) in files {
        if !clips_path_allowed(&p, &clips_key, &exports_key) {
            continue;
        }
        let key = p.to_string_lossy().to_lowercase();
        found.insert(key.clone());
        match known.get(&key) {
            Some((id, true)) => {
                let _ = db.set_clip_missing(*id, false);
                changed = true;
            }
            Some(_) => {}
            None => {
                if !cfg.clips.import_untracked_files {
                    continue;
                }
                // Skip files still being written.
                let modified = p.metadata().and_then(|m| m.modified()).map(system_time_ms).unwrap_or(0);
                if now - modified < 10_000 {
                    continue;
                }
                let game_id = folder.as_ref().and_then(|f| game_for_folder(db, f).ok());
                let created = parse_timestamp_ms(&p).unwrap_or(modified);
                let title = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let size = p.metadata().map(|m| m.len() as i64).unwrap_or(0);
                match db.insert_clip(&NewClip {
                    path: p.to_string_lossy().to_string(),
                    title,
                    game_id,
                    created_at: created,
                    source_start_ms: None,
                    size,
                    source: "import".into(),
                    favorite: false,
                }) {
                    Ok(_) => changed = true,
                    Err(e) => tracing::warn!("import {}: {e:#}", p.display()),
                }
            }
        }
    }
    for (path, (id, missing)) in &known {
        if !missing && !found.contains(path) && !Path::new(path).exists() {
            let _ = db.set_clip_missing(*id, true);
            changed = true;
        }
    }
    changed
}

fn clips_path_allowed(path: &Path, clips: &Path, exports: &Path) -> bool {
    cv_core::paths::media_path_key(path).is_ok_and(|key| key.starts_with(clips) && !key.starts_with(exports))
}

/// Game row for a clips sub-folder (created on first sight).
fn game_for_folder(db: &Db, folder: &str) -> anyhow::Result<i64> {
    let name = legacy_folder_to_game(folder);
    if let Some(g) = db.game_by_name(&name)? {
        return Ok(g.id);
    }
    db.upsert_game(&format!("folder:{}", merge_key(&name)), &name, "", true, now_ms())
}

fn probe_pending(sh: &Shared, db: &Db) {
    let pending = db.unprobed_clips(16).unwrap_or_default();
    if pending.is_empty() {
        return;
    }
    let thumbs = sh.cfg.read().clips.generate_thumbnails;
    let mut any = false;
    for (id, path) in pending {
        if sh.is_shutting_down() {
            break;
        }
        let p = PathBuf::from(&path);
        if media_path_allowed(&p) && p.is_file() {
            probe_and_thumb(db, id, &p, thumbs);
            any = true;
        }
    }
    if any {
        sh.emit(Event::LibraryChanged);
    }
}

fn update_disk_status(sh: &Shared, db: &Db) {
    let cfg = sh.cfg();
    let recs = db.list_recordings().unwrap_or_default();
    let (clips_count, clips_bytes) = db.clip_stats().unwrap_or((0, 0));
    let folder = Path::new(&cfg.recording.folder);
    let (free, total) = if media_path_allowed(folder) {
        crate::win::shell::disk_space(folder).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    sh.update_status(|s| {
        s.disk.free_bytes = free;
        s.disk.total_bytes = total;
        s.disk.recordings_bytes = recs.iter().map(|r| r.size.max(0) as u64).sum();
        s.disk.recordings_count = recs.len() as u32;
        s.disk.oldest_ms = recs.iter().map(|r| r.start_ms).min();
        s.disk.clips_count = clips_count as u32;
        s.disk.clips_bytes = clips_bytes as u64;
    });
}

/// One-time: merge folders the old program created from raw window titles
/// (e.g. six "ARC Raiders" folders full of invisible characters) into clean
/// per-game folders. Every move is logged to `migration.log` in the data dir.
fn legacy_reorganize(sh: &Shared, db: &Db) -> anyhow::Result<()> {
    if db.meta_get(META_LEGACY_REORG)?.is_some() {
        return Ok(());
    }
    let cfg = sh.cfg();
    let root = PathBuf::from(&cfg.clips.folder);
    let clips = cv_core::paths::media_path_key(&root)?;
    let exports = cv_core::paths::media_path_key(&cfg.exports_dir())?;
    if !root.is_dir() {
        return Ok(()); // try again next start
    }
    let _lock = sh.library_lock.lock();
    let log_path = cv_core::paths::data_dir().join("migration.log");
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    writeln!(log, "== folder tidy-up {} ==", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))?;
    let mut moved = 0;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.eq_ignore_ascii_case(EXPORTS_DIR) {
            continue;
        }
        let key = cv_core::paths::media_path_key(&dir)?;
        if !clips_path_allowed(&dir, &clips, &exports) || exports.starts_with(&key) {
            continue;
        }
        let clean = sanitize_component(&legacy_folder_to_game(&name));
        if clean == name {
            continue;
        }
        let target = root.join(&clean);
        cfg.validate_clip_destination(&target)?;
        for f in std::fs::read_dir(&dir)?.flatten().map(|e| e.path()) {
            if !f.is_file() || !clips_path_allowed(&f, &clips, &exports) {
                continue;
            }
            let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let ext = f.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            let dest = unique_path(&target, &stem, &ext, None);
            match move_file(&f, &dest, Duration::from_secs(5)) {
                Ok(()) => {
                    writeln!(log, "{} -> {}", f.display(), dest.display())?;
                    if let Some(c) = db.clip_by_path(&f.to_string_lossy())? {
                        db.set_clip_path_and_title(c.id, &dest.to_string_lossy(), &c.title)?;
                    }
                    moved += 1;
                }
                Err(e) => writeln!(log, "FAILED {}: {e:#}", f.display())?,
            }
        }
        remove_dir_if_empty(Some(&dir));
    }
    db.meta_set(META_LEGACY_REORG, &now_ms().to_string())?;
    if moved > 0 {
        sh.log_event("info", format!("Tidied {moved} old clip(s) into per-game folders (see migration.log)"));
    }
    Ok(())
}

/// Move a recording into/out of the Kept folder (protected from retention).
pub fn set_kept(sh: &Shared, recording_id: i64, keep: bool) -> anyhow::Result<()> {
    let _lock = sh.library_lock.lock();
    sh.cfg.read().validate_media_folders()?;
    let db = Db::open_default()?;
    let rec = db.recording(recording_id)?.ok_or_else(|| anyhow::anyhow!("recording not found"))?;
    if rec.kept == keep {
        return Ok(());
    }
    anyhow::ensure!(!rec.is_live() || keep, "can't move the recording that's being written");
    let src = PathBuf::from(&rec.path);
    require_media_path(&src)?;
    let folder = PathBuf::from(&sh.cfg.read().recording.folder);
    let dest_dir = if keep { folder.join(KEPT_DIR) } else { folder };
    if rec.is_live() {
        // Can't move the file being recorded: flag it, the move happens later.
        db.set_recording_kept(recording_id, true, &rec.path)?;
        return Ok(());
    }
    let dest = kept_destination(&src, &dest_dir)?;
    move_file(&src, &dest, Duration::from_secs(10))?;
    db.set_recording_kept(recording_id, keep, &dest.to_string_lossy())?;
    sh.emit(Event::RecordingsChanged);
    Ok(())
}

fn kept_destination(source: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
    let stem = source.file_stem().and_then(|stem| stem.to_str()).ok_or_else(|| anyhow::anyhow!("Invalid recording filename"))?;
    let extension = source.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
    let destination = unique_path(dir, stem, extension, Some(source));
    require_media_path(&destination)?;
    Ok(destination)
}
