//! MP4 (ISO BMFF) writing and reading, written from scratch so ClipVault
//! needs no external tools.
//!
//! * [`frag::FragWriter`] writes the 24/7 recording as *fragmented* MP4: a
//!   header (`moov`) up front, then one self-contained `moof`+`mdat` pair per
//!   keyframe interval. A crash or power cut loses at most the last fragment.
//! * [`flat::FlatWriter`] writes clips and exports as regular "fast start"
//!   MP4 (index first), which every player, browser and Discord understands.
//! * [`reader::Mp4Reader`] reads both kinds (including files written by OBS or
//!   ffmpeg), lazily indexes fragments, and can follow a file that is still
//!   being written.
//!
//! ClipVault's own metadata (which app is on which audio track, etc.) is kept
//! in `free` boxes that start with [`META_MAGIC`]; other programs ignore them.

pub mod flat;
pub mod frag;
pub mod reader;

/// Payloads of `free` boxes that start with these bytes are ClipVault metadata.
pub const META_MAGIC: &[u8; 8] = b"CVMETA1\0";

/// Codec configuration for a track.
#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    /// H.264 with an `AVCDecoderConfigurationRecord`.
    H264 { avcc: Vec<u8> },
    /// AAC with its `AudioSpecificConfig`.
    Aac { asc: Vec<u8>, channels: u16, sample_rate: u32, avg_bitrate: u32 },
    /// Anything else we can read but not decode (kept so track indices line up).
    Other { fourcc: [u8; 4] },
}

impl Codec {
    pub fn is_video(&self) -> bool {
        matches!(self, Codec::H264 { .. })
    }
    pub fn is_audio(&self) -> bool {
        matches!(self, Codec::Aac { .. })
    }
}

/// What a writer needs to know about a track.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackSpec {
    /// 1-based track id.
    pub id: u32,
    /// Ticks per second for this track's timestamps.
    pub timescale: u32,
    pub codec: Codec,
    /// Video size (0 for audio).
    pub width: u16,
    pub height: u16,
    /// Shown by players as the track name (`hdlr` name).
    pub name: String,
    /// Disabled tracks are present but not played by default in other players.
    pub enabled: bool,
    /// Typical sample duration (used as the fragment default).
    pub default_duration: u32,
}

/// One sample to write.
#[derive(Debug, Clone, Copy)]
pub struct SampleRef<'a> {
    pub data: &'a [u8],
    /// In track timescale ticks.
    pub duration: u32,
    pub sync: bool,
    /// Composition offset (pts - dts) in ticks. ClipVault's encoders don't
    /// use B-frames, so this is normally 0.
    pub cts: i32,
}

// ------------------------------------------------------------ byte writer

/// Big-endian box builder.
#[derive(Default)]
pub(crate) struct Buf(pub Vec<u8>);

impl Buf {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u24(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes()[1..]);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Start a box; returns its position for [`Buf::end`].
    pub fn begin(&mut self, typ: &[u8; 4]) -> usize {
        let pos = self.0.len();
        self.u32(0);
        self.bytes(typ);
        pos
    }
    /// Start a "full box" (version + flags).
    pub fn begin_full(&mut self, typ: &[u8; 4], version: u8, flags: u32) -> usize {
        let pos = self.begin(typ);
        self.u8(version);
        self.u24(flags);
        pos
    }
    pub fn end(&mut self, pos: usize) {
        let size = (self.0.len() - pos) as u32;
        self.0[pos..pos + 4].copy_from_slice(&size.to_be_bytes());
    }
    pub fn patch_u32(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }
}

/// Seconds between 1904-01-01 (MP4 epoch) and 1970-01-01.
pub const MP4_EPOCH_OFFSET: u64 = 2_082_844_800;

pub(crate) const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// `ftyp` box.
pub(crate) fn write_ftyp(b: &mut Buf, fragmented: bool) {
    let p = b.begin(b"ftyp");
    b.bytes(b"isom");
    b.u32(0x200);
    if fragmented {
        b.bytes(b"isom");
        b.bytes(b"iso6");
        b.bytes(b"avc1");
        b.bytes(b"mp41");
    } else {
        b.bytes(b"isom");
        b.bytes(b"iso2");
        b.bytes(b"avc1");
        b.bytes(b"mp41");
    }
    b.end(p);
}

/// A `free` box carrying ClipVault metadata.
pub(crate) fn write_meta_box(b: &mut Buf, payload: &[u8]) {
    let p = b.begin(b"free");
    b.bytes(META_MAGIC);
    b.bytes(payload);
    b.end(p);
}

/// `stsd` with one sample entry for `t`.
pub(crate) fn write_stsd(b: &mut Buf, t: &TrackSpec) {
    let p = b.begin_full(b"stsd", 0, 0);
    b.u32(1);
    match &t.codec {
        Codec::H264 { avcc } => {
            let e = b.begin(b"avc1");
            b.zeros(6);
            b.u16(1); // data reference index
            b.u16(0);
            b.u16(0);
            b.zeros(12);
            b.u16(t.width);
            b.u16(t.height);
            b.u32(0x0048_0000);
            b.u32(0x0048_0000);
            b.u32(0);
            b.u16(1); // frame count
            let mut name = [0u8; 32];
            let label = b"ClipVault H.264";
            name[0] = label.len() as u8;
            name[1..1 + label.len()].copy_from_slice(label);
            b.bytes(&name);
            b.u16(0x0018);
            b.u16(0xFFFF);
            let c = b.begin(b"avcC");
            b.bytes(avcc);
            b.end(c);
            // BT.709, limited range: what the capture pipeline produces.
            let c = b.begin(b"colr");
            b.bytes(b"nclx");
            b.u16(1);
            b.u16(1);
            b.u16(1);
            b.u8(0);
            b.end(c);
            let c = b.begin(b"pasp");
            b.u32(1);
            b.u32(1);
            b.end(c);
            b.end(e);
        }
        Codec::Aac { asc, channels, sample_rate, avg_bitrate } => {
            let e = b.begin(b"mp4a");
            b.zeros(6);
            b.u16(1);
            b.zeros(8);
            b.u16(*channels);
            b.u16(16);
            b.u16(0);
            b.u16(0);
            b.u32((*sample_rate).min(65535) << 16);
            write_esds(b, t.id, asc, *avg_bitrate);
            b.end(e);
        }
        Codec::Other { fourcc } => {
            let e = b.begin(fourcc);
            b.zeros(6);
            b.u16(1);
            b.end(e);
        }
    }
    b.end(p);
}

fn write_descriptor_len(b: &mut Buf, len: usize) {
    // Always 4-byte form (what most muxers do): 0x80 0x80 0x80 len.
    b.u8(0x80 | ((len >> 21) & 0x7F) as u8);
    b.u8(0x80 | ((len >> 14) & 0x7F) as u8);
    b.u8(0x80 | ((len >> 7) & 0x7F) as u8);
    b.u8((len & 0x7F) as u8);
}

fn write_esds(b: &mut Buf, es_id: u32, asc: &[u8], avg_bitrate: u32) {
    let p = b.begin_full(b"esds", 0, 0);
    let dsi_len = asc.len();
    let dcd_len = 13 + 5 + dsi_len;
    let es_len = 3 + 5 + dcd_len + 5 + 1;
    b.u8(0x03);
    write_descriptor_len(b, es_len);
    b.u16(es_id as u16);
    b.u8(0);
    b.u8(0x04);
    write_descriptor_len(b, dcd_len);
    b.u8(0x40); // MPEG-4 audio
    b.u8(0x15); // audio stream
    b.u24(0x1800); // buffer size
    b.u32(avg_bitrate);
    b.u32(avg_bitrate);
    b.u8(0x05);
    write_descriptor_len(b, dsi_len);
    b.bytes(asc);
    b.u8(0x06);
    write_descriptor_len(b, 1);
    b.u8(0x02);
    b.end(p);
}

pub(crate) fn write_hdlr(b: &mut Buf, video: bool, name: &str) {
    let p = b.begin_full(b"hdlr", 0, 0);
    b.u32(0);
    b.bytes(if video { b"vide" } else { b"soun" });
    b.zeros(12);
    b.bytes(name.as_bytes());
    b.u8(0);
    b.end(p);
}

pub(crate) fn write_media_header_box(b: &mut Buf, video: bool) {
    if video {
        let p = b.begin_full(b"vmhd", 0, 1);
        b.zeros(8);
        b.end(p);
    } else {
        let p = b.begin_full(b"smhd", 0, 0);
        b.zeros(4);
        b.end(p);
    }
    let d = b.begin(b"dinf");
    let r = b.begin_full(b"dref", 0, 0);
    b.u32(1);
    let u = b.begin_full(b"url ", 0, 1);
    b.end(u);
    b.end(r);
    b.end(d);
}

pub(crate) fn write_tkhd(b: &mut Buf, t: &TrackSpec, created: u64, duration_movie: u64) {
    let video = t.codec.is_video();
    let flags = if t.enabled { 0x3 } else { 0x2 };
    let p = b.begin_full(b"tkhd", 1, flags);
    b.u64(created);
    b.u64(created);
    b.u32(t.id);
    b.u32(0);
    b.u64(duration_movie);
    b.zeros(8);
    b.u16(0); // layer
    b.u16(if video { 0 } else { 1 }); // audio tracks are alternatives of each other
    b.u16(if video { 0 } else { 0x0100 });
    b.u16(0);
    for m in UNITY_MATRIX {
        b.u32(m);
    }
    b.u32((t.width as u32) << 16);
    b.u32((t.height as u32) << 16);
    b.end(p);
}

pub(crate) fn write_mdhd(b: &mut Buf, timescale: u32, created: u64, duration: u64) {
    let p = b.begin_full(b"mdhd", 1, 0);
    b.u64(created);
    b.u64(created);
    b.u32(timescale);
    b.u64(duration);
    b.u16(0x55C4); // "und"
    b.u16(0);
    b.end(p);
}

pub(crate) fn write_mvhd(b: &mut Buf, created: u64, timescale: u32, duration: u64, next_track: u32) {
    let p = b.begin_full(b"mvhd", 1, 0);
    b.u64(created);
    b.u64(created);
    b.u32(timescale);
    b.u64(duration);
    b.u32(0x0001_0000);
    b.u16(0x0100);
    b.zeros(10);
    for m in UNITY_MATRIX {
        b.u32(m);
    }
    b.zeros(24);
    b.u32(next_track);
    b.end(p);
}

/// Sample flags for `trun`/`trex`.
pub(crate) fn sample_flags(sync: bool, video: bool) -> u32 {
    if !video || sync {
        0x0200_0000 // depends on no other sample
    } else {
        0x0101_0000 // depends on others, non-sync
    }
}

#[cfg(test)]
mod tests;
