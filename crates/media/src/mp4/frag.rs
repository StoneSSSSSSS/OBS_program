//! Fragmented MP4 writer for the continuous recording.
//!
//! Layout: `ftyp`, `moov` (track descriptions, no samples, `mvex`), optional
//! ClipVault `free` metadata, then repeated `[free meta]` `moof` `mdat`. Each
//! fragment holds every track's samples for one keyframe interval, so the file
//! is playable up to the last complete fragment even if writing stops abruptly.

use super::*;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Samples of one track inside a fragment.
pub struct TrackRun<'a> {
    pub track_id: u32,
    /// Decode time of the first sample, in track ticks since the file start.
    pub base_time: u64,
    pub samples: &'a [SampleRef<'a>],
}

pub struct FragWriter {
    w: BufWriter<File>,
    tracks: Vec<TrackSpec>,
    seq: u32,
    written: u64,
}

impl FragWriter {
    /// Create the file and write its header. `created_unix_s` is stored in the
    /// header; `meta` (if any) goes into a ClipVault metadata box after it.
    pub fn create(path: &Path, tracks: &[TrackSpec], created_unix_s: u64, meta: Option<&[u8]>) -> io::Result<FragWriter> {
        let file = File::create(path)?;
        let mut me = FragWriter { w: BufWriter::with_capacity(1 << 20, file), tracks: tracks.to_vec(), seq: 0, written: 0 };
        let mut b = Buf::default();
        write_ftyp(&mut b, true);
        write_moov(&mut b, tracks, created_unix_s + MP4_EPOCH_OFFSET);
        if let Some(m) = meta {
            write_meta_box(&mut b, m);
        }
        me.put(&b.0)?;
        me.w.flush()?;
        Ok(me)
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    pub fn tracks(&self) -> &[TrackSpec] {
        &self.tracks
    }

    /// Bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Append a ClipVault metadata box (applies from the next fragment on).
    pub fn write_meta(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut b = Buf::default();
        write_meta_box(&mut b, payload);
        self.put(&b.0)
    }

    /// Write one fragment. Runs with no samples are skipped; tracks without a
    /// run simply have nothing in this fragment (a gap in their timeline).
    pub fn write_fragment(&mut self, runs: &[TrackRun]) -> io::Result<()> {
        let runs: Vec<&TrackRun> = runs.iter().filter(|r| !r.samples.is_empty()).collect();
        if runs.is_empty() {
            return Ok(());
        }
        self.seq += 1;
        let mut b = Buf::default();
        let moof = b.begin(b"moof");
        let mfhd = b.begin_full(b"mfhd", 0, 0);
        b.u32(self.seq);
        b.end(mfhd);
        let mut offset_slots = Vec::with_capacity(runs.len());
        for r in &runs {
            let video = self.tracks.iter().find(|t| t.id == r.track_id).map(|t| t.codec.is_video()).unwrap_or(false);
            let traf = b.begin(b"traf");
            let tfhd = b.begin_full(b"tfhd", 0, 0x02_0000); // default-base-is-moof
            b.u32(r.track_id);
            b.end(tfhd);
            let tfdt = b.begin_full(b"tfdt", 1, 0);
            b.u64(r.base_time);
            b.end(tfdt);
            let has_cts = r.samples.iter().any(|s| s.cts != 0);
            let mut flags = 0x000001 | 0x000100 | 0x000200 | 0x000400;
            if has_cts {
                flags |= 0x000800;
            }
            let trun = b.begin_full(b"trun", if has_cts { 1 } else { 0 }, flags);
            b.u32(r.samples.len() as u32);
            offset_slots.push(b.len());
            b.i32(0); // data offset, patched below
            for s in r.samples {
                b.u32(s.duration);
                b.u32(s.data.len() as u32);
                b.u32(sample_flags(s.sync, video));
                if has_cts {
                    b.i32(s.cts);
                }
            }
            b.end(trun);
            b.end(traf);
        }
        b.end(moof);
        let moof_size = b.len() as u64;
        let total: u64 = runs.iter().flat_map(|r| r.samples.iter()).map(|s| s.data.len() as u64).sum();
        // Data offsets are relative to the start of the moof.
        let mut off = moof_size + 8;
        let large = total + 8 > u32::MAX as u64;
        if large {
            off += 8;
        }
        for (slot, r) in offset_slots.iter().zip(&runs) {
            b.patch_u32(*slot, off as u32);
            off += r.samples.iter().map(|s| s.data.len() as u64).sum::<u64>();
        }
        if large {
            b.u32(1);
            b.bytes(b"mdat");
            b.u64(total + 16);
        } else {
            b.u32((total + 8) as u32);
            b.bytes(b"mdat");
        }
        self.put(&b.0)?;
        for r in &runs {
            for s in r.samples {
                self.w.write_all(s.data)?;
            }
        }
        self.written += total;
        Ok(())
    }

    /// Push buffered bytes to the OS (so readers of the live file see them).
    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    /// Flush and close; returns the file size.
    pub fn finish(mut self) -> io::Result<u64> {
        self.w.flush()?;
        let f = self.w.into_inner().map_err(|e| e.into_error())?;
        let _ = f.sync_all();
        Ok(self.written)
    }
}

fn write_moov(b: &mut Buf, tracks: &[TrackSpec], created: u64) {
    let moov = b.begin(b"moov");
    let next = tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    write_mvhd(b, created, 1000, 0, next);
    for t in tracks {
        let video = t.codec.is_video();
        let trak = b.begin(b"trak");
        write_tkhd(b, t, created, 0);
        let mdia = b.begin(b"mdia");
        write_mdhd(b, t.timescale, created, 0);
        write_hdlr(b, video, &t.name);
        let minf = b.begin(b"minf");
        write_media_header_box(b, video);
        let stbl = b.begin(b"stbl");
        write_stsd(b, t);
        for typ in [b"stts", b"stsc", b"stco"] {
            let p = b.begin_full(typ, 0, 0);
            b.u32(0);
            b.end(p);
        }
        let p = b.begin_full(b"stsz", 0, 0);
        b.u32(0);
        b.u32(0);
        b.end(p);
        b.end(stbl);
        b.end(minf);
        b.end(mdia);
        b.end(trak);
    }
    let mvex = b.begin(b"mvex");
    for t in tracks {
        let p = b.begin_full(b"trex", 0, 0);
        b.u32(t.id);
        b.u32(1);
        b.u32(t.default_duration);
        b.u32(0);
        b.u32(sample_flags(false, t.codec.is_video()));
        b.end(p);
    }
    b.end(mvex);
    b.end(moov);
}
