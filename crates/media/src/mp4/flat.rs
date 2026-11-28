//! Regular ("fast start") MP4 writer for clips and exports.
//!
//! Sample data is streamed to a temporary file next to the destination while
//! the index is kept in memory; `finish` writes `ftyp`, `moov` (index first, so
//! browsers and Discord can play while downloading), then the data.

use super::*;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: u32,
    duration: u32,
    sync: bool,
    cts: i32,
}

struct Track {
    spec: TrackSpec,
    samples: Vec<Entry>,
    /// (offset in data, number of samples).
    chunks: Vec<(u64, u32)>,
    /// (media time where presentation starts, presented duration in track ticks).
    edit: Option<(u64, u64)>,
}

pub struct FlatWriter {
    tmp_path: PathBuf,
    tmp: BufWriter<File>,
    data_len: u64,
    tracks: Vec<Track>,
    last_track: Option<usize>,
    created: u64,
    movie_padding: u64,
}

impl FlatWriter {
    /// Start writing; `dest` is only created by [`FlatWriter::finish`].
    pub fn new(dest: &Path, tracks: &[TrackSpec], created_unix_s: u64) -> io::Result<FlatWriter> {
        let tmp_path = dest.with_extension("cvpart");
        let tmp = BufWriter::with_capacity(1 << 20, File::create(&tmp_path)?);
        Ok(FlatWriter {
            tmp_path,
            tmp,
            data_len: 0,
            tracks: tracks.iter().map(|s| Track { spec: s.clone(), samples: Vec::new(), chunks: Vec::new(), edit: None }).collect(),
            last_track: None,
            created: created_unix_s + MP4_EPOCH_OFFSET,
            movie_padding: 0,
        })
    }

    /// Keep the fast-start layout when appending an editor presentation edit.
    pub(crate) fn reserve_presentation_edit_space(&mut self) {
        self.movie_padding = 1024;
    }

    /// Index of the track with this id.
    pub fn track_index(&self, id: u32) -> Option<usize> {
        self.tracks.iter().position(|t| t.spec.id == id)
    }

    /// Append a sample to track `idx`. Feed samples roughly in time order
    /// across tracks; consecutive samples of one track form a chunk.
    pub fn add(&mut self, idx: usize, s: &SampleRef) -> io::Result<()> {
        self.tmp.write_all(s.data)?;
        let t = &mut self.tracks[idx];
        t.samples.push(Entry { size: s.data.len() as u32, duration: s.duration, sync: s.sync, cts: s.cts });
        let new_chunk = self.last_track != Some(idx) || t.chunks.last().map(|c| c.1 >= 512).unwrap_or(true);
        if new_chunk {
            t.chunks.push((self.data_len, 1));
        } else if let Some(c) = t.chunks.last_mut() {
            c.1 += 1;
        }
        self.last_track = Some(idx);
        self.data_len += s.data.len() as u64;
        Ok(())
    }

    /// Present track `idx` starting `media_start` ticks into its samples, for
    /// `duration` ticks (an edit list; hides a keyframe pre-roll).
    pub fn set_edit(&mut self, idx: usize, media_start: u64, duration: u64) {
        self.tracks[idx].edit = Some((media_start, duration));
    }

    /// Total media duration of a track in its ticks.
    pub fn track_ticks(&self, idx: usize) -> u64 {
        self.tracks[idx].samples.iter().map(|e| e.duration as u64).sum()
    }

    pub fn sample_count(&self, idx: usize) -> usize {
        self.tracks[idx].samples.len()
    }

    /// Write the final file. Tracks without samples are dropped.
    pub fn finish(mut self, dest: &Path, meta: Option<&[u8]>) -> io::Result<u64> {
        self.tmp.flush()?;
        drop(self.tmp);
        self.tracks.retain(|t| !t.samples.is_empty());
        let mut head = Buf::default();
        write_ftyp(&mut head, false);
        let ftyp_len = head.len() as u64;
        let meta_len = meta.map(|m| 8 + META_MAGIC.len() as u64 + m.len() as u64).unwrap_or(0);
        let large_mdat = self.data_len + 8 > u32::MAX as u64;
        let mdat_hdr = if large_mdat { 16 } else { 8 };
        // moov size doesn't depend on the chunk offsets' values, only on
        // whether they need 64 bits: build once to measure, once for real.
        let probe = build_moov(&self.tracks, self.created, 0, false);
        let mut base = ftyp_len + probe.len() as u64 + meta_len + self.movie_padding + mdat_hdr;
        let co64 = base + self.data_len > u32::MAX as u64;
        if co64 {
            let probe = build_moov(&self.tracks, self.created, 0, true);
            base = ftyp_len + probe.len() as u64 + meta_len + self.movie_padding + mdat_hdr;
        }
        let moov = build_moov(&self.tracks, self.created, base, co64);
        head.bytes(&moov.0);
        if self.movie_padding != 0 {
            let free = head.begin(b"free");
            head.zeros(self.movie_padding as usize - 8);
            head.end(free);
        }
        if let Some(m) = meta {
            write_meta_box(&mut head, m);
        }
        if large_mdat {
            head.u32(1);
            head.bytes(b"mdat");
            head.u64(self.data_len + 16);
        } else {
            head.u32((self.data_len + 8) as u32);
            head.bytes(b"mdat");
        }
        let out_tmp = dest.with_extension("cvwrite");
        {
            let mut out = BufWriter::with_capacity(1 << 20, File::create(&out_tmp)?);
            out.write_all(&head.0)?;
            let mut src = File::open(&self.tmp_path)?;
            src.seek(SeekFrom::Start(0))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
            out.flush()?;
            out.get_ref().sync_all().ok();
        }
        let _ = std::fs::remove_file(&self.tmp_path);
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        std::fs::rename(&out_tmp, dest)?;
        Ok(head.len() as u64 + self.data_len)
    }

    /// Throw away the partial output.
    pub fn abort(self) {
        drop(self.tmp);
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

fn movie_ticks(ticks: u64, timescale: u32) -> u64 {
    (ticks as u128 * 1000 / timescale.max(1) as u128) as u64
}

fn presented_ticks(t: &Track) -> u64 {
    match t.edit {
        Some((_, d)) => d,
        None => t.samples.iter().map(|e| e.duration as u64).sum(),
    }
}

fn build_moov(tracks: &[Track], created: u64, data_base: u64, co64: bool) -> Buf {
    let mut b = Buf::default();
    let moov = b.begin(b"moov");
    let movie_dur = tracks.iter().map(|t| movie_ticks(presented_ticks(t), t.spec.timescale)).max().unwrap_or(0);
    let next = tracks.iter().map(|t| t.spec.id).max().unwrap_or(0) + 1;
    write_mvhd(&mut b, created, 1000, movie_dur, next);
    for t in tracks {
        let video = t.spec.codec.is_video();
        let media_dur: u64 = t.samples.iter().map(|e| e.duration as u64).sum();
        let trak = b.begin(b"trak");
        write_tkhd(&mut b, &t.spec, created, movie_ticks(presented_ticks(t), t.spec.timescale));
        if let Some((start, dur)) = t.edit {
            let edts = b.begin(b"edts");
            let elst = b.begin_full(b"elst", 1, 0);
            b.u32(1);
            b.u64(movie_ticks(dur, t.spec.timescale));
            b.u64(start);
            b.u16(1);
            b.u16(0);
            b.end(elst);
            b.end(edts);
        }
        let mdia = b.begin(b"mdia");
        write_mdhd(&mut b, t.spec.timescale, created, media_dur);
        write_hdlr(&mut b, video, &t.spec.name);
        let minf = b.begin(b"minf");
        write_media_header_box(&mut b, video);
        let stbl = b.begin(b"stbl");
        write_stsd(&mut b, &t.spec);
        // stts: run-length durations.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for e in &t.samples {
            match runs.last_mut() {
                Some((n, d)) if *d == e.duration => *n += 1,
                _ => runs.push((1, e.duration)),
            }
        }
        let p = b.begin_full(b"stts", 0, 0);
        b.u32(runs.len() as u32);
        for (n, d) in &runs {
            b.u32(*n);
            b.u32(*d);
        }
        b.end(p);
        if t.samples.iter().any(|e| e.cts != 0) {
            let mut cr: Vec<(u32, i32)> = Vec::new();
            for e in &t.samples {
                match cr.last_mut() {
                    Some((n, c)) if *c == e.cts => *n += 1,
                    _ => cr.push((1, e.cts)),
                }
            }
            let p = b.begin_full(b"ctts", 1, 0);
            b.u32(cr.len() as u32);
            for (n, c) in &cr {
                b.u32(*n);
                b.i32(*c);
            }
            b.end(p);
        }
        if video && t.samples.iter().any(|e| !e.sync) {
            let syncs: Vec<u32> = t.samples.iter().enumerate().filter(|(_, e)| e.sync).map(|(i, _)| i as u32 + 1).collect();
            let p = b.begin_full(b"stss", 0, 0);
            b.u32(syncs.len() as u32);
            for s in syncs {
                b.u32(s);
            }
            b.end(p);
        }
        // stsc: chunk runs with the same sample count.
        let mut sc: Vec<(u32, u32)> = Vec::new();
        for (i, c) in t.chunks.iter().enumerate() {
            if sc.last().map(|(_, n)| *n != c.1).unwrap_or(true) {
                sc.push((i as u32 + 1, c.1));
            }
        }
        let p = b.begin_full(b"stsc", 0, 0);
        b.u32(sc.len() as u32);
        for (first, n) in &sc {
            b.u32(*first);
            b.u32(*n);
            b.u32(1);
        }
        b.end(p);
        let first = t.samples.first().map(|e| e.size).unwrap_or(0);
        let uniform = t.samples.iter().all(|e| e.size == first);
        let p = b.begin_full(b"stsz", 0, 0);
        if uniform {
            b.u32(first);
            b.u32(t.samples.len() as u32);
        } else {
            b.u32(0);
            b.u32(t.samples.len() as u32);
            for e in &t.samples {
                b.u32(e.size);
            }
        }
        b.end(p);
        if co64 {
            let p = b.begin_full(b"co64", 0, 0);
            b.u32(t.chunks.len() as u32);
            for c in &t.chunks {
                b.u64(data_base + c.0);
            }
            b.end(p);
        } else {
            let p = b.begin_full(b"stco", 0, 0);
            b.u32(t.chunks.len() as u32);
            for c in &t.chunks {
                b.u32((data_base + c.0) as u32);
            }
            b.end(p);
        }
        b.end(stbl);
        b.end(minf);
        b.end(mdia);
        b.end(trak);
    }
    b.end(moov);
    b
}
