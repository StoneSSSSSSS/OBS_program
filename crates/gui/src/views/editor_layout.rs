//! Current-frame geometry for editor regions.

use eframe::egui::{self, Align, Align2, Color32, FontId, Layout, Pos2, Rect, Sense, Stroke, Ui, UiBuilder, Vec2};

pub(super) const TWO_COLUMN_WIDTH: f32 = 840.0;
pub(crate) const SINGLE_TRANSPORT_ROW_WIDTH: f32 = 830.0;
const PREVIEW_HANDLE_HEIGHT: f32 = 24.0;
const MIN_TIMELINE_HEIGHT: f32 = 45.0;

#[derive(Default)]
pub(super) struct PreviewSize {
    extra_height: f32,
    watching: bool,
}

pub(crate) fn remember_interaction_rect(ui: &Ui, name: &'static str, rect: Rect) {
    let ctx = ui.ctx();
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|data| {
        if data.get_temp::<bool>(egui::Id::unique("clipvault-interaction-check")) == Some(true) {
            let clipped = rect.intersect(ui.clip_rect());
            data.insert_temp(egui::Id::unique(("clipvault-interaction-target", name)), clipped);
            data.insert_temp(egui::Id::unique(("clipvault-interaction-pass", name)), pass);
        }
    });
}

impl PreviewSize {
    pub fn watching(&self) -> bool {
        self.watching
    }

    fn timeline_height(&self, default: f32) -> f32 {
        if self.watching {
            MIN_TIMELINE_HEIGHT.min(default)
        } else {
            (default - self.extra_height).max(MIN_TIMELINE_HEIGHT.min(default))
        }
    }

    /// Return true when this interaction enters the larger watching layout.
    pub fn handle(&mut self, ui: &mut Ui, id: &'static str, layout: &EditorLayout) -> bool {
        let Some(rect) = layout.preview_handle else { return false };
        let was_watching = self.watching;
        region(ui, id, rect, |ui| {
            ui.horizontal_centered(|ui| {
                let width = (ui.available_width() - 136.0).max(1.0);
                let (grip, response) = ui.allocate_exact_size(
                    egui::vec2(width, PREVIEW_HANDLE_HEIGHT),
                    Sense::drag(),
                );
                remember_interaction_rect(ui, "preview_drag", grip);
                let response = response
                    .on_hover_cursor(egui::CursorIcon::ResizeVertical)
                    .on_hover_text("Drag vertically to resize preview");
                let color = if response.hovered() || response.dragged() {
                    crate::theme::TEXT
                } else {
                    crate::theme::TEXT_DIM
                };
                let center = grip.center();
                let painter = ui.painter_at(grip);
                for y in [-2.0, 2.0] {
                    painter.line_segment(
                        [center + egui::vec2(-14.0, y), center + egui::vec2(14.0, y)],
                        egui::Stroke::new(1.0, color),
                    );
                }
                if width >= 175.0 {
                    painter.text(
                        center + egui::vec2(22.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        "Drag to resize",
                        egui::FontId::proportional(11.0),
                        color,
                    );
                }
                if response.dragged() {
                    self.watching = false;
                    // Work from the currently visible pane size so a drag can
                    // reverse immediately after hitting a limit.
                    self.extra_height = (layout.default_timeline_height
                        - layout.timeline.height() + response.drag_delta().y)
                        .clamp(
                            -layout.video.height(),
                            (layout.default_timeline_height - MIN_TIMELINE_HEIGHT).max(0.0),
                        );
                }
                let (label, target) = if self.watching {
                    ("Restore layout", "preview_restore")
                } else {
                    ("Larger preview", "preview_larger")
                };
                let toggle = ui.small_button(label);
                remember_interaction_rect(ui, target, toggle.rect);
                if toggle.clicked() {
                    self.watching = !self.watching;
                }
            });
        });
        !was_watching && self.watching
    }
}

#[derive(Debug)]
pub(super) struct EditorLayout {
    pub video: Rect,
    pub inspector: Option<Rect>,
    pub timeline: Rect,
    pub transport: Rect,
    preview_handle: Option<Rect>,
    default_timeline_height: f32,
}

impl EditorLayout {
    pub fn new(
        ui: &mut Ui,
        inspector_width: Option<f32>,
        timeline_height: f32,
        cut_controls: bool,
    ) -> Self {
        Self::with_preview(ui, inspector_width, timeline_height, cut_controls, None)
    }

    pub fn with_preview(
        ui: &mut Ui,
        inspector_width: Option<f32>,
        timeline_height: f32,
        cut_controls: bool,
        preview: Option<&PreviewSize>,
    ) -> Self {
        let rect = ui.available_rect_before_wrap();
        let row_height = ui.spacing().interact_size.y.max(30.0) + ui.spacing().item_spacing.y;
        let _ = ui.allocate_rect(rect, Sense::hover());
        Self::split(
            rect,
            inspector_width,
            timeline_height,
            row_height,
            cut_controls,
            preview,
        )
    }

    fn split(
        rect: Rect,
        inspector_width: Option<f32>,
        timeline_height: f32,
        row_height: f32,
        cut_controls: bool,
        preview: Option<&PreviewSize>,
    ) -> Self {
        let mut player = rect;
        let inspector = inspector_width
            .filter(|_| rect.width() >= TWO_COLUMN_WIDTH)
            .map(|width| {
                let mut region = rect;
                region.min.x = (rect.right() - width).max(rect.left());
                player.max.x = region.left();
                region
            });
        let transport_rows = if player.width() < 420.0 {
            3.0
        } else if player.width() < SINGLE_TRANSPORT_ROW_WIDTH {
            2.0
        } else {
            1.0
        };
        let cut_rows = if cut_controls {
            if player.width() < 670.0 {
                2.0
            } else {
                1.0
            }
        } else {
            0.0
        };
        let height = player.height().max(0.0);
        let transport_height = ((transport_rows + cut_rows) * row_height).min(height * 0.52);
        let video_minimum = (height * 0.21).min(110.0);
        let handle_height = if preview.is_some() { PREVIEW_HANDLE_HEIGHT.min(height * 0.1) } else { 0.0 };
        let default_timeline_height = timeline_height;
        let desired = preview.map_or(timeline_height, |size| size.timeline_height(timeline_height));
        let timeline_height = desired.min((height - transport_height - video_minimum - handle_height).max(0.0));
        let mut transport = player;
        transport.min.y = player.bottom() - transport_height;
        let mut timeline = player;
        timeline.max.y = transport.top();
        timeline.min.y = timeline.bottom() - timeline_height;
        let mut video = player;
        video.max.y = timeline.top() - handle_height;
        let preview_handle = preview.map(|_| Rect::from_min_max(
            egui::pos2(video.left(), video.bottom()), egui::pos2(video.right(), timeline.top()),
        ));
        Self {
            video,
            inspector,
            timeline,
            transport,
            preview_handle,
            default_timeline_height,
        }
    }
}

/// Watching mode uses the full width for a single scrub strip.
pub(super) struct WatchingRuler {
    pub axis: Rect,
    pub track: Rect,
}

impl WatchingRuler {
    pub fn new(row: Rect) -> Self {
        let padding = (row.width() * 0.08).min(8.0);
        let axis = row.shrink2(Vec2::new(padding, 0.0));
        let center_y = row.top() + row.height() * if row.height() >= 26.0 { 0.70 } else { 0.5 };
        let track = Rect::from_center_size(
            egui::pos2(axis.center().x, center_y),
            egui::vec2(axis.width(), row.height().min(16.0) * 0.5),
        );
        Self { axis, track }
    }

    pub fn grip(&self, x: f32) -> Option<Pos2> {
        self.axis.x_range().contains(x).then_some(egui::pos2(x, self.track.center().y))
    }

    pub fn paint(
        &self, ui: &Ui, row: Rect, labels: [&str; 2], marks: [Option<f32>; 2],
        selection: Option<(f32, f32)>, playhead: Option<f32>, accent: Color32,
        recorded: impl IntoIterator<Item = (f32, f32)>,
    ) {
        use crate::theme;
        let painter = ui.painter_at(row);
        painter.rect(row, 5.0, theme::SIDEBAR, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
        if row.height() >= 26.0 && self.axis.width() >= 130.0 {
            let font = FontId::monospace(10.0);
            for (label, x, align) in [
                (labels[0], self.axis.left(), Align2::LEFT_TOP),
                (labels[1], self.axis.right(), Align2::RIGHT_TOP),
            ] {
                let text = painter.layout_no_wrap(label.to_owned(), font.clone(), theme::TEXT_DIM);
                if text.size().x <= self.axis.width() * 0.5 - 4.0 {
                    painter.galley(align.anchor_size(egui::pos2(x, row.top() + 3.0), text.size()).min,
                        text, theme::TEXT_DIM);
                }
            }
        }
        painter.rect_filled(self.track, 3.0, theme::SURFACE3);
        for (a, b) in recorded {
            let range = Rect::from_x_y_ranges(a..=b, self.track.y_range()).intersect(self.track);
            if range.is_positive() {
                painter.rect_filled(range, 2.0, theme::BORDER_STRONG);
            }
        }
        if let Some((a, b)) = selection {
            let range = Rect::from_x_y_ranges(a..=b, self.track.y_range()).intersect(self.track);
            if range.is_positive() {
                painter.rect_filled(range, 2.0, accent.gamma_multiply(0.40));
            }
        }
        let grip_radius = (row.height() * 0.14).min(5.0).min(self.axis.left() - row.left());
        for x in marks.into_iter().flatten() {
            if let Some(grip) = self.grip(x) {
                painter.rect_filled(
                    Rect::from_center_size(grip, egui::vec2(grip_radius * 1.4, grip_radius * 2.6)),
                    2.0, accent,
                );
            }
        }
        if let Some(grip) = playhead.and_then(|x| self.grip(x)) {
            let cursor = grip_radius * 0.84;
            painter.add(egui::Shape::convex_polygon(vec![
                grip - egui::vec2(0.0, cursor), grip + egui::vec2(cursor, 0.0),
                grip + egui::vec2(0.0, cursor), grip - egui::vec2(cursor, 0.0),
            ], Color32::WHITE, Stroke::new(1.0, theme::SIDEBAR)));
        }
    }
}

/// Draw into a fixed current-frame rectangle. The child can grow internally,
/// for example for scrolling, while shapes and hit regions stay in its slot.
pub(super) fn region(ui: &mut Ui, id: &'static str, rect: Rect, render: impl FnOnce(&mut Ui)) {
    if rect.is_positive() {
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(id)
                .max_rect(rect)
                .layout(Layout::top_down(Align::Min)),
        );
        child.shrink_clip_rect(rect);
        render(&mut child);
    }
}

pub(super) fn controls(ui: &mut Ui, id: &'static str, render: impl FnOnce(&mut Ui)) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .min_scrolled_height(0.0)
        .auto_shrink([false, false])
        .show(ui, render);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, vec2};

    #[test]
    fn live_resize_retains_widget_state_without_crossing_editor_regions() {
        use crate::lanes::{lanes, waveform_id, LaneState};
        use crate::player::{seek_bar, Player};
        use cv_core::config::Config;
        use eframe::egui::{Color32, RawInput};

        let sizes = [
            (1510.0, 760.0),
            (1310.0, 701.0),
            (1182.0, 771.0),
            (1130.0, 671.0),
            (1094.0, 671.0),
            (1062.0, 671.0),
            (1035.0, 640.0),
            (876.0, 550.0),
            (642.0, 390.0),
        ];
        for recording in [false, true] {
            let ctx = egui::Context::default();
            let cfg = Config::default();
            let mut player = Player::new(ctx.clone(), &cfg);
            player.lane_ids = vec![
                "full".into(),
                "mic".into(),
                "cat:game".into(),
                "app:fixture.exe".into(),
            ];
            player.track_names = vec![
                "Full mix".into(),
                "Microphone".into(),
                "Game".into(),
                "A very long name 隊友のアプリケーション with spaces".into(),
            ];
            let mut lanes_state = LaneState::default();
            lanes_state.sel = Some(("full".into(), 1.0, 2.0));
            let (mut in_pt, mut out_pt) = (Some(1.25), Some(4.5));
            let mut elapsed = 0.0;
            for (width, height) in sizes.into_iter().chain(sizes.into_iter().rev()) {
                let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, height));
                // The containing page has a navigation rail and top title. Use
                // the reported physical window sizes, with that space removed.
                let body = Rect::from_min_max(pos2(216.0, 52.0), pos2(width - 18.0, height - 18.0));
                let mut layout: Option<EditorLayout> = None;
                let mut wave_on_pass = None;
                let mut play_on_pass = None;
                // Retain the context so the real scroll areas preserve their
                // offsets between frames and when the window changes size.
                const SCROLL_FRAMES: usize = 24;
                for pass in 0..SCROLL_FRAMES {
                    elapsed += 1.0 / 60.0;
                    let events = if let Some(editor) = &layout {
                        vec![
                            egui::Event::PointerMoved(editor.timeline.left_top() + vec2(6.0, 6.0)),
                            egui::Event::MouseWheel {
                                unit: egui::MouseWheelUnit::Point,
                                source: egui::MouseWheelSource::Trackpad,
                                delta: vec2(0.0, -25.0),
                                modifiers: egui::Modifiers::NONE,
                                phase: if pass == 1 { egui::TouchPhase::Start } else { egui::TouchPhase::Move },
                            },
                        ]
                    } else {
                        Vec::new()
                    };
                    let mut output = ctx.run_ui(RawInput { screen_rect: Some(viewport), time: Some(elapsed), events, ..Default::default() }, |ui| {
                        region(ui, "test_editor_body", body, |ui| {
                            let mut editor = EditorLayout::new(
                                ui, Some((body.width() * 0.31).clamp(265.0, 340.0)),
                                if recording { 398.0 } else { 330.0 }, recording,
                            );
                            if let Some(inspector) = editor.inspector {
                                region(ui, "test_editor_inspector", inspector, |ui| {
                                    controls(ui, "test_editor_inspector_scroll", |ui| {
                                        ui.heading("Trim & export");
                                        ui.add(egui::Label::new("Long diagnostic text and metadata should wrap inside the inspector.").wrap());
                                        let done = ui.button("Done");
                                        assert!(inspector.contains_rect(done.interact_rect), "inspector command escaped at {width}×{height}");
                                    });
                                });
                            }
                            region(ui, "test_editor_transport", editor.transport, |ui| {
                                controls(ui, "test_editor_toolbar_scroll", |ui| {
                                    let (play, _) = player.transport(ui, "Thu 15 Jul 23:59:32 · 23h 31m ago", &cfg.player);
                                    if pass == 0 {
                                        play.scroll_to_me_animation(None, egui::style::ScrollAnimation::none());
                                    }
                                    play_on_pass = Some(play.interact_rect);
                                    if recording {
                                        ui.horizontal_wrapped(|ui| {
                                            for item in ["Start at playhead", "End at playhead", "Preview", "Export to size limit"] {
                                                let _ = ui.button(item);
                                            }
                                        });
                                    }
                                });
                            });
                            region(ui, "test_editor_timeline", editor.timeline, |ui| {
                                controls(ui, "test_editor_timeline_scroll", |ui| {
                                    let available = if recording {
                                        // Recording ruler and bands occupy 96 points before the instructions.
                                        ui.add_space(102.0);
                                        let help = crate::views::recordings::timeline_help(ui);
                                        if width == 1182.0 && pass == 0 {
                                            assert!(
                                                ui.clip_rect().contains_rect(help.rect),
                                                "recording help is clipped at the reported 1182×771 size"
                                            );
                                        }
                                        editor.timeline.height() - 106.0
                                    } else {
                                        seek_bar(ui, &mut player, &mut in_pt, &mut out_pt, Color32::LIGHT_BLUE);
                                        editor.timeline.height() - 44.0
                                    };
                                    lanes(ui, &mut player, 0.0, 10.0, recording, &[], &[], Some(2.0),
                                        &mut lanes_state, Color32::LIGHT_BLUE, false, None,
                                        (0.0, 10.0), (0.0, 10.0), available);
                                    wave_on_pass = ui.ctx().read_response(waveform_id("full"));
                                });
                            });
                            region(ui, "test_editor_video", editor.video, |ui| {
                                let response = player.video(ui, editor.video.size());
                                assert!(editor.video.contains_rect(response.interact_rect), "video escaped at {width}×{height}");
                            });
                            // Compact mode intentionally hides the inspector.
                            if body.width() < TWO_COLUMN_WIDTH {
                                assert!(editor.inspector.take().is_none());
                            }
                            layout = Some(editor);
                        });
                    });
                    let editor = layout.as_ref().unwrap();
                    assert_eq!(editor.inspector.is_some(), body.width() >= TWO_COLUMN_WIDTH);
                    let wave_is_reachable = wave_on_pass.as_ref().is_some_and(|wave| {
                        wave.interact_rect.height() >= 12.0 && wave.interact_rect.width() >= 24.0
                    });
                    if wave_is_reachable || pass + 1 == SCROLL_FRAMES {
                        let wave = wave_on_pass.take().expect("the full-mix waveform must render");
                        assert!(wave.interact_rect.height() >= 12.0 && wave.interact_rect.width() >= 24.0,
                            "waveform cannot be reached at {width}×{height} (recording={recording}): {:?}",
                            wave.interact_rect);
                        assert!(editor.timeline.expand(1.0).contains_rect(wave.interact_rect));
                        let play = play_on_pass.take().expect("player must render Play");
                        assert!(play.height() >= 12.0 && editor.transport.expand(1.0).contains_rect(play),
                            "Play cannot be reached at {width}×{height} (recording={recording}): {play:?}");
                    }
                    for hit in ctx.interactive_rects_last_pass() {
                        if body.intersects(hit) {
                            let constrained = [
                                Some(editor.video),
                                Some(editor.timeline),
                                Some(editor.transport),
                                editor.inspector,
                            ]
                            .into_iter()
                            .flatten()
                            .any(|section| section.expand(1.0).contains_rect(hit));
                            assert!(constrained, "interactive control crossed an editor boundary at {width}×{height}: {hit:?}");
                        }
                    }
                    assert!(lanes_state.sel.is_some(), "resize discarded lane selection");
                    output.textures_delta.clear();
                    if wave_is_reachable {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn watching_ruler_keeps_both_trim_grips_and_time_labels_inside_one_row() {
        use eframe::egui::{epaint::Shape, RawInput};

        let check = |shape: &Shape, row: Rect, labels: &mut Vec<String>| {
            let bounds = shape.visual_bounding_rect();
            assert!(!bounds.is_positive() || row.expand(0.5).contains_rect(bounds), "{shape:?}");
            if let Shape::Text(text) = shape {
                labels.push(text.galley.text().to_owned());
            }
        };
        for (width, height) in [(650.0, 40.0), (225.0, 36.0), (225.0, 25.0), (100.0, 40.0)] {
            for names in [["23:59:30", "00:00:15"], ["00:00.000", "00:40.000"]] {
                let context = egui::Context::default();
                let mut row = Rect::NOTHING;
                let mut output = context.run_ui(RawInput::default(), |ui| {
                    (row, _) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
                    let ruler = WatchingRuler::new(row);
                    let marks = [Some(ruler.axis.left()), Some(ruler.axis.right())];
                    assert!(marks.into_iter().flatten().all(|x| {
                        ruler.grip(x).is_some_and(|point| row.contains(point)
                            && ruler.track.contains(point))
                    }));
                    ruler.paint(ui, row, names, marks,
                        Some((ruler.axis.left(), ruler.axis.right())),
                        Some(ruler.axis.center().x), Color32::LIGHT_BLUE,
                        [(ruler.axis.left() - 8.0, ruler.axis.center().x)]);
                });
                output.textures_delta.clear();
                let mut labels = Vec::new();
                for painted in output.shapes {
                    check(&painted.shape, row, &mut labels);
                }
                let expected: &[&str] = if width >= 225.0 && height >= 26.0 { &names } else { &[] };
                assert_eq!(labels.iter().map(String::as_str).collect::<Vec<_>>(), expected);
            }
        }
    }

    #[test]
    fn expanded_preview_leaves_the_timeline_and_transport_reachable() {
        let preview = PreviewSize { watching: true, ..Default::default() };
        for size in [vec2(1100.0, 600.0), vec2(405.0, 340.0)] {
            let rect = Rect::from_min_size(pos2(0.0, 0.0), size);
            let normal = EditorLayout::split(rect, None, 398.0, 36.0, true, Some(&PreviewSize::default()));
            let watching = EditorLayout::split(rect, None, 398.0, 36.0, true, Some(&preview));
            let handle = watching.preview_handle.unwrap();
            assert!(watching.video.height() > normal.video.height());
            assert!(watching.timeline.height() >= MIN_TIMELINE_HEIGHT);
            assert_eq!(watching.transport, normal.transport);
            assert_eq!(handle.top(), watching.video.bottom());
            assert_eq!(handle.bottom(), watching.timeline.top());
        }
    }

    #[test]
    fn editor_regions_never_overlap_during_resize_in_either_direction() {
        let widths = [
            320.0, 419.0, 420.0, 669.0, 670.0, 769.0, 770.0, 839.0, 840.0, 841.0, 1100.0, 1500.0,
        ];
        for width in widths.into_iter().chain(widths.into_iter().rev()) {
            for height in [180.0, 330.0, 620.0, 980.0] {
                let rect = Rect::from_min_size(pos2(18.0, 48.0), vec2(width, height));
                for cut_controls in [false, true] {
                    let editor = EditorLayout::split(rect, Some(300.0), 320.0, 36.0, cut_controls, None);
                    assert_eq!(editor.inspector.is_some(), width >= TWO_COLUMN_WIDTH);
                    assert_eq!(editor.video.bottom(), editor.timeline.top());
                    assert_eq!(editor.timeline.bottom(), editor.transport.top());
                    assert_eq!(editor.transport.bottom(), rect.bottom());
                    assert!(editor.video.is_positive() && editor.transport.is_positive());
                    for section in [editor.video, editor.timeline, editor.transport] {
                        assert!(rect.contains_rect(section));
                        if let Some(inspector) = editor.inspector {
                            assert_eq!(section.right(), inspector.left());
                        }
                    }
                }
            }
        }
    }
}
