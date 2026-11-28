//! Video file names: which files are videos, and the timestamps ClipVault (and
//! OBS before it) put in file names (`%CCYY-%MM-%DD %hh-%mm-%ss`).

use chrono::{Local, NaiveDateTime, TimeZone};
use std::path::Path;

/// Name prefix of old OBS replay-buffer saves.
pub const REPLAY_PREFIX: &str = "Replay";

pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "flv", "ts", "m4v", "webm"];

pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Is this a replay-buffer save (as opposed to a continuous-recording segment)?
pub fn is_replay_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_lowercase().starts_with(&REPLAY_PREFIX.to_lowercase()))
        .unwrap_or(false)
}

/// Extract the local timestamp from a name like `2026-11-25 16-47-23.mp4` or
/// `Replay 2026-11-23 20-58-57.mp4` and return it as Unix ms.
pub fn parse_timestamp_ms(path: &Path) -> Option<i64> {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(\d{4})-(\d{2})-(\d{2})[ _T](\d{2})-(\d{2})-(\d{2})").unwrap()
    });
    let name = path.file_stem()?.to_str()?;
    let c = RE.captures(name)?;
    let s = format!("{}-{}-{} {}:{}:{}", &c[1], &c[2], &c[3], &c[4], &c[5], &c[6]);
    let naive = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok()?;
    // DST fold: pick the earliest valid interpretation.
    Local.from_local_datetime(&naive).earliest().map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        let p = Path::new(r"I:\OBS\Recordings\2026-11-25 16-47-23.mp4");
        let ms = parse_timestamp_ms(p).unwrap();
        let dt = Local.timestamp_millis_opt(ms).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-11-25 16:47:23");
        assert!(parse_timestamp_ms(Path::new("Replay 2026-11-23 20-58-57.mp4")).is_some());
        assert!(parse_timestamp_ms(Path::new("Deadlock 2026-01-02 03-04-05 (2).mp4")).is_some());
        assert_eq!(parse_timestamp_ms(Path::new("clip.mp4")), None);
        assert_eq!(parse_timestamp_ms(Path::new("2026-13-40 99-99-99.mp4")), None);
    }

    #[test]
    fn classifies_files() {
        assert!(is_replay_file(Path::new("Replay 2026-11-23 20-58-57.mp4")));
        assert!(!is_replay_file(Path::new("2026-11-23 20-58-57.mp4")));
        assert!(is_video_file(Path::new("a.MKV")));
        assert!(!is_video_file(Path::new("a.txt")));
        assert!(!is_video_file(Path::new("noext")));
    }
}
