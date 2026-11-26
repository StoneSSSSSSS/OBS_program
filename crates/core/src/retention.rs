//! Deciding which continuous-recording segments to delete.
//!
//! Pure logic so it can be tested exhaustively; the service supplies the
//! segment list and free-space figure and performs the deletions.
//!
//! Retention is measured in *hours of footage*, like the old program's "keep
//! the last 48 recordings": time the PC is switched off doesn't eat into it.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub start_ms: i64,
    pub end_ms: i64,
    pub size: u64,
    /// OBS is still writing this file.
    pub in_progress: bool,
    /// The user asked to keep this one.
    pub kept: bool,
}

impl SegmentInfo {
    fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Hours of footage to keep (newest first).
    pub keep_footage_ms: i64,
    /// Also delete anything that ended longer ago than this. 0 = no age limit.
    pub max_age_ms: i64,
    /// 0 = unlimited.
    pub max_total_bytes: u64,
    pub min_free_bytes: u64,
}

impl Policy {
    pub fn from_config(cfg: &crate::config::RecordingConfig) -> Self {
        Self {
            keep_footage_ms: cfg.retention_hours as i64 * 3_600_000,
            max_age_ms: cfg.max_age_days as i64 * 86_400_000,
            max_total_bytes: cfg.max_storage_gb as u64 * 1024 * 1024 * 1024,
            min_free_bytes: cfg.min_free_space_gb as u64 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// More footage than the retention setting.
    Footage,
    /// Older than the optional age limit.
    Age,
    TotalSize,
    LowDisk,
}

/// Returns the segments to delete (oldest first) and why.
///
/// Rules:
/// 1. Never touch the in-progress segment or kept segments.
/// 2. Walking from newest to oldest, keep segments until `keep_footage`
///    is covered (the segment that crosses the line is kept, so at least that
///    much footage always remains); delete the rest.
/// 3. Optionally delete segments that ended more than `max_age` ago.
/// 4. While total size exceeds `max_total_bytes`, delete the oldest.
/// 5. While free space is below `min_free_bytes`, delete the oldest.
pub fn select_for_deletion(
    segments: &[SegmentInfo],
    now_ms: i64,
    free_bytes: u64,
    policy: Policy,
) -> Vec<(PathBuf, Reason)> {
    // The footage budget counts every non-kept segment, including the live one.
    let mut by_newest: Vec<&SegmentInfo> = segments.iter().filter(|s| !s.kept).collect();
    by_newest.sort_by(|a, b| b.start_ms.cmp(&a.start_ms).then(b.path.cmp(&a.path)));
    let mut over_budget: Vec<PathBuf> = Vec::new();
    let mut covered = 0i64;
    for s in &by_newest {
        if covered >= policy.keep_footage_ms && !s.in_progress {
            over_budget.push(s.path.clone());
        }
        covered += s.duration_ms();
    }

    let mut candidates: Vec<&SegmentInfo> = segments.iter().filter(|s| !s.in_progress && !s.kept).collect();
    candidates.sort_by_key(|s| (s.start_ms, s.path.clone()));

    let mut out = Vec::new();
    let mut deleted = vec![false; candidates.len()];
    for (i, s) in candidates.iter().enumerate() {
        if over_budget.contains(&s.path) {
            out.push((s.path.clone(), Reason::Footage));
            deleted[i] = true;
        } else if policy.max_age_ms > 0 && s.end_ms < now_ms - policy.max_age_ms {
            out.push((s.path.clone(), Reason::Age));
            deleted[i] = true;
        }
    }

    let freed: u64 = candidates.iter().zip(&deleted).filter(|(_, d)| **d).map(|(s, _)| s.size).sum();
    let mut total: u64 = segments.iter().map(|s| s.size).sum::<u64>() - freed;
    let mut free = free_bytes + freed;
    for (i, s) in candidates.iter().enumerate() {
        if deleted[i] {
            continue;
        }
        let over_total = policy.max_total_bytes > 0 && total > policy.max_total_bytes;
        let low_disk = free < policy.min_free_bytes;
        if !over_total && !low_disk {
            break;
        }
        out.push((s.path.clone(), if over_total { Reason::TotalSize } else { Reason::LowDisk }));
        deleted[i] = true;
        total = total.saturating_sub(s.size);
        free += s.size;
    }
    out.sort_by_key(|(p, _)| candidates.iter().position(|s| &s.path == p).unwrap_or(usize::MAX));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const GB: u64 = 1024 * 1024 * 1024;

    fn seg(name: &str, start_h: i64, size_gb: u64) -> SegmentInfo {
        SegmentInfo {
            path: PathBuf::from(name),
            start_ms: start_h * H,
            end_ms: (start_h + 1) * H,
            size: size_gb * GB,
            in_progress: false,
            kept: false,
        }
    }

    fn policy(keep_h: i64) -> Policy {
        Policy { keep_footage_ms: keep_h * H, max_age_ms: 0, max_total_bytes: 0, min_free_bytes: 0 }
    }

    fn names(v: &[(PathBuf, Reason)]) -> Vec<String> {
        v.iter().map(|(p, _)| p.to_string_lossy().to_string()).collect()
    }

    #[test]
    fn keeps_hours_of_footage_not_wall_clock() {
        // Three 1h recordings spread over a week (PC off in between).
        let segs = vec![seg("mon", 0, 1), seg("wed", 48, 1), seg("sun", 144, 1)];
        // 2h of footage: keep sun + wed, delete mon, even though wed is days old.
        let d = select_for_deletion(&segs, 1000 * H, 100 * GB, policy(2));
        assert_eq!(names(&d), vec!["mon"]);
        assert_eq!(d[0].1, Reason::Footage);
        // 48h budget: nothing deleted, however old.
        assert!(select_for_deletion(&segs, 10_000 * H, 100 * GB, policy(48)).is_empty());
    }

    #[test]
    fn matches_old_program_on_real_shape() {
        // 48 one-hour files over 5 days + the live one: old program kept 48.
        let mut segs: Vec<SegmentInfo> = (0..48).map(|i| seg(&format!("r{i:02}"), i * 3, 13)).collect();
        let mut live = seg("live", 200, 1);
        live.in_progress = true;
        live.end_ms = live.start_ms + H / 2;
        segs.push(live);
        let d = select_for_deletion(&segs, 201 * H, 800 * GB, policy(48));
        // 47 newest complete files + half-hour live file = 47.5h < 48 -> the
        // oldest is the one crossing the line, so nothing is deleted.
        assert!(d.is_empty(), "{:?}", names(&d));
    }

    #[test]
    fn segment_crossing_the_budget_is_kept() {
        let segs = vec![seg("a", 0, 1), seg("b", 1, 1), seg("c", 2, 1)];
        let mut p = policy(1);
        p.keep_footage_ms = H + H / 2;
        assert_eq!(names(&select_for_deletion(&segs, 3 * H, 100 * GB, p)), vec!["a"]);
    }

    #[test]
    fn live_segment_counts_but_is_never_deleted() {
        let mut segs = vec![seg("old", 0, 1), seg("live", 1, 1)];
        segs[1].in_progress = true;
        assert_eq!(names(&select_for_deletion(&segs, 2 * H, 100 * GB, policy(1))), vec!["old"]);
        let p = Policy { keep_footage_ms: 0, max_age_ms: 0, max_total_bytes: 1, min_free_bytes: 1000 * GB };
        assert_eq!(names(&select_for_deletion(&segs, 2 * H, 0, p)), vec!["old"]);
    }

    #[test]
    fn kept_segments_are_exempt_and_dont_use_budget() {
        let mut segs = vec![seg("a", 0, 1), seg("b", 1, 1), seg("c", 2, 1)];
        segs[2].kept = true;
        assert_eq!(names(&select_for_deletion(&segs, 3 * H, 100 * GB, policy(1))), vec!["a"]);
    }

    #[test]
    fn optional_age_limit() {
        let segs = vec![seg("a", 0, 1), seg("b", 100, 1)];
        let p = Policy { keep_footage_ms: 1000 * H, max_age_ms: 24 * H, max_total_bytes: 0, min_free_bytes: 0 };
        let d = select_for_deletion(&segs, 110 * H, 100 * GB, p);
        assert_eq!(names(&d), vec!["a"]);
        assert_eq!(d[0].1, Reason::Age);
    }

    #[test]
    fn total_size_cap() {
        let segs = vec![seg("a", 0, 5), seg("b", 1, 5), seg("c", 2, 5), seg("d", 3, 5)];
        let p = Policy { keep_footage_ms: 1000 * H, max_age_ms: 0, max_total_bytes: 11 * GB, min_free_bytes: 0 };
        let d = select_for_deletion(&segs, 4 * H, 100 * GB, p);
        assert_eq!(names(&d), vec!["a", "b"]);
        assert!(d.iter().all(|(_, r)| *r == Reason::TotalSize));
    }

    #[test]
    fn low_disk_space() {
        let segs = vec![seg("c", 2, 3), seg("a", 0, 3), seg("b", 1, 3)];
        let p = Policy { keep_footage_ms: 1000 * H, max_age_ms: 0, max_total_bytes: 0, min_free_bytes: 10 * GB };
        let d = select_for_deletion(&segs, 4 * H, 5 * GB, p);
        assert_eq!(names(&d), vec!["a", "b"]);
        assert!(d.iter().all(|(_, r)| *r == Reason::LowDisk));
    }

    #[test]
    fn footage_deletions_count_towards_free_space() {
        let segs = vec![seg("a", 0, 4), seg("b", 10, 4)];
        let p = Policy { keep_footage_ms: H, max_age_ms: 0, max_total_bytes: 0, min_free_bytes: 6 * GB };
        let d = select_for_deletion(&segs, 12 * H, 3 * GB, p);
        assert_eq!(names(&d), vec!["a"]);
    }
}
