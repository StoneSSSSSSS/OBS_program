//! Opt-in input and renderer capture for disposable Editor review windows.

use eframe::egui::{self, Context, Event, Key, Modifiers, PointerButton, RawInput};
use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ENV: &str = "CLIPVAULT_EDITOR_REVIEW_DRIVER";
const REPARSE_POINT: u32 = 0x400;
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

pub struct Driver {
    run: PathBuf,
    mail: PathBuf,
    active: Option<(String, Action)>,
}

thread_local! {
    static DIALOG: RefCell<Option<DialogState>> = const { RefCell::new(None) };
}

struct DialogState {
    run: PathBuf,
    // Paths are selected in advance and consumed by a real picker button.
    next: Option<(bool, Vec<PathBuf>)>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Snapshot,
    Click {
        at: [u32; 2],
        button: Button,
        count: u8,
    },
    Drag {
        from: [u32; 2],
        to: [u32; 2],
        button: Button,
        steps: u8,
    },
    Scroll {
        at: [u32; 2],
        delta: [f32; 2],
    },
    Key {
        key: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
    Text {
        text: String,
    },
    Drop {
        at: [u32; 2],
        paths: Vec<PathBuf>,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Dialog {
        save: bool,
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Button {
    Primary,
    Secondary,
    Middle,
}

impl From<Button> for PointerButton {
    fn from(button: Button) -> Self {
        match button {
            Button::Primary => Self::Primary,
            Button::Secondary => Self::Secondary,
            Button::Middle => Self::Middle,
        }
    }
}

enum Action {
    Input(VecDeque<FrameInput>),
    Resize { size: [u32; 2], sent: Option<Instant> },
    Snapshot,
    Reply(Result<Value, String>),
}

#[derive(Default)]
struct FrameInput {
    events: Vec<Event>,
    dropped_files: Vec<egui::DroppedFileHandle>,
}

impl From<Vec<Event>> for FrameInput {
    fn from(events: Vec<Event>) -> Self {
        Self { events, ..Self::default() }
    }
}

impl Driver {
    pub fn from_env() -> Option<Self> {
        let mail = PathBuf::from(std::env::var_os(ENV)?);
        match Self::open(mail) {
            Ok(driver) => Some(driver),
            Err(error) => {
                tracing::warn!(%error, "Editor review driver disabled");
                None
            }
        }
    }

    fn open(mail: PathBuf) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::PermissionDenied, "not an isolated Editor review");
        let run = mail.parent().ok_or_else(invalid)?.to_owned();
        let reviews = run.parent().ok_or_else(invalid)?;
        let qa = reviews.parent().ok_or_else(invalid)?;
        let local = qa.parent().ok_or_else(invalid)?;
        let project = local.parent().ok_or_else(invalid)?;
        let manifest = project.join(".clipvault-project");
        let profile = run.join("profile");
        let profile_env = std::env::var_os("CLIPVAULT_DATA_DIR").map(PathBuf::from);
        if !mail.is_absolute()
            || mail.file_name().and_then(|s| s.to_str()) != Some("driver")
            || reviews.file_name().and_then(|s| s.to_str()) != Some("editor-reviews")
            || qa.file_name().and_then(|s| s.to_str()) != Some("qa")
            || local.file_name().and_then(|s| s.to_str()) != Some(".clipvault-local")
            || !run.file_name().and_then(|s| s.to_str()).is_some_and(valid_run_name)
            || !plain_metadata(&manifest)?.is_file()
            || !plain_dir_chain(&mail, project)
            || !plain_dir_chain(&profile, project)
            || profile_env.as_ref().filter(|p| p.is_absolute()).and_then(|p| p.canonicalize().ok())
                != Some(profile.canonicalize()?)
            || cv_core::paths::data_dir().canonicalize()? != profile.canonicalize()?
            || std::env::current_exe()?.canonicalize()? != run.join("clipvault.exe").canonicalize()?
            || fs::symlink_metadata(run.join(cv_core::paths::SERVICE_EXE)).is_ok()
        {
            return Err(invalid());
        }
        write_new(&mail.join("ready.json"), &serde_json::to_vec(&json!({
            "process_id": std::process::id(),
        }))?)?;
        DIALOG.with(|dialog| *dialog.borrow_mut() = Some(DialogState { run: run.clone(), next: None }));
        Ok(Self { run, mail, active: None })
    }

    pub fn inject(&mut self, ctx: &Context, input: &mut RawInput) {
        if input.viewport_id != egui::ViewportId::ROOT {
            return;
        }
        // Only this opt-in viewport receives the reviewer's input. Other windows can
        // use the desktop cursor and change focus without disturbing a gesture here.
        input.events.clear();
        input.hovered_files.clear();
        input.dropped_files.clear();
        input.focused = true;
        if let Some(viewport) = input.viewports.get_mut(&input.viewport_id) {
            viewport.focused = Some(true);
        }

        if self.active.is_none() {
            if let Some((id, request)) = self.read_request() {
                let action = request.and_then(|request| self.prepare(ctx, input, request))
                    .unwrap_or_else(|error| Action::Reply(Err(error)));
                self.active = Some((id, action));
            }
        }
        if let Some((_, Action::Input(frames))) = &mut self.active {
            if let Some(frame) = frames.pop_front() {
                input.events = frame.events;
                input.dropped_files = frame.dropped_files;
            }
        }
    }

    pub fn after_frame(&mut self, ctx: &Context) {
        ctx.request_repaint_after(Duration::from_millis(80));
        if ctx.will_discard() {
            return;
        }
        let Some((id, action)) = self.active.take() else { return };
        let result = match action {
            Action::Input(frames) if !frames.is_empty() => {
                self.active = Some((id, Action::Input(frames)));
                ctx.request_repaint_after(Duration::from_millis(15));
                return;
            }
            Action::Input(_) => Ok(json!({})),
            Action::Reply(result) => result,
            Action::Snapshot => {
                let mail = self.mail.clone();
                let scale = ctx.pixels_per_point();
                let native_scale = ctx.native_pixels_per_point().unwrap_or(1.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::ScreenshotCallback::new(move |img: std::sync::Arc<egui::ColorImage>| {
                        let result = capture(&mail, &id, &img, scale, native_scale)
                            .map_err(|error| error.to_string());
                        reply(&mail, &id, result);
                    }),
                ));
                return;
            }
            Action::Resize { size, sent } => {
                let ppp = ctx.pixels_per_point();
                let viewport_size = ctx.viewport_rect().size() * ppp;
                let actual = [viewport_size.x.round() as u32, viewport_size.y.round() as u32];
                if let Some(when) = sent {
                    if actual == size || when.elapsed() >= Duration::from_millis(800) {
                        let [width, height] = actual;
                        reply(&self.mail, &id, Ok(json!({
                            "width": width, "height": height, "exact_size": actual == size,
                        })));
                        return;
                    }
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                        egui::vec2(size[0] as f32 / ppp, size[1] as f32 / ppp),
                    ));
                }
                self.active = Some((id, Action::Resize { size, sent: sent.or_else(|| Some(Instant::now())) }));
                return;
            }
        };
        reply(&self.mail, &id, result);
    }

    fn read_request(&self) -> Option<(String, Result<Request, String>)> {
        if !plain_dir_chain(&self.mail, &self.run) {
            return None;
        }
        for entry in fs::read_dir(&self.mail).ok()?.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".request.json")).filter(|s| valid_id(s))
                else { continue };
            let path = entry.path();
            let request = (|| {
                if !plain_metadata(&path)?.is_file() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "request must be a regular file").into());
                }
                let mut contents = Vec::new();
                File::open(&path)?.take(MAX_REQUEST_BYTES + 1).read_to_end(&mut contents)?;
                if contents.len() as u64 > MAX_REQUEST_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "request exceeds 64 KiB").into());
                }
                Ok(serde_json::from_slice(&contents)?)
            })().map_err(|error: anyhow::Error| error.to_string());
            // Remove before executing so a failed reply can't cause a gesture to run twice.
            if fs::remove_file(path).is_ok() {
                return Some((id.to_owned(), request));
            }
        }
        None
    }

    fn prepare(&self, ctx: &Context, input: &RawInput, request: Request) -> Result<Action, String> {
        let scale = input.viewport().native_pixels_per_point.unwrap_or(1.0) * ctx.zoom_factor();
        let screen = input.screen_rect.unwrap_or_else(|| ctx.viewport_rect());
        let point = |[x, y]: [u32; 2]| {
            let pos = egui::pos2(x as f32 / scale, y as f32 / scale);
            (pos.x < screen.width() && pos.y < screen.height()).then_some(pos)
                .ok_or_else(|| "point is outside the current screenshot".to_owned())
        };
        let pointer_button = |pos, button: Button, pressed| Event::PointerButton {
            pos, button: button.into(), pressed, modifiers: Modifiers::NONE,
        };

        let mut frames = VecDeque::new();
        match request {
            Request::Snapshot => return Ok(Action::Snapshot),
            Request::Resize { width, height } => {
                if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
                    return Err("resize dimensions must be between 1 and 8192 pixels".to_owned());
                }
                return Ok(Action::Resize { size: [width, height], sent: None });
            }
            Request::Click { at, button, count } => {
                if !(1..=2).contains(&count) {
                    return Err("click count must be 1 or 2".to_owned());
                }
                let pos = point(at)?;
                frames.push_back(vec![Event::PointerMoved(pos)].into());
                for _ in 0..count {
                    frames.push_back(vec![pointer_button(pos, button, true)].into());
                    frames.push_back(vec![pointer_button(pos, button, false)].into());
                }
            }
            Request::Drag { from, to, button, steps } => {
                if !(2..=60).contains(&steps) {
                    return Err("drag steps must be between 2 and 60".to_owned());
                }
                let start = point(from)?;
                let end = point(to)?;
                frames.push_back(vec![Event::PointerMoved(start)].into());
                frames.push_back(vec![pointer_button(start, button, true)].into());
                for i in 1..=steps {
                    let pos = start + (end - start) * (f32::from(i) / f32::from(steps));
                    frames.push_back(vec![Event::PointerMoved(pos)].into());
                }
                frames.push_back(vec![pointer_button(end, button, false)].into());
            }
            Request::Scroll { at, delta: [dx, dy] } => {
                frames.push_back(vec![Event::PointerMoved(point(at)?)].into());
                frames.push_back(vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(dx, dy) / scale,
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Wheel,
                    modifiers: Modifiers::NONE,
                }].into());
            }
            Request::Key { key, ctrl, alt, shift } => {
                let key = Key::from_name(&key).ok_or_else(|| format!("unknown egui key {key}"))?;
                let modifiers = Modifiers { ctrl, alt, shift, command: ctrl, ..Modifiers::NONE };
                for pressed in [true, false] {
                    let key = Event::Key {
                        key, physical_key: Some(key), pressed, repeat: false, modifiers,
                    };
                    let events = if pressed {
                        vec![Event::ModifiersChanged(modifiers), key]
                    } else {
                        vec![key, Event::ModifiersChanged(Modifiers::NONE)]
                    };
                    frames.push_back(events.into());
                }
            }
            Request::Text { text } => frames.push_back(vec![Event::Text(text)].into()),
            Request::Drop { at, paths } => {
                let pos = point(at)?;
                let checked = self.check_paths(&paths, false)?;
                let dropped_files = paths.into_iter().zip(checked).map(|(relative, full)| {
                    Arc::new(ReviewFile { run: self.run.clone(), relative, full }) as egui::DroppedFileHandle
                }).collect::<Vec<_>>();
                if dropped_files.is_empty() {
                    return Err("provide at least one file to drop".to_owned());
                }
                frames.push_back(vec![Event::PointerMoved(pos)].into());
                frames.push_back(FrameInput { dropped_files, ..FrameInput::default() });
            }
            Request::Dialog { save, paths } => {
                if save && paths.len() > 1 {
                    return Err("a save dialog accepts at most one destination".to_owned());
                }
                self.check_paths(&paths, save)?;
                DIALOG.with(|dialog| {
                    if let Some(state) = dialog.borrow_mut().as_mut() {
                        state.next = Some((save, paths));
                    }
                });
                return Ok(Action::Reply(Ok(json!({ "queued": true }))));
            }
        }
        Ok(Action::Input(frames))
    }

    fn check_paths(&self, paths: &[PathBuf], save: bool) -> Result<Vec<PathBuf>, String> {
        paths.iter().map(|path| review_path(&self.run, path, save)).collect()
    }
}


#[derive(Debug)]
struct ReviewFile {
    run: PathBuf,
    relative: PathBuf,
    full: PathBuf,
}

impl egui::DroppedFile for ReviewFile {
    fn path(&self) -> &Path {
        &self.full
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        let path = review_path(&self.run, &self.relative, false)?;
        fs::read(path).map_err(|error| error.to_string())
    }
}
pub fn pick_file(native: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    single_dialog_file(false, native)
}

pub fn pick_files(native: impl FnOnce() -> Option<Vec<PathBuf>>) -> Option<Vec<PathBuf>> {
    match review_dialog(false) {
        Some(paths) => Some(paths).filter(|paths| !paths.is_empty()),
        None => native(),
    }
}

pub fn save_file(native: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    single_dialog_file(true, native)
}

fn single_dialog_file(save: bool, native: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    match review_dialog(save) {
        Some(mut paths) if paths.len() == 1 => paths.pop(),
        Some(_) => None,
        None => native(),
    }
}

fn review_dialog(save: bool) -> Option<Vec<PathBuf>> {
    DIALOG.with(|dialog| {
        let mut dialog = dialog.borrow_mut();
        let state = dialog.as_mut()?;
        let selected = state.next.take().filter(|(selected_save, _)| *selected_save == save);
        Some(selected.and_then(|(_, paths)| {
            paths.iter().map(|path| review_path(&state.run, path, save)).collect::<Result<Vec<_>, _>>().ok()
        }).unwrap_or_default())
    })
}

fn review_path(run: &Path, path: &Path, save: bool) -> Result<PathBuf, String> {
    let file_name_valid = |name: &std::ffi::OsStr| {
        let name = name.to_string_lossy();
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        !name.ends_with(['.', ' '])
            && !name.chars().any(|ch| ch.is_control() || ":<>\"|?*".contains(ch))
            && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
            && !stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| matches!(suffix, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    };
    let invalid = || format!("path must be a regular file inside this review run: {}", path.display());
    if !path.components().all(|component| matches!(component, Component::Normal(name) if file_name_valid(name)))
        || path.components().next().is_some_and(|component| {
            component.as_os_str().eq_ignore_ascii_case("driver")
        })
        || path.file_name().is_none()
    {
        return Err(invalid());
    }
    let full = run.join(path);
    if !full.parent().is_some_and(|parent| plain_dir_chain(parent, run)) {
        return Err(invalid());
    }
    match plain_metadata(&full) {
        Ok(metadata) if metadata.is_file() => Ok(full),
        Err(error) if save && error.kind() == io::ErrorKind::NotFound => Ok(full),
        _ => Err(invalid()),
    }
}

fn valid_run_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(brief, suffix)| {
        !brief.is_empty() && brief.len() <= 40
            && brief.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && suffix.len() == 8 && suffix.bytes().all(|c| c.is_ascii_hexdigit())
    })
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
}

fn plain_metadata(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "reparse points are not allowed"));
    }
    Ok(metadata)
}

fn plain_dir_chain(dir: &Path, root: &Path) -> bool {
    if !dir.starts_with(root) {
        return false;
    }
    // Visit parents first so metadata reads can't traverse a junction in the chain.
    if dir != root && !dir.parent().is_some_and(|parent| plain_dir_chain(parent, root)) {
        return false;
    }
    plain_metadata(dir).is_ok_and(|metadata| metadata.is_dir())
}

fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    new_file(path)?.write_all(bytes)
}

fn capture(mail: &Path, id: &str, image: &egui::ColorImage, scale: f32, native_scale: f32) -> anyhow::Result<Value> {
    anyhow::ensure!(mail.parent().is_some_and(|run| plain_dir_chain(mail, run)), "review directory unavailable");
    let [width, height] = image.size;
    let file_name = format!("{id}.png");
    let mut file = new_file(&mail.join(&file_name))?;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }
    use image::ImageEncoder as _;
    image::codecs::png::PngEncoder::new(&mut file).write_image(
        &rgba, width as u32, height as u32, image::ExtendedColorType::Rgba8,
    )?;
    Ok(json!({
        "image": file_name, "width": width, "height": height, "pixels_per_point": scale,
        "native_pixels_per_point": native_scale, "dpi": native_scale * 96.0,
    }))
}

fn reply(mail: &Path, id: &str, result: Result<Value, String>) {
    if !mail.parent().is_some_and(|run| plain_dir_chain(mail, run)) {
        return;
    }
    let result = match result {
        Ok(result) => json!({ "ok": true, "result": result }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    let partial = mail.join(format!("{id}.result.tmp"));
    let finish = || -> io::Result<()> {
        write_new(&partial, &serde_json::to_vec(&result)?)?;
        fs::rename(partial, mail.join(format!("{id}.result.json")))
    };
    if let Err(error) = finish() {
        tracing::warn!(%error, "could not write Editor review result");
    }
}
