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

/// One AAC track being encoded.
struct Enc {
    track: AudioTrack,
    enc: AacEncoder,
    start: i64,
    out: i64,
}

impl Enc {
    fn new(track: AudioTrack, kbps: u32, start: i64) -> anyhow::Result<Enc> {
        Ok(Enc { track, enc: AacEncoder::new(48_000, 2, kbps)?, start, out: 0 })
    }
    fn next_pos(&self) -> i64 {
        self.start + self.out * FRAME as i64
    }
    fn push(&mut self, pcm: &[f32], tx: &Sender<MuxMsg>) {
        match self.enc.encode(pcm) {
            Ok(frames) => self.emit(frames, tx),
            Err(e) => tracing::warn!("audio encoder: {e:#}"),
        }
    }
    fn finish(mut self, tx: &Sender<MuxMsg>) {
        if let Ok(frames) = self.enc.finish() {
            self.emit(frames, tx);
        }
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
}

/// Soft limiter so a loud mix can't clip harshly.
fn limit(v: f32) -> f32 {
    let a = v.abs();
    if a <= 0.8 {
        v
    } else {
        v.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

pub(crate) fn run(sh: Arc<Shared>, tx: Sender<MuxMsg>, namer: Arc<dyn AppNamer>, mic_rx: Receiver<(Option<Option<String>>, f32)>) {
    crate::mf::init();
    crate::util::boost_thread();
    let cfg = sh.cfg.lock().clone();
    let (ctx, crx) = unbounded::<Chunk>();
    let mut hub = Hub::new(sh.epoch.hns, 250);
    let mut mic_choice = cfg.mic.clone();
    let mut mic_gain = cfg.mic_gain;
    let mut mic: Option<AudioSource> = mic_choice.clone().map(|d| AudioSource::start(MIC_ID, SourceKind::Mic(d), ctx.clone()));
    let mut mic_restart: Option<Instant> = None;
    let mut apps: HashMap<u32, App> = HashMap::new();
    let mut next_id = 100u32;
    let mut slots: Vec<Option<(u32, Enc)>> = (0..cfg.app_slots).map(|_| None).collect();
    let start_pos = hub.position();
    let (Ok(mut full), Ok(mut mic_enc)) = (Enc::new(AudioTrack::Full, cfg.audio_kbps, start_pos), Enc::new(AudioTrack::Mic, cfg.audio_kbps, start_pos)) else {
        tracing::error!("AAC encoder unavailable; recording without sound");
        let _ = sh.events.send(RecEvent::Warning("Sound can't be recorded: the AAC encoder is unavailable".into()));
        let _ = tx.send(MuxMsg::AudioWatermark(i64::MAX));
        return;
    };
    // Per-app capture needs Windows 10 2004+; otherwise capture the default
    // output device as one source.
    let mut loopback: Option<AudioSource> = None;
    let mut app_capture_ok: Option<bool> = None;
    let mut last_scan = Instant::now() - Duration::from_secs(10);
    let mut level_acc = Levels::default();
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
                mic = mic_choice.clone().map(|d| AudioSource::start(MIC_ID, SourceKind::Mic(d), ctx.clone()));
            }
        }
        sh.status.lock().mic_ok = mic.as_ref().map(|m| !m.failed()).unwrap_or(false);

        // Which programs are playing sound?
        if !stopping && last_scan.elapsed() >= Duration::from_millis(300) {
            last_scan = Instant::now();
            if app_capture_ok != Some(false) {
                let sessions = sound_sessions();
                let procs = processes();
                let pids: Vec<u32> = sessions.iter().map(|s| s.pid).collect();
                let targets = choose_targets(&pids, &procs, &own, NEVER_CAPTURE);
                // Stop captures whose program went away (or is now covered by another).
                let gone: Vec<u32> = apps.iter().filter(|(_, a)| !targets.iter().any(|t| t.root == a.root) || a.source.failed()).map(|(id, _)| *id).collect();
                for id in gone {
                    if let Some(a) = apps.remove(&id) {
                        if a.source.failed() && app_capture_ok.is_none() {
                            app_capture_ok = Some(false);
                        }
                        hub.remove(id);
                        if let Some(s) = a.slot {
                            release_slot(&mut slots, s, &tx);
                            announce(&sh, &tx, &slots, &apps, hub.position());
                        }
                    }
                }
                for t in targets {
                    if apps.values().any(|a| a.root == t.root) {
                        continue;
                    }
                    let id = next_id;
                    next_id += 1;
                    let ident = namer.identify(&t.exe, t.root);
                    tracing::debug!("capturing {} ({}) pid {}", ident.name, t.exe, t.root);
                    let source = AudioSource::start(id, SourceKind::App(t.root), ctx.clone());
                    apps.insert(id, App { source, root: t.root, ident, slot: None, last_sound: 0, peak: 0.0 });
                }
                if app_capture_ok.is_none() && apps.values().any(|a| !a.source.failed()) {
                    app_capture_ok = Some(true);
                }
            }
            if app_capture_ok == Some(false) && loopback.is_none() {
                tracing::warn!("per-app audio capture isn't available on this Windows version; recording the default output device");
                apps.clear();
                loopback = Some(AudioSource::start(LOOPBACK_ID, SourceKind::DeviceLoopback(None), ctx.clone()));
            }
        }

        while let Ok(c) = crx.try_recv() {
            if c.source == MIC_ID || c.source == LOOPBACK_ID || apps.contains_key(&c.source) {
                hub.push(c);
            }
        }
        let blocks = if stopping { hub.pull_until(hub.frame_of(now_hns())) } else { hub.pull(now_hns()) };
        for b in blocks {
            let mut mix = vec![0.0f32; BLOCK * CHANNELS];
            // Microphone.
            let mic_blk: Vec<f32> = match b.sources.iter().find(|(id, _)| *id == MIC_ID) {
                Some((_, v)) => v.iter().map(|x| x * mic_gain).collect(),
                None => vec![0.0; BLOCK * CHANNELS],
            };
            level_acc.mic = level_acc.mic.max(peak(&mic_blk));
            for (m, x) in mix.iter_mut().zip(&mic_blk) {
                *m += x;
            }
            mic_enc.push(&mic_blk, &tx);
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
                        if let Some(free) = slots.iter().position(|s| s.is_none()) {
                            match Enc::new(AudioTrack::Slot(free as u32), cfg.app_kbps, b.pos) {
                                Ok(e) => {
                                    slots[free] = Some((*id, e));
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
            for s in slots.iter_mut().flatten() {
                let (app_id, enc) = s;
                let pcm = b.sources.iter().find(|(id, _)| id == app_id).map(|(_, v)| v.clone()).unwrap_or_else(|| vec![0.0; BLOCK * CHANNELS]);
                enc.push(&pcm, &tx);
            }
            if assigned {
                announce(&sh, &tx, &slots, &apps, b.pos);
            }
            for m in mix.iter_mut() {
                *m = limit(*m);
            }
            level_acc.full = level_acc.full.max(peak(&mix));
            full.push(&mix, &tx);
            // Free tracks of apps that have been quiet for a long time.
            let idle: Vec<u32> = apps.values().filter(|a| a.slot.is_some() && b.pos - a.last_sound > SLOT_IDLE_FRAMES).filter_map(|a| a.slot).collect();
            if !idle.is_empty() {
                for s in idle {
                    release_slot(&mut slots, s, &tx);
                    if let Some(a) = apps.values_mut().find(|a| a.slot == Some(s)) {
                        a.slot = None;
                    }
                }
                announce(&sh, &tx, &slots, &apps, b.pos);
            }
        }
        // Everything up to the slowest active encoder is complete.
        let mut wm = full.next_pos().min(mic_enc.next_pos());
        for (_, e) in slots.iter().flatten() {
            wm = wm.min(e.next_pos());
        }
        let _ = tx.send(MuxMsg::AudioWatermark(wm));

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
    full.finish(&tx);
    mic_enc.finish(&tx);
    for s in slots.into_iter().flatten() {
        s.1.finish(&tx);
    }
    let _ = tx.send(MuxMsg::AudioWatermark(i64::MAX));
    drop(mic);
    drop(loopback);
}

fn release_slot(slots: &mut [Option<(u32, Enc)>], s: u32, tx: &Sender<MuxMsg>) {
    if let Some((_, enc)) = slots.get_mut(s as usize).and_then(|x| x.take()) {
        enc.finish(tx);
    }
}

fn announce(sh: &Shared, tx: &Sender<MuxMsg>, slots: &[Option<(u32, Enc)>], apps: &HashMap<u32, App>, pos: i64) {
    let list: Vec<SlotInfo> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().and_then(|(id, _)| apps.get(id)).map(|a| SlotInfo { slot: i as u32, app: a.ident.clone() }))
        .collect();
    let _ = tx.send(MuxMsg::Slots { slots: list.clone() });
    let at_ms = sh.epoch.unix_ms + crate::capture::hub::frames_to_hns(pos) / 10_000;
    let _ = sh.events.send(RecEvent::Slots { at_ms, slots: list });
}
