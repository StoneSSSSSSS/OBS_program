//! Home: health at a glance, quick actions, live audio, recent clips, activity log.

use crate::app::{Ctx, Page};
use crate::theme::{self, icon};
use cv_core::ipc::{Request, Status};
use cv_core::model::{Clip, ClipFilter, EventEntry, Recording};
use cv_core::util::{format_duration, format_span_short, human_bytes, now_ms};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct HomeView {
    recent: Vec<Clip>,
    events: Vec<EventEntry>,
    footage_s: f64,
    recordings: Vec<Recording>,
    loaded: Option<Instant>,
    dirty: bool,
}

impl HomeView {
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn refresh(&mut self, c: &mut Ctx) {
        let stale = self.loaded.map(|t| t.elapsed() > Duration::from_secs(5)).unwrap_or(true);
        if !(self.dirty || stale) {
            return;
        }
        if let Some(db) = c.db() {
            self.recent = db.list_clips(&ClipFilter::default()).unwrap_or_default().into_iter().take(8).collect();
            self.events = db.recent_events(30).unwrap_or_default();
            self.recordings = db.list_recordings().unwrap_or_default();
            self.footage_s = self.recordings.iter().map(|r| r.duration).sum();
        }
        self.loaded = Some(Instant::now());
        self.dirty = false;
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        self.refresh(c);
        let status = c.service.status();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::heading("Home"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let live = status.as_ref().is_some_and(|s| {
                        s.recording.active && s.recording.written_until_ms > s.recording.started_ms
                    });
                    ui.add_enabled_ui(live, |ui| {
                        if theme::ghost_button(ui, Some(icon::FLAG), "Add marker")
                            .on_hover_text("Bookmark this moment on the 24/7 timeline")
                            .clicked() && c.request(Request::AddMarker { label: String::new() }).is_some()
                        {
                            c.toast("info", "Marker added");
                        }
                        if theme::primary_button(ui, Some(icon::LIVE), "Watch live").clicked() {
                            c.goto = Some(Page::Recordings);
                            crate::views::recordings::request_go_live();
                        }
                    }).response.on_disabled_hover_text("Available while recording full history.");
                });
            });
            ui.add_space(14.0);

            if let Some(s) = &status {
                for w in &s.warnings {
                    warning_banner(ui, w);
                }
                if !s.engine.running && !s.engine.last_error.is_empty() {
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, Some(icon::REFRESH), "Restart recorder").clicked() {
                            c.service.fire(Request::RestartRecorder);
                        }
                    });
                    ui.add_space(8.0);
                }
            } else {
                warning_banner(ui, "Connecting to the ClipVault service…");
            }

            let gap = 12.0;
            self.recent_footage(ui, c, status.as_ref());
            ui.add_space(gap);

            let cols = if ui.available_width() >= 960.0 { 4 } else { 2 };
            let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
            let cards: [&dyn Fn(&mut Ui, &mut Ctx, Option<&Status>, f64); 4] = [&recording_card, &replay_card, &mic_card, &storage_card];
            for row in cards.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for card in row {
                        ui.allocate_ui_with_layout(Vec2::new(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            theme::card().inner_margin(egui::Margin::same(13)).show(ui, |ui| {
                                ui.set_width(w - 28.0);
                                ui.set_min_height(130.0);
                                ui.vertical(|ui| card(ui, c, status.as_ref(), self.footage_s));
                            });
                        });
                    }
                });
                ui.add_space(gap);
            }

            if ui.available_width() >= 660.0 {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    let audio_w = (ui.available_width() * 0.27).clamp(230.0, 340.0);
                    let clips_w = ui.available_width() - audio_w - gap;
                    ui.allocate_ui_with_layout(Vec2::new(clips_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        self.recent_clips(ui, c);
                    });
                    ui.allocate_ui_with_layout(Vec2::new(audio_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        live_audio(ui, c);
                    });
                });
            } else {
                self.recent_clips(ui, c);
                ui.add_space(gap);
                live_audio(ui, c);
            }
            ui.add_space(gap);

            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Activity"));
                ui.add_space(6.0);
                if self.events.is_empty() {
                    ui.label(theme::label_dim("Nothing yet."));
                }
                for e in &self.events {
                    ui.horizontal(|ui| {
                        let color = match e.level.as_str() {
                            "error" => theme::DANGER,
                            "warn" => theme::WARNING,
                            _ => theme::TEXT_FAINT,
                        };
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, color);
                        let t = chrono::DateTime::from_timestamp_millis(e.at_ms)
                            .unwrap_or_default()
                            .with_timezone(&chrono::Local);
                        ui.label(
                            RichText::new(t.format("%b %d  %H:%M:%S").to_string())
                                .monospace()
                                .size(12.0)
                                .color(theme::TEXT_FAINT),
                        );
                        ui.label(RichText::new(&e.message).color(theme::TEXT_DIM));
                    });
                }
            });
        });
    }
}

impl HomeView {
    fn recent_clips(&self, ui: &mut Ui, c: &mut Ctx) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::title("Recent clips"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.link("View all clips").clicked() {
                        c.goto = Some(Page::Clips);
                    }
                });
            });
            if self.recent.is_empty() {
                ui.label(theme::label_dim(format!("No clips yet. Press {} in game to save one.", c.cfg.hotkeys.save_clip)));
                return;
            }
            egui::ScrollArea::horizontal().id_salt("recent").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for clip in &self.recent {
                        if recent_tile(ui, c, clip, 200.0).clicked() {
                            crate::views::clips::request_open(clip.id);
                            c.goto = Some(Page::Clips);
                        }
                    }
                });
            });
        });
    }

    fn recent_footage(&self, ui: &mut Ui, c: &mut Ctx, status: Option<&Status>) {
        let mut spans: Vec<_> = self.recordings.iter().filter(|r| !r.is_live()).map(|r| (r.start_ms, r.end_ms())).collect();
        if let Some(s) = status.filter(|s| c.cfg.recording.rolling_seconds().is_none() && s.recording.active) {
            spans.push((s.recording.started_ms, s.recording.written_until_ms));
        }
        spans.retain(|(a, b)| b > a);
        spans.sort_unstable();
        let latest_recording = spans.last().copied();
        let finalized = self.recordings.iter().find(|r| {
            !r.is_live() && latest_recording == Some((r.start_ms, r.end_ms()))
        });
        let mut joined: Vec<(i64, i64)> = Vec::new();
        for (start, end) in spans {
            if let Some(last) = joined.last_mut().filter(|s| start <= s.1 + 100) {
                last.1 = last.1.max(end);
            } else {
                joined.push((start, end));
            }
        }
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Recent footage"));
            ui.horizontal_top(|ui| {
                if let Some(recording) = finalized.filter(|_| ui.available_width() >= 760.0) {
                    if let Some(texture) = c.thumbs.recording(recording) {
                        let size = Vec2::new(216.0, 121.5);
                        let (frame, response) = ui.allocate_exact_size(size, Sense::click());
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), "Open latest recording preview"));
                        ui.painter().rect_filled(frame, 5.0, theme::SIDEBAR);
                        let image_size = texture.size_vec2();
                        let fit = (size.x / image_size.x).min(size.y / image_size.y);
                        let image = egui::Rect::from_center_size(frame.center(), image_size * fit);
                        egui::Image::new(&texture).corner_radius(egui::CornerRadius::same(5)).paint_at(ui, image);
                        ui.painter().rect_stroke(frame, 5.0, Stroke::new(1.0, if response.hovered() || response.has_focus() { theme::TEXT } else { theme::BORDER_STRONG }), egui::StrokeKind::Inside);
                        if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            crate::views::recordings::request_play_at(recording.start_ms);
                            c.goto = Some(Page::Recordings);
                        }
                        ui.add_space(8.0);
                    }
                }
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    self.footage_controls(ui, c, status, latest_recording, &joined);
                });
            });
        });
    }

    fn footage_controls(&self, ui: &mut Ui, c: &mut Ctx, status: Option<&Status>, latest_recording: Option<(i64, i64)>, joined: &[(i64, i64)]) {
        let rolling_minutes = c.cfg.recording.rolling_seconds().map(|seconds| seconds / 60);
        ui.horizontal_wrapped(|ui| {
            if let Some((start, end)) = latest_recording {
                let duration = format_duration((end - start) as f64 / 1000.0);
                if theme::primary_button(ui, Some(icon::PLAY), &format!("Open latest recording · {duration}")).clicked() {
                    crate::views::recordings::request_play_at(start);
                    c.goto = Some(Page::Recordings);
                }
            }
            {
                let available = if rolling_minutes.is_some() {
                    status.is_some_and(|s| s.replay.rolling_minutes == rolling_minutes
                        && s.replay.active && s.replay.available_seconds >= 30)
                } else {
                    joined.last().is_some_and(|(a, b)| b - a >= 30_000)
                };
                let save = ui.add_enabled(available && c.service.connected(), egui::Button::new("Clip last 30 seconds"));
                if save
                    .on_disabled_hover_text("Requires 30 seconds of continuous footage and the recorder service.")
                    .clicked()
                {
                    if rolling_minutes.is_some() {
                        c.service.fire(Request::SaveRecentRolling { seconds: 30 });
                    } else if let Some(&(_, end)) = joined.last() {
                        c.service.fire(Request::CutRange {
                            from_ms: end - 30_000,
                            to_ms: end,
                            title: String::new(),
                            game_id: None,
                            category_ids: vec![],
                            mix: None,
                            audio_edits: vec![],
                        });
                    }
                }
                if latest_recording.is_none() && ui.link("Recording settings").clicked() {
                    c.open_recording_settings = true;
                }
            }
        });
        if let Some(minutes) = rolling_minutes {
            ui.label(theme::label_dim(format!("Replay only keeps the last {minutes} min temporarily. Open Clips to edit a saved replay.")));
        }
        let Some(&(_, end)) = joined.last() else {
            if rolling_minutes.is_none() {
                ui.label(theme::label_dim("Recorded footage will appear here."));
            }
            return;
        };
        let begin = end - 3_600_000;
        let label = |ms| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap_or_default()
                .with_timezone(&chrono::Local)
                .format("%b %d  %H:%M")
                .to_string()
        };
        ui.label(theme::small(format!(
            "{}  –  {}    ·    Select a recorded section to play",
            label(begin),
            label(end)
        )));
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
        ui.painter().rect(rect, 4.0, theme::SIDEBAR, Stroke::new(1.0, theme::BORDER_STRONG), egui::StrokeKind::Inside);
        let x = |t: i64| rect.left() + ((t - begin) as f32 / 3_600_000.0).clamp(0.0, 1.0) * rect.width();
        for &(a, b) in joined.iter().filter(|(_, b)| *b > begin) {
            ui.painter()
                .rect_filled(egui::Rect::from_x_y_ranges(x(a)..=x(b), rect.shrink2(Vec2::new(0.0, 5.0)).y_range()), 2.0, c.accent);
        }
        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let t = begin + ((pos.x - rect.left()) / rect.width() * 3_600_000.0) as i64;
                if joined.iter().any(|&(a, b)| a <= t && t <= b) {
                    crate::views::recordings::request_play_at(t);
                    c.goto = Some(Page::Recordings);
                } else {
                    c.toast("info", "There is no recording at that time.");
                }
            }
        }
    }
}

fn live_audio(ui: &mut Ui, c: &mut Ctx) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::title("Live audio"));
        let meters = c.service.shared.lock().meters.clone();
        if meters.is_empty() {
            ui.label(theme::label_dim("No audio levels yet."));
        }
        let mut names: Vec<_> = meters.keys().collect();
        names.sort_by_key(|name| order_key(name));
        for name in names {
            ui.label(theme::small(name));
            theme::meter(ui, meters[name], ui.available_width());
        }
    });
}

fn order_key(name: &str) -> (u8, String) {
    let n = name.to_lowercase();
    let rank = if n.contains("desktop") {
        0
    } else if n.contains("game") {
        1
    } else if n.starts_with("voice") {
        2
    } else if n.starts_with("music") {
        3
    } else if n.contains("mic") {
        4
    } else {
        5
    };
    (rank, n)
}

fn warning_banner(ui: &mut Ui, text: &str) {
    egui::Frame::NONE
        .fill(theme::SURFACE2)
        .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::icon_text(icon::WARNING, 15.0).color(theme::WARNING));
                ui.label(RichText::new(text).color(theme::TEXT));
            });
        });
    ui.add_space(8.0);
}

fn big(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(RichText::new(text).font(egui::FontId::new(22.0, theme::bold())).color(color));
}

fn card_header(ui: &mut Ui, glyph: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(theme::icon_text(glyph, 14.0).color(theme::TEXT_FAINT));
        ui.label(RichText::new(title).color(theme::TEXT_DIM));
    });
    ui.add_space(4.0);
}

fn recording_card(ui: &mut Ui, c: &mut Ctx, s: Option<&Status>, footage_s: f64) {
    card_header(ui, icon::HISTORY, "24/7 recording");
    if let Some(minutes) = c.cfg.recording.rolling_seconds().map(|seconds| seconds / 60) {
        big(ui,"Replay only",theme::TEXT);
        ui.label(theme::label_dim(format!("Keep the last {minutes} min. Saved clips stay in your library.")));
        if ui.button("Change recording mode").clicked() { c.open_recording_settings=true; }
        return;
    }
    let Some(s) = s else {
        big(ui, "—", theme::TEXT_FAINT);
        return;
    };
    if s.recording.active {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            let t = ui.input(|i| i.time);
            let a = 0.55 + 0.45 * ((t * 2.5).sin() as f32 * 0.5 + 0.5);
            ui.painter().circle_filled(r.center(), 6.0, theme::LIVE.gamma_multiply(a));
            big(ui, "Recording", theme::TEXT);
        });
        ui.ctx().request_repaint_after(Duration::from_millis(80));
        let secs = (now_ms() - s.recording.started_ms).max(0) / 1000;
        ui.label(theme::label_dim(format!(
            "Current file {} · {} audio track{}",
            format_span_short(secs),
            s.recording.tracks,
            if s.recording.tracks == 1 { "" } else { "s" }
        )));
    } else if s.recording.paused_by_user {
        big(ui, "Paused", theme::TEXT_DIM);
        ui.label(theme::label_dim("Clips still work (replay buffer)."));
    } else {
        big(ui, "Stopped", theme::WARNING);
        ui.label(theme::label_dim("ClipVault will restart it automatically."));
    }
    ui.label(theme::small(format!(
        "{:.1} h stored · keeps {} h",
        footage_s / 3600.0,
        c.cfg.recording.retention_hours
    )));
    ui.add_space(6.0);
    let paused = s.recording.paused_by_user;
    if theme::ghost_button(ui, Some(if paused { icon::PLAY } else { icon::PAUSE }), if paused { "Resume" } else { "Pause" }).clicked() {
        c.service.fire(Request::SetRecording { enabled: paused });
    }
}

fn replay_card(ui: &mut Ui, c: &mut Ctx, s: Option<&Status>, _: f64) {
    card_header(ui, icon::BOLT, "Replay buffer");
    let Some(s) = s else {
        big(ui, "—", theme::TEXT_FAINT);
        return;
    };
    if s.replay.active {
        big(ui, &format!("Last {}", format_duration(s.replay.seconds as f64)), theme::TEXT);
        ui.label(theme::label_dim(format!("Press {} to save a clip", c.cfg.hotkeys.save_clip)));
    } else {
        big(ui, "Off", theme::DANGER);
        ui.label(theme::label_dim("Clips can't be saved right now."));
    }
    if let Some(g) = &s.game.current {
        ui.label(theme::small(format!("Playing: {g}")));
    } else {
        ui.label(theme::small("No game detected"));
    }
    ui.add_space(6.0);
    if theme::accent_button(ui, Some(icon::RECORD), "Save clip", c.accent).clicked() {
        c.service.fire(Request::SaveClip { short: false });
    }
}

fn mic_card(ui: &mut Ui, c: &mut Ctx, s: Option<&Status>, _: f64) {
    card_header(ui, icon::MIC, "Microphone");
    let Some(s) = s else {
        big(ui, "—", theme::TEXT_FAINT);
        return;
    };
    let (color, label) = match s.mic.state.as_str() {
        "ok" => (theme::SUCCESS, "Connected"),
        "fallback" => (theme::WARNING, "Using fallback"),
        "missing" => (theme::DANGER, "Not recording"),
        "disabled" => (theme::TEXT_DIM, "Guard off"),
        _ => (theme::TEXT_DIM, "Checking…"),
    };
    big(ui, label, color);
    let dev = if s.mic.device.is_empty() { "—".to_string() } else { s.mic.device.clone() };
    ui.label(theme::label_dim(dev).size(13.0));
    if s.mic.state == "fallback" && !s.mic.preferred.is_empty() {
        ui.label(theme::small(format!("Waiting for {}", s.mic.preferred)));
    }
    let peak = c
        .service
        .shared
        .lock()
        .meters
        .iter()
        .find(|(k, _)| k.to_lowercase().contains("mic"))
        .map(|(_, v)| *v)
        .unwrap_or(0.0);
    ui.add_space(6.0);
    theme::meter(ui, peak, ui.available_width().min(220.0));
}

fn storage_card(ui: &mut Ui, c: &mut Ctx, s: Option<&Status>, _: f64) {
    card_header(ui, icon::STORAGE, "Storage");
    let Some(s) = s else {
        big(ui, "—", theme::TEXT_FAINT);
        return;
    };
    let d = &s.disk;
    big(ui, &format!("{} free", human_bytes(d.free_bytes)), theme::TEXT);
    let frac = if d.total_bytes > 0 {
        1.0 - d.free_bytes as f32 / d.total_bytes as f32
    } else {
        0.0
    };
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(240.0), 6.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(3), theme::SURFACE3);
    let mut used = r;
    used.set_width(r.width() * frac);
    let low = d.free_bytes < c.cfg.recording.min_free_space_gb as u64 * 1024 * 1024 * 1024;
    ui.painter()
        .rect_filled(used, CornerRadius::same(3), if low { theme::WARNING } else { theme::TEXT_DIM });
    ui.add_space(4.0);
    ui.label(theme::label_dim(format!(
        "Recordings {} ({} files)",
        human_bytes(d.recordings_bytes),
        d.recordings_count
    )));
    ui.label(theme::label_dim(format!("Clips {} ({})", human_bytes(d.clips_bytes), d.clips_count)));
}

fn recent_tile(ui: &mut Ui, c: &mut Ctx, clip: &Clip, w: f32) -> egui::Response {
    let h = w * 9.0 / 16.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h + 42.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), &clip.title));
    let img = egui::Rect::from_min_size(rect.min, Vec2::new(w, h));
    let p = ui.painter();
    p.rect_filled(img, CornerRadius::same(10), theme::SURFACE2);
    if let Some(tex) = c.thumbs.get(&clip.thumb) {
        egui::Image::new(&tex).corner_radius(CornerRadius::same(10)).paint_at(ui, img);
    }
    if resp.hovered() || resp.has_focus() {
        ui.painter()
            .rect_stroke(img, CornerRadius::same(10), Stroke::new(1.5, theme::TEXT_DIM), egui::StrokeKind::Inside);
    }
    crate::views::clips::duration_badge(ui, img, clip.duration);
    ui.painter().text(
        egui::pos2(rect.left() + 2.0, img.bottom() + 8.0),
        egui::Align2::LEFT_TOP,
        crate::views::clips::truncate(&clip.title, 28),
        egui::FontId::new(13.0, theme::semibold()),
        theme::TEXT,
    );
    ui.painter().text(
        egui::pos2(rect.left() + 2.0, img.bottom() + 26.0),
        egui::Align2::LEFT_TOP,
        crate::views::clips::relative_time(clip.created_at),
        egui::FontId::proportional(12.0),
        theme::TEXT_FAINT,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_ui(|ui| {
        ui.set_max_width(420.0);
        ui.add(egui::Label::new(&clip.title).wrap());
    })
}
