//! A composed preview and export worker. Both use the same renderer.
use super::{availability::Availability, seconds, ticks};
use crate::{native_player::NativePlayer, theme};
use anyhow::{bail, Context as _, Result};
use cv_core::editor::{Id, Project, Span, TICKS_PER_SECOND};
use cv_media::editor::{self, RenderControl};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    time::{Duration, Instant},
};

const PROXY_LENGTH: u64 = 12 * TICKS_PER_SECOND;
static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

struct Worker {
    control: Arc<RenderControl>,
    result: Receiver<Result<PathBuf>>,
}

impl Worker {
    fn spawn(ctx: &egui::Context, name: &str, run: impl FnOnce(&RenderControl) -> Result<PathBuf> + Send + 'static, clean_abandoned: bool) -> Result<Self> {
        let control = Arc::new(RenderControl::default());
        let worker_control = Arc::clone(&control);
        let (tx, result) = mpsc::channel();
        let wake = ctx.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let completed = tx.send(run(&worker_control));
            if clean_abandoned {
                if let Err(mpsc::SendError(Ok(path))) = completed { let _ = fs::remove_file(path); }
            }
            wake.request_repaint();
        })?;
        Ok(Self { control, result })
    }

    fn result(&self) -> Option<Result<PathBuf>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("Render worker stopped"))),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

struct Proxy {
    span: Span,
    path: PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) { let _ = fs::remove_file(&self.path); }
}

struct ProxyTask {
    span: Span,
    worker: Worker,
}

impl Drop for ProxyTask {
    fn drop(&mut self) {
        self.worker.control.cancel();
        if let Some(Ok(path)) = self.worker.result() { let _ = fs::remove_file(path); }
    }
}

type Thumbnail = (u64, u64, Result<egui::ColorImage>);

#[derive(Default)]
pub(super) struct Preview {
    player: Option<NativePlayer>,
    texture: Option<egui::TextureHandle>,
    serial: Option<u64>,
    displayed: Option<(u64, u64)>,
    proxy: Option<Proxy>,
    task: Option<ProxyTask>,
    thumbnail: Option<Receiver<Thumbnail>>,
    shown: Option<(u64, u64)>,
    error: Option<String>,
    offline_source: Option<Id>,
    playing: bool,
    pending_play: bool,
    wait_seek: Option<(u64, Instant)>,
    next_thumb: Option<Instant>,
}

impl Preview {
    pub(super) fn playing(&self) -> bool { self.playing }

    pub(super) fn pause(&mut self) {
        self.playing = false;
        self.pending_play = false;
        if let Some(player) = &self.player { player.set_paused(true); }
    }

    pub(super) fn invalidate(&mut self, ctx: &egui::Context) {
        let previous = self.texture.take().zip(self.displayed);
        self.reset();
        if let Some((texture, displayed)) = previous {
            self.texture = Some(texture);
            self.displayed = Some(displayed);
        }
        self.next_thumb = Some(Instant::now() + Duration::from_millis(35));
        ctx.request_repaint_after(Duration::from_millis(35));
    }

    pub(super) fn reset(&mut self) {
        self.pause();
        if let Some(player) = &self.player { player.clear(); }
        self.proxy = None;
        self.task = None;
        self.texture = None;
        self.displayed = None;
        self.serial = None;
        self.shown = None;
        self.error = None;
        self.offline_source = None;
        self.wait_seek = None;
        self.next_thumb = None;
    }

    pub(super) fn start(&mut self, ctx: &egui::Context, project: &Project, _revision: u64, at: u64) {
        self.playing = true;
        self.error = None;
        if self.proxy.as_ref().is_some_and(|proxy| in_span(proxy.span, at, project.duration())) {
            if let Some(player) = &self.player {
                let proxy = self.proxy.as_ref().unwrap();
                player.seek(relative_time(proxy.span, at), true);
                player.set_paused(false);
                self.wait_seek = Some((at, Instant::now()));
            }
            return;
        }
        self.pending_play = true;
        self.ensure_proxy(ctx, project, at);
    }

    pub(super) fn seek(&mut self, ctx: &egui::Context, project: &Project, _revision: u64, at: u64) {
        self.pause();
        self.error = None;
        if let Some((proxy, player)) = self.proxy.as_ref().zip(self.player.as_ref()) {
            if in_span(proxy.span, at, project.duration()) {
                player.seek(relative_time(proxy.span, at), true);
                self.wait_seek = Some((at, Instant::now()));
                return;
            }
        }
        if let Some(player) = &self.player { player.clear(); }
        self.proxy = None;
        self.serial = None;
        self.task = None;
        self.next_thumb = Some(Instant::now());
        ctx.request_repaint();
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context, project: &Project, revision: u64, at: &mut u64, availability: &mut Availability) {
        if self.offline_source.is_some_and(|id| !availability.missing(id)) { self.invalidate(ctx); }
        if project.duration() == 0 { return; }
        if let Some(result) = self.task.as_ref().and_then(|task| task.worker.result()) {
            let task = self.task.take().unwrap();
            match result {
                Ok(path) => {
                    self.proxy = Some(Proxy { span: task.span, path });
                    let proxy = self.proxy.as_ref().unwrap();
                    let player = self.player.get_or_insert_with(|| NativePlayer::new(ctx.clone()));
                    let should_play = self.playing && self.pending_play;
                    player.load(proxy.path.clone(), relative_time(proxy.span, *at), false, !should_play);
                    self.pending_play = false;
                    self.serial = None;
                    self.wait_seek = Some((*at, Instant::now()));
                }
                Err(error) => {
                    self.playing = false;
                    self.pending_play = false;
                    self.report_error(project, &error, availability);
                }
            }
        }

        if let Some((_, player)) = self.proxy.as_ref().zip(self.player.as_ref()) {
            let state = player.state();
            if let Some(error) = state.error {
                self.playing = false;
                self.error = Some(format!("Playback: {error}"));
                return;
            }
            if let Some(frame) = player.frame().filter(|frame| self.serial != Some(frame.serial)) {
                let image = egui::ColorImage::from_rgba_premultiplied([frame.width, frame.height], &frame.rgba);
                self.serial = Some(frame.serial);
                self.displayed = self.proxy.as_ref().map(|p| (revision, p.span.start.saturating_add(ticks(frame.at_seconds))));
                self.set_image(ctx, image);
            }
            if self.playing && state.loaded {
                let span = self.proxy.as_ref().unwrap().span;
                let current = span.start.saturating_add(ticks(state.position)).min(span.end);
                let waiting = self.wait_seek.is_some_and(|(target, since)| {
                    current.abs_diff(target) > TICKS_PER_SECOND / 5 && since.elapsed() < Duration::from_millis(380)
                });
                if !waiting {
                    self.wait_seek = None;
                    if state.ended {
                        *at = span.end;
                        if *at < project.duration() {
                            self.pending_play = true;
                            self.ensure_proxy(ctx, project, *at);
                        } else { self.pause(); }
                    } else {
                        *at = current;
                    }
                }
            }
        }

        if let Some(result) = self.thumbnail.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(Some(result)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }) {
            self.thumbnail = None;
            if let Some((rendered_revision, rendered_at, result)) = result {
                if self.proxy.is_none() && revision == rendered_revision && *at == rendered_at {
                    self.shown = Some((revision, *at));
                    match result {
                        Ok(image) => {
                            self.displayed = Some((revision, rendered_at));
                            self.set_image(ctx, image);
                        }
                        Err(error) if self.error.is_none() => self.report_error(project, &error, availability),
                        Err(_) => {}
                    }
                }
            }
        }

        if self.proxy.is_none() && self.shown != Some((revision, *at)) && self.thumbnail.is_none() {
            let delay = self.next_thumb.and_then(|t| t.checked_duration_since(Instant::now()));
            if let Some(delay) = delay { ctx.request_repaint_after(delay); }
            else { self.request_thumbnail(ctx, project, revision, *at); }
        }
        if self.playing || self.task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn report_error(&mut self, project: &Project, error: &anyhow::Error, availability: &mut Availability) {
        let details = format!("{error:#}");
        self.offline_source = None;
        for asset in &project.assets {
            let name = super::filename(&asset.path);
            if details.contains(&name) && !asset.path.is_file() {
                self.offline_source = Some(asset.id);
                availability.mark_offline(asset.id);
                self.error = Some(format!("Media offline · {name}\nChoose Relink beside the project name."));
                return;
            }
        }
        self.error = Some(format!("Preview failed: {details}"));
    }

    fn ensure_proxy(&mut self, ctx: &egui::Context, project: &Project, at: u64) {
        let total = project.duration();
        if total == 0 { return; }
        let span = playback_span(total, at, project.canvas.fps);
        if self.task.as_ref().is_some_and(|task| task.span.start == span.start && task.span.end == span.end) { return; }
        if let Some(player) = &self.player { player.clear(); }
        self.proxy = None;
        self.task = None;
        let project = project.clone();
        let task = Worker::spawn(ctx, "clipvault-editor-preview", move |control| {
            let folder = cv_core::paths::data_dir().join("editor-preview");
            fs::create_dir_all(&folder)?;
            let output = unique_output(&folder, "mp4");
            let segment = if span.start == 0 && span.end == project.duration() { project } else { project.slice(span)? };
            let result = editor::render(&segment, &output, control);
            if result.is_err() { let _ = fs::remove_file(&output); }
            result.map(|_| output)
        }, true);
        match task {
            Ok(worker) => self.task = Some(ProxyTask { span, worker }),
            Err(error) => {
                self.playing = false;
                self.pending_play = false;
                self.error = Some(format!("Could not start preview: {error:#}"));
            }
        }
    }

    fn request_thumbnail(&mut self, ctx: &egui::Context, project: &Project, revision: u64, at: u64) {
        self.next_thumb = None;
        let (tx, rx) = mpsc::channel();
        let project = project.clone();
        let wake = ctx.clone();
        let task = std::thread::Builder::new().name("clipvault-editor-frame".into()).spawn(move || {
            let scale = 640.0 / f64::from(project.canvas.width.max(project.canvas.height).max(1));
            let width = (f64::from(project.canvas.width) * scale).round().max(1.0) as u32;
            let height = (f64::from(project.canvas.height) * scale).round().max(1.0) as u32;
            let frame_at = at.min(project.duration().saturating_sub(1));
            let result = editor::thumbnail(&project, frame_at, width, height).map(|im| {
                egui::ColorImage::from_rgba_unmultiplied([im.width() as usize, im.height() as usize], im.as_raw())
            });
            let _ = tx.send((revision, at, result));
            wake.request_repaint();
        });
        match task {
            Ok(_) => self.thumbnail = Some(rx),
            Err(error) => {
                self.shown = Some((revision, at));
                self.error = Some(format!("Could not show frame: {error}"));
            }
        }
    }

    fn set_image(&mut self, ctx: &egui::Context, image: egui::ColorImage) {
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture = Some(ctx.load_texture("clipvault-composed-preview", image, egui::TextureOptions::LINEAR));
        }
    }

    pub(super) fn picture(&self, ui: &Ui, rect: Rect, project: &Project, accent: Color32, revision: u64, at: u64) {
        let painter = ui.painter_at(rect);
        painter.rect(rect, 5.0, theme::SIDEBAR, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
        let source = Vec2::new(project.canvas.width.max(1) as f32, project.canvas.height.max(1) as f32);
        let scale = (rect.width() / source.x).min(rect.height() / source.y);
        let picture = Rect::from_center_size(rect.center(), source * scale);
        painter.rect_filled(picture, 2.0, Color32::BLACK);
        if let Some(texture) = self.texture.as_ref().filter(|_| project.duration() > 0 && self.error.is_none()) {
            let dimensions = texture.size_vec2();
            let fit = (picture.width() / dimensions.x).min(picture.height() / dimensions.y);
            let bounds = Rect::from_center_size(picture.center(), dimensions * fit);
            painter.image(texture.id(), bounds, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        let message = if let Some(error) = &self.error {
            Some((error.clone(), theme::DANGER))
        } else if let Some(task) = &self.task {
            Some((format!("Preparing playback… {:.0}%", task.worker.control.progress() * 100.0), accent))
        } else if project.duration() == 0 {
            Some(("Add media or a title".into(), theme::TEXT_DIM))
        } else if let Some((frame_revision, shown_at)) = self.displayed {
            let target = at.min(project.duration().saturating_sub(TICKS_PER_SECOND / u64::from(project.canvas.fps)));
            let seeking = if self.proxy.is_some() {
                shown_at.abs_diff(target) > TICKS_PER_SECOND / (2 * u64::from(project.canvas.fps))
            } else { self.shown != Some((revision, at)) };
            if frame_revision != revision {
                Some((format!("Updating edit at {}…", super::clock(at)), theme::TEXT))
            } else if !self.playing && seeking {
                Some((format!("Showing {} · Seeking {}…", super::clock(shown_at), super::clock(at)), theme::TEXT))
            } else { None }
        } else {
            Some((format!("Loading frame at {}…", super::clock(at)), theme::TEXT))
        };
        if let Some((message, color)) = message {
            let message = if rect.width() >= 110.0 { message }
                else if rect.width() < 60.0 {
                    if self.error.is_some() { "!".into() } else { "…".into() }
                }
                else if self.error.is_some() { "Error".into() }
                else if project.duration() == 0 { "Empty".into() }
                else { "Loading…".into() };
            let mut text = egui::text::LayoutJob::simple(message, FontId::proportional(13.0), color, (rect.width() - 16.0).max(1.0));
            text.wrap.max_rows = 2;
            text.wrap.break_anywhere = true;
            let galley = painter.layout_job(text);
            let pos = Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size()).min;
            let backdrop = Rect::from_min_size(pos, galley.size()).expand(4.0);
            painter.rect_filled(backdrop, 4.0, Color32::from_black_alpha(205));
            painter.galley(pos, galley, color);
        }
    }
}

fn playback_span(total: u64, at: u64, fps: u32) -> Span {
    let frame = TICKS_PER_SECOND.div_ceil(u64::from(fps.max(1)));
    let mut start = at.min(total.saturating_sub(1)) / PROXY_LENGTH * PROXY_LENGTH;
    if total.saturating_sub(start) < frame { start = start.saturating_sub(PROXY_LENGTH); }
    let boundary = start.saturating_add(PROXY_LENGTH);
    let end = if total.saturating_sub(boundary) < frame { total } else { boundary };
    Span { start, end }
}

#[test]
fn preview_keeps_a_subframe_audio_tail_with_the_last_picture() {
    let total = PROXY_LENGTH + TICKS_PER_SECOND / 100;
    for at in [0, PROXY_LENGTH - 1, PROXY_LENGTH, total] {
        assert_eq!(playback_span(total, at, 30), Span { start: 0, end: total });
    }
    assert_eq!(playback_span(3 * PROXY_LENGTH, PROXY_LENGTH, 30), Span { start: PROXY_LENGTH, end: 2 * PROXY_LENGTH });
}

fn in_span(span: Span, at: u64, duration: u64) -> bool {
    at >= span.start && (at < span.end || at == duration && span.end == duration)
}

fn relative_time(span: Span, at: u64) -> f64 {
    seconds(at.clamp(span.start, span.end.saturating_sub(1)) - span.start)
}

pub(super) enum ExportStatus {
    Running(f32, String),
    Saved(PathBuf),
    Failed(String),
}

#[derive(Default)]
pub(super) struct Export {
    task: Option<Worker>,
    completed: Option<Result<PathBuf>>,
    filename: String,
}

impl Export {
    pub(super) fn idle(&self) -> bool { self.task.is_none() }

    pub(super) fn clear_result(&mut self) { self.completed = None; }

    pub(super) fn start(&mut self, ctx: &egui::Context, project: Project, destination: PathBuf) -> Result<()> {
        self.completed = None;
        ensure_new(&destination)?;
        self.filename = super::filename(&destination);
        self.task = Some(Worker::spawn(ctx, "clipvault-editor-export", move |control| {
            editor::render(&project, &destination, control)?;
            Ok(destination)
        }, false)?);
        Ok(())
    }

    pub(super) fn poll(&mut self) {
        if let Some(result) = self.task.as_ref().and_then(Worker::result) {
            self.task = None;
            self.completed = Some(result);
        }
    }

    pub(super) fn status(&self) -> Option<ExportStatus> {
        if let Some(task) = &self.task {
            return Some(ExportStatus::Running(task.control.progress(), self.filename.clone()));
        }
        self.completed.as_ref().map(|result| match result {
            Ok(path) => ExportStatus::Saved(path.clone()),
            Err(error) => ExportStatus::Failed(format!("Export: {error:#}")),
        })
    }

    pub(super) fn cancel(&self) {
        if let Some(task) = &self.task { task.control.cancel(); }
    }
}

fn unique_output(folder: &Path, ext: &str) -> PathBuf {
    let seq = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    folder.join(format!(".clipvault-editor-{}-{seq}.{ext}", std::process::id()))
}

fn ensure_new(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(_) => bail!("This file already exists. Choose a new filename."),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn create_new(path: &Path, produce: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    ensure_new(path)?;
    let folder = path.parent().context("Choose a destination folder")?;
    let staging = unique_output(folder, path.extension().and_then(|value| value.to_str()).unwrap_or("tmp"));
    ensure_new(&staging)?;
    let result = produce(&staging).and_then(|_| publish_new(&staging, path));
    let _ = fs::remove_file(staging);
    result
}

fn publish_new(staging: &Path, destination: &Path) -> Result<()> {
    if fs::hard_link(staging, destination).is_ok() { return Ok(()); }
    // Some removable drives lack hard links. The exclusive create still protects existing files.
    let mut source = File::open(staging)?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(destination)?;
    let copy = io::copy(&mut source, &mut output).and_then(|_| output.sync_all());
    drop(output);
    if copy.is_err() { let _ = fs::remove_file(destination); }
    copy.map(|_| ()).map_err(Into::into)
}
