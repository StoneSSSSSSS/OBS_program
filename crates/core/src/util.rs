//! Small helpers shared across the crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (UTC).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convert a `SystemTime` to Unix milliseconds.
pub fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// "1.4 GB", "820 MB", "12 KB".
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// "1:02:03" for long durations, "2:03" for short ones.
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".to_string();
    }
    let total = secs.floor() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Like [`format_duration`] but with tenths of a second ("2:03.4").
pub fn format_duration_precise(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00.0".to_string();
    }
    let tenths = (secs * 10.0).floor() as u64;
    let base = format_duration((tenths / 10) as f64);
    format!("{base}.{}", tenths % 10)
}

/// "3h 12m", "45m", "12s" — compact human durations for status displays.
pub fn format_span_short(secs: i64) -> String {
    let secs = secs.max(0);
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// Random lowercase hex token.
pub fn random_token(len_bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; len_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write a file atomically: write to a temp sibling, flush, then rename over.
pub fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// On Windows, stop child processes (ffmpeg/ffprobe) from flashing a console.
pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_formatting() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(13_503_844_027), "12.6 GB");
        assert_eq!(human_bytes(500 * 1024 * 1024 * 1024), "500 GB");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(59.9), "0:59");
        assert_eq!(format_duration(61.0), "1:01");
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3725.5), "1:02:05");
        assert_eq!(format_duration(f64::NAN), "0:00");
        assert_eq!(format_duration_precise(12.34), "0:12.3");
        assert_eq!(format_span_short(30), "30s");
        assert_eq!(format_span_short(125), "2m");
        assert_eq!(format_span_short(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(format_span_short(2 * 86400 + 3600), "2d 1h");
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.toml");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn token_is_hex() {
        let t = random_token(16);
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
