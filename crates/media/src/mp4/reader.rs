//! MP4 reader for regular and fragmented files (ours, OBS's and ffmpeg's).
//!
//! Regular files are fully indexed from their sample tables. Fragmented files
//! are indexed per fragment (time range per track) and a fragment's sample
//! table is parsed on demand, so hour-long recordings stay cheap. A file that
//! is still being written can be followed with [`Mp4Reader::refresh`]; a
//! fragment only becomes visible once its data has been completely written.

use super::{Codec, META_MAGIC};
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: u32,
    pub kind: TrackKind,
    pub timescale: u32,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// `hdlr` name (what other tools show as the track title).
    pub name: String,
    pub enabled: bool,
    /// Edit list: (media time where presentation starts, presented duration in
    /// track ticks, or 0 if unknown).
    pub edit: Option<(i64, u64)>,
    /// Media duration in ticks from the header (0 for fragmented files).
    pub header_duration: u64,
}

impl TrackInfo {
    /// Seconds of media skipped by the edit list (keyframe pre-roll).
    pub fn edit_start_s(&self) -> f64 {
        self.edit.map(|(m, _)| m.max(0) as f64 / self.timescale.max(1) as f64).unwrap_or(0.0)
    }
}

/// One sample's location and timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Decode time in track ticks.
    pub dts: u64,
    pub duration: u32,
    pub cts: i32,
    pub sync: bool,
}

impl Sample {
    pub fn pts(&self) -> i64 {
        self.dts as i64 + self.cts as i64
    }
    pub fn end(&self) -> u64 {
        self.dts + self.duration as u64
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Trex {
    duration: u32,
    size: u32,
    flags: u32,
}

#[derive(Debug, Clone)]
struct FragRef {
    start: u64,
    end: u64,
    frag: u32,
}

#[derive(Debug, Clone)]
struct Fragment {
    offset: u64,
    size: u64,
}

pub struct Mp4Reader {
    path: PathBuf,
    file: File,
    pub tracks: Vec<TrackInfo>,
    movie_timescale: u32,
    trex: HashMap<u32, Trex>,
    /// Regular files: every sample of every track.
    flat: Vec<Vec<Sample>>,
    frags: Vec<Fragment>,
    /// Per track: fragments containing it, in time order.
    track_frags: Vec<Vec<FragRef>>,
    scan_pos: u64,
    have_moov: bool,
    /// ClipVault metadata payloads with the file offset they appeared at.
    pub metas: Vec<(u64, Vec<u8>)>,
    cache: Mutex<Vec<(u32, HashMap<u32, Vec<Sample>>)>>,
}

const CACHE_FRAGS: usize = 48;

// ------------------------------------------------------------ byte reading

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }
    fn u8(&mut self) -> Result<u8> {
        let v = *self.b.get(self.p).context("truncated box")?;
        self.p += 1;
        Ok(v)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.left() < n {
            bail!("truncated box");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u24(&mut self) -> Result<u32> {
        let s = self.take(3)?;
        Ok(u32::from_be_bytes([0, s[0], s[1], s[2]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_be_bytes(s.try_into().unwrap()))
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    fn full(&mut self) -> Result<(u8, u32)> {
        let v = self.u8()?;
        Ok((v, self.u24()?))
    }
}

/// Iterate child boxes of a payload: (type, body).
fn children(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= b.len() {
        let size32 = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = b[p + 4..p + 8].try_into().unwrap();
        let (hdr, size) = match size32 {
            1 => {
                if p + 16 > b.len() {
                    break;
                }
                (16usize, u64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()))
            }
            0 => (8usize, (b.len() - p) as u64),
            s => (8usize, s),
        };
        if size < hdr as u64 || p as u64 + size > b.len() as u64 {
            break;
        }
        out.push((typ, &b[p + hdr..p + size as usize]));
        p += size as usize;
    }
    out
}

fn child<'a>(b: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    children(b).into_iter().find(|(t, _)| t == typ).map(|(_, v)| v)
}

fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "end of file"));
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
}

// ------------------------------------------------------------ reader

impl Mp4Reader {
    pub fn open(path: &Path) -> Result<Mp4Reader> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut r = Mp4Reader {
            path: path.to_path_buf(),
            file,
            tracks: Vec::new(),
            movie_timescale: 1000,
            trex: HashMap::new(),
            flat: Vec::new(),
            frags: Vec::new(),
            track_frags: Vec::new(),
            scan_pos: 0,
            have_moov: false,
            metas: Vec::new(),
            cache: Mutex::new(Vec::new()),
        };
        r.refresh()?;
        if !r.have_moov {
            bail!("{} is not a playable MP4 (no header found)", path.display());
        }
        Ok(r)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_fragmented(&self) -> bool {
        !self.frags.is_empty() || (self.have_moov && self.flat.iter().all(|f| f.is_empty()) && !self.trex.is_empty())
    }

    /// Scan for boxes appended since the last call. Returns true if new
    /// samples became available.
    pub fn refresh(&mut self) -> Result<bool> {
        let len = self.file.metadata()?.len();
        let mut changed = false;
        loop {
            let pos = self.scan_pos;
            // A corrupt or incomplete header ends the usable part of the file.
            let Ok(Some((typ, hdr, size))) = self.box_header(pos, len) else { break };
            if pos + size > len {
                break; // still being written
            }
            match &typ {
                b"moov" if !self.have_moov => {
                    let body = self.read_vec(pos + hdr, (size - hdr) as usize)?;
                    self.parse_moov(&body)?;
                    self.have_moov = true;
                    changed = true;
                }
                b"moof" if self.have_moov => {
                    // Only accept the fragment once its mdat is complete too.
                    let next = pos + size;
                    match self.box_header(next, len) {
                        Ok(Some((t, _, s))) if &t == b"mdat" && next + s <= len => {}
                        Ok(Some((t, _, _))) if &t != b"mdat" => {}
                        _ => break,
                    }
                    let body = self.read_vec(pos + hdr, (size - hdr) as usize)?;
                    if self.add_fragment(pos, size, &body).is_ok() {
                        changed = true;
                    }
                }
                b"free" | b"skip" if size - hdr >= META_MAGIC.len() as u64 && size < 64 << 20 => {
                    let body = self.read_vec(pos + hdr, (size - hdr) as usize)?;
                    if body.starts_with(META_MAGIC) {
                        self.metas.push((pos, body[META_MAGIC.len()..].to_vec()));
                    }
                }
                _ => {}
            }
            // Advance only after the box is fully handled, so an error above
            // never makes us add the same fragment twice.
            self.scan_pos = pos + size;
        }
        Ok(changed)
    }

    /// (type, header length, total size) of the box at `pos`, if its header is complete.
    fn box_header(&self, pos: u64, len: u64) -> Result<Option<([u8; 4], u64, u64)>> {
        if pos + 8 > len {
            return Ok(None);
        }
        let mut h = [0u8; 16];
        let n = if pos + 16 <= len { 16 } else { 8 };
        read_at(&self.file, pos, &mut h[..n])?;
        let size32 = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = h[4..8].try_into().unwrap();
        match size32 {
            1 => {
                if n < 16 {
                    return Ok(None);
                }
                let s = u64::from_be_bytes(h[8..16].try_into().unwrap());
                if s < 16 {
                    bail!("corrupt box at {pos}");
                }
                Ok(Some((typ, 16, s)))
            }
            0 => {
                // Extends to the end of the file: only usable once complete,
                // which for a growing file we can't know. Treat as the rest.
                Ok(Some((typ, 8, len - pos)))
            }
            s if s < 8 => bail!("corrupt box at {pos}"),
            s => Ok(Some((typ, 8, s))),
        }
    }

    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        read_at(&self.file, offset, &mut v)?;
        Ok(v)
    }

    /// Read a sample's bytes.
    pub fn read(&self, s: &Sample) -> Result<Vec<u8>> {
        self.read_vec(s.offset, s.size as usize).with_context(|| format!("reading {}", self.path.display()))
    }

    // ------------------------------------------------------------ moov

    fn parse_moov(&mut self, moov: &[u8]) -> Result<()> {
        if let Some(mvhd) = child(moov, b"mvhd") {
            let mut r = Rd::new(mvhd);
            let (v, _) = r.full()?;
            if v == 1 {
                r.skip(16)?;
            } else {
                r.skip(8)?;
            }
            self.movie_timescale = r.u32()?.max(1);
        }
        if let Some(mvex) = child(moov, b"mvex") {
            for (t, body) in children(mvex) {
                if &t == b"trex" {
                    let mut r = Rd::new(body);
                    r.full()?;
                    let id = r.u32()?;
                    r.u32()?;
                    let duration = r.u32()?;
                    let size = r.u32()?;
                    let flags = r.u32()?;
                    self.trex.insert(id, Trex { duration, size, flags });
                }
            }
        }
        for (t, body) in children(moov) {
            if &t != b"trak" {
                continue;
            }
            match self.parse_trak(body) {
                Ok((info, samples)) => {
                    self.tracks.push(info);
                    self.flat.push(samples);
                    self.track_frags.push(Vec::new());
                }
                Err(e) => tracing::debug!("skipping track in {}: {e:#}", self.path.display()),
            }
        }
        if self.tracks.is_empty() {
            bail!("no tracks");
        }
        Ok(())
    }

    fn parse_trak(&self, trak: &[u8]) -> Result<(TrackInfo, Vec<Sample>)> {
        let tkhd = child(trak, b"tkhd").context("tkhd")?;
        let mut r = Rd::new(tkhd);
        let (v, flags) = r.full()?;
        if v == 1 {
            r.skip(16)?;
        } else {
            r.skip(8)?;
        }
        let id = r.u32()?;
        let mdia = child(trak, b"mdia").context("mdia")?;
        let mdhd = child(mdia, b"mdhd").context("mdhd")?;
        let mut r = Rd::new(mdhd);
        let (v, _) = r.full()?;
        let (timescale, duration) = if v == 1 {
            r.skip(16)?;
            (r.u32()?, r.u64()?)
        } else {
            r.skip(8)?;
            (r.u32()?, r.u32()? as u64)
        };
        let hdlr = child(mdia, b"hdlr").context("hdlr")?;
        let mut r = Rd::new(hdlr);
        r.full()?;
        r.u32()?;
        let handler: [u8; 4] = r.take(4)?.try_into().unwrap();
        r.skip(12).ok();
        let name_bytes = &hdlr[r.p.min(hdlr.len())..];
        let name_bytes = name_bytes.split(|b| *b == 0).next().unwrap_or(&[]);
        // Old QuickTime-style names start with a length byte.
        let name = String::from_utf8_lossy(name_bytes).trim().to_string();
        let kind = match &handler {
            b"vide" => TrackKind::Video,
            b"soun" => TrackKind::Audio,
            _ => TrackKind::Other,
        };
        let minf = child(mdia, b"minf").context("minf")?;
        let stbl = child(minf, b"stbl").context("stbl")?;
        let stsd = child(stbl, b"stsd").context("stsd")?;
        let (codec, width, height) = parse_stsd(stsd)?;
        let mut edit = None;
        if let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) {
            let mut r = Rd::new(elst);
            let (v, _) = r.full()?;
            let n = r.u32()?;
            let mut empty_movie = 0u64;
            for _ in 0..n {
                let (seg, media) = if v == 1 { (r.u64()?, r.u64()? as i64) } else { (r.u32()? as u64, r.u32()? as i32 as i64) };
                r.u32()?;
                if media < 0 {
                    empty_movie += seg; // initial delay; we ignore it but keep scanning
                    continue;
                }
                let dur = (seg as u128 * timescale as u128 / self.movie_timescale as u128) as u64;
                edit = Some((media, dur));
                break;
            }
            let _ = empty_movie;
        }
        let samples = parse_sample_tables(stbl, kind == TrackKind::Video).unwrap_or_default();
        Ok((
            TrackInfo {
                id,
                kind,
                timescale: timescale.max(1),
                codec,
                width,
                height,
                name,
                enabled: flags & 1 != 0,
                edit,
                header_duration: duration,
            },
            samples,
        ))
    }

    // ------------------------------------------------------------ fragments

    fn add_fragment(&mut self, offset: u64, size: u64, body: &[u8]) -> Result<()> {
        let parsed = self.parse_moof(offset, body)?;
        let idx = self.frags.len() as u32;
        let mut any = false;
        for (track_id, samples) in &parsed {
            let Some(ti) = self.tracks.iter().position(|t| t.id == *track_id) else { continue };
            let (Some(first), Some(last)) = (samples.first(), samples.last()) else { continue };
            self.track_frags[ti].push(FragRef { start: first.dts, end: last.end(), frag: idx });
            any = true;
        }
        if any {
            self.frags.push(Fragment { offset, size });
            let mut c = self.cache.lock();
            c.push((idx, parsed));
            if c.len() > CACHE_FRAGS {
                c.remove(0);
            }
        }
        Ok(())
    }

    /// Next decode time for a track whose fragment has no `tfdt`.
    fn track_end(&self, track_id: u32) -> u64 {
        self.tracks
            .iter()
            .position(|t| t.id == track_id)
            .and_then(|i| self.track_frags[i].last().map(|f| f.end))
            .unwrap_or(0)
    }

    fn parse_moof(&self, moof_offset: u64, body: &[u8]) -> Result<HashMap<u32, Vec<Sample>>> {
        let mut out: HashMap<u32, Vec<Sample>> = HashMap::new();
        let mut prev_traf_end: Option<u64> = None;
        for (t, traf) in children(body) {
            if &t != b"traf" {
                continue;
            }
            let tfhd = child(traf, b"tfhd").context("tfhd")?;
            let mut r = Rd::new(tfhd);
            let (_, fl) = r.full()?;
            let track_id = r.u32()?;
            let trex = self.trex.get(&track_id).copied().unwrap_or_default();
            let base_offset = if fl & 0x01 != 0 {
                r.u64()?
            } else if fl & 0x02_0000 != 0 {
                moof_offset
            } else {
                // Legacy: data follows the previous track fragment's data.
                prev_traf_end.unwrap_or(moof_offset)
            };
            if fl & 0x02 != 0 {
                r.u32()?;
            }
            let def_dur = if fl & 0x08 != 0 { r.u32()? } else { trex.duration };
            let def_size = if fl & 0x10 != 0 { r.u32()? } else { trex.size };
            let def_flags = if fl & 0x20 != 0 { r.u32()? } else { trex.flags };
            let video = self.tracks.iter().find(|t| t.id == track_id).map(|t| t.kind == TrackKind::Video).unwrap_or(false);
            let mut dts = match child(traf, b"tfdt") {
                Some(b) => {
                    let mut r = Rd::new(b);
                    let (v, _) = r.full()?;
                    if v == 1 {
                        r.u64()?
                    } else {
                        r.u32()? as u64
                    }
                }
                None => out.get(&track_id).and_then(|v| v.last().map(|s| s.end())).unwrap_or_else(|| self.track_end(track_id)),
            };
            let mut data_pos = base_offset;
            let list = out.entry(track_id).or_default();
            for (ct, trun) in children(traf) {
                if &ct != b"trun" {
                    continue;
                }
                let mut r = Rd::new(trun);
                let (_, tf) = r.full()?;
                let n = r.u32()? as usize;
                if tf & 0x01 != 0 {
                    data_pos = (base_offset as i64 + r.u32()? as i32 as i64) as u64;
                }
                let first_flags = if tf & 0x04 != 0 { Some(r.u32()?) } else { None };
                if n > 2_000_000 {
                    bail!("implausible sample count");
                }
                list.reserve(n);
                for i in 0..n {
                    let duration = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                    let size = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                    let mut flags = if tf & 0x400 != 0 { r.u32()? } else { def_flags };
                    if i == 0 {
                        if let Some(f) = first_flags {
                            flags = f;
                        }
                    }
                    let cts = if tf & 0x800 != 0 { r.u32()? as i32 } else { 0 };
                    let sync = !video || flags & 0x0001_0000 == 0;
                    list.push(Sample { offset: data_pos, size, dts, duration, cts, sync });
                    data_pos += size as u64;
                    dts += duration as u64;
                }
            }
            prev_traf_end = Some(data_pos);
        }
        Ok(out)
    }

    fn fragment_samples(&self, frag: u32, track_id: u32) -> Result<Vec<Sample>> {
        {
            let c = self.cache.lock();
            if let Some((_, m)) = c.iter().find(|(i, _)| *i == frag) {
                return Ok(m.get(&track_id).cloned().unwrap_or_default());
            }
        }
        let f = self.frags.get(frag as usize).context("bad fragment")?.clone();
        let raw = self.read_vec(f.offset, f.size as usize)?;
        let hdr = if u32::from_be_bytes(raw[0..4].try_into().unwrap()) == 1 { 16 } else { 8 };
        let mut parsed = self.parse_moof(f.offset, &raw[hdr..])?;
        // Fragments without `tfdt` were timed from the previous fragment at
        // scan time; restore those times.
        for (tid, list) in parsed.iter_mut() {
            let Some(ti) = self.tracks.iter().position(|t| t.id == *tid) else { continue };
            if let (Some(fr), Some(first)) = (self.track_frags[ti].binary_search_by_key(&frag, |x| x.frag).ok().map(|i| &self.track_frags[ti][i]), list.first()) {
                let shift = fr.start as i64 - first.dts as i64;
                if shift != 0 {
                    for s in list.iter_mut() {
                        s.dts = (s.dts as i64 + shift).max(0) as u64;
                    }
                }
            }
        }
        let out = parsed.get(&track_id).cloned().unwrap_or_default();
        let mut c = self.cache.lock();
        c.push((frag, parsed));
        if c.len() > CACHE_FRAGS {
            c.remove(0);
        }
        Ok(out)
    }

    // ------------------------------------------------------------ queries

    pub fn track_index(&self, id: u32) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// First video track.
    pub fn video(&self) -> Option<usize> {
        self.tracks.iter().position(|t| t.kind == TrackKind::Video && t.codec.is_video())
    }

    /// Audio tracks we can decode, in file order.
    pub fn audio(&self) -> Vec<usize> {
        (0..self.tracks.len()).filter(|i| self.tracks[*i].kind == TrackKind::Audio && self.tracks[*i].codec.is_audio()).collect()
    }

    /// End of the last sample of a track, in ticks.
    pub fn end_ticks(&self, idx: usize) -> u64 {
        if let Some(f) = self.track_frags.get(idx).and_then(|v| v.last()) {
            return f.end;
        }
        self.flat.get(idx).and_then(|v| v.last().map(|s| s.end())).unwrap_or(0)
    }

    pub fn end_secs(&self, idx: usize) -> f64 {
        self.end_ticks(idx) as f64 / self.tracks[idx].timescale as f64
    }

    /// Presented duration in seconds (longest track, honouring edit lists).
    pub fn duration_secs(&self) -> f64 {
        (0..self.tracks.len())
            .filter(|i| self.tracks[*i].kind != TrackKind::Other)
            .map(|i| {
                let t = &self.tracks[i];
                let end = self.end_secs(i);
                match t.edit {
                    Some((m, d)) if d > 0 => (d as f64 / t.timescale as f64).min(end - m.max(0) as f64 / t.timescale as f64),
                    Some((m, _)) => end - m.max(0) as f64 / t.timescale as f64,
                    None => end,
                }
            })
            .fold(0.0, f64::max)
    }

    /// Visible frame starts in presentation order, in track ticks relative to the edit.
    /// Flat files already have an in-memory sample table. For a growing archive
    /// only inspect the fragment around the cursor and its two neighbors.
    pub fn frame_starts_near(&self, idx: usize, position: i64) -> Result<Vec<i64>> {
        let track = &self.tracks[idx];
        let (edit, length) = track.edit.unwrap_or((0, 0));
        let edit = edit.max(0);
        let end = if length > 0 { edit.saturating_add_unsigned(length) } else { i64::MAX };
        let mut starts = Vec::new();
        let mut collect = |samples: &[Sample]| {
            starts.extend(samples.iter().filter_map(|s| {
                let t = s.pts();
                (t < end && t.saturating_add(i64::from(s.duration)) > edit).then(|| t.saturating_sub(edit).max(0))
            }));
        };
        if let Some(flat) = self.flat.get(idx).filter(|flat| !flat.is_empty()) {
            collect(flat);
        } else if let Some(list) = self.track_frags.get(idx).filter(|list| !list.is_empty()) {
            let at = position.max(0).saturating_add(edit) as u64;
            let index = list.partition_point(|f| f.end <= at).min(list.len() - 1);
            for f in &list[index.saturating_sub(1)..(index + 2).min(list.len())] {
                collect(&self.fragment_samples(f.frag, track.id)?);
            }
        }
        starts.sort_unstable();
        starts.dedup();
        Ok(starts)
    }

    /// Samples of track `idx` overlapping `[from, to)` ticks, in order.
    pub fn samples(&self, idx: usize, from: u64, to: u64) -> Result<Vec<Sample>> {
        if to <= from {
            return Ok(Vec::new());
        }
        if let Some(flat) = self.flat.get(idx).filter(|v| !v.is_empty()) {
            let a = flat.partition_point(|s| s.end() <= from);
            let b = flat.partition_point(|s| s.dts < to);
            return Ok(flat[a..b.max(a)].to_vec());
        }
        let Some(list) = self.track_frags.get(idx) else { return Ok(Vec::new()) };
        let id = self.tracks[idx].id;
        let a = list.partition_point(|f| f.end <= from);
        let mut out = Vec::new();
        for f in &list[a..] {
            if f.start >= to {
                break;
            }
            for s in self.fragment_samples(f.frag, id)? {
                if s.end() > from && s.dts < to {
                    out.push(s);
                }
            }
        }
        Ok(out)
    }

    /// The last sync sample of track `idx` starting at or before `t` ticks
    /// (or the first sync sample after it if there is none before).
    pub fn sync_at_or_before(&self, idx: usize, t: u64) -> Result<Option<Sample>> {
        if let Some(flat) = self.flat.get(idx).filter(|v| !v.is_empty()) {
            let upto = flat.partition_point(|s| s.dts <= t);
            if let Some(s) = flat[..upto].iter().rev().find(|s| s.sync) {
                return Ok(Some(*s));
            }
            return Ok(flat[upto..].iter().find(|s| s.sync).copied());
        }
        let Some(list) = self.track_frags.get(idx) else { return Ok(None) };
        let id = self.tracks[idx].id;
        let k = list.partition_point(|f| f.start <= t);
        // Walk back from the fragment containing t.
        for f in list[..k].iter().rev() {
            let s = self.fragment_samples(f.frag, id)?;
            if let Some(x) = s.iter().rev().find(|x| x.sync && x.dts <= t) {
                return Ok(Some(*x));
            }
        }
        for f in &list[k..] {
            let s = self.fragment_samples(f.frag, id)?;
            if let Some(x) = s.iter().find(|x| x.sync) {
                return Ok(Some(*x));
            }
        }
        Ok(None)
    }

    /// All sync sample decode times of a track (seconds), e.g. for snapping.
    pub fn sync_times(&self, idx: usize) -> Result<Vec<f64>> {
        let ts = self.tracks[idx].timescale as f64;
        if let Some(flat) = self.flat.get(idx).filter(|v| !v.is_empty()) {
            return Ok(flat.iter().filter(|s| s.sync).map(|s| s.dts as f64 / ts).collect());
        }
        let mut out = Vec::new();
        if let Some(list) = self.track_frags.get(idx) {
            let id = self.tracks[idx].id;
            for f in list {
                for s in self.fragment_samples(f.frag, id)? {
                    if s.sync {
                        out.push(s.dts as f64 / ts);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Time ranges (ticks) of a track that have samples, merging fragments
    /// that touch. Gaps are where an app's audio track was unused.
    pub fn coverage(&self, idx: usize) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        if let Some(flat) = self.flat.get(idx).filter(|v| !v.is_empty()) {
            out.push((flat[0].dts, flat.last().unwrap().end()));
            return out;
        }
        if let Some(list) = self.track_frags.get(idx) {
            for f in list {
                match out.last_mut() {
                    Some(last) if f.start <= last.1 + 1 => last.1 = last.1.max(f.end),
                    _ => out.push((f.start, f.end)),
                }
            }
        }
        out
    }
}

fn parse_stsd(stsd: &[u8]) -> Result<(Codec, u32, u32)> {
    let mut r = Rd::new(stsd);
    r.full()?;
    let n = r.u32()?;
    if n == 0 {
        bail!("empty stsd");
    }
    let entries = children(&stsd[8..]);
    let (typ, e) = entries.first().context("stsd entry")?;
    match typ {
        b"avc1" | b"avc3" => {
            let mut r = Rd::new(e);
            r.skip(6 + 2 + 16)?;
            let w = r.u16()? as u32;
            let h = r.u16()? as u32;
            let rest = e.get(78..).unwrap_or(&[]);
            let avcc = child(rest, b"avcC").context("avcC")?.to_vec();
            Ok((Codec::H264 { avcc }, w, h))
        }
        b"mp4a" => {
            let mut r = Rd::new(e);
            r.skip(6 + 2 + 8)?;
            let channels = r.u16()?;
            r.skip(6)?;
            let sample_rate = r.u32()? >> 16;
            let rest = e.get(28..).unwrap_or(&[]);
            let esds = child(rest, b"esds").context("esds")?;
            let (asc, bitrate) = parse_esds(esds)?;
            // Trust the AudioSpecificConfig over the (16-bit) entry fields.
            let (sr, ch) = crate::aac::parse_asc(&asc).map(|(_, sr, ch)| (sr, ch)).unwrap_or((sample_rate, channels));
            Ok((Codec::Aac { asc, channels: ch, sample_rate: sr, avg_bitrate: bitrate }, 0, 0))
        }
        other => {
            let mut w = 0;
            let mut h = 0;
            if matches!(other, b"hvc1" | b"hev1" | b"av01" | b"vp09") && e.len() >= 28 {
                w = u16::from_be_bytes([e[24], e[25]]) as u32;
                h = u16::from_be_bytes([e[26], e[27]]) as u32;
            }
            Ok((Codec::Other { fourcc: *other }, w, h))
        }
    }
}

fn desc_len(r: &mut Rd) -> Result<usize> {
    let mut len = 0usize;
    for _ in 0..4 {
        let b = r.u8()?;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok(len)
}

/// AudioSpecificConfig + average bitrate from an `esds` body.
fn parse_esds(esds: &[u8]) -> Result<(Vec<u8>, u32)> {
    let mut r = Rd::new(esds);
    r.full()?;
    let mut bitrate = 0;
    while r.left() > 2 {
        let tag = r.u8()?;
        let len = desc_len(&mut r)?;
        match tag {
            0x03 => {
                r.u16()?;
                let flags = r.u8()?;
                if flags & 0x80 != 0 {
                    r.u16()?;
                }
                if flags & 0x40 != 0 {
                    let n = r.u8()? as usize;
                    r.skip(n)?;
                }
                if flags & 0x20 != 0 {
                    r.u16()?;
                }
            }
            0x04 => {
                r.skip(1 + 1 + 3 + 4)?;
                bitrate = r.u32()?;
            }
            0x05 => return Ok((r.take(len)?.to_vec(), bitrate)),
            _ => r.skip(len)?,
        }
    }
    bail!("no AudioSpecificConfig in esds")
}

/// Expand a regular file's sample tables.
fn parse_sample_tables(stbl: &[u8], video: bool) -> Result<Vec<Sample>> {
    let mut durations: Vec<(u32, u32)> = Vec::new();
    if let Some(b) = child(stbl, b"stts") {
        let mut r = Rd::new(b);
        r.full()?;
        for _ in 0..r.u32()? {
            durations.push((r.u32()?, r.u32()?));
        }
    }
    let total: u64 = durations.iter().map(|(n, _)| *n as u64).sum();
    if total == 0 {
        return Ok(Vec::new());
    }
    if total > 50_000_000 {
        bail!("implausible sample count");
    }
    let n = total as usize;
    let mut sizes = Vec::with_capacity(n);
    if let Some(b) = child(stbl, b"stsz") {
        let mut r = Rd::new(b);
        r.full()?;
        let uniform = r.u32()?;
        let count = r.u32()? as usize;
        if count > n {
            bail!("sample size table larger than the sample list");
        }
        if uniform != 0 {
            sizes.resize(count, uniform);
        } else {
            for _ in 0..count {
                sizes.push(r.u32()?);
            }
        }
    } else if let Some(b) = child(stbl, b"stz2") {
        let mut r = Rd::new(b);
        r.full()?;
        r.u24()?;
        let field = r.u8()?;
        let count = r.u32()? as usize;
        if count > n {
            bail!("sample size table larger than the sample list");
        }
        let mut i = 0;
        while i < count {
            match field {
                4 => {
                    let v = r.u8()?;
                    sizes.push((v >> 4) as u32);
                    if i + 1 < count {
                        sizes.push((v & 0xF) as u32);
                    }
                    i += 2;
                }
                8 => {
                    sizes.push(r.u8()? as u32);
                    i += 1;
                }
                _ => {
                    sizes.push(r.u16()? as u32);
                    i += 1;
                }
            }
        }
    }
    let mut ctts: Vec<(u32, i32)> = Vec::new();
    if let Some(b) = child(stbl, b"ctts") {
        let mut r = Rd::new(b);
        r.full()?;
        for _ in 0..r.u32()? {
            ctts.push((r.u32()?, r.u32()? as i32));
        }
    }
    let mut sync: Option<Vec<u32>> = None;
    if let Some(b) = child(stbl, b"stss") {
        let mut r = Rd::new(b);
        r.full()?;
        let mut v = Vec::new();
        for _ in 0..r.u32()? {
            v.push(r.u32()?);
        }
        sync = Some(v);
    }
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    if let Some(b) = child(stbl, b"stsc") {
        let mut r = Rd::new(b);
        r.full()?;
        for _ in 0..r.u32()? {
            let first = r.u32()?;
            let per = r.u32()?;
            r.u32()?;
            stsc.push((first, per));
        }
    }
    let mut chunks: Vec<u64> = Vec::new();
    if let Some(b) = child(stbl, b"stco") {
        let mut r = Rd::new(b);
        r.full()?;
        for _ in 0..r.u32()? {
            chunks.push(r.u32()? as u64);
        }
    } else if let Some(b) = child(stbl, b"co64") {
        let mut r = Rd::new(b);
        r.full()?;
        for _ in 0..r.u32()? {
            chunks.push(r.u64()?);
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut dts = 0u64;
    let mut dur_iter = durations.iter().flat_map(|(c, d)| std::iter::repeat(*d).take(*c as usize));
    let mut cts_iter = ctts.iter().flat_map(|(c, o)| std::iter::repeat(*o).take(*c as usize));
    let mut sync_iter = sync.as_ref().map(|v| v.iter().peekable());
    let mut sample_no = 0usize;
    for (ci, chunk_off) in chunks.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = stsc.iter().rev().find(|(first, _)| *first <= chunk_no).map(|(_, p)| *p).unwrap_or(0);
        let mut off = *chunk_off;
        for _ in 0..per {
            if sample_no >= n {
                break;
            }
            let size = sizes.get(sample_no).copied().unwrap_or(0);
            let duration = dur_iter.next().unwrap_or(0);
            let cts = cts_iter.next().unwrap_or(0);
            let is_sync = match &mut sync_iter {
                None => true,
                Some(it) => {
                    let num = sample_no as u32 + 1;
                    while it.peek().map(|v| **v < num).unwrap_or(false) {
                        it.next();
                    }
                    it.peek().map(|v| **v == num).unwrap_or(false)
                }
            };
            out.push(Sample { offset: off, size, dts, duration, cts, sync: is_sync || !video });
            off += size as u64;
            dts += duration as u64;
            sample_no += 1;
        }
    }
    Ok(out)
}
