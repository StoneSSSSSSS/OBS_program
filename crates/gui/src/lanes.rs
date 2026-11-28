//! Audio lanes: one row per audio track under the timeline, showing the
//! waveform, a mute button and a volume slider. Drag across a lane to select
//! part of it, then mute it or make it louder. Edits are non-destructive: they
//! are stored as metadata and applied during playback/export only.

use crate::player::Player;
use cv_core::mixer::{MixMode, MixSettings, LANE_FULL};
use crate::theme::{self, icon};
use cv_core::model::AudioEdit;
use cv_core::waveform::{wave_path, Waveform};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
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
    pub tracks: Option<&'a LaneTracks>,
}

pub type LaneTracks = BTreeMap<String, Vec<(usize, f64, f64)>>;

/// Keep each waveform track tied to the app occupying it at that file time.
pub fn waveform_tracks(
    info: &cv_media::probe::MediaInfo,
    duration: f64,
    overrides: &BTreeMap<String, String>,
) -> LaneTracks {
    use cv_core::mixer::TrackSource;
    use cv_media::probe::{slot_intervals, track_sources, TrackRole};

    let mut result = LaneTracks::new();
    let mut add = |track: usize, from: f64, to: f64, source: &TrackSource| {
        let category = match source {
            TrackSource::App { key, .. } => overrides.get(&key.to_lowercase()).map(String::as_str),
            _ => None,
        };
        for lane in source.lanes(category) {
            result.entry(lane).or_default().push((track, from, to));
        }
    };
    for (track, (info_track, source)) in info.audio.iter().zip(track_sources(info)).enumerate() {
        if let TrackRole::Slot { slot } = &info_track.role {
            for (from, to, app) in slot_intervals(info, *slot, duration) {
                add(track, from, to, &TrackSource::App {
                    key: app.key,
                    name: app.name,
                    category: app.category,
                });
            }
        } else {
            add(track, 0.0, duration, &source);
        }
    }
    result
}

/// The player publishes seek and load results asynchronously. Wait for a
/// playing snapshot inside the new selection before using its end boundary.
pub struct RangePreview {
    start: f64,
    pub end: f64,
    requested: Instant,
    armed: bool,
}

pub enum PreviewProgress {
    Playing,
    Finished,
    EndOfFile,
}

impl RangePreview {
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end, requested: Instant::now(), armed: false }
    }

    pub fn progress(&mut self, at: Option<f64>, paused: bool, eof: bool) -> PreviewProgress {
        let settled = self.requested.elapsed() >= Duration::from_secs(2);
        if !self.armed {
            self.armed = at.is_some_and(|at| !paused && at >= self.start - 0.1 && at < self.end);
            if !self.armed {
                return if settled { PreviewProgress::Finished } else { PreviewProgress::Playing };
            }
        }

        if at.is_some_and(|at| at >= self.end) {
            PreviewProgress::Finished
        } else if eof {
            PreviewProgress::EndOfFile
        } else {
            PreviewProgress::Playing
        }
    }
}

pub struct AudioSnapshot {
    pub edits: Vec<AudioEdit>,
    pub mix: MixSettings,
}

#[derive(Default)]
pub struct EditHistory(Vec<AudioSnapshot>);

impl EditHistory {
    pub fn save(&mut self, edits: &[AudioEdit], mix: MixSettings) {
        if self.0.len() == 30 {
            self.0.remove(0);
        }
        self.0.push(AudioSnapshot { edits: edits.to_vec(), mix });
    }

    pub fn undo(&mut self) -> Option<AudioSnapshot> {
        self.0.pop()
    }

    pub fn can_undo(&self) -> bool {
        !self.0.is_empty()
    }
}

#[derive(Default)]
struct GainGesture {
    lane: Option<String>,
}

impl GainGesture {
    /// True for a keyboard/click change or the first update of a pointer drag.
    fn new_change(&mut self, lane: &str, dragging: bool) -> bool {
        if !dragging {
            self.lane = None;
            return true;
        }
        if self.lane.as_deref() == Some(lane) {
            return false;
        }
        self.lane = Some(lane.to_owned());
        true
    }

    fn release(&mut self) {
        self.lane = None;
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct LaneDisplay {
    hidden: BTreeSet<String>,
    names: BTreeMap<String, String>,
}

impl LaneDisplay {
    fn path(clip_id: i64) -> std::path::PathBuf {
        cv_core::paths::data_dir().join("clip_lane_display").join(format!("{clip_id}.json"))
    }

    pub fn load(clip_id: i64) -> Self {
        std::fs::read(Self::path(clip_id)).ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, clip_id: i64) -> anyhow::Result<()> {
        let path = Self::path(clip_id);
        cv_core::util::write_atomic(&path, &serde_json::to_vec(self)?)?;
        Ok(())
    }
}

/// Caches waveform files by media path, reloading when the service updates them.
#[derive(Default)]
pub struct Waves {
    map: HashMap<String, (Option<Arc<Waveform>>, Option<SystemTime>, Instant)>,
}

impl Waves {
    pub fn get(&mut self, media: &str) -> Option<Arc<Waveform>> {
        let now = Instant::now();
        let stale = self.map.get(media).map(|(_, _, t)| now.duration_since(*t) > Duration::from_secs(5)).unwrap_or(true);
        if stale {
            let wp = wave_path(Path::new(media));
            let mtime = std::fs::metadata(&wp).and_then(|m| m.modified()).ok();
            let entry = self.map.entry(media.to_string()).or_insert((None, None, now));
            if entry.1 != mtime || (entry.0.is_none() && mtime.is_some()) {
                entry.0 = mtime.and_then(|_| Waveform::load(&wp).ok()).map(Arc::new);
                entry.1 = mtime;
            }
            entry.2 = now;
        }
        self.map.get(media).and_then(|(w, _, _)| w.clone())
    }
}

#[derive(Default)]
pub struct LaneState {
    /// Selected range: (track, start, end) in axis units.
    pub sel: Option<(String, f64, f64)>,
    /// Capture the lane and press position even when Windows delivers the move and release together.
    wave_press: Option<(String, f32)>,
    pub all_tracks: bool,
    pub custom_pct: f32,
    rename_draft: Option<(String, String)>,
    gain_gesture: GainGesture,
    edit_dialog: Option<EditRange>,
    reveal_app: Option<String>,
}

fn waveform_id(lane: &str) -> egui::Id {
    egui::Id::unique(("audio_lane_waveform", lane))
}

struct EditRange {
    lane: String,
    start: f64,
    end: f64,
    percent: f32,
    action: RangeAction,
}

#[derive(Clone, Copy, PartialEq)]
enum RangeAction {
    Mute,
    Gain,
    Reset,
}

pub enum LaneAction {
    Seek(f64),
    /// Apply `gain` to `[a, b)` on these tracks (1.0 = remove edits there).
    Edit { lanes: Vec<String>, a: f64, b: f64, gain: f32 },
    /// Previous mix is present for the first update of the gesture.
    MixChanged { previous: Option<MixSettings> },
    Undo,
    DisplayChanged,
}

/// Single-mode edits switch the monitor so the result can be heard immediately.
pub fn mode_for_edit(lanes: &[String], gain: f32) -> Option<MixMode> {
    if gain == 1.0 || lanes.is_empty() {
        return None;
    }
    let full = lanes.iter().any(|lane| lane == LANE_FULL);
    let separate = lanes.iter().any(|lane| lane != LANE_FULL);
    match (full, separate) {
        (true, false) => Some(MixMode::Full),
        (false, true) => Some(MixMode::Separate),
        _ => None,
    }
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
    can_undo: bool,
    mut display: Option<&mut LaneDisplay>,
    edit_bounds: (f64, f64),
    default_range: (f64, f64),
    height_budget: f32,
) -> Vec<LaneAction> {
    let top = ui.cursor().top();
    let mut actions = Vec::new();
    let lane_ids = player.lane_ids.clone();
    let n = lane_ids.len();
    let has_full = lane_ids.iter().any(|lane| lane == LANE_FULL);
    if st.custom_pct == 0.0 {
        st.custom_pct = 150.0;
    }

    // Header: mode switch and help.
    ui.horizontal_wrapped(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("Audio tracks").font(FontId::new(14.0, theme::semibold())).color(theme::TEXT));
        if has_full && n > 1 {
            ui.add_space(8.0);
            let full = player.mix.mode == MixMode::Full;
            if ui.add(egui::Button::selectable(full, "Full mix")).on_hover_text("Play the captured full mix").clicked() && !full {
                let previous = player.mix.clone();
                player.set_mode(MixMode::Full);
                actions.push(LaneAction::MixChanged { previous: Some(previous) });
            }
            if ui.add(egui::Button::selectable(!full, "Separate tracks")).on_hover_text("Balance categories and individual apps").clicked() && full {
                let previous = player.mix.clone();
                player.set_mode(MixMode::Separate);
                actions.push(LaneAction::MixChanged { previous: Some(previous) });
            }
        }
        let undo_clicked = ui.add_enabled(can_undo, egui::Button::new("Undo audio change")).clicked();
        let undo_shortcut = can_undo && !ui.ctx().egui_wants_keyboard_input()
            && ui.input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z));
        if undo_clicked || undo_shortcut {
            st.sel = None;
            actions.push(LaneAction::Undo);
        }
        if let Some(display) = display.as_deref_mut() {
            ui.menu_button("Track names & visibility", |ui| {
                if !display.hidden.is_empty() && ui.button("Show all lanes").clicked() {
                    display.hidden.clear();
                    actions.push(LaneAction::DisplayChanged);
                }
                egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                    for (i, lane) in lane_ids.iter().enumerate() {
                        let label = player.track_names.get(i).map(String::as_str).unwrap_or(lane);
                        let mut visible = !display.hidden.contains(lane);
                        ui.push_id(lane, |ui| ui.horizontal(|ui| {
                            if ui.checkbox(&mut visible, display.names.get(lane).map(String::as_str).unwrap_or(label)).changed() {
                                if visible {
                                    display.hidden.remove(lane);
                                } else {
                                    display.hidden.insert(lane.clone());
                                    if st.sel.as_ref().is_some_and(|(selected, _, _)| selected == lane) {
                                        st.sel = None;
                                    }
                                }
                                actions.push(LaneAction::DisplayChanged);
                            }
                            if ui.small_button("Rename").clicked() {
                                st.rename_draft = Some((lane.clone(), display.names.get(lane).cloned().unwrap_or_else(|| label.to_owned())));
                            }
                        }));
                    }
                });
            });
        }
    });

    if let (Some(display), Some((lane, name))) = (display.as_deref_mut(), &mut st.rename_draft) {
        let mut close = false;
        egui::Modal::new(ui.scope_id().with("rename_lane")).show(ui.ctx(), |ui| {
            ui.label(theme::title("Rename audio lane"));
            let input = ui.text_edit_singleline(name);
            let submit = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() || submit {
                    let name = name.trim();
                    if name.is_empty() {
                        display.names.remove(lane);
                    } else {
                        display.names.insert(lane.clone(), name.chars().take(64).collect());
                    }
                    actions.push(LaneAction::DisplayChanged);
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            st.rename_draft = None;
        }
    }
    let label_for = |i: usize, lane: &str| -> String {
        display.as_ref().and_then(|d| d.names.get(lane)).cloned()
            .or_else(|| player.track_names.get(i).cloned()).unwrap_or_else(|| format!("Track {}", i + 1))
    };
    let labels: Vec<String> = lane_ids.iter().enumerate().map(|(i, lane)| label_for(i, lane)).collect();
    let visible_lanes: Vec<&String> = lane_ids.iter()
        .filter(|lane| !display.as_ref().is_some_and(|d| d.hidden.contains(*lane))).collect();
    if st.sel.as_ref().is_some_and(|(id, _, _)| !visible_lanes.iter().any(|lane| *lane == id)) {
        st.sel = None;
    }
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!visible_lanes.is_empty(), egui::Button::new("Edit audio range")).clicked() {
            let (lane, start, end) = match &st.sel {
                Some((lane, start, end)) => (lane.clone(), *start, *end),
                None => {
                    let lane = visible_lanes.iter()
                        .find(|id| (**id == LANE_FULL) == (player.mix.mode == MixMode::Full))
                        .or_else(|| visible_lanes.first()).map(|id| (*id).clone()).unwrap_or_default();
                    (lane, default_range.0, default_range.1)
                }
            };
            st.edit_dialog = Some(EditRange { lane, start, end, percent: 100.0, action: RangeAction::Mute });
        }
        if !visible_lanes.is_empty() {
            ui.label(theme::small("Drag across a waveform to select a range."));
        }
    });
    if let Some(dialog) = &mut st.edit_dialog {
        let mut close = false;
        egui::Modal::new(ui.scope_id().with("edit_audio_range")).show(ui.ctx(), |ui| {
            ui.set_min_width(300.0);
            ui.label(theme::title("Edit audio range"));
            let name = lane_ids.iter().position(|id| id == &dialog.lane)
                .and_then(|i| labels.get(i)).map(String::as_str).unwrap_or("Choose lane");
            egui::ComboBox::from_id_salt("range_lane").selected_text(name).show_ui(ui, |ui| {
                for (i, id) in lane_ids.iter().enumerate().filter(|(_, id)| visible_lanes.contains(id)) {
                    ui.selectable_value(&mut dialog.lane, id.clone(), &labels[i]);
                }
            });
            for (label, value) in [("Start", &mut dialog.start), ("End", &mut dialog.end)] {
                ui.horizontal(|ui| {
                    ui.label(label);
                    let input = egui::DragValue::new(value).range(edit_bounds.0..=edit_bounds.1).speed(0.1);
                    if wall {
                        ui.add(input.custom_formatter(|s, _| {
                            chrono::DateTime::from_timestamp_millis((s * 1000.0) as i64)
                                .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                                .unwrap_or_default()
                        }).custom_parser(|value| {
                            use chrono::TimeZone;
                            let time = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f").ok()?;
                            chrono::Local.from_local_datetime(&time).earliest().map(|t| t.timestamp_millis() as f64 / 1000.0)
                        }));
                    } else {
                        ui.add(input.suffix(" s").max_decimals(3));
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut dialog.action, RangeAction::Mute, "Mute");
                ui.selectable_value(&mut dialog.action, RangeAction::Gain, "Adjust gain");
                ui.selectable_value(&mut dialog.action, RangeAction::Reset, "Clear edit");
            });
            if dialog.action == RangeAction::Gain {
                ui.add(egui::DragValue::new(&mut dialog.percent).range(0.0..=400.0).suffix("%"));
            }
            let a = dialog.start.clamp(edit_bounds.0, edit_bounds.1);
            let b = dialog.end.clamp(edit_bounds.0, edit_bounds.1);
            let valid = b - a >= 0.01 && visible_lanes.iter().any(|id| **id == dialog.lane);
            ui.horizontal(|ui| {
                if ui.add_enabled(valid, egui::Button::new("Apply")).clicked() {
                    let gain = match dialog.action {
                        RangeAction::Mute => 0.0,
                        RangeAction::Gain => dialog.percent / 100.0,
                        RangeAction::Reset => 1.0,
                    };
                    actions.push(LaneAction::Edit { lanes: vec![dialog.lane.clone()], a, b, gain });
                    st.sel = Some((dialog.lane.clone(), a, b));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            st.edit_dialog = None;
        }
    }
    // Selection toolbar.
    if let Some((tr, a, b)) = st.sel.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(4.0);
            let name = if st.all_tracks { "all audio".to_string() } else {
                lane_ids.iter().position(|lane| *lane == tr).and_then(|i| labels.get(i)).cloned().unwrap_or_default()
            };
            ui.label(RichText::new(format!("Selected {}–{} on {}", fmt_time(a, wall), fmt_time(b, wall), name)).color(theme::TEXT));
            let lanes: Vec<String> = if st.all_tracks { lane_ids.iter().filter(|id| !id.starts_with("app:")).cloned().collect() } else { vec![tr] };
            let apply = |gain: f32, actions: &mut Vec<LaneAction>| actions.push(LaneAction::Edit { lanes: lanes.clone(), a, b, gain });
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
            ui.checkbox(&mut st.all_tracks, "All audio");
            if theme::icon_button(ui, icon::CLOSE, 11.0, "Clear selection").clicked() {
                st.sel = None;
            }
        });
    }
    if n == 0 {
        return actions;
    }

    let span = (t1 - t0).max(1e-6);
    let header_height = ui.cursor().top() - top;
    let rows_height = (height_budget - header_height).clamp(LANE_H, 210.0);
    egui::ScrollArea::vertical().id_salt("audio_lane_rows").max_height(rows_height).show(ui, |ui| {
        for (i, lane) in lane_ids.iter().enumerate() {
            if !visible_lanes.contains(&lane) {
                continue;
            }
            let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), LANE_H), Sense::hover());
            if st.reveal_app.as_ref() == Some(lane) {
                ui.scroll_to_rect(row, Some(egui::Align::Center));
                st.reveal_app = None;
            }
            let axis = axis_rect(row);
            let x_of = |t: f64| axis.left() + (((t - t0) / span) as f32 * axis.width());
            let t_of = |x: f32| t0 + ((x - axis.left()) / axis.width().max(1.0)) as f64 * span;
            let edit_time = |x: f32| t_of(x).clamp(edit_bounds.0, edit_bounds.1);
            let muted_by_mode = has_full && n > 1 && ((player.mix.mode == MixMode::Full) != (lane == LANE_FULL));
            let audible_gain = if muted_by_mode { 0.0 } else { player.mix.lane(lane).effective() };

            // ---- header (name, mute, volume)
            let head = Rect::from_min_size(row.min, Vec2::new(GUTTER - 4.0, LANE_H - 4.0));
            ui.painter().rect_filled(head, CornerRadius::same(8), theme::SURFACE);
            let name = &labels[i];
            let dim = audible_gain <= 0.0005;
            ui.painter().text(
                head.left_top() + egui::vec2(10.0, 6.0),
                Align2::LEFT_TOP,
                crate::views::clips::truncate(name, 14),
                FontId::new(12.5, theme::semibold()),
                if dim { theme::TEXT_FAINT } else { theme::TEXT },
            );
            if player.expandable(lane) {
                let expanded = player.expanded(lane);
                let button_rect = Rect::from_center_size(Pos2::new(head.right() - 53.0, head.top() + 13.0), Vec2::new(52.0,20.0));
                if ui.put(button_rect, egui::Button::new(if expanded { "Apps ▾" } else { "Apps ▸" }).frame(false))
                    .on_hover_text(if expanded { "Collapse apps" } else { "Show apps in this category" }).clicked()
                {
                    player.set_expanded(lane, !expanded);
                    if !expanded {
                        st.reveal_app = player.lane_ids.iter()
                            .skip_while(|id| *id != lane)
                            .skip(1)
                            .take_while(|id| id.starts_with("app:"))
                            .find(|id| !display.as_ref().is_some_and(|d| d.hidden.contains(*id)))
                            .cloned();
                    }
                }
            }
            let g = player.mix.lane(lane).effective();
            let mute_rect = Rect::from_center_size(Pos2::new(head.right() - 16.0, head.top() + 13.0), Vec2::splat(22.0));
            let mresp = ui.interact(mute_rect, ui.scope_id().with(("lane_mute", lane)), Sense::click());
            let glyph = if g <= 0.0005 { icon::MUTE } else { icon::VOLUME };
            if mresp.hovered() {
                ui.painter().circle_filled(mute_rect.center(), 11.0, theme::SURFACE3);
            }
            ui.painter().text(mute_rect.center(), Align2::CENTER_CENTER, glyph, FontId::new(11.0, theme::icons_family()), theme::TEXT_DIM);
            if mresp.on_hover_text(if g <= 0.0005 { "Unmute" } else { "Mute" }).clicked() {
                let previous = player.mix.clone();
                player.set_lane_muted(lane, g > 0.0005);
                actions.push(LaneAction::MixChanged { previous: Some(previous) });
            }
            let slider_rect = Rect::from_min_size(head.left_top() + egui::vec2(8.0, 22.0), Vec2::new(GUTTER - 60.0, 14.0));
            let mut gv = g;
            let sresp = ui.put(slider_rect, egui::Slider::new(&mut gv, 0.0..=4.0).show_value(false).trailing_fill(true));
            if sresp.changed() {
                let dragging = sresp.is_pointer_button_down_on() || sresp.drag_stopped();
                let previous = st.gain_gesture.new_change(lane, dragging).then(|| player.mix.clone());
                player.set_lane_gain(lane, gv);
                actions.push(LaneAction::MixChanged { previous });
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
                    let from = (ta - s.start).max(0.0);
                    let to = (tb - s.start).min(s.end - s.start);
                    if let Some(mapping) = s.tracks {
                        if let Some(intervals) = mapping.get(lane) {
                            for &(track, a, b) in intervals {
                                if a < to && b > from {
                                    v = v.max(s.wave.max_in(track, a.max(from), b.min(to), 48));
                                }
                            }
                        }
                    }
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
            for e in edits.iter().filter(|e| e.lane_id() == *lane && e.end > t0 && e.start < t1) {
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
            if let Some((tr, a, b)) = &st.sel {
                if (tr == lane || st.all_tracks) && *a < t1 && *b > t0 {
                    let r = Rect::from_x_y_ranges(x_of(*a).max(wave_area.left())..=x_of(*b).min(wave_area.right()), wave_area.y_range());
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
            let resp = ui.interact(wave_area, waveform_id(lane), Sense::click_and_drag());
            // An edit edge under the pointer (for resizing).
            let edge_at = |x: f32| -> Option<(f64, f64, f32, bool)> {
                edits.iter().filter(|e| e.lane_id() == *lane).find_map(|e| {
                    if (x - x_of(e.start)).abs() <= 5.0 {
                        Some((e.start, e.end, e.gain, true))
                    } else if (x - x_of(e.end)).abs() <= 5.0 {
                        Some((e.start, e.end, e.gain, false))
                    } else {
                        None
                    }
                })
            };
            let hover_edge = resp.hover_pos().and_then(|p| edge_at(p.x)).is_some();
            if resp.contains_pointer() && ui.input(|i| i.pointer.primary_pressed()) {
                if let Some(origin) = ui.input(|i| i.pointer.press_origin()) {
                    st.wave_press = Some((lane.clone(), origin.x));
                }
            }
            let mut selected_by_drag = false;
            let anchor = st.wave_press.as_ref().filter(|(id, _)| id == lane).map(|(_, x)| *x);
            if let (Some(x), Some(pos)) = (anchor, ui.input(|i| i.pointer.latest_pos())) {
                let released = ui.input(|i| i.pointer.primary_released());
                if (pos.x - x).abs() >= 3.0 {
                    selected_by_drag = true;
                    let t = edit_time(pos.x.clamp(wave_area.left(), wave_area.right()));
                    let (from, to) = if let Some((a, b, gain, left)) = edge_at(x) {
                        let (start, end) = if left {
                            (t.min(b - 0.05).max(edit_bounds.0), b)
                        } else {
                            (a, t.max(a + 0.05).min(edit_bounds.1))
                        };
                        if released && end - start >= 0.01 {
                            actions.push(LaneAction::Edit { lanes: vec![lane.clone()], a, b, gain: 1.0 });
                            actions.push(LaneAction::Edit { lanes: vec![lane.clone()], a: start, b: end, gain });
                        }
                        (start, end)
                    } else {
                        let start = edit_time(x);
                        (start.min(t), start.max(t))
                    };
                    st.sel = Some((lane.clone(), from, to));
                }
            }
            if resp.clicked() && !selected_by_drag {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let t = t_of(pos.x);
                    match edits.iter().find(|e| e.lane_id() == *lane && e.start <= t && e.end >= t) {
                        Some(e) => {
                            st.sel = Some((lane.clone(), e.start, e.end));
                            st.custom_pct = e.gain * 100.0;
                        }
                        None => {
                            st.sel = None;
                            actions.push(LaneAction::Seek(t));
                        }
                    }
                }
            }
            if hover_edge || anchor.is_some_and(|x| edge_at(x).is_some()) {
                resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            } else {
                resp.on_hover_cursor(egui::CursorIcon::Text);
            }
        }
    });
    if !ui.input(|i| i.pointer.primary_down()) {
        st.wave_press = None;
        st.gain_gesture.release();
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_ignores_an_ended_snapshot_before_the_new_playhead_arrives() {
        let mut preview = RangePreview::new(1.0, 2.0);
        assert!(matches!(preview.progress(Some(8.0), true, true), PreviewProgress::Playing));
        assert!(matches!(preview.progress(Some(1.1), false, false), PreviewProgress::Playing));
        assert!(matches!(preview.progress(Some(2.0), false, false), PreviewProgress::Finished));
    }

    #[test]
    fn undo_restores_one_edit_gesture() {
        let mut history = EditHistory::default();
        let mut edits = vec![AudioEdit::new("app:game.exe", 1.0, 2.0, 0.0)];
        history.save(&edits, MixSettings::default());

        // Resizing clears the old region and adds a new one in the same gesture.
        cv_core::model::apply_edit(&mut edits, "app:game.exe", 1.0, 2.0, 1.0);
        cv_core::model::apply_edit(&mut edits, "app:game.exe", 1.0, 3.0, 0.0);
        let snapshot = history.undo().unwrap();
        assert_eq!(snapshot.edits, vec![AudioEdit::new("app:game.exe", 1.0, 2.0, 0.0)]);
        assert!(!history.can_undo());
    }

    #[test]
    fn one_drag_mute_and_region_edit_undo_in_reverse_order() {
        use cv_core::mixer::LaneGain;
        let mut history = EditHistory::default();
        let mut gesture = GainGesture::default();
        let mut mix = MixSettings::default();
        let lane = "cat:voice";
        let mut edits = Vec::new();

        for gain in [0.95, 0.8, 0.5] {
            if gesture.new_change(lane, true) {
                history.save(&edits, mix.clone());
            }
            mix.set(lane, LaneGain { gain, muted: false });
        }
        gesture.release();

        history.save(&edits, mix.clone());
        mix.set(lane, LaneGain { gain: 0.5, muted: true });
        history.save(&edits, mix.clone());
        edits.push(AudioEdit::new(lane, 1.0, 2.0, 0.0));

        let region = history.undo().unwrap();
        assert!(region.edits.is_empty());
        assert!(region.mix.lane(lane).muted);

        let mute = history.undo().unwrap();
        assert!(!mute.mix.lane(lane).muted);
        assert_eq!(mute.mix.lane(lane).gain, 0.5);

        let drag = history.undo().unwrap();
        assert_eq!(drag.mix, MixSettings::default());
        assert!(!history.can_undo());
        assert!(gesture.new_change(lane, true));
    }

    #[test]
    fn mixer_mode_and_keyboard_gain_changes_are_individual_actions() {
        let mut history = EditHistory::default();
        let mut gesture = GainGesture::default();
        let mut mix = MixSettings::default();
        history.save(&[], mix.clone());
        mix.mode = MixMode::Full;
        for gain in [0.9, 0.8] {
            assert!(gesture.new_change(LANE_FULL, false));
            history.save(&[], mix.clone());
            mix.set(LANE_FULL, cv_core::mixer::LaneGain { gain, muted: false });
        }
        assert_eq!(history.undo().unwrap().mix.lane(LANE_FULL).gain, 0.9);
        assert_eq!(history.undo().unwrap().mix.mode, MixMode::Full);
        assert_eq!(history.undo().unwrap().mix.mode, MixMode::Separate);
    }

    #[test]
    fn saved_display_uses_lane_ids() {
        let mut display = LaneDisplay::default();
        display.hidden.insert("cat:other".into());
        display.names.insert("app:game.exe".into(), "Game audio".into());

        let reopened: LaneDisplay = serde_json::from_slice(&serde_json::to_vec(&display).unwrap()).unwrap();
        assert!(reopened.hidden.contains("cat:other"));
        assert_eq!(reopened.names.get("app:game.exe").map(String::as_str), Some("Game audio"));
    }
}

#[cfg(test)]
#[path = "lanes/tests_drag.rs"]
mod tests_drag;
