use super::{clock, seconds, ticks, Workbench};
use crate::qa_driver;
use cv_core::editor::{CaptionPosition, Id, Span};
use eframe::egui;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct Captions {
    pub(super) open: bool,
    was_open: bool,
    selected: Option<Id>,
    text: String,
    start: f64,
    end: f64,
    message: String,
}

impl Captions {
    fn new_at(&mut self, tick: u64) {
        self.selected = None;
        self.text.clear();
        self.start = seconds(tick);
        self.end = self.start + 2.0;
        self.message.clear();
    }
}

enum Action { Apply, Remove, Import(PathBuf), Export(PathBuf), Go(u64), Position(CaptionPosition) }

impl Workbench {
    pub(super) fn caption_window(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !self.captions.open {
            self.captions.was_open = false;
            return;
        }
        let mut state = std::mem::take(&mut self.captions);
        if !state.was_open {
            self.pause();
            state.new_at(self.playhead);
            state.was_open = true;
        }
        let mut open = true;
        let mut action = None;
        let size = ctx.viewport_rect().size();
        let project = &self.history.project;
        egui::Window::new("Captions").id(egui::Id::unique("editor-caption-window"))
            .open(&mut open).collapsible(false).default_width(460.0_f32.min(size.x * 0.83))
            .max_width((size.x - 24.0).max(150.0)).max_height((size.y - 60.0).max(170.0)).vscroll(true)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("New at Playhead").clicked() { state.new_at(self.playhead); }
                    if ui.button("Import SRT…").clicked() {
                        if let Some(path) = qa_driver::pick_file(|| rfd::FileDialog::new().set_parent(frame)
                            .add_filter("SubRip subtitles", &["srt"]).pick_file()) { action = Some(Action::Import(path)); }
                    }
                    if ui.add_enabled(!project.captions.is_empty(), egui::Button::new("Export SRT…")).clicked() {
                        if let Some(path) = qa_driver::save_file(|| rfd::FileDialog::new().set_parent(frame)
                            .set_file_name("Subtitles.srt").add_filter("SubRip subtitles", &["srt"]).save_file()) { action = Some(Action::Export(path)); }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Caption position");
                    for (position, label) in [(CaptionPosition::Bottom, "Bottom"), (CaptionPosition::Top, "Top")] {
                        if ui.selectable_label(project.caption_position == position, label).clicked()
                            && position != project.caption_position { action = Some(Action::Position(position)); }
                    }
                });
                if !project.captions.is_empty() {
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().id_salt("editor-caption-list").max_height(130.0).show(ui, |ui| {
                        let mut cues = project.captions.iter().collect::<Vec<_>>();
                        cues.sort_by_key(|cue| (cue.range.start, cue.id));
                        for cue in cues {
                            let name = format!("{}   {}", clock(cue.range.start), cue.text.lines().next().unwrap_or(""));
                            if ui.add(egui::Button::selectable(state.selected == Some(cue.id), name).truncate()).clicked() {
                                state.selected = Some(cue.id);
                                state.text = cue.text.clone();
                                state.start = seconds(cue.range.start);
                                state.end = seconds(cue.range.end);
                                state.message.clear();
                                action = Some(Action::Go(cue.range.start));
                            }
                        }
                    });
                }
                ui.separator();
                ui.label("Caption text");
                ui.add(egui::TextEdit::multiline(&mut state.text).desired_width(f32::INFINITY).desired_rows(2));
                ui.horizontal_wrapped(|ui| {
                    ui.label("Start");
                    ui.add(egui::DragValue::new(&mut state.start).range(0.0..=i64::MAX as f64 / 1e7).speed(0.04).max_decimals(3).suffix(" s").custom_parser(super::parse_time)).on_hover_text("Click to type. Ctrl+A selects the whole time.");
                    ui.label("End");
                    ui.add(egui::DragValue::new(&mut state.end).range(0.0..=i64::MAX as f64 / 1e7).speed(0.04).max_decimals(3).suffix(" s").custom_parser(super::parse_time)).on_hover_text("Click to type. Ctrl+A selects the whole time.");
                });
                let valid = !state.text.trim().is_empty() && ticks(state.end) > ticks(state.start);
                ui.horizontal_wrapped(|ui| {
                    let label = if state.selected.is_some() { "Save Caption" } else { "Add Caption" };
                    if ui.add_enabled(valid, egui::Button::new(label)).clicked() { action = Some(Action::Apply); }
                    if ui.add_enabled(state.selected.is_some(), egui::Button::new("Remove Caption")).clicked() { action = Some(Action::Remove); }
                });
                if state.end <= state.start { ui.label("Set the end after the start."); }
                if !state.message.is_empty() { ui.add(egui::Label::new(&state.message).wrap()); }
            });
        state.open = open;
        match action {
            Some(Action::Apply) => {
                let range = Span { start: ticks(state.start), end: ticks(state.end) };
                let text = state.text.trim().to_string();
                let result = self.history.edit(|project| {
                    if let Some(id) = state.selected {
                        let cue = project.captions.iter_mut().find(|cue| cue.id == id).ok_or_else(|| anyhow::anyhow!("The selected cue was removed."))?;
                        cue.range = range;
                        cue.text = text;
                        Ok(id)
                    } else { project.caption(text, range) }
                });
                match result {
                    Ok(id) => { state.selected = Some(id); state.message = "Caption saved.".into(); self.changed(ctx); self.seek(range.start, ctx); }
                    Err(error) => state.message = format!("{error:#}"),
                }
            }
            Some(Action::Remove) => if let Some(id) = state.selected {
                match self.history.edit(|project| project.remove(id)) {
                    Ok(()) => { state.new_at(self.playhead); self.changed(ctx); }
                    Err(error) => state.message = format!("{error:#}"),
                }
            },
            Some(Action::Import(path)) => match self.history.edit(|project| project.import_srt(&path)) {
                Ok(count) => { state.message = format!("Imported {count} captions."); self.changed(ctx); }
                Err(error) => state.message = format!("{error:#}"),
            },
            Some(Action::Export(path)) => match self.history.project.export_srt(&path) {
                Ok(()) => state.message = format!("Saved {}", path.display()),
                Err(error) => state.message = format!("{error:#}"),
            },
            Some(Action::Go(tick)) => self.seek(tick, ctx),
            Some(Action::Position(position)) => { self.change(ctx, |p| { p.caption_position = position; Ok(()) }); }
            None => {}
        }
        self.captions = state;
    }
}
