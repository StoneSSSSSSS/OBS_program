//! Top-level app: navigation, shared resources, toasts.

use crate::player::Player;
use crate::service::{Service, UiEvent};
use crate::theme::{self, icon};
use crate::thumbs::Thumbs;
use crate::views::{clips::ClipsView, home::HomeView, recordings::RecordingsView, settings::SettingsView};
use cv_core::config::Config;
use cv_core::db::Db;
use cv_core::ipc::Request;
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Recordings,
    Clips,
    Settings,
}

pub struct Toast {
    pub level: String,
    pub text: String,
    pub at: Instant,
}

/// Things views share.
pub struct Ctx {
    pub cfg: Config,
    pub service: Service,
    pub thumbs: Thumbs,
    pub player: Player,
    pub waves: crate::lanes::Waves,
    pub db: Option<Db>,
    pub accent: Color32,
    pub toasts: Vec<Toast>,
    /// Requested navigation (views set this).
    pub goto: Option<Page>,
    pub open_recording_settings: bool,
}

impl Ctx {
    pub fn report_mark(&mut self, result: Result<(), &'static str>) {
        if let Err(message) = result {
            self.toast("info", message);
        }
    }
    pub fn toast(&mut self, level: &str, text: impl Into<String>) {
        self.toasts.push(Toast { level: level.into(), text: text.into(), at: Instant::now() });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    /// Fire a request; show any error as a toast.
    pub fn request(&mut self, req: Request) -> Option<serde_json::Value> {
        match self.service.call(req) {
            Ok(v) => Some(v),
            Err(e) => {
                self.toast("error", e);
                None
            }
        }
    }

    pub fn db(&mut self) -> Option<&Db> {
        if self.db.is_none() {
            self.db = Db::open_default().ok();
        }
        self.db.as_ref()
    }

    pub fn reload_config(&mut self) {
        self.cfg = Config::load_or_default();
        self.accent = theme::accent(self.cfg.general.accent);
    }
}

pub struct App {
    pub c: Ctx,
    page: Page,
    home: HomeView,
    clips: ClipsView,
    recordings: RecordingsView,
    settings: SettingsView,
    titlebar_done: u32,
    selftest: Option<SelfTest>,
}

/// `CLIPVAULT_SELFTEST=<dir>`: visit each page, save a screenshot of the app
/// window to <dir>, then exit. Used to check the UI without screen capture.
struct SelfTest {
    dir: std::path::PathBuf,
    step: usize,
    at: Instant,
    entered: bool,
    shot: bool,
}

const SELFTEST_STEPS: [&str; 8] = ["home", "live", "clips", "clip_detail", "settings_general", "settings_recording", "settings_audio", "settings_games"];

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> App {
        let ctx = &cc.egui_ctx;
        theme::install_fonts(ctx);
        let accent = theme::accent(cfg.general.accent);
        theme::apply(ctx, accent, cfg.general.ui_scale);
        let player = Player::new(ctx.clone(), &cfg);
        crate::views::clips::set_repaint_ctx(ctx);
        let page = match cfg.general.start_page.as_str() {
            "recordings" => Page::Recordings,
            "clips" => Page::Clips,
            _ => Page::Home,
        };
        let c = Ctx {
            service: Service::start(ctx.clone()),
            thumbs: Thumbs::new(ctx.clone()),
            player,
            waves: Default::default(),
            db: Db::open_default().ok(),
            accent,
            toasts: Vec::new(),
            goto: None,
            open_recording_settings: false,
            cfg,
        };
        App {
            c,
            page,
            home: HomeView::default(),
            clips: ClipsView::default(),
            recordings: RecordingsView::default(),
            settings: SettingsView::default(),
            titlebar_done: 0,
            selftest: std::env::var_os("CLIPVAULT_SELFTEST").map(|d| SelfTest { dir: d.into(), step: 0, at: Instant::now(), entered: false, shot: false }),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.c.thumbs.poll(ctx);
        self.c.player.set_preferences(&self.c.cfg);

        self.c.player.after_load_check();
        while let Ok(ev) = self.c.service.events.try_recv() {
            match ev {
                UiEvent::LibraryChanged => {
                    self.clips.mark_dirty();
                    self.home.mark_dirty();
                }
                UiEvent::ClipAdded(id) => {
                    self.clips.mark_dirty();
                    self.clips.highlight = Some(id);
                    self.home.mark_dirty();
                }
                UiEvent::RecordingsChanged => self.recordings.mark_dirty(),
                UiEvent::Toast { level, message } => self.c.toast(&level, message),
                UiEvent::JobDone(job) => {
                    if job.state == "done" {
                        self.clips.mark_dirty();
                        if let Some(id) = job.clip_id {
                            self.clips.highlight = Some(id);
                        }
                    }
                }
            }
        }
        // Keep animating while playing (time display/seek bar).
        let st = self.c.player.state();
        if self.c.player.path.is_some() && !st.paused {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(1000));
        }
    }

    fn navigate(&mut self, page: Page) {
        if page == self.page {
            return;
        }
        if self.page == Page::Settings {
            self.settings.leave(&mut self.c);
        }
        if self.page == Page::Recordings { self.recordings.leave(&mut self.c); }
        if self.page == Page::Clips { self.clips.save_position(&mut self.c); }
        // Leaving a page with a player: pause playback.
        if matches!(self.page, Page::Recordings | Page::Clips) {
            self.c.player.set_paused(true);
        }
        self.page = page;
    }

    fn sidebar(&mut self, ui: &mut Ui, icon_only: bool) {
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space(if icon_only { 14.0 } else { 16.0 });
            let (r, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
            ui.painter().rect(r, 6.0, theme::SURFACE2, Stroke::new(1.0, theme::BORDER_STRONG), egui::StrokeKind::Inside);
            ui.painter().circle_stroke(r.center(), 8.5, Stroke::new(1.75, theme::TEXT));
            ui.painter().circle_filled(r.center(), 3.3, self.c.accent);
            if !icon_only {
                ui.label(RichText::new("ClipVault").font(FontId::new(20.0, theme::bold())).color(theme::TEXT));
            }
        });
        ui.add_space(15.0);
        ui.separator();
        ui.add_space(10.0);
        let items = [
            (Page::Home, icon::HOME, "Home"),
            (Page::Recordings, icon::HISTORY, "24/7 Recordings"),
            (Page::Clips, icon::LIBRARY, "Clips"),
            (Page::Settings, icon::SETTINGS, "Settings"),
        ];
        for (page, glyph, label) in items {
            let selected = self.page == page;
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
            resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, label));
            let rect = rect.shrink2(Vec2::new(10.0, 2.0));
            let bg = if selected {
                self.c.accent
            } else if resp.hovered() {
                theme::SURFACE3
            } else {
                Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, 5.0, bg);
            if resp.has_focus() {
                ui.painter().rect_stroke(rect.expand(2.0), 7.0, Stroke::new(1.5, theme::TEXT), egui::StrokeKind::Outside);
            }
            let fg = if selected { theme::on_fill(bg) } else { theme::TEXT_DIM };
            let glyph_pos = if icon_only { rect.center() } else { rect.left_center() + egui::vec2(20.0, 0.0) };
            ui.painter().text(glyph_pos, Align2::CENTER_CENTER, glyph, FontId::new(15.0, theme::icons_family()), fg);
            if !icon_only {
                ui.painter().text(
                    rect.left_center() + egui::vec2(40.0, 0.0),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::new(14.0, if selected { theme::semibold() } else { egui::FontFamily::Proportional }),
                    fg,
                );
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked() {
                self.navigate(page);
            }
        }

        // Bottom: status + save clip.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            let status = self.c.service.status();
            let connected = self.c.service.connected();
            let enabled = connected && status.as_ref().is_some_and(|s| s.replay.active);
            if icon_only {
                if ui.add_enabled_ui(enabled, |ui| theme::icon_button(ui, icon::RECORD, 18.0, "Save replay"))
                    .inner.on_disabled_hover_text("Available while the replay buffer is recording").clicked()
                {
                    self.c.service.fire(Request::SaveClip { short: false });
                }
                return;
            }
            let w = ui.available_width() - 28.0;
            ui.allocate_ui(Vec2::new(w, 40.0), |ui| {
                ui.set_width(w);
                ui.add_enabled_ui(enabled, |ui| {
                    if theme::accent_button(ui, Some(icon::RECORD), "Save replay", self.c.accent)
                        .on_disabled_hover_text("Available while the replay buffer is recording")
                        .on_hover_text(format!("Saves the last {} ({})", cv_core::util::format_duration(self.c.cfg.clips.clip_length_secs as f64), self.c.cfg.hotkeys.save_clip))
                        .clicked()
                    {
                        self.c.service.fire(Request::SaveClip { short: false });
                    }
                });
            });
            ui.add_space(10.0);
            theme::card().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(w - 24.0);
                status_block(ui, connected, status.as_ref());
            });
        });
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        self.c.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(5));
        if self.c.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for t in self.c.toasts.iter().rev() {
                    let (glyph, color) = match t.level.as_str() {
                        "error" => (icon::CLOSE, theme::DANGER),
                        "warning" => (icon::WARNING, theme::WARNING),
                        "success" => (icon::CHECK, theme::SUCCESS),
                        _ => (icon::INFO, self.c.accent),
                    };
                    egui::Frame::NONE
                        .fill(theme::SURFACE2)
                        .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(14, 10))
                        .shadow(egui::Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(110) })
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                ui.label(theme::icon_text(glyph, 15.0).color(color));
                                ui.label(RichText::new(&t.text).color(theme::TEXT));
                            });
                        });
                    ui.add_space(8.0);
                }
            });
    }
}

impl App {
    fn selftest_step(&mut self, ctx: &egui::Context) {
        let Some(t) = &mut self.selftest else { return };
        ctx.request_repaint_after(Duration::from_millis(200));
        let name = SELFTEST_STEPS.get(t.step).copied();
        if !t.entered {
            // Enter the step (navigate), then wait for it to settle.
            t.entered = true;
            t.shot = false;
            t.at = Instant::now();
            match name {
                Some("home") => self.page = Page::Home,
                Some("live") => {
                    self.navigate(Page::Recordings);
                    crate::views::recordings::request_go_live();
                }
                Some("clips") => self.navigate(Page::Clips),
                Some("clip_detail") => {
                    let clips = self.c.db().and_then(|d| d.list_clips(&Default::default()).ok()).unwrap_or_default();
                    if let Some(c) = clips.iter().find(|c| c.audio_tracks > 1).or(clips.first()) {
                        crate::views::clips::request_open(c.id);
                    }
                }
                Some(s) if s.starts_with("settings_") => {
                    self.navigate(Page::Settings);
                    self.settings.select_tab(&s["settings_".len()..]);
                }
                Some(_) => {}
                None => {
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_secs(2));
                        std::process::exit(0);
                    });
                    self.selftest = None;
                }
            }
            return;
        }
        let wait = match name {
            Some("home") => 5,
            Some("live") | Some("clip_detail") => 6,
            _ => 3,
        };
        if !t.shot && t.at.elapsed() >= Duration::from_secs(wait) {
            t.shot = true;
            t.at = Instant::now();
            let path = t.dir.join(format!("{:02}_{}.png", t.step + 1, name.unwrap_or("x")));
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img: std::sync::Arc<egui::ColorImage>| {
                let [w, h] = img.size;
                let mut buf = Vec::with_capacity(w * h * 4);
                for p in &img.pixels {
                    buf.extend_from_slice(&p.to_array());
                }
                if let Some(im) = image::RgbaImage::from_raw(w as u32, h as u32, buf) {
                    let _ = im.save(&path);
                }
            })));
        } else if t.shot && t.at.elapsed() >= Duration::from_millis(800) {
            t.step += 1;
            t.entered = false;
        }
    }
}

pub fn status_block(ui: &mut Ui, connected: bool, status: Option<&cv_core::ipc::Status>) {
    let row = |ui: &mut Ui, color: Color32, text: &str| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, color);
            ui.label(RichText::new(text).size(12.5).color(theme::TEXT_DIM));
        });
    };
    let Some(s) = status.filter(|_| connected) else {
        row(ui, theme::WARNING, "Starting service…");
        return;
    };
    if !s.engine.running { row(ui, theme::WARNING, "Recorder idle"); } else if s.recording.active {
        let secs = (cv_core::util::now_ms() - s.recording.started_ms).max(0) / 1000;
        row(ui, theme::LIVE, &format!("Recording · {}", cv_core::util::format_span_short(secs)));
    } else if s.recording.paused_by_user {
        row(ui, theme::TEXT_FAINT, "24/7 paused");
    } else {
        row(ui, theme::WARNING, "Not recording");
    }
    let replay = if s.replay.active { theme::SUCCESS } else { theme::DANGER };
    row(ui, replay, if s.replay.active { "Replay buffer on" } else { "Replay buffer off" });
    let (mc, mt) = match s.mic.state.as_str() {
        "ok" => (theme::SUCCESS, "Mic OK"),
        "fallback" => (theme::WARNING, "Mic fallback"),
        "missing" => (theme::DANGER, "No mic"),
        "disabled" => (theme::TEXT_FAINT, "Mic guard off"),
        _ => (theme::TEXT_FAINT, "Mic…"),
    };
    row(ui, mc, mt);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.titlebar_done < 3 {
            self.titlebar_done += 1;
            crate::dark_title_bar();
        }
        self.poll(&ctx);
        self.selftest_step(&ctx);
        if let Some(p) = self.c.goto.take() {
            self.navigate(p);
        }
        if std::mem::take(&mut self.c.open_recording_settings) {
            self.settings.select_tab("recording");
            self.navigate(Page::Settings);
        }

        let fullscreen = self.c.player.fullscreen && matches!(self.page, Page::Recordings | Page::Clips);
        let compact = ui.available_width() < 1120.0;
        let icon_only = ui.available_width() < 840.0;
        if !fullscreen {
            egui::Panel::left("nav")
                .exact_size(if icon_only { 62.0 } else if compact { 174.0 } else { 196.0 })
                .resizable(false)
                .frame(egui::Frame::NONE.fill(theme::SIDEBAR).stroke(Stroke::new(1.0, theme::BORDER)))
                .show(ui, |ui| self.sidebar(ui, icon_only));
        }
        let margin = if fullscreen { Margin::ZERO } else if compact { Margin::symmetric(18, 14) } else { Margin::symmetric(24, 20) };
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(margin))
            .show(ui, |ui| match self.page {
                Page::Home => self.home.ui(ui, &mut self.c),
                Page::Recordings => self.recordings.ui(ui, &mut self.c),
                Page::Clips => self.clips.ui(ui, &mut self.c),
                Page::Settings => self.settings.ui(ui, &mut self.c),
            });
        self.toasts(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&glow::Context>) {
        if self.page == Page::Clips { self.clips.save_position(&mut self.c); }
        self.recordings.flush_edits();
        self.c.player.close();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }
}
