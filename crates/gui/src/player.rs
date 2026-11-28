//! Playback surface shared by the recording timeline and clip editor.
pub(crate) mod shortcuts;

use crate::native_player::{NativePlayer, PreviewMix};
use crate::theme::{self, icon};
use cv_core::config::{Config, PlayerConfig};
pub use cv_core::mixer::MixMode;
use cv_core::mixer::{self, LaneGain, MixSettings, TrackSource};
use cv_core::model::AudioEdit;
use cv_core::util::format_duration;
use cv_media::probe::{self, MediaInfo, TrackRole};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
pub struct PlayState {
    pub time_pos: f64,
    pub duration: f64,
    pub speed: f64,
    pub paused: bool,
    pub eof: bool,
    pub first_frame: bool,
    pub last_frame: bool,
    pub next_frame_pos: Option<f64>,
    pub loaded: bool,
}

pub struct Player {
    native: NativePlayer,
    info: Option<Arc<MediaInfo>>,
    texture: Option<egui::TextureHandle>,
    serial: Option<u64>,
    pub path: Option<String>,
    pub live: bool,
    pub track_count: usize,
    pub track_names: Vec<String>,
    pub lane_ids: Vec<String>,
    children: BTreeMap<String, BTreeMap<String, String>>,
    expanded: BTreeSet<String>,
    overrides: BTreeMap<String, String>,
    pub mix: MixSettings,
    pub edits: Vec<AudioEdit>,
    fade: f64,
    looping: bool,
    speed: f64,
    speeds: [f64; 7],
    pub volume: f32,
    pub muted: bool,
    pub show_mixer: bool,
    pub fullscreen: bool,
    pub scrubbing: bool,
    last_seek: Instant,
}

impl Player {
    pub fn new(ctx: egui::Context, cfg: &Config) -> Self {
        Self {
            native: NativePlayer::new(ctx),
            info: None,
            texture: None,
            serial: None,
            path: None,
            live: false,
            track_count: 0,
            track_names: Vec::new(),
            lane_ids: Vec::new(),
            children: BTreeMap::new(),
            expanded: BTreeSet::new(),
            overrides: cfg.audio.app_categories.clone(),
            mix: MixSettings::default(),
            edits: Vec::new(),
            fade: cfg.player.fade_s(),
            looping: false,
            speed: 1.0,
            speeds: [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0],
            volume: cfg.player.volume as f32 / 100.0,
            muted: false,
            show_mixer: true,
            fullscreen: false,
            scrubbing: false,
            last_seek: Instant::now(),
        }
    }
    pub fn state(&self) -> PlayState {
        let s = self.native.state();
        PlayState {
            time_pos: s.position,
            duration: s.duration,
            speed: self.speed,
            paused: s.paused,
            eof: s.ended,
            first_frame: s.first_frame,
            last_frame: s.last_frame,
            next_frame_pos: s.next_frame_pos,
            loaded: s.loaded,
        }
    }
    pub fn open(
        &mut self,
        path: &str,
        start: f64,
        _tracks: usize,
        _names: &[String],
        saved: Option<&MixSettings>,
        live: bool,
        autoplay: bool,
        cfg: &PlayerConfig,
    ) {
        self.fade = cfg.fade_s();
        self.mix = saved.cloned().unwrap_or_else(|| MixSettings {
            mode: if cfg.default_mix_mode == "full" { MixMode::Full } else { MixMode::Separate },
            ..Default::default()
        });
        self.path = Some(path.into());
        self.live = live;
        self.edits.clear();
        self.looping = false;
        self.info = None;
        self.serial = None;
        self.texture = None;
        self.track_count = 0;
        self.rebuild_lanes();
        self.speed = f64::from(cfg.default_speed);
        self.native.load(path.into(), start, live, !autoplay);
        self.native.set_speed(self.speed);
        self.apply_mix();
        self.apply_volume();
    }
    pub fn after_load_check(&mut self) {
        let state = self.native.state();
        if let Some(info) = state.info {
            if self.info.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &info)) {
                self.track_count = info.audio.len();
                self.info = Some(info);
                self.rebuild_lanes();
            }
        }
        if self.looping && state.ended && self.path.is_some() && self.last_seek.elapsed().as_millis() > 150 {
            self.seek(0.0, true);
            self.set_paused(false);
        }
    }
    fn rebuild_lanes(&mut self) {
        self.lane_ids.clear();
        self.track_names.clear();
        self.children.clear();
        let Some(info) = self.info.as_ref() else { return };
        let sources = probe::track_sources(info);
        let mut add = |key: &str, name: &str, category: &str| {
            let cat = self.overrides.get(&key.to_lowercase()).map(String::as_str).unwrap_or(category);
            self.children
                .entry(mixer::category_lane(cat))
                .or_default()
                .insert(mixer::app_lane(key), name.to_owned());
        };
        for src in &sources {
            if let TrackSource::App { key, name, category } = src {
                add(key, name, category);
            }
        }
        for meta in &info.slot_history {
            for slot in &meta.slots {
                let a = &slot.app;
                add(&a.key, &a.name, &a.category);
            }
        }
        if sources.contains(&TrackSource::Full) {
            self.lane_ids.push("full".into());
            self.track_names.push("Full mix".into());
        }
        for (category, name) in mixer::CATEGORIES {
            let id = mixer::category_lane(category);
            self.lane_ids.push(id.clone());
            self.track_names.push(name.into());
            if self.expanded.contains(&id) {
                if let Some(apps) = self.children.get(&id) {
                    for (key, name) in apps {
                        self.lane_ids.push(key.clone());
                        self.track_names.push(name.clone());
                    }
                }
            }
        }
        for (i, src) in sources.iter().enumerate() {
            if matches!(src, TrackSource::Other(_)) && matches!(info.audio[i].role, TrackRole::Other) {
                self.lane_ids.push(format!("track:{i}"));
                self.track_names.push(info.audio[i].name.clone());
            }
        }
    }
    pub fn expandable(&self, id: &str) -> bool {
        self.children.contains_key(id)
    }
    pub fn media_info(&self) -> Option<&MediaInfo> {
        self.info.as_deref()
    }
    pub fn expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }
    pub fn set_expanded(&mut self, id: &str, yes: bool) {
        if yes {
            self.expanded.insert(id.into());
        } else {
            self.expanded.remove(id);
        }
        self.rebuild_lanes();
    }
    pub fn set_preferences(&mut self, cfg: &Config) {
        let fade = cfg.player.fade_s();
        if self.fade != fade || self.overrides != cfg.audio.app_categories {
            self.fade = fade;
            self.overrides = cfg.audio.app_categories.clone();
            self.rebuild_lanes();
            self.apply_mix();
        }
    }
    fn apply_mix(&self) {
        self.native.set_mix(PreviewMix {
            mix: self.mix.clone(),
            edits: self.edits.clone(),
            edit_offset: 0.0,
            fade: self.fade,
            overrides: self.overrides.clone(),
        });
    }
    pub fn set_edits(&mut self, edits: Vec<AudioEdit>) {
        if edits != self.edits {
            self.edits = edits;
            self.apply_mix();
        }
    }
    pub fn restore_audio(&mut self, mix: MixSettings, edits: Vec<AudioEdit>) {
        self.mix = mix;
        self.edits = edits;
        self.apply_mix();
    }
    pub fn set_lane_gain(&mut self, id: &str, gain: f32) {
        self.mix.mode = if id == "full" { MixMode::Full } else { MixMode::Separate };
        self.mix.set(id, LaneGain { gain, muted: gain == 0.0 });
        self.apply_mix();
    }
    pub fn set_mode(&mut self, mode: MixMode) {
        self.mix.mode = mode;
        self.apply_mix();
    }
    pub fn set_lane_muted(&mut self, id: &str, muted: bool) {
        let mut lane = self.mix.lane(id);
        lane.muted = muted;
        if !muted && lane.gain <= 0.0 {
            lane.gain = 1.0;
        }
        self.mix.mode = if id == "full" { MixMode::Full } else { MixMode::Separate };
        self.mix.set(id, lane);
        self.apply_mix();
    }
    pub fn set_paused(&self, p: bool) {
        self.native.set_paused(p);
    }
    pub fn set_loop(&mut self, yes: bool) {
        self.looping = yes;
    }
    pub fn close(&mut self) {
        self.native.clear();
        self.path = None;
        self.info = None;
        self.texture = None;
        self.serial = None;
        self.looping = false;
        self.live = false;
        self.track_count = 0;
        self.rebuild_lanes();
    }
    pub fn apply_volume(&self) {
        self.native.set_volume(if self.muted { 0.0 } else { f64::from(self.volume) });
    }
    pub fn toggle_pause(&mut self) {
        let s = self.state();
        if s.eof {
            self.seek(0.0, true);
            self.set_paused(false);
        } else {
            self.set_paused(!s.paused);
        }
    }
    pub fn seek(&mut self, t: f64, exact: bool) {
        self.native.seek(t.max(0.0), exact);
        self.last_seek = Instant::now();
    }
    pub fn seek_relative(&mut self, delta: f64) {
        let s = self.state();
        self.seek((s.time_pos + delta).clamp(0.0, s.duration.max(0.0)), true);
    }
    pub fn frame_step(&mut self, forward: bool) {
        self.native.step(forward);
        self.last_seek = Instant::now();
    }

    pub fn seek_latest(&mut self) {
        self.native.seek_latest();
        self.last_seek = Instant::now();
    }
    pub fn video(&mut self, ui: &mut Ui, size: Vec2) -> egui::Response {
        let size = size.max(Vec2::ZERO).min(ui.available_size().max(Vec2::ZERO));
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        if !rect.is_positive() {
            return response;
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(5), Color32::BLACK);
        let state = self.native.state();
        if self.path.is_some() {
            if let Some(frame) = self.native.frame() {
                if self.serial != Some(frame.serial) {
                    let image = egui::ColorImage::from_rgba_premultiplied([frame.width, frame.height], &frame.rgba);
                    if let Some(tex) = &mut self.texture {
                        tex.set(image, egui::TextureOptions::LINEAR);
                    } else {
                        self.texture = Some(ui.ctx().load_texture("clipvault-video", image, egui::TextureOptions::LINEAR));
                    }
                    self.serial = Some(frame.serial);
                }
            }
        }
        if let Some(tex) = &self.texture {
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let dest = Rect::from_center_size(rect.center(), size * scale);
            crate::views::editor_layout::remember_interaction_rect(ui, "preview_picture", dest);
            painter.image(tex.id(), dest, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        let message = if self.path.is_none() {
            Some("Choose a recording or clip".to_owned())
        } else if let Some(err) = state.error {
            Some(format!("Playback: {err}"))
        } else if self.texture.is_none() {
            Some("Opening video…".into())
        } else {
            None
        };
        if let Some(msg) = message {
            let mut job = egui::text::LayoutJob::simple(
                msg, FontId::new(15.0, theme::semibold()), theme::TEXT, (rect.width() - 16.0).max(1.0),
            );
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = painter.layout_job(job);
            painter.galley(rect.center() - galley.size() * 0.5, galley, theme::TEXT);
        }
        if !self.fullscreen {
            let border = if response.has_focus() { theme::TEXT } else { theme::BORDER_STRONG };
            painter.rect_stroke(rect, 5.0, Stroke::new(1.0, border), egui::StrokeKind::Inside);
        }
        if response.double_clicked() {
            self.fullscreen = !self.fullscreen;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        } else if response.clicked() {
            self.toggle_pause();
        }
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
    /// Keyboard shortcuts (only when no text field has focus).
    pub fn shortcuts(&mut self, ui: &Ui, cfg: &PlayerConfig) -> ShortcutHits {
        use shortcuts::Action;

        let mut hits = ShortcutHits::default();
        let small = f64::from(cfg.seek_small_secs);
        let large = f64::from(cfg.seek_large_secs);
        for action in shortcuts::actions(ui, cfg) {
            match action {
                Action::JumpLatest => hits.jump_latest = true,
                Action::BackOrExit => hits.escape = true,
                _ if self.path.is_none() => continue,
                Action::Play => hits.toggle = true,
                Action::Back => hits.seek = Some(-small),
                Action::Forward => hits.seek = Some(small),
                Action::BackLong => hits.seek = Some(-large),
                Action::ForwardLong => hits.seek = Some(large),
                Action::PreviousFrame => hits.frame_step = Some(false),
                Action::NextFrame => hits.frame_step = Some(true),
                Action::MarkStart => hits.mark_in = true,
                Action::MarkEnd => hits.mark_out = true,
                Action::VolumeUp => hits.volume = Some(0.05),
                Action::VolumeDown => hits.volume = Some(-0.05),
                Action::Mute => hits.mute = true,
                Action::Fullscreen => hits.fullscreen = true,
            }
        }
        if hits.toggle {
            self.toggle_pause();
        }
        if let Some(d) = hits.seek {
            self.seek_relative(d);
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
    pub fn transport(
        &mut self, ui: &mut Ui, time_label: &str, cfg: &PlayerConfig,
    ) -> (egui::Response, Option<bool>) {
        use shortcuts::{hint, Action};

        let mut step = None;
        let st = self.state();
        let compact = ui.available_width() < crate::views::editor_layout::SINGLE_TRANSPORT_ROW_WIDTH;
        let play = ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if theme::icon_button(ui, icon::REWIND, 16.0, &hint(cfg, Action::BackLong, &format!("Back {}s", cfg.seek_large_secs))).clicked() {
                self.seek_relative(-(cfg.seek_large_secs as f64));
            }
            if theme::icon_button(ui, icon::SKIP_BACK, 16.0, &hint(cfg, Action::Back, &format!("Back {}s", cfg.seek_small_secs))).clicked() {
                self.seek_relative(-(cfg.seek_small_secs as f64));
            }
            let previous = ui.add_enabled(st.loaded, egui::Button::new("‹│")).on_hover_text(hint(cfg, Action::PreviousFrame, "Previous frame"));
            crate::views::editor_layout::remember_interaction_rect(ui, "previous_frame", previous.rect);
            if previous.clicked() {
                step = Some(false);
            }
            let play_glyph = if st.paused || !st.loaded { icon::PLAY } else { icon::PAUSE };
            let play = theme::icon_button(ui, play_glyph, 20.0, &hint(cfg, Action::Play, "Play / pause"));
            if play.clicked() {
                self.toggle_pause();
            }
            let next = ui.add_enabled(st.loaded, egui::Button::new("│›")).on_hover_text(hint(cfg, Action::NextFrame, "Next frame"));
            crate::views::editor_layout::remember_interaction_rect(ui, "next_frame", next.rect);
            if next.clicked() {
                step = Some(true);
            }
            if theme::icon_button(ui, icon::SKIP_FWD, 16.0, &hint(cfg, Action::Forward, &format!("Forward {}s", cfg.seek_small_secs))).clicked() {
                self.seek_relative(cfg.seek_small_secs as f64);
            }
            if theme::icon_button(ui, icon::FAST_FWD, 16.0, &hint(cfg, Action::ForwardLong, &format!("Forward {}s", cfg.seek_large_secs))).clicked() {
                self.seek_relative(cfg.seek_large_secs as f64);
            }
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(time_label).monospace().color(theme::TEXT_DIM)).truncate())
                .on_hover_text(time_label);

            if !compact {
                self.transport_options(ui, st.speed, cfg);
            }
            play
        }).inner;
        if compact {
            ui.horizontal_wrapped(|ui| self.transport_options(ui, st.speed, cfg));
        }
        (play, step)
    }

    fn transport_options(&mut self, ui: &mut Ui, speed: f64, cfg: &PlayerConfig) {
        use shortcuts::{hint, Action};
        ui.spacing_mut().item_spacing.x = 6.0;
        let fs_glyph = if self.fullscreen { icon::EXIT_FULLSCREEN } else { icon::FULLSCREEN };
        if theme::icon_button(ui, fs_glyph, 15.0, &hint(cfg, Action::Fullscreen, "Fullscreen")).clicked() {
            self.fullscreen = !self.fullscreen;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        let mixer_tip = if self.show_mixer {
            "Hide audio tracks".to_string()
        } else {
            format!("Show audio tracks ({})", self.track_count.max(1))
        };
        let mresp = theme::icon_button(ui, icon::MIXER, 15.0, &mixer_tip);
        if self.show_mixer {
            ui.painter()
                .rect_stroke(mresp.rect, CornerRadius::same(20), Stroke::new(1.0, theme::TEXT_DIM), egui::StrokeKind::Inside);
        }
        if mresp.clicked() {
            self.show_mixer = !self.show_mixer;
        }
        // Speed
        let speed = speed.max(0.01);
        egui::ComboBox::from_id_salt("speed")
            .width(64.0)
            .selected_text(format!("{}x", trim_float(speed)))
            .show_ui(ui, |ui| {
                for s in self.speeds {
                    if ui.selectable_label((speed - s).abs() < 0.01, format!("{}x", trim_float(s))).clicked() {
                        {
                            self.speed = s;
                            self.native.set_speed(s);
                        }
                    }
                }
            });
        // Volume
        let slider_width = ui.spacing().slider_width.min(104.0).min(ui.available_width().max(40.0));
        ui.spacing_mut().slider_width = slider_width;
        let mut vol = self.volume;
        let r = ui.add(egui::Slider::new(&mut vol, 0.0..=1.5).show_value(false).trailing_fill(true));
        if r.changed() {
            self.volume = vol;
            self.muted = false;
            self.apply_volume();
        }
        let vglyph = if self.muted || self.volume == 0.0 { icon::MUTE } else { icon::VOLUME };
        if theme::icon_button(ui, vglyph, 15.0, &hint(cfg, Action::Mute, "Mute")).clicked() {
            self.muted = !self.muted;
            self.apply_volume();
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ShortcutHits {
    pub toggle: bool,
    pub seek: Option<f64>,
    pub frame_step: Option<bool>,
    pub jump_latest: bool,
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
    let width = ui.available_width().max(1.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), Vec2::new((rect.width() - 16.0).max(1.0), 6.0));
    let x_of = |t: f64| track.left() + (t / dur).clamp(0.0, 1.0) as f32 * track.width();
    let t_of = |x: f32| (((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64) * dur;
    let p = ui.painter_at(rect);
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
            p.text(egui::pos2(x, rect.top()), Align2::CENTER_TOP, label, FontId::proportional(9.0), accent);
        }
    }
    let hx = x_of(st.time_pos);
    p.circle_filled(
        egui::pos2(hx, track.center().y),
        if resp.hovered() || resp.dragged() { 7.0 } else { 5.5 },
        Color32::WHITE,
    );
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
