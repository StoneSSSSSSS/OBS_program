//! Video side of the recorder: steady frame clock -> capture -> GPU
//! conversion/scaling -> hardware H.264.

use super::{MonitorChoice, MuxMsg, Shared, VideoPacket};
use crate::capture::display::CaptureError;
use crate::capture::DisplayCapture;
use crate::clock::now_hns;
use crate::d3d::{fit_size, list_outputs, Converter, Gpu};
use crate::mf::h264::{EncodedFrame, EncoderConfig, VideoEncoder};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) fn run(sh: Arc<Shared>, tx: Sender<MuxMsg>) {
    crate::mf::init();
    crate::util::boost_thread();
    let mut attempt = 0u32;
    while !sh.stop.load(Ordering::SeqCst) {
        let r = pipeline(&sh, &tx);
        // Close the open stretch of video so it isn't stretched over the outage.
        let _ = tx.send(MuxMsg::VideoEnd);
        match r {
            Ok(()) => break,
            Err(e) => {
                attempt += 1;
                let msg = format!("{e:#}");
                if attempt == 1 || attempt % 20 == 0 {
                    tracing::warn!("screen recording problem (retrying): {msg}");
                }
                sh.status.lock().video_error = Some(msg);
                let until = Instant::now() + Duration::from_secs(if attempt < 3 { 1 } else { 3 });
                while Instant::now() < until && !sh.stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

struct Sps {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

fn send(tx: &Sender<MuxMsg>, frames: Vec<EncodedFrame>, params: &mut Sps, size: (u32, u32)) {
    for f in frames {
        let c = crate::h264::annexb_to_avcc(&f.data);
        if c.sps.is_some() {
            params.sps = c.sps.clone();
        }
        if c.pps.is_some() {
            params.pps = c.pps.clone();
        }
        let key = f.key || c.idr;
        let _ = tx.send(MuxMsg::Video(VideoPacket {
            time_hns: f.time_hns,
            data: c.data,
            key,
            sps: if key { params.sps.clone() } else { None },
            pps: if key { params.pps.clone() } else { None },
            width: size.0,
            height: size.1,
        }));
    }
}

fn pipeline(sh: &Arc<Shared>, tx: &Sender<MuxMsg>) -> Result<()> {
    let cfg = sh.cfg.lock().clone();
    let outs = list_outputs();
    let out = match &cfg.monitor {
        MonitorChoice::Device(name) => outs.iter().find(|o| o.device_name.eq_ignore_ascii_case(name)),
        MonitorChoice::Primary => None,
    }
    .or_else(|| outs.iter().find(|o| o.primary))
    .or(outs.first())
    .context("no monitor found")?
    .clone();
    let gpu = Arc::new(Gpu::on_adapter(out.adapter_index)?);
    let mut cap = match sh.window_test {
        #[cfg(feature = "qa-window")]
        Some(hwnd) => DisplayCapture::Wgc(crate::capture::wgc::WgcCapture::new_owned_window(gpu.clone(), hwnd)?),
        #[cfg(not(feature = "qa-window"))]
        Some(_) => anyhow::bail!("owned-window QA capture is unavailable"),
        None => DisplayCapture::open(gpu.clone(), out.output_index, cfg.capture_method)?,
    };
    let (w, h) = fit_size(cap.size(), cfg.max_height);
    let mut conv = Converter::new(&gpu, cap.size(), (w, h), true)?;
    let fps = cfg.fps.clamp(10, 240);
    let gop = ((fps as f32 * cfg.keyframe_secs).round() as u32).max(1);
    let mut enc = VideoEncoder::new(Some(gpu.clone()), EncoderConfig { width: w, height: h, fps, rate: cfg.rate, gop, choice: cfg.encoder, realtime: true })?;
    {
        let mut st = sh.status.lock();
        st.width = w;
        st.height = h;
        st.encoder = enc.info.name.clone();
        st.capture_method = cap.method_name().to_string();
        st.video_error = None;
    }
    tracing::info!("screen {}x{} -> {w}x{h} @ {fps} fps via {} + {}", cap.size().0, cap.size().1, cap.method_name(), enc.info.name);
    let pool: Vec<_> = (0..8).map(|_| gpu.nv12_target(w, h)).collect::<Result<_>>()?;
    let frame_hns = 10_000_000 / fps as i64;
    // Exact tick times (no accumulated rounding).
    let tick_at = |start: i64, n: i64| start + (n as i128 * 10_000_000 / fps as i128) as i64;
    let start = now_hns();
    let mut submitted: usize = 0;
    let mut n: i64 = 0;
    let mut params = Sps { sps: None, pps: None };
    let mut encoded = 0u64;
    let mut dropped = 0u64;
    let mut stat_at = Instant::now();
    let mut stat_frames = 0u64;
    let mut lost_since: Option<Instant> = None;
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            break;
        }
        let tick = tick_at(start, n);
        // Take in screen updates until it's time for the next frame.
        loop {
            let now = now_hns();
            if now >= tick {
                break;
            }
            let wait_ms = ((tick - now) / 10_000).clamp(0, 50) as u32;
            match cap.poll(wait_ms) {
                Ok(_) => {}
                Err(CaptureError::Lost) => {
                    lost_since.get_or_insert_with(Instant::now);
                    if cap.reopen().is_err() {
                        std::thread::sleep(Duration::from_millis(wait_ms.max(1) as u64));
                        if lost_since.map(|t| t.elapsed() > Duration::from_secs(4)).unwrap_or(false) {
                            // Resolution changed or the monitor went away: rebuild everything.
                            let rest = enc.finish().unwrap_or_default();
                            send(tx, rest, &mut params, (w, h));
                            anyhow::bail!("screen capture was interrupted");
                        }
                    } else {
                        lost_since = None;
                    }
                }
                Err(CaptureError::Other(e)) => return Err(e),
            }
            if sh.stop.load(Ordering::SeqCst) {
                break;
            }
        }
        // Running late (system busy)? Skip the frames we missed instead of
        // bursting; timestamps stay exact.
        let behind = (now_hns() - tick) / frame_hns;
        if behind >= 2 {
            n += behind;
            dropped += behind as u64;
            continue;
        }
        if !cap.has_image() {
            n += 1;
            continue;
        }
        let force = sh.force_key.swap(false, Ordering::SeqCst);
        let src = cap.frame(cfg.cursor).clone();
        // Rotate through the pool per submitted frame, so a frame still
        // queued in the encoder is never overwritten after skipped ticks.
        let dst = &pool[submitted % pool.len()];
        submitted += 1;
        conv.blit(&src, dst)?;
        let frames = enc.encode_texture(dst, tick, force)?;
        send(tx, frames, &mut params, (w, h));
        encoded += 1;
        stat_frames += 1;
        n += 1;
        if stat_at.elapsed() >= Duration::from_secs(2) {
            let mut st = sh.status.lock();
            st.fps = stat_frames as f64 / stat_at.elapsed().as_secs_f64();
            st.dropped_frames = dropped + enc.dropped;
            stat_at = Instant::now();
            stat_frames = 0;
        }
    }
    let rest = enc.finish().unwrap_or_default();
    send(tx, rest, &mut params, (w, h));
    tracing::info!("screen recording stopped after {encoded} frames ({dropped} skipped)");
    Ok(())
}
