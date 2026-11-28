//! The 24/7 recorder: screen + every app's sound + microphone, written as
//! crash-safe fragmented MP4 segments.
//!
//! Three threads:
//! * video (`video.rs`): steady frame clock, screen capture, GPU conversion,
//!   hardware H.264 encoding;
//! * audio (`audio.rs`): finds programs playing sound, captures each one on
//!   its own plus the microphone, lines everything up, mixes the "Full mix",
//!   and AAC-encodes the full mix, the mic and one track per app;
//! * muxer (this file): interleaves both into one fragment per keyframe
//!   interval and rolls over to a new file every few minutes.
//!
//! Track layout of every segment: 1 video, 2 full mix, 3 microphone,
//! 4.. app tracks ("slots"). Which app occupies which slot is stored as
//! ClipVault metadata in the file (and reported as events).

mod audio;
mod video;

#[cfg(test)]
mod rollover_test;

use crate::capture::hub::{frames_to_hns, hns_to_frames};
use crate::capture::CaptureMethod;
use crate::clock::{now_hns, Epoch};
use crate::mf::h264::{EncoderChoice, RateControl};
use crate::mp4::frag::{FragWriter, TrackRun};
use crate::mp4::{Codec, SampleRef, TrackSpec};
use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const TRACK_VIDEO: u32 = 1;
pub const TRACK_FULL: u32 = 2;
pub const TRACK_MIC: u32 = 3;
pub const FIRST_SLOT_TRACK: u32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum MonitorChoice {
    Primary,
    /// `\\.\DISPLAYn`
    Device(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecorderConfig {
    pub folder: PathBuf,
    pub segment_secs: u32,
    pub fps: u32,
    /// Scale the picture down to at most this height (0 = full resolution).
    pub max_height: u32,
    pub monitor: MonitorChoice,
    pub capture_method: CaptureMethod,
    pub encoder: EncoderChoice,
    pub rate: RateControl,
    pub keyframe_secs: f32,
    pub cursor: bool,
    /// Full mix and microphone bitrate.
    pub audio_kbps: u32,
    /// Per-app track bitrate.
    pub app_kbps: u32,
    /// Number of per-app tracks in each file.
    pub app_slots: u32,
    /// Record each app's sound on its own track.
    pub record_apps: bool,
    /// Microphone endpoint id (`Some(None)` = Windows default, `None` = no mic).
    pub mic: Option<Option<String>>,
    pub mic_gain: f32,
    /// Process ids never captured (ClipVault itself).
    pub exclude_pids: Vec<u32>,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        RecorderConfig {
            folder: PathBuf::from("."),
            segment_secs: 30 * 60,
            fps: 60,
            max_height: 1080,
            monitor: MonitorChoice::Primary,
            capture_method: CaptureMethod::Auto,
            encoder: EncoderChoice::Auto,
            rate: RateControl::Vbr { kbps: 12_000, max_kbps: 20_000 },
            keyframe_secs: 1.0,
            cursor: true,
            audio_kbps: 160,
            app_kbps: 128,
            app_slots: 10,
            record_apps: true,
            mic: Some(None),
            mic_gain: 1.0,
            exclude_pids: Vec::new(),
        }
    }
}

/// How the recorder should name and categorise an app (supplied by the
/// service, which knows games and the user's category lists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppIdent {
    /// Stable key: lower-case exe name.
    pub key: String,
    pub name: String,
    /// "game", "voice", "media", "other".
    pub category: String,
}

pub trait AppNamer: Send + Sync {
    fn identify(&self, exe: &str, pid: u32) -> AppIdent;
}

/// Which app is on which app track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slot: u32,
    pub app: AppIdent,
}

/// ClipVault metadata written into recordings (JSON in a `free` box).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SegmentMeta {
    /// Seconds into the file this applies from.
    pub at_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<TrackMeta>,
    pub slots: Vec<SlotInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub id: u32,
    /// "video", "full", "mic", "slot", or (in clips) "app".
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppIdent>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum RecEvent {
    SegmentStarted { path: PathBuf, start_ms: i64 },
    SegmentFinished { path: PathBuf, start_ms: i64, duration_s: f64, bytes: u64 },
    /// App track assignment changed (`at_ms` wall clock).
    Slots { at_ms: i64, slots: Vec<SlotInfo> },
    Warning(String),
}

/// Live levels (0..1 peak) for meters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Levels {
    pub full: f32,
    pub mic: f32,
    pub apps: Vec<(AppIdent, f32)>,
}

#[derive(Debug, Clone, Default)]
pub struct RecorderStatus {
    pub running: bool,
    pub path: Option<PathBuf>,
    pub segment_start_ms: i64,
    pub bytes: u64,
    /// Wall clock up to which everything is safely in the file.
    pub written_until_ms: i64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub dropped_frames: u64,
    pub encoder: String,
    pub capture_method: String,
    pub video_error: Option<String>,
    pub levels: Levels,
    pub mic_ok: bool,
}

// ------------------------------------------------------------ messages

pub(crate) struct VideoPacket {
    pub time_hns: i64,
    pub data: Vec<u8>,
    pub key: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AudioTrack {
    Full,
    Mic,
    Slot(u32),
}

pub(crate) struct AudioPacket {
    pub track: AudioTrack,
    /// Frame position (48 kHz) of the first sample.
    pub pos: i64,
    pub data: Vec<u8>,
}

pub(crate) enum MuxMsg {
    Video(VideoPacket),
    Audio(AudioPacket),
    /// The video pipeline stopped (capture lost, monitor gone, shutting down).
    VideoEnd,
    /// All audio before this frame position has been sent.
    AudioWatermark(i64),
    /// App-track assignment from frame position pos on.
    Slots { pos: i64, slots: Vec<SlotInfo> },
    /// Loudness of every audio track (file order) for the 100 ms starting at `pos`.
    Peaks { pos: i64, levels: Vec<u8> },
    Flush { after_hns: i64, reply: Sender<i64> },
    Stop,
}

pub(crate) struct Shared {
    pub cfg: Mutex<RecorderConfig>,
    pub window_test: Option<isize>,
    pub status: Mutex<RecorderStatus>,
    pub stop: AtomicBool,
    pub force_key: AtomicBool,
    pub epoch: Epoch,
    pub events: Sender<RecEvent>,
}

pub struct Recorder {
    shared: Arc<Shared>,
    mux_tx: Sender<MuxMsg>,
    video: Option<std::thread::JoinHandle<()>>,
    audio: Option<std::thread::JoinHandle<()>>,
    mux: Option<std::thread::JoinHandle<()>>,
    mic_tx: Sender<(Option<Option<String>>, f32)>,
}

impl Recorder {
    pub fn start(cfg: RecorderConfig, namer: Arc<dyn AppNamer>, events: Sender<RecEvent>) -> Result<Recorder> {
        Self::start_with(cfg, namer, events, None)
    }

    /// Synthetic QA window with silent audio; cannot select a display or another process.
    #[cfg(feature = "qa-window")]
    pub fn start_test_window(mut cfg: RecorderConfig, namer: Arc<dyn AppNamer>, events: Sender<RecEvent>, hwnd: isize) -> Result<Recorder> {
        cfg.mic = None;
        cfg.record_apps = false;
        cfg.cursor = false;
        Self::start_with(cfg, namer, events, Some(hwnd))
    }

    fn start_with(cfg: RecorderConfig, namer: Arc<dyn AppNamer>, events: Sender<RecEvent>, window_test: Option<isize>) -> Result<Recorder> {
        std::fs::create_dir_all(&cfg.folder).with_context(|| format!("creating {}", cfg.folder.display()))?;
        let shared = Arc::new(Shared {
            cfg: Mutex::new(cfg),
            window_test,
            status: Mutex::new(RecorderStatus { running: true, ..Default::default() }),
            stop: AtomicBool::new(false),
            force_key: AtomicBool::new(false),
            epoch: Epoch::now(),
            events,
        });
        let (mux_tx, mux_rx) = unbounded();
        let (mic_tx, mic_rx) = unbounded();
        let mux = {
            let sh = shared.clone();
            std::thread::Builder::new().name("rec-mux".into()).spawn(move || Muxer::new(sh).run(mux_rx))?
        };
        let video = {
            let (sh, tx) = (shared.clone(), mux_tx.clone());
            std::thread::Builder::new().name("rec-video".into()).spawn(move || video::run(sh, tx))?
        };
        let audio = {
            let (sh, tx) = (shared.clone(), mux_tx.clone());
            std::thread::Builder::new().name("rec-audio".into()).spawn(move || audio::run(sh, tx, namer, mic_rx))?
        };
        Ok(Recorder { shared, mux_tx, video: Some(video), audio: Some(audio), mux: Some(mux), mic_tx })
    }

    pub fn status(&self) -> RecorderStatus {
        self.shared.status.lock().clone()
    }

    pub fn epoch(&self) -> Epoch {
        self.shared.epoch
    }

    /// Change the microphone (or turn it off) without restarting.
    pub fn set_mic(&self, mic: Option<Option<String>>, gain: f32) {
        let _ = self.mic_tx.send((mic, gain));
    }

    /// Make sure everything recorded until now is in the file: forces a
    /// keyframe and waits for that fragment to be written. Returns the wall
    /// clock (Unix ms) up to which the recording is complete.
    pub fn flush(&self, timeout: Duration) -> Result<i64> {
        let (tx, rx) = bounded(1);
        let now = now_hns();
        self.mux_tx.send(MuxMsg::Flush { after_hns: now, reply: tx }).context("recorder stopped")?;
        self.shared.force_key.store(true, Ordering::SeqCst);
        rx.recv_timeout(timeout).context("the recording didn't catch up in time")
    }

    /// Stop and finish the current file properly.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.video.take() {
            let _ = t.join();
        }
        if let Some(t) = self.audio.take() {
            let _ = t.join();
        }
        let _ = self.mux_tx.send(MuxMsg::Stop);
        if let Some(t) = self.mux.take() {
            let _ = t.join();
        }
        self.shared.status.lock().running = false;
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.mux.is_some() {
            self.shutdown();
        }
    }
}

/// File name for a segment starting at `unix_ms` (local time).
pub fn segment_name(unix_ms: i64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Local.timestamp_millis_opt(unix_ms).single().unwrap_or_else(chrono::Local::now);
    format!("{}.mp4", dt.format("%Y-%m-%d %H-%M-%S"))
}

/// Read the ClipVault metadata from a file's metadata payloads, in order.
pub fn parse_meta(metas: &[(u64, Vec<u8>)]) -> Vec<SegmentMeta> {
    metas.iter().filter_map(|(_, b)| serde_json::from_slice::<SegmentMeta>(b).ok()).collect()
}

// ------------------------------------------------------------ muxer

/// Write a fragment even if audio hasn't caught up after this long.
const AUDIO_WAIT_HNS: i64 = 30_000_000;
/// A pause in the video longer than this starts a new file.
const GAP_HNS: i64 = 15_000_000;

struct Gop {
    start: i64,
    packets: Vec<VideoPacket>,
}

struct Segment {
    w: FragWriter,
    path: PathBuf,
    start_hns: i64,
    start_frames: i64,
    /// Wall clock at `start_hns` (anchored when the file was opened).
    start_ms: i64,
    avcc: Vec<u8>,
    size: (u32, u32),
    end_hns: i64,
    /// Live waveform of this file's audio tracks.
    wave: cv_core::waveform::Waveform,
    wave_saved: std::time::Instant,
}

/// Frames per waveform value (100 ms).
const WAVE_STEP: i64 = 4800;

/// 100 ns -> 90 kHz ticks, rounded to nearest.
fn ticks90(hns: i64) -> i64 {
    ((hns as i128 * 9 + if hns >= 0 { 500 } else { -500 }) / 1000) as i64
}

struct Muxer {
    sh: Arc<Shared>,
    seg: Option<Segment>,
    gops: VecDeque<Gop>,
    video_ended: bool,
    audio: HashMap<AudioTrack, VecDeque<AudioPacket>>,
    watermark: i64,
    /// App-track assignments in effect at the end of what's been written.
    slots: Vec<SlotInfo>,
    /// Assignment changes not yet written: (frame position, assignment).
    slot_changes: VecDeque<(i64, Vec<SlotInfo>)>,
    waiters: Vec<(i64, Sender<i64>)>,
    /// After a write error, don't try again before this time.
    retry_at: Option<std::time::Instant>,
    last_trim: std::time::Instant,
    peaks: VecDeque<(i64, Vec<u8>)>,
}

impl Muxer {
    fn new(sh: Arc<Shared>) -> Muxer {
        Muxer {
            sh,
            seg: None,
            gops: VecDeque::new(),
            video_ended: false,
            audio: HashMap::new(),
            watermark: 0,
            slots: Vec::new(),
            slot_changes: VecDeque::new(),
            waiters: Vec::new(),
            retry_at: None,
            last_trim: std::time::Instant::now(),
            peaks: VecDeque::new(),
        }
    }

    fn run(mut self, rx: Receiver<MuxMsg>) {
        loop {
            let msg = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(m) => Some(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            };
            match msg {
                Some(MuxMsg::Video(p)) => {
                    self.video_ended = false;
                    if p.key {
                        self.gops.push_back(Gop { start: p.time_hns, packets: vec![p] });
                    } else if let Some(g) = self.gops.back_mut() {
                        g.packets.push(p);
                    }
                    self.write_ready(false);
                }
                Some(MuxMsg::VideoEnd) => {
                    self.video_ended = true;
                    self.write_ready(false);
                }
                Some(MuxMsg::Audio(p)) => self.audio.entry(p.track).or_default().push_back(p),
                Some(MuxMsg::AudioWatermark(w)) => {
                    self.watermark = self.watermark.max(w);
                    self.write_ready(false);
                }
                Some(MuxMsg::Slots { pos, slots }) => self.slot_changes.push_back((pos, slots)),
                Some(MuxMsg::Peaks { pos, levels }) => self.peaks.push_back((pos, levels)),
                Some(MuxMsg::Flush { after_hns, reply }) => self.waiters.push((after_hns, reply)),
                Some(MuxMsg::Stop) => break,
                None => self.write_ready(false),
            }
            if self.last_trim.elapsed() >= Duration::from_millis(500) {
                self.last_trim = std::time::Instant::now();
                self.trim_stale_audio();
            }
        }
        // Write everything that's left, then close the file.
        self.video_ended = true;
        self.write_ready(true);
        if let Some(seg) = self.seg.take() {
            self.finish_segment(seg);
        }
    }

    fn frame_pos(&self, hns: i64) -> i64 {
        hns_to_frames(hns - self.sh.epoch.hns)
    }

    fn frame_hns(&self) -> i64 {
        10_000_000 / self.sh.cfg.lock().fps.max(1) as i64
    }

    /// Audio that can never be written (no video around it) is dropped, so a
    /// recording without a picture can't eat memory.
    fn trim_stale_audio(&mut self) {
        let limit = match self.gops.front() {
            Some(g) => self.frame_pos(g.start) - 2 * crate::mf::aac::FRAME as i64,
            None => self.frame_pos(now_hns()) - 48_000 * 10,
        };
        for q in self.audio.values_mut() {
            while q.front().map(|p| p.pos < limit).unwrap_or(false) {
                q.pop_front();
            }
        }
        while self.peaks.front().map(|p| p.0 < limit).unwrap_or(false) {
            self.peaks.pop_front();
        }
        while self.slot_changes.len() > 1 && self.slot_changes[1].0 < limit {
            let (_, s) = self.slot_changes.pop_front().unwrap();
            self.slots = s;
        }
    }

    fn write_ready(&mut self, finishing: bool) {
        loop {
            let ready_end = if self.gops.len() >= 2 {
                Some(self.gops[1].start)
            } else if (finishing || self.video_ended) && self.gops.len() == 1 {
                let g = &self.gops[0];
                Some(g.packets.last().map(|p| p.time_hns).unwrap_or(g.start) + self.frame_hns())
            } else {
                None
            };
            let Some(end) = ready_end else { break };
            let audio_ready = self.watermark >= self.frame_pos(end) || now_hns() - end > AUDIO_WAIT_HNS;
            if !finishing && !audio_ready {
                break; // audio hasn't caught up yet
            }
            let gop = self.gops.pop_front().unwrap();
            if self.retry_at.map(|t| std::time::Instant::now() < t).unwrap_or(false) {
                continue; // recently failed to write: drop this stretch
            }
            if let Err(e) = self.write_gop(gop, end) {
                tracing::error!("writing recording: {e:#}");
                let _ = self.sh.events.send(RecEvent::Warning(format!("Recording write failed: {e:#}")));
                self.retry_at = Some(std::time::Instant::now() + Duration::from_secs(30));
                if let Some(seg) = self.seg.take() {
                    self.finish_segment(seg);
                }
            } else {
                self.retry_at = None;
            }
        }
    }

    /// Write queued app-track assignment changes that happen before `upto`.
    fn write_slot_changes(&mut self, upto_frames: i64) -> Result<()> {
        while self.slot_changes.front().map(|c| c.0 < upto_frames).unwrap_or(false) {
            let (pos, slots) = self.slot_changes.pop_front().unwrap();
            let seg = self.seg.as_mut().unwrap();
            let at_s = ((pos - seg.start_frames) as f64 / 48_000.0).max(0.0);
            let meta = SegmentMeta { at_s, slots: slots.clone(), ..Default::default() };
            seg.w.write_meta(&serde_json::to_vec(&meta)?)?;
            self.slots = slots;
        }
        Ok(())
    }

    fn write_gop(&mut self, gop: Gop, end: i64) -> Result<()> {
        let first = &gop.packets[0];
        let cfg = self.sh.cfg.lock().clone();
        // Roll over to a new file? (length reached, a pause, or new video settings)
        let roll = match &self.seg {
            Some(s) => {
                gop.start - s.start_hns >= cfg.segment_secs as i64 * 10_000_000 - self.frame_hns() / 2
                    || gop.start - s.end_hns > GAP_HNS
                    || (first.width, first.height) != s.size
                    || first.sps.as_ref().map(|sps| crate::h264::build_avcc(sps, first.pps.as_deref().unwrap_or(&[])) != s.avcc).unwrap_or(false)
            }
            None => true,
        };
        if roll {
            let mut continuous_start = None;
            if let Some(seg) = self.seg.take() {
                if seg.end_hns == gop.start {
                    continuous_start = Some(seg.start_ms + (gop.start - seg.start_hns) / 10_000);
                }
                self.finish_segment(seg);
            }
            // Assignments that took effect before this file starts go in its header.
            let start_frames = self.frame_pos(gop.start);
            while self.slot_changes.front().map(|c| c.0 <= start_frames).unwrap_or(false) {
                self.slots = self.slot_changes.pop_front().unwrap().1;
            }
            self.seg = Some(self.open_segment(&gop, &cfg, continuous_start)?);
        }
        let end_frames = self.frame_pos(end);
        self.write_slot_changes(end_frames)?;
        let seg_start_hns = self.seg.as_ref().unwrap().start_hns;
        let seg_start_frames = self.seg.as_ref().unwrap().start_frames;
        let fh = self.frame_hns();
        // Video: durations from rounded absolute positions, so they add up exactly.
        let mut vs: Vec<SampleRef> = Vec::with_capacity(gop.packets.len());
        for (i, p) in gop.packets.iter().enumerate() {
            let mut next = gop.packets.get(i + 1).map(|n| n.time_hns).unwrap_or(end);
            if next - p.time_hns > 10_000_000 {
                next = p.time_hns + fh; // never stretch a frame over a pause
            }
            let dur = (ticks90(next - seg_start_hns) - ticks90(p.time_hns - seg_start_hns)).max(1) as u32;
            vs.push(SampleRef { data: &p.data, duration: dur, sync: p.key, cts: 0 });
        }
        let mut runs = vec![TrackRun { track_id: TRACK_VIDEO, base_time: ticks90(gop.start - seg_start_hns).max(0) as u64, samples: &vs }];
        // Audio: this interval's packets, split where a track has gaps.
        let from_frames = (self.frame_pos(gop.start) - 2 * crate::mf::aac::FRAME as i64).max(seg_start_frames);
        let mut taken: Vec<(u32, Vec<AudioPacket>)> = Vec::new();
        for (track, q) in self.audio.iter_mut() {
            let id = match track {
                AudioTrack::Full => TRACK_FULL,
                AudioTrack::Mic => TRACK_MIC,
                AudioTrack::Slot(s) => FIRST_SLOT_TRACK + s,
            };
            let mut v: Vec<AudioPacket> = Vec::new();
            while q.front().map(|p| p.pos < end_frames).unwrap_or(false) {
                let p = q.pop_front().unwrap();
                // Skip audio from before this file / from a pause, and overlaps.
                if p.pos >= from_frames && v.last().map(|l| p.pos >= l.pos + crate::mf::aac::FRAME as i64).unwrap_or(true) {
                    v.push(p);
                }
            }
            if !v.is_empty() {
                taken.push((id, v));
            }
        }
        taken.sort_by_key(|(id, _)| *id);
        let mut audio_samples: Vec<(u32, u64, Vec<SampleRef>)> = Vec::new();
        for (id, packets) in &taken {
            let mut run_start = 0;
            for i in 1..=packets.len() {
                let contiguous = i < packets.len() && packets[i].pos == packets[i - 1].pos + crate::mf::aac::FRAME as i64;
                if !contiguous {
                    let samples: Vec<SampleRef> = packets[run_start..i].iter().map(|p| SampleRef { data: &p.data, duration: crate::mf::aac::FRAME as u32, sync: true, cts: 0 }).collect();
                    audio_samples.push((*id, (packets[run_start].pos - seg_start_frames) as u64, samples));
                    run_start = i;
                }
            }
        }
        for (id, base, samples) in &audio_samples {
            runs.push(TrackRun { track_id: *id, base_time: *base, samples });
        }
        let seg = self.seg.as_mut().unwrap();
        seg.w.write_fragment(&runs)?;
        seg.w.flush()?;
        seg.end_hns = end;
        // Waveform values for what was just written.
        let n = seg.wave.tracks.max(1);
        while self.peaks.front().map(|p| p.0 < end_frames).unwrap_or(false) {
            let (pos, levels) = self.peaks.pop_front().unwrap();
            let idx = (pos - seg.start_frames).div_euclid(WAVE_STEP);
            if idx < 0 {
                continue;
            }
            let idx = idx as usize;
            let need = (idx + 1) * n;
            if seg.wave.peaks.len() < need {
                seg.wave.peaks.resize(need, 0);
            }
            for (k, v) in levels.iter().take(n).enumerate() {
                let slot = &mut seg.wave.peaks[idx * n + k];
                *slot = (*slot).max(*v);
            }
        }
        if seg.wave_saved.elapsed() >= Duration::from_secs(5) {
            seg.wave_saved = std::time::Instant::now();
            let _ = seg.wave.save(&cv_core::waveform::wave_path(&seg.path));
        }
        let written_ms = seg.start_ms + (end - seg.start_hns) / 10_000;
        {
            let mut st = self.sh.status.lock();
            st.bytes = seg.w.bytes_written();
            st.written_until_ms = written_ms;
        }
        self.waiters.retain(|(after, reply)| {
            if end >= *after {
                let _ = reply.send(written_ms);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    fn open_segment(&mut self, gop: &Gop, cfg: &RecorderConfig, continuous_start: Option<i64>) -> Result<Segment> {
        let first = &gop.packets[0];
        let sps = first.sps.clone().context("keyframe without SPS")?;
        let pps = first.pps.clone().context("keyframe without PPS")?;
        let avcc = crate::h264::build_avcc(&sps, &pps);
        // An ordinary rotation shares the previous segment's endpoint. A new
        // capture or a discontinuity anchors its first frame to the wall clock.
        let unix_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let start_ms = continuous_start.unwrap_or_else(|| unix_now - (now_hns() - gop.start) / 10_000);
        std::fs::create_dir_all(&cfg.folder)?;
        let mut path = cfg.folder.join(segment_name(start_ms));
        let mut n = 2;
        while path.exists() {
            path = cfg.folder.join(segment_name(start_ms).replace(".mp4", &format!(" ({n}).mp4")));
            n += 1;
        }
        let audio_spec = |id: u32, name: String, kbps: u32, enabled: bool| TrackSpec {
            id,
            timescale: 48_000,
            codec: Codec::Aac { asc: crate::aac::asc_lc(48_000, 2), channels: 2, sample_rate: 48_000, avg_bitrate: crate::mf::aac::nearest_bitrate(kbps) * 1000 },
            width: 0,
            height: 0,
            name,
            enabled,
            default_duration: 1024,
        };
        let mut tracks = vec![
            TrackSpec {
                id: TRACK_VIDEO,
                timescale: 90_000,
                codec: Codec::H264 { avcc: avcc.clone() },
                width: first.width as u16,
                height: first.height as u16,
                name: "Video".into(),
                enabled: true,
                default_duration: 90_000 / cfg.fps.max(1),
            },
            audio_spec(TRACK_FULL, "Full mix".into(), cfg.audio_kbps, true),
            audio_spec(TRACK_MIC, "Microphone".into(), cfg.audio_kbps, false),
        ];
        let mut metas = vec![
            TrackMeta { id: TRACK_VIDEO, role: "video".into(), slot: None, app: None, name: "Video".into() },
            TrackMeta { id: TRACK_FULL, role: "full".into(), slot: None, app: None, name: "Full mix".into() },
            TrackMeta { id: TRACK_MIC, role: "mic".into(), slot: None, app: None, name: "Microphone".into() },
        ];
        if cfg.record_apps {
            for s in 0..cfg.app_slots {
                tracks.push(audio_spec(FIRST_SLOT_TRACK + s, format!("App {}", s + 1), cfg.app_kbps, false));
                metas.push(TrackMeta { id: FIRST_SLOT_TRACK + s, role: "slot".into(), slot: Some(s), app: None, name: String::new() });
            }
        }
        let meta = SegmentMeta { at_s: 0.0, start_ms: Some(start_ms), tracks: metas, slots: self.slots.clone() };
        let w = match FragWriter::create(&path, &tracks, (start_ms / 1000).max(0) as u64, Some(&serde_json::to_vec(&meta)?)) {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&path); // don't leave empty files behind
                return Err(e).with_context(|| format!("creating {}", path.display()));
            }
        };
        tracing::info!("recording to {}", path.display());
        {
            let mut st = self.sh.status.lock();
            st.path = Some(path.clone());
            st.segment_start_ms = start_ms;
            st.bytes = 0;
        }
        let _ = self.sh.events.send(RecEvent::SegmentStarted { path: path.clone(), start_ms });
        let n_audio = 2 + if cfg.record_apps { cfg.app_slots as usize } else { 0 };
        let wave = cv_core::waveform::Waveform { tracks: n_audio, rate: cv_core::waveform::PEAKS_PER_SEC, peaks: Vec::new() };
        Ok(Segment { w, path, start_hns: gop.start, start_frames: self.frame_pos(gop.start), start_ms, avcc, size: (first.width, first.height), end_hns: gop.start, wave, wave_saved: std::time::Instant::now() })
    }

    fn finish_segment(&mut self, seg: Segment) {
        let duration_s = (seg.end_hns - seg.start_hns) as f64 / 1e7;
        let path = seg.path.clone();
        let _ = seg.wave.save(&cv_core::waveform::wave_path(&path));
        match seg.w.finish() {
            Ok(bytes) => {
                let _ = self.sh.events.send(RecEvent::SegmentFinished { path, start_ms: seg.start_ms, duration_s, bytes });
            }
            Err(e) => tracing::error!("closing {}: {e:#}", path.display()),
        }
        self.sh.status.lock().path = None;
    }
}

/// For tests and tools: wall-clock span of a finished recording file.
pub fn file_span(path: &Path) -> Result<(i64, f64)> {
    let r = crate::mp4::reader::Mp4Reader::open(path)?;
    let meta = parse_meta(&r.metas);
    let start = meta.first().and_then(|m| m.start_ms).unwrap_or(0);
    Ok((start, r.duration_secs()))
}

#[allow(dead_code)]
fn _assert_send() {
    fn s<T: Send>() {}
    s::<Recorder>();
    let _ = frames_to_hns(0);
}
