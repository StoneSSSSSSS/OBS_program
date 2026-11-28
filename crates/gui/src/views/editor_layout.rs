//! Current-frame geometry for editor regions.

use eframe::egui::{self, Align, Layout, Rect, Sense, Ui, UiBuilder};

pub(super) const TWO_COLUMN_WIDTH: f32 = 840.0;
pub(crate) const SINGLE_TRANSPORT_ROW_WIDTH: f32 = 770.0;

#[derive(Debug)]
pub(super) struct EditorLayout {
    pub video: Rect,
    pub inspector: Option<Rect>,
    pub timeline: Rect,
    pub transport: Rect,
}

impl EditorLayout {
    pub fn new(
        ui: &mut Ui,
        inspector_width: Option<f32>,
        timeline_height: f32,
        cut_controls: bool,
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
        )
    }

    fn split(
        rect: Rect,
        inspector_width: Option<f32>,
        timeline_height: f32,
        row_height: f32,
        cut_controls: bool,
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
        let timeline_height =
            timeline_height.min((height - transport_height - video_minimum).max(0.0));
        let mut transport = player;
        transport.min.y = player.bottom() - transport_height;
        let mut timeline = player;
        timeline.max.y = transport.top();
        timeline.min.y = timeline.bottom() - timeline_height;
        let mut video = player;
        video.max.y = timeline.top();
        Self {
            video,
            inspector,
            timeline,
            transport,
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
                                    let play = player.transport(ui, "Thu 15 Jul 23:59:32 · 23h 31m ago", &cfg.player);
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
    fn editor_regions_never_overlap_during_resize_in_either_direction() {
        let widths = [
            320.0, 419.0, 420.0, 669.0, 670.0, 769.0, 770.0, 839.0, 840.0, 841.0, 1100.0, 1500.0,
        ];
        for width in widths.into_iter().chain(widths.into_iter().rev()) {
            for height in [180.0, 330.0, 620.0, 980.0] {
                let rect = Rect::from_min_size(pos2(18.0, 48.0), vec2(width, height));
                for cut_controls in [false, true] {
                    let editor = EditorLayout::split(rect, Some(300.0), 320.0, 36.0, cut_controls);
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
