//! Exact timeline and canvas values, committed as a single history edit.
use super::{clock, filename, frame_tick, role_picker, seconds, ticks, Selection, Workbench};
use crate::theme;
use anyhow::{ensure, Context as _, Result};
use cv_core::editor::{AssetKind, AudioFade, Canvas, Project, Rotation, SoundRole, Source, Span, VisualTransform};
use eframe::egui::{self, RichText, Ui};

#[derive(Clone, PartialEq)]
enum Properties {
    Shot { source: Source, input: f64, output: f64, gain: f64, fade: [f64; 2], visual_transform: VisualTransform },
    Sound { role: SoundRole, at: f64, input: f64, output: f64, gain: f64, fade: [f64; 2] },
}

#[derive(Default)]
pub(super) struct Inspector {
    selection: Option<Selection>,
    revision: Option<u64>,
    properties: Option<Properties>,
    original: Option<Properties>,
    name: String,
    canvas: Option<Canvas>,
}

impl Workbench {
    pub(super) fn inspector(&mut self, ui: &mut Ui) {
        let mut state = std::mem::take(&mut self.inspector);
        if state.revision != Some(self.revision) {
            state.revision = Some(self.revision);
            state.name = self.history.project.name.clone();
            state.canvas = Some(self.history.project.canvas);
            state.selection = None;
            state.properties = None;
        }
        if state.selection != self.selected {
            state.selection = self.selected;
            state.properties = self.selected.and_then(|selection| properties(&self.history.project, selection));
            state.original = state.properties.clone();
        }

        let apply = ui.horizontal_wrapped(|ui| {
            ui.strong("Inspector");
            let label = if self.selected.is_some() { "Apply Changes" } else { "Apply Project Settings" };
            let pending = state.properties != state.original;
            let button = ui.button(label).on_hover_text("Apply edited fields. Enter also applies changes to a selected item.");
            if pending { ui.label(theme::small("Unapplied edits")); }
            button.clicked()
        }).inner;
        egui::ScrollArea::vertical().id_salt("editor-inspector-fields")
            .max_height(ui.available_height()).show(ui, |ui| {
                if let Some(selection) = self.selected {
                    self.item_properties(ui, &mut state, selection);
                    ui.separator();
                    egui::CollapsingHeader::new("Project Settings").show(ui, |ui| {
                        self.project_properties(ui, &mut state, true);
                    });
                } else {
                    self.project_properties(ui, &mut state, false);
                }
            });
        let enter = self.selected.is_some() && state.properties != state.original && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if apply || enter {
            if let (Some(selection), Some(values)) = (self.selected, state.properties.as_ref()) {
                if state.original.as_ref() != Some(values) {
                    self.apply_item(ui.ctx(), selection, values.clone());
                }
            } else if self.selected.is_none() {
                self.apply_project_settings(ui.ctx(), &state);
            }
        }
        self.inspector = state;
    }

    fn project_properties(&mut self, ui: &mut Ui, state: &mut Inspector, show_apply: bool) {
        let Some(canvas) = &mut state.canvas else { return; };
        ui.label("Project Name");
        ui.add(egui::TextEdit::singleline(&mut state.name).desired_width(ui.available_width()));
        let sizes = [
            ("Landscape · 720p", (1280, 720)),
            ("Landscape · 1080p", (1920, 1080)),
            ("Portrait · 720 × 1280", (720, 1280)),
            ("Portrait · 1080 × 1920", (1080, 1920)),
            ("Square · 1:1", (1080, 1080)),
        ];
        let mut size = (canvas.width, canvas.height);
        let size_label = sizes.iter().find(|(_, dims)| *dims == size).map(|(name, _)| *name).unwrap_or("Custom");
        ui.label("Canvas");
        egui::ComboBox::from_id_salt("editor-canvas").selected_text(size_label).show_ui(ui, |ui| {
            for (name, dims) in sizes { ui.selectable_value(&mut size, dims, name); }
        });
        (canvas.width, canvas.height) = size;
        ui.label("Frame Rate");
        egui::ComboBox::from_id_salt("editor-fps").selected_text(format!("{} fps", canvas.fps)).show_ui(ui, |ui| {
            for fps in [24, 25, 30, 60] { ui.selectable_value(&mut canvas.fps, fps, format!("{fps} fps")); }
        });
        if show_apply && ui.button("Apply Project Settings").clicked() {
            self.apply_project_settings(ui.ctx(), state);
        }
    }

    fn apply_project_settings(&mut self, ctx: &egui::Context, state: &Inspector) {
        let Some(canvas) = state.canvas else { return; };
        let name = state.name.trim().to_owned();
        if self.history.project.name == name && self.history.project.canvas == canvas { return; }
        self.change(ctx, |project| {
            ensure!(!name.is_empty(), "Enter a project name.");
            project.name = name;
            project.canvas = canvas;
            Ok(())
        });
    }

    fn item_properties(&mut self, ui: &mut Ui, state: &mut Inspector, selection: Selection) {
        let Some(values) = &mut state.properties else { return; };
        let project = &self.history.project;
        let at = match selection {
            Selection::Shot(id) => project.shot_start(id).unwrap_or(0),
            Selection::Sound(id) => project.sounds.iter().find(|sound| sound.id == id).map_or(0, |sound| sound.at),
        };
        match values {
            Properties::Shot { source, input, output, gain, fade, visual_transform } => {
                let limit = match source {
                    Source::Asset(id) => {
                        let asset = project.asset(*id).unwrap();
                        let label = filename(&asset.path);
                        ui.add(egui::Label::new(&label).truncate()).on_hover_text(asset.path.display().to_string());
                        if asset.info.kind == AssetKind::Video { seconds(asset.info.duration) } else { seconds(i64::MAX as u64) }
                    }
                    Source::Title(title) => {
                        ui.label("Title Text");
                        ui.add(egui::TextEdit::singleline(title).desired_width(ui.available_width()));
                        seconds(i64::MAX as u64)
                    }
                    Source::Color(color) => {
                        ui.label("Matte Color");
                        ui.color_edit_button_srgb(color);
                        seconds(i64::MAX as u64)
                    }
                };
                ui.label(RichText::new(format!("Timeline start {}", clock(at))).small().color(theme::TEXT_DIM));
                time_field(ui, "Source In", input, limit);
                time_field(ui, "Source Out", output, limit);
                if let Source::Asset(id) = source {
                    visual_transform_fields(ui, visual_transform);
                    if project.asset(*id).is_some_and(|asset| asset.info.kind == AssetKind::Video) {
                        gain_field(ui, gain);
                        fade_fields(ui, fade, *output - *input);
                    }
                }
            }
            Properties::Sound { role, at, input, output, gain, fade } => {
                let Selection::Sound(id) = selection else { return; };
                let sound = project.sounds.iter().find(|sound| sound.id == id).unwrap();
                let asset = project.asset(sound.asset).unwrap();
                ui.add(egui::Label::new(filename(&asset.path)).truncate()).on_hover_text(asset.path.display().to_string());
                time_field(ui, "Start on Timeline", at, seconds(i64::MAX as u64));
                gain_field(ui, gain);
                ui.label("Audio Lane");
                role_picker(ui, "editor-sound-role", role);
                time_field(ui, "Source In", input, seconds(asset.info.duration));
                time_field(ui, "Source Out", output, seconds(asset.info.duration));
                fade_fields(ui, fade, *output - *input);
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("Set In at Playhead").clicked() { self.mark_trim(selection, true, ui.ctx()); }
            if ui.button("Set Out at Playhead").clicked() { self.mark_trim(selection, false, ui.ctx()); }
        });
    }

    fn apply_item(&mut self, ctx: &egui::Context, selection: Selection, values: Properties) {
        self.change(ctx, |project| {
            match (selection, values) {
                (Selection::Shot(id), Properties::Shot { source, input, output, gain, fade, visual_transform }) => {
                    let shot = project.shots.iter_mut().find(|shot| shot.id == id).context("Select a video item")?;
                    shot.source = source;
                    trim_and_fade(&mut shot.range, &mut shot.audio_fade, Span { start: ticks(input), end: ticks(output) }, Some(fade.map(ticks)))?;
                    shot.gain = gain;
                    shot.visual_transform = visual_transform;
                }
                (Selection::Sound(id), Properties::Sound { role, at, input, output, gain, fade }) => {
                    let sound = project.sounds.iter_mut().find(|sound| sound.id == id).context("Select an audio item")?;
                    sound.role = role;
                    sound.at = ticks(at);
                    trim_and_fade(&mut sound.range, &mut sound.audio_fade, Span { start: ticks(input), end: ticks(output) }, Some(fade.map(ticks)))?;
                    sound.gain = gain;
                }
                _ => unreachable!(),
            }
            Ok(())
        });
    }

    fn mark_trim(&mut self, selection: Selection, start: bool, ctx: &egui::Context) {
        let playhead = self.playhead;
        self.change(ctx, |project| {
            let min_length = frame_tick(1, project.canvas.fps);
            let (at, range, fade, sound_at) = match selection {
                Selection::Shot(id) => {
                    let at = project.shot_start(id).context("Select a video item")?;
                    let shot = project.shots.iter_mut().find(|shot| shot.id == id).unwrap();
                    (at, &mut shot.range, &mut shot.audio_fade, None)
                }
                Selection::Sound(id) => {
                    let sound = project.sounds.iter_mut().find(|sound| sound.id == id).context("Select an audio item")?;
                    (sound.at, &mut sound.range, &mut sound.audio_fade, Some(&mut sound.at))
                }
            };
            let offset = playhead.checked_sub(at).context("Place the playhead inside the selected item.")?;
            ensure!(offset <= range.len(), "Place the playhead inside the selected item.");
            let mut next = *range;
            if start {
                ensure!(range.len().saturating_sub(offset) >= min_length, "Leave at least one frame in the item.");
                next.start += offset;
                if let Some(value) = sound_at { *value = playhead; }
            } else {
                ensure!(offset >= min_length, "Leave at least one frame in the item.");
                next.end = range.start + offset;
            }
            trim_and_fade(range, fade, next, None)
        });
    }
}

fn properties(project: &Project, selection: Selection) -> Option<Properties> {
    match selection {
        Selection::Shot(id) => project.shots.iter().find(|shot| shot.id == id).map(|shot| Properties::Shot {
            source: shot.source.clone(), input: seconds(shot.range.start), output: seconds(shot.range.end), gain: shot.gain,
            visual_transform: shot.visual_transform,
            fade: fade_ticks(shot.audio_fade).map(seconds),
        }),
        Selection::Sound(id) => project.sounds.iter().find(|sound| sound.id == id).map(|sound| Properties::Sound {
            role: sound.role, at: seconds(sound.at), input: seconds(sound.range.start), output: seconds(sound.range.end), gain: sound.gain,
            fade: fade_ticks(sound.audio_fade).map(seconds),
        }),
    }
}

pub(super) fn time_field(ui: &mut Ui, label: &str, value: &mut f64, limit: f64) -> egui::Response {
    ui.horizontal_wrapped(|ui| {
        let text = ui.label(label);
        ui.add(egui::DragValue::new(value).range(0.0..=limit).speed(0.02).max_decimals(7)
            .suffix(" s").update_while_editing(false)).labelled_by(text.id)
    }).inner
}

fn fade_ticks(fade: Option<AudioFade>) -> [u64; 2] {
    fade.map_or([0, 0], |fade| [fade.fade_in, fade.fade_out])
}

pub(super) fn trim_and_fade(range: &mut Span, fade: &mut Option<AudioFade>, next: Span, durations: Option<[u64; 2]>) -> Result<()> {
    let old = fade_ticks(*fade);
    let new = durations.unwrap_or(old);
    if *range != next || old != new {
        // A split keeps the original fade curve; editing a trim or duration creates a new curve.
        *fade = AudioFade::for_range(next, new[0], new[1])?;
        *range = next;
    }
    Ok(())
}

fn fade_fields(ui: &mut Ui, values: &mut [f64; 2], duration: f64) {
    for (label, value) in ["Fade in", "Fade out"].into_iter().zip(values) {
        let limit = duration.max(*value);
        time_field(ui, label, value, limit);
    }
}

fn visual_transform_fields(ui: &mut Ui, transform: &mut VisualTransform) {
    let rotations = [
        (Rotation::None, "0°"),
        (Rotation::Clockwise90, "90°"),
        (Rotation::Clockwise180, "180°"),
        (Rotation::Clockwise270, "270°"),
    ];
    let selected = match transform.rotation {
        Rotation::None => "0°",
        Rotation::Clockwise90 => "90°",
        Rotation::Clockwise180 => "180°",
        Rotation::Clockwise270 => "270°",
    };
    ui.horizontal_wrapped(|ui| {
        let label = ui.label("Rotation");
        egui::ComboBox::from_id_salt("editor-shot-rotation")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (rotation, text) in rotations {
                    ui.selectable_value(&mut transform.rotation, rotation, text);
                }
            })
            .response
            .labelled_by(label.id)
            .on_hover_text("Clockwise rotation");
    });
    ui.checkbox(&mut transform.mirror, "Mirror Horizontally")
        .on_hover_text("Flip left to right after rotation");
}

fn gain_field(ui: &mut Ui, value: &mut f64) {
    ui.horizontal_wrapped(|ui| {
        let text = ui.label("Gain");
        let mut percent = *value * 100.0;
        if ui.add(egui::DragValue::new(&mut percent).range(0.0..=200.0).speed(1.0)
            .max_decimals(1).suffix("%").update_while_editing(false)).labelled_by(text.id).changed()
        {
            *value = percent / 100.0;
        }
    });
}
