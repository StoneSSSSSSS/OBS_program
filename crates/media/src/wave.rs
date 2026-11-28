//! Waveforms (loudness over time, per audio track) for files the recorder
//! didn't produce live: decode each track and measure its peaks.

use crate::audioread::{TrackPcm, OUT_RATE};
use crate::mp4::reader::Mp4Reader;
use anyhow::Result;
use cv_core::waveform::{PeakMeter, Waveform, PEAKS_PER_SEC};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Build the waveform of every audio track from `from_s` on. `throttle`
/// sleeps between seconds of audio so background work stays gentle.
pub fn generate(path: &Path, from_s: f64, throttle: Option<Duration>, cancel: &AtomicBool) -> Result<Waveform> {
    generate_chunks(path, from_s, OUT_RATE as usize, |span| {
        if cancel.load(Ordering::SeqCst) {
            anyhow::bail!("cancelled");
        }
        if let Some(delay) = throttle.filter(|_| !span.is_zero()) {
            std::thread::sleep(delay);
        }
        Ok(())
    })
}

/// Decode historical media in bounded chunks. The caller can wait before each
/// chunk without discarding decoded peaks or restarting the audio decoder.
pub fn generate_archive(path: &Path, before_chunk: impl FnMut(Duration) -> Result<()>) -> Result<Waveform> {
    generate_chunks(path, 0.0, OUT_RATE as usize / 5, before_chunk)
}

fn generate_chunks(
    path: &Path,
    from_s: f64,
    step: usize,
    mut before_chunk: impl FnMut(Duration) -> Result<()>,
) -> Result<Waveform> {
    before_chunk(Duration::ZERO)?;
    let r = Mp4Reader::open(path)?;
    let info = crate::probe::probe_reader(&r);
    let n = info.audio.len();
    if n == 0 {
        anyhow::bail!("no audio");
    }
    let dur = r.audio().iter().map(|i| r.end_secs(*i)).fold(0.0, f64::max).min(info.duration.max(0.0) + 1.0);
    let frames_total = ((dur - from_s).max(0.0) * PEAKS_PER_SEC as f64).floor() as usize;
    let mut per_track: Vec<Vec<u8>> = vec![Vec::with_capacity(frames_total); n];
    let span = Duration::from_secs_f64(step as f64 / f64::from(OUT_RATE));
    for (k, a) in info.audio.iter().enumerate() {
        before_chunk(Duration::ZERO)?;
        let ti = r.track_index(a.track_id).unwrap();
        let mut pcm = TrackPcm::new(&r, ti)?;
        let start = (from_s * OUT_RATE as f64) as i64;
        pcm.seek(&r, start)?;
        let mut meter = PeakMeter::new(OUT_RATE);
        let mut buf = vec![0.0f32; step * 2];
        let mut pos = start;
        // Track coverage: skip decoding unused stretches of app tracks.
        let mut coverage = r.coverage(ti).into_iter().peekable();
        let ts = r.tracks[ti].timescale as f64;
        while per_track[k].len() < frames_total {
            before_chunk(span)?;
            let t = pos as f64 / OUT_RATE as f64;
            while coverage.peek().is_some_and(|(_, end)| *end as f64 / ts <= t) {
                coverage.next();
            }
            let covered = coverage.peek().is_some_and(|(start, _)| (*start as f64 / ts) < t + span.as_secs_f64());
            if covered {
                pcm.read(&r, pos, &mut buf)?;
                meter.push(&buf, 2, &mut per_track[k]);
            } else {
                meter.silence(step, &mut per_track[k]);
            }
            pos += step as i64;
        }
        per_track[k].truncate(frames_total);
    }
    before_chunk(Duration::ZERO)?;
    let mut peaks = Vec::with_capacity(frames_total * n);
    for f in 0..frames_total {
        for t in per_track.iter() {
            peaks.push(t.get(f).copied().unwrap_or(0));
        }
    }
    Ok(Waveform { tracks: n, rate: PEAKS_PER_SEC, peaks })
}
