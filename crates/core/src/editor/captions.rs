//! Subtitle cues use the same project clock as pictures and audio.
use super::{Id, Project, Span};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caption {
    pub id: Id,
    pub range: Span,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionRun {
    pub range: Span,
    pub text: String,
}

fn timecode(s: &str) -> Result<u64> {
    let (hours, rest) = s.split_once(':').context("Use hours:minutes:seconds,milliseconds.")?;
    let (minutes, rest) = rest.split_once(':').context("Missing seconds in subtitle time.")?;
    let (seconds, fraction) = rest.split_once([',', '.']).context("Missing milliseconds in subtitle time.")?;
    ensure!((1..=3).contains(&fraction.len()) && fraction.bytes().all(|c| c.is_ascii_digit()), "Invalid subtitle milliseconds.");
    let (h, m, s): (u64, u64, u64) = (hours.parse()?, minutes.parse()?, seconds.parse()?);
    ensure!(m < 60 && s < 60, "Subtitle minutes and seconds must be below 60.");
    let ms: u64 = fraction.parse()?;
    let total = u128::from(h) * 3_600_000 + u128::from(m) * 60_000 + u128::from(s) * 1000
        + u128::from(ms * 10u64.pow(3 - fraction.len() as u32));
    ensure!(total * 10_000 <= i64::MAX as u128, "Subtitle time is too large.");
    Ok((total * 10_000) as u64)
}

fn stamp(ms: u64) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1_000 % 60, ms % 1000)
}

fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(bytes.len() % 2 == 0, "Incomplete UTF-16 subtitle file.");
        let little = bytes[0] == 0xff;
        let units: Vec<_> = bytes[2..].chunks_exact(2).map(|p| {
            if little { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) }
        }).collect();
        return String::from_utf16(&units).context("Invalid UTF-16 subtitles.");
    }
    Ok(std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
        .context("Save subtitles in UTF-8 or UTF-16.")?.to_owned())
}

fn parse_srt(text: &str) -> Result<Vec<CaptionRun>> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().peekable();
    let mut cues = Vec::new();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() { continue; }
        let timing = if line.trim().bytes().all(|c| c.is_ascii_digit()) {
            lines.next().context("Subtitle number has no time range.")?
        } else { line };
        let (a, b) = timing.split_once("-->").with_context(|| format!("Missing time range in subtitle {}.", cues.len() + 1))?;
        let start = timecode(a.trim())?;
        let end = timecode(b.split_whitespace().next().unwrap_or(""))?;
        ensure!(end > start, "Subtitle {} ends before it starts.", cues.len() + 1);
        let mut words = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() { break; }
            words.push(lines.next().unwrap().trim());
        }
        ensure!(!words.is_empty(), "Subtitle {} has no text.", cues.len() + 1);
        cues.push(CaptionRun { range: Span { start, end }, text: words.join("\n") });
    }
    ensure!(!cues.is_empty(), "No subtitles found.");
    Ok(cues)
}

impl Project {
    pub fn caption(&mut self, text: String, range: Span) -> Result<Id> {
        ensure!(!text.trim().is_empty(), "Enter the caption text.");
        range.check(i64::MAX as u64)?;
        let id = self.allocate_id()?;
        self.captions.push(Caption { id, range, text });
        Ok(id)
    }

    pub fn import_srt(&mut self, path: &Path) -> Result<usize> {
        const LIMIT: u64 = 8 * 1024 * 1024;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= LIMIT, "Subtitle file exceeds 8 MB.");
        let cues = parse_srt(&decode(&bytes)?)?;
        let count = cues.len();
        ensure!(self.next_id.checked_add(count as u64).is_some(), "Too many subtitle cues.");
        for cue in cues { self.caption(cue.text, cue.range)?; }
        Ok(count)
    }

    pub fn export_srt(&self, path: &Path) -> Result<()> {
        ensure!(path.extension().and_then(|x| x.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("srt")), "Use the .srt extension.");
        if self.captions.is_empty() { bail!("Add a caption before exporting subtitles."); }
        let path = std::path::absolute(path)?;
        let mut file = tempfile::NamedTempFile::new_in(path.parent().context("Choose an output folder.")?)?;
        let mut cues: Vec<_> = self.captions.iter().collect();
        cues.sort_by_key(|cue| (cue.range.start, cue.id));
        for (i, cue) in cues.into_iter().enumerate() {
            let a = cue.range.start / 10_000;
            let b = (cue.range.end / 10_000).max(a + 1);
            writeln!(file, "{}\n{} --> {}\n{}\n", i + 1, stamp(a), stamp(b), cue.text)?;
        }
        file.as_file().sync_all()?;
        file.persist_noclobber(path).context("Choose a new subtitle filename.")?;
        Ok(())
    }

    /// Make non-overlapping display intervals, joining simultaneous subtitles on separate lines.
    pub fn caption_runs(&self) -> Vec<CaptionRun> {
        let mut changes: BTreeMap<u64, Vec<(usize, bool)>> = BTreeMap::new();
        for (i, cue) in self.captions.iter().enumerate() {
            changes.entry(cue.range.start).or_default().push((i, true));
            changes.entry(cue.range.end).or_default().push((i, false));
        }
        let mut active = BTreeSet::<usize>::new();
        let mut runs = Vec::new();
        let mut start = 0;
        for (end, events) in changes {
            if start < end && !active.is_empty() {
                let text = active.iter().map(|&i| self.captions[i].text.as_str()).collect::<Vec<_>>().join("\n");
                runs.push(CaptionRun { range: Span { start, end }, text });
            }
            for (i, enter) in events {
                if enter { active.insert(i); } else { active.remove(&i); }
            }
            start = end;
        }
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::TICKS_PER_SECOND as S;

    #[test]
    fn captions_slice_join_and_round_trip_across_windows_text_formats() -> Result<()> {
        let original = "1\r\n00:00:01,000 --> 00:00:03,000\r\nFirst\r\n\r\n2\r\n00:00:02,000 --> 00:00:04,000\r\n二\r\n";
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(original.encode_utf16().flat_map(u16::to_le_bytes));
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("source.srt");
        std::fs::write(&input, bytes)?;
        let mut p = Project::default();
        assert_eq!(p.import_srt(&input)?, 2);
        assert_eq!(p.duration(), 4 * S);
        assert_eq!(p.caption_runs().iter().map(|r| &*r.text).collect::<Vec<_>>(), ["First", "First\n二", "二"]);
        let window = p.slice(Span { start: 2 * S, end: 3 * S })?;
        assert_eq!(window.duration(), S);
        assert_eq!(window.caption_runs(), [CaptionRun { range: Span { start: 0, end: S }, text: "First\n二".into() }]);
        let output = dir.path().join("release.srt");
        p.export_srt(&output)?;
        assert!(p.export_srt(&output).is_err());
        assert_eq!(parse_srt(&std::fs::read_to_string(output)?)?, parse_srt(original)?);
        std::fs::write(&input, "00:00:00,000 --> 00:00:00,200\nGood\n\n2\nBAD\n")?;
        assert!(p.import_srt(&input).is_err());
        assert_eq!(p.captions.len(), 2);
        Ok(())
    }
}
