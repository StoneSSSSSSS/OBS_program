//! A tiny order-preserving INI editor for OBS's `basic.ini` / `global.ini`.
//!
//! Only touches the keys we set; everything else (including unknown sections,
//! comments and key order) is written back byte-for-byte.

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Section(String),
    Kv { key: String, value: String },
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IniDoc {
    lines: Vec<Line>,
    bom: bool,
    crlf: bool,
}

impl IniDoc {
    pub fn parse(text: &str) -> IniDoc {
        let bom = text.starts_with('\u{FEFF}');
        let text = text.trim_start_matches('\u{FEFF}');
        let crlf = text.contains("\r\n");
        let lines = text
            .lines()
            .map(|raw| {
                let t = raw.trim();
                if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
                    Line::Section(t[1..t.len() - 1].to_string())
                } else if !t.starts_with(';') && !t.starts_with('#') && t.contains('=') {
                    let (k, v) = raw.split_once('=').unwrap();
                    Line::Kv { key: k.trim().to_string(), value: v.to_string() }
                } else {
                    Line::Other(raw.to_string())
                }
            })
            .collect();
        IniDoc { lines, bom, crlf }
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<IniDoc> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::util::write_atomic(path, self.to_string().as_bytes())
    }

    fn section_range(&self, section: &str) -> Option<(usize, usize)> {
        let start = self
            .lines
            .iter()
            .position(|l| matches!(l, Line::Section(s) if s == section))?;
        let end = self.lines[start + 1..]
            .iter()
            .position(|l| matches!(l, Line::Section(_)))
            .map(|p| start + 1 + p)
            .unwrap_or(self.lines.len());
        Some((start, end))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let (s, e) = self.section_range(section)?;
        self.lines[s + 1..e].iter().find_map(|l| match l {
            Line::Kv { key: k, value } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        self.get(section, key)?.trim().parse().ok()
    }

    pub fn has_section(&self, section: &str) -> bool {
        self.section_range(section).is_some()
    }

    /// Set a value; returns true if the document changed.
    pub fn set(&mut self, section: &str, key: &str, value: &str) -> bool {
        match self.section_range(section) {
            Some((s, e)) => {
                for l in &mut self.lines[s + 1..e] {
                    if let Line::Kv { key: k, value: v } = l {
                        if k == key {
                            if v == value {
                                return false;
                            }
                            *v = value.to_string();
                            return true;
                        }
                    }
                }
                // Insert after the last key/value of the section (before trailing blanks).
                let mut insert_at = e;
                while insert_at > s + 1 && matches!(&self.lines[insert_at - 1], Line::Other(o) if o.trim().is_empty()) {
                    insert_at -= 1;
                }
                self.lines.insert(insert_at, Line::Kv { key: key.to_string(), value: value.to_string() });
                true
            }
            None => {
                if let Some(Line::Other(o)) = self.lines.last() {
                    if !o.trim().is_empty() {
                        self.lines.push(Line::Other(String::new()));
                    }
                } else if !self.lines.is_empty() {
                    self.lines.push(Line::Other(String::new()));
                }
                self.lines.push(Line::Section(section.to_string()));
                self.lines.push(Line::Kv { key: key.to_string(), value: value.to_string() });
                true
            }
        }
    }

    pub fn remove(&mut self, section: &str, key: &str) -> bool {
        if let Some((s, e)) = self.section_range(section) {
            if let Some(pos) = self.lines[s + 1..e]
                .iter()
                .position(|l| matches!(l, Line::Kv { key: k, .. } if k == key))
            {
                self.lines.remove(s + 1 + pos);
                return true;
            }
        }
        false
    }
}

impl std::fmt::Display for IniDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let nl = if self.crlf { "\r\n" } else { "\n" };
        if self.bom {
            write!(f, "\u{FEFF}")?;
        }
        for l in &self.lines {
            match l {
                Line::Section(s) => write!(f, "[{s}]{nl}")?,
                Line::Kv { key, value } => write!(f, "{key}={value}{nl}")?,
                Line::Other(o) => write!(f, "{o}{nl}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[General]\nName=Untitled\n\n[AdvOut]\nRecFilePath=I:/OBS/Recordings\nRecTracks=1\n\n[Hotkeys]\nReplayBuffer={\"x\":1}\n";

    #[test]
    fn roundtrip_is_exact() {
        let doc = IniDoc::parse(SAMPLE);
        assert_eq!(doc.to_string(), SAMPLE);
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_eq!(IniDoc::parse(&crlf).to_string(), crlf);
        let bom = format!("\u{FEFF}{SAMPLE}");
        assert_eq!(IniDoc::parse(&bom).to_string(), bom);
    }

    #[test]
    fn get_set_remove() {
        let mut doc = IniDoc::parse(SAMPLE);
        assert_eq!(doc.get("AdvOut", "RecTracks"), Some("1"));
        assert_eq!(doc.get_u32("AdvOut", "RecTracks"), Some(1));
        assert_eq!(doc.get("General", "RecTracks"), None);
        assert!(doc.set("AdvOut", "RecTracks", "63"));
        assert!(!doc.set("AdvOut", "RecTracks", "63"));
        assert!(doc.set("AdvOut", "RecSplitFileTime", "30"));
        assert!(doc.set("Video", "FPSInt", "60"));
        assert!(doc.remove("Hotkeys", "ReplayBuffer"));
        assert!(!doc.remove("Hotkeys", "ReplayBuffer"));
        let text = doc.to_string();
        assert!(text.contains("[AdvOut]\nRecFilePath=I:/OBS/Recordings\nRecTracks=63\nRecSplitFileTime=30\n\n[Hotkeys]"));
        assert!(text.ends_with("[Video]\nFPSInt=60\n"));
        // Values containing '=' survive.
        let mut d2 = IniDoc::parse("[A]\nk=a=b\n");
        assert_eq!(d2.get("A", "k"), Some("a=b"));
        d2.set("A", "k", "c=d");
        assert_eq!(d2.to_string(), "[A]\nk=c=d\n");
    }
}
