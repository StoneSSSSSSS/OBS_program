//! Settings: every option in config.toml, grouped into sections, with a
//! save bar that warns when OBS has to restart.

use crate::app::Ctx;
use crate::theme::{self, icon};
use cv_core::audio_names::Endpoint;
use cv_core::config::{Accent, Config, ENCODERS, RECORDING_FORMATS};
use cv_core::hotkey::Hotkey;
use cv_core::ipc::{AudioDevices, Request};
use cv_core::model::Game;
use eframe::egui::{self, Color32, Key, Margin, RichText, Ui, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Obs,
    Advanced,
    About,
}

const TABS: [(Tab, &str, &str); 12] = [
    (Tab::General, icon::SETTINGS, "General"),
    (Tab::Recording, icon::HISTORY, "24/7 recording"),
    (Tab::Clips, icon::LIBRARY, "Clips & replay"),
    (Tab::Hotkeys, icon::KEYBOARD, "Hotkeys"),
    (Tab::Overlay, icon::BOLT, "Overlay & alerts"),
    (Tab::Audio, icon::MIC, "Audio & mic"),
    (Tab::Games, icon::GAME, "Games"),
    (Tab::Player, icon::PLAY, "Player"),
    (Tab::Export, icon::SHARE, "Export & sharing"),
    (Tab::Obs, icon::DEVICE, "OBS"),
    (Tab::Advanced, icon::MORE, "Advanced"),
    (Tab::About, icon::INFO, "About"),
];

pub struct SettingsView {
    tab: Tab,
    draft: Option<Config>,
    saved: Option<Config>,
    capturing: Option<&'static str>,
    devices: Option<AudioDevices>,
    games: Vec<Game>,
    games_loaded: bool,
    new_item: [String; 6],
    confirm_restart: bool,
    merge_from: Option<i64>,
}

impl Default for SettingsView {
    fn default() -> Self {
        Self {
            tab: Tab::General,
            draft: None,
            saved: None,
            capturing: None,
            devices: None,
            games: Vec::new(),
            games_loaded: false,
            new_item: Default::default(),
            confirm_restart: false,
            merge_from: None,
        }
    }
}

impl SettingsView {
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

        if dirty {
            egui::Panel::bottom("save_bar")
                .frame(egui::Frame::NONE.inner_margin(Margin { left: 0, right: 0, top: 10, bottom: 0 }))
                .show(ui, |ui| self.save_bar(ui, c));
        }
        egui::Panel::left("settings_tabs")
            .exact_size(210.0)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(Margin { left: 0, right: 16, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                for (tab, glyph, label) in TABS {
                    let sel = self.tab == tab;
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::click());
                    let bg = if sel { theme::SURFACE3 } else if resp.hovered() { theme::SURFACE2 } else { Color32::TRANSPARENT };
                    ui.painter().rect_filled(rect, egui::CornerRadius::same(8), bg);
                    let fg = if sel { theme::TEXT } else { theme::TEXT_DIM };
                    ui.painter().text(rect.left_center() + egui::vec2(16.0, 0.0), egui::Align2::CENTER_CENTER, glyph, egui::FontId::new(13.0, theme::icons_family()), if sel { c.accent } else { theme::TEXT_FAINT });
                    ui.painter().text(rect.left_center() + egui::vec2(34.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(13.5), fg);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.tab = tab;
                    }
                }
            });

        egui::ScrollArea::vertical().id_salt("settings_scroll").auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(820.0);
            let mut d = self.draft.take().unwrap();
            match self.tab {
                Tab::General => self.general(ui, c, &mut d),
                Tab::Recording => recording(ui, c, &mut d),
                Tab::Clips => self.clips(ui, c, &mut d),
                Tab::Hotkeys => self.hotkeys(ui, c, &mut d),
                Tab::Overlay => overlay(ui, c, &mut d),
                Tab::Audio => self.audio(ui, c, &mut d),
                Tab::Games => self.games(ui, c, &mut d),
                Tab::Player => player(ui, c, &mut d),
                Tab::Export => export(ui, c, &mut d),
                Tab::Obs => obs(ui, c, &mut d),
                Tab::Advanced => advanced(ui, c, &mut d),
                Tab::About => about(ui, c),
            }
            self.draft = Some(d);
        });
        self.restart_modal(ui, c);
    }

    fn save_bar(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let restart = self.saved.as_ref().zip(self.draft.as_ref()).map(|(s, d)| s.obs_restart_needed(d)).unwrap_or(false);
        theme::card().inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::icon_text(icon::INFO, 14.0).color(theme::TEXT_DIM));
                ui.label(RichText::new("You have unsaved changes").color(theme::TEXT));
                if restart {
                    ui.label(theme::label_dim("· some need OBS to restart"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, Some(icon::CHECK), "Save").clicked() {
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
        });
    }

    fn restart_modal(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.confirm_restart {
            return;
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("restart_obs")).show(ui.ctx(), |ui| {
            ui.set_width(420.0);
            ui.label(theme::title("Restart OBS to apply?"));
            ui.label(theme::label_dim("Recording settings only take effect after OBS restarts. That leaves a gap of about 10 seconds in the 24/7 recording."));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, Some(icon::REFRESH), "Save & restart OBS").clicked() {
                    choice = Some(true);
                }
                if theme::ghost_button(ui, None, "Save, restart later").clicked() {
                    choice = Some(false);
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

    fn save(&mut self, ctx: &egui::Context, c: &mut Ctx, restart_obs: bool) {
        let Some(mut d) = self.draft.clone() else { return };
        d.sanitize();
        if let Err(e) = d.save() {
            c.toast("error", format!("Couldn't save settings: {e:#}"));
            return;
        }
        self.saved = Some(d.clone());
        self.draft = Some(d.clone());
        let r = c.service.call(Request::ReloadConfig { restart_obs });
        match r {
            Ok(_) => c.toast("success", if restart_obs { "Saved, restarting OBS" } else { "Settings saved" }),
            Err(e) => c.toast("warning", format!("Saved; the service will pick it up when it starts ({e})")),
        }
        c.reload_config();
        theme::apply(ctx, c.accent, c.cfg.general.ui_scale);
    }

    // ------------------------------------------------------------- tabs

    fn general(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Startup", |ui| {
            toggle_row(ui, c, "Start with Windows", "Runs the background service at login. It launches OBS and keeps everything recording.", &mut d.general.start_with_windows);
            toggle_row(ui, c, "Show tray icon", "A small icon next to the clock: red when recording, amber when something needs attention.", &mut d.general.show_tray_icon);
            toggle_row(ui, c, "Windows notifications", "Pop-ups when the mic disconnects, OBS crashes, or the disk is nearly full.", &mut d.general.notifications);
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
        group(ui, "Clip length", |ui| {
            row(ui, "Default clip length", "How much the replay buffer keeps: a saved clip is the last N seconds. Requires OBS restart.", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.clip_length_secs).range(5..=3600).suffix(" s"));
                ui.label(theme::small(cv_core::util::format_duration(d.clips.clip_length_secs as f64)));
            });
            row(ui, "Short clip length", "Length of the \"save short clip\" hotkey (trimmed from the replay).", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.short_clip_secs).range(5..=600).suffix(" s"));
            });
            row(ui, "Replay buffer memory limit", "Upper bound on RAM the replay buffer may use. Requires OBS restart.", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.replay_max_memory_mb).range(256..=65536).suffix(" MB"));
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
            row(ui, "Game detection window", "How far back to look when deciding which game a clip is from.", |ui| {
                ui.add(egui::DragValue::new(&mut d.clips.attribution_window_secs).range(5..=3600).suffix(" s"));
            });
            toggle_row(ui, c, "Favourite game clips automatically", "Clips saved while a game is running start as favourites.", &mut d.clips.auto_favorite);
        });
        group(ui, "Library", |ui| {
            toggle_row(ui, c, "Thumbnails", "Generate preview images for clips.", &mut d.clips.generate_thumbnails);
            toggle_row(ui, c, "Import files added by hand", "Videos you copy into the clips folder show up in the library.", &mut d.clips.import_untracked_files);
            toggle_row(ui, c, "Delete to Recycle Bin", "Deleted clips can be restored from the Recycle Bin.", &mut d.clips.delete_to_recycle_bin);
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
            self.hotkey_row(ui, c, "save_clip", "Save clip", "Saves the replay buffer (the last few minutes).", &mut d.hotkeys.save_clip);
            self.hotkey_row(ui, c, "save_short", "Save short clip", "Saves only the last \"short clip length\" seconds.", &mut d.hotkeys.save_short_clip);
            self.hotkey_row(ui, c, "marker", "Add marker", "Bookmarks this moment on the 24/7 timeline.", &mut d.hotkeys.add_marker);
            self.hotkey_row(ui, c, "toggle_rec", "Pause/resume 24/7 recording", "", &mut d.hotkeys.toggle_recording);
            self.hotkey_row(ui, c, "open_gui", "Open ClipVault", "", &mut d.hotkeys.open_gui);
        });
        group(ui, "In the player", |ui| {
            for (k, v) in [
                ("Space / K", "Play / pause"),
                ("← / →", "Back / forward (Shift for bigger jumps)"),
                (", / .", "Previous / next frame"),
                ("I / O", "Mark in / out"),
                ("↑ / ↓", "Volume"),
                ("M", "Mute"),
                ("F / double-click", "Fullscreen"),
                ("Esc", "Leave fullscreen / back to library"),
            ] {
                ui.horizontal(|ui| {
                    ui.add_sized([150.0, 20.0], egui::Label::new(RichText::new(k).monospace().color(theme::TEXT)));
                    ui.label(theme::label_dim(v));
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
            let btn = egui::Button::new(RichText::new(text).monospace().color(if capturing { c.accent } else { theme::TEXT })).min_size(Vec2::new(200.0, 30.0));
            if ui.add(btn).clicked() && !capturing {
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
            ui.label(theme::small("When a USB headset drops and reconnects, Windows often registers it as a new device (\"2- Headset…\") and OBS keeps recording the old, silent one. ClipVault matches mics by name and switches OBS to the connected one within a couple of seconds."));
            ui.add_space(4.0);
            toggle_row(ui, c, "Keep the mic connected", "Watch audio devices and re-point OBS automatically.", &mut d.audio.mic_guard);
            row(ui, "Preferred microphones", "First connected one wins. Drag order with the arrows.", |ui| {
                ui.vertical(|ui| {
                    let mut remove = None;
                    let mut swap = None;
                    let n = d.audio.mic_priority.len();
                    for (i, m) in d.audio.mic_priority.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}.", i + 1)).color(theme::TEXT_FAINT));
                            ui.label(m);
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
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("add_mic").selected_text("Add a connected mic…").width(260.0).show_ui(ui, |ui| {
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
            row(ui, "If none are connected", "What OBS records when no preferred mic is plugged in.", |ui| {
                egui::ComboBox::from_id_salt("mic_fallback")
                    .selected_text(if d.audio.mic_fallback == "none" { "Leave OBS alone" } else { "Windows default mic" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.audio.mic_fallback, "windows_default".to_string(), "Windows default mic");
                        ui.selectable_value(&mut d.audio.mic_fallback, "none".to_string(), "Leave OBS alone");
                    });
            });
            row(ui, "Mic volume", "Gain applied in OBS (100% = unchanged).", |ui| {
                ui.add(egui::Slider::new(&mut d.audio.mic_gain, 0.0..=4.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            });
            toggle_row(ui, c, "Noise suppression", "Adds OBS's RNNoise filter to your mic (affects recordings).", &mut d.audio.mic_noise_suppression);
            row(ui, "Warn when the mic is silent", "Notify if the mic records nothing for this long while recording (0 = off).", |ui| {
                ui.add(egui::DragValue::new(&mut d.audio.mic_silence_warning_mins).range(0..=600).suffix(" min"));
            });
        });
        group(ui, "Separate audio tracks", |ui| {
            toggle_row(ui, c, "Record separate tracks", "Six tracks: full mix, game, voice chat, music, mic, desktop. Lets you rebalance clips later. Requires OBS restart.", &mut d.audio.multitrack);
            toggle_row(ui, c, "Game track follows your game", "Points the Game track at whichever game you're playing.", &mut d.audio.game_audio_capture);
            toggle_row(ui, c, "Keep apps on other devices in the full mix", "e.g. Discord playing on a headset's \"chat\" channel instead of the default device.", &mut d.audio.auto_mix_other_devices);
            row(ui, "Voice chat apps", "Recorded on the Voice chat track.", |ui| list_editor(ui, &mut d.audio.voice_apps, &mut self.new_item[0], "Discord.exe"));
            row(ui, "Music apps", "Recorded on the Music track. Sound is split per program, not per browser tab.", |ui| {
                ui.vertical(|ui| {
                    list_editor(ui, &mut d.audio.music_apps, &mut self.new_item[1], "Spotify.exe");
                    if ui.button("Add browsers & media players").on_hover_text("YouTube, Twitch and other browser audio get their own slider (all browser tabs share it)").clicked() {
                        for app in MEDIA_APPS {
                            if !d.audio.music_apps.iter().any(|x| x.eq_ignore_ascii_case(app)) {
                                d.audio.music_apps.push(app.to_string());
                            }
                        }
                    }
                });
            });
            ui.label(theme::small("Apps that aren't on the Voice chat or Music lists (and aren't a game) are only in the Full mix and Desktop tracks."));
            ui.add_space(8.0);
            row(ui, "Desktop audio device", "\"default\" follows Windows. Or pin a specific output.", |ui| {
                let outputs: Vec<Endpoint> = self.devices.as_ref().map(|x| x.outputs.clone()).unwrap_or_default();
                egui::ComboBox::from_id_salt("desktop_dev").selected_text(d.audio.desktop_device.clone()).width(280.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.audio.desktop_device, "default".to_string(), "default (follow Windows)");
                    for e in &outputs {
                        let name = cv_core::audio_names::clean_display_name(&e.name);
                        ui.selectable_value(&mut d.audio.desktop_device, name.clone(), name);
                    }
                });
            });
        });
    }

    fn games(&mut self, ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
        group(ui, "Detection", |ui| {
            toggle_row(ui, c, "Full-screen apps are games", "Anything full-screen that isn't a browser or tool counts as a game.", &mut d.games.fullscreen_is_game);
            row(ui, "Game follow delay", "A game must be focused this long before the Game track switches to it.", |ui| {
                ui.add(egui::DragValue::new(&mut d.games.audio_follow_delay_secs).range(0..=60).suffix(" s"));
            });
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
            egui::Grid::new("games_grid").num_columns(4).spacing([12.0, 8.0]).striped(false).show(ui, |ui| {
                ui.label(theme::small("NAME"));
                ui.label(theme::small("EXECUTABLE"));
                ui.label(theme::small("IS A GAME"));
                ui.label(theme::small("CLIPS"));
                ui.end_row();
                for g in self.games.iter_mut() {
                    let r = ui.add(egui::TextEdit::singleline(&mut g.name).desired_width(200.0));
                    if r.lost_focus() && !g.name.trim().is_empty() {
                        if let Some(db) = c.db() {
                            let _ = db.rename_game(g.id, g.name.trim());
                        }
                        reload = true;
                    }
                    let exe = if g.exe_path.is_empty() { g.key.clone() } else { cv_core::games::exe_name_of(&g.exe_path) };
                    ui.label(RichText::new(exe).size(12.0).color(theme::TEXT_FAINT));
                    let mut ov = g.is_game_override;
                    let label = match ov {
                        None => format!("Auto ({})", if g.auto_is_game { "yes" } else { "no" }),
                        Some(true) => "Yes".into(),
                        Some(false) => "No".into(),
                    };
                    egui::ComboBox::from_id_salt(("isgame", g.id)).selected_text(label).width(110.0).show_ui(ui, |ui| {
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
                    ui.horizontal(|ui| {
                        ui.label(g.clip_count.to_string());
                        if ui.small_button("Merge…").on_hover_text("Move this game's clips into another game").clicked() {
                            self.merge_from = Some(g.id);
                        }
                    });
                    ui.end_row();
                }
            });
            if reload {
                self.games_loaded = false;
            }
        });
        if let Some(from) = self.merge_from {
            let mut close = false;
            let from_name = self.games.iter().find(|g| g.id == from).map(|g| g.name.clone()).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("merge")).show(ui.ctx(), |ui| {
                ui.set_width(380.0);
                ui.label(theme::title(format!("Merge \"{from_name}\" into…")));
                ui.label(theme::small("Its clips move to the other game (their files stay where they are)."));
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for g in self.games.clone() {
                        if g.id != from && ui.button(&g.name).clicked() {
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

fn recording(ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
    group(ui, "Continuous recording", |ui| {
        toggle_row(ui, c, "Record 24/7", "Keep a rolling recording so you can go back and clip anything later.", &mut d.recording.enabled);
        folder_row(ui, "Recordings folder", "Requires OBS restart.", &mut d.recording.folder);
        row(ui, "Keep this much footage", "Oldest footage is deleted beyond this. Time the PC is off doesn't count.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.retention_hours).range(1..=1440).suffix(" h"));
            ui.label(theme::small(format!("≈ {:.1} days of recording", d.recording.retention_hours as f32 / 24.0)));
        });
        row(ui, "Also delete footage older than", "0 = never delete by age.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.max_age_days).range(0..=3650).suffix(" days"));
        });
        row(ui, "Storage cap", "Max space for 24/7 recordings (0 = no cap).", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.max_storage_gb).range(0..=100000).suffix(" GB"));
        });
        row(ui, "Always keep free", "Deletes old footage early if the drive gets this full.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.min_free_space_gb).range(1..=10000).suffix(" GB"));
        });
        row(ui, "File length", "The recording is split into files this long. Shorter files make cuts faster. Requires OBS restart.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.segment_minutes).range(1..=240).suffix(" min"));
        });
        row(ui, "File format", "Fragmented MP4 survives crashes and power cuts without corruption.", |ui| {
            egui::ComboBox::from_id_salt("format").selected_text(label_for(RECORDING_FORMATS, &d.recording.format)).width(320.0).show_ui(ui, |ui| {
                for (k, v) in RECORDING_FORMATS {
                    ui.selectable_value(&mut d.recording.format, k.to_string(), *v);
                }
            });
        });
    });
    group(ui, "Quality (requires OBS restart)", |ui| {
        row(ui, "Encoder", "", |ui| {
            egui::ComboBox::from_id_salt("encoder").selected_text(label_for(ENCODERS, &d.recording.encoder)).width(280.0).show_ui(ui, |ui| {
                for (k, v) in ENCODERS {
                    ui.selectable_value(&mut d.recording.encoder, k.to_string(), *v);
                }
            });
        });
        row(ui, "Rate control", "CQP = constant quality (recommended). CBR/VBR = fixed bitrate.", |ui| {
            for rc in ["CQP", "VBR", "CBR"] {
                ui.selectable_value(&mut d.recording.rate_control, rc.to_string(), rc);
            }
        });
        if d.recording.rate_control == "CQP" {
            row(ui, "Quality level", "Lower = better quality and bigger files. 18-28 is typical.", |ui| {
                ui.add(egui::Slider::new(&mut d.recording.cqp, 10..=40));
            });
        } else {
            row(ui, "Bitrate", "Video bitrate.", |ui| {
                ui.add(egui::DragValue::new(&mut d.recording.bitrate_kbps).range(500..=300000).speed(100).suffix(" kbps"));
                ui.label(theme::small(format!("≈ {:.1} GB/hour", d.recording.bitrate_kbps as f64 * 3600.0 / 8.0 / 1024.0 / 1024.0)));
            });
            if d.recording.rate_control == "VBR" {
                row(ui, "Max bitrate", "", |ui| {
                    ui.add(egui::DragValue::new(&mut d.recording.max_bitrate_kbps).range(500..=400000).speed(100).suffix(" kbps"));
                });
            }
        }
        row(ui, "Preset", "NVENC: p1 fastest … p7 best quality.", |ui| {
            egui::ComboBox::from_id_salt("preset").selected_text(d.recording.preset.clone()).show_ui(ui, |ui| {
                let presets: &[&str] = if d.recording.encoder.contains("x264") { &["ultrafast", "superfast", "veryfast", "faster", "fast", "medium"] } else { &["p1", "p2", "p3", "p4", "p5", "p6", "p7"] };
                for p in presets {
                    ui.selectable_value(&mut d.recording.preset, p.to_string(), *p);
                }
            });
        });
        row(ui, "Frame rate", "", |ui| {
            for f in [30, 60, 120, 144] {
                ui.selectable_value(&mut d.recording.fps, f, format!("{f} fps"));
            }
        });
        row(ui, "Output resolution", "Downscale the recording (empty = same as your screen), e.g. 1920x1080.", |ui| {
            ui.add(egui::TextEdit::singleline(&mut d.recording.output_resolution).hint_text("native").desired_width(140.0));
        });
        row(ui, "Keyframe interval", "Smaller = more precise instant cuts, slightly bigger files.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.keyframe_interval_secs).range(1..=10).suffix(" s"));
        });
        row(ui, "Audio bitrate", "Per track.", |ui| {
            egui::ComboBox::from_id_salt("abr").selected_text(format!("{} kbps", d.recording.audio_bitrate_kbps)).show_ui(ui, |ui| {
                for b in [96, 128, 160, 192, 256, 320] {
                    ui.selectable_value(&mut d.recording.audio_bitrate_kbps, b, format!("{b} kbps"));
                }
            });
        });
    });
    group(ui, "Reliability", |ui| {
        toggle_row(ui, c, "Restart recording if it stops", "If OBS stops recording on its own, start it again.", &mut d.recording.auto_restart);
        row(ui, "Stall detection", "Restart the recording if the file stops growing for this long.", |ui| {
            ui.add(egui::DragValue::new(&mut d.recording.stall_timeout_secs).range(20..=600).suffix(" s"));
        });
    });
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
        row(ui, "Hardware decoding", "Use the GPU to decode video. Try \"auto-copy-safe\" if playback looks wrong.", |ui| {
            for h in ["auto-safe", "auto-copy-safe", "no"] {
                ui.selectable_value(&mut d.player.hwdec, h.to_string(), h);
            }
        });
    });
    group(ui, "Audio tracks", |ui| {
        row(ui, "Default mix", "How multi-track files start playing.", |ui| {
            ui.selectable_value(&mut d.player.default_mix_mode, "full".to_string(), "Full mix");
            ui.selectable_value(&mut d.player.default_mix_mode, "separate".to_string(), "Separate tracks");
        });
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

fn export(ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
    group(ui, "Exports", |ui| {
        folder_row(ui, "Export folder", "Empty = <clips folder>\\_Exports.", &mut d.export.folder);
        row(ui, "Share size target", "Default size for \"Share\" exports (Discord's free limit is 10 MB).", |ui| {
            ui.add(egui::DragValue::new(&mut d.export.target_size_mb).range(1..=4096).suffix(" MB"));
        });
        row(ui, "Max height for shares", "Downscale big recordings when targeting a size (0 = keep).", |ui| {
            for h in [0u32, 720, 1080, 1440] {
                ui.selectable_value(&mut d.export.max_height, h, if h == 0 { "Keep".to_string() } else { format!("{h}p") });
            }
        });
        toggle_row(ui, c, "Copy to clipboard after export", "Paste the finished file straight into Discord.", &mut d.export.copy_to_clipboard);
        toggle_row(ui, c, "Open the folder after export", "", &mut d.export.open_folder_after);
    });
    group(ui, "Cutting & encoding", |ui| {
        toggle_row(ui, c, "Frame-exact cuts by default", "Re-encodes when clipping/trimming (slower). Off = instant lossless cuts.", &mut d.export.precise_cuts);
        row(ui, "Encoder for re-encodes", "", |ui| {
            for e in ["h264_nvenc", "hevc_nvenc", "libx264"] {
                ui.selectable_value(&mut d.export.video_encoder, e.to_string(), e);
            }
        });
        row(ui, "Re-encode quality", "Lower = better (CQ/CRF).", |ui| {
            ui.add(egui::Slider::new(&mut d.export.quality, 15..=35));
        });
        row(ui, "Export audio bitrate", "", |ui| {
            for b in [128u32, 160, 192, 256, 320] {
                ui.selectable_value(&mut d.export.audio_bitrate_kbps, b, format!("{b}k"));
            }
        });
    });
}

fn obs(ui: &mut Ui, c: &mut Ctx, d: &mut Config) {
    group(ui, "OBS Studio", |ui| {
        file_row(ui, "OBS executable", "", &mut d.obs.exe_path, &["exe"]);
        toggle_row(ui, c, "Launch OBS automatically", "Start OBS when the service starts.", &mut d.obs.launch_on_start);
        row(ui, "Launch delay", "Wait after login before starting OBS.", |ui| {
            ui.add(egui::DragValue::new(&mut d.obs.launch_delay_secs).range(0..=600).suffix(" s"));
        });
        toggle_row(ui, c, "Start minimised to tray", "", &mut d.obs.minimize_to_tray);
        toggle_row(ui, c, "Restart OBS if it crashes", "", &mut d.obs.restart_on_crash);
        toggle_row(ui, c, "Restart OBS even if I close it", "Off: closing OBS yourself pauses recording until you start it again.", &mut d.obs.restart_if_closed);
        row(ui, "Crash restart limit", "Stop auto-restarting after this many restarts in an hour.", |ui| {
            ui.add(egui::DragValue::new(&mut d.obs.max_restarts_per_hour).range(1..=100).suffix(" / hour"));
        });
        row(ui, "Process priority", "Higher priority helps avoid dropped frames while gaming. Requires OBS restart.", |ui| {
            for (k, v) in [("normal", "Normal"), ("above_normal", "Above normal"), ("high", "High")] {
                ui.selectable_value(&mut d.obs.process_priority, k.to_string(), v);
            }
        });
        toggle_row(ui, c, "Let ClipVault manage OBS output settings", "Writes the recording/replay/track settings into OBS. Turn off to configure OBS by hand.", &mut d.obs.manage_settings);
        row(ui, "Shutdown timeout", "How long to wait for OBS to close before ending it.", |ui| {
            ui.add(egui::DragValue::new(&mut d.obs.shutdown_timeout_secs).range(3..=120).suffix(" s"));
        });
        row(ui, "", "", |ui| {
            if theme::ghost_button(ui, Some(icon::REFRESH), "Restart OBS now").clicked() {
                c.service.fire(Request::RestartObs);
                c.toast("info", "Restarting OBS…");
            }
        });
    });
    group(ui, "Connection", |ui| {
        row(ui, "Websocket port", "", |ui| {
            ui.add(egui::DragValue::new(&mut d.obs.websocket_port).range(1024..=65535));
        });
        row(ui, "Websocket password", "Empty = use OBS's own (recommended).", |ui| {
            ui.add(egui::TextEdit::singleline(&mut d.obs.websocket_password).password(true).desired_width(200.0));
        });
        folder_row(ui, "OBS settings folder", "Empty = %APPDATA%\\obs-studio.", &mut d.obs.config_dir);
    });
}

fn advanced(ui: &mut Ui, _c: &mut Ctx, d: &mut Config) {
    group(ui, "Tools", |ui| {
        file_row(ui, "ffmpeg.exe", "Empty = auto-detect.", &mut d.advanced.ffmpeg_path, &["exe"]);
        file_row(ui, "ffprobe.exe", "Empty = next to ffmpeg.", &mut d.advanced.ffprobe_path, &["exe"]);
        file_row(ui, "libmpv-2.dll", "Empty = next to ClipVault. Takes effect on restart of the app.", &mut d.advanced.mpv_dll_path, &["dll"]);
    });
    group(ui, "Service", |ui| {
        row(ui, "Local port", "Port the app uses to talk to the background service.", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.ipc_port).range(1024..=65000));
        });
        row(ui, "Health check interval", "", |ui| {
            ui.add(egui::DragValue::new(&mut d.advanced.health_check_secs).range(1..=60).suffix(" s"));
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
            ("OBS", s.as_ref().map(|s| if s.obs.connected { format!("connected (OBS {})", s.obs.version) } else if s.obs.running { "running, connecting".into() } else { "not running".into() }).unwrap_or_default()),
            ("Video engine", if c.player.mpv.is_some() { "libmpv loaded".into() } else { c.player.init_error.clone().unwrap_or_default() }),
            ("Settings", cv_core::paths::config_path().display().to_string()),
            ("Data", cv_core::paths::data_dir().display().to_string()),
        ];
        egui::Grid::new("about").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            for (k, v) in rows {
                ui.label(theme::label_dim(k));
                ui.label(v);
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if theme::ghost_button(ui, Some(icon::FOLDER), "Open logs").clicked() {
                let _ = std::process::Command::new("explorer.exe").arg(cv_core::paths::logs_dir()).spawn();
            }
            if theme::ghost_button(ui, Some(icon::FOLDER), "Open data folder").clicked() {
                let _ = std::process::Command::new("explorer.exe").arg(cv_core::paths::data_dir()).spawn();
            }
            if theme::ghost_button(ui, Some(icon::REFRESH), "Restart service").on_hover_text("OBS keeps recording while the service restarts").clicked() {
                c.service.fire(Request::Shutdown);
                c.toast("info", "Restarting the service…");
            }
        });
    });
}

// ------------------------------------------------------------ helpers

fn group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::title(title));
        ui.add_space(8.0);
        add(ui);
    });
    ui.add_space(14.0);
}

fn row(ui: &mut Ui, label: &str, help: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(300.0, 20.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(300.0);
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
        ui.add(egui::TextEdit::singleline(v).desired_width(300.0));
        if theme::icon_button(ui, icon::FOLDER, 13.0, "Browse").clicked() {
            if let Some(p) = rfd::FileDialog::new().set_directory(if v.is_empty() { "C:\\" } else { v.as_str() }).pick_folder() {
                *v = p.display().to_string();
            }
        }
    });
}

fn file_row(ui: &mut Ui, label: &str, help: &str, v: &mut String, exts: &[&str]) {
    row(ui, label, help, |ui| {
        ui.add(egui::TextEdit::singleline(v).desired_width(300.0));
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
                        ui.label(item);
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
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(new_item).hint_text(hint).desired_width(200.0));
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
