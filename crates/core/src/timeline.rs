//! Mapping wall-clock time onto the continuous-recording segment files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub path: PathBuf,
    /// Wall-clock start (Unix ms).
    pub start_ms: i64,
    /// Length of the file in ms (for the live file: recorded so far).
    pub duration_ms: i64,
    pub live: bool,
}

impl Segment {
    pub fn end_ms(&self) -> i64 {
        self.start_ms + self.duration_ms.max(0)
    }
}

/// Position inside a segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Located {
    pub index: usize,
    pub offset_ms: i64,
    /// The requested time fell into a gap; this is the start of the next segment.
    pub snapped: bool,
}

/// Find the segment containing wall-clock time `t_ms`. `segments` must be
/// sorted by start. Times in gaps snap forward to the next segment; times
/// after the end snap to the end of the last segment.
pub fn locate(segments: &[Segment], t_ms: i64) -> Option<Located> {
    if segments.is_empty() {
        return None;
    }
    // Last segment that starts at or before t.
    let idx = segments.partition_point(|s| s.start_ms <= t_ms);
    if idx == 0 {
        return Some(Located { index: 0, offset_ms: 0, snapped: true });
    }
    let i = idx - 1;
    let s = &segments[i];
    if t_ms < s.end_ms() {
        return Some(Located { index: i, offset_ms: t_ms - s.start_ms, snapped: false });
    }
    if i + 1 < segments.len() {
        return Some(Located { index: i + 1, offset_ms: 0, snapped: true });
    }
    Some(Located { index: i, offset_ms: s.duration_ms.max(0), snapped: true })
}

/// Wall-clock time for an offset in a segment.
pub fn to_wall(segments: &[Segment], index: usize, offset_ms: i64) -> Option<i64> {
    segments.get(index).map(|s| s.start_ms + offset_ms)
}

/// One piece of a cut that spans one or more files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CutPart {
    pub path: PathBuf,
    /// Seconds into the file where this piece starts.
    pub in_s: f64,
    /// Seconds into the file where it ends (None = to the end of the file).
    pub out_s: Option<f64>,
}

/// Plan a cut of wall-clock range `[from_ms, to_ms)` across segments.
/// Gaps between segments are skipped.
pub fn plan_cut(segments: &[Segment], from_ms: i64, to_ms: i64) -> Vec<CutPart> {
    let mut parts = Vec::new();
    if to_ms <= from_ms {
        return parts;
    }
    for s in segments {
        let a = from_ms.max(s.start_ms);
        let b = to_ms.min(s.end_ms());
        if b <= a {
            continue;
        }
        let in_s = (a - s.start_ms) as f64 / 1000.0;
        let out_s = if b >= s.end_ms() && !s.live { None } else { Some((b - s.start_ms) as f64 / 1000.0) };
        parts.push(CutPart { path: s.path.clone(), in_s, out_s });
    }
    parts
}

/// Total seconds covered by a cut plan (needs the segment durations for
/// open-ended parts).
pub fn plan_duration_s(parts: &[CutPart], segments: &[Segment]) -> f64 {
    parts
        .iter()
        .map(|p| {
            let end = p.out_s.unwrap_or_else(|| {
                segments
                    .iter()
                    .find(|s| s.path == p.path)
                    .map(|s| s.duration_ms as f64 / 1000.0)
                    .unwrap_or(p.in_s)
            });
            (end - p.in_s).max(0.0)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str, start: i64, dur: i64) -> Segment {
        Segment { path: PathBuf::from(name), start_ms: start, duration_ms: dur, live: false }
    }

    #[test]
    fn locating() {
        let segs = vec![seg("a", 1000, 1000), seg("b", 2000, 1000), seg("c", 5000, 1000)];
        assert_eq!(locate(&segs, 1500), Some(Located { index: 0, offset_ms: 500, snapped: false }));
        assert_eq!(locate(&segs, 2000), Some(Located { index: 1, offset_ms: 0, snapped: false }));
        assert_eq!(locate(&segs, 3500), Some(Located { index: 2, offset_ms: 0, snapped: true }));
        assert_eq!(locate(&segs, 500), Some(Located { index: 0, offset_ms: 0, snapped: true }));
        assert_eq!(locate(&segs, 9000), Some(Located { index: 2, offset_ms: 1000, snapped: true }));
        assert_eq!(locate(&[], 0), None);
        assert_eq!(to_wall(&segs, 1, 250), Some(2250));
    }

    #[test]
    fn cut_within_one_segment() {
        let segs = vec![seg("a", 0, 60_000), seg("b", 60_000, 60_000)];
        let p = plan_cut(&segs, 10_000, 20_000);
        assert_eq!(p, vec![CutPart { path: "a".into(), in_s: 10.0, out_s: Some(20.0) }]);
        assert!((plan_duration_s(&p, &segs) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn cut_spanning_segments_and_gap() {
        let segs = vec![seg("a", 0, 60_000), seg("b", 60_000, 60_000), seg("c", 200_000, 60_000)];
        let p = plan_cut(&segs, 50_000, 210_000);
        assert_eq!(
            p,
            vec![
                CutPart { path: "a".into(), in_s: 50.0, out_s: None },
                CutPart { path: "b".into(), in_s: 0.0, out_s: None },
                CutPart { path: "c".into(), in_s: 0.0, out_s: Some(10.0) },
            ]
        );
        assert!((plan_duration_s(&p, &segs) - 80.0).abs() < 1e-9);
        assert!(plan_cut(&segs, 10, 10).is_empty());
    }

    #[test]
    fn live_segment_always_has_explicit_out_point() {
        let mut segs = vec![seg("live", 0, 30_000)];
        segs[0].live = true;
        let p = plan_cut(&segs, 0, 40_000);
        assert_eq!(p[0].out_s, Some(30.0));
    }
}
