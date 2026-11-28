//! Audio side of the recorder: microphone + one capture per program that
//! plays sound, lined up by the hub, mixed into the "Full mix", and encoded.

use super::{AppIdent, AppNamer, AudioPacket, AudioTrack, Levels, MuxMsg, RecEvent, Shared, SlotInfo};
use crate::capture::audio::{AudioSource, Chunk, SourceKind, CHANNELS};
use crate::capture::hub::{Hub, BLOCK};
use crate::capture::sessions::{choose_targets, processes, sound_sessions, NEVER_CAPTURE};
use crate::clock::now_hns;
use crate::mf::aac::{AacEncoder, FRAME};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIC_ID: u32 = 1;
const LOOPBACK_ID: u32 = 2;
/// Below this peak a block counts as silent.
const SILENCE: f32 = 1.0e-4;
/// Give an app's track back after this long without sound.
const SLOT_IDLE_FRAMES: i64 = 48_000 * 600;
/// If the audio clock is this far behind real time (sleep/hibernate), jump ahead.
const MAX_LAG_FRAMES: i64 = 48_000 * 5;
const HUB_LATENCY_MS: u32 = 250;

/// One AAC track being encoded.
struct Enc {
    track: AudioTrack,
    kbps: u32,
    enc: Option<AacEncoder>,
    start: i64,
    out: i64,
    /// Frames to drop from the start of the next input (slot reuse alignment).
    skip: usize,
    errors: u32,
}

impl Enc {
    fn new(track: AudioTrack, kbps: u32, start: i64) -> anyhow::Result<Enc> {
        Ok(Enc { track, kbps, enc: Some(AacEncoder::new(48_000, 2, kbps)?), start, out: 0, skip: 0, errors: 0 })
    }

    fn next_pos(&self) -> i64 {
        self.start + self.out * FRAME as i64
    }

    /// Position up to which this track is complete (a broken encoder never
    /// holds the recording back).
    fn watermark(&self) -> i64 {
        if self.enc.is_some() {
            self.next_pos()
        } else {
            i64::MAX
        }
    }

    /// Encode one hub block that starts at `block_pos`.
    fn push(&mut self, block_pos: i64, pcm: &[f32], tx: &Sender<MuxMsg>) {
        let mut pcm = pcm;
        if self.skip > 0 {
            let k = self.skip.min(pcm.len() / CHANNELS);
            pcm = &pcm[k * CHANNELS..];
            self.skip -= k;
        }
        if pcm.is_empty() {
            return;
        }
        let Some(enc) = self.enc.as_mut() else {
            // Try to bring a failed encoder back, starting after this block.
            if self.errors < 1000 {
                if let Ok(e) = AacEncoder::new(48_000, 2, self.kbps) {
                    self.enc = Some(e);
                    self.start = block_pos + BLOCK as i64;
                    self.out = 0;
                }
            }
            return;
        };
        match enc.encode(pcm) {
            Ok(frames) => self.emit(frames, tx),
            Err(e) => {
                self.errors += 1;
                if self.errors < 5 || self.errors % 1000 == 0 {
                    tracing::warn!("audio encoder failed ({:?}): {e:#}", self.track);
                }
                // Start over at the next block so timing stays right.
                self.enc = None;
            }
        }
    }

    fn finish(mut self, tx: &Sender<MuxMsg>) -> i64 {
        if let Some(mut e) = self.enc.take() {
            if let Ok(frames) = e.finish() {
                self.emit(frames, tx);
            }
        }
        self.next_pos()
    }

    fn emit(&mut self, frames: Vec<Vec<u8>>, tx: &Sender<MuxMsg>) {
        for data in frames {
            let pos = self.next_pos();
            self.out += 1;
            let _ = tx.send(MuxMsg::Audio(AudioPacket { track: self.track, pos, data }));
        }
    }
}

struct App {
    source: AudioSource,
    root: u32,
    ident: AppIdent,
    slot: Option<u32>,
    last_sound: i64,
    peak: f32,
    delivered: bool,
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

pub(crate) fn run(sh: Arc<Shared>, tx: Sender<MuxMsg>, namer: Arc<dyn AppNamer>, mic_rx: Receiver<(Option<Option<String>>, f32)>) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(&sh, &tx, namer, mic_rx)));
    if r.is_err() {
        tracing::error!("audio recording crashed; continuing without sound");
        let _ = sh.events.send(RecEvent::Warning("Sound recording stopped because of an internal error".into()));
    }
    // Never let the video wait for audio that won't come.
    let _ = tx.send(MuxMsg::AudioWatermark(i64::MAX));
}

struct State {
    full: Enc,
    mic_enc: Enc,
    slots: Vec<Option<(u32, Enc)>>,
    /// Where each slot's last encoder ended (reuse must start after it).
    slot_end: Vec<i64>,
}

fn run_inner(sh: &Arc<Shared>, tx: &Sender<MuxMsg>, namer: Arc<dyn AppNamer>, mic_rx: Receiver<(Option<Option<String>>, f32)>) {
    crate::mf::init();
    crate::util::boost_thread();
    let cfg = sh.cfg.lock().clone();
    let (ctx, crx) = unbounded::<Chunk>();
    let mut hub = Hub::new(sh.epoch.hns, HUB_LATENCY_MS);
    let mut mic_choice = cfg.mic.clone();
    let mut mic_gain = cfg.mic_gain;
    let mut mic: Option<AudioSource> = mic_choice.clone().map(|d| AudioSource::start(MIC_ID, SourceKind::Mic(d), ctx.clone()));
    let mut mic_restart: Option<Instant> = None;
    let mut apps: HashMap<u32, App> = HashMap::new();
    let mut retry_after: HashMap<u32, Instant> = HashMap::new();
    let mut next_id = 100u32;
    let start_pos = hub.position();
    let (Ok(full), Ok(mic_enc)) = (Enc::new(AudioTrack::Full, cfg.audio_kbps, start_pos), Enc::new(AudioTrack::Mic, cfg.audio_kbps, start_pos)) else {
        tracing::error!("AAC encoder unavailable; recording without sound");
        let _ = sh.events.send(RecEvent::Warning("Sound can't be recorded: the AAC encoder is unavailable".into()));
        return;
    };
    let mut st = State { full, mic_enc, slots: (0..cfg.app_slots).map(|_| None).collect(), slot_end: vec![i64::MIN; cfg.app_slots as usize] };
    // Per-app capture needs Windows 10 2004+; otherwise capture the default
    // output device as one source.
    let mut loopback: Option<AudioSource> = None;
    let mut app_delivered = false;
    let mut app_failures = 0u32;
    let mut last_scan = Instant::now() - Duration::from_secs(10);
    let mut level_acc = Levels::default();
    // Waveform: per-track peak over the current 100 ms window.
    let n_wave = 2 + if cfg.record_apps { cfg.app_slots as usize } else { 0 };
    let mut wave_pos: Option<i64> = None;
    let mut wave_acc = vec![0.0f32; n_wave];
    // Hub blocks have a fixed format. Reuse the mix for the full recording
    // and borrow source samples when sending them to each independent encoder.
    let mut mix = [0.0f32; BLOCK * CHANNELS];
    let silence = [0.0f32; BLOCK * CHANNELS];
    let mut level_at = Instant::now();
    let own: Vec<u32> = cfg.exclude_pids.iter().copied().chain(std::iter::once(std::process::id())).collect();

    loop {
        let stopping = sh.stop.load(Ordering::SeqCst);
        // Microphone changes (mic guard) and recovery after unplugging.
        while let Ok((m, g)) = mic_rx.try_recv() {
            mic_gain = g;
            if m != mic_choice {
                mic_choice = m;
                drop(mic.take());
                hub.remove(MIC_ID);
                mic = mic_choice.clone().map(|d| AudioSource::start(MIC_ID, SourceKind::Mic(d), ctx.clone()));
            }
        }
        if mic.as_ref().map(|m| m.failed()).unwrap_or(false) {
            let due = mic_restart.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true);
            if due {
                mic_restart = Some(Instant::now());
                drop(mic.take());
                mic = mic_choice.clone().map(|d| AudioSource::start(MIC_ID, SourceKind::Mic(d), ctx.clone()));
            }
        }
        sh.status.lock().mic_ok = mic.as_ref().map(|m| !m.failed()).unwrap_or(false);

        // Which programs are playing sound?
        if !stopping && loopback.is_none() && last_scan.elapsed() >= Duration::from_millis(300) {
            last_scan = Instant::now();
            let sessions = sound_sessions();
            let procs = processes();
            let pids: Vec<u32> = sessions.iter().map(|s| s.pid).collect();
            let targets = choose_targets(&pids, &procs, &own, NEVER_CAPTURE, &crate::capture::sessions::process_created);
            retry_after.retain(|_, t| t.elapsed() < Duration::from_secs(30));
            // Stop captures whose program went away (or is now covered by another).
            let gone: Vec<u32> = apps.iter().filter(|(_, a)| !targets.iter().any(|t| t.root == a.root) || a.source.failed()).map(|(id, _)| *id).collect();
            let mut changed = false;
            for id in gone {
                if let Some(a) = apps.remove(&id) {
                    if a.source.failed() {
                        retry_after.insert(a.root, Instant::now());
                        if !a.delivered {
                            app_failures += 1;
                        }
                    }
                    hub.remove(id);
                    if let Some(s) = a.slot {
                        release_slot(&mut st, s, tx);
                        changed = true;
                    }
                }
            }
            if changed {
                announce(sh, tx, &st, &apps, hub.position());
            }
            for t in targets {
                if apps.values().any(|a| a.root == t.root) || retry_after.contains_key(&t.root) {
                    continue;
                }
                let id = next_id;
                next_id += 1;
                let ident = namer.identify(&t.exe, t.root);
                tracing::debug!("capturing {} ({}) pid {}", ident.name, t.exe, t.root);
                let source = AudioSource::start(id, SourceKind::App(t.root), ctx.clone());
                apps.insert(id, App { source, root: t.root, ident, slot: None, last_sound: 0, peak: 0.0, delivered: false });
            }
            if !app_delivered && app_failures >= 3 {
                tracing::warn!("per-app audio capture isn't working here; recording the default output device instead");
                apps.clear();
                loopback = Some(AudioSource::start(LOOPBACK_ID, SourceKind::DeviceLoopback(None), ctx.clone()));
            }
        }

        while let Ok(c) = crx.try_recv() {
            if let Some(a) = apps.get_mut(&c.source) {
                a.delivered = true;
                app_delivered = true;
                hub.push(c);
            } else if c.source == MIC_ID || c.source == LOOPBACK_ID {
                hub.push(c);
            }
        }
        // Woke from sleep (or stalled)? Don't produce hours of silence: jump ahead.
        let now_frame = hub.frame_of(now_hns());
        if !stopping && now_frame - hub.position() > MAX_LAG_FRAMES {
            let target = now_frame - (HUB_LATENCY_MS as i64 * 48);
            tracing::info!("audio clock jumped {:.1} s ahead (sleep?)", (target - hub.position()) as f64 / 48_000.0);
            hub.skip_to(target);
            let p = hub.position();
            let old_full = std::mem::replace(&mut st.full, Enc::new(AudioTrack::Full, cfg.audio_kbps, p).unwrap_or_else(|_| Enc { track: AudioTrack::Full, kbps: cfg.audio_kbps, enc: None, start: p, out: 0, skip: 0, errors: 0 }));
            old_full.finish(tx);
            let old_mic = std::mem::replace(&mut st.mic_enc, Enc::new(AudioTrack::Mic, cfg.audio_kbps, p).unwrap_or_else(|_| Enc { track: AudioTrack::Mic, kbps: cfg.audio_kbps, enc: None, start: p, out: 0, skip: 0, errors: 0 }));
            old_mic.finish(tx);
            let any = st.slots.iter().any(|s| s.is_some());
            for s in 0..st.slots.len() {
                release_slot(&mut st, s as u32, tx);
            }
            for a in apps.values_mut() {
                a.slot = None;
            }
            if any {
                announce(sh, tx, &st, &apps, p);
            }
        }
        let blocks = if stopping { hub.pull_until(hub.frame_of(now_hns())) } else { hub.pull(now_hns()) };
        for b in blocks {
            // Close the waveform window when this block starts a new one.
            let win = b.pos.div_euclid(4800) * 4800;
            if let Some(w) = wave_pos.filter(|w| *w != win) {
                let levels = wave_acc.iter().map(|v| cv_core::waveform::level_u8(*v)).collect();
                let _ = tx.send(MuxMsg::Peaks { pos: w, levels });
                wave_acc.iter_mut().for_each(|v| *v = 0.0);
            }
            wave_pos = Some(win);
            if let Some((_, mic_samples)) = b.sources.iter().find(|(id, _)| *id == MIC_ID) {
                for (out, sample) in mix.iter_mut().zip(mic_samples) {
                    *out = sample * mic_gain;
                }
            } else {
                mix.fill(0.0);
            }
            let mic_peak = peak(&mix);
            level_acc.mic = level_acc.mic.max(mic_peak);
            wave_acc[1] = wave_acc[1].max(mic_peak);
            st.mic_enc.push(b.pos, &mix, tx);
            // Apps.
            let mut assigned = false;
            for (id, v) in &b.sources {
                if *id == LOOPBACK_ID {
                    for (m, x) in mix.iter_mut().zip(v) {
                        *m += x;
                    }
                    continue;
                }
                let Some(app) = apps.get_mut(id) else { continue };
                for (m, x) in mix.iter_mut().zip(v) {
                    *m += x;
                }
                let p = peak(v);
                app.peak = app.peak.max(p);
                if p > SILENCE {
                    app.last_sound = b.pos;
                    if app.slot.is_none() && cfg.record_apps {
                        if let Some(free) = st.slots.iter().position(|s| s.is_none()) {
                            let start = b.pos.max(st.slot_end[free]);
                            match Enc::new(AudioTrack::Slot(free as u32), cfg.app_kbps, start) {
                                Ok(mut e) => {
                                    e.skip = (start - b.pos).max(0) as usize;
                                    st.slots[free] = Some((*id, e));
                                    app.slot = Some(free as u32);
                                    assigned = true;
                                    tracing::info!("{} gets app track {}", app.ident.name, free + 1);
                                }
                                Err(e) => tracing::warn!("app track encoder: {e:#}"),
                            }
                        }
                    }
                }
            }
            // App tracks (silence while an assigned app is quiet).
            for (i, s) in st.slots.iter_mut().enumerate() {
                let Some((app_id, enc)) = s else { continue };
                let pcm = b.sources.iter().find(|(id, _)| id == app_id).map(|(_, v)| v.as_slice()).unwrap_or(&silence);
                if let Some(w) = wave_acc.get_mut(2 + i) {
                    *w = w.max(peak(pcm));
                }
                enc.push(b.pos, pcm, tx);
            }
            if assigned {
                announce(sh, tx, &st, &apps, b.pos);
            }
            for m in mix.iter_mut() {
                *m = cv_core::mixer::limit(*m);
            }
            let full_peak = peak(&mix);
            level_acc.full = level_acc.full.max(full_peak);
            wave_acc[0] = wave_acc[0].max(full_peak);
            st.full.push(b.pos, &mix, tx);
            // Free tracks of apps that have been quiet for a long time.
            let idle: Vec<u32> = apps.values().filter(|a| a.slot.is_some() && b.pos - a.last_sound > SLOT_IDLE_FRAMES).filter_map(|a| a.slot).collect();
            if !idle.is_empty() {
                for s in idle {
                    release_slot(&mut st, s, tx);
                    if let Some(a) = apps.values_mut().find(|a| a.slot == Some(s)) {
                        a.slot = None;
                    }
                }
                announce(sh, tx, &st, &apps, b.pos);
            }
        }
        // Everything up to the slowest working encoder is complete.
        let mut wm = st.full.watermark().min(st.mic_enc.watermark());
        for (_, e) in st.slots.iter().flatten() {
            wm = wm.min(e.watermark());
        }
        if wm != i64::MAX {
            let _ = tx.send(MuxMsg::AudioWatermark(wm));
        }

        if level_at.elapsed() >= Duration::from_millis(100) {
            level_acc.apps = apps.values_mut().filter(|a| a.peak > SILENCE || a.slot.is_some()).map(|a| (a.ident.clone(), std::mem::take(&mut a.peak))).collect();
            sh.status.lock().levels = std::mem::take(&mut level_acc);
            level_at = Instant::now();
        }
        if stopping {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Flush encoders.
    st.full.finish(tx);
    st.mic_enc.finish(tx);
    for s in st.slots.into_iter().flatten() {
        s.1.finish(tx);
    }
    drop(mic);
    drop(loopback);
}

fn release_slot(st: &mut State, s: u32, tx: &Sender<MuxMsg>) {
    if let Some((_, enc)) = st.slots.get_mut(s as usize).and_then(|x| x.take()) {
        st.slot_end[s as usize] = enc.finish(tx);
    }
}

fn announce(sh: &Shared, tx: &Sender<MuxMsg>, st: &State, apps: &HashMap<u32, App>, pos: i64) {
    let list: Vec<SlotInfo> = st
        .slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().and_then(|(id, _)| apps.get(id)).map(|a| SlotInfo { slot: i as u32, app: a.ident.clone() }))
        .collect();
    let _ = tx.send(MuxMsg::Slots { pos, slots: list.clone() });
    let at_ms = sh.epoch.unix_ms + crate::capture::hub::frames_to_hns(pos) / 10_000;
    let _ = sh.events.send(RecEvent::Slots { at_ms, slots: list });
}
