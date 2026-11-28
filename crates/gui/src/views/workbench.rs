//! The independent Editor page. Recording and Clips keep their shared player.
mod availability;
mod captions;
mod inspector;
mod preview;
mod timeline;

use super::{editor_layout::region, workbench_wiki::{self, Wiki}};
use crate::{app::Ctx, qa_driver, theme};
use anyhow::{bail, Context as _, Result};
use cv_core::editor::{Asset, AssetInfo, AssetKind, History, Id, Project, SoundRole, TICKS_PER_SECOND};
use eframe::egui::{self, Align, Color32, Key, Layout, Margin, Rect, RichText, Sense, Ui, Vec2};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
};

const PROJECT_EXTENSION: &str = "cvproj";
const MIN_WINDOW: Vec2 = egui::vec2(360.0, 500.0);
pub(crate) const STANDARD_MIN_WINDOW: Vec2 = egui::vec2(690.0, 500.0);

pub(crate) fn set_min_window(ctx: &egui::Context, active: bool) {
    ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(
        if active { MIN_WINDOW } else { STANDARD_MIN_WINDOW },
    ));
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Workbench,
    Wiki,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Dock {
    #[default]
    Media,
    Inspector,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selection {
    Shot(Id),
    Sound(Id),
}

impl Selection {
    fn id(self) -> Id {
        match self {
            Self::Shot(id) | Self::Sound(id) => id,
        }
    }
}

#[derive(Clone, Copy)]
enum Replace {
    New,
    Open,
    Close,
}

#[derive(Clone, Copy)]
enum Generated {
    Title,
    Color,
}

struct Generator {
    kind: Generated,
    at_start: bool,
    text: String,
    rgb: [u8; 3],
    duration: f64,
}

enum Notice {
    Info(String),
    Error(String),
    MissingMedia(Id),
    ExportError(String),
}

type Probed = Vec<(PathBuf, Result<AssetInfo>)>;

pub struct Workbench {
    history: History,
    file: Option<PathBuf>,
    saved: Project,
    unsaved: bool,
    tab: Tab,
    wiki: Wiki,
    captions: captions::Captions,
    dock: Dock,
    dock_open: bool,
    preview_expanded: bool,
    compact_preview_height: Option<f32>,
    seek_input: Option<(String, bool)>,
    error_details: Option<String>,
    selected_asset: Option<Id>,
    selected: Option<Selection>,
    audio_role: SoundRole,
    timeline: timeline::Timeline,
    inspector: inspector::Inspector,
    playhead: u64,
    preview: preview::Preview,
    export: preview::Export,
    last_export: Option<PathBuf>,
    imports: Option<Receiver<Probed>>,
    availability: availability::Availability,
    relink_task: Option<(Id, Receiver<Result<(PathBuf, AssetInfo)>>)>,
    replace: Option<Replace>,
    close_confirmed: bool,
    generator: Option<Generator>,
    notice: Option<Notice>,
    revision: u64,
}

impl Default for Workbench {
    fn default() -> Self {
        let project = Project::default();
        Self {
            saved: project.clone(),
            unsaved: false,
            history: History::new(project),
            file: None,
            tab: Tab::default(),
            wiki: Wiki::default(),
            captions: captions::Captions::default(),
            dock: Dock::default(),
            dock_open: false,
            preview_expanded: false,
            compact_preview_height: None,
            seek_input: None,
            error_details: None,
            selected_asset: None,
            selected: None,
            audio_role: SoundRole::Music,
            timeline: timeline::Timeline::default(),
            inspector: inspector::Inspector::default(),
            playhead: 0,
            preview: preview::Preview::default(),
            export: preview::Export::default(),
            last_export: None,
            imports: None,
            availability: availability::Availability::default(),
            relink_task: None,
            replace: None,
            close_confirmed: false,
            generator: None,
            notice: None,
            revision: 0,
        }
    }
}

impl Workbench {
    pub fn pause(&mut self) {
        self.preview.pause();
    }

    pub(crate) fn qa_view(&mut self, page: &str) {
        let mut project = Project::default();
        let id = project.title("Opening title".into(), 2 * TICKS_PER_SECOND).expect("fixture title");
        project.color([62; 3], 3 * TICKS_PER_SECOND).expect("fixture color");
        *self = Self::default();
        self.saved = project.clone();
        self.history = History::new(project);
        self.selected = Some(Selection::Shot(id));
        self.dock = Dock::Inspector;
        self.dock_open = matches!(page, "workbench_short_inspector" | "workbench_short_bin");
        if page == "workbench_short_bin" {
            self.dock = Dock::Media;
            let info = AssetInfo { kind: AssetKind::Audio, fps_milli: None, duration: TICKS_PER_SECOND, width: 0, height: 0 };
            self.selected_asset = Some(self.history.project.import(PathBuf::from("Synthetic cue.wav"), info).expect("fixture asset"));
            self.notice = Some(Notice::Info("Imported 1 file".into()));
        }
        self.preview_expanded = page == "workbench_preview";
        self.tab = if page == "workbench_wiki" { Tab::Wiki } else { Tab::Workbench };
    }

    pub fn intercept_close(&mut self, ctx: &egui::Context) -> bool {
        if self.close_confirmed || !self.dirty() {
            return false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.pause();
        self.tab = Tab::Workbench;
        if !matches!(self.replace, Some(Replace::Close)) {
            self.notice = None;
            self.replace = Some(Replace::Close);
        }
        true
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &Ctx, frame: &eframe::Frame) {
        self.finish_import(ui.ctx());
        self.finish_relink(ui.ctx());
        self.availability.poll(ui.ctx());
        self.export.poll();
        if self.tab == Tab::Workbench {
            self.preview.poll(ui.ctx(), &self.history.project, self.revision, &mut self.playhead);
        }

        ui.horizontal_wrapped(|ui| {
            ui.heading("Editor");
            ui.separator();
            ui.selectable_value(&mut self.tab, Tab::Workbench, "Workbench");
            ui.selectable_value(&mut self.tab, Tab::Wiki, "Wiki");
        });
        ui.add_space(5.0);
        if self.tab == Tab::Wiki {
            self.pause();
            workbench_wiki::show(ui, &mut self.wiki);
            return;
        }

        self.commands(ui, frame);
        let compact = ui.available_width() < 600.0 || self.preview_expanded;
        if compact {
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_label(!self.dock_open, "Timeline").clicked() { self.dock_open = false; }
                self.dock_button(ui, Dock::Media, "Media Bin");
                self.dock_button(ui, Dock::Inspector, "Inspector");
            });
        }
        self.output_status(ui, frame);
        ui.add_space(6.0);
        if ui.available_width() < 600.0 && !self.preview_expanded {
            self.compact_workspace(ui, frame, ctx.accent);
        } else {
            egui::ScrollArea::vertical()
            .id_salt("editor-workbench")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.upper(ui, frame, ctx.accent);
                ui.add_space(10.0);
                self.timeline(ui, ctx.accent);
            });
        }

        if self.seek_input.is_none() { self.shortcuts(ui, frame); }
        self.seek_dialog(ui.ctx());
        self.details_window(ui.ctx());
        self.replace_prompt(ui.ctx(), frame);
        self.generator_prompt(ui.ctx());
        self.caption_window(ui.ctx(), frame);
    }

    fn commands(&mut self, ui: &mut Ui, frame: &eframe::Frame) {
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("Project", |ui| {
                for (label, action) in [("New Project…", Replace::New), ("Open Project…", Replace::Open)] {
                    if ui.button(label).clicked() {
                        ui.close();
                        self.request_replace(action, frame, ui.ctx());
                    }
                }
                ui.separator();
                if ui.button("Save Project").on_hover_text("Ctrl+S").clicked() {
                    ui.close();
                    self.save(false, frame);
                }
                if ui.button("Save Project As…").on_hover_text("Ctrl+Shift+S").clicked() {
                    ui.close();
                    self.save(true, frame);
                }
            });
            if ui.add_enabled(self.imports.is_none(), egui::Button::new("Import Media…"))
                .on_hover_text("Ctrl+I · or drop files into the Editor").clicked()
            {
                self.pick_media(frame, ui.ctx());
            }
            let can_export = self.export.idle() && self.history.project.duration() > 0;
            if ui.add_enabled(can_export, egui::Button::new("Export MP4…")).clicked() {
                self.export(frame, ui.ctx());
            }
            if ui.button("Captions…").clicked() {
                self.pause();
                self.captions.open = true;
            }
            let name = self.file.as_deref().map(filename).unwrap_or_else(|| self.history.project.name.clone());
            let label = format!("{name}{}", if self.dirty() { " *" } else { "" });
            let details = self.file.as_ref().map_or_else(|| name.clone(), |file| {
                format!("{}\n{}", self.history.project.name, file.display())
            });
            ui.add(egui::Label::new(RichText::new(label).strong()).truncate()).on_hover_text(details);
        });
    }

    fn output_status(&mut self, ui: &mut Ui, frame: &eframe::Frame) {
        let mut row = false;
        let mut choose_export = false;
        if let Some(status) = self.export.status() {
            row = true;
            ui.horizontal_wrapped(|ui| match status {
                preview::ExportStatus::Running(progress, name) => {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                    ui.add(egui::ProgressBar::new(progress).desired_width(104.0).show_percentage())
                        .on_hover_text(format!("Exporting {name}"));
                    if ui.button("Cancel Export").clicked() {
                        self.export.cancel();
                    }
                }
                preview::ExportStatus::Saved(path) => {
                    if ui.small_button("Dismiss").on_hover_text("Hide the export result").clicked() { self.export.clear_result(); }
                    let label = format!("Exported {}", filename(&path));
                    ui.add(egui::Label::new(&label).truncate()).on_hover_text(path.display().to_string());
                    if ui.button("Open Output Folder").clicked() {
                        let mut target = std::ffi::OsString::from("/select,");
                        target.push(path);
                        if let Err(error) = std::process::Command::new("explorer.exe").arg(target).spawn() {
                            self.notice = Some(Notice::Error(format!("Open folder: {error}")));
                        }
                    }
                }
                preview::ExportStatus::Failed(error) => {
                    choose_export = ui.small_button("Choose new filename…").clicked();
                    if ui.small_button("Details").clicked() { self.error_details = Some(error.clone()); }
                    ui.add(egui::Label::new(RichText::new(&error).color(theme::DANGER)).truncate()).on_hover_text(error);
                }
            });
        }
        if self.imports.is_some() {
            row = true;
            ui.label("Importing media…");
        } else if self.relink_task.is_some() {
            row = true;
            ui.label("Relinking media…");
        } else if let Some(notice) = &self.notice {
            row = true;
            match notice {
                Notice::MissingMedia(id) => {
                    let id = *id;
                    if let Some(asset) = self.history.project.asset(id) {
                        let name = filename(&asset.path);
                        let path = asset.path.display().to_string();
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Relink Media…").clicked() { self.relink(id, frame, ui.ctx()); }
                            ui.add(egui::Label::new(format!("Missing for export: {name}")).truncate()).on_hover_text(path);
                        });
                    }
                }
                Notice::Info(message) => {
                    ui.add(egui::Label::new(RichText::new(message).color(theme::TEXT_DIM)).truncate()).on_hover_text(message);
                }
                Notice::Error(message) | Notice::ExportError(message) => {
                    let export_problem = matches!(notice, Notice::ExportError(_));
                    ui.horizontal_wrapped(|ui| {
                        if export_problem { choose_export = ui.small_button("Choose new filename…").clicked(); }
                        if ui.small_button("Details").clicked() { self.error_details = Some(message.clone()); }
                        ui.add(egui::Label::new(RichText::new(message.lines().next().unwrap_or(message)).color(theme::DANGER)).truncate()).on_hover_text(message);
                    });
                }
            }
        }
        if choose_export { self.export(frame, ui.ctx()); }
        if !row && ui.ctx().viewport_rect().height() >= 590.0 { ui.add_space(17.0); }
    }

    fn details_window(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error_details.as_ref() else { return; };
        let mut open = true;
        let size = ctx.viewport_rect().size();
        egui::Window::new("Error details").open(&mut open).collapsible(false)
            .default_width((size.x - 40.0).clamp(120.0, 500.0))
            .max_height((size.y - 70.0).max(80.0)).show(ctx, |ui| {
                if ui.button("Copy details").clicked() { ctx.copy_text(message.clone()); }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add(egui::Label::new(message).selectable(true).wrap());
                });
            });
        if !open { self.error_details = None; }
    }

    fn dock_button(&mut self, ui: &mut Ui, dock: Dock, label: &str) {
        if ui.selectable_label(self.dock_open && self.dock == dock, label).clicked() {
            self.dock_open = !self.dock_open || self.dock != dock;
            self.dock = dock;
        }
    }

    fn compact_workspace(&mut self, ui: &mut Ui, frame: &eframe::Frame, accent: Color32) {
        let width = ui.available_width();
        let available = ui.available_height();
        if available < 265.0 {
            ui.horizontal(|ui| {
                let thumbnail = if self.dock_open { egui::vec2(50.0, 28.0) } else { egui::vec2(70.0, 40.0) };
                let (picture, open) = ui.allocate_exact_size(thumbnail, Sense::click());
                self.preview.picture(ui, picture, &self.history.project, accent, self.revision, self.playhead);
                if open.on_hover_text("Expand preview").clicked() { self.preview_expanded = true; }
                let duration = self.history.project.duration();
                if ui.add_enabled(duration > 0, egui::Button::new(if self.preview.playing() { "Pause" } else { "Play" })).clicked() { self.toggle_play(ui.ctx()); }
                if ui.add_enabled(duration > 0, egui::Button::new(clock(self.playhead))).on_hover_text("Go to time…").clicked() {
                    self.seek_input = Some((String::new(), true));
                }
                if ui.small_button("Expand").clicked() { self.preview_expanded = true; }
            });
        } else {
            let maximum = (available - 138.0).max(123.0);
            let preferred = self.compact_preview_height.unwrap_or(available * 0.40);
            let height = preferred.clamp(123.0, maximum);
            let (stage, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
            region(ui, "editor-stage", stage, |ui| self.stage_card(ui, accent));
            let (grip, drag) = ui.allocate_exact_size(egui::vec2(width, 13.0), Sense::click_and_drag());
            let drag = drag.on_hover_cursor(egui::CursorIcon::ResizeVertical)
                .on_hover_text("Drag to resize the preview. Double-click to reset.");
            ui.painter().hline(grip.x_range(), grip.center().y, egui::Stroke::new(1.0, theme::BORDER));
            let center = egui::Rect::from_center_size(grip.center(), egui::vec2(38.0, 3.0));
            ui.painter().rect_filled(center, 2.0, theme::TEXT_FAINT);
            if drag.double_clicked() { self.compact_preview_height = None; }
            else if drag.dragged() { self.compact_preview_height = Some((height + drag.drag_delta().y).clamp(123.0, maximum)); }
        }
        if self.dock_open {
            let (dock, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
            region(ui, "editor-compact-dock", dock, |ui| self.dock_card(ui, frame, false));
        } else {
            egui::ScrollArea::vertical().id_salt("editor-compact-timeline").auto_shrink([false, false])
                .show(ui, |ui| self.timeline(ui, accent));
        }
    }

    fn upper(&mut self, ui: &mut Ui, frame: &eframe::Frame, accent: Color32) {
        let width = ui.available_width();
        if self.preview_expanded {
            let height = (ui.clip_rect().height() * 0.80).clamp(220.0, 565.0);
            let (stage, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
            region(ui, "editor-stage", stage, |ui| self.stage_card(ui, accent));
            if self.dock_open {
                ui.add_space(6.0);
                let (dock, _) = ui.allocate_exact_size(egui::vec2(width, 230.0), Sense::hover());
                region(ui, "editor-compact-dock", dock, |ui| self.dock_card(ui, frame, false));
            }
            return;
        }

        let height = (ui.clip_rect().height() * 0.43).clamp(222.0, 328.0);
        let (row, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
        let gap = 9.0;
        if width >= 900.0 {
            let bin_w = (width * 0.21).clamp(190.0, 268.0);
            let insp_w = (width * 0.23).clamp(226.0, 302.0);
            let bin = Rect::from_min_size(row.min, egui::vec2(bin_w, height));
            let inspector = Rect::from_min_size(egui::pos2(row.right() - insp_w, row.top()), egui::vec2(insp_w, height));
            let stage = Rect::from_min_max(bin.right_top() + egui::vec2(gap, 0.0), inspector.left_bottom() - egui::vec2(gap, 0.0));
            region(ui, "editor-bin", bin, |ui| card(ui, |ui| self.bin(ui, frame)));
            region(ui, "editor-stage", stage, |ui| self.stage_card(ui, accent));
            region(ui, "editor-inspector", inspector, |ui| card(ui, |ui| self.inspector(ui)));
        } else {
            let dock_w = (width * 0.36).clamp(220.0, 290.0);
            let dock = Rect::from_min_size(egui::pos2(row.right() - dock_w, row.top()), egui::vec2(dock_w, height));
            let stage = Rect::from_min_max(row.min, dock.left_bottom() - egui::vec2(gap, 0.0));
            region(ui, "editor-stage", stage, |ui| self.stage_card(ui, accent));
            region(ui, "editor-dock", dock, |ui| self.dock_card(ui, frame, true));
        }
    }

    fn dock_card(&mut self, ui: &mut Ui, frame: &eframe::Frame, show_switcher: bool) {
        card(ui, |ui| {
            if show_switcher {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.dock, Dock::Media, "Media Bin");
                    ui.selectable_value(&mut self.dock, Dock::Inspector, "Inspector");
                });
                ui.separator();
            }
            match self.dock {
                Dock::Media => self.bin(ui, frame),
                Dock::Inspector => self.inspector(ui),
            }
        });
    }

    fn stage_card(&mut self, ui: &mut Ui, accent: Color32) {
        card(ui, |ui| {
            let duration = self.history.project.duration();
            ui.horizontal(|ui| {
                let audio_only = !self.history.project.sounds.is_empty() && self.history.project.visual_duration() == 0;
                ui.strong(if audio_only { "Preview · Audio only" } else { "Preview" });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let action = if self.preview_expanded { "Restore Preview" } else { "Expand Preview" };
                    let label = if ui.available_width() < 238.0 {
                        if self.preview_expanded { "Restore" } else { "Expand" }
                    } else { action };
                    let button = ui.button(label);
                    button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, action));
                    if button.on_hover_text(action).clicked() { self.preview_expanded = !self.preview_expanded; }
                    let canvas = self.history.project.canvas;
                    let mut fps = canvas.fps;
                    let source_rate = self.selected_asset.and_then(|id| self.history.project.asset(id))
                        .and_then(|asset| asset.info.fps_milli);
                    ui.menu_button(format!("{fps} fps"), |ui| {
                        ui.label("Project frame rate");
                        if let Some(rate) = source_rate {
                            if [24_000, 25_000, 30_000, 60_000].contains(&rate) {
                                if ui.button(format!("Match selected source · {} fps", rate / 1000)).clicked() {
                                    fps = rate / 1000;
                                    ui.close();
                                }
                            } else { ui.label(format!("Selected source: {} fps", format_fps(rate))); }
                        }
                        for value in [24, 25, 30, 60] {
                            if ui.selectable_value(&mut fps, value, format!("{value} fps")).clicked() { ui.close(); }
                        }
                    }).response.on_hover_text("Change project frame rate");
                    if fps != canvas.fps {
                        self.change(ui.ctx(), |p| { p.canvas.fps = fps; Ok(()) });
                    }
                    if ui.available_width() >= 68.0 {
                        ui.label(theme::small(format!("{}×{}", canvas.width, canvas.height)));
                    }
                });
            });
            let transport = if ui.available_width() < 340.0 { 57.0 } else { 33.0 };
            let scrub_height = if self.preview_expanded { 26.0 } else { 0.0 };
            let height = (ui.available_height() - transport - scrub_height - 5.0).max(42.0);
            let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, duration > 0, "Toggle preview playback"));
            self.preview.picture(ui, rect, &self.history.project, accent, self.revision, self.playhead);
            if response.clicked() { self.toggle_play(ui.ctx()); }
            if self.preview_expanded {
                ui.scope(|ui| {
                    ui.spacing_mut().slider_width = ui.available_width();
                    let mut at = seconds(self.playhead);
                    let bar = egui::Slider::new(&mut at, 0.0..=seconds(duration)).show_value(false);
                    let response = ui.add_enabled(duration > 0, bar).on_hover_text("Scrub the preview");
                    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Slider, duration > 0, "Preview playhead"));
                    if response.changed() { self.seek(ticks(at), ui.ctx()); }
                });
            }
            ui.horizontal_wrapped(|ui| {
                let playing = self.preview.playing();
                if ui.add_enabled(duration > 0, egui::Button::new(if playing { "Pause" } else { "Play" }))
                    .on_hover_text("Space").clicked()
                {
                    self.toggle_play(ui.ctx());
                }
                for (label, delta) in [("Previous Frame", -1), ("Next Frame", 1)] {
                    let button = ui.add_enabled(duration > 0, egui::Button::new(if delta < 0 { "‹│" } else { "│›" }));
                    button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, duration > 0, label));
                    if button.on_hover_text(label).clicked() {
                        self.step(delta, ui.ctx());
                    }
                }
                let position = clock(self.playhead);
                let shown = format!("{} / {}", position, clock(duration));
                let goto = ui.add_enabled(duration > 0, egui::Button::new(RichText::new(shown).monospace()).truncate())
                    .on_hover_text(format!("Go to time… · Frame {}", frame_number(self.playhead, self.history.project.canvas.fps) + 1));
                if goto.clicked() { self.seek_input = Some((String::new(), true)); }
            });
        });
    }

    fn seek_dialog(&mut self, ctx: &egui::Context) {
        let Some((text, first)) = &mut self.seek_input else { return };
        let mut open = true;
        let mut target = None;
        let duration = self.history.project.duration();
        egui::Window::new("Go to time").open(&mut open).collapsible(false).resizable(false)
            .show(ctx, |ui| {
                ui.label("Seconds or minutes:seconds");
                let edit = ui.add(egui::TextEdit::singleline(text).hint_text("1.88 or 01:23.400").desired_width(180.0));
                if *first { edit.request_focus(); *first = false; }
                let parsed = parse_time(text).map(ticks).filter(|&at| at <= duration);
                let go = ui.add_enabled(parsed.is_some(), egui::Button::new("Go"));
                if go.clicked() || ui.input(|i| i.key_pressed(Key::Enter)) { target = parsed; }
                if !text.is_empty() && parsed.is_none() { ui.label(format!("Enter a time up to {}.", clock(duration))); }
            });
        if let Some(at) = target { self.seek(at, ctx); }
        if target.is_some() || !open || ctx.input(|i| i.key_pressed(Key::Escape)) { self.seek_input = None; }
    }

    fn bin(&mut self, ui: &mut Ui, frame: &eframe::Frame) {
        ui.strong("Media Bin");
        if self.history.project.assets.is_empty() {
            ui.add(egui::Label::new("Import video, images or audio, or drop files here.").wrap());
            if ui.add_enabled(self.imports.is_none(), egui::Button::new("Import Media…")).clicked() {
                self.pick_media(frame, ui.ctx());
            }
            return;
        }

        let mut add = None;
        let mut relink = None;
        let hovered_file = ui.input(|i| !i.raw.hovered_files.is_empty());
        if hovered_file {
            ui.label("Drop files to import");
        }
        let selected = self.selected_asset.and_then(|id| self.history.project.asset(id));
        let compact = ui.ctx().viewport_rect().height() < 590.0 && ui.available_width() < 600.0;
        ui.horizontal_wrapped(|ui| {
            if let Some(asset) = selected.filter(|asset| asset.info.kind == AssetKind::Audio && compact) {
                egui::ComboBox::from_id_salt(("editor-bin-role-compact", asset.id)).width(65.0)
                    .selected_text(role_name(self.audio_role)).show_ui(ui, |ui| {
                        for role in [SoundRole::Voice, SoundRole::Music, SoundRole::Effect] {
                            ui.selectable_value(&mut self.audio_role, role, role_name(role));
                        }
                    });
            }
            let eligible = selected.is_some_and(|asset| !self.availability.missing(asset.id));
            if ui.add_enabled(eligible, egui::Button::new("Add to Timeline")).clicked() { add = self.selected_asset; }
            if ui.add_enabled(selected.is_some() && self.relink_task.is_none(), egui::Button::new("Relink Media…")).clicked() {
                relink = self.selected_asset;
            }
        });
        if selected.is_some_and(|asset| asset.info.kind == AssetKind::Audio) && !compact {
            role_picker(ui, "editor-bin-role", &mut self.audio_role);
        }
        ui.separator();
        let assets = &self.history.project.assets;
        egui::ScrollArea::vertical().id_salt("editor-media-list").max_height(ui.available_height().max(1.0)).show_rows(ui, 35.0, assets.len(), |ui, rows| {
            for asset in &assets[rows] {
                let missing = self.availability.missing(asset.id);
                let response = media_row(ui, asset, self.selected_asset == Some(asset.id), missing);
                response.dnd_set_drag_payload(timeline::BinAsset(asset.id));
                if response.clicked() || response.drag_started() { self.selected_asset = Some(asset.id); }
                if response.double_clicked() && !missing { add = Some(asset.id); }
            }
        });

        if let Some(id) = add { self.place_asset(id, None, None, ui.ctx()); }
        if let Some(id) = relink { self.relink(id, frame, ui.ctx()); }
    }

    fn import_files(&mut self, files: Vec<PathBuf>, ctx: &egui::Context) {
        if self.imports.is_some() || files.is_empty() { return; }
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        let task = std::thread::Builder::new().name("clipvault-editor-import".into()).spawn(move || {
            let result = files.into_iter().map(|path| {
                let info = cv_media::editor::probe(&path);
                (path, info)
            }).collect();
            let _ = tx.send(result);
            wake.request_repaint();
        });
        match task {
            Ok(_) => self.imports = Some(rx),
            Err(error) => self.notice = Some(Notice::Error(format!("Could not start import: {error}"))),
        }
    }

    pub fn dropped_files(&mut self, files: Vec<PathBuf>, ctx: &egui::Context) {
        if self.tab == Tab::Workbench { self.import_files(files, ctx); }
    }

    fn finish_import(&mut self, ctx: &egui::Context) {
        let Some(task) = &self.imports else { return; };
        let files = match task.try_recv() {
            Ok(files) => files,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.imports = None;
                self.notice = Some(Notice::Error("Import worker stopped".into()));
                return;
            }
        };
        self.imports = None;
        let mut errors = Vec::new();
        let mut imported_message = None;
        let mut accepted = Vec::new();
        for (path, result) in files {
            match result {
                Ok(info) => accepted.push((path, info)),
                Err(error) => {
                    let name = filename(&path);
                    let reason = error.root_cause().to_string();
                    let problem = if path.is_file() { "Unsupported or unreadable media" } else { "Missing file" };
                    errors.push(format!("{problem} · {name}. {reason}"));
                }
            }
        }
        if !accepted.is_empty() {
            let total = accepted.len();
            let before = self.history.project.assets.len();
            if let Some(first) = self.change(ctx, |project| {
                let mut first = None;
                for (path, info) in accepted {
                    let id = project.import(path, info)?;
                    if first.is_none() { first = Some(id); }
                }
                first.context("No media selected")
            }) {
                self.selected_asset = Some(first);
                let added = self.history.project.assets.len().saturating_sub(before);
                let info = match (added, total - added) {
                    (0, _) => "Already in the Media Bin.".to_owned(),
                    (n, 0) => format!("Imported {n} media {}", if n == 1 { "file" } else { "files" }),
                    (n, seen) => format!("Imported {n} · {seen} already in the Media Bin"),
                };
                imported_message = Some(info);
            }
        }
        if errors.is_empty() {
            if let Some(message) = imported_message { self.notice = Some(Notice::Info(message)); }
        } else {
            let prefix = imported_message.map_or_else(String::new, |message| format!("{message} · "));
            self.notice = Some(Notice::Error(format!("{prefix}Skipped {} · {}", errors.len(), errors.join("\n"))));
        }
    }

    fn pick_media(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        self.pause();
        if let Some(paths) = qa_driver::pick_files(|| media_dialog(frame, "Import Media").pick_files()) {
            self.import_files(paths, ctx);
        }
    }

    fn relink(&mut self, id: Id, frame: &eframe::Frame, ctx: &egui::Context) {
        self.pause();
        if let Some(path) = qa_driver::pick_file(|| media_dialog(frame, "Relink Media").pick_file()) {
            let (tx, rx) = mpsc::channel();
            let wake = ctx.clone();
            let task = std::thread::Builder::new().name("clipvault-editor-relink".into()).spawn(move || {
                let result = cv_media::editor::probe(&path).map(|info| (path, info));
                let _ = tx.send(result);
                wake.request_repaint();
            });
            match task {
                Ok(_) => self.relink_task = Some((id, rx)),
                Err(error) => self.notice = Some(Notice::Error(format!("Could not start relink: {error}"))),
            }
        }
    }

    fn finish_relink(&mut self, ctx: &egui::Context) {
        let Some((id, task)) = &self.relink_task else { return; };
        let id = *id;
        let result = match task.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("Relink worker stopped")),
        };
        self.relink_task = None;
        match result {
            Ok((path, info)) => { self.change(ctx, |project| project.relink(id, path, info)); }
            Err(error) => self.notice = Some(Notice::Error(format!("Relink failed: {error:#}"))),
        }
    }

    fn place_asset(&mut self, id: Id, at: Option<u64>, drop_role: Option<SoundRole>, ctx: &egui::Context) {
        let Some(kind) = self.history.project.asset(id).map(|asset| asset.info.kind) else { return; };
        match (kind, drop_role) {
            (AssetKind::Audio, _) => {
                let role = drop_role.unwrap_or(self.audio_role);
                let at = at.unwrap_or(self.playhead);
                if let Some(id) = self.change(ctx, |project| project.add_sound(id, role, at)) {
                    self.selected = Some(Selection::Sound(id));
                }
            }
            (_, Some(_)) => self.notice = Some(Notice::Error("Drop video and images on the Video lane".into())),
            (_, None) => {
                let index = at.map(|at| self.insertion_index(at, None));
                if let Some(id) = self.change(ctx, |project| {
                    let id = project.append(id)?;
                    if let Some(index) = index { project.move_shot(id, index)?; }
                    Ok(id)
                }) {
                    self.selected = Some(Selection::Shot(id));
                }
            }
        }
    }

    fn insertion_index(&self, at: u64, skip: Option<Id>) -> usize {
        let mut start = 0;
        let mut index = 0;
        for shot in &self.history.project.shots {
            let length = shot.range.end - shot.range.start;
            if Some(shot.id) != skip && at >= start + length / 2 {
                index += 1;
            }
            start += length;
        }
        index
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        if self.history.project.duration() == 0 { return; }
        if self.preview.playing() {
            self.pause();
        } else {
            if self.playhead >= self.history.project.duration() { self.playhead = 0; }
            self.preview.start(ctx, &self.history.project, self.revision, self.playhead);
        }
    }

    fn seek(&mut self, tick: u64, ctx: &egui::Context) {
        self.playhead = tick.min(self.history.project.duration());
        self.preview.seek(ctx, &self.history.project, self.revision, self.playhead);
    }

    fn step(&mut self, delta: i64, ctx: &egui::Context) {
        let fps = self.history.project.canvas.fps;
        let frame = frame_number(self.playhead, fps).saturating_add_signed(delta);
        self.seek(frame_tick(frame, fps), ctx);
    }

    fn dirty(&self) -> bool {
        self.unsaved
    }

    fn change<T>(&mut self, ctx: &egui::Context, edit: impl FnOnce(&mut Project) -> Result<T>) -> Option<T> {
        match self.history.edit(edit) {
            Ok(result) => {
                self.changed(ctx);
                self.notice = None;
                Some(result)
            }
            Err(error) => {
                self.notice = Some(Notice::Error(format!("{error:#}")));
                None
            }
        }
    }

    fn changed(&mut self, ctx: &egui::Context) {
        self.unsaved = self.history.project != self.saved;
        self.revision = self.revision.wrapping_add(1);
        let project = &self.history.project;
        self.playhead = self.playhead.min(project.duration());
        self.selected = self.selected.filter(|selection| match selection {
            Selection::Shot(id) => project.shots.iter().any(|shot| shot.id == *id),
            Selection::Sound(id) => project.sounds.iter().any(|sound| sound.id == *id),
        });
        self.selected_asset = self.selected_asset.filter(|&id| project.asset(id).is_some());
        self.availability.sync(&project.assets);
        self.preview.invalidate(ctx);
    }

    fn undo(&mut self, ctx: &egui::Context) {
        if self.history.undo() { self.changed(ctx); }
    }

    fn redo(&mut self, ctx: &egui::Context) {
        if self.history.redo() { self.changed(ctx); }
    }

    fn request_replace(&mut self, action: Replace, frame: &eframe::Frame, ctx: &egui::Context) {
        self.pause();
        if self.dirty() {
            self.notice = None;
            self.replace = Some(action);
        } else {
            self.replace_project(action, frame, ctx);
        }
    }

    fn replace_project(&mut self, action: Replace, frame: &eframe::Frame, ctx: &egui::Context) {
        let (project, path) = match action {
            Replace::Close => {
                self.close_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            Replace::New => (Project::default(), None),
            Replace::Open => {
                let Some(path) = qa_driver::pick_file(|| {
                    project_dialog(frame, "Open Project", self.file.as_deref()).pick_file()
                }) else { return; };
                match Project::load(&path) {
                    Ok(project) => (project, Some(path)),
                    Err(error) => {
                        self.notice = Some(Notice::Error(format!("Could not open project: {error:#}")));
                        return;
                    }
                }
            }
        };
        self.saved = project.clone();
        self.unsaved = false;
        self.history = History::new(project);
        self.availability.sync(&self.history.project.assets);
        self.availability.refresh();
        self.file = path;
        self.selected = None;
        self.selected_asset = None;
        self.playhead = 0;
        self.imports = None;
        self.relink_task = None;
        self.inspector = inspector::Inspector::default();
        self.captions = captions::Captions::default();
        self.seek_input = None;
        self.error_details = None;
        self.generator = None;
        self.export.clear_result();
        self.last_export = None;
        self.timeline = timeline::Timeline::default();
        self.notice = None;
        self.revision = self.revision.wrapping_add(1);
        self.preview.reset();
    }

    fn replace_prompt(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(action) = self.replace else { return; };
        let modal = egui::Modal::new(egui::Id::unique("editor-unsaved")).show(ctx, |ui| {
            ui.set_max_width(255.0);
            ui.heading("Save changes?");
            let closing = matches!(action, Replace::Close);
            ui.label(if closing { "Save the current project before closing ClipVault." }
                else { "Save the current project before continuing." });
            if let Some(Notice::Error(error)) = &self.notice {
                ui.add(egui::Label::new(RichText::new(error).color(theme::DANGER)).wrap());
            }
            ui.horizontal_wrapped(|ui| {
                let save = if closing { "Save and Close" } else { "Save and Continue" };
                if ui.button(save).clicked() && self.save(false, frame) {
                    self.replace = None;
                    self.replace_project(action, frame, ctx);
                }
                if ui.button("Discard Changes").clicked() {
                    self.replace = None;
                    self.replace_project(action, frame, ctx);
                }
                if ui.button("Cancel").clicked() { self.replace = None; }
            });
        });
        if modal.should_close() { self.replace = None; }
    }

    fn save(&mut self, as_new: bool, frame: &eframe::Frame) -> bool {
        self.pause();
        let result = self.save_project(as_new, frame);
        match result {
            Ok(Some(path)) => {
                self.notice = Some(Notice::Info(format!("Saved {}", filename(&path))));
                self.saved = self.history.project.clone();
                self.unsaved = false;
                self.file = Some(path);
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.notice = Some(Notice::Error(format!("Could not save project: {error:#}")));
                false
            }
        }
    }

    fn save_project(&self, as_new: bool, frame: &eframe::Frame) -> Result<Option<PathBuf>> {
        if let Some(path) = self.file.as_ref().filter(|_| !as_new) {
            self.history.project.save(path)?;
            return Ok(Some(path.clone()));
        }
        let Some(path) = qa_driver::save_file(|| {
            project_dialog(frame, "Save Project As", self.file.as_deref())
                .set_file_name(format!("{}.{}", safe_name(&self.history.project.name), PROJECT_EXTENSION))
                .save_file()
        }) else { return Ok(None); };
        let path = extension(path, PROJECT_EXTENSION)?;
        preview::create_new(&path, |staging| self.history.project.save(staging))?;
        Ok(Some(path))
    }

    fn export(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        self.pause();
        let project = &self.history.project;
        let pictures = project.shots.iter().filter_map(|shot| match shot.source {
            cv_core::editor::Source::Asset(id) => Some(id), _ => None,
        });
        if let Some(id) = pictures.chain(project.sounds.iter().map(|sound| sound.asset))
            .find(|&id| self.availability.missing(id))
        {
            self.export.clear_result();
            self.selected_asset = Some(id);
            self.dock = Dock::Media;
            self.dock_open = true;
            self.notice = Some(Notice::MissingMedia(id));
            return;
        }
        let projects = cv_core::paths::data_dir();
        let folder = self.last_export.as_deref().or(self.file.as_deref()).and_then(Path::parent).unwrap_or(&projects);
        let preferred = self.last_export.clone().unwrap_or_else(|| folder.join(format!("{}.mp4", safe_name(&self.history.project.name))));
        let Some(path) = qa_driver::save_file(|| {
            rfd::FileDialog::new().set_parent(frame).set_title("Export MP4")
                .set_directory(folder).add_filter("MP4 video", &["mp4"])
                .set_file_name(suggest_output(&preferred, |p| p.exists())).save_file()
        }) else { return; };
        let result = extension(path, "mp4").and_then(|path| {
            self.last_export = Some(path.clone());
            self.export.start(ctx, self.history.project.clone(), path)
        });
        self.notice = result.err().map(|error| Notice::ExportError(format!("Could not export: {error:#}")));
    }

    fn start_generator(&mut self, kind: Generated) {
        self.pause();
        self.generator = Some(Generator { kind, at_start: self.playhead == 0, text: "Title".into(), rgb: [38; 3], duration: 2.0 });
    }

    fn generator_prompt(&mut self, ctx: &egui::Context) {
        let Some(draft) = &mut self.generator else { return; };
        let mut accept = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("editor-generator")).show(ctx, |ui| {
            ui.set_max_width(250.0);
            match draft.kind {
                Generated::Title => {
                    ui.heading("Add Title");
                    ui.add(egui::TextEdit::singleline(&mut draft.text).hint_text("Title text"));
                }
                Generated::Color => {
                    ui.heading("Add Color");
                    ui.color_edit_button_srgb(&mut draft.rgb);
                }
            }
            ui.horizontal(|ui| {
                ui.label("Duration");
                ui.add(egui::DragValue::new(&mut draft.duration).range(0.04..=600.0).speed(0.1).suffix(" s"));
            });
            ui.horizontal(|ui| {
                ui.label("Placement");
                ui.selectable_value(&mut draft.at_start, true, "Beginning");
                ui.selectable_value(&mut draft.at_start, false, "End");
            });
            ui.horizontal(|ui| {
                accept = ui.button("Add to Timeline").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if cancel || modal.should_close() { self.generator = None; }
        if accept {
            let Some(draft) = self.generator.take() else { return; };
            let duration = ticks(draft.duration);
            if let Some(id) = self.change(ctx, |project| {
                let id = match draft.kind {
                    Generated::Title => project.title(draft.text, duration)?,
                    Generated::Color => project.color(draft.rgb, duration)?,
                };
                if draft.at_start { project.move_shot(id, 0)?; }
                Ok(id)
            }) {
                self.selected = Some(Selection::Shot(id));
                let at = self.history.project.shot_start(id).unwrap_or(0);
                self.seek(at, ctx);
            }
        }
    }

    fn shortcuts(&mut self, ui: &Ui, frame: &eframe::Frame) {
        if self.replace.is_some() || self.generator.is_some() || self.captions.open || ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        use egui::Modifiers;
        let ctx = ui.ctx();
        for (modifiers, key) in [
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::S),
            (Modifiers::COMMAND, Key::S),
            (Modifiers::COMMAND, Key::O),
            (Modifiers::COMMAND, Key::N),
            (Modifiers::COMMAND, Key::I),
            (Modifiers::COMMAND | Modifiers::SHIFT, Key::Z),
            (Modifiers::COMMAND, Key::Z),
            (Modifiers::COMMAND, Key::Y),
            (Modifiers::NONE, Key::Space),
            (Modifiers::NONE, Key::Comma),
            (Modifiers::NONE, Key::Period),
            (Modifiers::NONE, Key::S),
            (Modifiers::NONE, Key::Delete),
        ] {
            if !ctx.input_mut(|input| input.consume_key(modifiers, key)) { continue; }
            match key {
                Key::S if modifiers.shift => { self.save(true, frame); }
                Key::S if modifiers.command => { self.save(false, frame); }
                Key::O => self.request_replace(Replace::Open, frame, ctx),
                Key::N => self.request_replace(Replace::New, frame, ctx),
                Key::I => self.pick_media(frame, ctx),
                Key::Z if modifiers.shift => self.redo(ctx),
                Key::Z => self.undo(ctx),
                Key::Y => self.redo(ctx),
                Key::Space => self.toggle_play(ctx),
                Key::Comma => self.step(-1, ctx),
                Key::Period => self.step(1, ctx),
                Key::S => self.split(ctx),
                Key::Delete => self.remove_selection(ctx),
                _ => {}
            }
        }
    }
}

fn card(ui: &mut Ui, draw: impl FnOnce(&mut Ui)) {
    let size = ui.available_size();
    egui::Frame::NONE.fill(theme::SURFACE).corner_radius(7)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(Margin::same(9)).show(ui, |ui| {
            ui.set_min_size((size - egui::vec2(20.0, 20.0)).max(Vec2::ZERO));
            draw(ui);
        });
}

fn parse_time(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('s').trim();
    let parts: Vec<_> = text.split(':').collect();
    if parts.is_empty() || parts.len() > 3 { return None; }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit() || (c == b'.' && i + 1 == parts.len())) {
            return None;
        }
        let value: f64 = part.parse().ok()?;
        if !value.is_finite() || (i > 0 && value >= 60.0) { return None; }
        total = 60.0 * total + value;
    }
    total.is_finite().then_some(total)
}

fn format_fps(milli: u32) -> String {
    format!("{:.3}", f64::from(milli) / 1000.0).trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn media_row(ui: &mut Ui, asset: &Asset, selected: bool, missing: bool) -> egui::Response {
    let name = filename(&asset.path);
    let (kind, icon) = match asset.info.kind {
        AssetKind::Video => ("Video", theme::icon::PLAY),
        AssetKind::Image => ("Still image", theme::icon::LIBRARY),
        AssetKind::Audio => ("Audio", theme::icon::VOLUME),
    };
    let meta = if missing {
        format!("{kind} · Missing")
    } else if asset.info.kind == AssetKind::Image {
        kind.to_owned()
    } else if let Some(fps) = asset.info.fps_milli {
        format!("{kind} · {} · {} fps", clock(asset.info.duration), format_fps(fps))
    } else {
        format!("{kind} · {}", clock(asset.info.duration))
    };
    let response = ui.push_id(asset.id, |ui| {
        let size = egui::vec2(ui.available_width(), 35.0);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, format!("{name}, {meta}")));
        let painter = ui.painter_at(rect);
        let fill = if selected { theme::SURFACE3 } else if response.hovered() { theme::SURFACE2 } else { Color32::TRANSPARENT };
        let border = if response.has_focus() { egui::Stroke::new(1.5, theme::TEXT) } else { egui::Stroke::NONE };
        painter.rect(rect, 5.0, fill, border, egui::StrokeKind::Inside);
        painter.text(rect.left_center() + egui::vec2(14.0, 0.0), egui::Align2::CENTER_CENTER, icon,
            egui::FontId::new(14.0, theme::icons_family()), theme::TEXT_DIM);
        let x = rect.left() + 27.0;
        let mut text = egui::text::LayoutJob::simple(name, egui::FontId::proportional(13.0), theme::TEXT,
            (rect.right() - x - 4.0).max(1.0));
        text.wrap.max_rows = 1;
        text.wrap.break_anywhere = true;
        painter.galley(egui::pos2(x, rect.top() + 3.0), painter.layout_job(text), theme::TEXT);
        painter.text(egui::pos2(x, rect.bottom() - 3.0), egui::Align2::LEFT_BOTTOM, &meta,
            egui::FontId::proportional(10.5), theme::TEXT_DIM);
        response
    }).inner;
    response.on_hover_text(format!("{meta}\n{}", asset.path.display()))
}

fn project_dialog(frame: &eframe::Frame, title: &str, current: Option<&Path>) -> rfd::FileDialog {
    let base = cv_core::paths::data_dir();
    rfd::FileDialog::new().set_parent(frame).set_title(title)
        .set_directory(current.and_then(Path::parent).unwrap_or(&base))
        .add_filter("ClipVault Project", &[PROJECT_EXTENSION])
}

fn media_dialog(frame: &eframe::Frame, title: &str) -> rfd::FileDialog {
    rfd::FileDialog::new().set_parent(frame).set_title(title)
        .add_filter("Editor media", &["mp4", "m4v", "mov", "wav", "mp3", "png", "jpg", "jpeg"])
        .add_filter("All files", &["*"])
}

fn extension(path: PathBuf, expected: &str) -> Result<PathBuf> {
    match path.extension() {
        None => Ok(path.with_extension(expected)),
        Some(ext) if ext.eq_ignore_ascii_case(expected) => Ok(path),
        _ => bail!("Use a .{expected} filename"),
    }
}

fn filename(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

fn suggest_output(path: &Path, occupied: impl Fn(&Path) -> bool) -> String {
    if !occupied(path) { return filename(path); }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let (base, first) = stem.strip_suffix(')').and_then(|text| text.rsplit_once(" ("))
        .and_then(|(base, number)| Some((base, number.parse::<u32>().ok()?.checked_add(1)?)))
        .unwrap_or((&stem, 2));
    for version in first..first.saturating_add(1000) {
        let name = format!("{base} ({version}).mp4");
        if !occupied(&path.with_file_name(&name)) { return name; }
    }
    format!("{base} {}.mp4", chrono::Local::now().format("%Y%m%d-%H%M%S"))
}

fn safe_name(name: &str) -> String {
    let name: String = name.chars().take(80).map(|c| {
        if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }
    }).collect();
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() { "ClipVault Edit".into() } else { name.into() }
}

fn ticks(seconds: f64) -> u64 {
    (seconds * TICKS_PER_SECOND as f64).round() as u64
}

fn seconds(tick: u64) -> f64 {
    tick as f64 / TICKS_PER_SECOND as f64
}

fn clock(tick: u64) -> String {
    let ms = tick / (TICKS_PER_SECOND / 1000);
    if ms >= 3_600_000 {
        format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    } else {
        format!("{:02}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
    }
}

fn frame_number(tick: u64, fps: u32) -> u64 {
    ((tick as u128 * fps as u128 + TICKS_PER_SECOND as u128 / 2) / TICKS_PER_SECOND as u128) as u64
}

fn frame_tick(frame: u64, fps: u32) -> u64 {
    ((frame as u128 * TICKS_PER_SECOND as u128 + fps as u128 / 2) / fps.max(1) as u128) as u64
}

fn role_name(role: SoundRole) -> &'static str {
    match role {
        SoundRole::Music => "Music",
        SoundRole::Voice => "Voice",
        SoundRole::Effect => "Effects",
    }
}

#[cfg(test)]
mod workbench_tests {
    use super::*;

    #[test]
    fn accepts_seconds_and_clock_values() {
        assert_eq!(parse_time("1:23.400"), Some(83.4));
        assert_eq!(parse_time("1.880 s"), Some(1.88));
        assert_eq!(parse_time("1:00:01.25"), Some(3601.25));
        assert_eq!(parse_time("1:60"), None);
        assert_eq!(parse_time("-3"), None);
    }

    #[test]
    fn export_dialog_suggests_an_available_version() {
        let occupied = |p: &Path| ["Game.mp4", "Game (2).mp4"].iter().any(|name| p.ends_with(name));
        assert_eq!(suggest_output(Path::new("Game.mp4"), occupied), "Game (3).mp4");
        assert_eq!(suggest_output(Path::new("Game (2).mp4"), occupied), "Game (3).mp4");
        assert_eq!(suggest_output(Path::new("Fresh.mp4"), occupied), "Fresh.mp4");
    }

    #[test]
    fn undo_to_saved_project_clears_unsaved_state() {
        let mut editor = Workbench::default();
        let ctx = egui::Context::default();
        assert!(!editor.dirty());
        editor.change(&ctx, |p| p.color([15, 30, 45], TICKS_PER_SECOND)).unwrap();
        assert!(editor.dirty());
        editor.undo(&ctx);
        assert!(!editor.dirty());
        editor.redo(&ctx);
        assert!(editor.dirty());
    }
}

fn role_picker(ui: &mut Ui, id: &'static str, role: &mut SoundRole) {
    ui.push_id(id, |ui| ui.horizontal_wrapped(|ui| {
        for r in [SoundRole::Music, SoundRole::Voice, SoundRole::Effect] {
            ui.selectable_value(role, r, role_name(r));
        }
    }));
}
