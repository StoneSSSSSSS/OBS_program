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
    text: String,
    rgb: [u8; 3],
    duration: f64,
}

enum Notice {
    Info(String),
    Error(String),
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
    selected_asset: Option<Id>,
    selected: Option<Selection>,
    audio_role: SoundRole,
    timeline: timeline::Timeline,
    inspector: inspector::Inspector,
    playhead: u64,
    preview: preview::Preview,
    export: preview::Export,
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
            selected_asset: None,
            selected: None,
            audio_role: SoundRole::Music,
            timeline: timeline::Timeline::default(),
            inspector: inspector::Inspector::default(),
            playhead: 0,
            preview: preview::Preview::default(),
            export: preview::Export::default(),
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
                self.dock_button(ui, Dock::Media, "Media Bin");
                self.dock_button(ui, Dock::Inspector, "Inspector");
            });
        }
        self.output_status(ui);
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("editor-workbench")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.upper(ui, frame, ctx.accent);
                ui.add_space(10.0);
                self.timeline(ui, ctx.accent);
            });

        self.shortcuts(ui, frame);
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

    fn output_status(&mut self, ui: &mut Ui) {
        let mut row = false;
        if let Some(status) = self.export.status() {
            row = true;
            ui.horizontal_wrapped(|ui| match status {
                preview::ExportStatus::Running(progress) => {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                    ui.add(egui::ProgressBar::new(progress).desired_width(104.0).show_percentage());
                    if ui.button("Cancel Export").clicked() {
                        self.export.cancel();
                    }
                }
                preview::ExportStatus::Saved(path) => {
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
                    ui.add(egui::Label::new(RichText::new(&error).color(theme::DANGER)).truncate())
                        .on_hover_text(error);
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
            let (message, color) = match notice {
                Notice::Info(message) => (message, theme::TEXT_DIM),
                Notice::Error(message) => (message, theme::DANGER),
            };
            ui.add(egui::Label::new(RichText::new(message).color(color)).truncate())
                .on_hover_text(message);
        }
        if !row { ui.add_space(17.0); }
    }

    fn dock_button(&mut self, ui: &mut Ui, dock: Dock, label: &str) {
        if ui.selectable_label(self.dock_open && self.dock == dock, label).clicked() {
            self.dock_open = !self.dock_open || self.dock != dock;
            self.dock = dock;
        }
    }

    fn upper(&mut self, ui: &mut Ui, frame: &eframe::Frame, accent: Color32) {
        let width = ui.available_width();
        if width < 600.0 || self.preview_expanded {
            let height = if self.preview_expanded {
                (ui.clip_rect().height() * 0.80).clamp(220.0, 565.0)
            } else {
                (width * 0.48 + 78.0).clamp(188.0, 318.0)
            };
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
            let height = (ui.available_height() - transport - 5.0).max(42.0);
            let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, duration > 0, "Toggle preview playback"));
            self.preview.picture(ui, rect, &self.history.project, accent, self.revision, self.playhead);
            if response.clicked() { self.toggle_play(ui.ctx()); }
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
                ui.add(egui::Label::new(RichText::new(shown).monospace()).truncate())
                    .on_hover_text(format!("Frame {}", frame_number(self.playhead, self.history.project.canvas.fps) + 1));
            });
        });
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
        let selected_audio = self.selected_asset.and_then(|id| self.history.project.asset(id))
            .is_some_and(|asset| asset.info.kind == AssetKind::Audio);
        let actions_height = if selected_audio { 117.0 } else { 79.0 };
        let list_height = (ui.available_height() - actions_height).max(48.0);
        let assets = &self.history.project.assets;
        egui::ScrollArea::vertical().id_salt("editor-media-list").max_height(list_height).show_rows(ui, 35.0, assets.len(), |ui, rows| {
            for asset in &assets[rows] {
                let missing = self.availability.missing(asset.id);
                let response = media_row(ui, asset, self.selected_asset == Some(asset.id), missing);
                response.dnd_set_drag_payload(timeline::BinAsset(asset.id));
                if response.clicked() || response.drag_started() { self.selected_asset = Some(asset.id); }
                if response.double_clicked() && !missing { add = Some(asset.id); }
            }
        });

        if let Some(asset) = self.selected_asset.and_then(|id| self.history.project.asset(id)) {
            let id = asset.id;
            let missing = self.availability.missing(id);
            ui.separator();
            if asset.info.kind == AssetKind::Audio {
                role_picker(ui, "editor-bin-role", &mut self.audio_role);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!missing, egui::Button::new("Add to Timeline")).clicked() { add = Some(id); }
                if ui.add_enabled(self.relink_task.is_none(), egui::Button::new("Relink Media…")).clicked() { relink = Some(id); }
            });
        }
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
        let mut accepted = Vec::new();
        for (path, result) in files {
            match result {
                Ok(info) => accepted.push((path, info)),
                Err(error) => errors.push(format!("{}: {error:#}", filename(&path))),
            }
        }
        if !accepted.is_empty() {
            let count = accepted.len();
            if let Some(first) = self.change(ctx, |project| {
                let mut first = None;
                for (path, info) in accepted {
                    let id = project.import(path, info)?;
                    if first.is_none() { first = Some(id); }
                }
                first.context("No media selected")
            }) {
                self.selected_asset = Some(first);
                self.notice = Some(Notice::Info(format!("Imported {count} media {}", if count == 1 { "file" } else { "files" })));
            }
        }
        if !errors.is_empty() { self.notice = Some(Notice::Error(errors.join("  ·  "))); }
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
        let projects = cv_core::paths::data_dir();
        let Some(path) = qa_driver::save_file(|| {
            rfd::FileDialog::new().set_parent(frame).set_title("Export MP4")
                .set_directory(self.file.as_deref().and_then(Path::parent).unwrap_or(&projects))
                .add_filter("MP4 video", &["mp4"])
                .set_file_name(format!("{}.mp4", safe_name(&self.history.project.name))).save_file()
        }) else { return; };
        let result = extension(path, "mp4").and_then(|path| self.export.start(ctx, self.history.project.clone(), path));
        if let Err(error) = result { self.notice = Some(Notice::Error(format!("Could not export: {error:#}"))); }
    }

    fn start_generator(&mut self, kind: Generated) {
        self.pause();
        self.generator = Some(Generator { kind, text: "Title".into(), rgb: [26, 37, 52], duration: 2.0 });
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
                accept = ui.button("Add to Timeline").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if cancel || modal.should_close() { self.generator = None; }
        if accept {
            let Some(draft) = self.generator.take() else { return; };
            let duration = ticks(draft.duration);
            if let Some(id) = self.change(ctx, |project| match draft.kind {
                Generated::Title => project.title(draft.text, duration),
                Generated::Color => project.color(draft.rgb, duration),
            }) {
                self.selected = Some(Selection::Shot(id));
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
