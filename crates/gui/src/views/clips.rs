//! Clips library (grid with game/category filters) and the clip detail view
//! (player, trim, mixer, export, organise).

use crate::app::Ctx;
use crate::lanes::{self, LaneAction, LaneState, PreviewProgress, RangePreview, WaveSource};
use crate::player::seek_bar;
use crate::theme::{self, icon};
use cv_core::ipc::{ExportSource, Request};
use cv_core::model::{apply_edit, AudioEdit, Category, Clip, ClipFilter, ClipSort, Game};
use cv_core::util::{format_duration, format_duration_precise, human_bytes, now_ms};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

mod timeline;

static OPEN_REQUEST: AtomicI64 = AtomicI64::new(0);

/// Ask the clips page to open a clip (used from Home).
pub fn request_open(clip_id: i64) {
    OPEN_REQUEST.store(clip_id, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq)]
enum Filter {
    All,
    Favorites,
    Recent,
    Uncategorized,
    Game(i64),
    Category(i64),
}

impl Default for Filter {
    fn default() -> Self {
        Filter::All
    }
}

#[derive(Default)]
struct Detail {
    clip: Option<Clip>,
    title_edit: String,
    notes_edit: String,
    notes_dirty: Option<Instant>,
    in_pt: Option<f64>,
    out_pt: Option<f64>,
    confirm_delete: bool,
    new_cat: String,
    mix_dirty: Option<Instant>,
    share_mb: u32,
    /// Non-destructive audio edits (clip time).
    edits: Vec<AudioEdit>,
    edits_dirty: Option<Instant>,
    lanes: LaneState,
    lane_display: lanes::LaneDisplay,
    edit_history: lanes::EditHistory,
    timeline: timeline::ClipTimeline,
    preview: Option<RangePreview>,
    game_search: String,
    /// Where this clip was when the player was last showing it.
    resume_pos: f64,
}

#[derive(Default)]
pub struct ClipsView {
    filter: Filter,
    search: String,
    sort: ClipSort,
    clips: Vec<Clip>,
    games: Vec<Game>,
    cats: Vec<Category>,
    selected: HashSet<i64>,
    dirty: bool,
    loaded: bool,
    card_w: f32,
    open: Option<i64>,
    detail: Detail,
    new_cat: String,
    renaming_cat: Option<(i64, String)>,
    confirm_bulk_delete: bool,
    pub highlight: Option<i64>,
    last_search: String,
}

impl ClipsView {
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn refresh(&mut self, c: &mut Ctx) {
        if self.loaded && !self.dirty && self.search == self.last_search {
            return;
        }
        let mut f = ClipFilter { search: self.search.clone(), sort: self.sort, ..Default::default() };
        match self.filter {
            Filter::Favorites => f.favorites_only = true,
            Filter::Uncategorized => f.uncategorized = true,
            Filter::Game(g) => f.game_id = Some(g),
            Filter::Category(k) => f.category_id = Some(k),
            _ => {}
        }
        if let Some(db) = c.db() {
            let mut clips = db.list_clips(&f).unwrap_or_default();
            if self.filter == Filter::Recent {
                let cutoff = now_ms() - 7 * 86_400_000;
                clips.retain(|c| c.created_at >= cutoff);
            }
            self.clips = clips;
            self.games = db.list_games().unwrap_or_default().into_iter().filter(|g| g.clip_count > 0).collect();
            self.cats = db.list_categories().unwrap_or_default();
        }
        // Refresh the open clip's metadata too.
        if let Some(id) = self.open {
            if let Some(db) = c.db() {
                if let Ok(Some(clip)) = db.clip(id) {
                    if self.detail.clip.as_ref().map(|c| c.path != clip.path || c.title != clip.title).unwrap_or(false) {
                        self.detail.title_edit = clip.title.clone();
                    }
                    self.detail.clip = Some(clip);
                }
            }
        }
        self.selected.retain(|id| self.clips.iter().any(|c| c.id == *id));
        self.loaded = true;
        self.dirty = false;
        self.last_search = self.search.clone();
    }

    pub fn save_position(&mut self, c: &mut Ctx) {
        if let Some(clip) = self.detail.clip.clone() {
            self.flush_audio(c, clip.id);
            self.stop_preview(c);
            if c.player.path.as_deref() == Some(clip.path.as_str()) {
                let pos = c.player.state().time_pos;
                if let Some(db) = c.db() {
                    let _ = db.set_clip_position(clip.id, pos);
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if self.card_w == 0.0 {
            self.card_w = 260.0;
        }
        let req = OPEN_REQUEST.swap(0, Ordering::SeqCst);
        if req != 0 {
            self.open_clip(c, req);
        }
        self.refresh(c);
        self.reclaim_player(c);
        if self.open.is_some() {
            self.detail_ui(ui, c);
        } else {
            self.library_ui(ui, c);
        }
    }

    /// The player is shared with the 24/7 view: if it's showing something
    /// else now, reload this clip (paused where it was).
    fn reclaim_player(&mut self, c: &mut Ctx) {
        let Some(clip) = self.detail.clip.clone() else { return };
        if self.open.is_none() {
            return;
        }
        if c.player.path.as_deref() == Some(clip.path.as_str()) {
            self.detail.resume_pos = c.player.state().time_pos;
            return;
        }
        let mix = if c.cfg.player.remember_clip_mix { clip.mixer.clone() } else { None };
        c.player.open(&clip.path, self.detail.resume_pos, clip.audio_tracks.max(0) as usize, &clip.track_names, mix.as_ref(), false, false, &c.cfg.player);
        c.player.set_edits(self.detail.edits.clone());
        c.player.set_loop(c.cfg.player.loop_clips);
    }

    fn open_clip(&mut self, c: &mut Ctx, id: i64) {
        self.save_position(c);
        let Some(clip) = c.db().and_then(|db| db.clip(id).ok().flatten()) else { return };
        if let Some(db) = c.db() {
            let _ = db.bump_clip_views(id);
        }
        let start = if clip.last_position > 1.0 && clip.last_position < clip.duration - 2.0 { clip.last_position } else { 0.0 };
        let mix = if c.cfg.player.remember_clip_mix { clip.mixer.clone() } else { None };
        c.player.open(&clip.path, start, clip.audio_tracks.max(0) as usize, &clip.track_names, mix.as_ref(), false, c.cfg.player.autoplay, &c.cfg.player);
        c.player.set_edits(clip.audio_edits.clone());
        if c.player.track_count > 1 {
            c.player.show_mixer = true;
        }
        c.player.set_loop(c.cfg.player.loop_clips);
        self.detail = Detail {
            title_edit: clip.title.clone(),
            notes_edit: clip.notes.clone(),
            share_mb: c.cfg.export.target_size_mb,
            edits: clip.audio_edits.clone(),
            lane_display: lanes::LaneDisplay::load(id),
            clip: Some(clip),
            ..Default::default()
        };
        self.open = Some(id);
    }

    fn close_clip(&mut self, c: &mut Ctx) {
        self.save_position(c);
        c.player.close();
        c.player.set_loop(false);
        self.open = None;
        self.detail = Detail::default();
        self.dirty = true;
    }

    // ------------------------------------------------------------ library

    fn library_ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        egui::Panel::left("clip_filters")
            .exact_size(236.0)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(Margin { left: 0, right: 18, top: 0, bottom: 0 }))
            .show(ui, |ui| self.filters_ui(ui, c));

        // Header
        ui.horizontal(|ui| {
            ui.label(theme::heading(self.filter_title()));
            ui.label(theme::label_dim(format!("{} clip{}", self.clips.len(), if self.clips.len() == 1 { "" } else { "s" })));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::Slider::new(&mut self.card_w, 180.0..=420.0).show_value(false)).on_hover_text("Card size");
                let before = self.sort;
                egui::ComboBox::from_id_salt("sort").selected_text(self.sort.label()).show_ui(ui, |ui| {
                    for s in ClipSort::ALL {
                        ui.selectable_value(&mut self.sort, s, s.label());
                    }
                });
                if self.sort != before {
                    self.dirty = true;
                }
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search clips, games, categories…").desired_width(260.0));
                ui.label(theme::icon_text(icon::SEARCH, 14.0).color(theme::TEXT_FAINT));
            });
        });
        ui.add_space(8.0);

        let last_saved = c.service.shared.lock().jobs.values()
            .filter(|job| job.kind == "clip" && job.state == "done")
            .filter_map(|job| job.clip_id.map(|id| (job.id, id)))
            .max_by_key(|(job_id, _)| *job_id)
            .map(|(_, clip_id)| clip_id);
        if let Some(id) = last_saved {
            if ui.button("Open last saved clip").clicked() {
                self.open_clip(c, id);
                return;
            }
        }

        if !self.selected.is_empty() {
            self.bulk_bar(ui, c);
            ui.add_space(8.0);
        }

        if self.clips.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::icon_text(icon::LIBRARY, 40.0).color(theme::TEXT_FAINT));
                ui.label(theme::title(if self.search.is_empty() { "No clips here yet" } else { "No matches" }));
                ui.label(theme::label_dim(format!("Save a clip with {} while you play.", c.cfg.hotkeys.save_clip)));
                if ui.button("Choose a range from recordings").clicked() { c.goto=Some(crate::app::Page::Recordings); }
            });
            return;
        }

        let gap = 16.0;
        let avail = ui.available_width();
        let cols = ((avail + gap) / (self.card_w + gap)).floor().max(1.0) as usize;
        let cw = (avail - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = cw * 9.0 / 16.0 + 56.0;
        let rows = self.clips.len().div_ceil(cols);
        let clips = self.clips.clone();
        let mut open = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ch + gap, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for clip in clips.iter().skip(r * cols).take(cols) {
                        if let Some(a) = self.card(ui, c, clip, cw) {
                            match a {
                                CardAction::Open => open = Some(clip.id),
                                CardAction::Toggle => {
                                    if !self.selected.remove(&clip.id) {
                                        self.selected.insert(clip.id);
                                    }
                                }
                            }
                        }
                    }
                });
                ui.add_space(gap - ui.spacing().item_spacing.y);
            }
        });
        if let Some(id) = open {
            self.open_clip(c, id);
        }
        self.confirm_bulk(ui, c);
    }

    fn filter_title(&self) -> String {
        match &self.filter {
            Filter::All => "All clips".into(),
            Filter::Favorites => "Favorites".into(),
            Filter::Recent => "Last 7 days".into(),
            Filter::Uncategorized => "Uncategorized".into(),
            Filter::Game(g) => self.games.iter().find(|x| x.id == *g).map(|x| x.name.clone()).unwrap_or_else(|| "Game".into()),
            Filter::Category(k) => self.cats.iter().find(|x| x.id == *k).map(|x| x.name.clone()).unwrap_or_else(|| "Category".into()),
        }
    }

    fn filters_ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        egui::ScrollArea::vertical().id_salt("filters").auto_shrink(false).show(ui, |ui| {
            theme::section(ui, "Library");
            for (f, glyph, label) in [
                (Filter::All, icon::LIBRARY, "All clips"),
                (Filter::Favorites, icon::STAR_FILL, "Favorites"),
                (Filter::Recent, icon::CLOCK, "Last 7 days"),
                (Filter::Uncategorized, icon::TAG, "Uncategorized"),
            ] {
                if filter_row(ui, self.filter == f, glyph, label, None, c.accent).clicked() {
                    self.filter = f;
                    self.dirty = true;
                }
            }
            ui.add_space(10.0);
            theme::section(ui, "Games & apps");
            for g in self.games.clone() {
                let f = Filter::Game(g.id);
                if filter_row(ui, self.filter == f, if g.is_game() { icon::GAME } else { icon::DEVICE }, &g.name, Some(g.clip_count), c.accent).clicked() {
                    self.filter = f;
                    self.dirty = true;
                }
            }
            ui.add_space(10.0);
            theme::section(ui, "Categories");
            for cat in self.cats.clone() {
                if let Some((id, name)) = &mut self.renaming_cat {
                    if *id == cat.id {
                        let r = ui.add(egui::TextEdit::singleline(name).desired_width(180.0));
                        if r.lost_focus() {
                            let (id, name) = (*id, name.clone());
                            if let Some(db) = c.db() {
                                if let Err(e) = db.rename_category(id, &name) {
                                    c.toast("error", format!("{e:#}"));
                                }
                            }
                            self.renaming_cat = None;
                            self.dirty = true;
                        } else {
                            r.request_focus();
                        }
                        continue;
                    }
                }
                let f = Filter::Category(cat.id);
                let resp = filter_row(ui, self.filter == f, icon::TAG, &cat.name, Some(cat.clip_count), c.accent);
                if resp.clicked() {
                    self.filter = f.clone();
                    self.dirty = true;
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        self.renaming_cat = Some((cat.id, cat.name.clone()));
                        ui.close();
                    }
                    if ui.button("Delete category").clicked() {
                        if let Some(db) = c.db() {
                            let _ = db.delete_category(cat.id);
                        }
                        if self.filter == f {
                            self.filter = Filter::All;
                        }
                        self.dirty = true;
                        ui.close();
                    }
                });
            }
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.new_cat).hint_text("New category").desired_width(150.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (theme::icon_button(ui, icon::ADD, 13.0, "Add category").clicked() || enter) && !self.new_cat.trim().is_empty() {
                    let name = self.new_cat.trim().to_string();
                    if let Some(db) = c.db() {
                        match db.create_category(&name, "") {
                            Ok(_) => self.new_cat.clear(),
                            Err(e) => c.toast("error", format!("{e:#}")),
                        }
                    }
                    self.dirty = true;
                }
            });
            ui.label(theme::small("Right-click a category to rename or delete it."));
        });
    }

    fn bulk_bar(&mut self, ui: &mut Ui, c: &mut Ctx) {
        theme::card().inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::title(format!("{} selected", self.selected.len())));
                ui.add_space(12.0);
                let ids: Vec<i64> = self.selected.iter().copied().collect();
                ui.menu_button("Add to category", |ui| {
                    for cat in self.cats.clone() {
                        if ui.button(&cat.name).clicked() {
                            if let Some(db) = c.db() {
                                for id in &ids {
                                    let _ = db.add_clip_category(*id, cat.id);
                                }
                            }
                            self.dirty = true;
                            ui.close();
                        }
                    }
                    if self.cats.is_empty() {
                        ui.label("Create a category first (left panel)");
                    }
                });
                ui.menu_button("Move to game", |ui| {
                    let games = c.db().and_then(|d| d.list_games().ok()).unwrap_or_default();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for g in games {
                            if ui.button(&g.name).clicked() {
                                for id in &ids {
                                    c.request(Request::SetClipGame { clip_id: *id, game_id: Some(g.id) });
                                }
                                self.dirty = true;
                                ui.close();
                            }
                        }
                    });
                });
                if ui.button("Favorite").clicked() {
                    if let Some(db) = c.db() {
                        for id in &ids {
                            let _ = db.set_clip_favorite(*id, true);
                        }
                    }
                    self.dirty = true;
                }
                if ui.button(RichText::new("Delete").color(theme::DANGER)).clicked() {
                    if c.cfg.general.confirm_deletes {
                        self.confirm_bulk_delete = true;
                    } else {
                        self.delete(c, ids.clone());
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear selection").clicked() {
                        self.selected.clear();
                    }
                    if ui.button("Select all").clicked() {
                        self.selected = self.clips.iter().map(|c| c.id).collect();
                    }
                });
            });
        });
    }

    fn delete(&mut self, c: &mut Ctx, ids: Vec<i64>) {
        if let Some(v) = c.request(Request::DeleteClips { clip_ids: ids }) {
            let n = v["deleted"].as_u64().unwrap_or(0);
            let where_ = if c.cfg.clips.delete_to_recycle_bin { " (moved to the Recycle Bin)" } else { "" };
            c.toast("info", format!("Deleted {n} clip{}{where_}", if n == 1 { "" } else { "s" }));
        }
        self.selected.clear();
        self.dirty = true;
    }

    fn confirm_bulk(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.confirm_bulk_delete {
            return;
        }
        let n = self.selected.len();
        let mut close = false;
        let mut go = false;
        egui::Modal::new(egui::Id::unique("bulk_delete")).show(ui.ctx(), |ui| {
            ui.set_width(360.0);
            ui.label(theme::title(format!("Delete {n} clip{}?", if n == 1 { "" } else { "s" })));
            ui.label(theme::label_dim(if c.cfg.clips.delete_to_recycle_bin { "They'll go to the Recycle Bin." } else { "This can't be undone." }));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::accent_button(ui, Some(icon::DELETE), "Delete", theme::DANGER).clicked() {
                    go = true;
                }
                if theme::ghost_button(ui, None, "Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            let ids = self.selected.iter().copied().collect();
            self.delete(c, ids);
            close = true;
        }
        if close {
            self.confirm_bulk_delete = false;
        }
    }

    fn card(&mut self, ui: &mut Ui, c: &mut Ctx, clip: &Clip, w: f32) -> Option<CardAction> {
        let h = w * 9.0 / 16.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h + 56.0), Sense::click());
        if !ui.is_rect_visible(rect) {
            return None;
        }
        let selected = self.selected.contains(&clip.id);
        let img = Rect::from_min_size(rect.min, Vec2::new(w, h));
        let hovered = resp.hovered();
        ui.painter().rect_filled(img, CornerRadius::same(12), theme::SURFACE2);
        match c.thumbs.get(&clip.thumb) {
            Some(tex) => {
                egui::Image::new(&tex).corner_radius(CornerRadius::same(12)).paint_at(ui, img);
            }
            None => {
                ui.painter().text(img.center(), Align2::CENTER_CENTER, icon::PLAY, FontId::new(26.0, theme::icons_family()), theme::TEXT_FAINT);
            }
        }
        if hovered {
            ui.painter().rect_filled(img, CornerRadius::same(12), Color32::from_white_alpha(8));
            ui.painter().circle_filled(img.center(), 24.0, Color32::from_black_alpha(140));
            ui.painter().text(img.center() + egui::vec2(2.0, 0.0), Align2::CENTER_CENTER, icon::PLAY, FontId::new(18.0, theme::icons_family()), Color32::WHITE);
        }
        let highlight = self.highlight == Some(clip.id);
        if selected || highlight {
            ui.painter().rect_stroke(img, CornerRadius::same(12), Stroke::new(2.5, c.accent), egui::StrokeKind::Inside);
        } else if hovered {
            ui.painter().rect_stroke(img, CornerRadius::same(12), Stroke::new(1.0, theme::BORDER_STRONG), egui::StrokeKind::Inside);
        }
        duration_badge(ui, img, clip.duration);
        if clip.favorite {
            let p = img.left_top() + egui::vec2(16.0, 16.0);
            ui.painter().circle_filled(p, 12.0, Color32::from_black_alpha(150));
            ui.painter().text(p, Align2::CENTER_CENTER, icon::STAR_FILL, FontId::new(12.0, theme::icons_family()), theme::WARNING);
        }
        if clip.audio_tracks > 1 {
            let p = img.left_bottom() + egui::vec2(10.0, -10.0);
            let t = format!("{} tracks", clip.audio_tracks);
            let g = ui.painter().layout_no_wrap(t, FontId::proportional(11.0), Color32::WHITE);
            let r = Rect::from_min_size(p - egui::vec2(0.0, g.size().y + 6.0), g.size() + egui::vec2(12.0, 6.0));
            ui.painter().rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(170));
            ui.painter().galley(r.min + egui::vec2(6.0, 3.0), g, Color32::WHITE);
        }
        // Selection checkbox (hover or selection mode).
        let cb = Rect::from_center_size(img.right_top() + egui::vec2(-18.0, 18.0), Vec2::splat(22.0));
        let mut action = None;
        if hovered || selected || !self.selected.is_empty() {
            ui.painter().rect_filled(cb, CornerRadius::same(6), if selected { c.accent } else { Color32::from_black_alpha(150) });
            ui.painter().rect_stroke(cb, CornerRadius::same(6), Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
            if selected {
                ui.painter().text(cb.center(), Align2::CENTER_CENTER, icon::CHECK, FontId::new(12.0, theme::icons_family()), Color32::WHITE);
            }
        }
        let title_pos = egui::pos2(rect.left() + 2.0, img.bottom() + 10.0);
        let max_chars = ((w / 7.5) as usize).max(10);
        ui.painter().text(title_pos, Align2::LEFT_TOP, truncate(&clip.title, max_chars), FontId::new(14.0, theme::semibold()), theme::TEXT);
        let meta = format!(
            "{} · {}",
            clip.game_name.clone().unwrap_or_else(|| "Unknown".into()),
            relative_time(clip.created_at)
        );
        ui.painter().text(title_pos + egui::vec2(0.0, 21.0), Align2::LEFT_TOP, truncate(&meta, max_chars + 4), FontId::proportional(12.5), theme::TEXT_FAINT);

        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);
        if resp.clicked() {
            let pos = resp.interact_pointer_pos().unwrap_or_default();
            action = if cb.contains(pos) || ctrl || !self.selected.is_empty() { Some(CardAction::Toggle) } else { Some(CardAction::Open) };
            self.highlight = None;
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.context_menu(|ui| {
            if ui.button("Open").clicked() {
                action = Some(CardAction::Open);
                ui.close();
            }
            let fav_label = if clip.favorite { "Remove from favorites" } else { "Add to favorites" };
            if ui.button(fav_label).clicked() {
                if let Some(db) = c.db() {
                    let _ = db.set_clip_favorite(clip.id, !clip.favorite);
                }
                self.dirty = true;
                ui.close();
            }
            ui.menu_button("Add to category", |ui| {
                for cat in self.cats.clone() {
                    let has = clip.category_ids.contains(&cat.id);
                    if ui.selectable_label(has, &cat.name).clicked() {
                        if let Some(db) = c.db() {
                            let _ = if has { db.remove_clip_category(clip.id, cat.id) } else { db.add_clip_category(clip.id, cat.id) };
                        }
                        self.dirty = true;
                    }
                }
            });
            if ui.button("Copy file (paste into Discord)").clicked() {
                if c.request(Request::CopyFileToClipboard { path: clip.path.clone() }).is_some() {
                    c.toast("success", "File copied, paste it anywhere");
                }
                ui.close();
            }
            if ui.button("Show in folder").clicked() {
                show_in_folder(&clip.path);
                ui.close();
            }
            ui.separator();
            if ui.button(RichText::new("Delete").color(theme::DANGER)).clicked() {
                self.selected = [clip.id].into();
                if c.cfg.general.confirm_deletes {
                    self.confirm_bulk_delete = true;
                } else {
                    self.delete(c, vec![clip.id]);
                }
                ui.close();
            }
        });
        action
    }

    // ------------------------------------------------------------- detail

    fn detail_ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let Some(clip) = self.detail.clip.clone() else {
            self.open = None;
            return;
        };
        let hits = c.player.shortcuts(ui, &c.cfg.player);
        let st = c.player.state();
        let duration = self.media_duration(c, &clip);
        if hits.mark_in {
            self.stop_preview(c);
            self.mark_in(st.time_pos, duration);
        }
        if hits.mark_out {
            self.stop_preview(c);
            self.mark_out(st.time_pos, duration);
        }
        if let Some(preview) = &mut self.detail.preview {
            match preview.progress(st.loaded.then_some(st.time_pos), st.paused, st.eof) {
                PreviewProgress::Playing => ui.ctx().request_repaint_after(Duration::from_millis(30)),
                PreviewProgress::Finished | PreviewProgress::EndOfFile => self.stop_preview(c),
            }
        } else if !st.eof {
            c.player.set_loop(c.cfg.player.loop_clips);
        }
        if hits.escape && !c.player.fullscreen {
            self.close_clip(c);
            return;
        }
        if c.player.fullscreen {
            self.fullscreen_player(ui, c);
            return;
        }

        // Header
        ui.horizontal(|ui| {
            if theme::icon_button(ui, icon::BACK, 16.0, "Back to library (Esc)").clicked() {
                self.close_clip(c);
            }
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.detail.title_edit)
                    .font(FontId::new(20.0, theme::bold()))
                    .frame(egui::Frame::NONE)
                    .desired_width(ui.available_width() - 260.0),
            );
            if r.lost_focus() && self.detail.title_edit.trim() != clip.title && !self.detail.title_edit.trim().is_empty() {
                let title = self.detail.title_edit.trim().to_string();
                if c.request(Request::RenameClip { clip_id: clip.id, title }).is_some() {
                    self.dirty = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let star = if clip.favorite { icon::STAR_FILL } else { icon::STAR };
                if theme::icon_button(ui, star, 16.0, "Favorite").clicked() {
                    if let Some(db) = c.db() {
                        let _ = db.set_clip_favorite(clip.id, !clip.favorite);
                    }
                    self.dirty = true;
                }
            });
        });
        ui.add_space(10.0);
        if self.dirty {
            self.refresh(c);
        }
        if self.open.is_none() {
            return;
        }

        let side_w = 340.0;
        egui::Panel::right("clip_side")
            .exact_size(side_w)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(Margin { left: 18, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("clip_side_scroll").auto_shrink(false).show(ui, |ui| self.side_ui(ui, c, &clip));
            });

        // Controls in a bottom panel so they always fit; video takes the rest.
        egui::Panel::bottom("clip_controls").resizable(false).frame(egui::Frame::NONE).show(ui, |ui| {
            ui.add_space(4.0);
            let lanes_on = c.player.show_mixer;
            self.timeline_controls(ui, c, duration, lanes_on);
            if lanes_on {
                self.clip_lanes(ui, c, &clip);
            }
            let label = format!("{} / {}", format_duration_precise(st.time_pos), format_duration(st.duration));
            c.player.transport(ui, &label, &c.cfg.player);
        });
        let avail = ui.available_size();
        let vh = (avail.x * 9.0 / 16.0).min(avail.y).max(120.0);
        c.player.video(ui, Vec2::new(avail.x, vh));
        self.persist_audio(c, &clip);
    }

    fn media_duration(&self, c: &Ctx, clip: &Clip) -> f64 {
        let state = c.player.state();
        if state.loaded && state.duration.is_finite() && state.duration > 0.0 {
            state.duration
        } else {
            if clip.duration.is_finite() { clip.duration.max(0.0) } else { 0.0 }
        }
    }

    fn stop_preview(&mut self, c: &mut Ctx) {
        if self.detail.preview.take().is_some() {
            c.player.set_paused(true);
            c.player.set_loop(!c.player.state().eof && c.cfg.player.loop_clips);
        }
    }

    fn clip_lanes(&mut self, ui: &mut Ui, c: &mut Ctx, clip: &Clip) {
        let dur = self.media_duration(c, clip);
        let (t0, t1) = self.detail.timeline.range(dur);
        let wave = c.waves.get(&clip.path);
        let tracks = c.player.media_info()
            .map(|info| lanes::waveform_tracks(info, dur, &c.cfg.audio.app_categories));
        let sources: Vec<WaveSource> = wave.iter().map(|w| WaveSource {
            start: 0.0, end: dur, wave: w, tracks: tracks.as_ref(),
        }).collect();
        let ph = Some(c.player.state().time_pos);
        let default_range = self.selection(dur).unwrap_or((0.0, dur));
        let acts = lanes::lanes(ui, &mut c.player, t0, t1, false, &sources, &self.detail.edits, ph, &mut self.detail.lanes, c.accent, self.detail.edit_history.can_undo(), Some(&mut self.detail.lane_display), (0.0, dur), default_range);
        let mut region_saved = false;
        for action in acts {
            match action {
                LaneAction::Seek(t) => {
                    self.stop_preview(c);
                    c.player.seek(t, true);
                }
                LaneAction::Edit { lanes, a, b, gain } => {
                    if !region_saved {
                        self.detail.edit_history.save(&self.detail.edits, c.player.mix.clone());
                        region_saved = true;
                    }
                    for lane in &lanes {
                        apply_edit(&mut self.detail.edits, lane, a, b, gain);
                    }
                    if let Some(mode) = lanes::mode_for_edit(&lanes, gain).filter(|mode| *mode != c.player.mix.mode) {
                        c.player.set_mode(mode);
                        self.detail.mix_dirty = Some(Instant::now());
                    }
                    c.player.set_edits(self.detail.edits.clone());
                    self.detail.edits_dirty = Some(Instant::now());
                }
                LaneAction::Undo => {
                    if let Some(snapshot) = self.detail.edit_history.undo() {
                        let now = Instant::now();
                        if snapshot.mix != c.player.mix {
                            self.detail.mix_dirty = Some(now);
                        }
                        if snapshot.edits != self.detail.edits {
                            self.detail.edits_dirty = Some(now);
                        }
                        self.detail.edits = snapshot.edits;
                        c.player.restore_audio(snapshot.mix, self.detail.edits.clone());
                    }
                }
                LaneAction::MixChanged { previous } => {
                    if let Some(previous) = previous {
                        self.detail.edit_history.save(&self.detail.edits, previous);
                    }
                    self.detail.mix_dirty = Some(Instant::now());
                }
                LaneAction::DisplayChanged => {
                    if let Err(error) = self.detail.lane_display.save(clip.id) {
                        c.toast("error", format!("Couldn't save lane display: {error}"));
                    }
                }
            }
        }
    }

    /// Flush audio changes before navigation and before any job reads the clip.
    fn flush_audio(&mut self, c: &mut Ctx, clip_id: i64) -> bool {
        let belongs = self.detail.clip.as_ref().is_some_and(|clip| c.player.path.as_deref() == Some(clip.path.as_str()));
        let mix = (self.detail.mix_dirty.is_some() && c.cfg.player.remember_clip_mix && belongs).then(|| c.player.mix.clone());
        let edits = self.detail.edits_dirty.is_some().then_some(self.detail.edits.as_slice());
        if mix.is_none() && edits.is_none() {
            self.detail.mix_dirty = None;
            return true;
        }
        let Some(db) = c.db() else {
            c.toast("error", "Could not open the library to save the audio changes");
            return false;
        };
        let result = (|| -> anyhow::Result<()> {
            if let Some(mix) = &mix {
                db.set_clip_mixer(clip_id, Some(mix))?;
            }
            if let Some(edits) = edits {
                db.set_clip_audio_edits(clip_id, edits)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            c.toast("error", format!("Could not save audio changes: {error}"));
            return false;
        }
        if let Some(clip) = &mut self.detail.clip {
            if mix.is_some() {
                clip.mixer = mix;
            }
            if edits.is_some() {
                clip.audio_edits = self.detail.edits.clone();
            }
        }
        self.detail.mix_dirty = None;
        self.detail.edits_dirty = None;
        true
    }

    fn persist_audio(&mut self, c: &mut Ctx, clip: &Clip) {
        let due = [self.detail.mix_dirty, self.detail.edits_dirty].into_iter().flatten()
            .any(|changed| changed.elapsed() >= Duration::from_millis(700));
        if due && !self.flush_audio(c, clip.id) {
            let now = Instant::now();
            self.detail.mix_dirty = self.detail.mix_dirty.map(|_| now);
            self.detail.edits_dirty = self.detail.edits_dirty.map(|_| now);
        }
        if self.detail.mix_dirty.is_some() || self.detail.edits_dirty.is_some() {
            ui_repaint_soon();
        }
    }

    fn fullscreen_player(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let st = c.player.state();
        egui::Panel::bottom("clip_fs_controls").resizable(false).frame(egui::Frame::NONE.fill(theme::BG).inner_margin(Margin::symmetric(16, 8))).show(ui, |ui| {
            seek_bar(ui, &mut c.player, &mut self.detail.in_pt, &mut self.detail.out_pt, c.accent);
            let label = format!("{} / {}", format_duration_precise(st.time_pos), format_duration(st.duration));
            c.player.transport(ui, &label, &c.cfg.player);
        });
        let avail = ui.available_size();
        c.player.video(ui, avail);
    }

    fn side_ui(&mut self, ui: &mut Ui, c: &mut Ctx, clip: &Clip) {
        ui.set_max_width(ui.available_width());
        if !self.detail.edits.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Audio edits"));
                let n = self.detail.edits.len();
                ui.label(theme::small(format!(
                    "{n} part{} of the audio muted or changed. The original audio is untouched; exports and shares use these edits.",
                    if n == 1 { "" } else { "s" }
                )));
                if ui.button("Remove all edits").clicked() {
                    self.detail.edit_history.save(&self.detail.edits, c.player.mix.clone());
                    self.detail.edits.clear();
                    c.player.set_edits(Vec::new());
                    self.detail.edits_dirty = Some(Instant::now());
                }
            });
            ui.add_space(12.0);
        }

        // Trim / cut.
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Trim"));
            ui.label(theme::small("Move the playhead, then set each end of your selection."));
            ui.add_space(4.0);
            let pos = c.player.state().time_pos;
            let duration = self.media_duration(c, clip);
            ui.horizontal(|ui| {
                if ui.button(format!("Set start  {}", self.detail.in_pt.map(format_duration_precise).unwrap_or("—".into()))).on_hover_text("Start here (I)").clicked() {
                    self.stop_preview(c);
                    self.mark_in(pos, duration);
                }
                if ui.button(format!("Set end  {}", self.detail.out_pt.map(format_duration_precise).unwrap_or("—".into()))).on_hover_text("End here (O)").clicked() {
                    self.stop_preview(c);
                    self.mark_out(pos, duration);
                }
                if theme::icon_button(ui, icon::CLOSE, 12.0, "Clear").clicked() {
                    self.stop_preview(c);
                    self.detail.in_pt = None;
                    self.detail.out_pt = None;
                }
            });
            if let Some((a, b)) = self.selection(duration) {
                ui.label(theme::label_dim(format!("Selection {}", format_duration_precise(b - a))));
                let previewing = self.detail.preview.is_some();
                if ui.button(if previewing { "Stop preview" } else { "Preview selection" }).clicked() {
                    if previewing {
                        self.stop_preview(c);
                    } else {
                        c.player.set_loop(false);
                        c.player.seek(a, true);
                        c.player.set_paused(false);
                        self.detail.preview = Some(RangePreview::new(a, b));
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    if theme::primary_button(ui, Some(icon::SCISSORS), "Save as new clip").clicked()
                        && self.flush_audio(c, clip.id)
                    {
                        if c.request(Request::TrimClip { clip_id: clip.id, start_s: a, end_s: b, replace: false }).is_some() {
                            c.toast("info", "Trimming…");
                        }
                    }
                    if theme::ghost_button(ui, None, "Trim original").on_hover_text("Replaces this clip (the original goes to the Recycle Bin)").clicked()
                        && self.flush_audio(c, clip.id)
                    {
                        self.close_clip(c);
                        if c.request(Request::TrimClip { clip_id: clip.id, start_s: a, end_s: b, replace: true }).is_some() {
                            c.toast("info", "Trimming…");
                        }
                    }
                });
            } else if self.detail.in_pt.is_some() || self.detail.out_pt.is_some() {
                ui.label(theme::small("Choose a selection longer than 0.1 seconds."));
            }
        });
        ui.add_space(12.0);

        // Export / share.
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Export to share"));
            ui.label(theme::small("Exports use your current audio mix and the trim selection, if any."));
            ui.add_space(4.0);
            let range = self.selection(self.media_duration(c, clip));
            let valid = range.is_some() || (self.detail.in_pt.is_none() && self.detail.out_pt.is_none());
            ui.add_enabled_ui(valid, |ui| {
                let mut export = |target_size_mb| {
                    if c.request(Request::Export {
                        source: ExportSource::Clip { clip_id: clip.id },
                        range,
                        mix: c.player.mix.clone(),
                        edits: Some(self.detail.edits.clone()),
                        target_size_mb,
                        title: String::new(),
                    }).is_some() {
                        c.toast("info", "Exporting…");
                    }
                };
                ui.horizontal(|ui| {
                    ui.label("Limit");
                    ui.add(egui::DragValue::new(&mut self.detail.share_mb).range(1..=4000).suffix(" MB"));
                    if theme::primary_button(ui, Some(icon::SHARE), "Export to size limit").on_hover_text("Re-encode to fit the selected file size").clicked() {
                        export(Some(self.detail.share_mb));
                    }
                });
                if theme::ghost_button(ui, Some(icon::SHARE), "Export full quality").on_hover_text("Mixes audio to one track").clicked() {
                    export(None);
                }
            });
            show_jobs(ui,c,true);
        });
        ui.add_space(12.0);

        // Organise.
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Details"));
            ui.add_space(4.0);
            let games = c.db().and_then(|d| d.list_games().ok()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(theme::label_dim("Game"));
                let current = clip.game_name.clone().unwrap_or_else(|| "Unknown".into());
                egui::ComboBox::from_id_salt("clip_game").selected_text(current).width(220.0).show_ui(ui, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.detail.game_search).hint_text("Search games"));
                    let query = self.detail.game_search.trim().to_lowercase();
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for g in games.iter().filter(|g| g.name.to_lowercase().contains(&query)) {
                            if ui.selectable_label(clip.game_id == Some(g.id), &g.name).clicked() {
                                c.request(Request::SetClipGame { clip_id: clip.id, game_id: Some(g.id) });
                                self.dirty = true;
                            }
                        }
                    });
                });
            });
            ui.add_space(4.0);
            ui.label(theme::label_dim("Categories"));
            let cats = self.cats.clone();
            ui.horizontal_wrapped(|ui| {
                for cat in &cats {
                    let on = clip.category_ids.contains(&cat.id);
                    if ui.add(egui::Button::selectable(on, &cat.name)).clicked() {
                        if let Some(db) = c.db() {
                            let _ = if on { db.remove_clip_category(clip.id, cat.id) } else { db.add_clip_category(clip.id, cat.id) };
                        }
                        self.dirty = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.detail.new_cat).hint_text("New category").desired_width(170.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (theme::icon_button(ui, icon::ADD, 12.0, "Create and add").clicked() || enter) && !self.detail.new_cat.trim().is_empty() {
                    let name = self.detail.new_cat.trim().to_string();
                    if let Some(db) = c.db() {
                        if let Ok(id) = db.create_category(&name, "") {
                            let _ = db.add_clip_category(clip.id, id);
                        }
                    }
                    self.detail.new_cat.clear();
                    self.dirty = true;
                }
            });
            ui.add_space(4.0);
            ui.label(theme::label_dim("Notes"));
            if ui.add(egui::TextEdit::multiline(&mut self.detail.notes_edit).desired_rows(3).desired_width(f32::INFINITY)).changed() {
                self.detail.notes_dirty = Some(Instant::now());
            }
            if let Some(t) = self.detail.notes_dirty {
                if t.elapsed() > Duration::from_millis(600) {
                    let notes = self.detail.notes_edit.clone();
                    if let Some(db) = c.db() {
                        let _ = db.set_clip_notes(clip.id, &notes);
                    }
                    self.detail.notes_dirty = None;
                }
                ui.ctx().request_repaint_after(Duration::from_millis(300));
            }
            ui.add_space(6.0);
            let when = chrono::DateTime::from_timestamp_millis(clip.created_at).unwrap_or_default().with_timezone(&chrono::Local);
            egui::Grid::new("clip_meta").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(theme::small(k));
                    ui.label(RichText::new(v).size(12.5).color(theme::TEXT_DIM));
                    ui.end_row();
                };
                row(ui, "Saved", when.format("%a %b %d %Y, %H:%M").to_string());
                row(ui, "Length", format_duration(clip.duration));
                if clip.width > 0 {
                    row(ui, "Video", format!("{}×{} · {:.0} fps", clip.width, clip.height, clip.fps));
                }
                row(ui, "Size", human_bytes(clip.size.max(0) as u64));
                row(ui, "Audio", format!("{} track{}", clip.audio_tracks, if clip.audio_tracks == 1 { "" } else { "s" }));
                row(ui, "Source", match clip.source.as_str() {
                    "replay" => "Replay buffer".into(),
                    "recording" => "Clipped from 24/7".into(),
                    "trim" => "Trimmed copy".into(),
                    _ => "Imported".into(),
                });
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if theme::ghost_button(ui, Some(icon::COPY), "Copy file").on_hover_text("Paste straight into Discord or Explorer").clicked() {
                    if c.request(Request::CopyFileToClipboard { path: clip.path.clone() }).is_some() {
                        c.toast("success", "File copied, paste it anywhere");
                    }
                }
                if theme::ghost_button(ui, Some(icon::FOLDER), "Show in folder").clicked() {
                    show_in_folder(&clip.path);
                }
                if let Some(src) = clip.source_start_ms {
                    let available = c.db().and_then(|d| d.list_recordings().ok()).map(|v| v.iter().any(|r| r.start_ms <= src && r.end_ms() >= src || r.is_live() && r.start_ms <= src)).unwrap_or(false);
                    if available && theme::ghost_button(ui, Some(icon::HISTORY), "Show in 24/7").on_hover_text("Jump to this moment in the continuous recording").clicked() {
                        self.save_position(c);
                        crate::views::recordings::request_play_at(src);
                        c.goto = Some(crate::app::Page::Recordings);
                    }
                }
                if theme::ghost_button(ui, Some(icon::DELETE), "Delete").clicked() {
                    self.detail.confirm_delete = true;
                }
            });
        });
        if self.detail.confirm_delete {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("del_one")).show(ui.ctx(), |ui| {
                ui.set_width(340.0);
                ui.label(theme::title("Delete this clip?"));
                ui.label(theme::label_dim(&clip.title));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if theme::accent_button(ui, Some(icon::DELETE), "Delete", theme::DANGER).clicked() {
                        c.player.close();
                        self.delete(c, vec![clip.id]);
                        self.open = None;
                        self.detail = Detail::default();
                        close = true;
                    }
                    if theme::ghost_button(ui, None, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.detail.confirm_delete = false;
            }
        }
    }
}

enum CardAction {
    Open,
    Toggle,
}

fn filter_row(ui: &mut Ui, selected: bool, glyph: &str, label: &str, count: Option<i64>, accent: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let bg = if selected {
        theme::SURFACE3
    } else if resp.hovered() {
        theme::SURFACE2
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
    let fg = if selected { theme::TEXT } else { theme::TEXT_DIM };
    let gcolor = if selected { accent } else { theme::TEXT_FAINT };
    ui.painter().text(rect.left_center() + egui::vec2(16.0, 0.0), Align2::CENTER_CENTER, glyph, FontId::new(13.0, theme::icons_family()), gcolor);
    ui.painter().text(rect.left_center() + egui::vec2(34.0, 0.0), Align2::LEFT_CENTER, truncate(label, 24), FontId::proportional(13.5), fg);
    if let Some(n) = count {
        ui.painter().text(rect.right_center() - egui::vec2(10.0, 0.0), Align2::RIGHT_CENTER, n.to_string(), FontId::proportional(12.0), theme::TEXT_FAINT);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn jobs_list(ui: &mut Ui, c: &mut Ctx) {
    show_jobs(ui,c,false);
}

fn show_jobs(ui:&mut Ui,c:&mut Ctx,exports_only:bool){
    let jobs: Vec<_> = {
        let s = c.service.shared.lock();
        let mut v: Vec<_> = s.jobs.values().cloned().collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.id));
        v.into_iter().filter(|j|!exports_only || j.kind=="export").take(4).collect()
    };
    for j in jobs {
        ui.add_space(4.0);
        let active = j.state == "running" || j.state == "queued";
        ui.horizontal_wrapped(|ui| {
            let (color, text) = match j.state.as_str() {
                "done" => (theme::SUCCESS, "Done".to_string()),
                "failed" => (theme::DANGER, "Failed".to_string()),
                "cancelled" => (theme::TEXT_FAINT, "Cancelled".to_string()),
                "queued" => (theme::TEXT_FAINT, "Queued".to_string()),
                _ => (theme::TEXT_DIM, format!("{:.0}%", j.progress * 100.0)),
            };
            ui.label(RichText::new(text).size(12.5).color(color));
            if let Some(bytes) = j.output_bytes {
                ui.label(theme::small(cv_core::util::human_bytes(bytes))).on_hover_text(format!("{bytes} bytes"));
            }
            if active {
                if ui.small_button("Cancel").clicked() {
                    c.request(Request::CancelJob { id: j.id });
                }
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            } else if let Some(out) = &j.output {
                if let Some(id) = j.clip_id.filter(|_| j.kind != "export") {
                    if ui.small_button("Open saved clip").clicked() {
                        request_open(id);
                        c.goto = Some(crate::app::Page::Clips);
                    }
                }
                if ui.small_button("Open folder").on_hover_text("Show this file in Explorer").clicked() {
                    show_in_folder(out);
                }
            }
        });
        let name = j.output.as_deref().and_then(|path| std::path::Path::new(path).file_name())
            .and_then(|name| name.to_str()).unwrap_or(&j.title);
        let filename = ui.add(egui::Label::new(RichText::new(name).size(12.5).color(theme::TEXT_DIM))
            .wrap().selectable(true));
        if let Some(path) = &j.output {
            filename.on_hover_ui(|ui| {
                ui.set_max_width(360.0);
                ui.add(egui::Label::new(path).wrap().selectable(true));
            });
        }
        if j.state == "running" {
            ui.add(egui::ProgressBar::new(j.progress).desired_height(4.0));
        }
        if j.state == "failed" && !j.message.is_empty() {
            ui.add(egui::Label::new(RichText::new(&j.message).size(12.0).color(theme::DANGER)).wrap());
        }
    }
}

pub fn duration_badge(ui: &Ui, img: Rect, secs: f64) {
    if secs <= 0.0 {
        return;
    }
    let g = ui.painter().layout_no_wrap(format_duration(secs), FontId::new(11.5, theme::semibold()), Color32::WHITE);
    let r = Rect::from_min_size(img.right_bottom() - g.size() - egui::vec2(20.0, 16.0), g.size() + egui::vec2(10.0, 6.0));
    ui.painter().rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(180));
    ui.painter().galley(r.min + egui::vec2(5.0, 3.0), g, Color32::WHITE);
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>().trim_end())
    }
}

pub fn relative_time(ms: i64) -> String {
    let secs = (now_ms() - ms) / 1000;
    let dt = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().with_timezone(&chrono::Local);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        dt.format("%A %H:%M").to_string()
    } else {
        dt.format("%b %d, %Y").to_string()
    }
}

pub fn show_in_folder(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{path}")).spawn();
}

fn ui_repaint_soon() {
    // Debounced saves need a frame after the delay even when idle.
    if let Some(ctx) = REPAINT_CTX.get() {
        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

static REPAINT_CTX: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

pub fn set_repaint_ctx(ctx: &egui::Context) {
    let _ = REPAINT_CTX.set(ctx.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_clamp_without_moving_the_opposite_boundary() {
        let mut view = ClipsView::default();
        assert_eq!(view.selection(10.0), None);
        view.mark_in(-2.0, 10.0);
        view.mark_out(20.0, 10.0);
        assert_eq!(view.selection(10.0), Some((0.0, 10.0)));
        view.mark_in(8.0, 10.0);
        view.mark_out(5.0, 10.0);
        assert_eq!(view.selection(10.0), Some((8.0, 8.101)));
        view.mark_in(10.0,10.0);
        assert!((view.detail.in_pt.unwrap()-8.0).abs()<1e-6);
        assert_eq!(view.selection(0.0), None);
    }
}
