//! Home: health at a glance, quick actions, live audio, recent clips, activity log.

use crate::app::{Ctx, Page};
use crate::theme::{self, icon};
use cv_core::ipc::{Request, Status};
use cv_core::model::{Clip, ClipFilter, EventEntry};
use cv_core::util::{format_duration, format_span_short, human_bytes, now_ms};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct HomeView {
    recent: Vec<Clip>,
    events: Vec<EventEntry>,
    footage_s: f64,
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
            self.footage_s = db.list_recordings().unwrap_or_default().iter().filter(|r| !r.kept).map(|r| r.duration).sum();
        }
        self.loaded = Some(Instant::now());
        self.dirty = false;
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        self.refresh(c);
        let status = c.service.status();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::heading("Overview"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::ghost_button(ui, Some(icon::FLAG), "Add marker").on_hover_text("Bookmark this moment on the 24/7 timeline").clicked() {
                        c.request(Request::AddMarker { label: String::new() });
                        c.toast("info", "Marker added");
                    }
                    if theme::primary_button(ui, Some(icon::LIVE), "Watch live").clicked() {
                        c.goto = Some(Page::Recordings);
                        crate::views::recordings::request_go_live();
                    }
                });
            });
            ui.add_space(14.0);

            if let Some(s) = &status {
                for w in &s.warnings {
                    warning_banner(ui, w);
                }
                if s.obs.closed_by_user || (!s.obs.running && !s.warnings.is_empty()) {
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, Some(icon::REFRESH), "Start OBS").clicked() {
                            c.service.fire(Request::RestartObs);
                c.toast("info", "Restarting OBS…");
                        }
                    });
                    ui.add_space(8.0);
                }
            } else {
                warning_banner(ui, "Connecting to the ClipVault service…");
            }

            let cols = if ui.available_width() > 1100.0 { 4 } else { 2 };
            let gap = 14.0;
            let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
            let cards: [&dyn Fn(&mut Ui, &mut Ctx, Option<&Status>, f64); 4] = [&recording_card, &replay_card, &mic_card, &storage_card];
            for row in cards.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for card in row {
                        ui.allocate_ui_with_layout(Vec2::new(w, 170.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            theme::card().show(ui, |ui| {
                                ui.set_width(w - 34.0);
                                ui.set_min_height(140.0);
                                ui.vertical(|ui| card(ui, c, status.as_ref(), self.footage_s));
                            });
                        });
                    }
                });
                ui.add_space(gap);
            }

            // Live audio + recent clips side by side.
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                let left_w = (ui.available_width() * 0.34).max(300.0);
                ui.allocate_ui_with_layout(Vec2::new(left_w, 300.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    theme::card().show(ui, |ui| {
                        ui.set_width(left_w - 34.0);
                        ui.label(theme::title("Live audio"));
                        ui.label(theme::small("What OBS is recording right now"));
                        ui.add_space(8.0);
                        let meters = c.service.shared.lock().meters.clone();
                        if meters.is_empty() {
                            ui.label(theme::label_dim("No audio levels yet."));
                        }
                        let mut names: Vec<_> = meters.keys().cloned().collect();
                        names.sort_by_key(|n| order_key(n));
                        egui::Grid::new("meters").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                            for n in names {
                                ui.label(RichText::new(&n).color(theme::TEXT_DIM));
                                theme::meter(ui, meters[&n], (left_w - 190.0).max(80.0));
                                ui.end_row();
                            }
                        });
                    });
                });
                let right_w = ui.available_width();
                ui.allocate_ui_with_layout(Vec2::new(right_w, 300.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    theme::card().show(ui, |ui| {
                        ui.set_width(right_w - 34.0);
                        ui.horizontal(|ui| {
                            ui.label(theme::title("Recent clips"));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.link("View all").clicked() {
                                    c.goto = Some(Page::Clips);
                                }
                            });
                        });
                        ui.add_space(6.0);
                        if self.recent.is_empty() {
                            ui.label(theme::label_dim(format!("No clips yet. Press {} in game to save one.", c.cfg.hotkeys.save_clip)));
                        }
                        let tw = 200.0;
                        egui::ScrollArea::horizontal().id_salt("recent").show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for clip in self.recent.clone() {
                                    if recent_tile(ui, c, &clip, tw).clicked() {
                                        crate::views::clips::request_open(clip.id);
                                        c.goto = Some(Page::Clips);
                                    }
                                }
                            });
                        });
                    });
                });
            });
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
                        let t = chrono::DateTime::from_timestamp_millis(e.at_ms).unwrap_or_default().with_timezone(&chrono::Local);
                        ui.label(RichText::new(t.format("%b %d  %H:%M:%S").to_string()).monospace().size(12.0).color(theme::TEXT_FAINT));
                        ui.label(RichText::new(&e.message).color(theme::TEXT_DIM));
                    });
                }
            });
        });
    }
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
        .fill(theme::WARNING.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, theme::WARNING.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(10))
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
        ui.label(theme::label_dim(format!("Current file {} · {} audio track{}", format_span_short(secs), s.recording.tracks, if s.recording.tracks == 1 { "" } else { "s" })));
    } else if s.recording.paused_by_user {
        big(ui, "Paused", theme::TEXT_DIM);
        ui.label(theme::label_dim("Clips still work (replay buffer)."));
    } else {
        big(ui, "Stopped", theme::WARNING);
        ui.label(theme::label_dim("ClipVault will restart it automatically."));
    }
    ui.label(theme::small(format!("{:.1} h stored · keeps {} h", footage_s / 3600.0, c.cfg.recording.retention_hours)));
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
    let peak = c.service.shared.lock().meters.iter().find(|(k, _)| k.to_lowercase().contains("mic")).map(|(_, v)| *v).unwrap_or(0.0);
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
    let frac = if d.total_bytes > 0 { 1.0 - d.free_bytes as f32 / d.total_bytes as f32 } else { 0.0 };
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(240.0), 6.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(3), theme::SURFACE3);
    let mut used = r;
    used.set_width(r.width() * frac);
    let low = d.free_bytes < c.cfg.recording.min_free_space_gb as u64 * 1024 * 1024 * 1024;
    ui.painter().rect_filled(used, CornerRadius::same(3), if low { theme::WARNING } else { theme::TEXT_DIM });
    ui.add_space(4.0);
    ui.label(theme::label_dim(format!("Recordings {} ({} files)", human_bytes(d.recordings_bytes), d.recordings_count)));
    ui.label(theme::label_dim(format!("Clips {} ({})", human_bytes(d.clips_bytes), d.clips_count)));
}

fn recent_tile(ui: &mut Ui, c: &mut Ctx, clip: &Clip, w: f32) -> egui::Response {
    let h = w * 9.0 / 16.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h + 42.0), Sense::click());
    let img = egui::Rect::from_min_size(rect.min, Vec2::new(w, h));
    let p = ui.painter();
    p.rect_filled(img, CornerRadius::same(10), theme::SURFACE2);
    if let Some(tex) = c.thumbs.get(&clip.thumb) {
        egui::Image::new(&tex).corner_radius(CornerRadius::same(10)).paint_at(ui, img);
    }
    if resp.hovered() {
        ui.painter().rect_stroke(img, CornerRadius::same(10), Stroke::new(1.5, theme::TEXT_DIM), egui::StrokeKind::Inside);
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
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
