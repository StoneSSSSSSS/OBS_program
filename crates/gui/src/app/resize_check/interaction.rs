//! Event-driven native editor checks, enabled only by the disposable profile sweep.
use super::{App, Page};
use eframe::egui::{self, Context, Event, Id, Pos2, Rect, Vec2, ViewportCommand};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Step {
    Open,
    PrimeArchive,
    ArchiveLatest,
    BackFrame,
    NextFrame,
    DragPreview,
    DragStart,
    DragEnd,
    Capture,
    CaptureTrim,
    LargerPreview,
    CaptureLargerPreview,
    RestorePreview,
    ArmLive,
    GoLive,
    LiveLatest,
    Done,
}

pub(super) struct Interaction {
    profile: PathBuf,
    step: Step,
    since: Instant,
    pending: VecDeque<Vec<Event>>,
    last_frame: f64,
    previous_frame: f64,
    preview_y: f32,
    pictures: (Rect, Rect),
    marks: (i64, i64),
    edge_lag: f64,
    screenshot: Arc<AtomicBool>,
}

impl Interaction {
    pub(super) fn new(profile: PathBuf) -> Self {
        Self {
            profile,
            step: Step::Open,
            since: Instant::now(),
            pending: VecDeque::new(),
            last_frame: 0.0,
            previous_frame: 0.0,
            preview_y: 0.0,
            pictures: (Rect::NOTHING, Rect::NOTHING),
            marks: (0, 0),
            edge_lag: 0.0,
            screenshot: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn inject(&mut self, input: &mut egui::RawInput) {
        if let Some(events) = self.pending.pop_front() {
            input.events.extend(events);
        }
    }

    fn set_step(&mut self, step: Step) {
        self.step = step;
        self.since = Instant::now();
    }

    fn target_rect(ctx: &Context, name: &'static str) -> Option<Rect> {
        let pass = ctx.cumulative_pass_nr();
        ctx.data(|data| {
            if data.get_temp::<u64>(Id::unique(("clipvault-interaction-pass", name))) != Some(pass) {
                return None;
            }
            data.get_temp::<Rect>(Id::unique(("clipvault-interaction-target", name)))
        })
        .filter(|rect| rect.width() >= 5.0 && rect.height() >= 5.0)
    }

    fn target(ctx: &Context, name: &'static str) -> Option<Pos2> {
        Self::target_rect(ctx, name).map(|rect| rect.center())
    }

    fn take_screenshot(&self, ctx: &Context, path: PathBuf) {
        self.screenshot.store(false, Ordering::Release);
        let done = Arc::clone(&self.screenshot);
        ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(
            move |image| {
                let rgba = image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect();
                if let Some(image) = image::RgbaImage::from_raw(image.size[0] as u32, image.size[1] as u32, rgba) {
                    if image.save(&path).is_ok() {
                        done.store(true, Ordering::Release);
                    }
                }
            },
        )));
    }

    fn gesture(&mut self, from: Pos2, to: Pos2) {
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.pending.push_back(vec![Event::PointerMoved(from), button(from, true)]);
        if from != to {
            self.pending.push_back(vec![Event::PointerMoved(to)]);
        }
        self.pending.push_back(vec![button(to, false)]);
    }

    fn click(&mut self, ctx: &Context, target: &'static str, next: Step) {
        if let Some(pos) = Self::target(ctx, target) {
            self.gesture(pos, pos);
            self.set_step(next);
        }
    }

    pub(super) fn advance(&mut self, app: &mut App, ctx: &Context, dir: &Path) -> Result<bool, String> {
        if !self.pending.is_empty() {
            return Ok(false);
        }
        if self.since.elapsed() > Duration::from_secs(10) && !matches!(self.step, Step::Open) {
            let st = app.c.player.state();
            return Err(format!(
                "{:?} timed out (loaded {}, paused {}, time {:.3}, last frame {}, picture {:?}, baseline {:?}, restore visible {})",
                self.step, st.loaded, st.paused, st.time_pos, st.last_frame,
                Self::target_rect(ctx, "preview_picture"), self.pictures.0,
                Self::target_rect(ctx, "preview_restore").is_some(),
            ));
        }
        let st = app.c.player.state();
        match self.step {
            Step::Open => {
                ctx.data_mut(|data| data.insert_temp(Id::unique("clipvault-interaction-check"), true));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(1182.0, 771.0)));
                app.navigate(Page::Recordings);
                ctx.memory_mut(|memory| {
                    if let Some(id) = memory.focused() {
                        memory.surrender_focus(id);
                    }
                });
                self.pending.push_back(vec![Event::PointerGone]);
                // Draw the actual audio lanes so enlarging the preview reflows a
                // populated editor and still leaves the transport available.
                app.c.player.show_mixer = true;
                self.set_step(Step::PrimeArchive);
            }
            Step::PrimeArchive => {
                if st.loaded && (ctx.viewport_rect().size() - Vec2::new(1182.0, 771.0)).abs().max_elem() < 3.0 {
                    // The prior shortcut phase may already be displaying the last
                    // frame. Starting elsewhere makes Jump to latest an observable
                    // presentation transition before clicking Previous frame.
                    app.c.player.set_paused(true);
                    app.c.player.seek(5.0, true);
                    self.set_step(Step::ArchiveLatest);
                }
            }
            Step::ArchiveLatest => {
                if st.loaded && st.paused && !st.last_frame && (st.time_pos - 5.0).abs() < 0.25 {
                    self.click(ctx, "jump_latest", Step::BackFrame);
                }
            }
            Step::BackFrame => {
                if st.loaded && st.paused && st.last_frame {
                    self.last_frame = st.time_pos;
                    self.click(ctx, "previous_frame", Step::NextFrame);
                }
            }
            Step::NextFrame => {
                if st.paused && !st.last_frame && st.time_pos < self.last_frame {
                    self.previous_frame = st.time_pos;
                    self.click(ctx, "next_frame", Step::DragPreview);
                }
            }
            Step::DragPreview => {
                if st.paused && st.last_frame && st.time_pos > self.previous_frame
                    && app.recordings.qa_mark_trim()
                {
                    if let Some(pos) = Self::target(ctx, "preview_drag") {
                        self.preview_y = pos.y;
                        self.marks = app.recordings.qa_trim_marks().ok_or("fixture trim marks")?;
                        self.gesture(pos, pos + Vec2::new(0.0, 45.0));
                        self.set_step(Step::DragStart);
                    }
                }
            }
            Step::DragStart => {
                if Self::target(ctx, "preview_drag").is_some_and(|p| p.y > self.preview_y + 20.0) {
                    if let Some(start) = Self::target(ctx, "trim_start") {
                        self.gesture(start, start - Vec2::new(35.0, 0.0));
                        self.set_step(Step::DragEnd);
                    }
                }
            }
            Step::DragEnd => {
                let Some(marks) = app.recordings.qa_trim_marks() else { return Ok(false) };
                if marks.0 < self.marks.0 && marks.1 == self.marks.1 {
                    if let Some(end) = Self::target(ctx, "trim_end") {
                        self.marks = marks;
                        self.gesture(end, end + Vec2::new(35.0, 0.0));
                        self.set_step(Step::Capture);
                    }
                }
            }
            Step::Capture => {
                let Some(marks) = app.recordings.qa_trim_marks() else { return Ok(false) };
                if marks.0 == self.marks.0 && marks.1 > self.marks.1 {
                    let Some(picture) = Self::target_rect(ctx, "preview_picture") else { return Ok(false) };
                    self.pictures.0 = picture;
                    self.marks = marks;
                    self.pending.push_back(vec![Event::PointerGone]);
                    self.set_step(Step::CaptureTrim);
                }
            }
            Step::CaptureTrim => {
                if app.c.toasts.is_empty() && self.since.elapsed() >= Duration::from_millis(250) {
                    self.take_screenshot(ctx, dir.join("recordings-preview-drag-and-trim.png"));
                    self.set_step(Step::LargerPreview);
                }
            }
            Step::LargerPreview => {
                if self.screenshot.load(Ordering::Acquire) {
                    if let Some(toggle) = Self::target(ctx, "preview_larger") {
                        self.gesture(toggle, toggle);
                        self.pending.push_back(vec![Event::PointerGone]);
                        self.set_step(Step::CaptureLargerPreview);
                    }
                }
            }
            Step::CaptureLargerPreview => {
                if app.c.toasts.is_empty() && self.since.elapsed() >= Duration::from_millis(250) {
                    let Some(picture) = Self::target_rect(ctx, "preview_picture") else { return Ok(false) };
                    if Self::target(ctx, "preview_restore").is_some()
                        && picture.area() >= self.pictures.0.area() * 1.5
                        && picture.height() >= self.pictures.0.height() + 80.0
                    {
                        self.pictures.1 = picture;
                        self.take_screenshot(ctx, dir.join("recordings-larger-preview.png"));
                        self.set_step(Step::RestorePreview);
                    }
                }
            }
            Step::RestorePreview => {
                if self.screenshot.load(Ordering::Acquire) {
                    self.click(ctx, "preview_restore", Step::ArmLive);
                }
            }
            Step::ArmLive => {
                let restored = Self::target_rect(ctx, "preview_picture")
                    .is_some_and(|rect| (rect.size() - self.pictures.0.size()).abs().max_elem() < 2.0);
                if restored && app.recordings.qa_trim_marks() == Some(self.marks) {
                    std::fs::write(self.profile.join("qa-arm-live"), []).map_err(|e| e.to_string())?;
                    self.set_step(Step::GoLive);
                }
            }
            Step::GoLive => {
                if app.c.service.status().is_some_and(|s| s.recording.active && s.recording.written_until_ms > s.recording.started_ms) {
                    if app.recordings.qa_live_ready() {
                        self.click(ctx, "go_live", Step::LiveLatest);
                    } else {
                        app.recordings.mark_dirty();
                    }
                }
            }
            Step::LiveLatest => {
                if st.loaded && !st.paused && app.c.player.path.as_deref().is_some_and(|p| {
                    app.c.service.status().is_some_and(|s| s.recording.path == p)
                }) {
                    self.edge_lag = st.time_pos;
                    self.click(ctx, "jump_latest", Step::Done);
                }
            }
            Step::Done => {
                if st.loaded && !st.paused && st.last_frame && st.time_pos >= self.edge_lag {
                    let growth = self.pictures.1.area() / self.pictures.0.area();
                    std::fs::write(dir.join("interaction-result.txt"), format!(
                        "PASS: archive latest, previous and next frames, preview splitter, both trim handles, larger preview ({growth:.2}× picture area) and restore, Go live, live latest.\n"
                    )).map_err(|e| e.to_string())?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}
