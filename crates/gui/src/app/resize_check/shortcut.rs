//! Exercise a saved player remap through actual native UI event dispatch.
use super::{App, Page};
use cv_core::config::Config;
use eframe::egui::{self, Context, Event, Id, Key, Modifiers, Pos2, Rect, ViewportCommand};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Step {
    OpenSettings, Edit, Input, Save, Saved, Reopen, Verify, Recordings, LastFrame,
    PreviousFrame, FocusSearch, TypeSearch, PreserveWhileTyping, Blur, UseShortcut,
    Arrive, Done,
}

pub(super) struct ShortcutCheck {
    step: Step,
    since: Instant,
    pending: VecDeque<Vec<Event>>,
    scale: f32,
    latest: f64,
    before: f64,
    screenshot: Arc<AtomicBool>,
}

impl Default for ShortcutCheck {
    fn default() -> Self {
        Self {
            step: Step::OpenSettings,
            since: Instant::now(),
            pending: VecDeque::new(),
            scale: 1.0,
            latest: 0.0,
            before: 0.0,
            screenshot: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ShortcutCheck {
    pub(super) fn inject(&mut self, input: &mut egui::RawInput) {
        if let Some(events) = self.pending.pop_front() {
            input.events.extend(events);
        }
    }

    fn next(&mut self, step: Step) {
        self.step = step;
        self.since = Instant::now();
    }

    fn target(ctx: &Context, name: &'static str) -> Option<Pos2> {
        ctx.data(|data| data.get_temp::<Rect>(Id::unique(("clipvault-interaction-target", name))))
            .filter(|rect| rect.width() >= 5.0 && rect.height() >= 5.0)
            .map(|rect| rect.center())
    }

    fn click(&mut self, ctx: &Context, target: &'static str, next: Step) {
        let Some(pos) = Self::target(ctx, target) else { return };
        let button = |pressed| Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE,
        };
        self.pending.push_back(vec![Event::PointerMoved(pos), button(true)]);
        self.pending.push_back(vec![button(false)]);
        self.next(next);
    }

    fn key(&mut self, key: Key, modifiers: Modifiers, text: Option<&str>) {
        let stroke = |pressed| Event::Key {
            key, physical_key: None, pressed, repeat: false, modifiers,
        };
        let mut down = vec![stroke(true)];
        if let Some(text) = text {
            down.push(Event::Text(text.to_owned()));
        }
        self.pending.push_back(down);
        self.pending.push_back(vec![stroke(false)]);
    }

    fn capture(&mut self, ctx: &Context, path: PathBuf) {
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

    pub(super) fn advance(&mut self, app: &mut App, ctx: &Context, dir: &Path) -> Result<bool, String> {
        if !self.pending.is_empty() {
            return Ok(false);
        }
        let player = app.c.player.state();
        if self.since.elapsed() > Duration::from_secs(10) && !matches!(self.step, Step::OpenSettings) {
            return Err(format!("{:?} timed out at {:.3}s (paused={}, last_frame={}, query={:?})",
                self.step, player.time_pos, player.paused, player.last_frame, app.recordings.qa_shortcut_query()));
        }
        match self.step {
            Step::OpenSettings => {
                self.scale = ctx.zoom_factor();
                ctx.data_mut(|data| {
                    data.insert_temp(Id::unique("clipvault-interaction-check"), true);
                    data.insert_temp(Id::unique("clipvault-qa-scroll-shortcut"), true);
                });
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(1182.0, 771.0)));
                app.navigate(Page::Settings);
                app.settings.qa_reopen_player_hotkeys();
                self.next(Step::Edit);
            }
            Step::Edit => {
                if app.settings.qa_latest_binding().as_deref() == Some("End")
                    && (ctx.viewport_rect().size() - egui::vec2(1182.0, 771.0)).abs().max_elem() < 3.0
                {
                    self.click(ctx, "shortcut_latest_primary", Step::Input);
                }
            }
            Step::Input if ctx.egui_wants_keyboard_input() => {
                self.key(Key::A, Modifiers::CTRL | Modifiers::COMMAND, None);
                self.key(Key::Y, Modifiers::NONE, Some("Y"));
                self.next(Step::Save);
            }
            Step::Save => {
                if app.settings.qa_latest_binding().as_deref() == Some("Y") {
                    self.click(ctx, "shortcut_settings_save", Step::Saved);
                }
            }
            Step::Saved => {
                if persisted_latest().as_deref() == Some("Y") {
                    ctx.set_zoom_factor(self.scale);
                    app.navigate(Page::Home);
                    Self::clear_target(ctx, "shortcut_latest_primary");
                    self.next(Step::Reopen);
                }
            }
            Step::Reopen => {
                app.navigate(Page::Settings);
                app.settings.qa_reopen_player_hotkeys();
                ctx.data_mut(|data| data.insert_temp(Id::unique("clipvault-qa-scroll-shortcut"), true));
                self.next(Step::Verify);
            }
            Step::Verify => {
                if Self::target(ctx, "shortcut_latest_primary").is_some()
                    && app.settings.qa_latest_binding().as_deref() == Some("Y")
                {
                    self.capture(ctx, dir.join("shortcut-settings-reopened.png"));
                    self.next(Step::Recordings);
                }
            }
            Step::Recordings => {
                if self.screenshot.load(Ordering::Acquire) {
                    Self::clear_target(ctx, "jump_latest");
                    app.navigate(Page::Recordings);
                    self.next(Step::LastFrame);
                }
            }
            Step::LastFrame => {
                if player.loaded {
                    self.click(ctx, "jump_latest", Step::PreviousFrame);
                }
            }
            Step::PreviousFrame => {
                if player.loaded && player.paused && player.last_frame {
                    self.latest = player.time_pos;
                    self.click(ctx, "previous_frame", Step::FocusSearch);
                }
            }
            Step::FocusSearch => {
                if player.paused && !player.last_frame && player.time_pos < self.latest {
                    self.before = player.time_pos;
                    if Self::target(ctx, "shortcut_recording_search").is_none() {
                        ctx.data_mut(|data| data.insert_temp(Id::unique("clipvault-qa-scroll-recording-search"), true));
                    }
                    self.click(ctx, "shortcut_recording_search", Step::TypeSearch);
                }
            }
            Step::TypeSearch if ctx.egui_wants_keyboard_input() => {
                self.key(Key::Y, Modifiers::NONE, Some("y"));
                self.next(Step::PreserveWhileTyping);
            }
            Step::PreserveWhileTyping => {
                if app.recordings.qa_shortcut_query().ends_with('y') {
                    if (player.time_pos - self.before).abs() > 0.001 || !player.paused || player.last_frame {
                        return Err(format!("typing the bound key changed the paused frame from {:.3}s to {:.3}s", self.before, player.time_pos));
                    }
                    self.capture(ctx, dir.join("shortcut-text-entry-kept-frame.png"));
                    self.next(Step::Blur);
                }
            }
            Step::Blur => {
                if self.screenshot.load(Ordering::Acquire) {
                    self.click(ctx, "shortcut_recording_search_heading", Step::UseShortcut);
                }
            }
            Step::UseShortcut if !ctx.egui_wants_keyboard_input() => {
                self.key(Key::Y, Modifiers::NONE, Some("y"));
                self.next(Step::Arrive);
            }
            Step::Arrive => {
                if player.paused && player.last_frame && player.time_pos > self.before
                    && (player.time_pos - self.latest).abs() < 0.002
                {
                    self.capture(ctx, dir.join("shortcut-jumped-to-latest.png"));
                    self.next(Step::Done);
                }
            }
            Step::Done => {
                if self.screenshot.load(Ordering::Acquire) {
                    std::fs::write(dir.join("shortcut-result.txt"), format!(
                        "PASS: changed End to Y via Settings, saved and reopened it; typed y into the recording search at {:.3}s without playback, then pressed Y outside the field and jumped to {:.3}s.\n",
                        self.before, self.latest,
                    )).map_err(|error| error.to_string())?;
                    return Ok(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn clear_target(ctx: &Context, name: &'static str) {
        ctx.data_mut(|data| data.remove::<Rect>(Id::unique(("clipvault-interaction-target", name))));
    }
}

fn persisted_latest() -> Option<String> {
    Config::load_or_default().player.hotkeys.get("jump_latest").map(|binding| binding[0].clone())
}
