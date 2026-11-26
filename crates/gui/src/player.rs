//! The player: video surface, transport controls and the per-track mixer.
//! Shared by the clip view and the 24/7 recordings view.

use crate::mpv::Mpv;
use crate::theme::{self, icon};
use cv_core::config::{PlayerConfig, TRACK_NAMES};
use cv_core::model::AudioEdit;
use cv_core::util::format_duration;
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Key, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixMode {
    Full,
    Separate,
}

pub struct Player {
    pub mpv: Option<Arc<Mpv>>,
    pub init_error: Option<String>,
    /// Path currently loaded.
    pub path: Option<String>,
    pub live: bool,
    pub track_count: usize,
    pub track_names: Vec<String>,
    pub mode: MixMode,
    /// Per-track gains used in "separate" mode.
    pub stem_gains: Vec<f32>,
    /// Non-destructive per-track edits in the loaded file's time.
    pub edits: Vec<AudioEdit>,
    pub volume: f32,
    pub muted: bool,
    pub show_mixer: bool,
    pub fullscreen: bool,
    pub scrubbing: bool,
    pub speeds: [f64; 7],
    last_seek: std::time::Instant,
}

impl Player {
    pub fn new(mpv: Result<Arc<Mpv>, String>, cfg: &PlayerConfig) -> Player {
        let (mpv, init_error) = match mpv {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e)),
        };
        Player {
            mpv,
            init_error,
            path: None,
            live: false,
            track_count: 0,
            track_names: Vec::new(),
            mode: if cfg.default_mix_mode == "separate" { MixMode::Separate } else { MixMode::Full },
            stem_gains: Vec::new(),
            edits: Vec::new(),
            volume: cfg.volume as f32 / 100.0,
            muted: false,
            show_mixer: true,
            fullscreen: false,
            scrubbing: false,
            speeds: [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0],
            last_seek: std::time::Instant::now(),
        }
    }

    pub fn state(&self) -> crate::mpv::PlayState {
        self.mpv.as_ref().map(|m| m.state()).unwrap_or_default()
    }

    pub fn names_for(tracks: usize, stored: &[String]) -> Vec<String> {
        (0..tracks)
            .map(|i| {
                if let Some(n) = stored.get(i).filter(|n| !n.is_empty()) {
                    n.clone()
                } else if tracks == TRACK_NAMES.len() {
                    TRACK_NAMES[i].to_string()
                } else if tracks == 1 {
                    "Audio".to_string()
                } else {
                    format!("Track {}", i + 1)
                }
            })
            .collect()
    }

    /// Gains actually sent to mpv for the current mode.
    pub fn effective_gains(&self) -> Vec<f32> {
        let n = self.track_count.max(1);
        if n == 1 {
            return vec![1.0];
        }
        match self.mode {
            MixMode::Full => {
                let mut g = vec![0.0; n];
                g[0] = 1.0;
                g
            }
            MixMode::Separate => {
                let mut g = self.stem_gains.clone();
                g.resize(n, 1.0);
                g
            }
        }
    }

    /// Gain expressions (mode gains x edits) for every track.
    pub fn exprs(&self) -> Vec<String> {
        cv_core::mixer::exprs_for(&self.effective_gains(), &self.edits, 0.0)
    }

    /// Push the current gains/edits to mpv (`changed` = only that track).
    pub fn apply_audio(&self, changed: Option<usize>) {
        if let Some(m) = &self.mpv {
            m.set_exprs(&self.exprs(), changed);
        }
    }

    /// Replace the edits (file time) and apply them.
    pub fn set_edits(&mut self, edits: Vec<AudioEdit>) {
        if edits != self.edits {
            self.edits = edits;
            self.apply_audio(None);
        }
    }

    pub fn set_track_gain(&mut self, track: usize, gain: f32) {
        if self.stem_gains.len() <= track {
            self.stem_gains.resize(track + 1, 1.0);
        }
        self.stem_gains[track] = gain;
        if track > 0 && self.mode == MixMode::Full {
            self.set_mode(MixMode::Separate);
        } else {
            self.apply_audio(Some(track));
        }
    }

    /// Open a file. `tracks` = known audio track count (0 = unknown).
    #[allow(clippy::too_many_arguments)]
    pub fn open(&mut self, path: &str, start: f64, tracks: usize, stored_names: &[String], saved_mix: Option<&[f32]>, live: bool, autoplay: bool, cfg: &PlayerConfig) {
        let Some(mpv) = self.mpv.clone() else { return };
        self.track_count = tracks;
        self.track_names = Self::names_for(tracks, stored_names);
        let multitrack = tracks == TRACK_NAMES.len();
        self.stem_gains = match saved_mix {
            Some(g) if g.len() == tracks && tracks > 1 => {
                self.mode = if g.first().copied().unwrap_or(0.0) > 0.0 && g.iter().skip(1).all(|v| *v == 0.0) { MixMode::Full } else { MixMode::Separate };
                g.to_vec()
            }
            _ => {
                self.mode = if cfg.default_mix_mode == "separate" && tracks > 1 { MixMode::Separate } else { MixMode::Full };
                cv_core::mixer::default_gains(tracks, "separate", multitrack)
            }
        };
        if tracks > 1 && self.stem_gains.iter().all(|g| *g == 0.0) {
            self.stem_gains = cv_core::mixer::default_gains(tracks, "separate", multitrack);
        }
        let exprs = self.exprs();
        let same_file = self.path.as_deref() == Some(path) && self.live == live;
        if same_file && mpv.state().loaded {
            mpv.seek_abs(start, true);
            if autoplay {
                mpv.set_pause(false);
            }
            return;
        }
        self.path = Some(path.to_string());
        self.live = live;
        if let Err(e) = mpv.load(path, start, tracks, &exprs, live, !autoplay) {
            tracing::warn!("load {path}: {e:#}");
        }
        let _ = mpv.set("speed", &format!("{}", cfg.default_speed));
        self.apply_volume();
    }

    /// After a file with unknown track count loads, configure mixing.
    pub fn after_load_check(&mut self) {
        let Some(mpv) = self.mpv.clone() else { return };
        if self.track_count == 0 && mpv.state().loaded {
            let n = mpv.audio_track_count();
            if n > 0 {
                self.track_count = n;
                self.track_names = Self::names_for(n, &[]);
                self.stem_gains = cv_core::mixer::default_gains(n, "separate", n == TRACK_NAMES.len());
                if n > 1 {
                    let lc = cv_core::mixer::mpv_lavfi_complex(n).unwrap_or_default();
                    let _ = mpv.set("lavfi-complex", &lc);
                    let _ = mpv.set("af", &cv_core::mixer::mpv_af(&self.exprs()));
                }
            }
        }
    }

    pub fn close(&mut self) {
        if let Some(m) = &self.mpv {
            m.stop();
        }
        self.path = None;
        self.live = false;
    }

    pub fn apply_volume(&self) {
        if let Some(m) = &self.mpv {
            let _ = m.set("volume", &format!("{:.0}", self.volume * 100.0));
            let _ = m.set("mute", if self.muted { "yes" } else { "no" });
        }
    }

    pub fn toggle_pause(&self) {
        if let Some(m) = &self.mpv {
            let st = m.state();
            if st.eof && st.paused {
                m.seek_abs(0.0, true);
            }
            m.set_pause(!st.paused);
        }
    }

    pub fn seek(&mut self, t: f64, exact: bool) {
        if let Some(m) = &self.mpv {
            m.seek_abs(t, exact);
            self.last_seek = std::time::Instant::now();
        }
    }

    pub fn seek_relative(&mut self, delta: f64) {
        let st = self.state();
        let target = (st.time_pos + delta).max(0.0);
        let target = if st.duration > 0.0 && !self.live { target.min(st.duration) } else { target };
        self.seek(target, true);
    }

    pub fn frame_step(&self, forward: bool) {
        if let Some(m) = &self.mpv {
            let _ = m.command(&[if forward { "frame-step" } else { "frame-back-step" }]);
        }
    }

    pub fn set_mode(&mut self, mode: MixMode) {
        self.mode = mode;
        if let Some(m) = &self.mpv {
            m.set_exprs(&self.exprs(), None);
            let _ = m.set("af", &cv_core::mixer::mpv_af(&self.exprs()));
        }
    }

    // ------------------------------------------------------------ UI

    /// Draw the video. Click toggles pause, double-click toggles fullscreen.
    pub fn video(&mut self, ui: &mut Ui, size: Vec2) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        ui.painter().rect_filled(rect, CornerRadius::same(if self.fullscreen { 0 } else { 12 }), Color32::BLACK);
        match (&self.mpv, &self.path) {
            (None, _) => {
                let msg = self.init_error.clone().unwrap_or_else(|| "Video engine unavailable".into());
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, format!("Player unavailable: {msg}"), FontId::proportional(14.0), theme::TEXT_DIM);
            }
            (Some(_), None) => {
                ui.painter().text(rect.center() - egui::vec2(0.0, 14.0), Align2::CENTER_CENTER, icon::PLAY, FontId::new(34.0, theme::icons_family()), theme::TEXT_FAINT);
                ui.painter().text(rect.center() + egui::vec2(0.0, 22.0), Align2::CENTER_CENTER, "Nothing playing", FontId::proportional(14.0), theme::TEXT_FAINT);
            }
            (Some(m), Some(_)) => {
                let st = m.state();
                if let Some(err) = &st.last_error {
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, format!("Can't play this file: {err}"), FontId::proportional(14.0), theme::DANGER);
                } else {
                    // Inset by 1px so rounded corners stay clean.
                    let inner = rect.shrink(if self.fullscreen { 0.0 } else { 1.0 });
                    ui.painter().add(m.paint_callback(inner));
                    if !st.loaded {
                        ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Loading…", FontId::proportional(14.0), theme::TEXT_DIM);
                    } else if st.paused && !self.scrubbing {
                        // Subtle play glyph when paused.
                        let c = rect.center();
                        ui.painter().circle_filled(c, 30.0, Color32::from_black_alpha(120));
                        ui.painter().text(c + egui::vec2(2.0, 0.0), Align2::CENTER_CENTER, icon::PLAY, FontId::new(24.0, theme::icons_family()), Color32::WHITE);
                    }
                }
            }
        }
        if resp.clicked() {
            self.toggle_pause();
        }
        if resp.double_clicked() {
            self.toggle_pause(); // undo the single-click toggle
            self.fullscreen = !self.fullscreen;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        resp
    }

    /// Keyboard shortcuts (only when no text field has focus).
    pub fn shortcuts(&mut self, ui: &Ui, cfg: &PlayerConfig) -> ShortcutHits {
        let mut hits = ShortcutHits::default();
        if ui.ctx().egui_wants_keyboard_input() || self.path.is_none() {
            return hits;
        }
        let (small, large) = (cfg.seek_small_secs as f64, cfg.seek_large_secs as f64);
        ui.input(|i| {
            let shift = i.modifiers.shift;
            if i.key_pressed(Key::Space) || i.key_pressed(Key::K) {
                hits.toggle = true;
            }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::J) {
                hits.seek = Some(-(if shift { large } else { small }));
            }
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::L) {
                hits.seek = Some(if shift { large } else { small });
            }
            if i.key_pressed(Key::Comma) {
                hits.frame = Some(false);
            }
            if i.key_pressed(Key::Period) {
                hits.frame = Some(true);
            }
            if i.key_pressed(Key::I) {
                hits.mark_in = true;
            }
            if i.key_pressed(Key::O) {
                hits.mark_out = true;
            }
            if i.key_pressed(Key::M) {
                hits.mute = true;
            }
            if i.key_pressed(Key::F) {
                hits.fullscreen = true;
            }
            if i.key_pressed(Key::Escape) {
                hits.escape = true;
            }
            if i.key_pressed(Key::ArrowUp) {
                hits.volume = Some(0.05);
            }
            if i.key_pressed(Key::ArrowDown) {
                hits.volume = Some(-0.05);
            }
        });
        if hits.toggle {
            self.toggle_pause();
        }
        if let Some(d) = hits.seek {
            self.seek_relative(d);
        }
        if let Some(f) = hits.frame {
            self.frame_step(f);
        }
        if hits.mute {
            self.muted = !self.muted;
            self.apply_volume();
        }
        if let Some(v) = hits.volume {
            self.volume = (self.volume + v).clamp(0.0, 1.5);
            self.apply_volume();
        }
        if hits.fullscreen || (hits.escape && self.fullscreen) {
            self.fullscreen = !self.fullscreen;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        hits
    }

    /// Play/pause, skip buttons, time, speed, volume, mixer toggle.
    pub fn transport(&mut self, ui: &mut Ui, time_label: &str, cfg: &PlayerConfig) {
        let st = self.state();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if theme::icon_button(ui, icon::REWIND, 16.0, &format!("Back {}s (Shift+←)", cfg.seek_large_secs)).clicked() {
                self.seek_relative(-(cfg.seek_large_secs as f64));
            }
            if theme::icon_button(ui, icon::SKIP_BACK, 16.0, &format!("Back {}s (←)", cfg.seek_small_secs)).clicked() {
                self.seek_relative(-(cfg.seek_small_secs as f64));
            }
            let play_glyph = if st.paused || !st.loaded { icon::PLAY } else { icon::PAUSE };
            if theme::icon_button(ui, play_glyph, 20.0, "Play/pause (Space)").clicked() {
                self.toggle_pause();
            }
            if theme::icon_button(ui, icon::SKIP_FWD, 16.0, &format!("Forward {}s (→)", cfg.seek_small_secs)).clicked() {
                self.seek_relative(cfg.seek_small_secs as f64);
            }
            if theme::icon_button(ui, icon::FAST_FWD, 16.0, &format!("Forward {}s (Shift+→)", cfg.seek_large_secs)).clicked() {
                self.seek_relative(cfg.seek_large_secs as f64);
            }
            ui.add_space(8.0);
            ui.label(RichText::new(time_label).monospace().color(theme::TEXT_DIM));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let fs_glyph = if self.fullscreen { icon::EXIT_FULLSCREEN } else { icon::FULLSCREEN };
                if theme::icon_button(ui, fs_glyph, 15.0, "Fullscreen (F)").clicked() {
                    self.fullscreen = !self.fullscreen;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                let mixer_tip = if self.show_mixer { "Hide audio tracks".to_string() } else { format!("Show audio tracks ({})", self.track_count.max(1)) };
                let mresp = theme::icon_button(ui, icon::MIXER, 15.0, &mixer_tip);
                if self.show_mixer {
                    ui.painter().rect_stroke(mresp.rect, CornerRadius::same(20), Stroke::new(1.0, theme::TEXT_DIM), egui::StrokeKind::Inside);
                }
                if mresp.clicked() {
                    self.show_mixer = !self.show_mixer;
                }
                // Speed
                let speed = st.speed.max(0.01);
                egui::ComboBox::from_id_salt("speed")
                    .width(64.0)
                    .selected_text(format!("{}x", trim_float(speed)))
                    .show_ui(ui, |ui| {
                        for s in self.speeds {
                            if ui.selectable_label((speed - s).abs() < 0.01, format!("{}x", trim_float(s))).clicked() {
                                if let Some(m) = &self.mpv {
                                    let _ = m.set("speed", &s.to_string());
                                }
                            }
                        }
                    });
                // Volume
                let mut vol = self.volume;
                let r = ui.add(egui::Slider::new(&mut vol, 0.0..=1.5).show_value(false).trailing_fill(true));
                if r.changed() {
                    self.volume = vol;
                    self.muted = false;
                    self.apply_volume();
                }
                let vglyph = if self.muted || self.volume == 0.0 { icon::MUTE } else { icon::VOLUME };
                if theme::icon_button(ui, vglyph, 15.0, "Mute (M)").clicked() {
                    self.muted = !self.muted;
                    self.apply_volume();
                }
            });
        });
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ShortcutHits {
    pub toggle: bool,
    pub seek: Option<f64>,
    pub frame: Option<bool>,
    pub mark_in: bool,
    pub mark_out: bool,
    pub mute: bool,
    pub fullscreen: bool,
    pub escape: bool,
    pub volume: Option<f32>,
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Seek bar for a single file with optional in/out (trim) markers.
/// Returns Some(seconds) when the user seeks.
pub fn seek_bar(ui: &mut Ui, player: &mut Player, in_pt: &mut Option<f64>, out_pt: &mut Option<f64>, accent: Color32) -> Option<f64> {
    let st = player.state();
    let dur = st.duration.max(0.001);
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width() - 16.0, 6.0));
    let x_of = |t: f64| track.left() + (t / dur).clamp(0.0, 1.0) as f32 * track.width();
    let t_of = |x: f32| (((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64) * dur;
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(3), theme::SURFACE3);
    // Selection
    if let (Some(a), Some(b)) = (*in_pt, *out_pt) {
        let r = Rect::from_x_y_ranges(x_of(a)..=x_of(b), track.top() - 4.0..=track.bottom() + 4.0);
        p.rect_filled(r, CornerRadius::same(3), accent.gamma_multiply(0.35));
    }
    let played = Rect::from_x_y_ranges(track.left()..=x_of(st.time_pos), track.y_range());
    p.rect_filled(played, CornerRadius::same(3), theme::TEXT_DIM);
    for (pt, label) in [(*in_pt, "IN"), (*out_pt, "OUT")] {
        if let Some(t) = pt {
            let x = x_of(t);
            p.line_segment([egui::pos2(x, rect.top() + 2.0), egui::pos2(x, rect.bottom() - 2.0)], Stroke::new(2.0, accent));
            p.text(egui::pos2(x, rect.top()), Align2::CENTER_BOTTOM, label, FontId::proportional(9.0), accent);
        }
    }
    let hx = x_of(st.time_pos);
    p.circle_filled(egui::pos2(hx, track.center().y), if resp.hovered() || resp.dragged() { 7.0 } else { 5.5 }, Color32::WHITE);
    if let Some(pos) = resp.hover_pos() {
        let t = t_of(pos.x);
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::unique("seek_tip"), egui::PopupAnchor::Pointer)
            .show(|ui| ui.label(format_duration(t)));
    }
    let mut seek = None;
    if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            player.scrubbing = true;
            let t = t_of(pos.x);
            if player.last_seek.elapsed().as_millis() > 60 {
                player.seek(t, false);
            }
            seek = Some(t);
        }
    }
    if resp.drag_stopped() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = t_of(pos.x);
            player.seek(t, true);
            seek = Some(t);
        }
        player.scrubbing = false;
    }
    let _ = (seek, 0);
    seek
}
