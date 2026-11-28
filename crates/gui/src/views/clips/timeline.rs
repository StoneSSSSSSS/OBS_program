//! Clip timeline and range controls.

use super::ClipsView;
use crate::app::Ctx;
use crate::lanes::gutter_width;
use crate::theme;
use cv_core::util::format_duration_precise;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};

mod viewport;
use viewport::TimelineViewport;

#[derive(Clone, Copy)]
enum TimelineDrag {
    In,
    Out,
    Select { anchor: f64 },
    Scrub,
}

#[derive(Default)]
pub(super) struct ClipTimeline {
    view: TimelineViewport,
    drag: Option<TimelineDrag>,
    last_scrub: Option<Instant>,
}

impl ClipTimeline {
    pub(super) fn range(&self, duration: f64) -> (f64, f64) {
        let window = self.view.visible(duration);
        (window.start, window.end())
    }
}

impl ClipsView {
    pub(super) fn timeline_controls(&mut self, ui: &mut Ui, c: &mut Ctx, duration: f64, lanes_on: bool) {
        let at = c.player.state().time_pos;
        let zoom = |view: &mut TimelineViewport, ui: &mut Ui| {
            for (label, factor, help) in [("−", 2.0, "Zoom out"), ("+", 0.5, "Zoom in at playhead")] {
                if ui.small_button(label).on_hover_text(help).clicked() {
                    view.zoom(duration, factor, at, 0.5);
                }
            }
            if ui.small_button("Fit clip").clicked() {
                view.fit();
            }
        };
        if lanes_on && ui.available_width() < 395.0 {
            ui.horizontal_wrapped(|ui| zoom(&mut self.detail.timeline.view, ui));
            self.clip_timeline(ui, c, duration, false);
        } else {
            let gutter = gutter_width(ui.available_width());
            ui.horizontal(|ui| {
                if lanes_on {
                    ui.allocate_ui_with_layout(
                        Vec2::new((gutter - ui.spacing().item_spacing.x).max(1.0), 30.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| zoom(&mut self.detail.timeline.view, ui),
                    );
                }
                self.clip_timeline(ui, c, duration, false);
            });
        }
    }

    pub(super) fn mark_in(&mut self, at: f64, duration: f64) -> Result<(), &'static str> {
        let at = at.clamp(0.0, duration);
        if self.detail.out_pt.unwrap_or(duration) - at <= 0.1 {
            return Err("Choose a start at least 0.1 seconds before the end.");
        }
        self.detail.in_pt = Some(at);
        Ok(())
    }

    pub(super) fn mark_out(&mut self, at: f64, duration: f64) -> Result<(), &'static str> {
        let at = at.clamp(0.0, duration);
        if at - self.detail.in_pt.unwrap_or(0.0) <= 0.1 {
            return Err("Choose an end at least 0.1 seconds after the start.");
        }
        self.detail.out_pt = Some(at);
        Ok(())
    }

    pub(super) fn selection(&self, duration: f64) -> Option<(f64, f64)> {
        if self.detail.in_pt.is_none() && self.detail.out_pt.is_none() {
            return None;
        }
        let start = self.detail.in_pt.unwrap_or(0.0).clamp(0.0, duration);
        let end = self.detail.out_pt.unwrap_or(duration).clamp(0.0, duration);
        (end > start + 0.1).then_some((start, end))
    }

    pub(super) fn clip_timeline(&mut self, ui: &mut Ui, c: &mut Ctx, duration: f64, watching: bool) {
        let window = self.detail.timeline.view.visible(duration);
        let (start, end) = (window.start, window.end());
        let width = ui.available_width().max(1.0);
        let height = if watching { ui.available_height().clamp(1.0, 40.0) } else { 30.0 };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
        let compact = watching.then(|| crate::views::editor_layout::WatchingRuler::new(rect));
        let axis = compact.as_ref().map_or(rect, |ruler| ruler.axis);
        let x_of = |time| axis.left() + window.fraction_at(time) as f32 * axis.width();
        let fraction_of = |x: f32| f64::from((x - axis.left()) / axis.width());
        let t_of = |x| window.time_at(fraction_of(x));
        let visible_x = |at: f64| (start..=end).contains(&at).then(|| x_of(at));
        let time = c.player.state().time_pos;
        let mut hover_handle = false;
        if let Some(compact) = &compact {
            let marks = [self.detail.in_pt.and_then(visible_x), self.detail.out_pt.and_then(visible_x)];
            compact.paint(ui, rect, [&format_duration_precise(start), &format_duration_precise(end)],
                marks, self.selection(duration).filter(|(a, b)| *a < end && *b > start).map(|(a, b)| (x_of(a), x_of(b))),
                visible_x(time), c.accent, [(axis.left(), axis.right())]);
            hover_handle = response.hover_pos().is_some_and(|pos| {
                marks.into_iter().flatten().any(|x| (x - pos.x).abs() <= 6.0)
            });
        } else {
            let p = ui.painter_at(rect);
            p.rect(rect, 5.0, theme::SIDEBAR, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
            if let Some((a, b)) = self.selection(duration) {
                if a < end && b > start {
                    let selection = Rect::from_x_y_ranges(x_of(a)..=x_of(b), rect.top() + 13.0..=rect.bottom());
                    p.rect(selection, 2.0, c.accent.gamma_multiply(0.18), Stroke::new(1.0, c.accent), egui::StrokeKind::Inside);
                }
            }
            let intervals = if width >= 360.0 { 4 } else if width >= 190.0 { 2 } else { 1 };
            for i in 0..=intervals {
                let at = window.time_at(i as f64 / f64::from(intervals));
                let x = x_of(at);
                let align = if i == 0 { Align2::LEFT_TOP } else if i == intervals { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
                p.text(Pos2::new(x, rect.top() + 2.0), align, format_duration_precise(at), FontId::monospace(10.0), theme::TEXT_DIM);
            }
            for (mark, label) in [(self.detail.in_pt, "Start"), (self.detail.out_pt, "End")] {
                if let Some(at) = mark.filter(|at| (start..=end).contains(at)) {
                    let x = x_of(at);
                    p.line_segment(
                        [Pos2::new(x, rect.top() + 13.0), Pos2::new(x, rect.bottom())],
                        Stroke::new(2.0, c.accent),
                    );
                    p.circle_filled(Pos2::new(x, rect.top() + 19.0), 4.0, c.accent);
                    hover_handle |= response.hover_pos().is_some_and(|pos| (pos.x - x).abs() <= 6.0);
                    p.text(Pos2::new(x + 5.0, rect.bottom()), Align2::LEFT_BOTTOM, label,
                        FontId::monospace(10.0), theme::TEXT);
                }
            }
            if (start..=end).contains(&time) {
                let x = x_of(time);
                p.line_segment([Pos2::new(x, rect.top() + 14.0), Pos2::new(x, rect.bottom())], Stroke::new(2.0, Color32::WHITE));
            }
        }
        if let Some(pos) = response.interact_pointer_pos() {
            let at = t_of(pos.x).min(duration);
            if response.clicked() {
                self.stop_preview(c);
                if ui.input(|i| i.modifiers.shift) {
                    if self.detail.in_pt.is_some_and(|a| a < at) {
                        c.report_mark(self.mark_out(at, duration));
                    } else {
                        c.report_mark(self.mark_in(at, duration));
                    }
                } else {
                    c.player.seek(at, true);
                }
            }
            if response.drag_started_by(egui::PointerButton::Primary) {
                self.stop_preview(c);
                let origin = pos.x - response.total_drag_delta().unwrap_or_default().x;
                let at_edge = |mark: Option<f64>| {
                    mark.is_some_and(|t| (start..=end).contains(&t) && (x_of(t) - origin).abs() <= 6.0)
                };
                self.detail.timeline.drag = Some(if at_edge(self.detail.in_pt) {
                    TimelineDrag::In
                } else if at_edge(self.detail.out_pt) {
                    TimelineDrag::Out
                } else if ui.input(|i| i.modifiers.shift) {
                    TimelineDrag::Select { anchor: t_of(origin).min(duration) }
                } else {
                    TimelineDrag::Scrub
                });
            }
            let released = response.drag_stopped_by(egui::PointerButton::Primary);
            if response.dragged_by(egui::PointerButton::Primary) || released {
                match self.detail.timeline.drag {
                    Some(TimelineDrag::In) => {
                        let latest = self.detail.out_pt.map_or(duration, |b| (b - 0.101).max(0.0));
                        self.detail.in_pt = Some(at.min(latest));
                    }
                    Some(TimelineDrag::Out) => {
                        let earliest = self.detail.in_pt.map_or(0.0, |a| (a + 0.101).min(duration));
                        self.detail.out_pt = Some(at.max(earliest));
                    }
                    Some(TimelineDrag::Select { anchor }) => {
                        self.detail.in_pt = Some(anchor.min(at));
                        self.detail.out_pt = Some(anchor.max(at));
                    }
                    Some(TimelineDrag::Scrub) => {
                        let due = self.detail.timeline.last_scrub.is_none_or(|last| last.elapsed() >= Duration::from_millis(45));
                        if released || due {
                            c.player.seek(at, released);
                            self.detail.timeline.last_scrub = Some(Instant::now());
                        }
                    }
                    None => {}
                }
                if released {
                    self.detail.timeline.drag = None;
                }
            }
        }
        if hover_handle {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        let view = &mut self.detail.timeline.view;
        if response.double_clicked() {
            view.fit();
        } else {
            if let Some(pos) = response.hover_pos() {
                let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
                if shift || delta.x.abs() > 0.1 {
                    let delta = if delta.x.abs() > 0.1 { delta.x } else { delta.y };
                    view.pan(duration, -f64::from(delta / width));
                } else if delta.y.abs() > 0.1 {
                    let factor = (-f64::from(delta.y) * 0.006).exp();
                    view.zoom(duration, factor, t_of(pos.x), fraction_of(pos.x));
                }
            }
            if response.dragged_by(egui::PointerButton::Secondary) {
                view.pan(duration, -f64::from(response.drag_delta().x / width));
            }
        }
        response.on_hover_text("Scroll to zoom. Shift-scroll or right-drag to pan. Double-click to reset zoom. Shift-drag to select, or drag an In or Out marker.");
    }
}
