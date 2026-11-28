//! Greyscale dark theme, fonts, icons and small custom widgets.

use cv_core::config::Accent;
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response, RichText, Sense,
    Shadow, Stroke, StrokeKind, Ui, Vec2,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_gray(19);
pub const SIDEBAR: Color32 = Color32::from_gray(12);
pub const SURFACE: Color32 = Color32::from_gray(26);
pub const SURFACE2: Color32 = Color32::from_gray(36);
pub const SURFACE3: Color32 = Color32::from_gray(48);
pub const BORDER: Color32 = Color32::from_gray(51);
pub const BORDER_STRONG: Color32 = Color32::from_gray(88);
pub const TEXT: Color32 = Color32::from_gray(246);
pub const TEXT_DIM: Color32 = Color32::from_gray(187);
pub const TEXT_FAINT: Color32 = Color32::from_gray(151);
pub const SUCCESS: Color32 = Color32::from_gray(209);
pub const WARNING: Color32 = Color32::from_gray(214);
pub const DANGER: Color32 = Color32::from_gray(236);
pub const LIVE: Color32 = Color32::from_gray(248);

pub fn accent(a: Accent) -> Color32 {
    match a {
        Accent::Mono => Color32::from_gray(226),
        Accent::Red => Color32::from_rgb(229, 72, 77),
        Accent::Blue => Color32::from_rgb(59, 130, 246),
        Accent::Green => Color32::from_rgb(34, 197, 94),
        Accent::Purple => Color32::from_rgb(168, 85, 247),
        Accent::Orange => Color32::from_rgb(249, 115, 22),
    }
}

/// Segoe MDL2 Assets glyphs (present on every Windows 10/11 install).
#[allow(dead_code)]
pub mod icon {
    pub const PLAY: &str = "\u{E768}";
    pub const PAUSE: &str = "\u{E769}";
    pub const HOME: &str = "\u{E80F}";
    pub const HISTORY: &str = "\u{E81C}";
    pub const LIBRARY: &str = "\u{E8F1}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const RECORD: &str = "\u{E7C8}";
    pub const SEARCH: &str = "\u{E721}";
    pub const STAR: &str = "\u{E734}";
    pub const STAR_FILL: &str = "\u{E735}";
    pub const TAG: &str = "\u{E8EC}";
    pub const GAME: &str = "\u{E7FC}";
    pub const DELETE: &str = "\u{E74D}";
    pub const EDIT: &str = "\u{E70F}";
    pub const FOLDER: &str = "\u{E8B7}";
    pub const SHARE: &str = "\u{E72D}";
    pub const COPY: &str = "\u{E8C8}";
    pub const CUT: &str = "\u{E8C6}";
    pub const ADD: &str = "\u{E710}";
    pub const CLOSE: &str = "\u{E711}";
    pub const CHECK: &str = "\u{E73E}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const INFO: &str = "\u{E946}";
    pub const MIC: &str = "\u{E720}";
    pub const VOLUME: &str = "\u{E767}";
    pub const MUTE: &str = "\u{E74F}";
    pub const BACK: &str = "\u{E72B}";
    pub const SKIP_BACK: &str = "\u{E892}";
    pub const SKIP_FWD: &str = "\u{E893}";
    pub const REWIND: &str = "\u{EB9E}";
    pub const FAST_FWD: &str = "\u{EB9D}";
    pub const CHEVRON_LEFT: &str = "\u{E76B}";
    pub const CHEVRON_RIGHT: &str = "\u{E76C}";
    pub const CHEVRON_DOWN: &str = "\u{E70D}";
    pub const FLAG: &str = "\u{E7C1}";
    pub const LOCK: &str = "\u{E72E}";
    pub const REFRESH: &str = "\u{E72C}";
    pub const MIXER: &str = "\u{E9E9}";
    pub const STORAGE: &str = "\u{EDA2}";
    pub const CLOCK: &str = "\u{E823}";
    pub const LIVE: &str = "\u{E93E}";
    pub const FULLSCREEN: &str = "\u{E740}";
    pub const EXIT_FULLSCREEN: &str = "\u{E73F}";
    pub const SPEED: &str = "\u{EC4A}";
    pub const KEYBOARD: &str = "\u{E765}";
    pub const DEVICE: &str = "\u{E7F8}";
    pub const BOLT: &str = "\u{E945}";
    pub const SCISSORS: &str = "\u{E8C6}";
    pub const MORE: &str = "\u{E712}";
}

pub fn icons_family() -> FontFamily {
    FontFamily::Name("icons".into())
}
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

fn load_system_font(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    std::fs::read(dir.join("Fonts").join(name)).ok()
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |key: &str, file: &str| -> bool {
        match load_system_font(file) {
            Some(bytes) => {
                fonts.font_data.insert(key.to_string(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            None => false,
        }
    };
    let regular = add("segoe", "segoeui.ttf");
    let semi = add("segoe-semibold", "seguisb.ttf");
    let bolded = add("segoe-bold", "segoeuib.ttf");
    let icons = add("mdl2", "segmdl2.ttf") || add("mdl2", "SegoeIcons.ttf");
    let symbols = add("segoe-symbol", "seguisym.ttf");
    let mono = add("consolas", "consola.ttf");

    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    if regular {
        prop.insert(0, "segoe".into());
    }
    if symbols {
        prop.push("segoe-symbol".into());
    }
    if icons {
        prop.push("mdl2".into());
    }
    let base: Vec<String> = fonts.families[&FontFamily::Proportional].clone();
    let mk = |primary: Option<&str>| {
        let mut v: Vec<String> = primary.map(|p| vec![p.to_string()]).unwrap_or_default();
        v.extend(base.iter().cloned());
        v
    };
    fonts.families.insert(semibold(), mk(if semi { Some("segoe-semibold") } else { None }));
    fonts.families.insert(bold(), mk(if bolded { Some("segoe-bold") } else if semi { Some("segoe-semibold") } else { None }));
    let mut icon_fam = Vec::new();
    if icons {
        icon_fam.push("mdl2".to_string());
    }
    icon_fam.extend(base.iter().cloned());
    fonts.families.insert(icons_family(), icon_fam);
    if mono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, accent_color: Color32, scale: f32) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_zoom_factor(scale);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        use egui::TextStyle::*;
        style.text_styles = [
            (Heading, FontId::new(26.0, bold())),
            (Name("title".into()), FontId::new(17.0, semibold())),
            (Body, FontId::new(14.0, FontFamily::Proportional)),
            (Button, FontId::new(14.0, semibold())),
            (Small, FontId::new(12.0, FontFamily::Proportional)),
            (Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 8.0);
        s.button_padding = Vec2::new(12.0, 6.0);
        s.interact_size = Vec2::new(36.0, 30.0);
        s.slider_width = 160.0;
        s.combo_width = 180.0;
        s.text_edit_width = 260.0;
        s.window_margin = Margin::same(16);
        s.menu_margin = Margin::same(6);
        s.scroll.bar_width = 8.0;
        s.scroll.floating = true;

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.extreme_bg_color = Color32::from_gray(12);
        v.faint_bg_color = SURFACE;
        v.code_bg_color = SURFACE2;
        v.window_stroke = Stroke::new(1.0, BORDER_STRONG);
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) };
        v.selection.bg_fill = accent_color.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = accent_color;
        v.text_cursor.stroke = Stroke::new(2.0, TEXT);
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;
        v.striped = false;
        let r = CornerRadius::same(5);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = SURFACE3;
        w.inactive.weak_bg_fill = SURFACE2;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = Color32::from_gray(63);
        w.hovered.weak_bg_fill = SURFACE3;
        w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = Color32::from_gray(73);
        w.active.weak_bg_fill = Color32::from_gray(54);
        w.active.bg_stroke = Stroke::new(1.0, TEXT);
        w.active.fg_stroke = Stroke::new(2.0, TEXT);
        w.active.corner_radius = r;
        w.open = w.active;
    });
}

// ------------------------------------------------------------ widgets

pub fn icon_text(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).font(FontId::new(size, icons_family()))
}

pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).font(FontId::new(17.0, semibold())).color(TEXT)
}

pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).font(FontId::new(28.0, bold())).color(TEXT)
}

pub fn label_dim(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(TEXT_DIM)
}

pub fn small(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(12.0).color(TEXT_FAINT)
}

pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(TEXT_FAINT).font(FontId::new(11.0, semibold())));
    ui.add_space(2.0);
}

pub fn card() -> egui::Frame {
    egui::Frame::NONE
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(16))
}

/// Solid light "primary" button (monochrome look).
pub fn primary_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    custom_button(ui, glyph, text, TEXT, BG, true)
}

/// Accent-coloured button (used for record-ish actions).
pub fn accent_button(ui: &mut Ui, glyph: Option<&str>, text: &str, color: Color32) -> Response {
    custom_button(ui, glyph, text, color, on_fill(color), true)
}

pub fn on_fill(fill: Color32) -> Color32 {
    let linear = |value: u8| {
        let value = f32::from(value) / 255.0;
        if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    };
    let luminance = 0.2126 * linear(fill.r()) + 0.7152 * linear(fill.g()) + 0.0722 * linear(fill.b());
    if luminance >= 0.179 { Color32::BLACK } else { Color32::WHITE }
}

pub fn ghost_button(ui: &mut Ui, glyph: Option<&str>, text: &str) -> Response {
    custom_button(ui, glyph, text, SURFACE2, TEXT, false)
}

fn custom_button(ui: &mut Ui, glyph: Option<&str>, text: &str, fill: Color32, fg: Color32, solid: bool) -> Response {
    let font = FontId::new(14.0, semibold());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let icon_w = if glyph.is_some() { 22.0 } else { 0.0 };
    let pad = Vec2::new(14.0, 8.0);
    let size = Vec2::new(galley.size().x + icon_w + pad.x * 2.0, (galley.size().y + pad.y * 2.0).max(34.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), text));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let pressed = resp.is_pointer_button_down_on();
        let mut bg = fill;
        if solid {
            if pressed {
                bg = bg.gamma_multiply(0.8);
            } else if hovered {
                bg = blend(bg, Color32::WHITE, 0.12);
            }
        } else if pressed {
            bg = SURFACE3;
        } else if hovered {
            bg = SURFACE3;
        }
        let stroke = if solid { Stroke::NONE } else { Stroke::new(1.0, if hovered { BORDER_STRONG } else { BORDER }) };
        let color = if solid { on_fill(bg) } else { fg };
        ui.painter().rect(rect, CornerRadius::same(5), bg, stroke, StrokeKind::Inside);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect.expand(2.0), 7.0, Stroke::new(1.5, TEXT), StrokeKind::Outside);
        }
        let mut x = rect.left() + pad.x;
        if let Some(g) = glyph {
            ui.painter().text(
                egui::pos2(x + 8.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                g,
                FontId::new(14.0, icons_family()),
                color,
            );
            x += icon_w;
        }
        let galley = if color == fg {
            galley
        } else {
            ui.painter().layout_no_wrap(text.to_owned(), FontId::new(14.0, semibold()), color)
        };
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn icon_button_size(glyph_size: f32) -> f32 {
    glyph_size + 16.0
}

/// Round icon-only button.
pub fn icon_button(ui: &mut Ui, glyph: &str, size: f32, tooltip: &str) -> Response {
    let d = icon_button_size(size);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(d), Sense::click());
    if !tooltip.is_empty() {
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), tooltip));
    }
    if ui.is_rect_visible(rect) {
        let bg = if resp.is_pointer_button_down_on() {
            SURFACE3
        } else if resp.hovered() {
            SURFACE2
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same((d / 2.0) as u8), bg);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, CornerRadius::same((d / 2.0) as u8), Stroke::new(1.5, TEXT), StrokeKind::Inside);
        }
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, FontId::new(size, icons_family()), TEXT);
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    }
}

/// iOS-style toggle switch.
pub fn toggle(ui: &mut Ui, on: &mut bool, accent_color: Color32) -> Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    let bg = blend(SURFACE3, accent_color, t);
    ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.0, on_fill(bg));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small rounded status pill with a coloured dot.
pub fn pill(ui: &mut Ui, color: Color32, text: &str) -> Response {
    let font = FontId::new(12.0, semibold());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, TEXT);
    let size = Vec2::new(galley.size().x + 28.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(12), color.gamma_multiply(0.16));
    ui.painter().circle_filled(egui::pos2(rect.left() + 12.0, rect.center().y), 4.0, color);
    ui.painter().galley(egui::pos2(rect.left() + 21.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
    resp
}

pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Distinct neutral values for activity regions on the timeline.
pub fn game_color(id: i64) -> Color32 {
    Color32::from_gray(130 + (id.wrapping_mul(2654435761) as u64 % 55) as u8)
}

/// A horizontal level meter (0..1 linear peak).
pub fn meter(ui: &mut Ui, peak: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(3), SURFACE3);
    let db = if peak > 0.0 { 20.0 * peak.log10() } else { -60.0 };
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let color = if db > -3.0 { DANGER } else if db > -12.0 { WARNING } else { SUCCESS };
    let mut r = rect;
    r.set_width(rect.width() * frac);
    ui.painter().rect_filled(r, CornerRadius::same(3), color);
}
