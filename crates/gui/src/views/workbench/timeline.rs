//! Timeline geometry and gestures calculated from the current frame's width.
use super::{clock, filename, frame_number, frame_tick, role_name, seconds, ticks, Generated, Selection, Workbench};
use crate::theme;
use anyhow::Context as _;
use cv_core::editor::{AssetKind, Id, Project, SoundRole, Source, Span, TICKS_PER_SECOND};
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui};

const TICK_HEIGHT: f32 = 28.0;
const LANE_HEIGHT: f32 = 35.0;
const ROLES: [SoundRole; 3] = [SoundRole::Music, SoundRole::Voice, SoundRole::Effect];

#[derive(Clone, Copy)]
pub(super) struct BinAsset(pub Id);

#[derive(Default)]
pub(super) struct Timeline {
    window: Option<Window>,
    gesture: Option<Gesture>,
}

#[derive(Clone, Copy)]
struct Window {
    start: u64,
    span: u64,
}

impl Window {
    fn visible(window: Option<Self>, duration: u64) -> Self {
        let mut window = window.unwrap_or(Self { start: 0, span: duration });
        window.span = window.span.clamp((TICKS_PER_SECOND / 10).min(duration), duration);
        window.start = window.start.min(duration - window.span);
        window
    }

    fn x(self, axis: Rect, tick: u64) -> f32 {
        let fraction = (tick as f64 - self.start as f64) / self.span as f64;
        axis.left() + axis.width() * fraction as f32
    }

    fn tick(self, axis: Rect, x: f32) -> u64 {
        self.start.saturating_add((self.span as f64 * f64::from(((x - axis.left()) / axis.width()).clamp(0.0, 1.0))).round() as u64)
    }
}

#[derive(Clone)]
struct Item {
    selection: Selection,
    at: u64,
    range: Span,
    limit: u64,
    label: String,
    rgb: Color32,
}

struct Lane {
    role: Option<SoundRole>,
    rows: Vec<Vec<Item>>,
}

#[derive(Clone, Copy)]
enum Drag {
    Move,
    TrimIn,
    TrimOut,
}

#[derive(Clone)]
struct Gesture {
    item: Item,
    kind: Drag,
    origin: f32,
    ticks_per_point: f64,
}

impl Workbench {
    pub(super) fn timeline(&mut self, ui: &mut Ui, accent: Color32) {
        egui::Frame::NONE.fill(theme::SURFACE).stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(7).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                let width = ui.available_width();
                ui.set_min_width(width);
                ui.horizontal_wrapped(|ui| {
                    ui.strong("Timeline");
                    if ui.add_enabled(self.history.can_undo(), egui::Button::new("Undo")).on_hover_text("Ctrl+Z").clicked() { self.undo(ui.ctx()); }
                    if ui.add_enabled(self.history.can_redo(), egui::Button::new("Redo")).on_hover_text("Ctrl+Shift+Z").clicked() { self.redo(ui.ctx()); }
                    let selected = self.selected.is_some();
                    if ui.add_enabled(selected, egui::Button::new("Split")).on_hover_text("S · split selected item at playhead").clicked() { self.split(ui.ctx()); }
                    if ui.add_enabled(selected, egui::Button::new("Duplicate")).clicked() { self.duplicate(ui.ctx()); }
                    if ui.add_enabled(selected, egui::Button::new("Remove")).on_hover_text("Delete").clicked() { self.remove_selection(ui.ctx()); }
                    if ui.button("+ Title").clicked() { self.start_generator(Generated::Title); }
                    if ui.button("+ Color").clicked() { self.start_generator(Generated::Color); }
                    let shots = &self.history.project.shots;
                    let earlier = matches!(self.selected, Some(Selection::Shot(id)) if shots.first().is_some_and(|shot| shot.id != id));
                    let later = matches!(self.selected, Some(Selection::Shot(id)) if shots.last().is_some_and(|shot| shot.id != id));
                    if ui.add_enabled(earlier, egui::Button::new("Move Earlier")).clicked() { self.move_selected(-1, ui.ctx()); }
                    if ui.add_enabled(later, egui::Button::new("Move Later")).clicked() { self.move_selected(1, ui.ctx()); }
                });

                let duration = self.history.project.duration();
                let domain = duration.max(2 * TICKS_PER_SECOND);
                ui.horizontal_wrapped(|ui| {
                    let mut seek = seconds(self.playhead);
                    if super::inspector::time_field(ui, "Playhead", &mut seek, seconds(duration)).changed() {
                        self.seek(ticks(seek), ui.ctx());
                    }
                    self.item_picker(ui);
                    if ui.button("Fit").on_hover_text("Fit the full timeline").clicked() { self.timeline.window = None; }
                    for (title, factor) in [("Zoom Out", 1.6), ("Zoom In", 0.625)] {
                        let button = ui.button(if factor > 1.0 { "−" } else { "+" });
                        button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, title));
                        if button.on_hover_text(title).clicked() { self.zoom(domain, factor, self.playhead, 0.5); }
                    }
                    ui.menu_button("Pan", |ui| {
                        if ui.button("Pan Left").clicked() { self.pan(domain, -0.3); ui.close(); }
                        if ui.button("Pan Right").clicked() { self.pan(domain, 0.3); ui.close(); }
                    });
                });
                ui.add_space(5.0);
                self.board(ui, domain, accent);
            });
    }

    fn item_picker(&mut self, ui: &mut Ui) {
        let project = &self.history.project;
        let width = ui.available_width().clamp(70.0, 138.0);
        let label = if self.selected.is_some() { "Selected Item" } else { "Select Item" };
        egui::ComboBox::from_id_salt("editor-select-item").width(width).selected_text(label).show_ui(ui, |ui| {
            ui.selectable_value(&mut self.selected, None, "None");
            let mut at = 0;
            for (idx, shot) in project.shots.iter().enumerate() {
                let name = shot_name(project, &shot.source);
                let label = format!("Video {} · {} · {name}", idx + 1, clock(at));
                ui.selectable_value(&mut self.selected, Some(Selection::Shot(shot.id)), label);
                at += shot.range.len();
            }
            for sound in &project.sounds {
                let name = project.asset(sound.asset).map(|asset| filename(&asset.path)).unwrap_or_default();
                let label = format!("{} · {} · {name}", role_name(sound.role), clock(sound.at));
                ui.selectable_value(&mut self.selected, Some(Selection::Sound(sound.id)), label);
            }
        });
    }

    fn board(&mut self, ui: &mut Ui, domain: u64, accent: Color32) {
        let lanes = lanes(&self.history.project);
        let total_rows: usize = lanes.iter().map(|lane| lane.rows.len()).sum();
        let window = Window::visible(self.timeline.window, domain);
        let width = ui.available_width();
        let labels = (width * 0.22).clamp(54.0, 92.0);
        let (ruler, _) = ui.allocate_exact_size(egui::vec2(width, TICK_HEIGHT), Sense::hover());
        let axis = Rect::from_min_max(ruler.left_top() + egui::vec2(labels, 0.0), ruler.right_bottom() - egui::vec2(9.0, 0.0));
        let response = ui.interact(axis, ui.make_persistent_id("editor-ruler"), Sense::click_and_drag());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Scrub Timeline"));
        let cursor = self.playhead;
        if response.clicked() || response.dragged() || response.drag_stopped() {
            if let Some(pos) = response.interact_pointer_pos() {
                let at = self.snap(window.tick(axis, pos.x));
                if self.playhead != at { self.seek(at, ui.ctx()); }
            }
        }
        self.axis_scroll(ui, &response, window, axis, domain);
        paint_ruler(ui, ruler, axis, window, cursor, accent);

        let mut drop = None;
        let mut select = None;
        let mut committed = None;
        let step = LANE_HEIGHT + ui.spacing().item_spacing.y;
        let visible_height = 4.0 * step - ui.spacing().item_spacing.y + 2.0;
        egui::ScrollArea::vertical().id_salt("editor-track-scroll").max_height(visible_height)
            .auto_shrink([false, true]).show(ui, |ui| {
                // Use the ruler's horizontal transform for every lane, including when the rows scroll vertically.
                let board_right = axis.right().min(ui.clip_rect().right());
                for (lane_index, lane) in lanes.into_iter().enumerate() {
                    for (row_index, row) in lane.rows.into_iter().enumerate() {
                        let (track, _) = ui.allocate_exact_size(egui::vec2(width, LANE_HEIGHT), Sense::hover());
                        let content = Rect::from_min_max(Pos2::new(axis.left(), track.top()), Pos2::new(board_right, track.bottom()));
                        if !content.is_positive() { continue; }
                        let response = ui.interact(content, ui.make_persistent_id(("editor-drop", lane_index, row_index)), Sense::click());
                        if response.clicked() {
                            if let Some(pos) = response.interact_pointer_pos() {
                                select = Some(None);
                                let at = self.snap(window.tick(axis, pos.x));
                                self.seek(at, ui.ctx());
                            }
                        }
                        let name = lane.role.map_or("Video", role_name);
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{name} track")));
                        paint_track(ui, track, content, name, row_index, row.is_empty(), total_rows);
                        if response.dnd_hover_payload::<BinAsset>().is_some() {
                            ui.painter_at(content).rect_stroke(content.shrink(1.0), 4.0, Stroke::new(1.5, accent), StrokeKind::Inside);
                        }
                        if let Some(asset) = response.dnd_release_payload::<BinAsset>() {
                            if let Some(pos) = ui.input(|input| input.pointer.interact_pos()) {
                                drop = Some((asset.0, self.snap(window.tick(axis, pos.x)), lane.role));
                            }
                        }
                        self.axis_scroll(ui, &response, window, axis, domain);
                        for item in row {
                            let end = item.at.saturating_add(item.range.len());
                            let left = window.x(axis, item.at);
                            let right = window.x(axis, end);
                            let rect = Rect::from_min_max(Pos2::new(left, content.top() + 4.0), Pos2::new(right, content.bottom() - 4.0))
                                .intersect(content);
                            if !rect.is_positive() || rect.width() < 0.5 { continue; }
                            let id = ui.make_persistent_id(("editor-item", item.selection.id()));
                            let clip = ui.interact(rect, id, Sense::click_and_drag());
                            let selected = self.selected == Some(item.selection);
                            clip.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, &item.label));
                            if clip.clicked() { select = Some(Some(item.selection)); }
                            if clip.double_clicked() { self.seek(item.at, ui.ctx()); }

                            if let Some(pos) = clip.hover_pos() {
                                let edge = edge_at(pos.x, left, right, rect.width());
                                if !matches!(edge, Drag::Move) { ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal); }
                                else { ui.ctx().set_cursor_icon(CursorIcon::Grab); }
                            }
                            if clip.drag_started() {
                                if let Some(pos) = clip.interact_pointer_pos() {
                                    let origin = pos.x - clip.total_drag_delta().unwrap_or_default().x;
                                    let kind = edge_at(origin, left, right, rect.width());
                                    self.timeline.gesture = Some(Gesture {
                                        item: item.clone(), kind, origin,
                                        ticks_per_point: window.span as f64 / f64::from(axis.width()),
                                    });
                                    select = Some(Some(item.selection));
                                    self.pause();
                                }
                            }
                            if clip.drag_stopped() {
                                if let Some(gesture) = self.timeline.gesture.take() {
                                    if let Some(pos) = ui.input(|input| input.pointer.interact_pos()) {
                                        committed = Some((gesture, pos.x));
                                    }
                                }
                            }
                            paint_item(ui, rect, &item, selected, clip.has_focus(), accent);
                            clip.on_hover_text(format!("{}\n{} · {}", item.label, clock(item.at), clock(item.range.len())));
                        }
                        let x = window.x(axis, self.playhead);
                        if content.x_range().contains(x) {
                            ui.painter_at(content).line_segment([Pos2::new(x, content.top()), Pos2::new(x, content.bottom())], Stroke::new(1.6, accent));
                        }
                        if let Some((gesture, pos)) = self.timeline.gesture.as_ref().zip(ui.input(|i| i.pointer.interact_pos())) {
                            let tick = self.gesture_tick(gesture, pos.x);
                            let x = window.x(axis, tick);
                            if content.x_range().contains(x) {
                                ui.painter_at(content).line_segment(
                                    [Pos2::new(x, content.top()), Pos2::new(x, content.bottom())],
                                    Stroke::new(1.2, theme::TEXT_DIM),
                                );
                            }
                        }
                    }
                }
            });
        if let Some(selection) = select { self.selected = selection; }
        if let Some((id, at, role)) = drop { self.place_asset(id, Some(at), role, ui.ctx()); }
        if let Some((gesture, x)) = committed { self.commit_gesture(gesture, x, ui.ctx()); }
    }

    fn axis_scroll(&mut self, ui: &Ui, response: &egui::Response, window: Window, axis: Rect, duration: u64) {
        if let Some(pos) = response.hover_pos() {
            let (delta, modifiers) = ui.input(|input| (input.smooth_scroll_delta, input.modifiers));
            if modifiers.ctrl && delta.y.abs() > 0.1 {
                let anchor = window.tick(axis, pos.x);
                let fraction = ((pos.x - axis.left()) / axis.width()).clamp(0.0, 1.0);
                self.zoom(duration, (-f64::from(delta.y) * 0.008).exp(), anchor, f64::from(fraction));
                ui.input_mut(|input| input.smooth_scroll_delta.y = 0.0);
            } else if modifiers.shift && delta.y.abs() > 0.1 || delta.x.abs() > 0.1 {
                let x = if delta.x.abs() > 0.1 { delta.x } else { delta.y };
                self.pan(duration, -f64::from(x / axis.width()));
                ui.input_mut(|input| input.smooth_scroll_delta = egui::Vec2::ZERO);
            }
        }
    }

    fn zoom(&mut self, duration: u64, factor: f64, center: u64, fraction: f64) {
        let window = Window::visible(self.timeline.window, duration);
        let span = (window.span as f64 * factor).round() as u64;
        let span = span.clamp((TICKS_PER_SECOND / 10).min(duration), duration);
        let start = center.saturating_sub((span as f64 * fraction).round() as u64);
        self.timeline.window = Some(Window::visible(Some(Window { start, span }), duration));
    }

    fn pan(&mut self, duration: u64, fraction: f64) {
        let window = Window::visible(self.timeline.window, duration);
        let start = window.start.saturating_add_signed((window.span as f64 * fraction).round() as i64);
        self.timeline.window = Some(Window::visible(Some(Window { start, span: window.span }), duration));
    }

    fn snap(&self, at: u64) -> u64 {
        let fps = self.history.project.canvas.fps;
        frame_tick(frame_number(at, fps), fps)
    }

    fn gesture_tick(&self, gesture: &Gesture, x: f32) -> u64 {
        let base = match gesture.kind {
            Drag::TrimOut => gesture.item.at.saturating_add(gesture.item.range.len()),
            _ => gesture.item.at,
        };
        self.snap(base.saturating_add_signed((f64::from(x - gesture.origin) * gesture.ticks_per_point).round() as i64))
    }

    fn commit_gesture(&mut self, gesture: Gesture, x: f32, ctx: &egui::Context) {
        let target = self.gesture_tick(&gesture, x);
        let item = gesture.item;
        let min = frame_tick(1, self.history.project.canvas.fps).min(item.range.len());
        if matches!((gesture.kind, item.selection), (Drag::Move, Selection::Shot(_))) {
            let midpoint = target.saturating_add(item.range.len() / 2);
            let id = item.selection.id();
            let index = self.insertion_index(midpoint, Some(id));
            self.change(ctx, |project| project.move_shot(id, index));
            return;
        }

        self.change(ctx, |project| {
            let (range, fade, at) = match item.selection {
                Selection::Shot(id) => {
                    let shot = project.shots.iter_mut().find(|shot| shot.id == id).context("Select a video item")?;
                    (&mut shot.range, &mut shot.audio_fade, None)
                }
                Selection::Sound(id) => {
                    let sound = project.sounds.iter_mut().find(|sound| sound.id == id).context("Select an audio item")?;
                    (&mut sound.range, &mut sound.audio_fade, Some(&mut sound.at))
                }
            };
            let mut next = *range;
            match gesture.kind {
                Drag::Move => { if let Some(at) = at { *at = target; } }
                Drag::TrimIn => {
                    let new_start = if target >= item.at {
                        item.range.start.saturating_add(target - item.at)
                    } else {
                        item.range.start.saturating_sub(item.at - target)
                    }.min(item.range.end.saturating_sub(min));
                    if let Some(at) = at {
                        *at = item.at.saturating_add(new_start).saturating_sub(item.range.start);
                    }
                    next.start = new_start;
                }
                Drag::TrimOut => {
                    next.end = item.range.start.saturating_add(target.saturating_sub(item.at))
                        .clamp(item.range.start.saturating_add(min), item.limit);
                }
            }
            super::inspector::trim_and_fade(range, fade, next, None)
        });
    }

    pub(super) fn split(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selected else { return; };
        let at = self.snap(self.playhead);
        if let Some(id) = self.change(ctx, |project| project.split(selection.id(), at)) {
            self.selected = Some(match selection { Selection::Shot(_) => Selection::Shot(id), Selection::Sound(_) => Selection::Sound(id) });
        }
    }

    fn duplicate(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selected else { return; };
        if let Some(id) = self.change(ctx, |project| project.duplicate(selection.id())) {
            self.selected = Some(match selection { Selection::Shot(_) => Selection::Shot(id), Selection::Sound(_) => Selection::Sound(id) });
        }
    }

    pub(super) fn remove_selection(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selected else { return; };
        self.change(ctx, |project| project.remove(selection.id()));
    }

    fn move_selected(&mut self, delta: isize, ctx: &egui::Context) {
        let Some(Selection::Shot(id)) = self.selected else { return; };
        let shots = &self.history.project.shots;
        let Some(index) = shots.iter().position(|shot| shot.id == id) else { return; };
        let target = index.saturating_add_signed(delta).min(shots.len() - 1);
        if target != index { self.change(ctx, |project| project.move_shot(id, target)); }
    }
}

fn shot_name(project: &Project, source: &Source) -> String {
    match source {
        Source::Asset(id) => project.asset(*id).map(|asset| filename(&asset.path)).unwrap_or_else(|| "Missing media".into()),
        Source::Title(title) => title.clone(),
        Source::Color(_) => "Color matte".into(),
    }
}

fn lanes(project: &Project) -> Vec<Lane> {
    let mut at = 0;
    let video = project.shots.iter().map(|shot| {
        let limit = match shot.source {
            Source::Asset(id) => project.asset(id).filter(|asset| asset.info.kind == AssetKind::Video)
                .map_or(i64::MAX as u64, |asset| asset.info.duration),
            _ => i64::MAX as u64,
        };
        let rgb = match shot.source {
            Source::Color([r, g, b]) => Color32::from_rgb(r, g, b),
            Source::Title(_) => Color32::from_gray(91),
            Source::Asset(_) => Color32::from_gray(69),
        };
        let item = Item { selection: Selection::Shot(shot.id), at, range: shot.range, limit, label: shot_name(project, &shot.source), rgb };
        at += shot.range.len();
        item
    }).collect();
    let mut result = vec![Lane { role: None, rows: vec![video] }];
    for (role, rgb) in ROLES.into_iter().zip([Color32::from_gray(82), Color32::from_gray(67), Color32::from_gray(94)]) {
        let mut items = project.sounds.iter().filter(|sound| sound.role == role).collect::<Vec<_>>();
        items.sort_by_key(|sound| (sound.at, sound.id));
        let mut rows: Vec<Vec<Item>> = vec![Vec::new()];
        for sound in items {
            let Some(asset) = project.asset(sound.asset) else { continue; };
            let index = rows.iter().position(|row| row.last().is_none_or(|last| last.at.saturating_add(last.range.len()) <= sound.at))
                .unwrap_or(rows.len());
            if rows.len() == index { rows.push(Vec::new()); }
            rows[index].push(Item {
                selection: Selection::Sound(sound.id), at: sound.at, range: sound.range, limit: asset.info.duration,
                label: filename(&asset.path), rgb,
            });
        }
        result.push(Lane { role: Some(role), rows });
    }
    result
}

fn edge_at(x: f32, left: f32, right: f32, visible_width: f32) -> Drag {
    let tolerance = (visible_width * 0.24).min(7.0);
    if visible_width < 12.0 { Drag::Move }
    else if (x - left).abs() <= tolerance { Drag::TrimIn }
    else if (x - right).abs() <= tolerance { Drag::TrimOut }
    else { Drag::Move }
}

fn paint_ruler(ui: &Ui, ruler: Rect, axis: Rect, window: Window, cursor: u64, accent: Color32) {
    let painter = ui.painter_at(ruler);
    painter.rect_filled(ruler, 4.0, theme::SIDEBAR);
    painter.text(ruler.left_center() + egui::vec2(6.0, 0.0), Align2::LEFT_CENTER, "Time",
        FontId::proportional(12.0), theme::TEXT_DIM);
    let approx = window.span as f64 * 80.0 / f64::from(axis.width().max(1.0));
    let power = 10.0_f64.powf(approx.max(1.0).log10().floor());
    let interval = ([1.0, 2.0, 5.0, 10.0].into_iter().find(|value| power * value >= approx).unwrap_or(10.0) * power).round().max(1.0) as u64;
    let end = window.start.saturating_add(window.span);
    let first = window.start.div_ceil(interval).saturating_mul(interval);
    let placed = ui.painter_at(axis);
    let mut tick = first;
    while tick <= end {
        let x = window.x(axis, tick);
        placed.line_segment([Pos2::new(x, axis.bottom() - 5.0), Pos2::new(x, axis.bottom())], Stroke::new(1.0, theme::BORDER_STRONG));
        let name = if interval < TICKS_PER_SECOND { clock(tick) } else { clock(tick).split('.').next().unwrap_or_default().to_owned() };
        let galley = placed.layout_no_wrap(name, FontId::monospace(11.0), theme::TEXT_DIM);
        if x + galley.size().x + 3.0 <= axis.right() {
            placed.galley(Pos2::new(x + 3.0, axis.top() + 4.0), galley, theme::TEXT_DIM);
        }
        let Some(next) = tick.checked_add(interval) else { break; };
        tick = next;
    }
    let x = window.x(axis, cursor);
    if axis.x_range().contains(x) {
        placed.line_segment([Pos2::new(x, axis.top()), Pos2::new(x, axis.bottom())], Stroke::new(1.6, accent));
        placed.circle_filled(Pos2::new(x, axis.top() + 3.0), 3.0, accent);
    }
}

fn paint_track(ui: &Ui, track: Rect, content: Rect, label: &str, row: usize, empty: bool, total_rows: usize) {
    let painter = ui.painter_at(track);
    painter.rect_filled(track, 0.0, theme::SIDEBAR);
    painter.rect_filled(content, 4.0, theme::SURFACE2);
    if row == 0 {
        painter.text(track.left_center() + egui::vec2(6.0, 0.0), Align2::LEFT_CENTER, label,
            FontId::new(12.0, theme::semibold()), theme::TEXT);
    }
    if empty && total_rows == 4 && content.width() >= 170.0 {
        let hint = if label == "Video" { "Drop video or images here" } else { "Drop audio here" };
        painter.text(content.left_center() + egui::vec2(7.0, 0.0), Align2::LEFT_CENTER, hint,
            FontId::proportional(11.0), theme::TEXT_FAINT);
    }
}

fn paint_item(ui: &Ui, rect: Rect, item: &Item, selected: bool, focused: bool, accent: Color32) {
    let painter = ui.painter_at(rect);
    let border = if focused { theme::TEXT } else if selected { accent } else { theme::BORDER_STRONG };
    painter.rect(rect, 4.0, item.rgb, Stroke::new(if selected || focused { 1.8 } else { 1.0 }, border), StrokeKind::Inside);
    if rect.width() >= 14.0 {
        let color = if theme::on_fill(item.rgb) == Color32::WHITE { Color32::WHITE } else { Color32::BLACK };
        let mut text = egui::text::LayoutJob::simple(item.label.clone(), FontId::proportional(12.0), color, rect.width() - 12.0);
        text.wrap.max_rows = 1;
        text.wrap.break_anywhere = true;
        let galley = painter.layout_job(text);
        painter.galley(rect.left_center() + egui::vec2(6.0, -galley.size().y * 0.5), galley, color);
        if selected && rect.width() >= 35.0 {
            for x in [rect.left() + 3.0, rect.right() - 3.0] {
                painter.line_segment([Pos2::new(x, rect.center().y - 4.0), Pos2::new(x, rect.center().y + 4.0)], Stroke::new(1.5, accent));
            }
        }
    }
}
