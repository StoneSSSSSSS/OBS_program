//! File/folder naming: sanitising, invisible-character stripping, templates,
//! and cleaning up the folder names produced by the old Python program.

use std::path::{Path, PathBuf};

/// True for characters that are invisible or used to obfuscate window titles
/// (some anti-cheat protected games sprinkle zero-width characters into their
/// titles, which made the old program create a new folder for every clip).
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
        | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}'
        | '\u{E0000}'..='\u{E007F}')
        || c.is_control()
}

pub fn strip_invisible(s: &str) -> String {
    s.chars().filter(|c| !is_invisible(*c)).collect()
}

/// Collapse runs of whitespace into single spaces and trim.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

const RESERVED: [&str; 28] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    "COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³",
];

const STEM_LIMIT: usize = 100;

/// Clean a title for display, without imposing filesystem restrictions.
/// Keep joiners, variation selectors and tags used by scripts and emoji.
pub fn clean_display_title(s: &str) -> String {
    let visible: String = s.chars()
        .filter(|&c| c.is_whitespace() || !is_invisible(c) || matches!(
            c, '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0020}'..='\u{E007F}'
        ))
        .collect();
    collapse_ws(&visible)
}

/// Make `s` safe to use as a single Windows path component.
pub fn sanitize_component(s: &str) -> String {
    sanitize_component_to(s, STEM_LIMIT)
}

fn sanitize_component_to(s: &str, limit: usize) -> String {
    let cleaned: String = strip_invisible(s)
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c => c,
        })
        .collect();
    let collapsed = collapse_ws(&cleaned);
    let mut remaining = limit;
    let mut out: String = collapsed.trim_start_matches(['.', ' ']).chars().take_while(|c| {
        if let Some(next) = remaining.checked_sub(c.len_utf16()) {
            remaining = next;
            true
        } else {
            false
        }
    }).collect();
    out.truncate(out.trim_end_matches(['.', ' ']).len());
    if out.is_empty() {
        return "Untitled".to_string();
    }
    let end = out.find('.').unwrap_or(out.len());
    let device = out[..end].trim_end().to_ascii_uppercase();
    if RESERVED.contains(&device.as_str()) {
        if out.encode_utf16().count() == limit {
            // Make room without splitting a Unicode scalar or changing an
            // interior dot that is part of the filename.
            out.pop();
            out.truncate(out.trim_end_matches(['.', ' ']).len());
        }
        let device_end = out.find('.').unwrap_or(out.len());
        out.insert(device_end, '_');
    }
    out
}

/// `dir/stem.ext`, or `dir/stem (2).ext`, `dir/stem (3).ext`... whichever is free.
/// `ignore` is a path that may be treated as free (the file being renamed).
pub fn unique_path(dir: &Path, stem: &str, ext: &str, ignore: Option<&Path>) -> PathBuf {
    unique_path_with_suffix(dir, stem, None, ext, ignore)
}

/// A safe filename with an optional descriptive suffix that survives truncation.
pub fn unique_path_with_suffix(dir: &Path, title: &str, suffix: Option<&str>, ext: &str, ignore: Option<&Path>) -> PathBuf {
    let ext = sanitize_component_to(ext.trim_start_matches('.'), 24);
    let suffix = suffix.map(|s| format!(" ({})", sanitize_component_to(s, 30))).unwrap_or_default();
    let base = sanitize_component_to(title, STEM_LIMIT - suffix.encode_utf16().count() - 10);
    let stem = format!("{base}{suffix}");
    for n in 1..10_000 {
        let path = if n == 1 {
            dir.join(format!("{stem}.{ext}"))
        } else {
            dir.join(format!("{stem} ({n}).{ext}"))
        };
        let same_as_ignored = ignore
            .map(|i| i.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy()))
            .unwrap_or(false);
        if same_as_ignored || !path.exists() {
            return path;
        }
    }
    dir.join(format!("{stem} {}.{ext}", crate::util::random_token(4)))
}

/// Fill `{game} {date} {time} {exe}` tokens. Unknown tokens are left as-is.
pub fn render_template(template: &str, game: &str, exe: &str, at: chrono::DateTime<chrono::Local>) -> String {
    sanitize_component(&render_title(template, game, exe, at))
}

/// Render the same name template as a display title.
pub fn render_title(template: &str, game: &str, exe: &str, at: chrono::DateTime<chrono::Local>) -> String {
    let out = template
        .replace("{game}", game)
        .replace("{exe}", exe.trim_end_matches(".exe").trim_end_matches(".EXE"))
        .replace("{date}", &at.format("%Y-%m-%d").to_string())
        .replace("{time}", &at.format("%H-%M-%S").to_string());
    let title = clean_display_title(&out);
    if title.is_empty() { "Untitled".to_string() } else { title }
}

/// Map a folder name created by the old program (which used raw window titles)
/// to a sensible game/app name. Merges e.g. 6 variants of "ARC Raiders" whose
/// titles contained zero-width characters, and "Content_Warning" vs
/// "Content Warning".
pub fn legacy_folder_to_game(name: &str) -> String {
    let s = collapse_ws(&strip_invisible(name).replace('_', " "));
    let lower = s.to_lowercase();

    const APP_SUFFIXES: [(&str, &str); 10] = [
        (" - google chrome", "Google Chrome"),
        (" - discord", "Discord"),
        (" - vlc media player", "VLC media player"),
        (" - notepad", "Notepad"),
        (" - microsoft edge", "Microsoft Edge"),
        (" - mozilla firefox", "Firefox"),
        (" - visual studio code", "Visual Studio Code"),
        (" - file explorer", "File Explorer"),
        (" - youtube", "YouTube"),
        (" - spotify", "Spotify"),
    ];
    for (suffix, app) in APP_SUFFIXES {
        if lower.ends_with(suffix) || lower == suffix.trim_start_matches(" - ") {
            return app.to_string();
        }
    }
    if lower.starts_with("command prompt") {
        return "Command Prompt".into();
    }
    if lower.starts_with("windows powershell") {
        return "Windows PowerShell".into();
    }
    static OBS_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)^obs \d[\d.]*\b.*profile").unwrap());
    if OBS_RE.is_match(&s) {
        return "OBS Studio".into();
    }
    static BUILD_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)\s+-\s+build\b.*$").unwrap());
    let s = BUILD_RE.replace(&s, "").to_string();
    static UNITY_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)\s+-\s+unity\s+\S+.*$").unwrap());
    if UNITY_RE.is_match(&s) {
        return "Unity Editor".into();
    }
    let s = sanitize_component(&s);
    if s == "Untitled" {
        "Desktop".into()
    } else {
        s
    }
}

/// Key used to merge names that differ only in case/spacing.
pub fn merge_key(name: &str) -> String {
    collapse_ws(&strip_invisible(name)).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_zero_width() {
        let arc = "A\u{200B}\u{200B}\u{200B}R\u{200B}\u{FEFF}\u{FEFF}C \u{FEFF}\u{FEFF}\u{200B}R\u{200B}ai\u{FEFF}der\u{200B}s";
        assert_eq!(strip_invisible(arc), "ARC Raiders");
        assert_eq!(legacy_folder_to_game(arc), "ARC Raiders");
    }

    #[test]
    fn sanitizes_components() {
        assert_eq!(sanitize_component("a/b\\c:d*e?f\"g<h>i|j"), "a b c d e f g h i j");
        assert_eq!(sanitize_component("  name.  "), "name");
        assert_eq!(sanitize_component("..."), "Untitled");
        assert_eq!(sanitize_component(""), "Untitled");
        assert_eq!(sanitize_component("CON"), "CON_");
        assert_eq!(sanitize_component("con.txt"), "con_.txt");
        assert_eq!(sanitize_component("COM¹"), "COM¹_");
        assert_eq!(sanitize_component("Battlefield™ V"), "Battlefield™ V");
        let long = "x".repeat(300);
        assert_eq!(sanitize_component(&long).len(), 100);
    }

    #[test]
    fn unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = unique_path(dir.path(), "clip", "mp4", None);
        assert!(p1.ends_with("clip.mp4"));
        std::fs::write(&p1, b"x").unwrap();
        let p2 = unique_path(dir.path(), "clip", ".mp4", None);
        assert!(p2.ends_with("clip (2).mp4"));
        // The file being renamed counts as free.
        let p3 = unique_path(dir.path(), "clip", "mp4", Some(&p1));
        assert_eq!(p3, p1);
    }

    #[test]
    fn display_title_keeps_punctuation_and_emoji_beyond_filename_limit() {
        let title = format!("{} ⚔️ 👩‍💻 : “公園 / final?”", "食".repeat(140));
        let dirty = format!(" \t{title}\u{202E}\r\n ");
        let cleaned = clean_display_title(&dirty);
        assert_eq!(cleaned, title);
        assert!(sanitize_component(&cleaned).encode_utf16().count() <= STEM_LIMIT);
    }

    #[test]
    fn suffixes_and_collision_indices_survive_utf16_filename_limits() {
        let dir = tempfile::tempdir().unwrap();
        let title = format!("CON.txt/{} : ‘last’?", "🧪".repeat(120));
        let first = unique_path_with_suffix(dir.path(), &title, Some("10 MB"), "mp4", None);
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("CON_.txt"));
        assert!(name.ends_with(" (10 MB).mp4"));
        assert!(name.encode_utf16().count() <= STEM_LIMIT + 4);
        std::fs::write(&first, b"first clip").unwrap();
        let second = unique_path_with_suffix(dir.path(), &title, Some("10 MB"), "mp4", None);
        assert_ne!(first, second);
        assert!(second.file_name().unwrap().to_str().unwrap().ends_with(" (10 MB) (2).mp4"));
    }

    #[test]
    fn templates() {
        use chrono::TimeZone;
        let at = chrono::Local.with_ymd_and_hms(2026, 11, 25, 16, 47, 23).unwrap();
        assert_eq!(
            render_template("{game} {date} {time}", "Deadlock", "deadlock.exe", at),
            "Deadlock 2026-11-25 16-47-23"
        );
        assert_eq!(render_template("{exe}_{date}", "X", "cs2.exe", at), "cs2_2026-11-25");
        assert_eq!(render_template("{game}: best", "A/B", "", at), "A B best");
    }

    #[test]
    fn legacy_folder_names() {
        let cases = [
            ("#bot-command  pLace - Discord", "Discord"),
            ("pLace__pLace_-_Discord", "Discord"),
            ("how does Dileptus eat - Google Search - Google Chrome", "Google Chrome"),
            ("2025-05-19 17-14-27mp4 - VLC media player", "VLC media player"),
            ("Untitled - Notepad", "Notepad"),
            ("Command_Prompt_-_ping__1111_t", "Command Prompt"),
            ("OBS 3100 - Profile Untitled - Scenes Untitled", "OBS Studio"),
            ("Noita_-_Build_Jan_25_2025_-_155541", "Noita"),
            ("Content_Warning", "Content Warning"),
            ("Content Warning", "Content Warning"),
            ("Counter-Strike_2", "Counter-Strike 2"),
            ("Domain of Magica", "Domain of Magica"),
            ("Asteroids_game2_-_Gameplay_-_Windows,_Mac,_Linux_-_Unity_202236f1_DX11", "Unity Editor"),
            ("Battlefield™ V", "Battlefield™ V"),
        ];
        for (input, want) in cases {
            assert_eq!(legacy_folder_to_game(input), want, "input: {input}");
        }
        assert_eq!(merge_key("Content  Warning"), merge_key("content warning"));
    }
}
