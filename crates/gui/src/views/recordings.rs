//! The 24/7 view: a zoomable wall-clock timeline across all recording files,
//! with game activity bands, markers and clips; live playback; clip-from-range.

use crate::app::Ctx;
use crate::theme::{self, icon};
use chrono::{Local, TimeZone};
use cv_core::ipc::Request;
use cv_core::model::{ActivitySpan, Category, Clip, Game, Marker, Recording};
use cv_core::timeline::{locate, Segment};
use cv_core::util::{format_duration, human_bytes, now_ms};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static GO_LIVE: AtomicBool = AtomicBool::new(false);

pub fn request_go_live() {
    GO_LIVE.store(true, Ordering::SeqCst);
}

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

struct CutDialog {
    title: String,
    game_id: Option<i64>,
    cats: Vec<i64>,
    precise: bool,
}

#[derive(Default)]
pub struct RecordingsView {
    recs: Vec<Recording>,
    spans: Vec<ActivitySpan>,
    markers: Vec<Marker>,
    clips: Vec<Clip>,
    games: Vec<Game>,
    cats: Vec<Category>,
    dirty: bool,
    loaded: Option<Instant>,
    view_end: i64,
    view_span: i64,
    follow_live: bool,
    cur: Option<usize>,
    cur_path: Option<String>,
    in_ms: Option<i64>,
    out_ms: Option<i64>,
    dialog: Option<CutDialog>,
    init: bool,
    rename_marker: Option<(i64, String)>,
}

impl RecordingsView {
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn refresh(&mut self, c: &mut Ctx) {
        let stale = self.loaded.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true);
        if !(self.dirty || stale) {
            return;
        }
        if let Some(db) = c.db() {
            self.recs = db.list_recordings().unwrap_or_default();
            let (from, to) = self.extent();
            self.spans = db.activity_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.markers = db.markers_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.clips = db.clips_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.games = db.list_games().unwrap_or_default();
            self.cats = db.list_categories().unwrap_or_default();
        }
        // Keep `cur` pointing at the same file after a refresh.
        if let Some(p) = &self.cur_path {
            self.cur = self.recs.iter().position(|r| &r.path == p);
        }
        self.loaded = Some(Instant::now());
        self.dirty = false;
    }

    fn seg_end(r: &Recording) -> i64 {
        if r.is_live() {
            now_ms()
        } else {
            r.start_ms + (r.duration.max(0.0) * 1000.0) as i64
        }
    }

    fn extent(&self) -> (i64, i64) {
        let start = self.recs.iter().map(|r| r.start_ms).min().unwrap_or(now_ms() - 6 * HOUR);
        let end = self.recs.iter().map(Self::seg_end).max().unwrap_or(now_ms()).max(now_ms());
        (start, end)
    }

    fn segments(&self) -> Vec<Segment> {
        self.recs
            .iter()
            .map(|r| Segment { path: PathBuf::from(&r.path), start_ms: r.start_ms, duration_ms: Self::seg_end(r) - r.start_ms, live: r.is_live() })
            .collect()
    }

    fn playhead(&self, c: &Ctx) -> Option<i64> {
        let i = self.cur?;
        let r = self.recs.get(i)?;
        let st = c.player.state();
        Some(r.start_ms + (st.time_pos * 1000.0) as i64)
    }

    fn tracks_for(&self, c: &Ctx, r: &Recording) -> usize {
        if r.audio_tracks > 0 {
            r.audio_tracks as usize
        } else if r.is_live() {
            c.service.status().map(|s| s.recording.tracks as usize).unwrap_or(0)
        } else {
            0
        }
    }

    /// Start playing at wall-clock time `t`.
    fn play_at(&mut self, c: &mut Ctx, t: i64, autoplay: bool) {
        let segs = self.segments();
        let Some(loc) = locate(&segs, t) else {
            c.toast("info", "Nothing was recorded at that time");
            return;
        };
        let rec = self.recs[loc.index].clone();
        let offset = loc.offset_ms as f64 / 1000.0;
        let same = self.cur == Some(loc.index) && c.player.path.as_deref() == Some(rec.path.as_str()) && c.player.state().loaded;
        let mpv_dur = c.player.state().duration;
        if same && !(rec.is_live() && offset > mpv_dur - 1.0) {
            c.player.seek(offset, true);
            if autoplay {
                if let Some(m) = &c.player.mpv {
                    m.set_pause(false);
                }
            }
        } else {
            let tracks = self.tracks_for(c, &rec);
            c.player.path = None; // force reload (live files need a fresh duration)
            c.player.open(&rec.path, offset, tracks, &[], None, rec.is_live(), autoplay, &c.cfg.player);
        }
        self.cur = Some(loc.index);
        self.cur_path = Some(rec.path.clone());
    }

    fn go_live(&mut self, c: &mut Ctx) {
        self.refresh(c);
        let t = now_ms() - c.cfg.player.live_offset_secs as i64 * 1000;
        self.follow_live = true;
        self.view_end = now_ms() + self.view_span / 20;
        self.play_at(c, t, true);
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.init {
            self.init = true;
            self.view_span = c.cfg.player.timeline_hours as i64 * HOUR;
            self.view_end = now_ms() + self.view_span / 20;
            self.follow_live = true;
            self.dirty = true;
        }
        self.refresh(c);
        if GO_LIVE.swap(false, Ordering::SeqCst) {
            self.go_live(c);
        }
        // Auto-advance to the next file at the end of a segment.
        let st = c.player.state();
        if let Some(i) = self.cur {
            if st.eof && st.loaded && i + 1 < self.recs.len() && c.player.path.is_some() {
                let next = self.recs[i + 1].start_ms;
                self.play_at(c, next, true);
            }
        }
        if self.follow_live {
            let target = now_ms() + self.view_span / 20;
            if (target - self.view_end).abs() > 500 {
                self.view_end = target;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }

        let hits = c.player.shortcuts(ui, &c.cfg.player);
        if hits.mark_in {
            self.in_ms = self.playhead(c);
        }
        if hits.mark_out {
            self.out_ms = self.playhead(c);
        }

        if c.player.fullscreen {
            let avail = ui.available_size();
            c.player.video(ui, Vec2::new(avail.x, avail.y - 150.0));
            egui::Frame::NONE.inner_margin(Margin::symmetric(16, 4)).show(ui, |ui| {
                self.timeline(ui, c);
                let label = self.time_label(c);
                c.player.transport(ui, &label, &c.cfg.player);
            });
            return;
        }

        // Header
        ui.horizontal(|ui| {
            ui.label(theme::heading("24/7 Recordings"));
            let total: f64 = self.recs.iter().filter(|r| !r.kept).map(|r| (Self::seg_end(r) - r.start_ms) as f64 / 1000.0).sum();
            ui.label(theme::label_dim(format!("{:.1} h stored · keeps {} h", total / 3600.0, c.cfg.recording.retention_hours)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::accent_button(ui, Some(icon::LIVE), "Go live", theme::LIVE).on_hover_text("Jump to what's being recorded right now").clicked() {
                    self.go_live(c);
                }
                ui.add_space(6.0);
                for (label, span) in [("All", 0i64), ("3d", 72 * HOUR), ("24h", 24 * HOUR), ("6h", 6 * HOUR), ("1h", HOUR), ("10m", 10 * MIN)] {
                    let span = if span == 0 {
                        let (a, b) = self.extent();
                        (b - a).max(HOUR) + HOUR / 2
                    } else {
                        span
                    };
                    let sel = (self.view_span - span).abs() < MIN;
                    if ui.add(egui::Button::selectable(sel, label)).clicked() {
                        let center = self.playhead(c).unwrap_or(self.view_end - self.view_span / 2);
                        self.view_span = span;
                        if label == "All" {
                            self.view_end = self.extent().1 + span / 40;
                            self.follow_live = false;
                        } else if self.follow_live {
                            self.view_end = now_ms() + span / 20;
                        } else {
                            self.view_end = center + span / 2;
                        }
                    }
                }
            });
        });
        ui.add_space(10.0);

        let side_w = 330.0;
        egui::Panel::right("rec_side")
            .exact_size(side_w)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(Margin { left: 18, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("rec_side_scroll").auto_shrink(false).show(ui, |ui| self.side(ui, c));
            });

        if self.recs.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::icon_text(icon::HISTORY, 40.0).color(theme::TEXT_FAINT));
                ui.label(theme::title("No recordings yet"));
                ui.label(theme::label_dim("Once OBS is recording, everything shows up here."));
            });
            return;
        }

        let avail = ui.available_size();
        let vh = (avail.x * 9.0 / 16.0).min(avail.y - 250.0).max(200.0);
        c.player.video(ui, Vec2::new(avail.x, vh));
        ui.add_space(6.0);
        self.timeline(ui, c);
        let label = self.time_label(c);
        c.player.transport(ui, &label, &c.cfg.player);
        ui.add_space(2.0);
        self.cut_bar(ui, c);
        self.cut_dialog(ui, c);
    }

    fn time_label(&self, c: &Ctx) -> String {
        match self.playhead(c) {
            Some(t) => {
                let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
                let behind = (now_ms() - t) / 1000;
                let live = self.cur.and_then(|i| self.recs.get(i)).map(|r| r.is_live()).unwrap_or(false);
                if live && behind < 30 {
                    format!("{}  ·  LIVE", dt.format("%a %d %b  %H:%M:%S"))
                } else {
                    format!("{}  ·  {} ago", dt.format("%a %d %b  %H:%M:%S"), cv_core::util::format_span_short(behind))
                }
            }
            None => "Click the timeline or Go live".into(),
        }
    }

    fn timeline(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 92.0), Sense::click_and_drag());
        let span = self.view_span.max(MIN);
        let start = self.view_end - span;
        let px_per_ms = rect.width() as f64 / span as f64;
        let x_of = |t: i64| rect.left() + ((t - start) as f64 * px_per_ms) as f32;
        let t_of = |x: f32| start + ((x - rect.left()) as f64 / px_per_ms) as i64;
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(10), theme::SURFACE);

        let ruler = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 20.0));
        let seg_band = Rect::from_x_y_ranges(rect.x_range(), rect.top() + 24.0..=rect.top() + 54.0);
        let act_band = Rect::from_x_y_ranges(rect.x_range(), rect.top() + 60.0..=rect.top() + 76.0);

        // Ticks.
        let steps = [MIN, 2 * MIN, 5 * MIN, 10 * MIN, 15 * MIN, 30 * MIN, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, 24 * HOUR];
        let step = *steps.iter().find(|s| **s as f64 * px_per_ms >= 90.0).unwrap_or(&(24 * HOUR));
        let tz = Local.timestamp_millis_opt(start).single().map(|d| d.offset().local_minus_utc() as i64 * 1000).unwrap_or(0);
        let mut t = ((start + tz) / step) * step - tz;
        while t <= self.view_end {
            let x = x_of(t);
            if x >= rect.left() {
                let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
                let midnight = dt.format("%H:%M").to_string() == "00:00";
                let label = if midnight || step >= 24 * HOUR { dt.format("%a %d").to_string() } else { dt.format("%H:%M").to_string() };
                p.line_segment([Pos2::new(x, ruler.bottom() - 4.0), Pos2::new(x, rect.bottom() - 4.0)], Stroke::new(1.0, if midnight { theme::BORDER_STRONG } else { Color32::from_white_alpha(10) }));
                p.text(Pos2::new(x + 4.0, ruler.top() + 3.0), Align2::LEFT_TOP, label, FontId::proportional(11.0), if midnight { theme::TEXT_DIM } else { theme::TEXT_FAINT });
            }
            t += step;
        }

        // Recording segments.
        for r in &self.recs {
            let (a, b) = (r.start_ms, Self::seg_end(r));
            if b < start || a > self.view_end {
                continue;
            }
            let sr = Rect::from_x_y_ranges(x_of(a).max(rect.left())..=x_of(b).min(rect.right()).max(x_of(a) + 1.0), seg_band.y_range());
            let fill = if r.kept { theme::blend(theme::SURFACE3, c.accent, 0.25) } else { theme::SURFACE3 };
            p.rect_filled(sr.shrink2(Vec2::new(0.5, 0.0)), CornerRadius::same(4), fill);
            if r.is_live() {
                p.rect_filled(Rect::from_x_y_ranges(sr.right() - 3.0..=sr.right(), sr.y_range()), CornerRadius::same(1), theme::LIVE);
            }
        }
        // Clips saved in this range.
        for cl in &self.clips {
            let a = cl.source_start_ms.unwrap_or(cl.created_at - (cl.duration * 1000.0) as i64);
            let b = cl.created_at;
            if b < start || a > self.view_end {
                continue;
            }
            let cr = Rect::from_x_y_ranges(x_of(a).max(rect.left())..=x_of(b).max(x_of(a) + 2.0), seg_band.bottom() - 6.0..=seg_band.bottom() - 2.0);
            p.rect_filled(cr, CornerRadius::same(2), c.accent.gamma_multiply(0.85));
        }
        // Activity bands.
        if c.cfg.player.show_activity_on_timeline {
            for s in &self.spans {
                if s.end_ms < start || s.start_ms > self.view_end {
                    continue;
                }
                let x0 = x_of(s.start_ms).max(rect.left());
                let x1 = x_of(s.end_ms).min(rect.right());
                if x1 - x0 < 0.5 {
                    continue;
                }
                let color = if s.is_game { theme::game_color(s.game_id.unwrap_or(0)) } else { Color32::from_white_alpha(28) };
                let br = Rect::from_x_y_ranges(x0..=x1, act_band.y_range());
                p.rect_filled(br, CornerRadius::same(3), color.gamma_multiply(if s.is_game { 0.85 } else { 1.0 }));
                if s.is_game && x1 - x0 > 60.0 {
                    p.text(Pos2::new(x0 + 5.0, br.center().y), Align2::LEFT_CENTER, crate::views::clips::truncate(&s.name, ((x1 - x0) / 7.0) as usize), FontId::new(10.5, theme::semibold()), Color32::from_black_alpha(220));
                }
            }
        }
        // Selection.
        if let (Some(a), Some(b)) = (self.in_ms, self.out_ms) {
            let (a, b) = (a.min(b), a.max(b));
            let sr = Rect::from_x_y_ranges(x_of(a)..=x_of(b), seg_band.top() - 2.0..=act_band.bottom() + 2.0);
            p.rect_filled(sr, CornerRadius::same(4), c.accent.gamma_multiply(0.22));
            p.rect_stroke(sr, CornerRadius::same(4), Stroke::new(1.0, c.accent), egui::StrokeKind::Inside);
        }
        for (pt, label) in [(self.in_ms, "IN"), (self.out_ms, "OUT")] {
            if let Some(t) = pt {
                let x = x_of(t);
                p.line_segment([Pos2::new(x, seg_band.top() - 4.0), Pos2::new(x, act_band.bottom() + 4.0)], Stroke::new(2.0, c.accent));
                p.text(Pos2::new(x, rect.bottom() - 2.0), Align2::CENTER_BOTTOM, label, FontId::new(9.0, theme::semibold()), c.accent);
            }
        }
        // Markers.
        for m in &self.markers {
            if m.at_ms < start || m.at_ms > self.view_end {
                continue;
            }
            let x = x_of(m.at_ms);
            let tri = vec![Pos2::new(x - 5.0, seg_band.top() - 6.0), Pos2::new(x + 5.0, seg_band.top() - 6.0), Pos2::new(x, seg_band.top() + 2.0)];
            p.add(egui::Shape::convex_polygon(tri, theme::WARNING, Stroke::NONE));
        }
        // Now + playhead.
        let nx = x_of(now_ms());
        if nx <= rect.right() {
            p.line_segment([Pos2::new(nx, seg_band.top() - 4.0), Pos2::new(nx, act_band.bottom())], Stroke::new(1.5, theme::LIVE));
        }
        if let Some(t) = self.playhead(c) {
            let x = x_of(t);
            p.line_segment([Pos2::new(x, ruler.bottom()), Pos2::new(x, rect.bottom() - 4.0)], Stroke::new(2.0, Color32::WHITE));
            p.circle_filled(Pos2::new(x, ruler.bottom()), 4.5, Color32::WHITE);
        }

        // Interaction.
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
            let mut tip = dt.format("%a %d %b %H:%M:%S").to_string();
            if let Some(s) = self.spans.iter().find(|s| s.start_ms <= t && s.end_ms >= t) {
                tip.push_str(&format!("\n{}", s.name));
            }
            if let Some(m) = self.markers.iter().find(|m| (x_of(m.at_ms) - pos.x).abs() < 6.0) {
                tip.push_str(&format!("\nMarker{}", if m.label.is_empty() { String::new() } else { format!(": {}", m.label) }));
            }
            if !self.recs.iter().any(|r| r.start_ms <= t && Self::seg_end(r) >= t) {
                tip.push_str("\nNot recorded");
            }
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::unique("tl_tip"), egui::PopupAnchor::Pointer).show(|ui| ui.label(tip));
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.1 {
                let factor = (1.0 - scroll as f64 * 0.0025).clamp(0.5, 2.0);
                let new_span = ((span as f64) * factor).clamp((2 * MIN) as f64, (30 * 24 * HOUR) as f64) as i64;
                let frac = (pos.x - rect.left()) as f64 / rect.width() as f64;
                let anchor = t_of(pos.x);
                self.view_end = anchor + ((1.0 - frac) * new_span as f64) as i64;
                self.view_span = new_span;
                if self.view_end < now_ms() {
                    self.follow_live = false;
                }
            }
        }
        if resp.dragged() {
            let dx = resp.drag_delta().x as f64;
            self.view_end -= (dx / px_per_ms) as i64;
            self.follow_live = false;
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                if ui.input(|i| i.modifiers.shift) {
                    // Shift-click sets the selection end nearest to the click.
                    match (self.in_ms, self.out_ms) {
                        (Some(a), _) if t > a => self.out_ms = Some(t),
                        _ => self.in_ms = Some(t),
                    }
                } else {
                    self.play_at(c, t.min(now_ms()), true);
                }
            }
        }
        resp.context_menu(|ui| {
            if let Some(pos) = ui.ctx().pointer_latest_pos() {
                let _ = pos;
            }
            if ui.button("Add marker at playhead").clicked() {
                if let (Some(t), Some(db)) = (self.playhead(c), c.db()) {
                    let _ = db.add_marker(t, "");
                }
                self.dirty = true;
                ui.close();
            }
            if ui.button("Clear selection").clicked() {
                self.in_ms = None;
                self.out_ms = None;
                ui.close();
            }
        });
        ui.label(theme::small("Click to play · drag to pan · scroll to zoom · Shift+click to select · I / O mark in and out"));
    }

    fn cut_bar(&mut self, ui: &mut Ui, c: &mut Ctx) {
        ui.horizontal(|ui| {
            let ph = self.playhead(c);
            let fmt = |t: Option<i64>| t.and_then(|t| Local.timestamp_millis_opt(t).single()).map(|d| d.format("%H:%M:%S").to_string()).unwrap_or("—".into());
            ui.add_enabled_ui(ph.is_some(), |ui| {
                if ui.button(format!("In {}", fmt(self.in_ms))).on_hover_text("Mark in at playhead (I)").clicked() {
                    self.in_ms = ph;
                }
                if ui.button(format!("Out {}", fmt(self.out_ms))).on_hover_text("Mark out at playhead (O)").clicked() {
                    self.out_ms = ph;
                }
            });
            if theme::icon_button(ui, icon::CLOSE, 12.0, "Clear selection").clicked() {
                self.in_ms = None;
                self.out_ms = None;
            }
            let valid = matches!((self.in_ms, self.out_ms), (Some(a), Some(b)) if (b - a).abs() > 500);
            if let (Some(a), Some(b)) = (self.in_ms, self.out_ms) {
                ui.label(theme::label_dim(format_duration((b - a).abs() as f64 / 1000.0)));
            }
            ui.add_enabled_ui(valid, |ui| {
                if theme::primary_button(ui, Some(icon::SCISSORS), "Create clip").clicked() {
                    self.dialog = Some(CutDialog { title: String::new(), game_id: None, cats: Vec::new(), precise: c.cfg.export.precise_cuts });
                }
            });
            ui.add_space(8.0);
            if theme::ghost_button(ui, Some(icon::FLAG), "Marker").on_hover_text("Add a marker at the playhead").clicked() {
                let t = ph.unwrap_or_else(now_ms);
                if let Some(db) = c.db() {
                    let _ = db.add_marker(t, "");
                }
                self.dirty = true;
            }
        });
    }

    fn cut_dialog(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let Some(d) = &mut self.dialog else { return };
        let (a, b) = match (self.in_ms, self.out_ms) {
            (Some(a), Some(b)) => (a.min(b), a.max(b)),
            _ => {
                self.dialog = None;
                return;
            }
        };
        let mut close = false;
        let mut create = false;
        egui::Modal::new(egui::Id::unique("cut_dialog")).show(ui.ctx(), |ui| {
            ui.set_width(420.0);
            ui.label(theme::title("Create clip from recording"));
            let from = Local.timestamp_millis_opt(a).single().unwrap_or_default();
            ui.label(theme::label_dim(format!("{} · {}", from.format("%a %d %b %H:%M:%S"), format_duration((b - a) as f64 / 1000.0))));
            ui.add_space(8.0);
            ui.label("Title");
            ui.add(egui::TextEdit::singleline(&mut d.title).hint_text("Automatic (game + date)").desired_width(f32::INFINITY));
            ui.add_space(4.0);
            ui.label("Game");
            let name = d.game_id.and_then(|g| self.games.iter().find(|x| x.id == g)).map(|g| g.name.clone()).unwrap_or_else(|| "Detect automatically".into());
            egui::ComboBox::from_id_salt("cut_game").selected_text(name).width(300.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.game_id, None, "Detect automatically");
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for g in &self.games {
                        ui.selectable_value(&mut d.game_id, Some(g.id), &g.name);
                    }
                });
            });
            if !self.cats.is_empty() {
                ui.add_space(4.0);
                ui.label("Categories");
                ui.horizontal_wrapped(|ui| {
                    for cat in &self.cats {
                        let on = d.cats.contains(&cat.id);
                        if ui.add(egui::Button::selectable(on, &cat.name)).clicked() {
                            if on {
                                d.cats.retain(|x| *x != cat.id);
                            } else {
                                d.cats.push(cat.id);
                            }
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.checkbox(&mut d.precise, "Frame-exact cut (re-encodes; slower)");
            ui.label(theme::small("Off: instant lossless cut, may start up to a couple of seconds early."));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, Some(icon::SCISSORS), "Create").clicked() {
                    create = true;
                }
                if theme::ghost_button(ui, None, "Cancel").clicked() {
                    close = true;
                }
            });
        });
        if create {
            let d = self.dialog.take().unwrap();
            c.request(Request::CutRange { from_ms: a, to_ms: b, title: d.title, game_id: d.game_id, category_ids: d.cats, precise: Some(d.precise) });
            c.toast("info", "Creating clip… it'll appear in Clips when done");
        } else if close {
            self.dialog = None;
        }
    }

    fn side(&mut self, ui: &mut Ui, c: &mut Ctx) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            c.player.mixer(ui, c.accent);
        });
        ui.add_space(12.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Jobs"));
            crate::views::clips::jobs_list(ui, c);
            if c.service.shared.lock().jobs.is_empty() {
                ui.label(theme::small("Clips you create from the timeline show progress here."));
            }
        });
        ui.add_space(12.0);
        // Markers.
        if !self.markers.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Markers"));
                for m in self.markers.clone().iter().rev().take(30) {
                    ui.horizontal(|ui| {
                        let dt = Local.timestamp_millis_opt(m.at_ms).single().unwrap_or_default();
                        if ui.link(dt.format("%a %H:%M:%S").to_string()).clicked() {
                            self.play_at(c, m.at_ms - 10_000, true);
                        }
                        match &mut self.rename_marker {
                            Some((id, text)) if *id == m.id => {
                                let r = ui.text_edit_singleline(text);
                                if r.lost_focus() {
                                    if let Some(db) = c.db() {
                                        let _ = db.rename_marker(m.id, text);
                                    }
                                    self.rename_marker = None;
                                    self.dirty = true;
                                } else {
                                    r.request_focus();
                                }
                            }
                            _ => {
                                let label = if m.label.is_empty() { "(no label)".to_string() } else { m.label.clone() };
                                if ui.label(RichText::new(label).color(theme::TEXT_DIM)).on_hover_text("Click to rename").clicked() {
                                    self.rename_marker = Some((m.id, m.label.clone()));
                                }
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, icon::DELETE, 11.0, "Delete marker").clicked() {
                                if let Some(db) = c.db() {
                                    let _ = db.delete_marker(m.id);
                                }
                                self.dirty = true;
                            }
                        });
                    });
                }
            });
            ui.add_space(12.0);
        }
        // Files.
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Recording files"));
            ui.label(theme::small("Locked files are never deleted automatically."));
            ui.add_space(4.0);
            let mut last_day = String::new();
            for (i, r) in self.recs.clone().iter().enumerate().rev() {
                let dt = Local.timestamp_millis_opt(r.start_ms).single().unwrap_or_default();
                let day = dt.format("%A %d %B").to_string();
                if day != last_day {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&day).size(12.0).color(theme::TEXT_FAINT));
                    last_day = day;
                }
                ui.horizontal(|ui| {
                    let playing = self.cur == Some(i);
                    let dur = (Self::seg_end(r) - r.start_ms) as f64 / 1000.0;
                    let text = format!("{}  {}", dt.format("%H:%M"), format_duration(dur));
                    let rt = RichText::new(text).color(if playing { theme::TEXT } else { theme::TEXT_DIM });
                    if ui.add(egui::Button::selectable(playing, rt)).clicked() {
                        self.play_at(c, r.start_ms, true);
                    }
                    if r.is_live() {
                        theme::pill(ui, theme::LIVE, "LIVE");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let glyph = icon::LOCK;
                        let tip = if r.kept { "Kept forever (click to allow deletion)" } else { "Keep this file forever" };
                        let resp = theme::icon_button(ui, glyph, 11.0, tip);
                        if r.kept {
                            ui.painter().circle_stroke(resp.rect.center(), 11.0, Stroke::new(1.0, c.accent));
                        }
                        if resp.clicked() {
                            c.request(Request::KeepRecording { recording_id: r.id, keep: !r.kept });
                            self.dirty = true;
                        }
                        ui.label(theme::small(human_bytes(r.size.max(0) as u64)));
                    });
                });
            }
        });
    }
}
