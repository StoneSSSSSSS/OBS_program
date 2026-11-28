//! Exercise actual category buttons with pointer events in the isolated UI sweep.
use super::{App, Page};
use crate::views::clips::{qa_category_anchor, QA_CATEGORY_CHECK};
use eframe::egui::{self, Context, Event, Id, Rect, Vec2, ViewportCommand};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const SIZE: Vec2 = Vec2::new(900.0, 671.0);
const NAMES: [&str; 2] = ["Short", "Tournament highlights from the final round"];
const ANCHORS: [&str; 4] = ["title", "sort", "card-size", "search"];

pub(super) fn stop(ctx: &Context) {
    ctx.data_mut(|data| data.insert_temp(Id::unique(QA_CATEGORY_CHECK), false));
}

#[derive(Default)]
pub(super) struct CategoryCheck {
    ids: Option<[i64; 2]>,
    index: usize,
    pressed: bool,
    baseline: Option<[Rect; ANCHORS.len()]>,
    pending: VecDeque<Vec<Event>>,
    screenshot: Option<Arc<AtomicBool>>,
    started: Option<Instant>,
}

impl CategoryCheck {
    pub(super) fn inject(&mut self, input: &mut egui::RawInput) {
        if let Some(events) = self.pending.pop_front() {
            input.events.extend(events);
        }
    }

    pub(super) fn advance(&mut self, app: &mut App, ctx: &Context, dir: &Path) -> Result<bool, String> {
        let started = self.started.get_or_insert_with(Instant::now);
        if started.elapsed() > Duration::from_secs(10) {
            return Err(format!("category switch timed out at fixture index {}", self.index));
        }
        let Some(ids) = self.ids else {
            if app.c.cfg.recording.enabled || std::env::var_os("CLIPVAULT_TEST_FIXTURE").is_none() {
                return Err("category switch needs the capture-disabled fixture".into());
            }
            let db = app.c.db().ok_or("fixture database is unavailable")?;
            let clip = db.list_clips(&Default::default()).map_err(|e| e.to_string())?
                .into_iter().next().ok_or("category switch needs a seeded clip")?;
            let mut ids = [0; NAMES.len()];
            for (id, name) in ids.iter_mut().zip(NAMES) {
                *id = db.create_category(name, "").map_err(|e| e.to_string())?;
                db.add_clip_category(clip.id, *id).map_err(|e| e.to_string())?;
            }
            self.ids = Some(ids);
            ctx.data_mut(|data| data.insert_temp(Id::unique(QA_CATEGORY_CHECK), true));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(SIZE));
            app.navigate(Page::Clips);
            app.clips.qa_open_library(&mut app.c);
            return Ok(false);
        };

        if let Some(done) = &self.screenshot {
            if !done.load(Ordering::Acquire) {
                return Ok(false);
            }
            self.screenshot = None;
            self.index += 1;
            self.pressed = false;
            if self.index == ids.len() {
                return Ok(true);
            }
        }
        if !self.pending.is_empty() || (ctx.viewport_rect().size() - SIZE).abs().max_elem() >= 2.0 {
            return Ok(false);
        }

        let id = ids[self.index];
        if !self.pressed {
            let Some(rect) = ctx.data(|data| data.get_temp::<Rect>(qa_category_anchor(id)))
                .filter(|rect| rect.width() >= 5.0 && rect.height() >= 5.0)
            else { return Ok(false); };
            let pos = rect.center();
            let button = |pressed| Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.pending.push_back(vec![Event::PointerMoved(pos), button(true)]);
            self.pending.push_back(vec![button(false)]);
            self.pending.push_back(vec![Event::PointerGone]);
            self.pressed = true;
            return Ok(false);
        }
        if !app.clips.qa_category_selected(id) {
            return Ok(false);
        }
        let mut bounds = [Rect::NOTHING; ANCHORS.len()];
        for (value, name) in bounds.iter_mut().zip(ANCHORS) {
            *value = ctx.data(|data| data.get_temp::<Rect>(qa_category_anchor(name)))
                .filter(|rect| rect.is_positive())
                .ok_or_else(|| format!("category switch could not locate {name}"))?;
        }
        if let Some(expected) = self.baseline {
            for (name, (before, after)) in ANCHORS.into_iter().zip(expected.into_iter().zip(bounds)) {
                if before != after {
                    return Err(format!("category switch moved {name}: {before:?} -> {after:?}"));
                }
            }
            std::fs::write(dir.join("library-category-geometry.txt"), format!(
                "PASS: short and long category buttons selected at {:?}; exact matching bounds\n{:#?}\n",
                ctx.viewport_rect().size(), ANCHORS.into_iter().zip(bounds).collect::<Vec<_>>()
            )).map_err(|e| e.to_string())?;
        } else {
            self.baseline = Some(bounds);
        }
        let path = dir.join(if self.index == 0 { "library-category-short.png" } else { "library-category-long.png" });
        let done = Arc::new(AtomicBool::new(false));
        self.screenshot = Some(Arc::clone(&done));
        ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |image| {
            let rgba = image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect();
            if let Some(image) = image::RgbaImage::from_raw(image.size[0] as u32, image.size[1] as u32, rgba) {
                if image.save(&path).is_ok() {
                    done.store(true, Ordering::Release);
                }
            }
        })));
        Ok(false)
    }
}
