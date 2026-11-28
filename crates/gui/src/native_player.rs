//! Asynchronous native H.264/AAC preview with a WASAPI playback clock.

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use cv_core::mixer::{limit, GainCurve, MixSettings};
use cv_core::model::AudioEdit;
use cv_media::audioread::{TrackPcm, OUT_RATE};
use cv_media::mf::h264dec::{DecodedFrame, VideoDecoder};
use cv_media::mp4::reader::{Mp4Reader, Sample};
use cv_media::mp4::Codec;
use cv_media::probe::{mix_gains, probe_reader, MediaInfo};
use eframe::egui;
use parking_lot::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

const TICK: Duration = Duration::from_millis(8);
const REFRESH: Duration = Duration::from_millis(200);
const RATE: f64 = OUT_RATE as f64;
const VIDEO_CLOCK: f64 = 10_000_000.0;

#[cfg(test)]
mod tests;

#[derive(Clone, PartialEq)]
pub struct PreviewMix {
    pub mix: MixSettings,
    pub edits: Vec<AudioEdit>,
    pub edit_offset: f64,
    pub fade: f64,
    pub overrides: BTreeMap<String, String>,
}

impl Default for PreviewMix {
    fn default() -> Self {
        Self {
            mix: MixSettings::default(),
            edits: Vec::new(),
            edit_offset: 0.0,
            fade: cv_core::mixer::DEFAULT_FADE_S,
            overrides: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct NativeState {
    pub loaded: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    pub ended: bool,
    pub first_frame: bool,
    pub last_frame: bool,
    pub next_frame_pos: Option<f64>,
    pub error: Option<String>,
    pub audio_tracks: usize,
    pub info: Option<Arc<MediaInfo>>,
}

pub struct VideoFrame {
    pub serial: u64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

enum Command {
    Load(u64, PathBuf, f64, bool, bool),
    Seek(f64, bool),
    Step(bool),
    Latest,
    Pause(bool),
    Speed(f64),
    Volume(f64),
    Mix(PreviewMix),
    Clear(u64),
}

#[derive(Default)]
struct View {
    generation: u64,
    state: NativeState,
    frame: Option<Arc<VideoFrame>>,
}

impl View {
    fn publish(&mut self, generation: u64, state: &NativeState, frame: Option<VideoFrame>) -> bool {
        if self.generation != generation {
            return false;
        }
        self.state = state.clone();
        if let Some(frame) = frame {
            self.frame = Some(Arc::new(frame));
        }
        true
    }
}

pub struct NativePlayer {
    tx: Sender<Command>,
    view: Arc<Mutex<View>>,
}

impl NativePlayer {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let view = Arc::new(Mutex::new(View::default()));
        let worker_view = view.clone();
        if let Err(e) = std::thread::Builder::new().name("clipvault-playback".into())
            .spawn(move || run(rx, worker_view, ctx))
        {
            view.lock().state.error = Some(format!("Can't start the playback thread: {e}"));
        }
        Self { tx, view }
    }

    pub fn load(&self, path: PathBuf, start: f64, live: bool, paused: bool) {
        self.reset_and_send(paused, |generation| Command::Load(generation, path, start, live, paused));
    }

    pub fn seek(&self, t: f64, exact: bool) {
        let _ = self.tx.send(Command::Seek(t, exact));
    }

    pub fn step(&self, forward: bool) {
        let _ = self.tx.send(Command::Step(forward));
    }

    pub fn seek_latest(&self) {
        let _ = self.tx.send(Command::Latest);
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.tx.send(Command::Pause(paused));
    }

    pub fn set_speed(&self, speed: f64) {
        if speed.is_finite() {
            let _ = self.tx.send(Command::Speed(speed.clamp(0.25, 4.0)));
        }
    }

    pub fn set_volume(&self, volume: f64) {
        if volume.is_finite() {
            let _ = self.tx.send(Command::Volume(volume.clamp(0.0, 1.0)));
        }
    }

    pub fn set_mix(&self, mix: PreviewMix) {
        let _ = self.tx.send(Command::Mix(mix));
    }

    pub fn clear(&self) {
        self.reset_and_send(true, Command::Clear);
    }

    fn reset_and_send(&self, paused: bool, command: impl FnOnce(u64) -> Command) {
        // The UI updates its path synchronously. Do not expose old media
        // state or a stale texture under that next path while opening it.
        let mut view = self.view.lock();
        view.generation = view.generation.wrapping_add(1);
        view.frame = None;
        view.state = NativeState { paused, ..Default::default() };
        if self.tx.send(command(view.generation)).is_err() {
            view.state.error = Some("The native playback worker is unavailable".into());
        }
    }

    pub fn state(&self) -> NativeState {
        self.view.lock().state.clone()
    }

    pub fn frame(&self) -> Option<Arc<VideoFrame>> {
        self.view.lock().frame.clone()
    }
}

struct AudioOutput {
    client: IAudioClient,
    render: IAudioRenderClient,
    capacity: u32,
    started: bool,
}

impl AudioOutput {
    fn open() -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let endpoint = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: 2,
                nSamplesPerSec: OUT_RATE,
                nAvgBytesPerSec: OUT_RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &format,
                None,
            )?;
            Ok(Self {
                render: client.GetService()?,
                capacity: client.GetBufferSize()?,
                client,
                started: false,
            })
        }
    }

    fn padding(&self) -> Result<u32> {
        Ok(unsafe { self.client.GetCurrentPadding()? })
    }

    fn write(&mut self, pcm: &[f32]) -> Result<()> {
        let frames = (pcm.len() / 2) as u32;
        unsafe {
            let ptr = self.render.GetBuffer(frames)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), ptr, pcm.len() * 4);
            self.render.ReleaseBuffer(frames, 0)?;
            if !self.started {
                self.client.Start()?;
                self.started = true;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        unsafe {
            if self.started {
                let _ = self.client.Stop();
                self.started = false;
            }
            let _ = self.client.Reset();
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.reset();
    }
}

struct AudioTrack {
    index: usize,
    edit_frames: i64,
    pcm: Option<TrackPcm>,
    broken: bool,
    read_until: i64,
    tail: [f32; 8],
}

struct Media {
    reader: Mp4Reader,
    video: usize,
    decoder: VideoDecoder,
    next_dts: u64,
    samples: VecDeque<Sample>,
    pictures: VecDeque<DecodedFrame>,
    drained: bool,
    audio: Vec<AudioTrack>,
    curves: Vec<GainCurve>,
    info: Arc<MediaInfo>,
    duration: f64,
    live: bool,
    refreshed: Instant,
    seek_picture: bool,
    frame_starts: Vec<i64>,
    first_frame: i64,
    last_frame: i64,
    shown_frame: Option<i64>,
    mixed: Vec<f32>,
    input: Vec<f32>,
    gains: Vec<f32>,
}

impl Media {
    fn open(path: PathBuf, live: bool, mix: &PreviewMix) -> Result<Self> {
        let reader = Mp4Reader::open(&path)?;
        let video = reader.video().context("This file has no supported H.264 video")?;
        let Codec::H264 { avcc } = &reader.tracks[video].codec else {
            anyhow::bail!("This file has no supported H.264 video");
        };
        // Decode to CPU memory because the renderer uploads RGBA textures to OpenGL.
        let decoder = VideoDecoder::new(avcc, None, reader.is_fragmented())?;
        let audio = reader.audio().iter().map(|&index| AudioTrack {
            index,
            edit_frames: (reader.tracks[index].edit_start_s() * RATE).round() as i64,
            pcm: None,
            broken: false,
            read_until: 0,
            tail: [0.0; 8],
        }).collect();
        let duration = reader.duration_secs();
        let info = Arc::new(probe_reader(&reader));
        let frame_starts = reader.frame_starts_near(video, 0)?;
        let first_frame = frame_starts.first().copied().unwrap_or(0);
        let last_frame = if reader.is_fragmented() {
            reader.frame_starts_near(video, i64::MAX)?.last().copied()
        } else {
            frame_starts.last().copied()
        }.unwrap_or(first_frame);
        let mut media = Self {
            reader, video, decoder, next_dts: 0, audio, curves: Vec::new(), info, duration, live,
            samples: VecDeque::new(), pictures: VecDeque::new(), drained: false,
            refreshed: Instant::now(), seek_picture: true,
            frame_starts, first_frame, last_frame, shown_frame: None,
            mixed: Vec::new(), input: Vec::new(), gains: Vec::new(),
        };
        media.update_curves(mix);
        Ok(media)
    }

    fn update_curves(&mut self, preview: &PreviewMix) {
        let PreviewMix { mix, edits, edit_offset, fade, overrides } = preview;
        self.curves = mix_gains(&self.info, mix, edits, *edit_offset, *fade, overrides);
    }

    fn refresh(&mut self, mix: &PreviewMix) -> Result<()> {
        if self.live && self.refreshed.elapsed() >= REFRESH {
            self.refreshed = Instant::now();
            if self.reader.refresh()? {
                self.duration = self.reader.duration_secs();
                let latest = self.reader.frame_starts_near(self.video, i64::MAX)?;
                if let Some(&last) = latest.last() {
                    self.last_frame = last;
                }
                if self.frame_starts.is_empty() {
                    self.first_frame = latest.first().copied().unwrap_or(0);
                    self.frame_starts = latest;
                }
                self.info = Arc::new(probe_reader(&self.reader));
                self.drained = false;
                self.update_curves(mix);
                // The PCM reader caches the track end and sample lookup window.
                for track in &mut self.audio {
                    track.pcm = None;
                }
            }
        }
        Ok(())
    }

    fn frame_seconds(&self, ticks: i64) -> f64 {
        ticks as f64 / f64::from(self.reader.tracks[self.video].timescale)
    }

    fn load_frame_window(&mut self, at: i64) -> Result<()> {
        if self.frame_starts.first().is_none_or(|&t| at <= t) || self.frame_starts.last().is_none_or(|&t| at >= t) {
            if self.reader.is_fragmented() {
                self.frame_starts = self.reader.frame_starts_near(self.video, at)?;
            }
        }
        Ok(())
    }

    fn neighbor(&mut self, at: i64, forward: bool) -> Result<i64> {
        self.load_frame_window(at)?;
        let before = self.frame_starts.partition_point(|&t| t < at);
        let after = self.frame_starts.partition_point(|&t| t <= at);
        let target = if forward {
            self.frame_starts.get(after).copied().unwrap_or(self.last_frame)
        } else {
            before.checked_sub(1).and_then(|i| self.frame_starts.get(i).copied()).unwrap_or(self.first_frame)
        };
        Ok(target)
    }

    fn step(&mut self, position: f64, forward: bool) -> Result<f64> {
        let scale = f64::from(self.reader.tracks[self.video].timescale);
        let at = if let Some(shown) = self.shown_frame {
            shown
        } else {
            // A seek and a frame shortcut can arrive in the same input batch.
            // Resolve the image under the seek cursor before advancing from it.
            let ticks = (position * scale).round() as i64;
            self.load_frame_window(ticks)?;
            let after = self.frame_starts.partition_point(|&t| t <= ticks);
            after.checked_sub(1).and_then(|i| self.frame_starts.get(i).copied()).unwrap_or(self.first_frame)
        };
        let target = self.neighbor(at, forward)?;
        let seconds = self.frame_seconds(target);
        if forward && !self.seek_picture && target > at {
            // The decoder is already positioned after the displayed image.
            // Consume its next picture without replaying from the previous keyframe.
            self.seek_picture = true;
            self.shown_frame = None;
            return Ok(seconds);
        }
        self.seek(seconds, true)
    }

    fn seek(&mut self, t: f64, exact: bool) -> Result<f64> {
        let target = if t.is_finite() { t.clamp(0.0, self.duration) } else { 0.0 };
        let track = &self.reader.tracks[self.video];
        let ticks = ((target + track.edit_start_s()) * track.timescale as f64).round() as u64;
        let key = self.reader.sync_at_or_before(self.video, ticks)?;
        self.next_dts = key.map_or(0, |s| s.dts);
        let position = match (exact, key) {
            (false, Some(s)) => (s.pts() as f64 / track.timescale as f64 - track.edit_start_s())
                .clamp(0.0, self.duration),
            _ => target,
        };
        self.samples.clear();
        self.pictures.clear();
        self.shown_frame = None;
        self.decoder.flush();
        self.drained = false;
        self.seek_picture = true;
        for track in &mut self.audio {
            track.pcm = None;
        }
        Ok(position)
    }

    fn next_sample(&mut self) -> Result<Option<Sample>> {
        if self.samples.is_empty() {
            let end = self.reader.end_ticks(self.video);
            if self.next_dts >= end {
                return Ok(None);
            }
            let window = u64::from(self.reader.tracks[self.video].timescale);
            self.samples = self.reader.samples(self.video, self.next_dts, self.next_dts + window)?.into();
            if self.samples.is_empty() {
                if let Some((start, _)) = self.reader.coverage(self.video).into_iter()
                    .find(|(start, _)| *start > self.next_dts)
                {
                    self.next_dts = start;
                    self.samples = self.reader.samples(self.video, start, start + window)?.into();
                }
            }
        }
        Ok(self.samples.pop_front())
    }

    fn picture(&mut self, position: f64) -> Result<Option<DecodedFrame>> {
        let track = &self.reader.tracks[self.video];
        let timescale = track.timescale as f64;
        let edit = track.edit_start_s();
        let target = (position.min((self.duration - 0.00001).max(0.0)) * VIDEO_CLOCK).round() as i64;
        let mut frame = None;
        // Decode a bounded batch so rapid scrubbing can supersede an expensive seek.
        for _ in 0..12 {
            while self.pictures.front().is_some_and(|f| f.pts_hns <= target) {
                frame = self.pictures.pop_front();
            }
            if self.pictures.front().is_some() {
                if frame.is_some() || self.seek_picture {
                    self.seek_picture = false;
                    return Ok(frame.or_else(|| self.pictures.pop_front()));
                }
                break;
            }
            let Some(sample) = self.next_sample()? else {
                if !self.drained {
                    // A recording's next complete fragment starts with a keyframe.
                    // Release decoder look-ahead at the current live edge.
                    self.drained = true;
                    self.pictures.extend(self.decoder.drain()?);
                    continue;
                }
                if frame.is_some() {
                    self.seek_picture = false;
                }
                break;
            };
            self.next_dts = sample.end().max(sample.dts.saturating_add(1));
            let pts = ((sample.pts() as f64 / timescale - edit) * VIDEO_CLOCK).round() as i64;
            self.pictures.extend(self.decoder.decode(&self.reader.read(&sample)?, pts)?);
            // For native live files, the decoder sees a continuous stream with no B-frames.
            // A sample ahead of playback may already be decoded and waiting in pictures.
        }
        if self.seek_picture {
            // Preserve the last decoded frame across ticks of a long seek.
            if let Some(f) = frame.take() {
                self.pictures.push_front(f);
            }
        }
        Ok(frame)
    }

    fn pcm(&mut self, first: f64, speed: f64, frames: usize, volume: f32) -> Result<Vec<f32>> {
        let from = first.floor() as i64;
        let count = (first - from as f64 + frames as f64 * speed).ceil() as usize + 2;
        let (mixed, input, gains) = (&mut self.mixed, &mut self.input, &mut self.gains);
        mixed.resize(count * 2, 0.0);
        mixed.fill(0.0);
        input.resize(count * 2, 0.0);
        gains.resize(count, 0.0);
        for (track, curve) in self.audio.iter_mut().zip(&self.curves) {
            if track.broken || curve.is_silent() {
                continue;
            }
            let pos = from.saturating_add(track.edit_frames);
            let mut overlap = 0;
            if track.pcm.is_none() {
                let reader = TrackPcm::new(&self.reader, track.index);
                match reader {
                    Ok(pcm) => track.pcm = Some(pcm),
                    Err(e) => {
                        tracing::warn!("Playback audio track {}: {e:#}", track.index);
                        track.broken = true;
                        continue;
                    }
                }
            } else if (1..=4).contains(&(track.read_until - pos)) {
                overlap = (track.read_until - pos) as usize;
                input[..overlap * 2].copy_from_slice(&track.tail[8 - overlap * 2..]);
            }
            let overlap = overlap.min(count);
            track.pcm.as_mut().unwrap().read(&self.reader, pos + overlap as i64, &mut input[overlap * 2..])?;
            let keep = input.len().min(track.tail.len());
            track.tail[8 - keep..].copy_from_slice(&input[input.len() - keep..]);
            track.read_until = pos + count as i64;
            curve.fill(from as f64 / RATE, 1.0 / RATE, gains);
            for ((dst, src), g) in mixed.chunks_exact_mut(2).zip(input.chunks_exact(2)).zip(gains.iter()) {
                dst[0] += src[0] * g;
                dst[1] += src[1] * g;
            }
        }
        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let x = first - from as f64 + i as f64 * speed;
            let j = x.floor() as usize;
            let f = (x - j as f64) as f32;
            for ch in 0..2 {
                let a = mixed[2 * j + ch];
                let b = mixed[2 * (j + 1) + ch];
                out.push(limit(a + (b - a) * f) * volume);
            }
        }
        Ok(out)
    }
}

struct Playback {
    generation: u64,
    state: NativeState,
    media: Option<Media>,
    output: Option<AudioOutput>,
    next_audio: f64,
    last_tick: Instant,
    output_retry: Instant,
    speed: f64,
    volume: f32,
    mix: PreviewMix,
    serial: u64,
}

impl Playback {
    fn new() -> Self {
        Self {
            generation: 0,
            state: NativeState { paused: true, ..Default::default() },
            media: None, output: None, next_audio: 0.0, last_tick: Instant::now(),
            output_retry: Instant::now(), speed: 1.0, volume: 1.0,
            mix: PreviewMix::default(), serial: 0,
        }
    }

    fn reset_audio(&mut self) {
        if let Some(output) = &mut self.output {
            output.reset();
        }
        self.next_audio = self.state.position * RATE;
        self.last_tick = Instant::now();
    }

    fn command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Load(generation, path, start, live, paused) => {
                self.generation = generation;
                self.media = None;
                self.state = NativeState { paused, ..Default::default() };
                self.reset_audio();
                let mut media = Media::open(path, live, &self.mix)?;
                let start = if start == f64::MAX { media.frame_seconds(media.last_frame) } else { start };
                self.state.position = media.seek(start, true)?;
                self.state.duration = media.duration;
                self.state.audio_tracks = media.audio.len();
                self.state.info = Some(media.info.clone());
                self.state.loaded = true;
                self.media = Some(media);
                self.reset_audio();
            }
            Command::Seek(position, exact) => {
                if let Some(media) = &mut self.media {
                    self.state.position = media.seek(position, exact)?;
                    self.state.ended = false;
                    self.reset_audio();
                }
            }
            Command::Step(forward) => {
                if let Some(media) = &mut self.media {
                    self.state.position = media.step(self.state.position, forward)?;
                    self.state.ended = false;
                    self.state.paused = true;
                    self.reset_audio();
                }
            }
            Command::Latest => {
                if let Some(media) = &mut self.media {
                    if media.live {
                        media.refreshed = Instant::now() - REFRESH;
                        media.refresh(&self.mix)?;
                    }
                    self.state.position = media.seek(media.frame_seconds(media.last_frame), true)?;
                    self.state.ended = false;
                    self.reset_audio();
                }
            }
            Command::Pause(paused) => {
                if self.state.paused != paused {
                    if !paused && self.state.ended {
                        if let Some(media) = &mut self.media {
                            self.state.position = media.seek(0.0, true)?;
                        }
                        self.state.ended = false;
                    }
                    self.state.paused = paused;
                    if paused {
                        if let Some(media) = &mut self.media {
                            if let Some(shown) = media.shown_frame {
                                let next = media.neighbor(shown, true)?;
                                self.state.next_frame_pos = (next > shown).then(|| media.frame_seconds(next));
                            }
                        }
                    }
                    self.reset_audio();
                }
            }
            Command::Speed(speed) => {
                if self.speed != speed {
                    self.speed = speed;
                    self.reset_audio();
                }
            }
            Command::Volume(volume) => {
                self.volume = volume as f32;
            }
            Command::Mix(mix) if self.mix != mix => {
                self.mix = mix;
                if let Some(media) = &mut self.media {
                    media.update_curves(&self.mix);
                }
                self.reset_audio();
            }
            Command::Mix(_) => {}
            Command::Clear(generation) => {
                self.generation = generation;
                self.media = None;
                self.state = NativeState { paused: true, ..Default::default() };
                self.reset_audio();
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<Option<VideoFrame>> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let Some(media) = &mut self.media else { return Ok(None) };
        media.refresh(&self.mix)?;
        self.state.duration = media.duration;
        self.state.info = Some(media.info.clone());
        if let Some(shown) = media.shown_frame {
            self.state.last_frame = shown >= media.last_frame;
        }
        let playing = !self.state.paused && !media.seek_picture;
        if playing && !media.audio.is_empty() && self.output.is_none() && now >= self.output_retry {
            self.output_retry = now + Duration::from_secs(2);
            match AudioOutput::open() {
                Ok(output) => {
                    self.next_audio = self.state.position * RATE;
                    self.output = Some(output);
                    self.state.error = None;
                }
                Err(e) => {
                    self.state.error = Some(format!("Audio output unavailable: {e}"));
                }
            }
        }
        if playing {
            let updated = if let Some(out) = &mut self.output {
                if media.audio.is_empty() {
                    Ok(None)
                } else {
                    (|| -> Result<Option<f64>> {
                        let padding = out.padding()?;
                        let audible = (self.next_audio - padding as f64 * self.speed) / RATE;
                        let desired = out.capacity.min(OUT_RATE * 45 / 1000);
                        let remaining = ((media.duration * RATE - self.next_audio) / self.speed)
                            .max(0.0).ceil() as u32;
                        let frames = desired.saturating_sub(padding).min(remaining);
                        if frames >= OUT_RATE / 200 || (frames > 0 && remaining == frames) {
                            let pcm = media.pcm(self.next_audio, self.speed, frames as usize, self.volume)?;
                            out.write(&pcm)?;
                            self.next_audio += frames as f64 * self.speed;
                        }
                        Ok(Some(audible))
                    })()
                }
            } else {
                Ok(None)
            };
            let pos = match updated {
                Ok(Some(audible)) => audible,
                Ok(None) => self.state.position + elapsed * self.speed,
                Err(e) => {
                    self.state.error = Some(format!("Audio playback interrupted: {e:#}"));
                    self.output = None;
                    self.output_retry = now + Duration::from_secs(2);
                    self.state.position + elapsed * self.speed
                }
            };
            self.state.position = pos.clamp(self.state.position, media.duration.max(self.state.position));
            if !media.live && self.state.position >= media.duration {
                self.state.ended = true;
                self.state.paused = true;
                if let Some(out) = &mut self.output {
                    out.reset();
                }
            }
        }
        if !playing && !media.seek_picture {
            return Ok(None);
        }
        let Some(frame) = media.picture(self.state.position)? else { return Ok(None) };
        let pts = ((frame.pts_hns as f64 / VIDEO_CLOCK) * f64::from(media.reader.tracks[media.video].timescale)).round() as i64;
        media.shown_frame = Some(pts.max(0));
        self.state.next_frame_pos = if self.state.paused {
            let next = media.neighbor(pts.max(0), true)?;
            (next > pts).then(|| media.frame_seconds(next))
        } else {
            None
        };
        self.state.first_frame = pts <= media.first_frame;
        self.state.last_frame = pts >= media.last_frame;
        let width = frame.width as usize;
        let height = frame.height as usize;
        let rgba = rgba_from_nv12(&frame);
        self.serial += 1;
        Ok(Some(VideoFrame { serial: self.serial, width, height, rgba }))
    }
}

/// Write the texture pixels directly, using the same limited-range BT.709
/// matrix as thumbnails. Each pair of adjacent pixels shares chroma.
fn rgba_from_nv12(frame: &DecodedFrame) -> Vec<u8> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let (luma, chroma) = frame.nv12.split_at(width * height);
    let mut rgba = Vec::with_capacity(luma.len() * 4);
    let channel = |v: i32| (v >> 8).clamp(0, 255) as u8;
    for (row, y) in luma.chunks_exact(width).enumerate() {
        let uv = &chroma[row / 2 * width..][..width];
        for (y, uv) in y.chunks_exact(2).zip(uv.chunks_exact(2)) {
            let u = i32::from(uv[0]) - 128;
            let v = i32::from(uv[1]) - 128;
            let (r, g, b) = (459 * v, -55 * u - 136 * v, 541 * u);
            for &luma in y {
                let luma = 298 * (i32::from(luma) - 16);
                rgba.extend_from_slice(&[channel(luma + r), channel(luma + g), channel(luma + b), 255]);
            }
        }
    }
    rgba
}

fn run(rx: Receiver<Command>, view: Arc<Mutex<View>>, ctx: egui::Context) {
    cv_media::mf::init();
    let mut playback = Playback::new();
    let mut last_paint = Instant::now();
    loop {
        let active = playback.media.as_ref().is_some_and(|media| {
            !playback.state.paused || media.seek_picture
        });
        let received = if active {
            rx.recv_timeout(TICK)
        } else if playback.media.as_ref().is_some_and(|m| m.live) {
            rx.recv_timeout(REFRESH)
        } else {
            rx.recv().map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
        };
        let mut changed = false;
        match received {
            Ok(cmd) => {
                let mut pending = Some(cmd);
                // Coalesce rapid drag seeks while respecting the order of other operations.
                while let Ok(next) = rx.try_recv() {
                    let replace = matches!((&pending, &next), (Some(Command::Seek(..)), Command::Seek(..)));
                    if !replace {
                        if let Some(cmd) = pending.take() {
                            if let Err(e) = playback.command(cmd) {
                                playback.state.error = Some(format!("{e:#}"));
                            }
                        }
                    }
                    pending = Some(next);
                }
                if let Some(cmd) = pending {
                    if let Err(e) = playback.command(cmd) {
                        playback.state.error = Some(format!("{e:#}"));
                    }
                }
                changed = true;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
        #[cfg(test)]
        tests::WORKER_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let previous_info = playback.state.info.clone();
        let frame = match playback.tick() {
            Ok(frame) => frame,
            Err(e) => {
                playback.state.error = Some(format!("{e:#}"));
                playback.state.paused = true;
                playback.state.loaded = false;
                playback.media = None;
                playback.reset_audio();
                changed = true;
                None
            }
        };
        let refreshed = match (&previous_info, &playback.state.info) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        let repaint = changed || refreshed || frame.is_some() || (active && last_paint.elapsed() >= REFRESH);
        if changed || active || frame.is_some() || refreshed {
            if !view.lock().publish(playback.generation, &playback.state, frame) {
                continue;
            }
            if repaint {
                last_paint = Instant::now();
                ctx.request_repaint();
            }
        }
    }
}
