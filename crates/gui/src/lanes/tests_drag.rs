use super::*;
use eframe::egui::{Event, Modifiers, PointerButton, RawInput};

const CHAT: &str = "app:fixture-chat.exe";
const WINDOW: (f64, f64) = (20.0, 60.0);

struct DragHarness {
    ctx: egui::Context,
    player: Player,
    state: LaneState,
    time: f64,
    wave: Waveform,
    tracks: LaneTracks,
}

impl DragHarness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let mut player = Player::new(ctx.clone(), &cv_core::config::Config::default());
        player.lane_ids = ["full", "mic", "cat:game", "cat:voice", CHAT, "cat:media", "cat:other"]
            .into_iter().map(str::to_owned).collect();
        player.track_names = ["Full mix", "Microphone", "Game", "Voice chat", "Fixture Chat",
            "Music & media", "Desktop"].into_iter().map(str::to_owned).collect();
        Self {
            ctx,
            player,
            state: LaneState::default(),
            time: 0.0,
            wave: Waveform { tracks: 1, rate: 10, peaks: vec![150; 400] },
            tracks: BTreeMap::from([(CHAT.to_owned(), vec![(0, 0.0, 40.0)])]),
        }
    }

    fn frame(&mut self, events: Vec<Event>) -> Vec<LaneAction> {
        self.time += 1.0 / 60.0;
        let mut result = Vec::new();
        let mut output = self.ctx.run_ui(RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(830.0, 440.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        }, |ui| {
            // Place the child at the bottom edge of the real scroll clip.
            ui.spacing_mut().item_spacing.y = 9.0;
            let source = WaveSource {
                start: WINDOW.0,
                end: WINDOW.1,
                wave: &self.wave,
                tracks: Some(&self.tracks),
            };
            result = lanes(ui, &mut self.player, WINDOW.0, WINDOW.1, false, &[source],
                &[], None, &mut self.state, Color32::LIGHT_BLUE, false, None, WINDOW, WINDOW, 300.0);
        });
        output.textures_delta.clear();
        result
    }

    fn chat(&self) -> egui::Response {
        self.ctx.read_response(waveform_id(CHAT)).expect("chat waveform widget")
    }

    fn select_at(&self, first: f32, last: f32) -> (Pos2, Pos2) {
        let row = self.chat();
        assert!(row.interact_rect.is_positive());
        assert!(row.interact_rect.height() < row.rect.height(), "the chat waveform must be clipped: {row:?}");
        let point = |fraction: f32| Pos2::new(
            row.rect.left() + row.rect.width() * fraction,
            row.interact_rect.center().y,
        );
        (point(first), point(last))
    }

    fn assert_selection(&self, start: f64, end: f64) {
        let (lane, actual_start, actual_end) = self.state.sel.as_ref().expect("selected audio region");
        assert_eq!(lane, CHAT);
        assert!((actual_start - start).abs() < 0.12, "{actual_start} != {start}");
        assert!((actual_end - end).abs() < 0.12, "{actual_end} != {end}");
    }
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

#[test]
fn one_pointer_move_selects_a_region_on_a_clipped_child_waveform() {
    let mut harness = DragHarness::new();
    harness.frame(vec![]);
    let (first, last) = harness.select_at(0.22, 0.64);

    harness.frame(vec![Event::PointerMoved(first), button(first, true)]);
    harness.frame(vec![Event::PointerMoved(last)]);
    let actions = harness.frame(vec![button(last, false)]);

    harness.assert_selection(28.8, 45.6);
    assert!(actions.is_empty(), "a drag must queue no seek or edit until Apply");
}

#[test]
fn dragging_reverse_outside_the_clipped_row_keeps_the_press_anchor() {
    let mut harness = DragHarness::new();
    harness.frame(vec![]);
    let (first, last) = harness.select_at(0.84, 0.13);

    harness.frame(vec![Event::PointerMoved(first), button(first, true)]);
    harness.frame(vec![Event::PointerMoved(Pos2::new(first.x - 22.0, first.y))]);
    let outside = Pos2::new(last.x, last.y + 50.0);
    harness.frame(vec![Event::PointerMoved(outside)]);
    let actions = harness.frame(vec![button(outside, false)]);

    harness.assert_selection(25.2, 53.6);
    assert!(actions.is_empty(), "dragging outside the lane must not seek");
}

#[test]
fn one_move_and_release_in_one_frame_still_selects_the_range() {
    let mut harness = DragHarness::new();
    harness.frame(vec![]);
    let (first, last) = harness.select_at(0.30, 0.70);

    harness.frame(vec![Event::PointerMoved(first), button(first, true)]);
    let actions = harness.frame(vec![Event::PointerMoved(last), button(last, false)]);

    harness.assert_selection(32.0, 48.0);
    assert!(actions.is_empty(), "a fast drag must not become a click-to-seek");
}

#[test]
fn click_without_a_drag_only_seeks() {
    let mut harness = DragHarness::new();
    harness.frame(vec![]);
    let (point, _) = harness.select_at(0.42, 0.42);

    harness.frame(vec![Event::PointerMoved(point), button(point, true)]);
    let actions = harness.frame(vec![button(point, false)]);

    assert!(harness.state.sel.is_none());
    assert!(matches!(actions.as_slice(), [LaneAction::Seek(pos)] if (pos - 36.8).abs() < 0.12));
}
