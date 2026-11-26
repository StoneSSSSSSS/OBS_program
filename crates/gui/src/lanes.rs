//! Audio lanes: one row per audio track under the timeline, showing the
//! waveform, a mute button and a volume slider. Drag across a lane to select
//! part of it, then mute it or make it louder. Edits are non-destructive: they
//! are stored as metadata and applied during playback/export only.

use crate::player::{MixMode, Player};
use crate::theme::{self, icon};
use cv_core::model::AudioEdit;
use cv_core::waveform::{wave_path, Waveform};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Width of the label column left of lanes (and of the timeline).
pub const GUTTER: f32 = 196.0;
/// Inset of the time axis inside the area right of the gutter.
pub const AXIS_INSET: f32 = 8.0;
const LANE_H: f32 = 40.0;

/// The time-axis rectangle for a full-width row rect.
pub fn axis_rect(row: Rect) -> Rect {
    Rect::from_x_y_ranges(row.left() + GUTTER + AXIS_INSET..=row.right() - AXIS_INSET, row.y_range())
}

/// A waveform placed on the lane time axis (`start`..`end` in axis units).
pub struct WaveSource<'a> {
    pub start: f64,
    pub end: f64,
    pub wave: &'a Waveform,
}

/// Caches waveform files by media path, reloading when the service updates them.
#[derive(Default)]
pub struct Waves {
    map: HashMap<String, (Option<Waveform>, Option<SystemTime>, Instant)>,
}

impl Waves {
    pub fn get(&mut self, media: &str) -> Option<&Waveform> {
        let now = Instant::now();
        let stale = self.map.get(media).map(|(_, _, t)| now.duration_since(*t) > Duration::from_secs(5)).unwrap_or(true);
        if stale {
            let wp = wave_path(Path::new(media));
            let mtime = std::fs::metadata(&wp).and_then(|m| m.modified()).ok();
            let entry = self.map.entry(media.to_string()).or_insert((None, None, now));
            if entry.1 != mtime || (entry.0.is_none() && mtime.is_some()) {
                entry.0 = mtime.and_then(|_| Waveform::load(&wp).ok());
                entry.1 = mtime;
            }
            entry.2 = now;
        }
        self.map.get(media).and_then(|(w, _, _)| w.as_ref())
    }
}

#[derive(Default)]
pub struct LaneState {
    /// Selected range: (track, start, end) in axis units.
    pub sel: Option<(usize, f64, f64)>,
    drag: Option<(usize, f64)>,
    pub all_tracks: bool,
    pub custom_pct: f32,
}

pub enum LaneAction {
    Seek(f64),
    /// Apply `gain` to `[a, b)` on these tracks (1.0 = remove edits there).
    Edit { tracks: Vec<usize>, a: f64, b: f64, gain: f32 },
    /// A track volume or the mix mode changed (persist the mix).
    MixChanged,
}

fn gain_label(g: f32) -> String {
    if g <= 0.0005 {
        "Muted".into()
    } else {
        let db = 20.0 * g.log10();
        if db >= 0.0 {
            format!("+{db:.0} dB")
        } else {
            format!("{db:.0} dB")
        }
    }
}

fn fmt_time(t: f64, wall: bool) -> String {
    if wall {
        chrono::DateTime::from_timestamp_millis((t * 1000.0) as i64)
            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
            .unwrap_or_default()
    } else {
        cv_core::util::format_duration_precise(t)
    }
}

/// Draw the lanes. `t0..t1` is the visible range in axis units; `wall` says
/// the axis is wall-clock seconds (24/7 view) rather than file seconds.
#[allow(clippy::too_many_arguments)]
pub fn lanes(
    ui: &mut Ui,
    player: &mut Player,
    t0: f64,
    t1: f64,
    wall: bool,
    sources: &[WaveSource],
    edits: &[AudioEdit],
    playhead: Option<f64>,
    st: &mut LaneState,
    accent: Color32,
) -> Vec<LaneAction> {
    let mut actions = Vec::new();
    let n = player.track_count;
    if st.custom_pct == 0.0 {
        st.custom_pct = 150.0;
    }

    // Header: mode switch and help.
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("Audio tracks").font(FontId::new(14.0, theme::semibold())).color(theme::TEXT));
        if n > 1 {
            ui.add_space(8.0);
            let full = player.mode == MixMode::Full;
            if ui.add(egui::Button::selectable(full, "Full mix")).on_hover_text("Play everything you heard plus your mic (track 1)").clicked() && !full {
                player.set_mode(MixMode::Full);
                actions.push(LaneAction::MixChanged);
            }
            if ui.add(egui::Button::selectable(!full, "Separate tracks")).on_hover_text("Play game, voice, music and mic separately so you can balance them").clicked() && full {
                player.set_mode(MixMode::Separate);
                actions.push(LaneAction::MixChanged);
            }
        }
        ui.add_space(10.0);
        ui.label(theme::small(if n > 1 {
            "Drag across a track to select part of it, then mute it or make it louder. The recording itself is never changed."
        } else {
            "This file has a single audio track. Recordings made since the upgrade have separate tracks."
        }));
    });

    // Selection toolbar.
    if let Some((tr, a, b)) = st.sel {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let name = if st.all_tracks { "all tracks".to_string() } else { player.track_names.get(tr).cloned().unwrap_or_default() };
            ui.label(RichText::new(format!("{}–{} on {}", fmt_time(a, wall), fmt_time(b, wall), name)).color(theme::TEXT));
            let tracks: Vec<usize> = if st.all_tracks { (0..n).collect() } else { vec![tr] };
            let apply = |gain: f32, actions: &mut Vec<LaneAction>| actions.push(LaneAction::Edit { tracks: tracks.clone(), a, b, gain });
            if ui.button("Mute").clicked() {
                apply(0.0, &mut actions);
            }
            if ui.button("Quieter (−6 dB)").clicked() {
                apply(0.5, &mut actions);
            }
            if ui.button("Louder (+6 dB)").clicked() {
                apply(2.0, &mut actions);
            }
            ui.add(egui::DragValue::new(&mut st.custom_pct).range(0.0..=400.0).suffix("%").speed(1.0));
            if ui.button("Set").on_hover_text("Set this part to the exact volume").clicked() {
                apply(st.custom_pct / 100.0, &mut actions);
            }
            if ui.button("Reset").on_hover_text("Remove edits in this part").clicked() {
                apply(1.0, &mut actions);
            }
            ui.checkbox(&mut st.all_tracks, "All tracks");
            if theme::icon_button(ui, icon::CLOSE, 11.0, "Clear selection").clicked() {
                st.sel = None;
            }
        });
    }
    if n == 0 {
        return actions;
    }

    let span = (t1 - t0).max(1e-6);
    for i in 0..n {
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), LANE_H), Sense::hover());
        let axis = axis_rect(row);
        let x_of = |t: f64| axis.left() + (((t - t0) / span) as f32 * axis.width());
        let t_of = |x: f32| t0 + ((x - axis.left()) / axis.width()) as f64 * span;
        let audible_gain = player.effective_gains().get(i).copied().unwrap_or(0.0);

        // ---- header (name, mute, volume)
        let head = Rect::from_min_size(row.min, Vec2::new(GUTTER - 4.0, LANE_H - 4.0));
        ui.painter().rect_filled(head, CornerRadius::same(8), theme::SURFACE);
        let name = player.track_names.get(i).cloned().unwrap_or_else(|| format!("Track {}", i + 1));
        let dim = audible_gain <= 0.0005;
        ui.painter().text(
            head.left_top() + egui::vec2(10.0, 6.0),
            Align2::LEFT_TOP,
            crate::views::clips::truncate(&name, 14),
            FontId::new(12.5, theme::semibold()),
            if dim { theme::TEXT_FAINT } else { theme::TEXT },
        );
        let mut g = player.stem_gains.get(i).copied().unwrap_or(1.0);
        if player.mode == MixMode::Full {
            g = if i == 0 { 1.0 } else { 0.0 };
        }
        let mute_rect = Rect::from_center_size(Pos2::new(head.right() - 16.0, head.top() + 13.0), Vec2::splat(22.0));
        let mresp = ui.interact(mute_rect, ui.scope_id().with(("lane_mute", i)), Sense::click());
        let glyph = if g <= 0.0005 { icon::MUTE } else { icon::VOLUME };
        if mresp.hovered() {
            ui.painter().circle_filled(mute_rect.center(), 11.0, theme::SURFACE3);
        }
        ui.painter().text(mute_rect.center(), Align2::CENTER_CENTER, glyph, FontId::new(11.0, theme::icons_family()), theme::TEXT_DIM);
        if mresp.on_hover_text(if g <= 0.0005 { "Unmute" } else { "Mute" }).clicked() {
            let new = if g <= 0.0005 { 1.0 } else { 0.0 };
            if player.mode == MixMode::Full && i == 0 && new == 0.0 {
                // Muting the full mix in full-mix mode: switch to stems with full mix off.
                player.stem_gains.resize(n, 1.0);
                player.stem_gains[0] = 0.0;
                player.set_mode(MixMode::Separate);
            } else {
                player.set_track_gain(i, new);
            }
            actions.push(LaneAction::MixChanged);
        }
        let slider_rect = Rect::from_min_size(head.left_top() + egui::vec2(8.0, 22.0), Vec2::new(GUTTER - 60.0, 14.0));
        let mut gv = g;
        let sresp = ui.put(slider_rect, egui::Slider::new(&mut gv, 0.0..=2.0).show_value(false).trailing_fill(true));
        if sresp.changed() {
            player.set_track_gain(i, gv);
            actions.push(LaneAction::MixChanged);
        }
        ui.painter().text(
            Pos2::new(head.right() - 8.0, slider_rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{:.0}%", gv * 100.0),
            FontId::monospace(10.5),
            theme::TEXT_FAINT,
        );

        // ---- waveform area
        let wave_area = Rect::from_x_y_ranges(axis.x_range(), row.top()..=row.bottom() - 4.0);
        let p = ui.painter_at(wave_area.expand2(Vec2::new(AXIS_INSET, 0.0)));
        p.rect_filled(wave_area, CornerRadius::same(6), theme::SURFACE);
        let mid = wave_area.center().y;
        let half = wave_area.height() / 2.0 - 3.0;
        let color = if dim { Color32::from_white_alpha(40) } else { theme::TEXT_DIM.gamma_multiply(0.85) };
        let px_step = 2.0;
        let mut x = wave_area.left();
        while x < wave_area.right() {
            let ta = t_of(x);
            let tb = t_of(x + px_step);
            let mut v = 0u8;
            for s in sources {
                if s.end <= ta || s.start >= tb {
                    continue;
                }
                v = v.max(s.wave.max_in(i, ta - s.start, tb - s.start, 48));
            }
            if v > 0 {
                let h = (v as f32 / 255.0) * half;
                p.line_segment([Pos2::new(x, mid - h), Pos2::new(x, mid + h)], Stroke::new(1.2, color));
            }
            x += px_step;
        }
        if sources.is_empty() {
            p.text(wave_area.center(), Align2::CENTER_CENTER, "waveform is being generated…", FontId::proportional(11.0), theme::TEXT_FAINT);
        }
        // Edits on this track.
        for e in edits.iter().filter(|e| e.track == i && e.end > t0 && e.start < t1) {
            let r = Rect::from_x_y_ranges(x_of(e.start).max(wave_area.left())..=x_of(e.end).min(wave_area.right()), wave_area.y_range());
            let tint = if e.gain <= 0.0005 {
                theme::DANGER
            } else if e.gain > 1.0 {
                theme::SUCCESS
            } else {
                theme::WARNING
            };
            p.rect_filled(r, CornerRadius::same(4), tint.gamma_multiply(0.22));
            p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, tint.gamma_multiply(0.7)), egui::StrokeKind::Inside);
            if r.width() > 46.0 {
                p.text(r.left_top() + egui::vec2(5.0, 3.0), Align2::LEFT_TOP, gain_label(e.gain), FontId::new(10.5, theme::semibold()), tint);
            }
        }
        // Selection.
        if let Some((tr, a, b)) = st.sel {
            if tr == i || st.all_tracks {
                let r = Rect::from_x_y_ranges(x_of(a).max(wave_area.left())..=x_of(b).min(wave_area.right()), wave_area.y_range());
                p.rect_filled(r, CornerRadius::same(4), accent.gamma_multiply(0.20));
                p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.5, accent), egui::StrokeKind::Inside);
            }
        }
        if let Some(ph) = playhead {
            let px = x_of(ph);
            if px >= wave_area.left() && px <= wave_area.right() {
                p.line_segment([Pos2::new(px, wave_area.top()), Pos2::new(px, wave_area.bottom())], Stroke::new(1.5, Color32::WHITE));
            }
        }

        // ---- interaction: drag = select, click = seek (or pick an edit)
        let resp = ui.interact(wave_area, ui.scope_id().with(("lane", i)), Sense::click_and_drag());
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                st.drag = Some((i, t_of(pos.x)));
            }
        }
        if resp.dragged() {
            if let (Some((tr, start)), Some(pos)) = (st.drag, resp.interact_pointer_pos()) {
                let t = t_of(pos.x.clamp(wave_area.left(), wave_area.right()));
                st.sel = Some((tr, start.min(t), start.max(t)));
            }
        }
        if resp.drag_stopped() {
            st.drag = None;
            if let Some((_, a, b)) = st.sel {
                if ((b - a) / span) * (axis.width() as f64) < 3.0 {
                    st.sel = None;
                }
            }
        }
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                match edits.iter().find(|e| e.track == i && e.start <= t && e.end >= t) {
                    Some(e) => {
                        st.sel = Some((i, e.start, e.end));
                        st.custom_pct = e.gain * 100.0;
                    }
                    None => {
                        st.sel = None;
                        actions.push(LaneAction::Seek(t));
                    }
                }
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::Text);
    }
    actions
}
