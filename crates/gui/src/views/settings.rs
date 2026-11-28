//! Settings: every option in config.toml, grouped into sections, with a
//! save bar that warns when recording needs to restart.

use crate::app::Ctx;
use crate::player::shortcuts;
use crate::theme::{self, icon};
use cv_core::audio_names::Endpoint;
use cv_core::config::{Accent, Config, HistoryMode, HEIGHTS, MAX_ROLLING_MINUTES, QUALITIES, ROLLING_SEGMENT_SECS};
use cv_core::hotkey::Hotkey;
use cv_core::ipc::{AudioDevices, MonitorInfo, Request};
use cv_core::model::Game;
use eframe::egui::{self, Color32, Key, Margin, RichText, Ui, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tab {
    General,
    Recording,
    Clips,
    Hotkeys,
    Overlay,
    Audio,
    Games,
    Player,
    Export,
    Advanced,
    About,
}

const TABS: [(Tab, &str, &str); 11] = [
    (Tab::General, icon::SETTINGS, "General"),
    (Tab::Recording, icon::HISTORY, "Recording & storage"),
    (Tab::Clips, icon::LIBRARY, "Saved clips"),
    (Tab::Hotkeys, icon::KEYBOARD, "Hotkeys"),
    (Tab::Overlay, icon::BOLT, "Overlay & alerts"),
    (Tab::Audio, icon::MIC, "Audio & mic"),
    (Tab::Games, icon::GAME, "Games"),
    (Tab::Player, icon::PLAY, "Player"),
    (Tab::Export, icon::SHARE, "Export & sharing"),
    (Tab::Advanced, icon::MORE, "Advanced"),
    (Tab::About, icon::INFO, "About"),
];

pub struct SettingsView {
    tab: Tab,
    draft: Option<Config>,
    saved: Option<Config>,
    capturing: Option<&'static str>,
    devices: Option<AudioDevices>,
    monitors: Option<Vec<MonitorInfo>>,
    games: Vec<Game>,
    games_loaded: bool,
    new_item: [String; 6],
    confirm_restart: bool,
    merge_from: Option<i64>,
    new_preset_mb: u32,
}

impl Default for SettingsView {
    fn default() -> Self {
        Self {
            tab: Tab::General,
            draft: None,
            saved: None,
            capturing: None,
            devices: None,
            monitors: None,
            games: Vec::new(),
            games_loaded: false,
            new_item: Default::default(),
            confirm_restart: false,
            merge_from: None,
            new_preset_mb: 25,
        }
    }
}

impl SettingsView {
    pub(crate) fn qa_reopen_player_hotkeys(&mut self) {
        self.draft = None;
        self.saved = None;
        self.tab = Tab::Hotkeys;
    }

    pub(crate) fn qa_latest_binding(&self, slot: usize) -> Option<String> {
        let cfg = &self.draft.as_ref()?.player;
        shortcuts::SHORTCUTS.iter()
            .find(|binding| binding.action == shortcuts::Action::JumpLatest)
            .and_then(|binding| binding.bindings(cfg).get(slot).map(|value| (*value).to_owned()))
    }

    /// Change only the staged QA draft so the sweep exercises the pinned save bar.
    pub(crate) fn qa_seed_dirty(&mut self, c: &Ctx) {
        let saved = self.saved.get_or_insert_with(|| c.cfg.clone());
        let draft = self.draft.get_or_insert_with(|| saved.clone());
        draft.general.confirm_deletes = !saved.general.confirm_deletes;
    }

    /// Called when navigating away: never leave global hotkeys suspended.
    pub fn leave(&mut self, c: &mut Ctx) {
        if self.capturing.take().is_some() {
            c.service.fire(Request::SuspendHotkeys { suspend: false });
        }
    }

    /// Jump to a section by name (used by the self-test).
    pub fn select_tab(&mut self, name: &str) {
        self.tab = match name {
            "recording" => Tab::Recording,
            "audio" => Tab::Audio,
            "games" => Tab::Games,
            "clips" => Tab::Clips,
            "hotkeys" => Tab::Hotkeys,
            "overlay" => Tab::Overlay,
            "player" => Tab::Player,
            "export" => Tab::Export,
            "advanced" => Tab::Advanced,
            "about" => Tab::About,
            _ => Tab::General,
        };
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if self.draft.is_none() {
            let cfg = Config::load_or_default();
            self.saved = Some(cfg.clone());
            self.draft = Some(cfg);
        }
        let dirty = self.draft != self.saved;

        ui.horizontal(|ui| {
            ui.label(theme::heading("Settings"));
        });
        ui.add_space(10.0);

        let mut body = ui.available_rect_before_wrap();
        let _ = ui.allocate_rect(body, egui::Sense::hover());
        if dirty {
            let restart = self.saved.as_ref().zip(self.draft.as_ref())
                .is_some_and(|(saved, draft)| saved.recorder_restart_needed(draft));
            let status = if restart { "Unsaved changes · recording will restart" } else { "Unsaved changes" };
            let compact = body.width() < 510.0;
            let mut footer = body;
            let height = if compact {
                let font = egui::TextStyle::Body.resolve(ui.style());
                let label = ui.painter().layout(status.to_owned(), font, theme::TEXT, (body.width() - 34.0).max(0.0));
                56.0 + ui.spacing().item_spacing.y + label.size().y
            } else {
                56.0
            };
            footer.min.y = (footer.bottom() - height).max(footer.top());
            body.max.y = (footer.top() - 10.0).max(body.top());
            super::editor_layout::region(ui, "settings_save_footer", footer, |ui| {
                self.save_bar(ui, c, status, restart, compact);
            });
        }
        super::editor_layout::region(ui, "settings_body", body, |ui| self.settings_body(ui, c));
        self.restart_modal(ui, c);
    }

    fn settings_body(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if ui.available_width() < 740.0 {
            let mut selected = self.tab;
            settings_section_selector(ui, &mut selected);
            if selected != self.tab {
                self.leave(c);
                self.tab = selected;
            }
            ui.add_space(8.0);
        } else {
            egui::Panel::left("settings_tabs")
            .exact_size(210.0)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(Margin { left: 0, right: 16, top: 0, bottom: 0 }))
            .show(ui, |ui| egui::ScrollArea::vertical().id_salt("settings_sections").auto_shrink(false).show(ui, |ui| {
                for (tab, glyph, label) in TABS {
                    let sel = self.tab == tab;
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, sel, label));
                    let bg = if sel { theme::SURFACE3 } else if resp.hovered() { theme::SURFACE2 } else { Color32::TRANSPARENT };
                    ui.painter().rect_filled(rect, egui::CornerRadius::same(8), bg);
                    if resp.has_focus() {
                        ui.painter().rect_stroke(rect, 8.0, egui::Stroke::new(1.5, theme::TEXT), egui::StrokeKind::Inside);
                    }
                    let fg = if sel { theme::TEXT } else { theme::TEXT_DIM };
                    ui.painter().text(rect.left_center() + egui::vec2(16.0, 0.0), egui::Align2::CENTER_CENTER, glyph, egui::FontId::new(13.0, theme::icons_family()), if sel { c.accent } else { theme::TEXT_FAINT });
                    ui.painter().text(rect.left_center() + egui::vec2(34.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(13.5), fg);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        if self.tab != tab {
                            self.leave(c);
                            self.tab = tab;
                        }
                    }
                }
            }));
        }

        egui::ScrollArea::vertical().id_salt(("settings_scroll", self.tab)).auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(820.0));
            let mut d = self.draft.take().unwrap();
            match self.tab {
                Tab::General => self.general(ui, c, &mut d),
                Tab::Recording => {
                    if self.monitors.is_none() {
                        self.monitors = Some(c.service.call(Request::Monitors).ok()
                            .and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default());
                    }
                    recording(ui, c, &mut d, &mut self.monitors);
                }
                Tab::Clips => self.clips(ui, c, &mut d),
                Tab::Hotkeys => self.hotkeys(ui, c, &mut d),
                Tab::Overlay => overlay(ui, c, &mut d),
                Tab::Audio => self.audio(ui, c, &mut d),
                Tab::Games => self.games(ui, c, &mut d),
                Tab::Player => player(ui, c, &mut d),
                Tab::Export => export(ui, c, &mut d, &mut self.new_preset_mb),
                Tab::Advanced => advanced(ui, c, &mut d),
                Tab::About => about(ui, c),
            }
            self.draft = Some(d);
        });
    }

    fn save_bar(&mut self, ui: &mut Ui, c: &mut Ctx, status: &str, restart: bool, compact: bool) {
        theme::card().inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if compact {
                ui.add(egui::Label::new(theme::label_dim(status)).wrap());
            }
            ui.horizontal_wrapped(|ui| {
                if !compact {
                    ui.label(theme::icon_text(icon::INFO, 14.0).color(theme::TEXT_DIM));
                    ui.label(RichText::new(status).color(theme::TEXT));
                }
                let save = theme::primary_button(ui, Some(icon::CHECK), "Save");
                super::editor_layout::remember_interaction_rect(ui, "shortcut_settings_save", save.rect);
                if save.clicked() {
                    if restart {
                        self.confirm_restart = true;
                    } else {
                        self.save(ui.ctx(), c, false);
                    }
                }
                if theme::ghost_button(ui, None, "Discard").clicked() {
                    self.draft = self.saved.clone();
                }
            });
        });
    }

    fn restart_modal(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.confirm_restart {
            return;
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("restart_recorder")).show(ui.ctx(), |ui| {
            ui.set_width(dialog_width(ui, 420.0));
            ui.label(theme::title("Restart recording to apply?"));
            ui.label(theme::label_dim("ClipVault closes the current file and starts a new one with your settings. The timeline may have a short gap."));
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if theme::primary_button(ui, Some(icon::REFRESH), "Save & restart recording").clicked() {
                    choice = Some(true);
                }
                if theme::ghost_button(ui, None, "Cancel").clicked() {
                    self.confirm_restart = false;
                }
            });
        });
        if let Some(r) = choice {
            self.confirm_restart = false;
            self.save(ui.ctx(), c, r);
        }
    }

    fn save(&mut self, ctx: &egui::Context, c: &mut Ctx, restart_recorder: bool) {
        let Some(mut d) = self.draft.clone() else { return };
        let shortcuts_changed = self.saved.as_ref().is_none_or(|saved| {
            saved.player.hotkeys != d.player.hotkeys || saved.hotkeys != d.hotkeys
        });
        if shortcuts_changed {
            if let Some(problem) = shortcuts::problem(&d.player, &d.hotkeys) {
                c.toast("error", format!("Check player shortcuts: {problem}"));
                return;
            }
        }
        d.sanitize();
        if let Err(error) = d.validate_media_folders() {
            c.toast("error", format!("Check your storage folders: {error}"));
            return;
        }
        if let Err(e) = d.save() {
            c.toast("error", format!("Couldn't save settings: {e:#}"));
            return;
        }
        self.saved = Some(d.clone());
        self.draft = Some(d.clone());
        let r = c.service.call(Request::ReloadConfig { restart_recorder });
        match r {
            Ok(_) => c.toast("success", if restart_recorder { "Settings saved. Restarting recording…" } else { "Settings saved" }),
            Err(e) => c.toast("warning", format!("Saved; the service will pick it up when it starts ({e})")),
        }
        c.reload_config();
        theme::apply(ctx, c.accent, c.cfg.general.ui_scale);
    }

    // ------------------------------------------------------------- tabs

    fn general(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Startup", |ui| {
            if cv_core::paths::uses_local_storage() {
                ui.label(theme::small("This project profile starts when you open ClipVault."));
            } else {
                toggle_row(ui, c, "Start with Windows", "Starts the ClipVault background service when you sign in.", &mut d.general.start_with_windows);
            }
            toggle_row(ui, c, "Show tray icon", "A small icon next to the clock: red when recording, amber when something needs attention.", &mut d.general.show_tray_icon);
            toggle_row(ui, c, "Windows notifications", "Pop-ups for microphone, recording, and storage alerts.", &mut d.general.notifications);
            row(ui, "Open on", "Which page ClipVault opens on.", |ui| {
                egui::ComboBox::from_id_salt("start_page").selected_text(d.general.start_page.clone()).show_ui(ui, |ui| {
                    for p in ["home", "recordings", "clips"] {
                        ui.selectable_value(&mut d.general.start_page, p.to_string(), p);
                    }
                });
            });
        });
        group(ui, "Appearance", |ui| {
            row(ui, "Accent colour", "Used sparingly on the greyscale theme (record button, selection).", |ui| {
                for a in Accent::ALL {
                    let col = theme::accent(a);
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::click());
                    ui.painter().circle_filled(r.center(), 10.0, col);
                    if d.general.accent == a {
                        ui.painter().circle_stroke(r.center(), 12.0, egui::Stroke::new(2.0, theme::TEXT));
                    }
                    if resp.on_hover_text(a.label()).clicked() {
                        d.general.accent = a;
                    }
                }
            });
            row(ui, "Interface size", "Zoom the whole interface.", |ui| {
                ui.add(egui::Slider::new(&mut d.general.ui_scale, 0.75..=1.75).step_by(0.05).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            });
            toggle_row(ui, c, "Ask before deleting", "Confirm before deleting clips.", &mut d.general.confirm_deletes);
            toggle_row(ui, c, "Reduce motion", "Fewer animations.", &mut d.general.reduce_motion);
        });
    }

    fn clips(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Saved clip length", |ui| {
            let policy = match d.recording.rolling_seconds() {
                Some(seconds) => format!("Replay only keeps the last {} min of unsaved footage.", seconds / 60),
                None if d.recording.enabled => format!("Full history keeps {} of footage.", d.recording.history_label()),
                None => "Full-history recording is turned off.".to_owned(),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::label_dim(policy));
                if ui.link("Change recording history").clicked() {
                    self.tab = Tab::Recording;
                }
            });
            row(ui, "Default clip length", "How far back the Save clip hotkey reaches.", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.clip_length_secs).range(5..=3600).suffix(" s"));
                ui.label(theme::small(cv_core::util::format_duration(d.clips.clip_length_secs as f64)));
            });
            row(ui, "Short clip length", "Length of the \"save short clip\" hotkey (trimmed from the replay).", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.short_clip_secs).range(5..=600).suffix(" s"));
            });
        });
        group(ui, "Organising", |ui| {
            folder_row(ui, "Clips folder", "Where clips are stored.", &mut d.clips.folder);
            toggle_row(ui, c, "One folder per game", "Files go into clips\\<Game>\\. You can still browse by category in the app.", &mut d.clips.organize_by_game);
            row(ui, "File name", "Tokens: {game} {date} {time} {exe}", |ui| {
                ui.add(egui::TextEdit::singleline(&mut d.clips.name_template).desired_width(260.0));
            });
            row(ui, "Name when no game is detected", "Used for clips of the desktop or unknown apps.", |ui| {
                ui.add(egui::TextEdit::singleline(&mut d.clips.unknown_game_name).desired_width(200.0));
            });
            toggle_row(ui, c, "Favourite game clips automatically", "Clips saved while a game is running start as favourites.", &mut d.clips.auto_favorite);
        });
        group(ui, "Library", |ui| {
            toggle_row(ui, c, "Thumbnails", "Generate preview images for clips.", &mut d.clips.generate_thumbnails);
            toggle_row(ui, c, "Import files added by hand", "Videos you copy into the clips folder show up in the library.", &mut d.clips.import_untracked_files);
            let (label, help) = if cv_core::paths::uses_local_storage() {
                ("Move deleted clips to project Trash", "Deleted files stay in the profile’s trash folder.")
            } else {
                ("Delete to Recycle Bin", "Deleted clips can be restored from the Recycle Bin.")
            };
            toggle_row(ui, c, label, help, &mut d.clips.delete_to_recycle_bin);
            row(ui, "Double-press protection", "Ignore a second save within this time.", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.save_cooldown_ms).range(0..=10000).suffix(" ms"));
            });
            row(ui, "", "", |ui| {
                if theme::ghost_button(ui, Some(icon::REFRESH), "Rescan clips folder").clicked() {
                    c.request(Request::RescanLibrary);
                    c.toast("info", "Rescanning…");
                }
            });
        });
    }

    fn hotkeys(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Global hotkeys", |ui| {
            ui.label(theme::small("Work everywhere, including in full-screen games. Click a box and press the keys. Mouse side buttons (Mouse4/Mouse5) work too. Esc cancels, Backspace clears."));
            ui.add_space(6.0);
            self.hotkey_row(ui, c, "save_clip", "Save clip", "Saves the last few minutes of available footage.", &mut d.hotkeys.save_clip);
            self.hotkey_row(ui, c, "save_short", "Save short clip", "Saves only the last \"short clip length\" seconds.", &mut d.hotkeys.save_short_clip);
            self.hotkey_row(ui, c, "marker", "Add marker", "Bookmarks this moment on the 24/7 timeline.", &mut d.hotkeys.add_marker);
            self.hotkey_row(ui, c, "toggle_rec", "Pause/resume 24/7 recording", "", &mut d.hotkeys.toggle_recording);
            self.hotkey_row(ui, c, "open_gui", "Open ClipVault", "", &mut d.hotkeys.open_gui);
        });
        group(ui, "In the player", |ui| {
            ui.add(egui::Label::new(theme::small(
                "Use a key name such as K, Left, F8 or Shift+K. Leave a box empty to disable that binding. Primary and alternate shortcuts work only in the player.",
            )).wrap());
            ui.label(theme::small("Esc closes fullscreen or returns to the library. Ctrl+Z undoes audio changes."));
            if ui.button("Restore player defaults").clicked() {
                d.player.hotkeys.retain(|id, _| !shortcuts::SHORTCUTS.iter().any(|def| def.id == id));
            }
            if let Some(problem) = shortcuts::problem(&d.player, &d.hotkeys) {
                ui.add(egui::Label::new(RichText::new(problem).color(theme::DANGER)).wrap());
            }
            for def in shortcuts::SHORTCUTS {
                ui.push_id(def.id, |ui| {
                    let mut bindings = def.bindings(&d.player).map(str::to_owned);
                    row(ui, def.label, "", |ui| {
                        for (slot, hint) in bindings.iter_mut().zip(["Primary", "Alternate"]) {
                            let input = ui.add(egui::TextEdit::singleline(slot).hint_text(hint).desired_width(120.0))
                                .on_hover_text(format!("{hint} shortcut for {}", def.label));
                            if def.action == shortcuts::Action::JumpLatest {
                                let target = if hint == "Primary" { "shortcut_latest_primary" } else { "shortcut_latest_alternate" };
                                super::editor_layout::remember_interaction_rect(ui, target, input.rect);
                                if hint == "Primary" && ui.ctx().data_mut(|data| data.remove_temp::<bool>(egui::Id::unique("clipvault-qa-scroll-shortcut"))).unwrap_or(false) {
                                    input.scroll_to_me_animation(Some(egui::Align::Center), egui::style::ScrollAnimation::none());
                                }
                            }
                        }
                    });
                    if def.bindings(&d.player) != bindings.each_ref().map(String::as_str) {
                        if bindings.each_ref().map(String::as_str) == def.defaults {
                            d.player.hotkeys.remove(def.id);
                        } else {
                            d.player.hotkeys.insert(def.id.to_string(), bindings);
                        }
                    }
                });
            }
        });
    }

    fn hotkey_row(&mut self, ui: &mut Ui, c: &mut Ctx, id: &'static str, label: &str, help: &str, value: &mut String) {
        row(ui, label, help, |ui| {
            let capturing = self.capturing == Some(id);
            let text = if capturing {
                "Press keys…".to_string()
            } else if value.is_empty() {
                "Not set".to_string()
            } else {
                value.clone()
            };
            let width = field_width(ui.available_width(), 200.0, 36.0);
            let btn = egui::Button::new(RichText::new(&text).monospace().color(if capturing { c.accent } else { theme::TEXT })).truncate();
            if ui.add_sized([width, 30.0], btn).on_hover_text(text).clicked() && !capturing {
                self.capturing = Some(id);
                let _ = c.service.call(Request::SuspendHotkeys { suspend: true });
            }
            if !value.is_empty() && theme::icon_button(ui, icon::CLOSE, 11.0, "Clear").clicked() {
                value.clear();
            }
            if capturing {
                if let Some(res) = capture_hotkey(ui) {
                    match res {
                        Captured::Cancel => {}
                        Captured::Clear => value.clear(),
                        Captured::Combo(s) => match Hotkey::parse(&s) {
                            Ok(Some(hk)) => *value = hk.to_string(),
                            Ok(None) => {}
                            Err(e) => c.toast("warning", format!("{s}: {e}")),
                        },
                    }
                    self.capturing = None;
                    let _ = c.service.call(Request::SuspendHotkeys { suspend: false });
                }
            }
            if let Err(e) = Hotkey::parse(value) {
                ui.label(RichText::new(e.to_string()).color(theme::DANGER).size(12.0));
            }
        });
    }

    fn audio(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        if self.devices.is_none() {
            if let Ok(v) = c.service.call(Request::AudioDevices) {
                self.devices = serde_json::from_value(v).ok();
            }
        }
        group(ui, "Microphone guard", |ui| {
            toggle_row(ui, c, "Record microphone", "Save the microphone on its own track and include it in the full mix.", &mut d.audio.record_mic);
            toggle_row(ui, c, "Choose a preferred mic", "Match microphones by name when Windows renames a reconnected headset.", &mut d.audio.mic_guard);
            row(ui, "Preferred microphones", "ClipVault uses the first connected mic in this list. Use the arrows to reorder it.", |ui| {
                ui.vertical(|ui| {
                    let mut remove = None;
                    let mut swap = None;
                    let n = d.audio.mic_priority.len();
                    for (i, m) in d.audio.mic_priority.iter().enumerate() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("{}.", i + 1)).color(theme::TEXT_FAINT));
                            ui.add(egui::Label::new(m).truncate()).on_hover_text(m);
                            if i > 0 && theme::icon_button(ui, "\u{E70E}", 10.0, "Move up").clicked() {
                                swap = Some(i - 1);
                            }
                            if i + 1 < n && theme::icon_button(ui, icon::CHEVRON_DOWN, 10.0, "Move down").clicked() {
                                swap = Some(i);
                            }
                            if theme::icon_button(ui, icon::CLOSE, 10.0, "Remove").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = swap {
                        d.audio.mic_priority.swap(i, i + 1);
                    }
                    if let Some(i) = remove {
                        d.audio.mic_priority.remove(i);
                    }
                    let inputs: Vec<Endpoint> = self.devices.as_ref().map(|x| x.inputs.clone()).unwrap_or_default();
                    ui.horizontal_wrapped(|ui| {
                        let width = field_width(ui.available_width(), 260.0, 36.0);
                        egui::ComboBox::from_id_salt("add_mic").selected_text("Add a connected mic…").width(width).truncate().show_ui(ui, |ui| {
                            for e in &inputs {
                                let name = cv_core::audio_names::clean_display_name(&e.name);
                                if ui.selectable_label(false, &name).clicked() && !d.audio.mic_priority.iter().any(|m| m.eq_ignore_ascii_case(&name)) {
                                    d.audio.mic_priority.push(name);
                                }
                            }
                        });
                        if theme::icon_button(ui, icon::REFRESH, 11.0, "Refresh devices").clicked() {
                            self.devices = None;
                        }
                    });
                });
            });
            row(ui, "If none are connected", "Choose a fallback when none of your preferred microphones are available.", |ui| {
                egui::ComboBox::from_id_salt("mic_fallback")
                    .selected_text(if d.audio.mic_fallback == "none" { "Record without a microphone" } else { "Windows default mic" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.audio.mic_fallback, "windows_default".to_string(), "Windows default mic");
                        ui.selectable_value(&mut d.audio.mic_fallback, "none".to_string(), "Record without a microphone");
                    });
            });
            row(ui, "Mic volume", "Level saved in the recording (100% = unchanged).", |ui| {
                ui.add(egui::Slider::new(&mut d.audio.mic_gain, 0.0..=4.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            });
            row(ui, "Warn when the mic is silent", "Notify if the mic records nothing for this long while recording (0 = off).", |ui| {
                ui.add(egui::DragValue::new(&mut d.audio.mic_silence_warning_mins).range(0..=600).suffix(" min"));
            });
        });
        group(ui, "App audio", |ui| {
            toggle_row(ui, c, "Record separate app tracks", "Keep individual apps adjustable alongside the full mix.", &mut d.recording.app_tracks);
            if d.recording.app_tracks {
                row(ui, "App track slots", "Maximum number of apps with their own tracks at once. Other apps stay audible in the full mix.", |ui| {
                    ui.add(egui::DragValue::new(&mut d.recording.app_track_count).range(1..=24));
                });
                row(ui, "App track bitrate", "Higher bitrates use more storage.", |ui| {
                    ui.add(egui::DragValue::new(&mut d.recording.app_bitrate_kbps).range(64..=320).suffix(" kbps"));
                });
            }
            row(ui, "Voice chat apps", "Group app tracks under Voice chat in the mixer.", |ui| list_editor(ui, &mut d.audio.voice_apps, &mut self.new_item[0], "Discord.exe"));
            row(ui, "Music apps", "Group app tracks under Music in the mixer. Browser tabs share a track.", |ui| {
                ui.vertical(|ui| {
                    list_editor(ui, &mut d.audio.music_apps, &mut self.new_item[1], "Spotify.exe");
                    if ui.button("Add browsers & media players").clicked() {
                        for app in MEDIA_APPS {
                            if !d.audio.music_apps.iter().any(|x| x.eq_ignore_ascii_case(app)) {
                                d.audio.music_apps.push(app.to_string());
                            }
                        }
                    }
                });
            });
        });
    }

    fn games(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Detection", |ui| {
            toggle_row(ui, c, "Full-screen apps are games", "Anything full-screen that isn't a browser or tool counts as a game.", &mut d.games.fullscreen_is_game);
            row(ui, "Extra game folders", "Anything run from these folders is a game.", |ui| list_editor(ui, &mut d.games.library_folders, &mut self.new_item[2], "E:\\games"));
            row(ui, "Always a game", "Executable names.", |ui| list_editor(ui, &mut d.games.always_games, &mut self.new_item[3], "javaw.exe"));
            row(ui, "Never a game", "Executable names.", |ui| list_editor(ui, &mut d.games.never_games, &mut self.new_item[4], "chrome.exe"));
        });
        if !self.games_loaded {
            self.games = c.db().and_then(|db| db.list_games().ok()).unwrap_or_default();
            self.games_loaded = true;
        }
        group(ui, "Games & apps seen", |ui| {
            ui.label(theme::small("Rename how a game appears, mark whether it's a game, or merge duplicates. Changes apply immediately."));
            ui.add_space(6.0);
            let mut reload = false;
            for g in &mut self.games {
                ui.push_id(g.id, |ui| {
                    let exe = if g.exe_path.is_empty() { g.key.clone() } else { cv_core::games::exe_name_of(&g.exe_path) };
                    ui.horizontal_wrapped(|ui| {
                        let width = ui.available_width().min(200.0);
                        let r = ui.add(egui::TextEdit::singleline(&mut g.name).desired_width(width).min_size(egui::Vec2::ZERO));
                        if r.lost_focus() && !g.name.trim().is_empty() {
                            if let Some(db) = c.db() {
                                let _ = db.rename_game(g.id, g.name.trim());
                            }
                            reload = true;
                        }
                        let mut ov = g.is_game_override;
                        let label = match ov {
                            None => format!("Game · Auto ({})", if g.auto_is_game { "yes" } else { "no" }),
                            Some(true) => "Game · Yes".into(),
                            Some(false) => "Game · No".into(),
                        };
                        egui::ComboBox::from_id_salt("isgame").selected_text(label).width(138.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut ov, None, "Auto");
                            ui.selectable_value(&mut ov, Some(true), "Yes");
                            ui.selectable_value(&mut ov, Some(false), "No");
                        });
                        if ov != g.is_game_override {
                            if let Some(db) = c.db() {
                                let _ = db.set_game_override(g.id, ov);
                            }
                            g.is_game_override = ov;
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("{} clips", g.clip_count));
                            if ui.small_button("Merge…").on_hover_text("Move this game's clips into another game").clicked() {
                                self.merge_from = Some(g.id);
                            }
                        });
                    });
                    ui.add(egui::Label::new(RichText::new(&exe).size(12.0).color(theme::TEXT_FAINT)).wrap()).on_hover_text(exe);
                });
                ui.separator();
            }
            if reload {
                self.games_loaded = false;
            }
        });
        if let Some(from) = self.merge_from {
            let mut close = false;
            let from_name = self.games.iter().find(|g| g.id == from).map(|g| g.name.clone()).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("merge")).show(ui.ctx(), |ui| {
                ui.set_width(dialog_width(ui, 380.0));
                ui.label(theme::title(format!("Merge \"{from_name}\" into…")));
                ui.label(theme::small("Its clips move to the other game (their files stay where they are)."));
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for g in self.games.clone() {
                        if g.id != from && ui.add(egui::Button::new(&g.name).wrap()).clicked() {
                            if let Some(db) = c.db.as_mut() {
                                if let Err(e) = db.merge_games(from, g.id) {
                                    c.toasts.push(crate::app::Toast { level: "error".into(), text: format!("{e:#}"), at: std::time::Instant::now() });
                                }
                            }
                            let _ = c.service.call(Request::Touch);
                            self.games_loaded = false;
                            close = true;
                        }
                    }
                });
                if theme::ghost_button(ui, None, "Cancel").clicked() {
                    close = true;
                }
            });
            if close {
                self.merge_from = None;
            }
        }
    }
}

// ---------------------------------------------------------- free sections

fn recording(ui: &mut Ui, c: &mut Ctx, d: &mut Config, monitors: &mut Option<Vec<MonitorInfo>>) {
    group(ui, "Recording history", |ui| {
        row(ui, "History mode", "Keep an archive or automatically discard older unsaved footage.", |ui| {
            ui.selectable_value(&mut d.recording.history_mode, HistoryMode::Archive, "Full history");
            ui.selectable_value(&mut d.recording.history_mode, HistoryMode::Rolling, "Replay only");
        });
        if d.recording.history_mode == HistoryMode::Rolling {
            row(ui, "Keep the last", "How much unsaved footage stays on disk while recording.", |ui| {
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        for minutes in [1, 2, 5, 10] {
                            ui.selectable_value(&mut d.recording.rolling_minutes, minutes, format!("{minutes} min"));
                        }
                        ui.add(egui::DragValue::new(&mut d.recording.rolling_minutes)
                            .range(1..=MAX_ROLLING_MINUTES).suffix(" min").prefix("Custom "));
                    });
                    ui.label(theme::small(format!(
                        "Up to {} min can be saved. Disk cleanup works in roughly {ROLLING_SEGMENT_SECS}-second segments.",
                        d.recording.rolling_minutes
                    )));
                });
            });
            ui.label(theme::small("Use the clip hotkeys or the Home replay action to save new footage. Replay only keeps it off the 24/7 timeline and clears unsaved footage when ClipVault stops."));
            ui.label(theme::small("Saved clips and existing 24/7 recordings stay in the library. Archive cleanup pauses while Replay only is active."));
        } else {
            toggle_row(ui, c, "Record 24/7", "Keep a searchable timeline. Turning this off keeps a temporary buffer for clip hotkeys.", &mut d.recording.enabled);
            folder_row(ui, "Recordings folder", "Where ClipVault saves continuous recordings.", &mut d.recording.folder);
            row(ui, "Keep this much footage", "Oldest footage is deleted beyond this. Time the PC is off doesn't count.", |ui| {
                let current = d.recording.history_minutes();
                egui::ComboBox::from_id_salt("archive_history_duration")
                    .selected_text(d.recording.history_label()).show_ui(ui, |ui| {
                        for (minutes, label) in [(1, "1 min"), (5, "5 min"), (10, "10 min"), (30, "30 min"), (60, "1 h"), (360, "6 h"), (1440, "24 h"), (2880, "48 h"), (10080, "7 days")] {
                            if ui.selectable_label(minutes == current, label).clicked() {
                                d.recording.set_history_minutes(minutes);
                            }
                        }
                    });
                let mut minutes = d.recording.history_minutes();
                if ui.add(egui::DragValue::new(&mut minutes).range(1..=86400)
                    .prefix("Custom: ").suffix(" min")).changed() {
                    d.recording.set_history_minutes(minutes);
                }
            });
            row(ui, "Also delete footage older than", "0 = never delete by age.", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.max_age_days).range(0..=3650).suffix(" days"));
            });
            row(ui, "Storage cap", "Max space for 24/7 recordings (0 = no cap).", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.max_storage_gb).range(0..=100000).suffix(" GB"));
            });
            row(ui, "Always keep free", "Deletes older footage if free space drops below this amount.", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.min_free_space_gb).range(1..=10000).suffix(" GB"));
            });
            row(ui, "File length", "Shorter segments give retention finer control.", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.segment_minutes).range(1..=240).suffix(" min"));
                let actual = d.recording.archive_segment_seconds();
                if actual < d.recording.segment_minutes * 60 {
                    ui.label(theme::small(format!("Using {actual} s files for this history window.")));
                }
            });
        }
    });
    group(ui, "Screen capture", |ui| {
        row(ui, "Monitor", "Select which screen to record.", |ui| {
            let selected = monitors.as_deref().unwrap_or_default().iter()
                .find(|monitor| monitor.device_name.eq_ignore_ascii_case(&d.recording.monitor));
            let label = if d.recording.monitor.eq_ignore_ascii_case("primary") {
                "Primary monitor".to_string()
            } else {
                selected.map(monitor_label).unwrap_or_else(|| d.recording.monitor.clone())
            };
            let width = field_width(ui.available_width(), 260.0, 38.0);
            egui::ComboBox::from_id_salt("recording_monitor").selected_text(&label).width(width).truncate().show_ui(ui, |ui| {
                ui.selectable_value(&mut d.recording.monitor, "primary".to_string(), "Primary monitor");
                for monitor in monitors.as_deref().unwrap_or_default() {
                    ui.selectable_value(&mut d.recording.monitor, monitor.device_name.clone(), monitor_label(monitor));
                }
            }).response.on_hover_text(label);
            if theme::icon_button(ui, icon::REFRESH, 11.0, "Refresh monitors").clicked() {
                *monitors = None;
            }
        });
        row(ui, "Capture method", "Auto chooses a Windows capture method for your display.", |ui| {
            let methods = [("auto", "Auto"), ("duplication", "Desktop Duplication"), ("graphics_capture", "Windows Graphics Capture")];
            egui::ComboBox::from_id_salt("capture_method").selected_text(label_for(&methods, &d.recording.capture_method)).show_ui(ui, |ui| {
                for (value, label) in methods {
                    ui.selectable_value(&mut d.recording.capture_method, value.to_string(), label);
                }
            });
        });
        toggle_row(ui, c, "Show mouse cursor", "Include the pointer in the video.", &mut d.recording.show_cursor);
    });
    group(ui, "Video and sound quality", |ui| {
        row(ui, "Encoder", "Auto uses a hardware encoder when available.", |ui| {
            ui.selectable_value(&mut d.recording.encoder, "auto".to_string(), "Auto");
            ui.selectable_value(&mut d.recording.encoder, "software".to_string(), "Software");
        });
        row(ui, "Video quality", "Higher quality uses more storage.", |ui| {
            egui::ComboBox::from_id_salt("video_quality").selected_text(label_for(QUALITIES, &d.recording.quality)).show_ui(ui, |ui| {
                for (value, label) in QUALITIES {
                    ui.selectable_value(&mut d.recording.quality, value.to_string(), *label);
                }
            });
        });
        if d.recording.quality == "custom" {
            row(ui, "Video bitrate", "Average bitrate target.", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.bitrate_kbps).range(500..=300000).speed(100).suffix(" kbps"));
            });
        }
        row(ui, "Frame rate", "Maximum frames per second.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.fps).range(10..=240).suffix(" fps"));
        });
        row(ui, "Resolution limit", "Preserves the screen's aspect ratio.", |ui| {
            for (height, label) in HEIGHTS {
                ui.selectable_value(&mut d.recording.max_height, *height, *label);
            }
        });
        row(ui, "Keyframe interval", "Shorter intervals improve seek responsiveness and use slightly more storage.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.keyframe_interval_secs).range(1..=10).suffix(" s"));
        });
        row(ui, "Full mix and mic bitrate", "Per track.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.audio_bitrate_kbps).range(64..=320).suffix(" kbps"));
        });
    });
}

fn monitor_label(monitor: &MonitorInfo) -> String {
    let display = monitor.device_name.rsplit('\\').next().unwrap_or(&monitor.device_name);
    let primary = if monitor.primary { " · primary" } else { "" };
    format!("{display} · {} × {}{primary}", monitor.width, monitor.height)
}

fn overlay(ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
    group(ui, "Clip saved overlay", |ui| {
        toggle_row(ui, c, "Show overlay", "Shows your overlay image when a clip is saved.", &mut d.overlay.enabled);
        file_row(ui, "Image", "Empty = the built-in overlay_img.png.", &mut d.overlay.image_path, &["png", "jpg", "jpeg", "bmp", "gif"]);
        row(ui, "Show for", "", |ui| {
            ui.add(egui::DragValue::new(&mut d.overlay.duration_ms).range(500..=30000).speed(50).suffix(" ms"));
        });
        row(ui, "Position", "", |ui| {
            egui::ComboBox::from_id_salt("ov_pos").selected_text(d.overlay.position.replace('_', " ")).show_ui(ui, |ui| {
                for p in ["top_right", "top_left", "top_center", "bottom_right", "bottom_left"] {
                    ui.selectable_value(&mut d.overlay.position, p.to_string(), p.replace('_', " "));
                }
            });
        });
        row(ui, "Monitor", "", |ui| {
            ui.selectable_value(&mut d.overlay.monitor, "primary".to_string(), "Primary");
            ui.selectable_value(&mut d.overlay.monitor, "active".to_string(), "Where the game is");
        });
        row(ui, "Margin", "Distance from the screen edge.", |ui| {
            ui.add(egui::DragValue::new(&mut d.overlay.margin_px).range(0..=500).suffix(" px"));
        });
        row(ui, "Size", "", |ui| {
            ui.add(egui::Slider::new(&mut d.overlay.scale, 0.25..=3.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
        });
        row(ui, "Opacity", "", |ui| {
            ui.add(egui::Slider::new(&mut d.overlay.opacity, 0.1..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
        });
        ui.label(theme::small("Note: overlays can't appear over games in exclusive full-screen mode (borderless works)."));
    });
    group(ui, "Sounds & toasts", |ui| {
        toggle_row(ui, c, "Play a sound when a clip is saved", "", &mut d.overlay.play_sound);
        file_row(ui, "Sound file", "Empty = the Windows notification sound. Must be a .wav.", &mut d.overlay.sound_path, &["wav"]);
        toggle_row(ui, c, "Small text notices", "Bottom-right notices for markers, mic changes and errors.", &mut d.overlay.show_text_toasts);
    });
}

fn player(ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
    group(ui, "Playback", |ui| {
        row(ui, "Default volume", "", |ui| {
            ui.add(egui::Slider::new(&mut d.player.volume, 0..=100).suffix("%"));
        });
        toggle_row(ui, c, "Autoplay", "Start playing when you open a clip.", &mut d.player.autoplay);
        toggle_row(ui, c, "Loop clips", "", &mut d.player.loop_clips);
        row(ui, "Default speed", "", |ui| {
            ui.add(egui::Slider::new(&mut d.player.default_speed, 0.25..=2.0).step_by(0.25).suffix("x"));
        });
        row(ui, "Small skip", "← / →", |ui| {
            ui.add(egui::DragValue::new(&mut d.player.seek_small_secs).range(1..=60).suffix(" s"));
        });
        row(ui, "Big skip", "Shift + ← / →", |ui| {
            ui.add(egui::DragValue::new(&mut d.player.seek_large_secs).range(5..=600).suffix(" s"));
        });
    });
    group(ui, "Audio tracks", |ui| {
        row(ui, "Default mix", "How multi-track files start playing.", |ui| {
            ui.selectable_value(&mut d.player.default_mix_mode, "full".to_string(), "Full mix");
            ui.selectable_value(&mut d.player.default_mix_mode, "separate".to_string(), "Separate tracks");
        });
        toggle_row(ui, c, "Smooth audio edits", "Fade into and out of edited regions to reduce clicks.", &mut d.player.smooth_edits);
        if d.player.smooth_edits {
            row(ui, "Edit fade length", "Length of the fade at each edit boundary.", |ui| {
                ui.add(egui::DragValue::new(&mut d.player.edit_fade_ms).range(1..=1000).suffix(" ms"));
            });
        }
        toggle_row(ui, c, "Remember each clip's mix", "Reopen clips with the volumes you last used.", &mut d.player.remember_clip_mix);
    });
    group(ui, "24/7 timeline", |ui| {
        row(ui, "Live delay", "How far behind real time \"Go live\" starts.", |ui| {
            ui.add(egui::DragValue::new(&mut d.player.live_offset_secs).range(2..=120).suffix(" s"));
        });
        row(ui, "Default zoom", "Hours visible on the timeline.", |ui| {
            ui.add(egui::DragValue::new(&mut d.player.timeline_hours).range(1..=336).suffix(" h"));
        });
        toggle_row(ui, c, "Show games on the timeline", "Coloured bands showing what you were playing.", &mut d.player.show_activity_on_timeline);
    });
}

fn export(ui: &mut Ui, c: &mut Ctx, d: &mut Config, new_preset_mb: &mut u32) {
    group(ui, "Exports", |ui| {
        let help = if cv_core::paths::uses_local_storage() {
            "Leave empty to use the profile’s exports folder."
        } else {
            "Leave empty to use the _Exports subfolder of your clips folder."
        };
        folder_row(ui, "Export folder", help, &mut d.export.folder);
        row(ui, "Default share limit", "Maximum file size for a Share export.", |ui| {
            ui.add(egui::DragValue::new(&mut d.export.target_size_mb).range(1..=4096).suffix(" MB"));
        });
        row(ui, "Share size presets", "Quick size choices in the export dialog.", |ui| {
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    let mut remove = None;
                    for &size in &d.export.size_presets_mb {
                        if ui.small_button(format!("{size} MB ×")).on_hover_text("Remove preset").clicked() {
                            remove = Some(size);
                        }
                    }
                    if let Some(size) = remove {
                        d.export.size_presets_mb.retain(|&preset| preset != size);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(new_preset_mb).range(1..=4096).suffix(" MB"));
                    if ui.button("Add preset").clicked() && !d.export.size_presets_mb.contains(new_preset_mb) {
                        d.export.size_presets_mb.push(*new_preset_mb);
                        d.export.size_presets_mb.sort_unstable();
                    }
                });
            });
        });
        row(ui, "Max height for shares", "Downscale big recordings when targeting a size (0 = keep).", |ui| {
            for h in [0u32, 720, 1080, 1440] {
                ui.selectable_value(&mut d.export.max_height, h, if h == 0 { "Keep".to_string() } else { format!("{h}p") });
            }
        });
        toggle_row(ui, c, "Copy to clipboard after export", "Paste the finished file straight into Discord.", &mut d.export.copy_to_clipboard);
        toggle_row(ui, c, "Open the folder after export", "", &mut d.export.open_folder_after);
    });
    group(ui, "Encoding", |ui| {
        row(ui, "Export audio bitrate", "Applied to the mixed audio track.", |ui| {
            ui.add(egui::DragValue::new(&mut d.export.audio_bitrate_kbps).range(64..=192).suffix(" kbps"));
        });
    });
}

fn advanced(ui: &mut Ui, _c: &mut Ctx, d: &mut Config) {
    group(ui, "Service", |ui| {
        row(ui, "Local port", "Starting port for the local connection. Takes effect when the background service restarts.", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.ipc_port).range(1024..=65000));
        });
        row(ui, "Activity sampling", "How often the focused window is checked.", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.activity_sample_ms).range(250..=10000).suffix(" ms"));
        });
        row(ui, "Activity history", "Days of game history to keep.", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.activity_history_days).range(1..=3650).suffix(" days"));
        });
    });
    group(ui, "Logging", |ui| {
        row(ui, "Log level", "", |ui| {
            for l in ["error", "warn", "info", "debug"] {
                ui.selectable_value(&mut d.advanced.log_level, l.to_string(), l);
            }
        });
        row(ui, "Keep logs for", "", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.log_retention_days).range(1..=365).suffix(" days"));
        });
    });
}

fn about(ui: &mut Ui, c: &mut Ctx) {
    group(ui, "ClipVault", |ui| {
        ui.label(theme::title(format!("Version {}", cv_core::VERSION)));
        ui.label(theme::label_dim("Records everything, clips anything. Background service + this app."));
        ui.add_space(8.0);
        let s = c.service.status();
        let rows = [
            ("Service", if c.service.connected() { format!("running (v{})", s.as_ref().map(|s| s.version.clone()).unwrap_or_default()) } else { "not running".into() }),
            ("Recorder", s.as_ref().map(|s| if s.engine.running {
                format!("{} · {}", s.engine.capture, s.engine.encoder)
            } else {
                s.engine.last_error.clone()
            }).unwrap_or_default()),
            ("Settings", cv_core::paths::config_path().display().to_string()),
            ("Data", cv_core::paths::data_dir().display().to_string()),
        ];
        for (k, v) in rows {
            row(ui, k, "", |ui| {
                ui.add(egui::Label::new(v).wrap());
            });
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if theme::ghost_button(ui, Some(icon::FOLDER), "Open logs").clicked() {
                let _ = std::process::Command::new("explorer.exe").arg(cv_core::paths::logs_dir()).spawn();
            }
            if theme::ghost_button(ui, Some(icon::FOLDER), "Open data folder").clicked() {
                let _ = std::process::Command::new("explorer.exe").arg(cv_core::paths::data_dir()).spawn();
            }
            if theme::ghost_button(ui, Some(icon::REFRESH), "Restart recorder").on_hover_text("Closes the current file and starts a new recording").clicked() {
                c.service.fire(Request::RestartRecorder);
                c.toast("info", "Restarting recording…");
            }
        });
    });
}

// ------------------------------------------------------------ helpers

fn settings_section_selector(ui: &mut Ui, selected: &mut Tab) -> egui::Response {
    let label = TABS
        .iter()
        .find(|(tab, _, _)| *tab == *selected)
        .map(|(_, _, label)| *label)
        .unwrap_or("General");
    egui::ComboBox::from_id_salt("settings_section")
        .selected_text(label)
        .width(ui.available_width().min(220.0))
        .truncate()
        .show_ui(ui, |ui| {
            for (tab, _, label) in TABS {
                ui.selectable_value(selected, tab, label);
            }
        })
        .response
        .on_hover_text(label)
}

fn group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::title(title));
        ui.add_space(8.0);
        add(ui);
    });
    ui.add_space(14.0);
}

fn dialog_width(ui: &Ui, preferred: f32) -> f32 {
    field_width(ui.ctx().content_rect().width(), preferred, 44.0)
}

fn field_width(available: f32, preferred: f32, trailing: f32) -> f32 {
    (available - trailing).clamp(0.0, preferred)
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn compact_section_switches_keep_selector_and_next_row_anchored() {
        for width in [180.0, 310.0, 620.0] {
            let ctx = egui::Context::default();
            let mut expected = None;
            for (mut selected, _, label) in TABS {
                let mut bounds = None;
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(width, 400.0))),
                        ..Default::default()
                    },
                    |ui| {
                        let selector = settings_section_selector(ui, &mut selected);
                        let next = ui.label("Section content");
                        bounds = Some((selector.rect, next.rect));
                    },
                );
                output.textures_delta.clear();
                let bounds = bounds.unwrap();
                assert_eq!(bounds, *expected.get_or_insert(bounds), "{label} shifted layout at {width}");
            }
        }
    }

    #[test]
    fn control_rows_and_long_paths_fit_across_the_reflow_boundary() {
        for width in [310.0, 435.0, 592.0, 610.0, 630.0, 780.0] {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(width, 512.0))),
                    ..Default::default()
                },
                |ui| {
                    let mut path = format!("C:\\{}", "LongFolderName".repeat(18));
                    group(ui, "Recording storage", |ui| {
                        folder_row(ui, "Recording folder", "Where ClipVault stores continuous recordings.", &mut path);
                        row(ui, "Data", "", |ui| {
                            ui.add(egui::Label::new(&path).wrap());
                        });
                    });
                    assert!(ui.min_rect().right() <= width + 0.5, "Settings row exceeded {width} points: {:?}", ui.min_rect());
                },
            );
            output.textures_delta.clear();
        }
    }
}

fn row(ui: &mut Ui, label: &str, help: &str, add: impl FnOnce(&mut Ui)) {
    if ui.available_width() < 610.0 {
        if !label.is_empty() {
            ui.label(RichText::new(label).color(theme::TEXT));
        }
        if !help.is_empty() {
            ui.add(egui::Label::new(theme::small(help)).wrap());
        }
        ui.horizontal_wrapped(add);
        ui.add_space(10.0);
        return;
    }
    ui.horizontal_top(|ui| {
        let label_width = (ui.available_width() * 0.43).min(300.0);
        ui.allocate_ui_with_layout(Vec2::new(label_width, 20.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(label_width);
            if !label.is_empty() {
                ui.label(RichText::new(label).color(theme::TEXT));
            }
            if !help.is_empty() {
                ui.add(egui::Label::new(theme::small(help)).wrap());
            }
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| add(ui));
    });
    ui.add_space(10.0);
}

fn toggle_row(ui: &mut Ui, c: &Ctx, label: &str, help: &str, v: &mut bool) {
    row(ui, label, help, |ui| {
        theme::toggle(ui, v, c.accent);
    });
}

fn folder_row(ui: &mut Ui, label: &str, help: &str, v: &mut String) {
    row(ui, label, help, |ui| {
        let width = field_width(ui.available_width(), 300.0, 38.0);
        ui.add(egui::TextEdit::singleline(v).desired_width(width));
        if theme::icon_button(ui, icon::FOLDER, 13.0, "Browse").clicked() {
            let start = if v.is_empty() { cv_core::paths::data_dir() } else { std::path::PathBuf::from(&*v) };
            if let Some(p) = rfd::FileDialog::new().set_directory(start).pick_folder() {
                *v = p.display().to_string();
            }
        }
    });
}

fn file_row(ui: &mut Ui, label: &str, help: &str, v: &mut String, exts: &[&str]) {
    row(ui, label, help, |ui| {
        let width = field_width(ui.available_width(), 300.0, 38.0);
        ui.add(egui::TextEdit::singleline(v).desired_width(width));
        if theme::icon_button(ui, icon::FOLDER, 13.0, "Browse").clicked() {
            if let Some(p) = rfd::FileDialog::new().add_filter("file", exts).pick_file() {
                *v = p.display().to_string();
            }
        }
    });
}

fn list_editor(ui: &mut Ui, list: &mut Vec<String>, new_item: &mut String, hint: &str) {
    ui.vertical(|ui| {
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, item) in list.iter().enumerate() {
                egui::Frame::NONE.fill(theme::SURFACE3).corner_radius(egui::CornerRadius::same(12)).inner_margin(Margin::symmetric(10, 3)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let label_width = field_width(ui.available_width(), 320.0, 38.0);
                        ui.add_sized([label_width, 20.0], egui::Label::new(item).truncate()).on_hover_text(item);
                        if ui.add(egui::Button::new(theme::icon_text(icon::CLOSE, 9.0)).frame(false)).clicked() {
                            remove = Some(i);
                        }
                    });
                });
            }
        });
        if let Some(i) = remove {
            list.remove(i);
        }
        ui.horizontal_wrapped(|ui| {
            let width = field_width(ui.available_width(), 200.0, 36.0);
            let r = ui.add(egui::TextEdit::singleline(new_item).hint_text(hint).desired_width(width));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (theme::icon_button(ui, icon::ADD, 11.0, "Add").clicked() || enter) && !new_item.trim().is_empty() {
                let v = new_item.trim().to_string();
                if !list.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
                    list.push(v);
                }
                new_item.clear();
            }
        });
    });
}

/// Browsers and media players offered by the "Add browsers & media players" button.
const MEDIA_APPS: [&str; 7] = ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "opera_gx.exe", "vlc.exe"];

fn label_for(pairs: &[(&str, &str)], key: &str) -> String {
    pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()).unwrap_or_else(|| key.to_string())
}

enum Captured {
    Cancel,
    Clear,
    Combo(String),
}

fn key_name(k: Key) -> Option<String> {
    use Key::*;
    let s = match k {
        Backtick => "`",
        Minus => "-",
        Equals => "=",
        OpenBracket => "[",
        CloseBracket => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Comma => ",",
        Period => ".",
        Slash => "/",
        Space => "Space",
        Enter => "Enter",
        Tab => "Tab",
        Insert => "Insert",
        Delete => "Delete",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        ArrowUp => "Up",
        ArrowDown => "Down",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        other => return Some(other.name().to_string()),
    };
    Some(s.to_string())
}

/// Read the next key combo (or mouse side button) from this frame's input.
fn capture_hotkey(ui: &Ui) -> Option<Captured> {
    ui.input(|i| {
        for e in &i.events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if *key == Key::Escape {
                        return Some(Captured::Cancel);
                    }
                    if *key == Key::Backspace && !modifiers.any() {
                        return Some(Captured::Clear);
                    }
                    let name = key_name(*key)?;
                    return Some(Captured::Combo(combo(modifiers, &name)));
                }
                egui::Event::PointerButton { button, pressed: true, modifiers, .. } => {
                    let name = match button {
                        egui::PointerButton::Extra1 => "Mouse4",
                        egui::PointerButton::Extra2 => "Mouse5",
                        _ => continue,
                    };
                    return Some(Captured::Combo(combo(modifiers, name)));
                }
                _ => {}
            }
        }
        None
    })
}

fn combo(m: &egui::Modifiers, key: &str) -> String {
    let mut parts = Vec::new();
    if m.ctrl {
        parts.push("Ctrl");
    }
    if m.alt {
        parts.push("Alt");
    }
    if m.shift {
        parts.push("Shift");
    }
    parts.push(key);
    parts.join("+")
}
