//! Background jobs: clipping from the 24/7 recording, trimming, exporting.
//! Run one at a time on their own thread with progress reported to the GUI.

use crate::clips::{attribute_game, clip_dir, probe_and_thumb};
use crate::shared::Shared;
use crate::win::ui::{ToastLevel, UiCmd};
use anyhow::{bail, Context, Result};
use cv_core::db::Db;
use cv_core::ffmpeg::{bitrate_for_size, concat_list, cut_args, export_args, EncodeOptions, ExportSpec};
use cv_core::ipc::{Event, JobInfo};
use cv_core::model::NewClip;
use cv_core::naming::{render_template, sanitize_component, unique_path};
use cv_core::timeline::{plan_cut, plan_duration_s, CutPart, Segment};
use cv_core::util::now_ms;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum JobKind {
    CutRange { from_ms: i64, to_ms: i64, title: String, game_id: Option<i64>, category_ids: Vec<i64>, precise: Option<bool> },
    Trim { clip_id: i64, start_s: f64, end_s: f64, replace: bool, precise: Option<bool> },
    Export { clip_id: i64, range: Option<(f64, f64)>, gains: Vec<f32>, target_size_mb: Option<u32> },
}

pub struct JobRequest {
    pub id: u64,
    pub kind: JobKind,
    pub cancel: Arc<AtomicBool>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CANCELS: once_cell::sync::Lazy<parking_lot::Mutex<Vec<(u64, Arc<AtomicBool>)>>> =
    once_cell::sync::Lazy::new(Default::default);

/// Queue a job; returns its id.
pub fn submit(sh: &Shared, kind: JobKind) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let cancel = Arc::new(AtomicBool::new(false));
    CANCELS.lock().push((id, cancel.clone()));
    let (label, title) = match &kind {
        JobKind::CutRange { title, .. } => ("clip", if title.is_empty() { "New clip".to_string() } else { title.clone() }),
        JobKind::Trim { .. } => ("trim", "Trimming clip".to_string()),
        JobKind::Export { .. } => ("export", "Exporting".to_string()),
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
            JobKind::CutRange { from_ms, to_ms, title, game_id, category_ids, precise } => {
                cut_range(&sh, *from_ms, *to_ms, title, *game_id, category_ids, *precise, &mut progress, &req.cancel)
            }
            JobKind::Trim { clip_id, start_s, end_s, replace, precise } => {
                trim(&sh, *clip_id, *start_s, *end_s, *replace, *precise, &mut progress, &req.cancel)
            }
            JobKind::Export { clip_id, range, gains, target_size_mb } => {
                export(&sh, *clip_id, *range, gains, *target_size_mb, &mut progress, &req.cancel)
            }
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

fn encode_opts(cfg: &cv_core::config::Config) -> EncodeOptions {
    EncodeOptions {
        video_encoder: cfg.export.video_encoder.clone(),
        quality: cfg.export.quality,
        audio_bitrate_kbps: cfg.export.audio_bitrate_kbps,
    }
}

#[allow(clippy::too_many_arguments)]
fn cut_range(
    sh: &Shared,
    from_ms: i64,
    to_ms: i64,
    title: &str,
    game_id: Option<i64>,
    category_ids: &[i64],
    precise: Option<bool>,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    anyhow::ensure!(to_ms > from_ms, "the end is before the start");
    anyhow::ensure!(to_ms - from_ms <= 6 * 3600 * 1000, "clips are limited to 6 hours");
    let tools = sh.tools().context("ffmpeg is not available")?;
    let db = Db::open_default()?;
    let now = now_ms();
    let recs = db.list_recordings()?;
    let segments: Vec<Segment> = recs
        .iter()
        .map(|r| Segment {
            path: PathBuf::from(&r.path),
            start_ms: r.start_ms,
            duration_ms: if r.is_live() { (now - r.start_ms - 3000).max(0) } else { (r.duration * 1000.0) as i64 },
            live: r.is_live(),
        })
        .collect();
    let parts = plan_cut(&segments, from_ms, to_ms);
    anyhow::ensure!(!parts.is_empty(), "nothing was recorded in that time range");
    // All parts must share one audio layout for a lossless join.
    let mut tracks: Option<i64> = None;
    for p in &parts {
        let r = recs.iter().find(|r| Path::new(&r.path) == p.path).unwrap();
        let t = if r.audio_tracks > 0 { r.audio_tracks } else { tools.probe(&p.path).map(|i| i.audio.len() as i64).unwrap_or(0) };
        match tracks {
            None => tracks = Some(t),
            Some(prev) if prev != t => bail!("that range spans recordings with different audio track layouts; clip each part separately"),
            _ => {}
        }
    }
    let total_s = plan_duration_s(&parts, &segments);

    let (gid, game_name, exe) = match game_id {
        Some(g) => {
            let game = db.game(g)?.context("game not found")?;
            let exe = cv_core::games::exe_name_of(&game.exe_path);
            (g, game.name, exe)
        }
        None => attribute_game(&db, from_ms, to_ms, &cfg.clips.unknown_game_name)?,
    };
    let local = chrono::DateTime::from_timestamp_millis(from_ms).unwrap_or_default().with_timezone(&chrono::Local);
    let title = if title.trim().is_empty() {
        render_template(&cfg.clips.name_template, &game_name, &exe, local)
    } else {
        sanitize_component(title)
    };
    let dir = clip_dir(&cfg, &game_name);
    std::fs::create_dir_all(&dir)?;
    let dest = unique_path(&dir, &title, "mp4", None);
    let tmp = cv_core::paths::temp_dir().join(format!("cut_{}.mp4", now_ms()));
    let list = cv_core::paths::temp_dir().join(format!("cut_{}.ffconcat", now_ms()));
    std::fs::create_dir_all(cv_core::paths::temp_dir())?;
    std::fs::write(&list, concat_list(&parts))?;
    let precise = precise.unwrap_or(cfg.export.precise_cuts);
    let args = cut_args(&parts, &tmp, &list, precise, tracks.unwrap_or(0) as usize, &encode_opts(&cfg))?;
    let r = tools.run_progress(&args, total_s, progress, cancel);
    let _ = std::fs::remove_file(&list);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    {
        let _lock = sh.library_lock.lock();
        crate::clips::move_file(&tmp, &dest, Duration::from_secs(10))?;
        let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(title);
        let id = db.insert_clip(&NewClip {
            path: dest.to_string_lossy().to_string(),
            title: stem,
            game_id: Some(gid),
            created_at: to_ms,
            source_start_ms: Some(from_ms),
            size: dest.metadata().map(|m| m.len() as i64).unwrap_or(0),
            source: "recording".into(),
            favorite: false,
        })?;
        drop(_lock);
        let mut db2 = Db::open_default()?;
        if !category_ids.is_empty() {
            db2.set_clip_categories(id, category_ids)?;
        }
        probe_and_thumb(&db, &tools, id, &dest, cfg.clips.generate_thumbnails);
        sh.toast(ToastLevel::Success, format!("Clip created: {}", dest.file_stem().unwrap_or_default().to_string_lossy()));
        sh.log_event("info", format!("Clipped {} from the 24/7 recording", cv_core::util::format_duration(total_s)));
        Ok((dest, Some(id)))
    }
}

#[allow(clippy::too_many_arguments)]
fn trim(
    sh: &Shared,
    clip_id: i64,
    start_s: f64,
    end_s: f64,
    replace: bool,
    precise: Option<bool>,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    anyhow::ensure!(end_s > start_s + 0.1, "the trim range is empty");
    let tools = sh.tools().context("ffmpeg is not available")?;
    let mut db = Db::open_default()?;
    let clip = db.clip(clip_id)?.context("clip not found")?;
    let src = PathBuf::from(&clip.path);
    anyhow::ensure!(src.is_file(), "the clip file is missing");
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let tmp = dir.join(format!(".cv_trim_{}.mp4", now_ms()));
    let parts = [CutPart { path: src.clone(), in_s: start_s.max(0.0), out_s: Some(end_s) }];
    let precise = precise.unwrap_or(cfg.export.precise_cuts);
    let args = cut_args(&parts, &tmp, Path::new("unused"), precise, clip.audio_tracks.max(0) as usize, &encode_opts(&cfg))?;
    if let Err(e) = tools.run_progress(&args, end_s - start_s, progress, cancel) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let _lock = sh.library_lock.lock();
    if replace {
        if cfg.clips.delete_to_recycle_bin {
            crate::win::shell::recycle(&[src.as_path()])?;
        } else {
            std::fs::remove_file(&src)?;
        }
        std::fs::rename(&tmp, &src)?;
        drop(_lock);
        probe_and_thumb(&db, &tools, clip_id, &src, cfg.clips.generate_thumbnails);
        let new_start = clip.source_start_ms.map(|s| s + (start_s * 1000.0) as i64);
        let _ = db.set_clip_source_start(clip_id, new_start);
        let _ = db.set_clip_position(clip_id, 0.0);
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
            source_start_ms: clip.source_start_ms.map(|s| s + (start_s * 1000.0) as i64),
            size: dest.metadata().map(|m| m.len() as i64).unwrap_or(0),
            source: "trim".into(),
            favorite: clip.favorite,
        })?;
        drop(_lock);
        db.set_clip_categories(id, &clip.category_ids)?;
        probe_and_thumb(&db, &tools, id, &dest, cfg.clips.generate_thumbnails);
        sh.toast(ToastLevel::Success, "Trimmed copy saved");
        Ok((dest, Some(id)))
    }
}

fn export(
    sh: &Shared,
    clip_id: i64,
    range: Option<(f64, f64)>,
    gains: &[f32],
    target_size_mb: Option<u32>,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<i64>)> {
    let cfg = sh.cfg();
    let tools = sh.tools().context("ffmpeg is not available")?;
    let db = Db::open_default()?;
    let clip = db.clip(clip_id)?.context("clip not found")?;
    let src = PathBuf::from(&clip.path);
    anyhow::ensure!(src.is_file(), "the clip file is missing");
    let info = tools.probe(&src)?;
    let n = info.audio.len();
    // Mix only if the user changed something or there are several tracks.
    let gains_full: Vec<(usize, f32)> = (0..n).map(|i| (i, gains.get(i).copied().unwrap_or(if i == 0 { 1.0 } else { 0.0 }))).collect();
    let untouched = n <= 1 && gains_full.first().map(|(_, g)| (*g - 1.0).abs() < 0.001).unwrap_or(true);
    let dur = match range {
        Some((a, b)) => (b - a).max(0.1),
        None => info.duration,
    };
    let video_kbps = match target_size_mb {
        Some(mb) => Some(
            bitrate_for_size(mb, dur, cfg.export.audio_bitrate_kbps)
                .with_context(|| format!("{} is too long to fit in {mb} MB; trim it first", cv_core::util::format_duration(dur)))?,
        ),
        None => None,
    };
    let spec = ExportSpec {
        gains: if untouched { Vec::new() } else { gains_full },
        range,
        target_size_mb,
        max_height: cfg.export.max_height,
        reencode_video: range.is_some() && cfg.export.precise_cuts,
    };
    let out_dir = cfg.exports_dir();
    std::fs::create_dir_all(&out_dir)?;
    let suffix = if target_size_mb.is_some() { "share" } else { "export" };
    let out = unique_path(&out_dir, &format!("{} ({suffix})", clip.title), "mp4", None);
    let tmp = out.with_extension("tmp.mp4");
    let enc = encode_opts(&cfg);
    let mut args = export_args(&src, &tmp, &spec, &enc, &info, video_kbps);
    let mut attempt = 0;
    loop {
        if let Err(e) = tools.run_progress(&args, dur, progress, cancel) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let size = tmp.metadata().map(|m| m.len()).unwrap_or(0);
        match (target_size_mb, video_kbps) {
            (Some(mb), Some(k)) if size > mb as u64 * 1024 * 1024 && attempt == 0 => {
                attempt += 1;
                let smaller = (k as f64 * 0.82) as u32;
                tracing::info!("export {} MB over target; retrying at {smaller} kbps", size / 1024 / 1024);
                args = export_args(&src, &tmp, &spec, &enc, &info, Some(smaller));
            }
            _ => break,
        }
    }
    std::fs::rename(&tmp, &out)?;
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
