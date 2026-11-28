//! The 24/7 view: a zoomable wall-clock timeline across all recording files
//! with an activity lane (what was focused), markers and clips, audio lanes
//! with non-destructive region edits, live playback and clip-from-range.

use super::editor_layout::{self, EditorLayout};
use crate::app::Ctx;
use crate::lanes::{self, LaneAction, LaneState, PreviewProgress, RangePreview, WaveSource};
use crate::player::shortcuts::{hint, Action};
use crate::theme::{self, icon};
use chrono::{Local, TimeZone};
use cv_core::ipc::{ExportSource, Request};
use cv_core::mixer::MixSettings;
use cv_core::model::{apply_edit, rebase_edits, ActivitySpan, AudioEdit, Category, Clip, Game, Marker, Recording};
use cv_core::timeline::{coverage, locate, plan_cut, plan_duration_s, Segment};
use cv_core::util::{format_duration, format_duration_precise, human_bytes, now_ms};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static GO_LIVE: AtomicBool = AtomicBool::new(false);
static PLAY_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn request_go_live() {
    GO_LIVE.store(true, Ordering::SeqCst);
}

/// Ask the 24/7 view to show and play a wall-clock moment (Unix ms).
pub fn request_play_at(ms: i64) {
    PLAY_AT.store(ms, Ordering::SeqCst);
}

/// A stretch of playing one game (activity spans merged across short breaks).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub game_id: Option<i64>,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Group game activity into sessions: same game, breaks shorter than 10 min.
pub fn game_sessions(spans: &[ActivitySpan]) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    let mut games: Vec<&ActivitySpan> = spans.iter().filter(|s| s.is_game).collect();
    games.sort_by_key(|s| s.start_ms);
    for s in games {
        if let Some(last) = out.iter_mut().rev().find(|x| x.game_id == s.game_id && x.name == s.name) {
            if s.start_ms - last.end_ms < 10 * MIN {
                last.end_ms = last.end_ms.max(s.end_ms);
                continue;
            }
        }
        out.push(Session { name: s.name.clone(), game_id: s.game_id, start_ms: s.start_ms, end_ms: s.end_ms });
    }
    out.retain(|x| x.end_ms - x.start_ms >= MIN);
    out.sort_by_key(|x| std::cmp::Reverse(x.start_ms));
    out
}

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

struct CutDialog {
    title: String,
    game_id: Option<i64>,
    cats: Vec<i64>,
    game_search: String,
    target_size_mb: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    TrimStart,
    TrimEnd,
    Scrub,
    Pan,
}

#[derive(Default)]
pub struct RecordingsView {
    recs: Vec<Recording>,
    spans: Vec<ActivitySpan>,
    markers: Vec<Marker>,
    clips: Vec<Clip>,
    games: Vec<Game>,
    cats: Vec<Category>,
    dirty: bool,
    loaded: Option<Instant>,
    view_end: i64,
    view_span: i64,
    follow_live: bool,
    live_playback: Option<(String, i64)>,
    cur: Option<usize>,
    cur_path: Option<String>,
    in_ms: Option<i64>,
    out_ms: Option<i64>,
    dialog: Option<CutDialog>,
    init: bool,
    rename_marker: Option<(i64, String)>,
    drag_mode: Option<DragMode>,
    scrub_resume: bool,
    last_scrub: Option<Instant>,
    /// Non-destructive edits on the 24/7 timeline (wall-clock seconds).
    edits: Vec<AudioEdit>,
    edits_dirty: Option<Instant>,
    edit_history: lanes::EditHistory,
    mix: Option<MixSettings>,
    mix_dirty: Option<Instant>,
    preview: Option<RangePreview>,
    hold_at_eof: bool,
    written_live: Option<(String, i64)>,
    session_search: String,
    footage_search: String,
    marker_search: String,
    share_mb: u32,
    waveform_media: std::collections::HashMap<String, (Instant, Option<cv_media::probe::MediaInfo>)>,
    lane_state: LaneState,
    /// Playhead when the player was last showing our file.
    resume_ms: Option<i64>,
    compact_files: bool,
    preview_size: editor_layout::PreviewSize,
}

pub(super) fn timeline_help(ui: &mut Ui) -> egui::Response {
    ui.add(
        egui::Label::new(theme::label_dim(
            "Click to seek. Drag the trim handles to adjust the range.",
        ))
        .wrap(),
    )
}

fn timeline_mix_file() -> PathBuf {
    cv_core::paths::data_dir().join("timeline_mix.json")
}

fn edits_file() -> PathBuf {
    cv_core::paths::data_dir().join("timeline_edits.json")
}

impl RecordingsView {
    pub(crate) fn qa_shortcut_query(&self) -> &str {
        &self.footage_search
    }

    pub(crate) fn qa_mark_trim(&mut self) -> bool {
        if self.selection().is_some() {
            return true;
        }
        let Some((start, duration)) = self.recs.iter().rev().find_map(|r| {
            let duration = self.seg_end(r).saturating_sub(r.start_ms);
            (r.probed && !r.is_live() && duration >= 500).then_some((r.start_ms, duration))
        }) else { return false };
        self.in_ms = Some(start + duration / 4);
        self.out_ms = Some(start + duration / 2);
        self.frame_recording(start + duration / 3);
        self.selection().is_some()
    }

    pub(crate) fn qa_live_ready(&self) -> bool {
        self.playable_live().is_some()
    }

    pub(crate) fn qa_trim_marks(&self) -> Option<(i64, i64)> {
        self.selection()
    }

    pub(crate) fn show_compact_files(&mut self) {
        self.compact_files = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn refresh(&mut self, c: &mut Ctx) {
        let written_live = c.service.status().and_then(|s| {
            (s.recording.active && s.recording.written_until_ms > 0)
                .then_some((s.recording.path, s.recording.written_until_ms))
        });
        self.dirty |= self.written_live.as_ref().map(|(path, _)| path)
            != written_live.as_ref().map(|(path, _)| path);
        self.written_live = written_live;
        let stale = self.loaded.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(true);
        if !(self.dirty || stale) {
            return;
        }
        if let Some(db) = c.db() {
            self.recs = db.list_recordings().unwrap_or_default();
            self.recs.retain(|r| r.is_live() || (r.probed && r.duration > 0.0));
            let (from, to) = self.extent();
            self.spans = merge_spans(db.activity_between(from - HOUR, to + HOUR).unwrap_or_default());
            self.markers = db.markers_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.clips = db.clips_between(from - HOUR, to + HOUR).unwrap_or_default();
            self.games = db.list_games().unwrap_or_default();
            self.cats = db.list_categories().unwrap_or_default();
        }
        if let Some(p) = &self.cur_path {
            self.cur = self.recs.iter().position(|r| &r.path == p);
        }
        self.loaded = Some(Instant::now());
        self.dirty = false;
    }

    fn seg_end(&self, r: &Recording) -> i64 {
        if r.is_live() {
            if let Some((_, end)) = self.written_live.as_ref().filter(|(path, _)| path == &r.path) {
                return (*end).max(r.start_ms);
            }
            return r.start_ms;
        }
        r.start_ms + (r.duration.max(0.0) * 1000.0) as i64
    }

    fn extent(&self) -> (i64, i64) {
        let start = self.recs.iter().map(|r| r.start_ms).min().unwrap_or(now_ms() - 6 * HOUR);
        let end = self.recs.iter().map(|r| self.seg_end(r)).max().unwrap_or(now_ms()).max(now_ms());
        (start, end)
    }

    fn segments(&self) -> Vec<Segment> {
        self.recs
            .iter()
            .map(|r| Segment { path: PathBuf::from(&r.path), start_ms: r.start_ms, duration_ms: self.seg_end(r) - r.start_ms, live: r.is_live() })
            .collect()
    }

    fn next_start(&self, current: usize) -> Option<i64> {
        let current = self.recs.get(current)?;
        let end = self.seg_end(current);
        // Database order is chronological. Contained copies cannot extend playback.
        self.recs.iter().skip_while(|r| r.start_ms <= current.start_ms)
            .find(|r| self.seg_end(r) > end && self.seg_end(r) > r.start_ms)
            .map(|r| r.start_ms)
    }

    fn playhead(&self, c: &Ctx) -> Option<i64> {
        let i = self.cur?;
        let r = self.recs.get(i)?;
        let state = c.player.state();
        (state.loaded && c.player.path.as_deref() == Some(r.path.as_str()))
            .then(|| r.start_ms + (state.time_pos * 1000.0) as i64)
    }

    fn tracks_for(&self, c: &Ctx, r: &Recording) -> usize {
        if r.audio_tracks > 0 {
            r.audio_tracks as usize
        } else if r.is_live() {
            c.service.status().map(|s| s.recording.tracks as usize).unwrap_or(0)
        } else {
            0
        }
    }

    /// Edits for the loaded segment, in that file's time.
    fn segment_edits(&self) -> Vec<AudioEdit> {
        let Some(r) = self.cur.and_then(|i| self.recs.get(i)) else { return Vec::new() };
        let s0 = r.start_ms as f64 / 1000.0;
        let s1 = if r.is_live() { f64::INFINITY } else { self.seg_end(r) as f64 / 1000.0 };
        rebase_edits(&self.edits, s0, s1)
    }

    /// Start playing at wall-clock time `t`.
    fn play_at(&mut self, c: &mut Ctx, t: i64, autoplay: bool) {
        self.live_playback = None;
        let segs = self.segments();
        let Some(loc) = locate(&segs, t) else {
            c.toast("info", "Nothing was recorded at that time");
            return;
        };
        let rec = self.recs[loc.index].clone();
        if self.seg_end(&rec) <= rec.start_ms {
            c.toast("info", "The recorder has not written playable footage yet");
            return;
        }
        self.hold_at_eof = false;
        let offset = loc.offset_ms as f64 / 1000.0;
        let same = self.cur == Some(loc.index) && c.player.path.as_deref() == Some(rec.path.as_str()) && c.player.state().loaded;
        let loaded_duration = c.player.state().duration;
        self.cur = Some(loc.index);
        self.cur_path = Some(rec.path.clone());
        if same && !(rec.is_live() && offset > loaded_duration - 1.0) {
            c.player.seek(offset, true);
            c.player.set_paused(!autoplay);
        } else {
            let tracks = self.tracks_for(c, &rec);
            c.player.open(&rec.path, offset, tracks, &[], self.mix.as_ref(), rec.is_live(), autoplay, &c.cfg.player);
            c.player.set_edits(self.segment_edits());
            c.player.set_loop(false);
        }
    }

    /// Fast seek while dragging the playhead.
    fn scrub_to(&mut self, c: &mut Ctx, t: i64) {
        let segs = self.segments();
        let Some(loc) = locate(&segs, t) else { return };
        let throttle = self.last_scrub.map(|i| i.elapsed() < Duration::from_millis(45)).unwrap_or(false);
        if self.cur == Some(loc.index) && c.player.state().loaded {
            if !throttle {
                c.player.seek(loc.offset_ms as f64 / 1000.0, false);
                self.last_scrub = Some(Instant::now());
            }
        } else if !throttle {
            self.play_at(c, t, false);
            self.last_scrub = Some(Instant::now());
        }
    }

    fn playable_bounds(&self) -> Option<(i64, i64)> {
        let playable = self.recs.iter().filter(|r| self.seg_end(r) > r.start_ms);
        let first = playable.clone().map(|r| r.start_ms).min()?;
        let last = playable.map(|r| self.seg_end(r)).max()?;
        Some((first, last))
    }

    fn move_trim_handle(&mut self, handle: DragMode, at: i64) {
        let Some((first, last)) = self.playable_bounds() else { return };
        match handle {
            DragMode::TrimStart => {
                let max = self.out_ms.map_or(last, |out| out.saturating_sub(101)).clamp(first, last);
                let _ = self.set_start(at.clamp(first, max));
            }
            DragMode::TrimEnd => {
                let min = self.in_ms.map_or(first, |start| start.saturating_add(101)).clamp(first, last);
                let _ = self.set_end(at.clamp(min, last));
            }
            _ => {}
        }
    }

    fn latest_playable(&self) -> Option<(i64, bool)> {
        self.recs.iter()
            .filter_map(|r| {
                let end = self.seg_end(r);
                (end > r.start_ms).then_some((end - 1, r.is_live()))
            })
            .max()
    }

    fn jump_latest(&mut self, c: &mut Ctx) {
        self.dirty = true;
        self.refresh(c);
        let Some((at, live)) = self.latest_playable() else {
            c.toast("info", "No playable footage is available yet");
            return;
        };
        self.follow_live = live;
        self.view_end = at + self.view_span / 20;
        self.play_at(c, at, live);
        self.hold_at_eof = !live;
        if live {
            self.live_playback = self.written_live.clone();
        }
    }

    fn adjacent_frame_time(&self, current: usize, forward: bool) -> Option<i64> {
        let segments = self.segments();
        let ordered = coverage(&segments);
        let position = ordered.iter().position(|(i, _)| *i == current)?;
        if forward {
            let (next, _) = ordered.get(position + 1)?;
            Some(segments[*next].start_ms)
        } else {
            let (_, end) = ordered.get(position.checked_sub(1)?)?;
            Some(end - 1)
        }
    }

    fn frame_step_handoff(
        &self,
        current: usize,
        state: &crate::player::PlayState,
        forward: bool,
    ) -> Option<i64> {
        let adjacent = self.adjacent_frame_time(current, forward)?;
        if forward {
            let start_ms = self.recs.get(current)?.start_ms;
            let crosses_join = state.next_frame_pos.is_some_and(|next| {
                next * 1_000.0 >= adjacent.saturating_sub(start_ms) as f64
            });
            (state.last_frame || crosses_join).then_some(adjacent)
        } else {
            state.first_frame.then_some(adjacent)
        }
    }

    fn step_recording_frame(&mut self, c: &mut Ctx, forward: bool) {
        self.stop_preview(c);
        self.live_playback = None;
        let state = c.player.state();
        let adjacent = self.cur.filter(|_| state.loaded)
            .and_then(|current| self.frame_step_handoff(current, &state, forward));
        if let Some(at) = adjacent {
            self.follow_live = false;
            self.play_at(c, at, false);
            self.hold_at_eof = true;
        } else {
            c.player.frame_step(forward);
        }
    }

    fn playable_live(&self) -> Option<&Recording> {
        self.recs.iter().rev().find(|r| r.is_live() && self.seg_end(r) > r.start_ms)
    }

    fn go_live(&mut self, c: &mut Ctx) {
        self.dirty = true;
        self.refresh(c);
        let Some(rec) = self.playable_live() else {
            c.toast("info", "No live footage is ready");
            return;
        };
        let end = self.seg_end(rec);
        let path = rec.path.clone();
        let offset = i64::from(c.cfg.player.live_offset_secs) * 1000;
        let t = (now_ms() - offset).min(end - 1).max(rec.start_ms);
        self.follow_live = true;
        self.view_end = end + self.view_span / 20;
        self.play_at(c, t, true);
        self.live_playback = Some((path, end));
    }

    fn next_live_segment(&self, paused: bool, reached_edge: bool) -> Option<Recording> {
        let (old, _) = self.live_playback.as_ref()?;
        if paused || !reached_edge {
            return None;
        }
        self.playable_live().filter(|r| &r.path != old).cloned()
    }

    fn follow_live_playback(&mut self, c: &mut Ctx, state: &crate::player::PlayState) -> bool {
        let Some((old, end)) = &mut self.live_playback else { return false };
        if self.cur_path.as_ref() != Some(old) {
            self.live_playback = None;
            return false;
        }
        if let Some((path, next_end)) = &self.written_live {
            if path == old {
                *end = (*end).max(*next_end);
                return false;
            }
        }
        let edge = *end;
        let at = self.playhead(c).unwrap_or(i64::MIN);
        let reached_edge = state.last_frame && at >= edge.saturating_sub(1_000);
        let Some(next) = self.next_live_segment(state.paused || !state.loaded, reached_edge) else { return false };
        let end = self.seg_end(&next);
        self.play_at(c, next.start_ms, true);
        self.live_playback = Some((next.path, end));
        true
    }

    fn load_edits(&mut self) {
        self.mix = std::fs::read(timeline_mix_file()).ok()
            .and_then(|data| serde_json::from_slice(&data).ok());
        if let Ok(text) = std::fs::read_to_string(edits_file()) {
            self.edits = serde_json::from_str(&text).unwrap_or_default();
        }
    }

    fn save_edits_if_due(&mut self) {
        let due = [self.edits_dirty, self.mix_dirty].into_iter().flatten()
            .any(|changed| changed.elapsed() >= Duration::from_millis(800));
        if due {
            // Forget edits for footage that no longer exists.
            if let Some(oldest) = self.recs.iter().map(|r| r.start_ms).min() {
                let cutoff = oldest as f64 / 1000.0;
                self.edits.retain(|e| e.end > cutoff);
            }
            self.flush_edits();
        }
    }

    pub fn flush_edits(&mut self) {
        if self.edits_dirty.is_some() {
            if let Ok(data) = serde_json::to_vec(&self.edits) {
                if cv_core::util::write_atomic(&edits_file(), &data).is_ok() {
                    self.edits_dirty = None;
                }
            }
        }
        if let Some(mix) = self.mix.as_ref().filter(|_| self.mix_dirty.is_some()) {
            if let Ok(data) = serde_json::to_vec(mix) {
                if cv_core::util::write_atomic(&timeline_mix_file(), &data).is_ok() {
                    self.mix_dirty = None;
                }
            }
        }
    }

    pub fn leave(&mut self, c: &mut Ctx) {
        self.stop_preview(c);
        self.flush_edits();
    }

    fn selection(&self) -> Option<(i64, i64)> {
        let (a, b) = (self.in_ms?, self.out_ms?);
        let (from, to) = (a.min(b), a.max(b));
        let start = self.recs.iter()
            .filter(|r| self.seg_end(r) > from && r.start_ms < to)
            .map(|r| r.start_ms.max(from)).min()?;
        let end = self.recs.iter()
            .filter(|r| self.seg_end(r) > from && r.start_ms < to)
            .map(|r| self.seg_end(r).min(to)).max()?;
        let segments = self.segments();
        let parts = plan_cut(&segments, start, end);
        (plan_duration_s(&parts, &segments) > 0.1).then_some((start, end))
    }

    fn stop_preview(&mut self, c: &mut Ctx) {
        if self.preview.take().is_some() {
            self.hold_at_eof = true;
            c.player.set_paused(true);
        }
    }

    fn set_start(&mut self, at: i64) -> Result<(), &'static str> {
        if self.out_ms.is_some_and(|end| end.saturating_sub(at) <= 100) {
            return Err("Choose a start more than 0.1 seconds before the end.");
        }
        self.in_ms = Some(at);
        Ok(())
    }

    fn set_end(&mut self, at: i64) -> Result<(), &'static str> {
        if self.in_ms.is_some_and(|start| at.saturating_sub(start) <= 100) {
            return Err("Choose an end more than 0.1 seconds after the start.");
        }
        self.out_ms = Some(at);
        Ok(())
    }

    fn frame_recording(&mut self, at:i64) {
        if let Some(r)=self.recs.iter().find(|r|r.start_ms<=at && self.seg_end(r)>at) {
            let duration=self.seg_end(r)-r.start_ms;
            self.view_span=(duration+duration/8).clamp(MIN,6*HOUR);
            self.view_end=at+self.view_span/2;
            self.follow_live=false;
        }
    }

    fn export_selection(&mut self, c: &mut Ctx, target_size_mb: Option<u32>, title: String) {
        let Some((from_ms, to_ms)) = self.selection() else { return };
        let edits = self.edits.iter()
            .filter(|edit| edit.overlaps(from_ms as f64 / 1000.0, to_ms as f64 / 1000.0))
            .cloned().collect();
        if c.request(Request::Export {
            source: ExportSource::Recording { from_ms, to_ms },
            range: None,
            mix: self.mix.clone().unwrap_or_else(|| c.player.mix.clone()),
            edits: Some(edits),
            target_size_mb,
            title,
        }).is_some() {
            c.toast("info", "Exporting selection…");
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, c: &mut Ctx) {
        if !self.init {
            self.init = true;
            self.view_span = c.cfg.player.timeline_hours as i64 * HOUR;
            self.view_end = now_ms() + self.view_span / 20;
            self.follow_live = true;
            self.dirty = true;
            self.load_edits();
            self.share_mb = c.cfg.export.target_size_mb;
            if c.player.track_count == 0 {
                c.player.show_mixer = true;
            }
        }
        self.refresh(c);
        self.save_edits_if_due();
        if self.edits_dirty.is_some() || self.mix_dirty.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
        // The player is shared with the clip view: take it back if needed.
        if let Some(p) = self.cur_path.clone() {
            if c.player.path.as_deref() == Some(p.as_str()) {
                self.resume_ms = self.playhead(c);
            } else if let Some(t) = self.resume_ms.take() {
                self.play_at(c, t, false);
            } else {
                self.cur = None;
                self.cur_path = None;
            }
        }
        if GO_LIVE.swap(false, Ordering::SeqCst) {
            self.compact_files = false;
            self.stop_preview(c);
            self.go_live(c);
        }
        let jump = PLAY_AT.swap(0, Ordering::SeqCst);
        if jump != 0 {
            self.compact_files = false;
            self.stop_preview(c);
            self.refresh(c);
            self.follow_live = false;
            self.frame_recording(jump);
            self.play_at(c, jump, true);
        }
        if self.cur.is_none() && jump==0 {
            if let Some(start)=self.recs.iter().rev().find(|r|self.seg_end(r)>r.start_ms).map(|r|r.start_ms){
                self.frame_recording(start);
                self.play_at(c,start,false);
            }
        }
        let st = c.player.state();
        let followed_rotation = self.follow_live_playback(c, &st);
        let playhead = self.playhead(c).map(|at| at as f64 / 1000.0);
        let next_start = self.cur.and_then(|i| self.next_start(i));
        let crossed_overlap = next_start.filter(|&next| !st.paused && !st.eof && self.drag_mode.is_none()
            && self.playhead(c).is_some_and(|at| at >= next)
            && self.preview.as_ref().is_none_or(|p| (next as f64 / 1000.0) < p.end));
        if followed_rotation || self.live_playback.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else if let Some(next) = crossed_overlap {
            self.play_at(c, self.playhead(c).unwrap_or(next), true);
        } else if let Some(preview) = &mut self.preview {
            match preview.progress(playhead, st.paused, st.eof) {
                PreviewProgress::Playing => ui.ctx().request_repaint_after(Duration::from_millis(30)),
                PreviewProgress::Finished => self.stop_preview(c),
                PreviewProgress::EndOfFile => {
                    let end = preview.end;
                    let next = next_start.filter(|next| (*next as f64 / 1000.0) < end);
                    if let Some(next) = next {
                        self.play_at(c, next, true);
                        self.preview = Some(RangePreview::new(next as f64 / 1000.0, end));
                    } else {
                        self.stop_preview(c);
                    }
                }
            }
        } else if self.cur.is_some() {
            if !st.eof && !st.paused {
                self.hold_at_eof = false;
            }
            if st.eof && st.loaded && !self.hold_at_eof && c.player.path.is_some() && self.drag_mode.is_none() {
                if let Some(next) = next_start { self.play_at(c, next, true); }
            }
        }
        // Keep the player's edits in sync with the timeline's.
        if self.cur.is_some() {
            let e = self.segment_edits();
            c.player.set_edits(e);
        }
        if self.follow_live {
            let target = self.latest_playable().map_or(self.view_end, |(at, _)| at + self.view_span / 20);
            if (target - self.view_end).abs() > 500 {
                self.view_end = target;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }

        let hits = c.player.shortcuts(ui, &c.cfg.player);
        if let Some(forward) = hits.frame_step {
            self.step_recording_frame(c, forward);
        }
        if hits.jump_latest {
            self.stop_preview(c);
            self.jump_latest(c);
        }
        if hits.mark_in {
            self.stop_preview(c);
            if let Some(at) = self.playhead(c) { c.report_mark(self.set_start(at)); }
        }
        if hits.mark_out {
            self.stop_preview(c);
            if let Some(at) = self.playhead(c) { c.report_mark(self.set_end(at)); }
        }

        let fullscreen = c.player.fullscreen;
        let watching = self.preview_size.watching() && !fullscreen;
        let compact = watching || ui.available_width() < editor_layout::TWO_COLUMN_WIDTH;
        if !fullscreen {
            if compact {
                ui.horizontal_wrapped(|ui| {
                    ui.label(theme::heading("24/7 Recordings"));
                    ui.selectable_value(&mut self.compact_files, false, "Timeline");
                    ui.selectable_value(&mut self.compact_files, true, "Files & jobs");
                });
                ui.add_space(6.0);
                if self.compact_files {
                    self.files_inspector(ui, c);
                    return;
                }
                ui.horizontal_wrapped(|ui| self.header_controls(ui, c, false));
            } else {
                self.header(ui, c);
            }
            ui.add_space(10.0);
        }

        if self.recs.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::icon_text(icon::HISTORY, 40.0).color(theme::TEXT_FAINT));
                ui.label(theme::title("No recordings yet"));
                let replay_only = c.cfg.recording.rolling_seconds().is_some();
                if replay_only {
                    ui.label(theme::label_dim("Replay only is on. Save a replay from Home, then open it in Clips."));
                    if ui.button("Go to Home").clicked() {
                        c.goto = Some(crate::app::Page::Home);
                    }
                } else if !c.service.connected() {
                    ui.label(theme::label_dim("Waiting for ClipVault Service to connect. You can choose your recording settings now."));
                } else if c.service.status().is_some_and(|s| s.recording.active) {
                    ui.label(theme::label_dim("Recording has started. The first playable footage will appear here shortly."));
                } else {
                    ui.label(theme::label_dim("Turn on full-history recording to start a timeline."));
                }
                if ui.button("Open recording settings").clicked() {
                    c.open_recording_settings = true;
                }
            });
            return;
        }

        let side_width = (!compact && !fullscreen).then(|| (ui.available_width() * 0.30).clamp(245.0, 330.0));
        let show_lanes = c.player.show_mixer && !fullscreen;
        let controls_height = if show_lanes { 398.0 } else { 102.0 };
        let layout = EditorLayout::with_preview(
            ui, side_width, controls_height, !fullscreen, (!fullscreen).then_some(&self.preview_size),
        );
        if self.preview_size.handle(ui, "recording_preview_handle", &layout) {
            self.compact_files = false;
        }
        if let Some(mut inspector) = layout.inspector {
            inspector.min.x += 18.0;
            editor_layout::region(ui, "rec_side", inspector, |ui| self.files_inspector(ui, c));
        }
        editor_layout::region(ui, "rec_transport", layout.transport, |ui| {
            editor_layout::controls(ui, "rec_transport_scroll", |ui| {
                let label = self.time_label(c);
                if let (_, Some(forward)) = c.player.transport(ui, &label, &c.cfg.player) {
                    self.step_recording_frame(c, forward);
                }
                if !fullscreen {
                    self.cut_bar(ui, c);
                }
            });
        });
        editor_layout::region(ui, "rec_controls", layout.timeline, |ui| {
            if watching {
                self.timeline(ui, c, true);
                return;
            }
            editor_layout::controls(ui, "rec_controls_scroll", |ui| {
                ui.add_space(6.0);
                self.timeline(ui, c, false);
                if show_lanes {
                    ui.add_space(4.0);
                    self.audio_lanes(ui, c, layout.timeline.height() - 106.0);
                }
            });
        });
        editor_layout::region(ui, "rec_video", layout.video, |ui| {
            let available = layout.video.size();
            let h = if fullscreen { available.y } else { (available.x * 9.0 / 16.0).min(available.y) };
            c.player.video(ui, Vec2::new(available.x, h));
        });
        self.cut_dialog(ui, c);
    }

    fn files_inspector(&mut self, ui: &mut Ui, c: &mut Ctx) {
        egui::ScrollArea::vertical().id_salt("rec_side_scroll").auto_shrink(false).show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            self.side(ui, c);
        });
    }

    fn header(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let total = self.recs.iter().filter(|r| !r.kept).fold(0.0, |seconds, r| {
            seconds + (self.seg_end(r) - r.start_ms) as f64 / 1000.0
        });
        let stored = cv_core::util::format_span_short(total as i64);
        let history = if c.cfg.recording.rolling_seconds().is_some() {
            format!("{stored} archived · Replay only is on")
        } else {
            format!("{stored} stored · keeps {}", c.cfg.recording.history_label())
        };
        let compact = ui.available_width() < 920.0;
        if compact {
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::heading("24/7 Recordings"));
                ui.label(theme::label_dim(&history));
            });
            ui.horizontal_wrapped(|ui| self.header_controls(ui, c, false));
        } else {
            ui.horizontal(|ui| {
                ui.label(theme::heading("24/7 Recordings"));
                ui.label(theme::label_dim(history));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.header_controls(ui, c, true));
            });
        }
    }

    fn header_controls(&mut self, ui: &mut Ui, c: &mut Ctx, right_aligned: bool) {
        let latest = self.latest_playable();
        let jump = ui.add_enabled(latest.is_some(), egui::Button::new("Jump to latest"))
            .on_hover_text(hint(&c.cfg.player, Action::JumpLatest, "Jump to the latest playable footage and follow when recording"))
            .on_disabled_hover_text("No playable footage is available yet");
        editor_layout::remember_interaction_rect(ui, "jump_latest", jump.rect);
        if jump.clicked() {
            self.stop_preview(c);
            self.jump_latest(c);
        }
        let live = self.playable_live().is_some();
        let live_button = ui.add_enabled_ui(live, |ui| theme::accent_button(ui, Some(icon::LIVE), "Go live", theme::LIVE))
            .inner
            .on_hover_text("Jump to the current recording")
            .on_disabled_hover_text("Available while recording full history.");
        editor_layout::remember_interaction_rect(ui, "go_live", live_button.rect);
        if live_button.clicked() {
            self.stop_preview(c);
            self.go_live(c);
        }
        ui.add_space(6.0);
        let ranges = [("10m", 10 * MIN), ("1h", HOUR), ("6h", 6 * HOUR), ("24h", 24 * HOUR), ("3d", 72 * HOUR), ("All", 0)];
        for i in 0..ranges.len() {
            let (label, span) = ranges[if right_aligned { ranges.len() - i - 1 } else { i }];
            let span = if span == 0 {
                let (a, b) = self.extent();
                (b - a).max(HOUR) + HOUR / 2
            } else {
                span
            };
            let selected = (self.view_span - span).abs() < MIN;
            if ui.add(egui::Button::selectable(selected, label)).clicked() {
                let center = self.playhead(c).unwrap_or(self.view_end - self.view_span / 2);
                self.view_span = span;
                if label == "All" {
                    self.view_end = self.extent().1 + span / 40;
                    self.follow_live = false;
                } else if self.follow_live {
                    self.view_end = now_ms() + span / 20;
                } else {
                    self.view_end = center + span / 2;
                }
            }
        }
    }

    fn time_label(&self, c: &Ctx) -> String {
        match self.playhead(c) {
            Some(t) => {
                let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
                let behind = (now_ms() - t) / 1000;
                let live = self.cur.and_then(|i| self.recs.get(i)).map(|r| r.is_live()).unwrap_or(false);
                if live && behind < 30 {
                    format!("{}  ·  LIVE", dt.format("%a %d %b  %H:%M:%S"))
                } else {
                    format!("{}  ·  {} ago", dt.format("%a %d %b  %H:%M:%S"), cv_core::util::format_span_short(behind))
                }
            }
            None => "Click the timeline or Go live".into(),
        }
    }

    fn timeline(&mut self, ui: &mut Ui, c: &mut Ctx, watching: bool) {
        let w = ui.available_width().max(1.0);
        let height = if watching { ui.available_height().clamp(1.0, 40.0) } else { 96.0 };
        let (row, _) = ui.allocate_exact_size(Vec2::new(w, height), Sense::hover());
        let compact = watching.then(|| editor_layout::WatchingRuler::new(row));
        let rect = compact.as_ref().map_or_else(|| lanes::axis_rect(row), |ruler| ruler.axis);
        let span = self.view_span.max(MIN);
        let start = self.view_end - span;
        let px_per_ms = rect.width() as f64 / span as f64;
        let x_of = |t: i64| rect.left() + ((t - start) as f64 * px_per_ms) as f32;
        let t_of = |x: f32| start + ((x - rect.left()) as f64 / px_per_ms) as i64;

        let ruler = Rect::from_x_y_ranges(rect.x_range(), row.top()..=row.top() + 20.0);
        let seg_band = Rect::from_x_y_ranges(rect.x_range(), row.top() + 24.0..=row.top() + 54.0);
        let act_band = Rect::from_x_y_ranges(rect.x_range(), row.top() + 60.0..=row.top() + 80.0);
        if let Some(compact) = &compact {
            let label = |at| Local.timestamp_millis_opt(at).single().unwrap_or_default().format("%H:%M:%S").to_string();
            let marks = [self.in_ms.map(x_of), self.out_ms.map(x_of)];
            let recorded = self.recs.iter().filter_map(|recording| {
                let (a, b) = (recording.start_ms, self.seg_end(recording));
                (b > a && b > start && a < self.view_end).then(|| (x_of(a), x_of(b)))
            });
            compact.paint(ui, row, [&label(start), &label(self.view_end)], marks,
                self.selection().map(|(a, b)| (x_of(a), x_of(b))),
                self.playhead(c).map(x_of), c.accent, recorded);
            editor_layout::remember_interaction_rect(ui, "watching_ruler", row);
            for (name, mark) in ["trim_start", "trim_end"].into_iter().zip(marks) {
                if let Some(grip) = mark.and_then(|x| compact.grip(x)) {
                    editor_layout::remember_interaction_rect(ui, name, Rect::from_center_size(grip, Vec2::splat(10.0)));
                }
            }
        } else {
            // Gutter labels.
            let gutter = Rect::from_x_y_ranges(row.left()..=rect.left(), row.y_range());
            let gp = ui.painter_at(gutter);
            let gx = row.left() + 10.0;
            gp.text(Pos2::new(gx, seg_band.center().y), Align2::LEFT_CENTER, "Recording", FontId::new(12.0, theme::semibold()), theme::TEXT_DIM);
            gp.text(Pos2::new(gx, act_band.center().y), Align2::LEFT_CENTER, "Activity", FontId::new(12.0, theme::semibold()), theme::TEXT_DIM);
            gp.text(Pos2::new(gx, act_band.bottom() + 9.0), Align2::LEFT_CENTER, "what was on screen", FontId::proportional(10.5), theme::TEXT_FAINT);

            let p = ui.painter_at(rect);
            p.rect(Rect::from_x_y_ranges(rect.x_range(), row.y_range()), 5.0, theme::SIDEBAR, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);

            // Ticks.
            let steps = [MIN, 2 * MIN, 5 * MIN, 10 * MIN, 15 * MIN, 30 * MIN, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, 24 * HOUR];
            let step = *steps.iter().find(|s| **s as f64 * px_per_ms >= 90.0).unwrap_or(&(24 * HOUR));
            let tz = Local.timestamp_millis_opt(start).single().map(|d| d.offset().local_minus_utc() as i64 * 1000).unwrap_or(0);
            let mut t = ((start + tz) / step) * step - tz;
            while t <= self.view_end {
                let x = x_of(t);
                if x >= rect.left() {
                    let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
                    let midnight = dt.format("%H:%M").to_string() == "00:00";
                    let label = if midnight || step >= 24 * HOUR { dt.format("%a %d").to_string() } else { dt.format("%H:%M").to_string() };
                    p.line_segment([Pos2::new(x, ruler.bottom() - 4.0), Pos2::new(x, act_band.bottom())], Stroke::new(1.0, if midnight { theme::BORDER_STRONG } else { Color32::from_white_alpha(10) }));
                    p.text(Pos2::new(x + 4.0, ruler.top() + 3.0), Align2::LEFT_TOP, label, FontId::proportional(11.0), if midnight { theme::TEXT_DIM } else { theme::TEXT_FAINT });
                }
                t += step;
            }

            // Recording segments.
            for r in &self.recs {
                let (a, b) = (r.start_ms, self.seg_end(r));
                if b < start || a > self.view_end {
                    continue;
                }
                let sr = Rect::from_x_y_ranges(x_of(a).max(rect.left())..=x_of(b).min(rect.right()).max(x_of(a) + 1.0), seg_band.y_range());
                let fill = if r.kept { theme::blend(theme::SURFACE3, c.accent, 0.43) } else { theme::BORDER_STRONG };
                p.rect_filled(sr.shrink2(Vec2::new(0.5, 0.0)), CornerRadius::same(4), fill);
                if r.is_live() {
                    p.rect_filled(Rect::from_x_y_ranges(sr.right() - 3.0..=sr.right(), sr.y_range()), CornerRadius::same(1), theme::LIVE);
                }
            }
            // Clips saved in this range (thin accent bars along the bottom of the band).
            for cl in &self.clips {
                let a = cl.source_start_ms.unwrap_or(cl.created_at - (cl.duration * 1000.0) as i64);
                let b = cl.created_at;
                if b < start || a > self.view_end {
                    continue;
                }
                let cr = Rect::from_x_y_ranges(x_of(a).max(rect.left())..=x_of(b).max(x_of(a) + 2.0), seg_band.bottom() - 6.0..=seg_band.bottom() - 2.0);
                p.rect_filled(cr, CornerRadius::same(2), c.accent.gamma_multiply(0.85));
            }
            // Activity lane: which program was focused.
            if c.cfg.player.show_activity_on_timeline {
                for s in &self.spans {
                    if s.end_ms < start || s.start_ms > self.view_end {
                        continue;
                    }
                    let x0 = x_of(s.start_ms).max(rect.left());
                    let x1 = x_of(s.end_ms).min(rect.right());
                    if x1 - x0 < 0.5 {
                        continue;
                    }
                    let color = if s.is_game { theme::game_color(s.game_id.unwrap_or(0)) } else { Color32::from_white_alpha(30) };
                    let br = Rect::from_x_y_ranges(x0..=x1, act_band.y_range());
                    p.rect_filled(br, CornerRadius::same(3), color);
                    if x1 - x0 > 46.0 {
                        let text_color = if s.is_game { Color32::from_black_alpha(220) } else { theme::TEXT_DIM };
                        p.with_clip_rect(br).text(Pos2::new(x0 + 5.0, br.center().y), Align2::LEFT_CENTER, crate::views::clips::truncate(&s.name, ((x1 - x0) / 7.0) as usize), FontId::new(10.5, theme::semibold()), text_color);
                    }
                }
            }
            // Selection.
            if let (Some(a), Some(b)) = (self.in_ms, self.out_ms) {
                let (a, b) = (a.min(b), a.max(b));
                let sr = Rect::from_x_y_ranges(x_of(a)..=x_of(b), seg_band.top() - 2.0..=act_band.bottom() + 2.0);
                p.rect_filled(sr, CornerRadius::same(3), c.accent.gamma_multiply(0.16));
                p.rect_stroke(sr, CornerRadius::same(3), Stroke::new(1.5, c.accent), egui::StrokeKind::Inside);
            }
            for (pt, label) in [(self.in_ms, "START"), (self.out_ms, "END")] {
                if let Some(t) = pt {
                    let x = x_of(t);
                    p.line_segment([Pos2::new(x, seg_band.top() - 4.0), Pos2::new(x, act_band.bottom() + 4.0)], Stroke::new(2.0, c.accent));
                    let grip = Pos2::new(x, act_band.bottom() + 4.0);
                    p.circle_filled(grip, 5.0, c.accent);
                    if rect.x_range().contains(x) {
                        let id = if label == "START" { "trim_start" } else { "trim_end" };
                        editor_layout::remember_interaction_rect(ui, id, Rect::from_center_size(grip, Vec2::splat(10.0)));
                    }
                    let align = if label == "START" { Align2::RIGHT_BOTTOM } else { Align2::LEFT_BOTTOM };
                    let offset = if label == "START" { -4.0 } else { 4.0 };
                    p.text(Pos2::new(x + offset, row.bottom() - 2.0), align, label, FontId::new(10.0, theme::semibold()), c.accent);
                }
            }
            // Markers.
            for m in &self.markers {
                if m.at_ms < start || m.at_ms > self.view_end {
                    continue;
                }
                let x = x_of(m.at_ms);
                let tri = vec![Pos2::new(x - 5.0, seg_band.top() - 6.0), Pos2::new(x + 5.0, seg_band.top() - 6.0), Pos2::new(x, seg_band.top() + 2.0)];
                p.add(egui::Shape::convex_polygon(tri, theme::WARNING, Stroke::NONE));
            }
            // Now + playhead (with a big grab handle).
            let nx = x_of(now_ms());
            if nx <= rect.right() {
                p.line_segment([Pos2::new(nx, seg_band.top() - 4.0), Pos2::new(nx, act_band.bottom())], Stroke::new(1.5, theme::LIVE));
            }
            if let Some(t) = self.playhead(c) {
                let x = x_of(t);
                p.line_segment([Pos2::new(x, ruler.bottom()), Pos2::new(x, act_band.bottom() + 4.0)], Stroke::new(2.0, Color32::WHITE));
                p.circle_filled(Pos2::new(x, ruler.bottom()), 7.0, Color32::WHITE);
            }

        }

        // Interaction: drag = scrub (drag the ruler or right-drag = pan),
        // wheel = zoom, Shift+wheel = pan, Shift+click = select.
        let resp = ui.interact(Rect::from_x_y_ranges(rect.x_range(), row.y_range()), ui.scope_id().with("timeline"), Sense::click_and_drag());
        let trim_handle = |pos: Pos2| {
            let trim_top = compact.as_ref().map_or(seg_band.top(), |ruler| ruler.track.top() - 5.0);
            if pos.y < trim_top {
                return None;
            }
            [(self.in_ms, DragMode::TrimStart), (self.out_ms, DragMode::TrimEnd)]
                .into_iter()
                .filter_map(|(time, mode)| {
                    let x = x_of(time?);
                    let distance = (x - pos.x).abs();
                    (rect.x_range().contains(x) && distance <= 8.0).then_some((distance, mode))
                })
                .min_by(|(a, _), (b, _)| a.total_cmp(b))
                .map(|(_, mode)| mode)
        };
        let hover_trim = resp.hover_pos().and_then(&trim_handle).is_some();
        let (pressed, released) = ui.input(|input| {
            let mut pressed = None;
            let mut released = None;
            for event in &input.events {
                if let egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: down, .. } = event {
                    if *down { pressed = Some(*pos); } else { released = Some(*pos); }
                }
            }
            (pressed, released)
        });
        if let Some(handle) = pressed.filter(|pos| resp.interact_rect.contains(*pos)).and_then(trim_handle) {
            self.stop_preview(c);
            self.follow_live = false;
            self.drag_mode = Some(handle);
        }
        let dragging_trim = self.drag_mode.filter(|m| matches!(m, DragMode::TrimStart | DragMode::TrimEnd));
        if let Some(handle) = dragging_trim {
            let position = released.or_else(|| resp.interact_pointer_pos().filter(|_| resp.dragged()));
            if let Some(pos) = position {
                self.move_trim_handle(handle, t_of(pos.x.clamp(rect.left(), rect.right())));
            }
            if released.is_some() || !ui.input(|i| i.pointer.primary_down()) {
                self.drag_mode = None;
            }
        }
        if dragging_trim.is_none() && resp.drag_started() {
            let pan_band_end = compact.as_ref().map_or(ruler.bottom(), |ruler| ruler.track.top() - 3.0);
            let on_ruler = resp.interact_pointer_pos().is_some_and(|p| p.y <= pan_band_end);
            let secondary = ui.input(|i| i.pointer.secondary_down() || i.pointer.middle_down());
            self.drag_mode = Some(if on_ruler || secondary { DragMode::Pan } else { DragMode::Scrub });
            if self.drag_mode == Some(DragMode::Scrub) {
                self.scrub_resume = !c.player.state().paused;
                self.stop_preview(c);
                c.player.set_paused(true);
                c.player.scrubbing = true;
                self.follow_live = false;
            }
        }
        if dragging_trim.is_none() && resp.dragged() {
            match self.drag_mode {
                Some(DragMode::Pan) => {
                    self.view_end -= (resp.drag_delta().x as f64 / px_per_ms) as i64;
                    self.follow_live = false;
                }
                Some(DragMode::Scrub) => {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        // Auto-pan when scrubbing past the edges.
                        if pos.x > rect.right() - 20.0 {
                            self.view_end += span / 60;
                        } else if pos.x < rect.left() + 20.0 {
                            self.view_end -= span / 60;
                        }
                        let t = t_of(pos.x.clamp(rect.left(), rect.right()));
                        self.scrub_to(c, t);
                    }
                }
                _ => {}
            }
        }
        if dragging_trim.is_none() && resp.drag_stopped() {
            if self.drag_mode == Some(DragMode::Scrub) {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let t = t_of(pos.x.clamp(rect.left(), rect.right()));
                    self.play_at(c, t, self.scrub_resume);
                }
                c.player.scrubbing = false;
            }
            self.drag_mode = None;
        }
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            let dt = Local.timestamp_millis_opt(t).single().unwrap_or_default();
            let mut tip = dt.format("%a %d %b %H:%M:%S").to_string();
            if let Some(s) = self.spans.iter().find(|s| s.start_ms <= t && s.end_ms >= t) {
                tip.push_str(&format!("\nOn screen: {}", s.name));
            }
            if let Some(m) = self.markers.iter().find(|m| (x_of(m.at_ms) - pos.x).abs() < 6.0) {
                tip.push_str(&format!("\nMarker{}", if m.label.is_empty() { String::new() } else { format!(": {}", m.label) }));
            }
            if !self.recs.iter().any(|r| r.start_ms <= t && self.seg_end(r) >= t) {
                tip.push_str("\nNot recorded");
            }
            if self.drag_mode.is_none() {
                tip.push_str("\nClick to seek. Scroll to zoom or drag the ruler to pan. Drag a trim handle to adjust the selection.");
                egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::unique("tl_tip"), egui::PopupAnchor::Pointer).show(|ui| ui.label(tip));
            }
            let (dx, dy) = ui.input(|i| (i.smooth_scroll_delta.x, i.smooth_scroll_delta.y));
            let shift = ui.input(|i| i.modifiers.shift);
            if dx.abs() > 0.1 || shift && dy.abs() > 0.1 {
                let delta = if dx.abs() > 0.1 { dx } else { dy };
                self.view_end -= (delta as f64 / px_per_ms) as i64;
                self.follow_live = false;
            } else if dy.abs() > 0.1 {
                let factor = (1.0 - dy as f64 * 0.0025).clamp(0.5, 2.0);
                let new_span = ((span as f64) * factor).clamp((2 * MIN) as f64, (30 * 24 * HOUR) as f64) as i64;
                let frac = (pos.x - rect.left()) as f64 / rect.width() as f64;
                let anchor = t_of(pos.x);
                self.view_end = anchor + ((1.0 - frac) * new_span as f64) as i64;
                self.view_span = new_span;
                if self.view_end < now_ms() {
                    self.follow_live = false;
                }
            }
        }
        if dragging_trim.is_none() && resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.stop_preview(c);
                let t = t_of(pos.x);
                if ui.input(|i| i.modifiers.shift) {
                    c.report_mark(match (self.in_ms, self.out_ms) {
                        (Some(a), _) if t > a => self.set_end(t),
                        _ => self.set_start(t),
                    });
                } else {
                    self.play_at(c, t, true);
                }
            }
        }
        let cursor = if hover_trim || matches!(self.drag_mode, Some(DragMode::TrimStart | DragMode::TrimEnd)) {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::PointingHand
        };
        let resp = resp.on_hover_cursor(cursor);
        resp.context_menu(|ui| {
            if ui.button("Add marker at playhead").clicked() {
                if let (Some(t), Some(db)) = (self.playhead(c), c.db()) {
                    let _ = db.add_marker(t, "");
                }
                self.dirty = true;
                ui.close();
            }
            if ui.button("Clear selection").clicked() {
                self.in_ms = None;
                self.out_ms = None;
                ui.close();
            }
        });
        if !watching {
            timeline_help(ui);
        }
    }

    fn audio_lanes(&mut self, ui: &mut Ui, c: &mut Ctx, height_budget: f32) {
        let bounds = self.extent();
        self.waveform_media.retain(|path, _| self.recs.iter().any(|recording| recording.path == *path));
        let span = self.view_span.max(MIN);
        let t0 = (self.view_end - span) as f64 / 1000.0;
        let t1 = self.view_end as f64 / 1000.0;
        // Waveforms for every visible recording file.
        let visible: Vec<(String, f64, f64, bool)> = self
            .recs
            .iter()
            .filter(|r| self.seg_end(r) as f64 / 1000.0 > t0 && (r.start_ms as f64 / 1000.0) < t1)
            .map(|r| (r.path.clone(), r.start_ms as f64 / 1000.0, self.seg_end(r) as f64 / 1000.0, r.is_live()))
            .collect();
        let mut owned = Vec::new();
        for (path, a, b, live) in &visible {
            if let Some(w) = c.waves.get(path) {
                let refresh = self.waveform_media.get(path).is_none_or(|(checked, info)| {
                    (*live || info.is_none()) && checked.elapsed() >= Duration::from_secs(10)
                });
                if refresh {
                    let info = cv_media::probe::probe(std::path::Path::new(path)).ok();
                    self.waveform_media.insert(path.clone(), (Instant::now(), info));
                }
                let tracks = self.waveform_media.get(path).and_then(|(_, info)| info.as_ref())
                    .map(|info| lanes::waveform_tracks(info, b - a, &c.cfg.audio.app_categories));
                owned.push((w, *a, *b, tracks));
            }
        }
        let sources: Vec<WaveSource> = owned.iter().map(|(w, a, b, tracks)| {
            WaveSource { start: *a, end: *b, wave: w, tracks: tracks.as_ref() }
        }).collect();
        let playhead = self.playhead(c).map(|t| t as f64 / 1000.0);
        let edit_bounds = (bounds.0 as f64 / 1000.0, bounds.1 as f64 / 1000.0);
        let default_range = self.selection().or_else(|| {
            self.cur.and_then(|i| self.recs.get(i)).map(|r| (r.start_ms, self.seg_end(r)))
        }).map(|(start, end)| (start as f64 / 1000.0, end as f64 / 1000.0)).unwrap_or(edit_bounds);
        let acts = lanes::lanes(ui, &mut c.player, t0, t1, true, &sources, &self.edits, playhead, &mut self.lane_state, c.accent, self.edit_history.can_undo(), None, edit_bounds, default_range, height_budget);
        let mut region_saved = false;
        for a in acts {
            match a {
                LaneAction::Seek(t) => {
                    self.stop_preview(c);
                    self.play_at(c, (t * 1000.0) as i64, !c.player.state().paused);
                }
                LaneAction::Edit { lanes, a, b, gain } => {
                    if !region_saved {
                        self.edit_history.save(&self.edits, c.player.mix.clone());
                        region_saved = true;
                    }
                    for lane in &lanes {
                        apply_edit(&mut self.edits, lane, a, b, gain);
                    }
                    if let Some(mode) = lanes::mode_for_edit(&lanes, gain).filter(|mode| *mode != c.player.mix.mode) {
                        c.player.set_mode(mode);
                        self.mix = Some(c.player.mix.clone());
                        self.mix_dirty = Some(Instant::now());
                    }
                    self.edits_dirty = Some(Instant::now());
                    let e = self.segment_edits();
                    c.player.set_edits(e);
                }
                LaneAction::MixChanged { previous } => {
                    if let Some(previous) = previous {
                        self.edit_history.save(&self.edits, previous);
                    }
                    self.mix = Some(c.player.mix.clone());
                    self.mix_dirty = Some(Instant::now());
                }
                LaneAction::DisplayChanged => {}
                LaneAction::Undo => {
                    if let Some(snapshot) = self.edit_history.undo() {
                        let now = Instant::now();
                        if snapshot.mix != c.player.mix {
                            self.mix = Some(snapshot.mix.clone());
                            self.mix_dirty = Some(now);
                        }
                        if snapshot.edits != self.edits {
                            self.edits_dirty = Some(now);
                        }
                        self.edits = snapshot.edits;
                        c.player.restore_audio(snapshot.mix, self.segment_edits());
                    }
                }
            }
        }
    }

    fn cut_bar(&mut self, ui: &mut Ui, c: &mut Ctx) {
        ui.horizontal_wrapped(|ui| {
            let ph = self.playhead(c);
            let fmt = |t: Option<i64>| t.and_then(|t| Local.timestamp_millis_opt(t).single()).map(|d| d.format("%H:%M:%S%.3f").to_string()).unwrap_or("—".into());
            ui.add_enabled_ui(ph.is_some(), |ui| {
                if ui.button(format!("Set start  {}", fmt(self.in_ms))).on_hover_text(hint(&c.cfg.player, Action::MarkStart, "Use the current frame as the start")).clicked() {
                    self.stop_preview(c);
                    if let Some(at) = ph { c.report_mark(self.set_start(at)); }
                }
                if ui.button(format!("Set end  {}", fmt(self.out_ms))).on_hover_text(hint(&c.cfg.player, Action::MarkEnd, "Use the current frame as the end")).clicked() {
                    self.stop_preview(c);
                    if let Some(at) = ph { c.report_mark(self.set_end(at)); }
                }
            });
            if theme::icon_button(ui, icon::CLOSE, 12.0, "Clear selection").clicked() {
                self.stop_preview(c);
                self.in_ms = None;
                self.out_ms = None;
            }
            let selection = self.selection();
            if let Some((a, b)) = selection {
                ui.label(theme::label_dim(format_duration_precise((b - a) as f64 / 1000.0)));
                let previewing = self.preview.is_some();
                if ui.button(if previewing { "Stop preview" } else { "Preview" }).clicked() {
                    if previewing {
                        self.stop_preview(c);
                    } else {
                        self.follow_live = false;
                        self.play_at(c, a, true);
                        self.preview = Some(RangePreview::new(a as f64 / 1000.0, b as f64 / 1000.0));
                    }
                }
                if theme::primary_button(ui, Some(icon::SCISSORS), "Save selected clip").clicked() {
                    self.dialog = Some(CutDialog {
                        title: String::new(), game_id: None, cats: Vec::new(),
                        game_search: String::new(), target_size_mb: self.share_mb,
                    });
                }
                ui.menu_button("Export", |ui| {
                    ui.add(egui::DragValue::new(&mut self.share_mb).range(1..=4000).suffix(" MB"));
                    if ui.button(format!("Under {} MB", self.share_mb)).clicked() {
                        self.export_selection(c, Some(self.share_mb), String::new());
                        ui.close();
                    }
                    if ui.button("Full quality").clicked() {
                        self.export_selection(c, None, String::new());
                        ui.close();
                    }
                });
            } else if self.in_ms.is_some() && self.out_ms.is_some() {
                ui.label(theme::small("No footage in this selection"));
            } else {
                ui.add_enabled(false,egui::Button::new("Save selected clip")).on_disabled_hover_text("Set the start and end first.");
            }
            ui.add_space(8.0);
            if theme::ghost_button(ui, Some(icon::FLAG), "Marker").on_hover_text("Add a marker at the playhead").clicked() {
                let t = ph.unwrap_or_else(now_ms);
                if let Some(db) = c.db() {
                    let _ = db.add_marker(t, "");
                }
                self.dirty = true;
            }
            if !self.edits.is_empty() {
                ui.add_space(8.0);
                ui.label(theme::small(format!("{} audio edit{}", self.edits.len(), if self.edits.len() == 1 { "" } else { "s" })));
                if ui.small_button("Clear edits").on_hover_text("Remove every mute/volume edit on the timeline").clicked() {
                    self.edit_history.save(&self.edits, c.player.mix.clone());
                    self.edits.clear();
                    c.player.set_edits(Vec::new());
                    self.edits_dirty = Some(Instant::now());
                }
            }
        });
    }

    fn cut_dialog(&mut self, ui: &mut Ui, c: &mut Ctx) {
        let Some((a, b)) = self.selection() else {
            self.dialog = None;
            return;
        };
        let Some(d) = &mut self.dialog else { return };
        let (a_s, b_s) = (a as f64 / 1000.0, b as f64 / 1000.0);
        let edits_in: Vec<AudioEdit> = self.edits.iter().filter(|e| e.overlaps(a_s, b_s)).cloned().collect();
        let mut close = false;
        let mut create = false;
        let mut export = None;
        egui::Modal::new(egui::Id::unique("cut_dialog")).show(ui.ctx(), |ui| {
            ui.set_width(440.0);
            ui.label(theme::title("Create clip from recording"));
            let from = Local.timestamp_millis_opt(a).single().unwrap_or_default();
            ui.label(theme::label_dim(format!("{} · {}", from.format("%a %d %b %H:%M:%S"), format_duration((b - a) as f64 / 1000.0))));
            ui.add_space(8.0);
            ui.label("Title");
            ui.add(egui::TextEdit::singleline(&mut d.title).hint_text("Automatic (game + date)").desired_width(f32::INFINITY));
            ui.add_space(4.0);
            ui.label("Game");
            let name = d.game_id.and_then(|g| self.games.iter().find(|x| x.id == g)).map(|g| g.name.clone()).unwrap_or_else(|| "Detect automatically".into());
            egui::ComboBox::from_id_salt("cut_game").selected_text(name).width(300.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.game_id, None, "Detect automatically");
                ui.add(egui::TextEdit::singleline(&mut d.game_search).hint_text("Search games"));
                let query = d.game_search.trim().to_lowercase();
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for g in self.games.iter().filter(|g| g.name.to_lowercase().contains(&query)) {
                        ui.selectable_value(&mut d.game_id, Some(g.id), &g.name);
                    }
                });
            });
            if !self.cats.is_empty() {
                ui.add_space(4.0);
                ui.label("Categories");
                ui.horizontal_wrapped(|ui| {
                    for cat in &self.cats {
                        let on = d.cats.contains(&cat.id);
                        if ui.add(egui::Button::selectable(on, &cat.name)).clicked() {
                            if on {
                                d.cats.retain(|x| *x != cat.id);
                            } else {
                                d.cats.push(cat.id);
                            }
                        }
                    }
                });
            }
            ui.add_space(4.0);
            let mix_note = if edits_in.is_empty() { "Your current track volumes are saved with the clip." } else { "Your track volumes and the audio edits in this range are saved with the clip." };
            ui.label(theme::small(format!("{mix_note} All original audio tracks are kept, so you can change them later.")));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, Some(icon::SCISSORS), "Create").clicked() {
                    create = true;
                }
                ui.menu_button("Export only", |ui| {
                    ui.add(egui::DragValue::new(&mut d.target_size_mb).range(1..=4000).suffix(" MB"));
                    if ui.button(format!("Under {} MB", d.target_size_mb)).clicked() {
                        export = Some(Some(d.target_size_mb));
                        ui.close();
                    }
                    if ui.button("Full quality").clicked() {
                        export = Some(None);
                        ui.close();
                    }
                });
                if theme::ghost_button(ui, None, "Cancel").clicked() {
                    close = true;
                }
            });
        });
        if create {
            let d = self.dialog.take().unwrap();
            if c.request(Request::CutRange {
                from_ms: a,
                to_ms: b,
                title: d.title,
                game_id: d.game_id,
                category_ids: d.cats,
                mix: self.mix.clone().or_else(|| Some(c.player.mix.clone())),
                audio_edits: edits_in,
            }).is_some() {
                c.toast("info", "Creating clip… it'll appear in Clips when done");
            }
        } else if let Some(target) = export {
            let d = self.dialog.take().unwrap();
            self.share_mb = d.target_size_mb;
            self.export_selection(c, target, d.title);
        } else if close {
            self.dialog = None;
        }
    }

    fn side(&mut self, ui: &mut Ui, c: &mut Ctx) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(theme::title("Jobs"));
            crate::views::clips::jobs_list(ui, c);
            if c.service.shared.lock().jobs.is_empty() {
                ui.label(theme::small("Clips you create from the timeline show progress here."));
            }
        });
        ui.add_space(12.0);
        let sessions = game_sessions(&self.spans);
        if !sessions.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Game sessions"));
                ui.add(egui::TextEdit::singleline(&mut self.session_search).hint_text("Search games"));
                let query = self.session_search.trim().to_lowercase();
                for s in sessions.iter().filter(|s| s.name.to_lowercase().contains(&query)).take(25) {
                    let dt = Local.timestamp_millis_opt(s.start_ms).single().unwrap_or_default();
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, theme::game_color(s.game_id.unwrap_or(0)));
                        let label = format!("{}  ·  {}", crate::views::clips::truncate(&s.name, 20), dt.format("%a %H:%M"));
                        if ui.add(egui::Label::new(RichText::new(label).color(theme::TEXT)).truncate().sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            self.stop_preview(c);
                            self.follow_live = false;
                            // Frame the whole session with a little margin on both sides.
                            let dur = s.end_ms - s.start_ms;
                            self.view_span = (dur * 12 / 10).max(10 * MIN);
                            self.view_end = s.start_ms - (self.view_span - dur) / 2 + self.view_span;
                            self.play_at(c, s.start_ms, true);
                            self.compact_files = false;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(theme::small(cv_core::util::format_span_short((s.end_ms - s.start_ms) / 1000)));
                        });
                    });
                }
            });
            ui.add_space(12.0);
        }
        if !self.markers.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::title("Markers"));
                ui.add(egui::TextEdit::singleline(&mut self.marker_search).hint_text("Search marker labels"));
                let query = self.marker_search.trim().to_lowercase();
                for m in self.markers.clone().iter().rev().filter(|m| m.label.to_lowercase().contains(&query)).take(30) {
                    ui.horizontal(|ui| {
                        let dt = Local.timestamp_millis_opt(m.at_ms).single().unwrap_or_default();
                        if ui.link(dt.format("%a %H:%M:%S").to_string()).clicked() {
                            self.stop_preview(c);
                            self.play_at(c, m.at_ms, true);
                            self.compact_files = false;
                        }
                        let editing = matches!(&self.rename_marker, Some((id, _)) if *id == m.id);
                        if editing {
                            if let Some((_, text)) = &mut self.rename_marker {
                                let r = ui.add(egui::TextEdit::singleline(text).desired_width(130.0).char_limit(60));
                                let done = r.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
                                if done {
                                    let label = text.trim().to_string();
                                    if let Some(db) = c.db() {
                                        let _ = db.rename_marker(m.id, &label);
                                    }
                                    self.rename_marker = None;
                                    self.dirty = true;
                                } else if !r.has_focus() {
                                    r.request_focus();
                                }
                            }
                        } else {
                            let label = if m.label.is_empty() { "(no label)".to_string() } else { m.label.clone() };
                            let resp = ui.add(egui::Label::new(RichText::new(label).color(theme::TEXT_DIM)).truncate().sense(Sense::click()));
                            if resp.on_hover_text("Click to rename").clicked() {
                                self.rename_marker = Some((m.id, m.label.clone()));
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, icon::DELETE, 11.0, "Delete marker").clicked() {
                                if let Some(db) = c.db() {
                                    let _ = db.delete_marker(m.id);
                                }
                                self.dirty = true;
                            }
                        });
                    });
                }
            });
            ui.add_space(12.0);
        }
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let heading = ui.label(theme::title("Recording files"));
            editor_layout::remember_interaction_rect(ui, "shortcut_recording_search_heading", heading.rect);
            let input = ui.add(egui::TextEdit::singleline(&mut self.footage_search).hint_text("Search file name or date"));
            editor_layout::remember_interaction_rect(ui, "shortcut_recording_search", input.rect);
            if ui.ctx().data_mut(|data| data.remove_temp::<bool>(egui::Id::unique("clipvault-qa-scroll-recording-search"))).unwrap_or(false) {
                input.scroll_to_me_animation(Some(egui::Align::Center), egui::style::ScrollAnimation::none());
            }
            let query = self.footage_search.trim().to_lowercase();
            let mut last_day = String::new();
            for (i, r) in self.recs.clone().iter().enumerate().rev() {
                let dt = Local.timestamp_millis_opt(r.start_ms).single().unwrap_or_default();
                let day = dt.format("%A %d %B").to_string();
                let date = dt.format("%Y-%m-%d %H:%M").to_string();
                if !r.path.to_lowercase().contains(&query) && !day.to_lowercase().contains(&query) && !date.contains(&query) {
                    continue;
                }
                if day != last_day {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&day).size(12.0).color(theme::TEXT_FAINT));
                    last_day = day;
                }
                let duration = (self.seg_end(r) - r.start_ms) as f64 / 1000.0;
                let label = format!("Play {}  ·  {}", dt.format("%H:%M"), format_duration(duration));
                let (play, keep) = recording_file_row(ui, label, r, self.cur == Some(i), c.accent);
                if play.clicked() {
                    self.frame_recording(r.start_ms);
                    self.play_at(c, r.start_ms, true);
                    self.compact_files = false;
                }
                if keep.clicked() {
                    c.service.fire(Request::KeepRecording { recording_id: r.id, keep: !r.kept });
                    self.dirty = true;
                }
            }
        });
    }
}

fn recording_file_row(
    ui: &mut Ui,
    play_label: String,
    recording: &Recording,
    playing: bool,
    accent: Color32,
) -> (egui::Response, egui::Response) {
    const LOCK_ICON_SIZE: f32 = 11.0;

    ui.horizontal_wrapped(|ui| {
        let text = RichText::new(play_label).color(if playing { theme::TEXT } else { theme::TEXT_DIM });
        let play = ui.add(egui::Button::selectable(playing, text).wrap());
        if recording.is_live() {
            theme::pill(ui, theme::LIVE, "LIVE");
        }

        let size = egui::WidgetText::from(theme::small(human_bytes(recording.size.max(0) as u64)))
            .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body);
        let lock_size = theme::icon_button_size(LOCK_ICON_SIZE);
        let metadata_width = lock_size + ui.spacing().item_spacing.x + size.size().x;
        let remaining = ui.available_size_before_wrap().x;
        // Reserve the entire right-aligned group. A full-width slot wraps it as a unit.
        let slot_width = if metadata_width <= remaining {
            remaining
        } else {
            ui.available_width()
        };
        let keep = ui.allocate_ui_with_layout(
            Vec2::new(slot_width, lock_size),
            egui::Layout::right_to_left(egui::Align::Center).with_main_wrap(true),
            |ui| {
                let tip = if recording.kept {
                    "Kept forever (click to allow deletion)"
                } else {
                    "Keep this file forever"
                };
                let keep = theme::icon_button(ui, icon::LOCK, LOCK_ICON_SIZE, tip);
                if recording.kept {
                    ui.painter().circle_stroke(keep.rect.center(), LOCK_ICON_SIZE, Stroke::new(1.0, accent));
                }
                ui.label(size);
                keep
            },
        ).inner;
        (play, keep)
    }).inner
}

/// Merge consecutive activity spans of the same program separated by short
/// gaps (alt-tab blips) so the lane reads cleanly.
fn merge_spans(mut spans: Vec<ActivitySpan>) -> Vec<ActivitySpan> {
    spans.sort_by_key(|s| s.start_ms);
    let mut out: Vec<ActivitySpan> = Vec::with_capacity(spans.len());
    for s in spans {
        if let Some(last) = out.last_mut() {
            if last.key == s.key && s.start_ms - last.end_ms < 15_000 {
                last.end_ms = last.end_ms.max(s.end_ms);
                continue;
            }
            // Fill tiny gaps between different programs too.
            if s.start_ms - last.end_ms < 3_000 && s.start_ms > last.end_ms {
                last.end_ms = s.start_ms;
            }
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod overlap_test;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(key: &str, a: i64, b: i64) -> ActivitySpan {
        ActivitySpan { id: 0, key: key.into(), game_id: None, name: key.into(), is_game: false, title: String::new(), start_ms: a, end_ms: b }
    }

    #[test]
    fn groups_game_sessions() {
        let g = |name: &str, id: i64, a: i64, b: i64| ActivitySpan { id: 0, key: name.into(), game_id: Some(id), name: name.into(), is_game: true, title: String::new(), start_ms: a, end_ms: b };
        let mut spans = vec![g("Deadlock", 1, 0, 20 * MIN), g("Deadlock", 1, 25 * MIN, 50 * MIN), g("CS2", 2, 60 * MIN, 61 * MIN - 1), g("Deadlock", 1, 3 * HOUR, 3 * HOUR + 30 * MIN)];
        spans.push(span("chrome", 20 * MIN, 25 * MIN));
        let s = game_sessions(&spans);
        // Newest first; the 5-minute break merges; the <1 min CS2 blip is dropped.
        assert_eq!(s.len(), 2);
        assert_eq!((s[1].start_ms, s[1].end_ms), (0, 50 * MIN));
        assert_eq!(s[0].start_ms, 3 * HOUR);
    }

    #[test]
    fn merges_activity_blips() {
        let v = merge_spans(vec![span("a", 0, 10_000), span("a", 12_000, 20_000), span("b", 21_000, 30_000), span("a", 60_000, 70_000)]);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].start_ms, v[0].end_ms), (0, 21_000));
        assert_eq!(v[1].key, "b");
        assert_eq!(v[2].start_ms, 60_000);
    }

    fn recording(path: &str, start_ms: i64, duration: f64, status: &str) -> Recording {
        Recording {
            id: 0,
            path: path.into(),
            start_ms,
            duration,
            size: 1,
            status: status.into(),
            audio_tracks: 1,
            kept: false,
            probed: true,
            probe_filetime: None,
        }
    }

    #[test]
    fn recording_file_controls_remain_visible_and_separate_while_resizing() {
        use egui::epaint::Shape;

        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, theme::TEXT, 1.0);

        for (status, kept, size, play_label) in [
            ("recording", false, 616 * 1024, "Play 04:35  ·  0:01"),
            ("complete", false, 2_621_444, "Play 04:33  ·  0:40"),
            ("complete", true, 1 << 50, "Play 18:05  ·  24:59:59"),
        ] {
            let mut file = recording("fixture.mp4", 1_000, 1.0, status);
            file.kept = kept;
            file.size = size;
            let size_label = human_bytes(size as u64);
            let mut wrapped = false;

            for width in (130..=300).chain([420, 230, 180, 420]) {
                let slot = Rect::from_min_size(Pos2::ZERO, Vec2::new(width as f32, 160.0));
                let mut controls = None;
                let mut output = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(slot),
                    ..Default::default()
                }, |ui| {
                    editor_layout::region(ui, "recording_file", slot, |ui| {
                        controls = Some(recording_file_row(
                            ui, play_label.into(), &file, file.is_live(), theme::SUCCESS,
                        ));
                    });
                });
                output.textures_delta.clear();
                let (play, keep) = controls.unwrap();
                let text_rect = |label: &str| {
                    output.shapes.iter().find_map(|painted| match &painted.shape {
                        Shape::Text(text) if text.galley.text() == label => Some(text.visual_bounding_rect()),
                        _ => None,
                    }).unwrap_or_else(|| panic!("Missing {label:?} at width {width}"))
                };
                let size_rect = text_rect(&size_label);
                let mut rects = vec![play.rect, size_rect, keep.rect];
                assert!(play.rect.contains_rect(text_rect(play_label)));
                assert!(keep.rect.contains_rect(text_rect(icon::LOCK)));

                let badge = output.shapes.iter().find_map(|painted| match &painted.shape {
                    Shape::Rect(rect) if rect.fill == theme::LIVE.gamma_multiply(0.16) => Some(rect.rect),
                    _ => None,
                });
                assert_eq!(badge.is_some(), file.is_live());
                if let Some(badge) = badge {
                    assert!(badge.contains_rect(text_rect("LIVE")));
                    rects.push(badge);
                    wrapped |= badge.bottom() < size_rect.top();
                }
                assert_eq!(output.shapes.iter().any(|painted| matches!(
                    &painted.shape, Shape::Circle(circle) if circle.stroke.color == theme::SUCCESS
                )), kept);

                for (index, rect) in rects.iter().enumerate() {
                    assert!(slot.contains_rect(*rect), "{status} at width {width}: {rect:?} exceeds {slot:?}");
                    for other in &rects[..index] {
                        assert!(!rect.intersects(*other), "{status} at width {width}: {rect:?} overlaps {other:?}");
                    }
                }
                assert!((keep.rect.right() - slot.right()).abs() < 1.0);
                assert!(size_rect.right() < keep.rect.left());
                if width == 420 {
                    assert!(rects.iter().all(|rect| rect.y_range().contains(play.rect.center().y)));
                }
            }
            if file.is_live() {
                assert!(wrapped, "The live row should move metadata to a second line when space runs out");
            }
        }
    }

    #[test]
    fn recording_actions_are_fully_painted_or_reachable_by_scrolling() {
        use egui::epaint::Shape;

        for (width, height, initially_visible) in [
            (370.0, 500.0, false),
            (500.0, 500.0, true),
            (685.0, 600.0, true),
            (768.0, 600.0, true),
            (938.0, 771.0, true),
            (1_245.0, 771.0, true),
        ] {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, theme::TEXT, 1.0);
            let cfg = cv_core::config::Config::default();
            let mut c = Ctx {
                player: crate::player::Player::new(ctx.clone(), &cfg),
                service: crate::service::Service::disconnected_for_test(),
                thumbs: crate::thumbs::Thumbs::new(ctx.clone()),
                waves: Default::default(),
                db: None,
                cfg,
                accent: theme::TEXT,
                toasts: Vec::new(),
                goto: None,
                open_recording_settings: false,
            };
            let mut view = RecordingsView {
                recs: vec![recording("fixture.mp4", 1_000, 40.0, "complete")],
                in_ms: Some(4_500),
                out_ms: Some(26_750),
                ..Default::default()
            };
            assert!(view.selection().is_some());
            let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(width + 40.0, height));
            let body = Rect::from_min_max(Pos2::new(20.0, 80.0), viewport.max - Vec2::splat(20.0));
            let mut visible = false;
            let mut pointer = None;
            for frame in 0..24 {
                let events = pointer.map_or_else(Vec::new, |pos| vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        source: egui::MouseWheelSource::Trackpad,
                        delta: Vec2::new(0.0, -30.0),
                        modifiers: egui::Modifiers::NONE,
                        phase: if frame == 1 { egui::TouchPhase::Start } else { egui::TouchPhase::Move },
                    },
                ]);
                let (mut transport, mut viewport_clip) = (Rect::NOTHING, Rect::NOTHING);
                let mut output = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(viewport),
                    time: Some(frame as f64 / 60.0),
                    events,
                    ..Default::default()
                }, |ui| {
                    editor_layout::region(ui, "recording_body", body, |ui| {
                        let sidebar = (width >= editor_layout::TWO_COLUMN_WIDTH)
                            .then(|| (width * 0.30).clamp(245.0, 330.0));
                        let layout = EditorLayout::with_preview(
                            ui, sidebar, 398.0, true, Some(&view.preview_size),
                        );
                        transport = layout.transport;
                        editor_layout::region(ui, "rec_transport", transport, |ui| {
                            editor_layout::controls(ui, "rec_transport_scroll", |ui| {
                                viewport_clip = ui.clip_rect().intersect(transport);
                                c.player.transport(ui, "Sat 28 Nov 04:29:15  ·  1m ago", &c.cfg.player);
                                view.cut_bar(ui, &mut c);
                            });
                        });
                    });
                });
                output.textures_delta.clear();
                visible = ["Export", "Marker"].into_iter().all(|label| {
                    let text = output.shapes.iter().find_map(|painted| match &painted.shape {
                        Shape::Text(text) if text.galley.text() == label => Some(text.visual_bounding_rect()),
                        _ => None,
                    });
                    let Some(text) = text else { return false };
                    output.shapes.iter().any(|painted| match &painted.shape {
                        Shape::Rect(button) if button.rect.width() < 130.0 && button.rect.height() <= 40.0
                            && button.rect.contains_rect(text) =>
                        {
                            let bounds = button.visual_bounding_rect();
                            painted.clip_rect.contains_rect(bounds) && viewport_clip.contains_rect(bounds)
                        }
                        _ => false,
                    })
                });
                if frame == 0 && initially_visible {
                    assert!(visible, "Export and Marker are clipped at {width}×{height} before scrolling");
                }
                if visible {
                    break;
                }
                pointer = Some(transport.right_top() + Vec2::new(-36.0, 28.0));
            }
            assert!(visible, "Export and Marker cannot be fully revealed at {width}×{height}");
        }
    }

    #[test]
    fn latest_uses_committed_edge_and_falls_back_to_the_latest_archive() {
        let mut view = RecordingsView {
            recs: vec![recording("archive.mp4", 1_000, 2.0, "complete"),
                       recording("active.mp4", 6_000, 900.0, "recording")],
            ..Default::default()
        };
        assert_eq!(view.latest_playable(), Some((2_999, false)));
        view.written_live = Some(("another.mp4".into(), 8_000));
        assert_eq!(view.latest_playable(), Some((2_999, false)));
        view.written_live = Some(("active.mp4".into(), 6_500));
        assert_eq!(view.latest_playable(), Some((6_499, true)));
    }

    #[test]
    fn live_rollover_waits_for_playable_bytes_and_respects_pause() {
        let mut view = RecordingsView {
            recs: vec![recording("old.mp4", 1_000, 2.0, "complete"),
                       recording("new.mp4", 3_000, 30.0, "recording")],
            live_playback: Some(("old.mp4".into(), 3_000)),
            ..Default::default()
        };
        assert!(view.next_live_segment(false, true).is_none());
        view.written_live = Some(("new.mp4".into(), 3_000));
        assert!(view.next_live_segment(false, true).is_none());
        view.written_live = Some(("new.mp4".into(), 3_500));
        assert!(view.next_live_segment(true, true).is_none());
        assert!(view.next_live_segment(false, false).is_none());
        assert_eq!(view.next_live_segment(false, true).unwrap().path, "new.mp4");
        view.live_playback = None;
        assert!(view.next_live_segment(false, true).is_none());
    }

    #[test]
    fn recording_frame_navigation_skips_gaps_and_contained_copies() {
        let view = RecordingsView {
            recs: vec![recording("first.mp4", 1_000, 4.0, "complete"),
                       recording("contained.mp4", 2_000, 1.0, "complete"),
                       recording("overlap.mp4", 4_000, 2.0, "complete"),
                       recording("after gap.mp4", 9_000, 2.0, "complete")],
            ..Default::default()
        };
        assert_eq!(view.adjacent_frame_time(0, true), Some(4_000));
        assert_eq!(view.adjacent_frame_time(2, false), Some(3_999));
        assert_eq!(view.adjacent_frame_time(2, true), Some(9_000));
        assert_eq!(view.adjacent_frame_time(3, false), Some(5_999));
        assert_eq!(view.adjacent_frame_time(3, true), None);

        let state = crate::player::PlayState {
            first_frame: true,
            ..Default::default()
        };
        assert_eq!(view.frame_step_handoff(2, &state, false), Some(3_999));
        let state = crate::player::PlayState {
            next_frame_pos: Some(3.0),
            last_frame: false,
            ..Default::default()
        };
        assert_eq!(view.frame_step_handoff(0, &state, true), Some(4_000));
        let state = crate::player::PlayState { next_frame_pos: Some(2.96), ..state };
        assert_eq!(view.frame_step_handoff(0, &state, true), None);
    }

    #[test]
    fn dragging_trim_handles_clamps_to_footage_and_preserves_endpoint_order() {
        let mut view = RecordingsView {
            recs: vec![recording("archive.mp4", 1_000, 2.0, "complete")],
            in_ms: Some(1_500),
            out_ms: Some(2_000),
            ..Default::default()
        };
        view.move_trim_handle(DragMode::TrimStart, 2_999);
        assert_eq!(view.selection(), Some((1_899, 2_000)));
        view.move_trim_handle(DragMode::TrimStart, 10);
        view.move_trim_handle(DragMode::TrimEnd, 10_000);
        assert_eq!(view.selection(), Some((1_000, 3_000)));
        view.move_trim_handle(DragMode::TrimEnd, 100);
        assert_eq!(view.selection(), Some((1_000, 1_101)));
    }

    #[test]
    fn selection_clamps_to_written_footage_and_rejects_gaps() {
        let mut view = RecordingsView {
            recs: vec![recording("closed.mp4", 1_000, 2.0, "complete"),
                       recording("live.mp4", 5_000, 1.0, "recording")],
            written_live: Some(("live.mp4".into(), 7_000)),
            in_ms: Some(10_000),
            out_ms: Some(0),
            ..Default::default()
        };
        assert_eq!(view.selection(), Some((1_000, 7_000)));
        view.in_ms = Some(4_000);
        view.out_ms = Some(4_900);
        assert_eq!(view.selection(), None);
    }

    #[allow(dead_code)]
    fn _types(_: Option<Pos2>) {}

    #[test]
    fn reversed_or_equal_mark_does_not_create_a_short_clip() {
        let mut view = RecordingsView::default();
        view.set_end(2_000).unwrap();
        assert!(view.set_start(3_000).is_err());
        assert!(view.set_start(2_000).is_err());
        assert_eq!((view.in_ms, view.out_ms), (None, Some(2_000)));
        view.set_start(1_800).unwrap();
        assert!(view.set_end(1_850).is_err());
        assert_eq!((view.in_ms, view.out_ms), (Some(1_800), Some(2_000)));
    }
}
