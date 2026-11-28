//! Lines up audio from many sources on one timeline.
//!
//! Every source delivers chunks stamped with the performance counter. The hub
//! places them on a shared frame grid (48 kHz since the recording started),
//! treats tiny timestamp jitter as continuous audio, corrects slow clock drift
//! one frame at a time, leaves silence where a source delivered nothing (apps
//! only deliver audio while they play), and hands out fixed 10 ms blocks a
//! little behind real time so late chunks still make it in.

use super::audio::{Chunk, CHANNELS, RATE};
use std::collections::{HashMap, VecDeque};

/// Frames per block (10 ms).
pub const BLOCK: usize = 480;
/// Timestamp differences up to this many frames count as continuous.
const TOLERANCE: i64 = 480;
/// Start correcting drift once the smoothed error passes this many frames.
const DRIFT_START: f64 = 48.0;

#[derive(Debug)]
struct Seg {
    pos: i64,
    data: Vec<f32>,
}

impl Seg {
    fn frames(&self) -> i64 {
        (self.data.len() / CHANNELS) as i64
    }
    fn end(&self) -> i64 {
        self.pos + self.frames()
    }
}

#[derive(Debug, Default)]
struct Source {
    segs: VecDeque<Seg>,
    expected_end: Option<i64>,
    err: f64,
}

/// One 10 ms slice of every source that had sound in it.
#[derive(Debug, Clone)]
pub struct Block {
    /// Frame position (48 kHz frames since the hub's epoch).
    pub pos: i64,
    /// (source id, BLOCK stereo frames). Sources without audio are omitted.
    pub sources: Vec<(u32, Vec<f32>)>,
}

pub struct Hub {
    epoch_hns: i64,
    next: i64,
    latency: i64,
    sources: HashMap<u32, Source>,
    /// Frames inserted/dropped for drift correction (diagnostics).
    pub corrections: i64,
}

/// 100 ns units -> frames, rounded to the nearest frame.
pub fn hns_to_frames(hns: i64) -> i64 {
    (hns as i128 * RATE as i128 + 5_000_000).div_euclid(10_000_000) as i64
}

pub fn frames_to_hns(frames: i64) -> i64 {
    (frames as i128 * 10_000_000 / RATE as i128) as i64
}

impl Hub {
    /// `epoch_hns` = performance-counter time of frame 0. Blocks are emitted
    /// `latency_ms` behind real time.
    pub fn new(epoch_hns: i64, latency_ms: u32) -> Hub {
        Hub { epoch_hns, next: 0, latency: (latency_ms as i64 * RATE as i64) / 1000, sources: HashMap::new(), corrections: 0 }
    }

    /// Frame position of the next block to be emitted.
    pub fn position(&self) -> i64 {
        self.next
    }

    pub fn frame_of(&self, hns: i64) -> i64 {
        hns_to_frames(hns - self.epoch_hns)
    }

    pub fn remove(&mut self, id: u32) {
        self.sources.remove(&id);
    }

    /// Jump the output position forward (after sleep), discarding anything
    /// queued before it.
    pub fn skip_to(&mut self, pos: i64) {
        if pos <= self.next {
            return;
        }
        let pos = self.next + (pos - self.next) / BLOCK as i64 * BLOCK as i64;
        self.next = pos;
        for src in self.sources.values_mut() {
            while src.segs.front().map(|s| s.end() <= pos).unwrap_or(false) {
                src.segs.pop_front();
            }
        }
    }

    pub fn push(&mut self, c: Chunk) {
        let frames = (c.data.len() / CHANNELS) as i64;
        if frames == 0 {
            return;
        }
        let mut pos = self.frame_of(c.time_hns);
        let mut data = c.data;
        let src = self.sources.entry(c.source).or_default();
        if let Some(end) = src.expected_end {
            let err = pos - end;
            if err.abs() <= TOLERANCE {
                // Continuous: place it right after the previous chunk, and
                // nudge by one frame when the clocks have drifted apart.
                src.err = src.err * 0.98 + err as f64 * 0.02;
                pos = end;
                if src.err > DRIFT_START {
                    // Source is slow: repeat one frame.
                    let first: Vec<f32> = data[..CHANNELS].to_vec();
                    data.splice(0..0, first);
                    src.err -= 1.0;
                    self.corrections += 1;
                } else if src.err < -DRIFT_START && data.len() > CHANNELS {
                    // Source is fast: drop one frame.
                    data.drain(..CHANNELS);
                    src.err += 1.0;
                    self.corrections += 1;
                }
            } else if err < 0 {
                // Overlaps what we already have: drop the overlapping start.
                let drop = (-err).min(frames) as usize;
                data.drain(..drop * CHANNELS);
                pos = end;
                if data.is_empty() {
                    return;
                }
            } else {
                // A gap (the app was silent): start a new run.
                src.err = 0.0;
            }
        }
        let seg = Seg { pos, data };
        src.expected_end = Some(seg.end());
        match src.segs.back_mut() {
            Some(last) if last.end() == seg.pos => last.data.extend_from_slice(&seg.data),
            _ => src.segs.push_back(seg),
        }
    }

    /// Emit every block that is now old enough.
    pub fn pull(&mut self, now_hns: i64) -> Vec<Block> {
        let limit = self.frame_of(now_hns) - self.latency;
        let mut out = Vec::new();
        while self.next + BLOCK as i64 <= limit {
            out.push(self.take_block());
        }
        out
    }

    /// Emit blocks up to `upto` frames regardless of latency (when stopping).
    pub fn pull_until(&mut self, upto: i64) -> Vec<Block> {
        let mut out = Vec::new();
        while self.next + BLOCK as i64 <= upto {
            out.push(self.take_block());
        }
        out
    }

    fn take_block(&mut self) -> Block {
        let (a, b) = (self.next, self.next + BLOCK as i64);
        let mut sources = Vec::new();
        for (id, src) in self.sources.iter_mut() {
            let mut buf: Option<Vec<f32>> = None;
            while let Some(seg) = src.segs.front_mut() {
                if seg.end() <= a {
                    src.segs.pop_front();
                    continue;
                }
                if seg.pos >= b {
                    break;
                }
                let from = seg.pos.max(a);
                let to = seg.end().min(b);
                let out = buf.get_or_insert_with(|| vec![0.0; BLOCK * CHANNELS]);
                let dst = ((from - a) as usize) * CHANNELS;
                let src_off = ((from - seg.pos) as usize) * CHANNELS;
                let n = ((to - from) as usize) * CHANNELS;
                out[dst..dst + n].copy_from_slice(&seg.data[src_off..src_off + n]);
                if seg.end() <= b {
                    src.segs.pop_front();
                } else {
                    // Keep the rest for the next block.
                    let keep = ((b - seg.pos) as usize) * CHANNELS;
                    seg.data.drain(..keep);
                    seg.pos = b;
                    break;
                }
            }
            if let Some(v) = buf {
                sources.push((*id, v));
            }
        }
        sources.sort_by_key(|(id, _)| *id);
        self.next = b;
        Block { pos: a, sources }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(src: u32, frame_pos: i64, frames: usize, value: f32) -> Chunk {
        Chunk { source: src, time_hns: frames_to_hns(frame_pos), data: vec![value; frames * CHANNELS] }
    }

    #[test]
    fn aligns_sources_and_leaves_silence() {
        let mut h = Hub::new(0, 0);
        // Source 1 continuous from 0; source 2 only from frame 700.
        h.push(chunk(1, 0, 960, 0.5));
        h.push(chunk(2, 700, 100, 0.25));
        let blocks = h.pull(frames_to_hns(960));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].sources.len(), 1, "source 2 silent in block 0");
        assert_eq!(blocks[1].sources.len(), 2);
        let s2 = &blocks[1].sources[1].1;
        // Frames 700..800 are at offsets 220..320 of block 1.
        assert_eq!(s2[219 * 2], 0.0);
        assert_eq!(s2[220 * 2], 0.25);
        assert_eq!(s2[319 * 2 + 1], 0.25);
        assert_eq!(s2[320 * 2], 0.0);
    }

    #[test]
    fn jitter_is_absorbed_and_gaps_kept() {
        let mut h = Hub::new(0, 0);
        h.push(chunk(1, 0, 480, 1.0));
        // Next chunk stamped 30 frames late: still continuous.
        h.push(chunk(1, 510, 480, 2.0));
        // Then a real gap of a second.
        h.push(chunk(1, 48_000, 480, 3.0));
        let blocks = h.pull(frames_to_hns(49_000));
        let b1 = &blocks[1].sources[0].1;
        assert!(b1.iter().all(|v| *v == 2.0), "second chunk exactly in block 1");
        let silent = blocks.iter().filter(|b| b.sources.is_empty()).count();
        assert!(silent > 90, "the gap stays silent ({silent})");
        let last = blocks.iter().find(|b| b.pos == 48_000).unwrap();
        assert!(last.sources[0].1.iter().all(|v| *v == 3.0));
    }

    #[test]
    fn overlap_is_dropped() {
        let mut h = Hub::new(0, 0);
        h.push(chunk(1, 0, 480, 1.0));
        // Stamped 600 frames early relative to the expected end (beyond jitter).
        h.push(Chunk { source: 1, time_hns: frames_to_hns(-120), data: (0..960 * 2).map(|i| if i < 600 * 2 { 9.0 } else { 2.0 }).collect() });
        let blocks = h.pull(frames_to_hns(960));
        assert!(blocks[0].sources[0].1.iter().all(|v| *v == 1.0));
        // 600 overlapping frames dropped; 360 frames of 2.0 follow at 480.
        let b1 = &blocks[1].sources[0].1;
        assert_eq!(b1[0], 2.0);
        assert_eq!(b1[359 * 2], 2.0);
        assert_eq!(b1[360 * 2], 0.0);
    }

    #[test]
    fn drift_is_corrected_without_jumps() {
        // A source whose clock runs 0.1% fast: it delivers 480 frames every
        // 9.99 ms of timestamps. Over 10 s the hub must stay within a few ms.
        let mut h = Hub::new(0, 0);
        let mut t = 0.0f64;
        for _ in 0..1000 {
            h.push(Chunk { source: 1, time_hns: (t * 10_000_000.0) as i64, data: vec![1.0; 480 * 2] });
            t += 480.0 / 48_000.0 * 0.999;
        }
        assert!(h.corrections > 300, "corrections {}", h.corrections);
        let src = h.sources.get(&1).unwrap();
        let end = src.expected_end.unwrap();
        let expected = hns_to_frames((t * 10_000_000.0) as i64);
        assert!((end - expected).abs() < 120, "end {end} vs {expected}");
        // No silent holes appeared in the middle.
        let blocks = h.pull_until(end);
        let holes = blocks.iter().filter(|b| b.sources.is_empty()).count();
        assert_eq!(holes, 0);
    }

    #[test]
    fn latency_holds_blocks_back() {
        let mut h = Hub::new(0, 100);
        h.push(chunk(1, 0, 4800, 1.0));
        assert!(h.pull(frames_to_hns(4800)).len() <= 1);
        assert_eq!(h.pull(frames_to_hns(4800 + 4800)).len(), 10);
        assert_eq!(h.position(), 4800);
    }
}
