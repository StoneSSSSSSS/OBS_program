//! Opt-in native-window regression. Inspect each rendered layout as the window
//! shrinks and grows. The fixture deliberately uses a separate profile and GUI.
use super::{App, Page};
mod category;
mod interaction;
mod shortcut;
use category::CategoryCheck;
use interaction::Interaction;
use eframe::egui::{self, epaint::Shape, Context, Rect, Vec2, ViewportCommand};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PAGES: &[(&str, &str)] = &[
    ("home", "Recent footage"),
    ("recordings", "24/7 Recordings"),
    ("recording_files", "Jobs"),
    ("library", "All clips"),
    ("editor", "Synthetic highlight"),
    ("editor_trim", "Trim"),
    ("editor_export", "Export to share"),
    ("workbench", "Workbench"),
    ("workbench_preview", "Workbench"),
    ("workbench_wiki", "Editor Wiki"),
    ("settings_general", "Startup"),
    ("settings_recording", "Recording history"),
    ("settings_clips", "Saved clip length"),
    ("settings_audio", "Microphone guard"),
    ("settings_hotkeys", "Global hotkeys"),
    ("settings_overlay", "Clip saved overlay"),
    ("settings_games", "Detection"),
    ("settings_player", "Playback"),
    ("settings_export", "Exports"),
    ("settings_advanced", "Service"),
    ("settings_about", "Version "),
];
const WIDTHS: &[f32] = &[
    690., 716., 754., 788., 812., 838., 840., 842., 880., 914., 950., 980., 1008., 1036., 1049.,
    1060., 1071., 1080., 1094., 1105., 1118., 1120., 1122., 1148., 1161., 1180., 1217., 1250.,
    1300.,
];

const WORKBENCH_WIDTHS: &[f32] = &[
    360., 420., 530., 590., 598., 600., 602., 618., 620., 622., 690., 780., 838., 840.,
    842., 898., 900., 902., 990., 1048., 1050., 1052., 1120., 1180., 1250., 1300.,
];

struct Line {
    label: String,
    bounds: Rect,
    clip: Rect,
}

struct Opaque {
    after_lines: usize,
    bounds: Rect,
}

impl Line {
    fn visible(&self) -> Rect {
        self.bounds.intersect(self.clip)
    }
}

fn collect_lines(shape: &Shape, clip: Rect, lines: &mut Vec<Line>, opaque: &mut Vec<Opaque>) {
    match shape {
        Shape::Vec(items) => {
            for shape in items {
                collect_lines(shape, clip, lines, opaque);
            }
        }
        Shape::Rect(rect) if rect.fill.a() == 255 && rect.blur_width == 0.0 => {
            let r = rect.corner_radius;
            let inset = f32::from(r.nw.max(r.ne).max(r.sw).max(r.se));
            // Ignore the rounded corners. A label wholly inside the remaining
            // fill has been covered, even when its own clip rect is intact.
            let bounds = rect.rect.shrink(inset).intersect(clip);
            if bounds.is_positive() {
                opaque.push(Opaque {
                    after_lines: lines.len(),
                    bounds,
                });
            }
        }
        Shape::Text(text) => {
            for row in &text.galley.rows {
                let label = row.text();
                if !label.chars().any(char::is_alphanumeric) {
                    continue;
                }
                let bounds = row
                    .visuals
                    .mesh_bounds
                    .translate(text.pos.to_vec2() + row.pos.to_vec2());
                lines.push(Line {
                    label,
                    bounds,
                    clip,
                });
            }
        }
        _ => {}
    }
}

fn overlaps(lines: &[Line], issues: &mut BTreeSet<String>) {
    let visible: Vec<_> = lines
        .iter()
        .filter_map(|line| {
            let rect = line.visible();
            (rect.width() >= 3.0 && rect.height() >= 4.0).then_some((&line.label, rect))
        })
        .collect();
    for (i, (a, a_bounds)) in visible.iter().enumerate() {
        for (b, b_bounds) in &visible[..i] {
            let shared = a_bounds.intersect(*b_bounds);
            if shared.width() > 3.
                && shared.height() > 0.55 * a_bounds.height().min(b_bounds.height())
            {
                issues.insert(format!("text overlap: {b} / {a}"));
            }
        }
    }
}

/// A trailing space requires a prefix with a word boundary, avoiding matches like
/// the trim help sentence or the Save replay button when checking Settings Save.
fn require_visible(
    lines: &[Line],
    opaque: &[Opaque],
    expected: &str,
    issues: &mut BTreeSet<String>,
) {
    let mut found = false;
    let mut occluded = false;
    for (index, line) in lines.iter().enumerate() {
        let label = line.label.trim();
        if if expected.ends_with(' ') {
            !label.starts_with(expected)
        } else {
            label != expected
        } {
            continue;
        }
        found = true;
        let visible = line.visible();
        if visible.width() >= 3.0
            && visible.height() >= 4.0
            && line.clip.expand(1.0).contains_rect(line.bounds)
        {
            if !opaque
                .iter()
                .any(|rect| rect.after_lines > index && rect.bounds.contains_rect(visible))
            {
                return;
            }
            occluded = true;
        }
    }
    let issue = if occluded {
        "required text occluded"
    } else if found {
        "required text clipped"
    } else {
        "required text missing"
    };
    issues.insert(format!("{issue}: {}", expected.trim_end()));
}

fn inspect(
    ctx: &Context,
    page: &str,
    page_label: &str,
    verify_controls: bool,
) -> (usize, BTreeSet<String>) {
    let rect = ctx.viewport_rect();
    let layer = egui::LayerId::background();
    let mut lines = Vec::new();
    let mut opaque = Vec::new();
    let mut issues = BTreeSet::new();
    ctx.graphics(|g| {
        if let Some(shapes) = g.get(layer) {
            for item in shapes.all_entries() {
                collect_lines(
                    &item.shape,
                    item.clip_rect.intersect(rect),
                    &mut lines,
                    &mut opaque,
                );
            }
        }
    });
    let visible_count = lines.iter().filter(|l| l.visible().is_positive()).count();
    if visible_count < 5 {
        issues.insert("missing rendered page text".into());
    }
    if verify_controls {
        require_visible(&lines, &opaque, page_label, &mut issues);
        let pinned = match page {
            "home" => &["Open latest recording · ", "Clip last 30 seconds"][..],
            "recordings" => &["Set start ", "Set end ", "Save selected clip"],
            "editor_trim" => &["Set start ", "Set end ", "Save as new clip"],
            "editor_export" => &["Export to size limit", "Export full quality"],
            "workbench" | "workbench_preview" => &["Project", "Import Media…", "Export MP4…"],
            "workbench_wiki" => &["All topics", "Clear"],
            page if page.starts_with("settings_") => &["Save", "Discard"],
            _ => &[],
        };
        for expected in pinned {
            require_visible(&lines, &opaque, expected, &mut issues);
        }
    }
    overlaps(&lines, &mut issues);
    (visible_count, issues)
}

pub(super) struct Sweep {
    dir: PathBuf,
    log: File,
    page: usize,
    step: usize,
    requested: Option<(Vec2, Instant)>,
    issues: BTreeSet<String>,
    frames: usize,
    scale: f32,
    initialized: bool,
    category: Option<CategoryCheck>,
    interaction: Option<Interaction>,
    shortcut: Option<shortcut::ShortcutCheck>,
    setup_started: Option<Instant>,
    scroll_passes_remaining: u8,
    screenshot: Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, Instant)>,
}

impl Sweep {
    pub(super) fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("CLIPVAULT_RESIZE_CHECK")?);
        let profile = PathBuf::from(std::env::var_os("CLIPVAULT_DATA_DIR")?);
        if !dir.is_absolute() || !profile.file_name()?.to_str()?.starts_with("clipvault-ipc-") {
            return None;
        }
        let log = File::create_new(dir.join("frames.tsv")).ok()?;
        let scale = std::env::var("CLIPVAULT_RESIZE_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|s| (0.75..=2.0).contains(s))
            .unwrap_or(1.0);
        Some(Self {
            dir,
            log,
            page: 0,
            step: 0,
            requested: None,
            issues: BTreeSet::new(),
            frames: 0,
            scale,
            initialized: false,
            category: Some(CategoryCheck::default()),
            interaction: std::env::var_os("CLIPVAULT_INTERACTION_CHECK")
                .map(|_| Interaction::new(profile)),
            shortcut: std::env::var_os("CLIPVAULT_INTERACTION_CHECK")
                .map(|_| shortcut::ShortcutCheck::default()),
            setup_started: None,
            scroll_passes_remaining: 0,
            screenshot: None,
        })
    }

    fn enter_page(&self, app: &mut App) {
        match PAGES[self.page].0 {
            "home" => app.navigate(Page::Home),
            "recordings" => {
                app.navigate(Page::Recordings);
                if let Some(recording) = app
                    .c
                    .db()
                    .and_then(|db| db.list_recordings().ok())
                    .and_then(|v| v.into_iter().next())
                {
                    crate::views::recordings::request_play_at(recording.start_ms);
                }
            }
            "library" => app.navigate(Page::Clips),
            name if name.starts_with("workbench") => {
                app.navigate(Page::Editor);
                app.editor.qa_view(name);
            }
            "recording_files" => app.recordings.show_compact_files(),
            "editor_trim" | "editor_export" => app.clips.show_compact_inspector(),
            "editor" => {
                if let Some(clip) = app
                    .c
                    .db()
                    .and_then(|db| db.list_clips(&Default::default()).ok())
                    .and_then(|v| v.into_iter().next())
                {
                    crate::views::clips::request_open(clip.id);
                }
            }
            name => {
                app.navigate(Page::Settings);
                app.settings
                    .select_tab(name.trim_start_matches("settings_"));
                app.settings.qa_seed_dirty(&app.c);
            }
        }
    }

    pub(super) fn inject_input(&mut self, input: &mut egui::RawInput) {
        if let Some(category) = &mut self.category {
            category.inject(input);
        }
        if let Some(shortcut) = &mut self.shortcut {
            shortcut.inject(input);
        }
        if let Some(interaction) = &mut self.interaction {
            interaction.inject(input);
        }
    }

    fn finish(&self, ctx: &Context) {
        let status = if self.issues.is_empty() { "PASS" } else { "FAIL" };
        let report = format!(
            "{status}: {} rendered frames\n{}\n",
            self.frames,
            self.issues.iter().cloned().collect::<Vec<_>>().join("\n")
        );
        let _ = std::fs::write(self.dir.join("result.txt"), report);
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    pub(super) fn after_frame(&mut self, app: &mut App, ctx: &Context) {
        if ctx.will_discard() {
            return;
        }
        ctx.request_repaint();
        if self.page >= PAGES.len() {
            if let Some(category) = &mut self.category {
                match category.advance(app, ctx, &self.dir) {
                    Ok(false) => return,
                    Ok(true) => {}
                    Err(error) => {
                        self.issues.insert(format!("library categories: {error}"));
                    }
                }
                category::stop(ctx);
                self.category = None;
            }
            if let Some(shortcut) = &mut self.shortcut {
                match shortcut.advance(app, ctx, &self.dir) {
                    Ok(false) => return,
                    Ok(true) => {}
                    Err(error) => { self.issues.insert(format!("player shortcuts: {error}")); }
                }
                self.shortcut = None;
            }
            if let Some(interaction) = &mut self.interaction {
                match interaction.advance(app, ctx, &self.dir) {
                    Ok(false) => return,
                    Ok(true) => {},
                    Err(error) => { self.issues.insert(format!("interactions: {error}")); }
                }
                self.interaction = None;
            }
            self.finish(ctx);
            return;
        }
        // Winit converts the size using the previous pass's pixels-per-point.
        // Apply the zoom before issuing the first native size command.
        if !self.initialized {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
            ctx.set_zoom_factor(self.scale);
            self.initialized = true;
            return;
        }
        if self.setup_started.is_none() {
            self.enter_page(app);
            self.setup_started = Some(Instant::now());
            return;
        }
        if self.requested.is_none() && self.step == 0 {
            let (page, _) = PAGES[self.page];
            let seeded = match page {
                "recordings" => app.recordings.qa_mark_trim(),
                "editor_trim" | "editor_export" => app.clips.qa_mark_trim(&app.c),
                _ => true,
            };
            if !seeded {
                if self
                    .setup_started
                    .is_some_and(|started| started.elapsed() < Duration::from_secs(3))
                {
                    return;
                }
                self.issues.insert(format!(
                    "{page}: could not establish a valid fixture selection"
                ));
            }
        }

        let widths = if PAGES[self.page].0.starts_with("workbench") { WORKBENCH_WIDTHS } else { WIDTHS };
        let size = ctx.viewport_rect().size();
        if let Some((wanted, since)) = self.requested {
            let (page, label) = PAGES[self.page];
            let ready = (size - wanted).abs().max_elem() < 2.;
            let timed_out = since.elapsed() > Duration::from_secs(2);
            let (count, issues) = inspect(ctx, page, label, ready && self.scroll_passes_remaining == 0);
            self.frames += 1;
            for issue in &issues {
                self.issues.insert(format!("{page}: {issue}"));
            }
            let _ = writeln!(
                self.log,
                "{}\t{}\t{:.1}\t{:.1}\t{count}\t{}",
                page,
                self.step,
                size.x,
                size.y,
                issues.into_iter().collect::<Vec<_>>().join(" | ")
            );
            if !ready && !timed_out {
                return;
            }
            if !ready {
                self.issues.insert(format!(
                    "{page}: native size did not reach {wanted:?}, got {size:?}"
                ));
            }
            if self.scroll_passes_remaining > 0 {
                // The inspector can change between a side pane and a full-width tab.
                // Apply the intended scroll after the native size is reached, then
                // allow the scroll area to use its new offset on the following pass.
                if self.scroll_passes_remaining == 2 {
                    app.clips.qa_show_export();
                }
                self.scroll_passes_remaining -= 1;
                return;
            }
            let capture = if matches!(
                page,
                "editor" | "recordings" | "editor_trim" | "editor_export" | "workbench" | "workbench_preview" | "workbench_wiki"
            ) {
                (page.starts_with("workbench") && self.step == widths.len() - 1)
                    || [0, 8, 10, 12, 16, 22, widths.len() + 9].contains(&self.step)
            } else {
                self.step == 10 || self.step == widths.len() + 1
            };
            if let Some((done, started)) = &self.screenshot {
                if !done.load(std::sync::atomic::Ordering::Acquire) {
                    if started.elapsed() < Duration::from_secs(3) {
                        return;
                    }
                    self.issues
                        .insert(format!("{page}: missing screenshot at {size:?}"));
                }
                self.screenshot = None;
            } else if capture {
                let path = self.dir.join(format!(
                    "{}-{:02}-{}x{}.png",
                    page,
                    self.step,
                    size.x.round(),
                    size.y.round()
                ));
                let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.screenshot = Some((done.clone(), Instant::now()));
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(
                    move |image| {
                        let rgba = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                        if let Some(im) = image::RgbaImage::from_raw(
                            image.size[0] as u32,
                            image.size[1] as u32,
                            rgba,
                        ) {
                            if im.save(&path).is_ok() {
                                done.store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                    },
                )));
                return;
            }
            self.step += 1;
            if self.step == widths.len() * 2 {
                self.page += 1;
                self.step = 0;
                if self.page == PAGES.len() {
                    return;
                }
                self.setup_started = None;
                self.requested = None;
                return;
            }
        }
        let index = if self.step < widths.len() {
            widths.len() - 1 - self.step
        } else {
            self.step - widths.len()
        };
        let width = widths[index];
        let height = if self.step < widths.len() {
            671.
        } else {
            510. + 0.27 * (width - widths[0])
        };
        let wanted = Vec2::new(width, height);
        self.requested = Some((wanted, Instant::now()));
        if PAGES[self.page].0 == "editor_export" {
            app.clips.qa_show_export();
            self.scroll_passes_remaining = 2;
        }
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
    }
}

#[test]
fn detects_crossing_labels_and_accepts_adjacent_controls() {
    let line = |label: &str, left: f32| Line {
        label: label.into(),
        bounds: Rect::from_min_size(egui::pos2(left, 8.), egui::vec2(60., 14.)),
        clip: Rect::EVERYTHING,
    };
    let mut issues = BTreeSet::new();
    overlaps(&[line("Player", 0.), line("Trim", 60.)], &mut issues);
    assert!(issues.is_empty());
    overlaps(&[line("Player", 0.), line("Trim", 40.)], &mut issues);
    assert!(issues.contains("text overlap: Player / Trim"));
}

#[test]
fn rejects_missing_cropped_and_completely_occluded_required_controls() {
    use egui::{pos2, vec2};
    let clip = Rect::from_min_size(pos2(0.0, 0.0), vec2(160.0, 40.0));
    let line = |label: &str, x, y| Line {
        label: label.into(),
        bounds: Rect::from_min_size(pos2(x, y), vec2(90.0, 15.0)),
        clip,
    };
    let mut issues = BTreeSet::new();
    require_visible(&[line("Save replay", 10.0, 5.0)], &[], "Save", &mut issues);
    assert!(issues.remove("required text missing: Save"));
    require_visible(
        &[line("Set start  00:04", 100.0, 5.0)],
        &[],
        "Set start ",
        &mut issues,
    );
    assert!(issues.remove("required text clipped: Set start"));
    require_visible(
        &[line("Set end  00:06", 10.0, 35.0)],
        &[],
        "Set end ",
        &mut issues,
    );
    assert!(issues.remove("required text clipped: Set end"));
    require_visible(&[line("Discard", 10.0, 60.0)], &[], "Discard", &mut issues);
    assert!(issues.remove("required text clipped: Discard"));
    let foreground = Opaque {
        after_lines: 1,
        bounds: clip,
    };
    require_visible(
        &[line("Discard", 10.0, 5.0)],
        &[foreground],
        "Discard",
        &mut issues,
    );
    assert!(issues.remove("required text occluded: Discard"));
    let background = Opaque {
        after_lines: 0,
        bounds: clip,
    };
    require_visible(
        &[line("Set start  00:04", 10.0, 5.0)],
        &[background],
        "Set start ",
        &mut issues,
    );
    assert!(issues.is_empty(), "{issues:?}");
}
