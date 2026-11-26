//! The 24/7 view: a zoomable wall-clock timeline across all recording files
//! with an activity lane (what was focused), markers and clips, audio lanes
//! with non-destructive region edits, live playback and clip-from-range.

use crate::app::Ctx;
use crate::lanes::{self, LaneAction, LaneState, WaveSource, GUTTER};
use crate::theme::{self, icon};
use chrono::{Local, TimeZone};
use cv_core::ipc::Request;
use cv_core::model::{apply_edit, ActivitySpan, AudioEdit, Category, Clip, Game, Marker, Recording};
use cv_core::timeline::{locate, Segment};
use cv_core::util::{format_duration, human_bytes, now_ms};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static GO_LIVE: AtomicBool = AtomicBool::new(false);
static PLAY_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn request_go_live() {
    GO_LIVE.store(true, Ordering::SeqCst);
}

/// Ask the 24/7 view to show and play a wall-clock moment (Unix ms).
pub fn request_play_at(ms: i64) {
    PLAY_AT.store(ms, Ordering::SeqCst);
}

/// A stretch of playing one game (activity spans merged across short breaks).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub game_id: Option<i64>,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Group game activity into sessions: same game, breaks shorter than 10 min.
pub fn game_sessions(spans: &[ActivitySpan]) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    let mut games: Vec<&ActivitySpan> = spans.iter().filter(|s| s.is_game).collect();
    games.sort_by_key(|s| s.start_ms);
    for s in games {
        if let Some(last) = out.iter_mut().rev().find(|x| x.game_id == s.game_id && x.name == s.name) {
            if s.start_ms - last.end_ms < 10 * MIN {
                last.end_ms = last.end_ms.max(s.end_ms);
                continue;
            }
        }
        out.push(Session { name: s.name.clone(), game_id: s.game_id, start_ms: s.start_ms, end_ms: s.end_ms });
    }
    out.retain(|x| x.end_ms - x.start_ms >= MIN);
    out.sort_by_key(|x| std::cmp::Reverse(x.start_ms));
    out
}

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

struct CutDialog {
    title: String,
    game_id: Option<i64>,
    cats: Vec<i64>,
    precise: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Scrub,
    Pan,
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
    drag_mode: Option<DragMode>,
    scrub_resume: bool,
    last_scrub: Option<Instant>,
    /// Non-destructive edits on the 24/7 timeline (wall-clock seconds).
    edits: Vec<AudioEdit>,
    edits_dirty: Option<Instant>,
    lane_state: LaneState,
    /// Playhead when the player was last showing our file.
    resume_ms: Option<i64>,
}

fn edits_file() -> PathBuf {
    cv_core::paths::data_dir().join("timeline_edits.json")
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
            self.spans = merge_spans(db.activity_between(from - HOUR, to + HOUR).unwrap_or_default());
            self.markers = db.markers_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.clips = db.clips_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.games = db.list_games().unwrap_or_default();
            self.cats = db.list_categories().unwrap_or_default();
        }
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
        Some(r.start_ms + (c.player.state().time_pos * 1000.0) as i64)
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

    /// Edits for the loaded segment, in that file's time.
    fn segment_edits(&self) -> Vec<AudioEdit> {
        let Some(r) = self.cur.and_then(|i| self.recs.get(i)) else { return Vec::new() };
        let s0 = r.start_ms as f64 / 1000.0;
        let s1 = Self::seg_end(r) as f64 / 1000.0 + 3600.0; // live files keep growing
        self.edits
            .iter()
            .filter(|e| e.overlaps(s0, s1))
            .map(|e| AudioEdit { track: e.track, start: e.start - s0, end: e.end - s0, gain: e.gain })
            .collect()
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
        self.cur = Some(loc.index);
        self.cur_path = Some(rec.path.clone());
        if same && !(rec.is_live() && offset > mpv_dur - 1.0) {
            c.player.seek(offset, true);
            if autoplay {
                if let Some(m) = &c.player.mpv {
                    m.set_pause(false);
                }
            }
        } else {
            let tracks = self.tracks_for(c, &rec);
            c.player.edits = self.segment_edits();
            c.player.path = None; // force reload (live files need a fresh duration)
            c.player.open(&rec.path, offset, tracks, &[], None, rec.is_live(), autoplay, &c.cfg.player);
        }
    }

    /// Fast seek while dragging the playhead.
    fn scrub_to(&mut self, c: &mut Ctx, t: i64) {
        let segs = self.segments();
        let Some(loc) = locate(&segs, t.min(now_ms() - 2000)) else { return };
        let throttle = self.last_scrub.map(|i| i.elapsed() < Duration::from_millis(45)).unwrap_or(false);
        if self.cur == Some(loc.index) && c.player.state().loaded {
            if !throttle {
                c.player.seek(loc.offset_ms as f64 / 1000.0, false);
                self.last_scrub = Some(Instant::now());
            }
        } else if !throttle {
            self.play_at(c, t, false);
            self.last_scrub = Some(Instant::now());
        }
    }

    fn go_live(&mut self, c: &mut Ctx) {
        self.refresh(c);
        let t = now_ms() - c.cfg.player.live_offset_secs as i64 * 1000;
        self.follow_live = true;
        self.view_end = now_ms() + self.view_span / 20;
        self.play_at(c, t, true);
    }

    fn load_edits(&mut self) {
        if let Ok(text) = std::fs::read_to_string(edits_file()) {
            self.edits = serde_json::from_str(&text).unwrap_or_default();
        }
    }

    fn save_edits_if_due(&mut self) {
        if let Some(t) = self.edits_dirty {
            if t.elapsed() > Duration::from_millis(800) {
                // Forget edits for footage that no longer exists.
                if let Some(oldest) = self.recs.iter().map(|r| r.start_ms).min() {
                    let cutoff = oldest as f64 / 1000.0;
                    self.edits.retain(|e| e.end > cutoff);
                }
                if let Ok(s) = serde_json::to_string(&self.edits) {
                    let _ = cv_core::util::write_atomic(&edits_file(), s.as_bytes());
                }
                self.edits_dirty = None;
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.init {
            self.init = true;
            self.view_span = c.cfg.player.timeline_hours as i64 * HOUR;
            self.view_end = now_ms() + self.view_span / 20;
            self.follow_live = true;
            self.dirty = true;
            self.load_edits();
            if c.player.track_count == 0 {
                c.player.show_mixer = true;
            }
        }
        self.refresh(c);
        self.save_edits_if_due();
        // The player is shared with the clip view: take it back if needed.
        if let Some(p) = self.cur_path.clone() {
            if c.player.path.as_deref() == Some(p.as_str()) {
                self.resume_ms = self.playhead(c);
            } else if let Some(t) = self.resume_ms.take() {
                self.play_at(c, t, false);
            } else {
                self.cur = None;
                self.cur_path = None;
            }
        }
        if GO_LIVE.swap(false, Ordering::SeqCst) {
            self.go_live(c);
        }
        let jump = PLAY_AT.swap(0, Ordering::SeqCst);
        if jump != 0 {
            self.refresh(c);
            self.follow_live = false;
            self.view_end = jump + self.view_span / 2;
            self.play_at(c, jump, true);
        }
        // Auto-advance to the next file at the end of a segment.
        let st = c.player.state();
        if let Some(i) = self.cur {
            if st.eof && st.loaded && i + 1 < self.recs.len() && c.player.path.is_some() && self.drag_mode.is_none() {
                let next = self.recs[i + 1].start_ms;
                self.play_at(c, next, true);
            }
        }
        // Keep the player's edits in sync with the timeline's.
        if self.cur.is_some() {
            let e = self.segment_edits();
            c.player.set_edits(e);
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

        let fullscreen = c.player.fullscreen;
        if !fullscreen {
            self.header(ui, c);
            ui.add_space(10.0);
            egui::Panel::right("rec_side")
                .exact_size(330.0)
                .resizable(false)
                .frame(egui::Frame::NONE.inner_margin(Margin { left: 18, right: 0, top: 0, bottom: 0 }))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("rec_side_scroll").auto_shrink(false).show(ui, |ui| {
                        ui.set_max_width(ui.available_width());
                        self.side(ui, c);
                    });
                });
        }

        if self.recs.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::icon_text(icon::HISTORY, 40.0).color(theme::TEXT_FAINT));
                ui.label(theme::title("No recordings yet"));
                ui.label(theme::label_dim("Once OBS is recording, everything shows up here."));
            });
            return;
        }

        // Controls live in a bottom panel so they're never pushed off screen;
        // the video gets whatever space is left.
        let bottom_margin = if fullscreen { Margin::symmetric(16, 8) } else { Margin::ZERO };
        egui::Panel::bottom("rec_controls")
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(bottom_margin).fill(if fullscreen { theme::BG } else { Color32::TRANSPARENT }))
            .show(ui, |ui| {
                ui.add_space(6.0);
                self.timeline(ui, c);
                if c.player.show_mixer && !fullscreen {
                    ui.add_space(4.0);
                    self.audio_lanes(ui, c);
                }
                let label = self.time_label(c);
                c.player.transport(ui, &label, &c.cfg.player);
                if !fullscreen {
                    self.cut_bar(ui, c);
                }
            });
        let avail = ui.available_size();
        let h = if fullscreen { avail.y } else { (avail.x * 9.0 / 16.0).min(avail.y).max(120.0) };
        c.player.video(ui, Vec2::new(avail.x, h));
        self.cut_dialog(ui, c);
    }

    fn header(&mut self, ui: &mut Ui, c: &mut Ctx) {
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
        let (row, _) = ui.allocate_exact_size(Vec2::new(w, 96.0), Sense::hover());
        let rect = lanes::axis_rect(row);
        let span = self.view_span.max(MIN);
        let start = self.view_end - span;
        let px_per_ms = rect.width() as f64 / span as f64;
        let x_of = |t: i64| rect.left() + ((t - start) as f64 * px_per_ms) as f32;
        let t_of = |x: f32| start + ((x - rect.left()) as f64 / px_per_ms) as i64;

        // Gutter labels.
        let ruler = Rect::from_x_y_ranges(rect.x_range(), row.top()..=row.top() + 20.0);
        let seg_band = Rect::from_x_y_ranges(rect.x_range(), row.top() + 24.0..=row.top() + 54.0);
        let act_band = Rect::from_x_y_ranges(rect.x_range(), row.top() + 60.0..=row.top() + 80.0);
        let gp = ui.painter();
        let gx = row.left() + 10.0;
        gp.text(Pos2::new(gx, seg_band.center().y), Align2::LEFT_CENTER, "Recording", FontId::new(12.0, theme::semibold()), theme::TEXT_DIM);
        gp.text(Pos2::new(gx, act_band.center().y), Align2::LEFT_CENTER, "Activity", FontId::new(12.0, theme::semibold()), theme::TEXT_DIM);
        gp.text(Pos2::new(gx, act_band.bottom() + 9.0), Align2::LEFT_CENTER, "what was on screen", FontId::proportional(10.5), theme::TEXT_FAINT);

        let p = ui.painter_at(rect.expand2(Vec2::new(lanes::AXIS_INSET, 0.0)));
        p.rect_filled(Rect::from_x_y_ranges(rect.x_range(), row.y_range()), CornerRadius::same(8), theme::SURFACE);

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
                p.line_segment([Pos2::new(x, ruler.bottom() - 4.0), Pos2::new(x, act_band.bottom())], Stroke::new(1.0, if midnight { theme::BORDER_STRONG } else { Color32::from_white_alpha(10) }));
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
        // Clips saved in this range (thin accent bars along the bottom of the band).
        for cl in &self.clips {
            let a = cl.source_start_ms.unwrap_or(cl.created_at - (cl.duration * 1000.0) as i64);
            let b = cl.created_at;
            if b < start || a > self.view_end {
                continue;
            }
            let cr = Rect::from_x_y_ranges(x_of(a).max(rect.left())..=x_of(b).max(x_of(a) + 2.0), seg_band.bottom() - 6.0..=seg_band.bottom() - 2.0);
            p.rect_filled(cr, CornerRadius::same(2), c.accent.gamma_multiply(0.85));
        }
        // Activity lane: which program was focused.
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
                let color = if s.is_game { theme::game_color(s.game_id.unwrap_or(0)) } else { Color32::from_white_alpha(30) };
                let br = Rect::from_x_y_ranges(x0..=x1, act_band.y_range());
                p.rect_filled(br, CornerRadius::same(3), color);
                if x1 - x0 > 46.0 {
                    let text_color = if s.is_game { Color32::from_black_alpha(220) } else { theme::TEXT_DIM };
                    p.text(Pos2::new(x0 + 5.0, br.center().y), Align2::LEFT_CENTER, crate::views::clips::truncate(&s.name, ((x1 - x0) / 7.0) as usize), FontId::new(10.5, theme::semibold()), text_color);
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
                p.text(Pos2::new(x, row.bottom() - 2.0), Align2::CENTER_BOTTOM, label, FontId::new(9.0, theme::semibold()), c.accent);
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
        // Now + playhead (with a big grab handle).
        let nx = x_of(now_ms());
        if nx <= rect.right() {
            p.line_segment([Pos2::new(nx, seg_band.top() - 4.0), Pos2::new(nx, act_band.bottom())], Stroke::new(1.5, theme::LIVE));
        }
        if let Some(t) = self.playhead(c) {
            let x = x_of(t);
            p.line_segment([Pos2::new(x, ruler.bottom()), Pos2::new(x, act_band.bottom() + 4.0)], Stroke::new(2.0, Color32::WHITE));
            p.circle_filled(Pos2::new(x, ruler.bottom()), 7.0, Color32::WHITE);
        }

        // Interaction: drag = scrub (drag the ruler or right-drag = pan),
        // wheel = zoom, Shift+wheel = pan, Shift+click = select.
        let resp = ui.interact(Rect::from_x_y_ranges(rect.x_range(), row.y_range()), ui.scope_id().with("timeline"), Sense::click_and_drag());
        if resp.drag_started() {
            let on_ruler = resp.interact_pointer_pos().map(|p| p.y <= ruler.bottom()).unwrap_or(false);
            let secondary = ui.input(|i| i.pointer.secondary_down() || i.pointer.middle_down());
            self.drag_mode = Some(if on_ruler || secondary { DragMode::Pan } else { DragMode::Scrub });
            if self.drag_mode == Some(DragMode::Scrub) {
                self.scrub_resume = !c.player.state().paused;
                if let Some(m) = &c.player.mpv {
                    m.set_pause(true);
                }
                c.player.scrubbing = true;
                self.follow_live = false;
            }
        }
        if resp.dragged() {
            match self.drag_mode {
                Some(DragMode::Pan) => {
                    self.view_end -= (resp.drag_delta().x as f64 / px_per_ms) as i64;
                    self.follow_live = false;
                }
                Some(DragMode::Scrub) => {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        // Auto-pan when scrubbing past the edges.
                        if pos.x > rect.right() - 20.0 {
                            self.view_end += span / 60;
                        } else if pos.x < rect.left() + 20.0 {
                            self.view_end -= span / 60;
                        }
                        let t = t_of(pos.x.clamp(rect.left(), rect.right()));
                        self.scrub_to(c, t);
                    }
                }
                None => {}
            }
        }
        if resp.drag_stopped() {
            if self.drag_mode == Some(DragMode::Scrub) {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let t = t_of(pos.x.clamp(rect.left(), rect.right()));
                    self.play_at(c, t.min(now_ms() - 2000), self.scrub_resume);
                }
                c.player.scrubbing = false;
            }
            self.drag_mode = None;
        }
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
            let mut tip = dt.format("%a %d %b %H:%M:%S").to_string();
            if let Some(s) = self.spans.iter().find(|s| s.start_ms <= t && s.end_ms >= t) {
                tip.push_str(&format!("\nOn screen: {}", s.name));
            }
            if let Some(m) = self.markers.iter().find(|m| (x_of(m.at_ms) - pos.x).abs() < 6.0) {
                tip.push_str(&format!("\nMarker{}", if m.label.is_empty() { String::new() } else { format!(": {}", m.label) }));
            }
            if !self.recs.iter().any(|r| r.start_ms <= t && Self::seg_end(r) >= t) {
                tip.push_str("\nNot recorded");
            }
            if self.drag_mode.is_none() {
                egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::unique("tl_tip"), egui::PopupAnchor::Pointer).show(|ui| ui.label(tip));
            }
            let (dx, dy) = ui.input(|i| (i.smooth_scroll_delta.x, i.smooth_scroll_delta.y));
            if dx.abs() > 0.1 {
                self.view_end -= (dx as f64 / px_per_ms) as i64;
                self.follow_live = false;
            } else if dy.abs() > 0.1 {
                let factor = (1.0 - dy as f64 * 0.0025).clamp(0.5, 2.0);
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
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                if ui.input(|i| i.modifiers.shift) {
                    match (self.in_ms, self.out_ms) {
                        (Some(a), _) if t > a => self.out_ms = Some(t),
                        _ => self.in_ms = Some(t),
                    }
                } else {
                    self.play_at(c, t.min(now_ms() - 2000), true);
                }
            }
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.context_menu(|ui| {
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
        ui.horizontal(|ui| {
            ui.add_space(GUTTER + lanes::AXIS_INSET);
            ui.label(theme::small("Drag to scrub · drag the time ruler to pan · scroll to zoom · Shift+scroll to pan · Shift+click or I / O to select"));
        });
    }

    fn audio_lanes(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let span = self.view_span.max(MIN);
        let t0 = (self.view_end - span) as f64 / 1000.0;
        let t1 = self.view_end as f64 / 1000.0;
        // Waveforms for every visible recording file.
        let visible: Vec<(String, f64, f64)> = self
            .recs
            .iter()
            .filter(|r| Self::seg_end(r) as f64 / 1000.0 > t0 && (r.start_ms as f64 / 1000.0) < t1)
            .map(|r| (r.path.clone(), r.start_ms as f64 / 1000.0, Self::seg_end(r) as f64 / 1000.0))
            .collect();
        let mut owned = Vec::new();
        for (path, a, b) in &visible {
            if let Some(w) = c.waves.get(path) {
                owned.push((w.clone(), *a, *b));
            }
        }
        let sources: Vec<WaveSource> = owned.iter().map(|(w, a, b)| WaveSource { start: *a, end: *b, wave: w }).collect();
        let playhead = self.playhead(c).map(|t| t as f64 / 1000.0);
        let acts = lanes::lanes(ui, &mut c.player, t0, t1, true, &sources, &self.edits, playhead, &mut self.lane_state, c.accent);
        for a in acts {
            match a {
                LaneAction::Seek(t) => self.play_at(c, (t * 1000.0) as i64, !c.player.state().paused),
                LaneAction::Edit { tracks, a, b, gain } => {
                    for tr in &tracks {
                        apply_edit(&mut self.edits, *tr, a, b, gain);
                    }
                    if tracks.iter().any(|t| *t > 0) && c.player.mode == crate::player::MixMode::Full && gain != 1.0 {
                        c.player.set_mode(crate::player::MixMode::Separate);
                        c.toast("info", "Switched to separate tracks so the edit is audible");
                    }
                    self.edits_dirty = Some(Instant::now());
                    let e = self.segment_edits();
                    c.player.set_edits(e);
                }
                LaneAction::MixChanged => {}
            }
        }
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
            if !self.edits.is_empty() {
                ui.add_space(8.0);
                ui.label(theme::small(format!("{} audio edit{}", self.edits.len(), if self.edits.len() == 1 { "" } else { "s" })));
                if ui.small_button("Clear edits").on_hover_text("Remove every mute/volume edit on the timeline").clicked() {
                    self.edits.clear();
                    self.edits_dirty = Some(Instant::now());
                }
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
        let (a_s, b_s) = (a as f64 / 1000.0, b as f64 / 1000.0);
        let edits_in: Vec<AudioEdit> = self.edits.iter().filter(|e| e.overlaps(a_s, b_s)).copied().collect();
        let mut close = false;
        let mut create = false;
        egui::Modal::new(egui::Id::unique("cut_dialog")).show(ui.ctx(), |ui| {
            ui.set_width(440.0);
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
            ui.add_space(4.0);
            let mix_note = if edits_in.is_empty() { "Your current track volumes are saved with the clip." } else { "Your track volumes and the audio edits in this range are saved with the clip." };
            ui.label(theme::small(format!("{mix_note} All original audio tracks are kept, so you can change them later.")));
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
            let gains = if c.player.track_count > 1 { c.player.effective_gains() } else { Vec::new() };
            c.request(Request::CutRange {
                from_ms: a,
                to_ms: b,
                title: d.title,
                game_id: d.game_id,
                category_ids: d.cats,
                precise: Some(d.precise),
                gains,
                audio_edits: edits_in,
            });
            c.toast("info", "Creating clip… it'll appear in Clips when done");
        } else if close {
            self.dialog = None;
        }
    }

    fn side(&mut self, ui: &mut Ui, c: &mut Ctx) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Jobs"));
            crate::views::clips::jobs_list(ui, c);
            if c.service.shared.lock().jobs.is_empty() {
                ui.label(theme::small("Clips you create from the timeline show progress here."));
            }
        });
        ui.add_space(12.0);
        let sessions = game_sessions(&self.spans);
        if !sessions.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Game sessions"));
                ui.label(theme::small("Click to jump to the start."));
                ui.add_space(4.0);
                for s in sessions.iter().take(25) {
                    let dt = Local.timestamp_millis_opt(s.start_ms).single().unwrap_or_default();
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, theme::game_color(s.game_id.unwrap_or(0)));
                        let label = format!("{}  ·  {}", crate::views::clips::truncate(&s.name, 20), dt.format("%a %H:%M"));
                        if ui.add(egui::Label::new(RichText::new(label).color(theme::TEXT)).truncate().sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.follow_live = false;
                            // Frame the whole session with a little margin on both sides.
                            let dur = s.end_ms - s.start_ms;
                            self.view_span = (dur * 12 / 10).max(10 * MIN);
                            self.view_end = s.start_ms - (self.view_span - dur) / 2 + self.view_span;
                            self.play_at(c, s.start_ms, true);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(theme::small(cv_core::util::format_span_short((s.end_ms - s.start_ms) / 1000)));
                        });
                    });
                }
            });
            ui.add_space(12.0);
        }
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
                        let editing = matches!(&self.rename_marker, Some((id, _)) if *id == m.id);
                        if editing {
                            if let Some((_, text)) = &mut self.rename_marker {
                                let r = ui.add(egui::TextEdit::singleline(text).desired_width(130.0).char_limit(60));
                                let done = r.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
                                if done {
                                    let label = text.trim().to_string();
                                    if let Some(db) = c.db() {
                                        let _ = db.rename_marker(m.id, &label);
                                    }
                                    self.rename_marker = None;
                                    self.dirty = true;
                                } else if !r.has_focus() {
                                    r.request_focus();
                                }
                            }
                        } else {
                            let label = if m.label.is_empty() { "(no label)".to_string() } else { m.label.clone() };
                            let resp = ui.add(egui::Label::new(RichText::new(label).color(theme::TEXT_DIM)).truncate().sense(Sense::click()));
                            if resp.on_hover_text("Click to rename").clicked() {
                                self.rename_marker = Some((m.id, m.label.clone()));
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
                        let tip = if r.kept { "Kept forever (click to allow deletion)" } else { "Keep this file forever" };
                        let resp = theme::icon_button(ui, icon::LOCK, 11.0, tip);
                        if r.kept {
                            ui.painter().circle_stroke(resp.rect.center(), 11.0, Stroke::new(1.0, c.accent));
                        }
                        if resp.clicked() {
                            c.service.fire(Request::KeepRecording { recording_id: r.id, keep: !r.kept });
                            self.dirty = true;
                        }
                        ui.label(theme::small(human_bytes(r.size.max(0) as u64)));
                    });
                });
            }
        });
    }
}

/// Merge consecutive activity spans of the same program separated by short
/// gaps (alt-tab blips) so the lane reads cleanly.
fn merge_spans(mut spans: Vec<ActivitySpan>) -> Vec<ActivitySpan> {
    spans.sort_by_key(|s| s.start_ms);
    let mut out: Vec<ActivitySpan> = Vec::with_capacity(spans.len());
    for s in spans {
        if let Some(last) = out.last_mut() {
            if last.key == s.key && s.start_ms - last.end_ms < 15_000 {
                last.end_ms = last.end_ms.max(s.end_ms);
                continue;
            }
            // Fill tiny gaps between different programs too.
            if s.start_ms - last.end_ms < 3_000 && s.start_ms > last.end_ms {
                last.end_ms = s.start_ms;
            }
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(key: &str, a: i64, b: i64) -> ActivitySpan {
        ActivitySpan { id: 0, key: key.into(), game_id: None, name: key.into(), is_game: false, title: String::new(), start_ms: a, end_ms: b }
    }

    #[test]
    fn groups_game_sessions() {
        let g = |name: &str, id: i64, a: i64, b: i64| ActivitySpan { id: 0, key: name.into(), game_id: Some(id), name: name.into(), is_game: true, title: String::new(), start_ms: a, end_ms: b };
        let mut spans = vec![g("Deadlock", 1, 0, 20 * MIN), g("Deadlock", 1, 25 * MIN, 50 * MIN), g("CS2", 2, 60 * MIN, 61 * MIN - 1), g("Deadlock", 1, 3 * HOUR, 3 * HOUR + 30 * MIN)];
        spans.push(span("chrome", 20 * MIN, 25 * MIN));
        let s = game_sessions(&spans);
        // Newest first; the 5-minute break merges; the <1 min CS2 blip is dropped.
        assert_eq!(s.len(), 2);
        assert_eq!((s[1].start_ms, s[1].end_ms), (0, 50 * MIN));
        assert_eq!(s[0].start_ms, 3 * HOUR);
    }

    #[test]
    fn merges_activity_blips() {
        let v = merge_spans(vec![span("a", 0, 10_000), span("a", 12_000, 20_000), span("b", 21_000, 30_000), span("a", 60_000, 70_000)]);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].start_ms, v[0].end_ms), (0, 21_000));
        assert_eq!(v[1].key, "b");
        assert_eq!(v[2].start_ms, 60_000);
    }

    #[allow(dead_code)]
    fn _types(_: Option<Pos2>) {}
}
