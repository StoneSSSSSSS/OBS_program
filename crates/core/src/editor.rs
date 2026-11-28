//! Non-destructive projects. Time is measured in 100-nanosecond ticks.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

mod captions;
mod storage;
pub use captions::{Caption, CaptionRun};
#[cfg(test)]
mod tests;

pub type Id = u64;
pub const TICKS_PER_SECOND: u64 = 10_000_000;
const STILL_DURATION: u64 = 3 * TICKS_PER_SECOND;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind { Video, Image, Audio }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub kind: AssetKind,
    pub duration: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: Id,
    pub path: PathBuf,
    pub info: AssetInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Asset(Id),
    Title(String),
    Color([u8; 3]),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

impl Span {
    pub fn len(self) -> u64 { self.end.saturating_sub(self.start) }

    pub fn is_empty(self) -> bool { self.end <= self.start }

    fn intersection(self, other: Self) -> Option<Self> {
        let part = Self { start: self.start.max(other.start), end: self.end.min(other.end) };
        (!part.is_empty()).then_some(part)
    }

    fn check(self, limit: u64) -> Result<()> {
        ensure!(!self.is_empty(), "The end must come after the start.");
        ensure!(self.end <= limit, "The range extends beyond the source.");
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    None,
    Clockwise90,
    Clockwise180,
    Clockwise270,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualTransform {
    pub rotation: Rotation,
    /// Horizontal reflection in the rotated picture's coordinates.
    pub mirror: bool,
}

impl VisualTransform {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// A gain envelope in original source time. Splitting an occurrence keeps its original ramp.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFade {
    pub anchor: Span,
    pub fade_in: u64,
    pub fade_out: u64,
}

impl AudioFade {
    pub fn for_range(anchor: Span, fade_in: u64, fade_out: u64) -> Result<Option<Self>> {
        if fade_in == 0 && fade_out == 0 { return Ok(None); }
        let fade = Self { anchor, fade_in, fade_out };
        fade.validate()?;
        Ok(Some(fade))
    }

    fn validate(self) -> Result<()> {
        self.anchor.check(i64::MAX as u64)?;
        ensure!(self.fade_in.checked_add(self.fade_out).is_some_and(|length| length <= self.anchor.len()),
            "The two fades together must fit within the selected source range.");
        Ok(())
    }

    pub fn gain_at(self, tick: u64) -> f64 {
        let entering = if self.fade_in == 0 { 1.0 }
            else { tick.saturating_sub(self.anchor.start).min(self.fade_in) as f64 / self.fade_in as f64 };
        let leaving = if self.fade_out == 0 { 1.0 }
            else { self.anchor.end.saturating_sub(tick).min(self.fade_out) as f64 / self.fade_out as f64 };
        entering.min(leaving)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    pub id: Id,
    pub source: Source,
    pub range: Span,
    pub gain: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_fade: Option<AudioFade>,
    #[serde(default, skip_serializing_if = "VisualTransform::is_identity")]
    pub visual_transform: VisualTransform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SoundRole { Music, Voice, Effect }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sound {
    pub id: Id,
    pub asset: Id,
    pub role: SoundRole,
    pub at: u64,
    pub range: Span,
    pub gain: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_fade: Option<AudioFade>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    format: String,
    version: u32,
    next_id: Id,
    pub name: String,
    pub canvas: Canvas,
    pub assets: Vec<Asset>,
    pub shots: Vec<Shot>,
    pub sounds: Vec<Sound>,
    #[serde(default)]
    pub captions: Vec<Caption>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            format: "clipvault-editor".into(), version: 1, next_id: 1, name: "Untitled".into(),
            canvas: Canvas { width: 1280, height: 720, fps: 30 },
            assets: Vec::new(), shots: Vec::new(), sounds: Vec::new(), captions: Vec::new(),
        }
    }
}

impl AssetInfo {
    fn validate(self) -> Result<()> {
        ensure!(self.duration <= i64::MAX as u64, "The source is too long.");
        if self.kind != AssetKind::Image { ensure!(self.duration > 0, "The source has no playable duration."); }
        if self.kind != AssetKind::Audio { ensure!(self.width > 0 && self.height > 0, "The source has no picture dimensions."); }
        Ok(())
    }
}

impl Project {
    fn allocate_id(&mut self) -> Result<Id> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).context("The project has exhausted its item IDs.")?;
        Ok(id)
    }

    pub fn asset(&self, id: Id) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.id == id)
    }

    pub fn visual_duration(&self) -> u64 {
        self.shots.iter().fold(0u64, |at, shot| at.saturating_add(shot.range.len()))
    }

    pub fn duration(&self) -> u64 {
        self.sounds.iter().map(|sound| sound.at.saturating_add(sound.range.len()))
            .chain(self.captions.iter().map(|cue| cue.range.end)).fold(self.visual_duration(), u64::max)
    }

    pub fn shot_start(&self, id: Id) -> Option<u64> {
        let mut at = 0;
        for shot in &self.shots {
            if shot.id == id { return Some(at); }
            at += shot.range.len();
        }
        None
    }

    pub fn import(&mut self, path: PathBuf, info: AssetInfo) -> Result<Id> {
        info.validate()?;
        ensure!(!path.as_os_str().is_empty(), "Choose a media file.");
        let path = std::path::absolute(path)?;
        if let Some(asset) = self.assets.iter().find(|asset| asset.path == path) {
            return Ok(asset.id);
        }
        let id = self.allocate_id()?;
        self.assets.push(Asset { id, path, info });
        Ok(id)
    }

    pub fn append(&mut self, asset: Id) -> Result<Id> {
        let info = self.asset(asset).context("Choose an imported picture or video.")?.info;
        let duration = match info.kind {
            AssetKind::Image => STILL_DURATION,
            AssetKind::Video => info.duration,
            AssetKind::Audio => bail!("Add sound to an audio lane."),
        };
        self.add_visual(Source::Asset(asset), duration)
    }

    pub fn title(&mut self, text: String, duration: u64) -> Result<Id> {
        ensure!(!text.trim().is_empty(), "Enter the title text.");
        self.add_visual(Source::Title(text), duration)
    }

    pub fn color(&mut self, rgb: [u8; 3], duration: u64) -> Result<Id> {
        self.add_visual(Source::Color(rgb), duration)
    }

    fn add_visual(&mut self, source: Source, duration: u64) -> Result<Id> {
        ensure!(duration > 0 && duration <= (i64::MAX as u64).saturating_sub(self.visual_duration()), "Choose a positive, playable duration.");
        let id = self.allocate_id()?;
        self.shots.push(Shot {
            id,
            source,
            range: Span { start: 0, end: duration },
            gain: 1.0,
            audio_fade: None,
            visual_transform: VisualTransform::default(),
        });
        Ok(id)
    }

    pub fn add_sound(&mut self, asset: Id, role: SoundRole, at: u64) -> Result<Id> {
        let info = self.asset(asset).context("Choose an imported sound.")?.info;
        ensure!(info.kind != AssetKind::Image, "Still pictures have no sound.");
        ensure!(at <= (i64::MAX as u64).saturating_sub(info.duration), "Choose an earlier audio start.");
        let id = self.allocate_id()?;
        self.sounds.push(Sound { id, asset, role, at, range: Span { start: 0, end: info.duration }, gain: 1.0, audio_fade: None });
        Ok(id)
    }

    pub fn split(&mut self, id: Id, tick: u64) -> Result<Id> {
        if let Some(i) = self.shots.iter().position(|item| item.id == id) {
            let start = self.shot_start(id).unwrap();
            let mut right = self.shots[i].clone();
            let boundary = split_point(right.range, start, tick)?;
            right.id = self.allocate_id()?;
            right.range.start = boundary;
            self.shots[i].range.end = boundary;
            let next = right.id;
            self.shots.insert(i + 1, right);
            return Ok(next);
        }
        let i = self.sounds.iter().position(|item| item.id == id).context("Select a timeline item to split.")?;
        let mut right = self.sounds[i].clone();
        let boundary = split_point(right.range, right.at, tick)?;
        right.id = self.allocate_id()?;
        right.at = tick;
        right.range.start = boundary;
        self.sounds[i].range.end = boundary;
        let next = right.id;
        self.sounds.insert(i + 1, right);
        Ok(next)
    }

    pub fn duplicate(&mut self, id: Id) -> Result<Id> {
        if let Some(i) = self.shots.iter().position(|item| item.id == id) {
            let mut copy = self.shots[i].clone();
            copy.id = self.allocate_id()?;
            let next = copy.id;
            self.shots.insert(i + 1, copy);
            return Ok(next);
        }
        let i = self.sounds.iter().position(|item| item.id == id).context("Select a timeline item to duplicate.")?;
        let mut copy = self.sounds[i].clone();
        copy.at = copy.at.checked_add(copy.range.len()).context("Sound is too long.")?;
        copy.id = self.allocate_id()?;
        let next = copy.id;
        self.sounds.insert(i + 1, copy);
        Ok(next)
    }

    pub fn remove(&mut self, id: Id) -> Result<()> {
        if let Some(i) = self.shots.iter().position(|item| item.id == id) {
            self.shots.remove(i);
        } else if let Some(i) = self.sounds.iter().position(|item| item.id == id) {
            self.sounds.remove(i);
        } else if let Some(i) = self.captions.iter().position(|item| item.id == id) {
            self.captions.remove(i);
        } else {
            bail!("Select a timeline item to remove.");
        }
        Ok(())
    }

    pub fn move_shot(&mut self, id: Id, new_index: usize) -> Result<()> {
        let i = self.shots.iter().position(|item| item.id == id).context("Select a picture or video to move.")?;
        ensure!(new_index < self.shots.len(), "Choose a position inside the sequence.");
        let shot = self.shots.remove(i);
        self.shots.insert(new_index, shot);
        Ok(())
    }

    pub fn relink(&mut self, id: Id, path: PathBuf, info: AssetInfo) -> Result<()> {
        info.validate()?;
        ensure!(!path.as_os_str().is_empty(), "Choose a replacement file.");
        let original = self.asset(id).context("Choose a media item to relink.")?;
        ensure!(original.info.kind == info.kind, "Choose the same media type as the original.");
        if info.kind != AssetKind::Image {
            let visual = self.shots.iter().filter(|s| s.source == Source::Asset(id)).map(|s| s.range.end);
            let audio = self.sounds.iter().filter(|s| s.asset == id).map(|s| s.range.end);
            ensure!(visual.chain(audio).max().unwrap_or(0) <= info.duration, "The replacement is shorter than an existing edit.");
        }
        let path = std::path::absolute(path)?;
        let asset = self.assets.iter_mut().find(|a| a.id == id).unwrap();
        asset.path = path;
        asset.info = info;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == "clipvault-editor" && self.version == 1, "Unsupported Editor project format.");
        let Canvas {width,height,fps} = self.canvas;
        ensure!(width >= 16 && height >= 16 && width <= 8192 && height <= 8192 && width % 2 == 0 && height % 2 == 0, "Choose even canvas dimensions from 16 to 8192.");
        ensure!((1..=120).contains(&fps), "Choose a frame rate from 1 to 120.");
        let mut ids = HashSet::new();
        let mut assets = HashMap::new();
        for asset in &self.assets {
            asset.info.validate()?;
            ensure!(!asset.path.as_os_str().is_empty(), "An imported item is missing its file path.");
            assets.insert(asset.id, asset);
            ensure!(ids.insert(asset.id), "A media ID is duplicated.");
        }
        for shot in &self.shots {
            ensure!(ids.insert(shot.id), "A timeline ID is duplicated.");
            let limit = match &shot.source {
                Source::Asset(id) => {
                    let info = assets.get(id).context("A timeline picture has no source in the bin.")?.info;
                    ensure!(info.kind != AssetKind::Audio, "Audio belongs on a sound lane.");
                    if info.kind == AssetKind::Video { info.duration } else { i64::MAX as u64 }
                }
                Source::Title(text) => { ensure!(!text.trim().is_empty(), "Enter the title text."); i64::MAX as u64 }
                Source::Color(_) => i64::MAX as u64,
            };
            ensure!(matches!(shot.source, Source::Asset(_)) || shot.visual_transform.is_identity(),
                "Rotation and mirroring require an imported video or picture.");
            shot.range.check(limit)?;
            check_gain(shot.gain)?;
            if let Some(fade) = shot.audio_fade { fade.validate()?; }
        }
        for sound in &self.sounds {
            ensure!(ids.insert(sound.id), "A timeline ID is duplicated.");
            let info = assets.get(&sound.asset).context("A timeline sound has no source in the bin.")?.info;
            ensure!(info.kind != AssetKind::Image, "A still image has no sound.");
            sound.range.check(info.duration)?;
            check_gain(sound.gain)?;
            if let Some(fade) = sound.audio_fade { fade.validate()?; }
        }
        for cue in &self.captions {
            ensure!(ids.insert(cue.id), "A caption ID is duplicated.");
            ensure!(!cue.text.trim().is_empty(), "Enter the caption text.");
            cue.range.check(i64::MAX as u64)?;
        }
        ensure!(self.next_id > 0 && ids.iter().all(|id| *id > 0 && *id < self.next_id), "Invalid project item IDs.");
        ensure!(self.duration() <= i64::MAX as u64, "The project exceeds the supported time range.");
        Ok(())
    }

    /// Rebase picture and audio trims to a bounded preview window. Shared source files stay intact.
    pub fn slice(&self, window: Span) -> Result<Self> {
        self.validate()?;
        window.check(self.duration())?;
        let mut result = self.clone();
        result.shots.clear();
        result.sounds.clear();
        result.captions.clear();
        let mut at = 0;
        for shot in &self.shots {
            let timeline = Span { start: at, end: at + shot.range.len() };
            at = timeline.end;
            if let Some(part) = timeline.intersection(window) {
                let mut out = shot.clone();
                out.range.start += part.start - timeline.start;
                out.range.end = out.range.start + part.len();
                result.shots.push(out);
            }
        }
        for sound in &self.sounds {
            let timeline = Span { start: sound.at, end: sound.at + sound.range.len() };
            if let Some(part) = timeline.intersection(window) {
                let mut out = sound.clone();
                out.at = part.start - window.start;
                out.range.start += part.start - timeline.start;
                out.range.end = out.range.start + part.len();
                result.sounds.push(out);
            }
        }
        for cue in &self.captions {
            if let Some(part) = cue.range.intersection(window) {
                let mut out = cue.clone();
                out.range = Span { start: part.start - window.start, end: part.end - window.start };
                result.captions.push(out);
            }
        }
        let tail = window.len() - result.visual_duration();
        if tail > 0 { result.color([0; 3], tail)?; }
        Ok(result)
    }
}

fn split_point(range: Span, at: u64, tick: u64) -> Result<u64> {
    ensure!(tick > at && tick - at < range.len(), "Place the playhead inside the selected item to split it.");
    Ok(range.start + (tick - at))
}

fn check_gain(gain: f64) -> Result<()> {
    ensure!((0.0..=2.0).contains(&gain), "Choose a volume between 0% and 200%.");
    Ok(())
}

pub struct History {
    pub project: Project,
    undo: std::collections::VecDeque<Project>,
    redo: Vec<Project>,
}

impl History {
    pub fn new(project: Project) -> Self {
        Self { project, undo: Default::default(), redo: Vec::new() }
    }

    pub fn edit<T>(&mut self, change: impl FnOnce(&mut Project) -> Result<T>) -> Result<T> {
        let mut draft = self.project.clone();
        let value = change(&mut draft)?;
        draft.validate()?;
        if draft != self.project {
            if self.undo.len() == 64 { self.undo.pop_front(); }
            self.undo.push_back(std::mem::replace(&mut self.project, draft));
            self.redo.clear();
        }
        Ok(value)
    }

    pub fn can_undo(&self) -> bool { !self.undo.is_empty() }
    pub fn can_redo(&self) -> bool { !self.redo.is_empty() }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else { return false };
        self.redo.push(std::mem::replace(&mut self.project, previous));
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push_back(std::mem::replace(&mut self.project, next));
        true
    }
}
