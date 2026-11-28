//! Background jobs: clipping from the 24/7 recording, trimming, exporting.
//! Run one at a time on their own thread with progress reported to the GUI.

use crate::clips::{attribute_game, clip_dir, move_file, plan, probe_and_thumb, temp_beside};
use crate::shared::{Footage, Shared};
use crate::win::ui::{ToastLevel, UiCmd};
use anyhow::{Context, Result};
use cv_core::db::Db;
use cv_core::ipc::{Event, ExportSource, JobInfo};
use cv_core::mixer::{compose, compose_slot, MixSettings, TrackSource};
use cv_core::model::{rebase_edits, AudioEdit, NewClip};
use cv_core::naming::{render_template, sanitize_component, unique_path};
use cv_core::util::now_ms;
use cv_media::cut::{cut, CutSource, CutSpan};
use cv_media::export::{bitrate_for_size, export as run_export, ExportJob, VideoOut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum JobKind {
    CutRange { from_ms: i64, to_ms: i64, title: String, game_id: Option<i64>, category_ids: Vec<i64>, mix: Option<MixSettings>, audio_edits: Vec<AudioEdit> },
    Trim { clip_id: i64, start_s: f64, end_s: f64, replace: bool },
    Export { source: ExportSource, range: Option<(f64, f64)>, mix: MixSettings, edits: Option<Vec<AudioEdit>>, target_size_mb: Option<u32>, title: String },
}

pub struct JobRequest {
    pub id: u64,
    pub kind: JobKind,
    pub cancel: Arc<AtomicBool>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CANCELS: once_cell::sync::Lazy<parking_lot::Mutex<Vec<(u64, Arc<AtomicBool>)>>> = once_cell::sync::Lazy::new(Default::default);

/// Queue a job; returns its id.
pub fn submit(sh: &Shared, kind: JobKind) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let cancel = Arc::new(AtomicBool::new(false));
    CANCELS.lock().push((id, cancel.clone()));
    let (label, title) = match &kind {
        JobKind::CutRange { title, .. } => ("clip", if title.is_empty() { "New clip".to_string() } else { title.clone() }),
        JobKind::Trim { .. } => ("trim", "Trimming clip".to_string()),
        JobKind::Export { title, target_size_mb, .. } => ("export", if title.is_empty() { match target_size_mb { Some(mb) => format!("Sharing (under {mb} MB)"), None => "Exporting".into() } } else { title.clone() }),
    };
    let info = JobInfo { id, kind: label.into(), title, progress: 0.0, state: "queued".into(), ..Default::default() };
    sh.jobs.lock().push(info.clone());
    sh.emit(Event::Job { job: info });
    let _ = sh.job_tx.send(JobRequest { id, kind, cancel });
    id
}

pub fn cancel(id: u64) -> bool {
    let c = CANCELS.lock();
    if let Some((_, flag)) = c.iter().find(|(i, _)| *i == id) {
        flag.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

fn update(sh: &Shared, id: u64, f: impl FnOnce(&mut JobInfo)) {
    let mut jobs = sh.jobs.lock();
    if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
        f(j);
        let j = j.clone();
        drop(jobs);
        sh.emit(Event::Job { job: j });
    }
}

pub fn run(sh: Arc<Shared>, rx: crossbeam_channel::Receiver<JobRequest>) {
    while let Ok(req) = rx.recv() {
        if sh.is_shutting_down() {
            break;
        }
        let id = req.id;
        if req.cancel.load(Ordering::SeqCst) {
            update(&sh, id, |j| j.state = "cancelled".into());
            CANCELS.lock().retain(|(i, _)| *i != id);
            continue;
        }
        update(&sh, id, |j| j.state = "running".into());
        let mut last_emit = Instant::now();
        let mut progress = |p: f32| {
            if last_emit.elapsed() > Duration::from_millis(250) || p >= 1.0 {
                last_emit = Instant::now();
                update(&sh, id, |j| j.progress = p);
            }
        };
        let result = match &req.kind {
            JobKind::CutRange { from_ms, to_ms, title, game_id, category_ids, mix, audio_edits } => cut_range(&sh, *from_ms, *to_ms, title, *game_id, category_ids, mix.as_ref(), audio_edits, &mut progress, &req.cancel),
            JobKind::Trim { clip_id, start_s, end_s, replace } => trim(&sh, *clip_id, *start_s, *end_s, *replace, &mut progress, &req.cancel),
            JobKind::Export { source, range, mix, edits, target_size_mb, title } => export(&sh, source, *range, mix, edits.as_deref(), *target_size_mb, title, &mut progress, &req.cancel),
        };
        match result {
            Ok((output, clip_id)) => {
                update(&sh, id, |j| {
                    j.state = "done".into();
                    j.progress = 1.0;
                    j.output = Some(output.to_string_lossy().to_string());
                    j.clip_id = clip_id;
                });
                if let Some(c) = clip_id {
                    sh.emit(Event::ClipAdded { clip_id: c });
                }
                sh.emit(Event::LibraryChanged);
            }
            Err(e) => {
                let cancelled = req.cancel.load(Ordering::SeqCst);
                let msg = if cancelled { "Cancelled".to_string() } else { format!("{e:#}") };
                if !cancelled {
                    sh.log_event("error", format!("Job failed: {msg}"));
                    sh.toast(ToastLevel::Error, format!("Failed: {msg}"));
                }
                update(&sh, id, |j| {
                    j.state = if cancelled { "cancelled".into() } else { "failed".into() };
                    j.message = msg;
                });
            }
        }
        CANCELS.lock().retain(|(i, _)| *i != id);
        // Keep the 10 most recent finished jobs for the GUI's job list.
        let mut jobs = sh.jobs.lock();
        let finished = jobs.iter().filter(|j| j.state != "running" && j.state != "queued").count();
        if finished > 10 {
            let mut to_drop = finished - 10;
            jobs.retain(|j| {
                if to_drop > 0 && j.state != "running" && j.state != "queued" {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
}

/// 24/7 footage overlapping a wall-clock range (the live file as far as written).
fn footage_for(sh: &Shared, from_ms: i64, to_ms: i64) -> Result<Vec<Footage>> {
    let db = Db::open_default()?;
    let (live_path, written) = {
        let status = sh.status.lock();
        (status.recording.path.clone(), status.recording.written_until_ms)
    };
    Ok(db
        .list_recordings()?
        .into_iter()
        .filter(|r| r.start_ms < to_ms && (r.end_ms() > from_ms || r.is_live()))
        .filter(|r| !r.is_live() || crate::shared::same_path(&r.path, &live_path))
        .map(|r| Footage {
            duration_s: if r.is_live() { (written - r.start_ms).max(0) as f64 / 1000.0 } else { r.duration },
            path: PathBuf::from(&r.path),
            start_ms: r.start_ms,
        })
        .collect())
}

/// Translate absolute edits through the intervals that made it into the cut.
fn edits_for_join(edits: &[AudioEdit], starts: &[i64], spans: &[CutSpan]) -> Vec<AudioEdit> {
    let mut mapped = Vec::new();
    for edit in edits {
        let mut pieces: Vec<AudioEdit> = Vec::new();
        for span in spans {
            let wall_start = starts[span.source_index] as f64 / 1000.0;
            let from = wall_start + span.from_s;
            let to = wall_start + span.to_s;
            if let Some(mut piece) = rebase_edits(std::slice::from_ref(edit), from, to).pop() {
                piece.start += span.output_start_s;
                piece.end += span.output_start_s;
                if let Some(last) = pieces.last_mut() {
                    if (last.end - piece.start).abs() < 0.000_001 {
                        last.end = piece.end;
                        continue;
                    }
                }
                pieces.push(piece);
            }
        }
        mapped.extend(pieces);
    }
    mapped
}

/// Cut a wall-clock range of the 24/7 recording, and translate any edits into clip time.
#[allow(clippy::too_many_arguments)]
fn cut_recording(sh: &Shared, from_ms: i64, to_ms: i64, dest: &Path, edits: &[AudioEdit], progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<(f64, i64, Vec<AudioEdit>)> {
    anyhow::ensure!(to_ms > from_ms, "the end is before the start");
    anyhow::ensure!(to_ms - from_ms <= 6 * 3600 * 1000, "clips are limited to 6 hours");
    let footage = footage_for(sh, from_ms, to_ms)?;
    let parts = plan(&footage, from_ms, to_ms);
    anyhow::ensure!(!parts.is_empty(), "nothing was recorded in that time range");
    let starts: Vec<_> = parts.iter().map(|part| footage.iter().find(|f| f.path == part.path).map(|f| f.start_ms).unwrap_or(from_ms)).collect();
    let start_ms = starts[0] + (parts[0].from_s * 1000.0) as i64;
    let out = cut(&parts, dest, Some(start_ms), progress, cancel).inspect_err(|_| {
        let _ = std::fs::remove_file(dest);
    })?;
    Ok((out.duration, start_ms, edits_for_join(edits, &starts, &out.spans)))
}

#[allow(clippy::too_many_arguments)]
fn cut_range(
    sh: &Shared,
    from_ms: i64,
    to_ms: i64,
    title: &str,
    game_id: Option<i64>,
    category_ids: &[i64],
    mix: Option<&MixSettings>,
    edits: &[AudioEdit],
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    let mut db = Db::open_default()?;
    let (gid, game_name, exe) = match game_id {
        Some(g) => {
            let game = db.game(g)?.context("game not found")?;
            let exe = cv_core::games::exe_name_of(&game.exe_path);
            (g, game.name, exe)
        }
        None => attribute_game(&db, from_ms, to_ms, &cfg.clips.unknown_game_name)?,
    };
    let local = chrono::DateTime::from_timestamp_millis(from_ms).unwrap_or_default().with_timezone(&chrono::Local);
    let title = if title.trim().is_empty() { render_template(&cfg.clips.name_template, &game_name, &exe, local) } else { sanitize_component(title) };
    let dir = clip_dir(&cfg, &game_name);
    std::fs::create_dir_all(&dir)?;
    let tmp = temp_beside(&dir);
    let (dur, start_ms, clip_edits) = cut_recording(sh, from_ms, to_ms, &tmp, edits, progress, cancel)?;
    let _lock = sh.library_lock.lock();
    let dest = unique_path(&dir, &title, "mp4", None);
    move_file(&tmp, &dest, Duration::from_secs(10))?;
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
    let id = db.insert_clip(&NewClip {
        path: dest.to_string_lossy().to_string(),
        title: stem,
        game_id: Some(gid),
        created_at: to_ms,
        source_start_ms: Some(start_ms),
        size: dest.metadata().map(|m| m.len() as i64).unwrap_or(0),
        source: "recording".into(),
        favorite: false,
    })?;
    drop(_lock);
    if !category_ids.is_empty() {
        db.set_clip_categories(id, category_ids)?;
    }
    // Non-destructive: the clip keeps every track; volumes and edits are
    // remembered with it and can be changed later.
    if let Some(m) = mix {
        db.set_clip_mixer(id, Some(m))?;
    }
    if !clip_edits.is_empty() {
        db.set_clip_audio_edits(id, &clip_edits)?;
    }
    probe_and_thumb(&db, id, &dest, cfg.clips.generate_thumbnails);
    if let Ok(w) = cv_media::wave::generate(&dest, 0.0, None, cancel) {
        let _ = w.save(&cv_core::waveform::wave_path(&dest));
    }
    sh.toast(ToastLevel::Success, format!("Clip created: {}", dest.file_stem().unwrap_or_default().to_string_lossy()));
    sh.log_event("info", format!("Clipped {} from the 24/7 recording", cv_core::util::format_duration(dur)));
    Ok((dest, Some(id)))
}

fn trim(sh: &Shared, clip_id: i64, start_s: f64, end_s: f64, replace: bool, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    anyhow::ensure!(end_s > start_s + 0.1, "the trim range is empty");
    let mut db = Db::open_default()?;
    let clip = db.clip(clip_id)?.context("clip not found")?;
    let src = PathBuf::from(&clip.path);
    anyhow::ensure!(src.is_file(), "the clip file is missing");
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let tmp = temp_beside(&dir);
    let start_ms = clip.source_start_ms.map(|s| s + (start_s * 1000.0) as i64);
    cut(&[CutSource { path: src.clone(), from_s: start_s.max(0.0), to_s: Some(end_s) }], &tmp, start_ms, progress, cancel).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    let edits = rebase_edits(&clip.audio_edits, start_s, end_s);
    let _lock = sh.library_lock.lock();
    let current = db.clip(clip_id).and_then(|clip| clip.context("clip not found"));
    if let Err(error) = current.and_then(|current| {
        anyhow::ensure!(current.path == clip.path && src.is_file(), "The source clip changed during trimming");
        anyhow::ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
        Ok(())
    }) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if replace {
        replace_trimmed_file(&tmp, &src, cfg.clips.delete_to_recycle_bin).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        drop(_lock);
        let _ = std::fs::remove_file(cv_core::waveform::wave_path(&src));
        probe_and_thumb(&db, clip_id, &src, cfg.clips.generate_thumbnails);
        db.set_clip_audio_edits(clip_id, &edits)?;
        let _ = db.set_clip_source_start(clip_id, start_ms);
        let _ = db.set_clip_position(clip_id, 0.0);
        if let Ok(w) = cv_media::wave::generate(&src, 0.0, None, cancel) {
            let _ = w.save(&cv_core::waveform::wave_path(&src));
        }
        sh.toast(ToastLevel::Success, "Clip trimmed");
        Ok((src, Some(clip_id)))
    } else {
        let dest = unique_path(&dir, &format!("{} (trim)", clip.title), "mp4", None);
        std::fs::rename(&tmp, &dest)?;
        let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let id = db.insert_clip(&NewClip {
            path: dest.to_string_lossy().to_string(),
            title: stem,
            game_id: clip.game_id,
            created_at: now_ms(),
            source_start_ms: start_ms,
            size: dest.metadata().map(|m| m.len() as i64).unwrap_or(0),
            source: "trim".into(),
            favorite: clip.favorite,
        })?;
        drop(_lock);
        db.set_clip_categories(id, &clip.category_ids)?;
        db.set_clip_audio_edits(id, &edits)?;
        if let Some(m) = &clip.mixer {
            db.set_clip_mixer(id, Some(m))?;
        }
        probe_and_thumb(&db, id, &dest, cfg.clips.generate_thumbnails);
        if let Ok(w) = cv_media::wave::generate(&dest, 0.0, None, cancel) {
            let _ = w.save(&cv_core::waveform::wave_path(&dest));
        }
        sh.toast(ToastLevel::Success, "Trimmed copy saved");
        Ok((dest, Some(id)))
    }
}

/// Keep the existing path in place until the same-volume rename succeeds.
fn replace_trimmed_file(replacement: &Path, original: &Path, recycle_original: bool) -> Result<()> {
    let backup = recycle_original.then(|| original.with_file_name(format!(
        ".cv_{}_{}", cv_core::util::random_token(12), original.file_name().unwrap_or_default().to_string_lossy()
    )));
    if let Some(path) = &backup {
        std::fs::hard_link(original, path).or_else(|_| std::fs::copy(original, path).map(|_| ()))
            .inspect_err(|_| { let _ = std::fs::remove_file(path); })
            .context("Could not preserve the original clip")?;
    }
    if let Err(error) = std::fs::rename(replacement, original) {
        if let Some(path) = &backup {
            let _ = std::fs::remove_file(path);
        }
        return Err(error).context("Could not replace the clip");
    }
    if let Some(path) = backup {
        if let Err(error) = crate::win::shell::recycle(&[path.as_path()]) {
            tracing::warn!("Original clip retained at {}: {error:#}", path.display());
        }
    }
    Ok(())
}

/// Adjust the retry to the observed overshoot, with a margin for encoder variance.
fn share_retry_bitrate(kbps: u32, actual: u64, cap: u64) -> Option<u32> {
    let next = (kbps as f64 * cap as f64 / actual as f64 * 0.9) as u32;
    (next >= 150).then_some(next)
}

/// Gain curve for every audio track of `path` from a mix + edits (file time).
pub fn gains_for(path: &Path, mix: &MixSettings, edits: &[AudioEdit], edit_offset: f64, fade: f64, overrides: &std::collections::BTreeMap<String, String>) -> Result<Vec<cv_core::mixer::GainCurve>> {
    let info = cv_media::probe::probe(path)?;
    let sources = cv_media::probe::track_sources(&info);
    let mut gains = compose(&sources, mix, edits, edit_offset, fade, overrides);
    for (i, a) in info.audio.iter().enumerate() {
        if let cv_media::probe::TrackRole::Slot { slot } = a.role {
            let iv: Vec<(f64, f64, TrackSource)> = cv_media::probe::slot_intervals(&info, slot, info.duration)
                .into_iter()
                .map(|(a, b, app)| (a, b, TrackSource::App { key: app.key, name: app.name, category: app.category }))
                .collect();
            gains[i] = compose_slot(&iv, mix, edits, edit_offset, fade, overrides);
        }
    }
    Ok(gains)
}

#[allow(clippy::too_many_arguments)]
fn export(
    sh: &Shared,
    source: &ExportSource,
    range: Option<(f64, f64)>,
    mix: &MixSettings,
    edits: Option<&[AudioEdit]>,
    target_size_mb: Option<u32>,
    title: &str,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    let out_dir = cfg.exports_dir();
    std::fs::create_dir_all(&out_dir)?;
    // Where the picture and sound come from.
    let (src, edits, start_ms, name, temp): (PathBuf, Vec<AudioEdit>, Option<i64>, String, Option<PathBuf>) = match source {
        ExportSource::Clip { clip_id } => {
            let db = Db::open_default()?;
            let clip = db.clip(*clip_id)?.context("clip not found")?;
            let e = edits.map(<[AudioEdit]>::to_vec).unwrap_or(clip.audio_edits);
            (PathBuf::from(&clip.path), e, clip.source_start_ms, clip.title.clone(), None)
        }
        ExportSource::Recording { from_ms, to_ms } => {
            let tmp = temp_beside(&out_dir);
            let mut sub = |p: f32| progress(p * 0.2);
            let (_, start, e) = cut_recording(sh, *from_ms, *to_ms, &tmp, edits.unwrap_or_default(), &mut sub, cancel)?;
            let local = chrono::DateTime::from_timestamp_millis(*from_ms).unwrap_or_default().with_timezone(&chrono::Local);
            (tmp.clone(), e, Some(start), format!("24-7 {}", local.format("%Y-%m-%d %H-%M-%S")), Some(tmp))
        }
    };
    anyhow::ensure!(src.is_file(), "the source file is missing");
    let result = (|| -> Result<PathBuf> {
        let gains = gains_for(&src, mix, &edits, 0.0, cfg.player.fade_s(), &cfg.audio.app_categories)?;
        let info = cv_media::probe::probe(&src)?;
        let dur = match range {
            Some((a, b)) => (b - a).max(0.1),
            None => info.duration,
        };
        let audio_kbps = cfg.export.audio_bitrate_kbps.min(192);
        let mut video = match target_size_mb {
            Some(mb) => VideoOut::Reencode {
                kbps: bitrate_for_size(mb, dur, audio_kbps).with_context(|| format!("{} is too long to fit in {mb} MB; trim it first", cv_core::util::format_duration(dur)))?,
                max_height: cfg.export.max_height,
            },
            None => VideoOut::Copy,
        };
        let label = if title.trim().is_empty() { name.clone() } else { sanitize_component(title) };
        let suffix = match target_size_mb {
            Some(mb) => format!("{mb} MB"),
            None => "export".into(),
        };
        let out = unique_path(&out_dir, &format!("{label} ({suffix})"), "mp4", None);
        let tmp = temp_beside(&out_dir);
        let base = if temp.is_some() { 0.2 } else { 0.0 };
        let result = (|| -> Result<()> {
            let max_bytes = target_size_mb.map(|mb| u64::from(mb) * 1024 * 1024);
            for attempt in 0..3 {
                let job = ExportJob { source: src.clone(), range, gains: gains.clone(), video: video.clone(), audio_kbps, start_ms };
                let mut sub = |p: f32| progress(base + p * (1.0 - base));
                run_export(&job, &tmp, &mut sub, cancel)?;
                anyhow::ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
                let size = tmp.metadata()?.len();
                if max_bytes.is_none_or(|cap| size <= cap) {
                    std::fs::rename(&tmp, &out)?;
                    return Ok(());
                }
                if attempt == 2 {
                    break;
                }
                let (Some(cap), VideoOut::Reencode { kbps, max_height }) = (max_bytes, &video) else { break };
                let Some(smaller) = share_retry_bitrate(*kbps, size, cap) else { break };
                tracing::info!("export is {size} bytes for a {cap}-byte limit; retrying at {smaller} kbps");
                video = VideoOut::Reencode { kbps: smaller, max_height: *max_height };
                std::fs::remove_file(&tmp)?;
            }
            anyhow::bail!("The export exceeded the {} MB limit. Trim the selection or choose a larger limit.", target_size_mb.unwrap_or_default())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        Ok(out)
    })();
    if let Some(t) = temp {
        let _ = std::fs::remove_file(t);
    }
    let out = result?;
    if cfg.export.copy_to_clipboard {
        sh.ui.send(UiCmd::CopyFile(out.clone()));
    }
    if cfg.export.open_folder_after {
        crate::win::shell::open_in_explorer(&out_dir);
    }
    let msg = if cfg.export.copy_to_clipboard { "Exported — copied to clipboard, paste it anywhere" } else { "Export finished" };
    sh.toast(ToastLevel::Success, msg);
    Ok((out, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_audio_edits_move_with_footage_across_a_gap() {
        let spans = [
            CutSpan { source_index: 0, from_s: 2.0, to_s: 8.0, output_start_s: 0.0 },
            CutSpan { source_index: 1, from_s: 0.0, to_s: 8.0, output_start_s: 6.0 },
        ];
        let starts = [100_000, 120_000];
        let edits = [
            AudioEdit::new("mic", 115.0, 119.0, 0.0),
            AudioEdit::new("mic", 122.0, 124.0, 0.0),
            AudioEdit::new("game", 106.0, 121.0, 0.5),
        ];
        assert_eq!(edits_for_join(&edits, &starts, &spans), vec![
            AudioEdit::new("mic", 8.0, 10.0, 0.0),
            AudioEdit::new("game", 4.0, 7.0, 0.5),
        ]);
    }

    #[test]
    fn failed_replacement_preserves_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.mp4");
        let missing = dir.path().join("missing.mp4");
        std::fs::write(&original, b"original").unwrap();

        assert!(replace_trimmed_file(&missing, &original, true).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn trim_replaces_original_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.mp4");
        let replacement = dir.path().join("replacement.mp4");
        std::fs::write(&original, b"original").unwrap();
        std::fs::write(&replacement, b"trimmed").unwrap();

        replace_trimmed_file(&replacement, &original, false).unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"trimmed");
        assert!(!replacement.exists());
    }

    #[test]
    fn share_retry_uses_observed_overshoot() {
        assert_eq!(share_retry_bitrate(1000, 20_000, 10_000), Some(450));
        assert_eq!(share_retry_bitrate(150, 20_000, 10_000), None);
    }
}
