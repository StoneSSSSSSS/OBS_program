//! H.264 bitstream helpers: Annex B <-> length-prefixed (MP4) conversion,
//! parameter-set handling and SPS parsing.

/// Split an Annex B stream into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = data.len();
    // Find the first start code.
    let mut start = None;
    while i + 3 <= n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            start = Some(i + 3);
            i += 3;
            break;
        }
        i += 1;
    }
    let Some(mut s) = start else {
        if !data.is_empty() {
            out.push(data);
        }
        return out;
    };
    while i + 3 <= n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // A 4-byte start code's leading zero belongs to it, not the NAL.
            let mut end = i;
            while end > s && data[end - 1] == 0 {
                end -= 1;
            }
            if end > s {
                out.push(&data[s..end]);
            }
            s = i + 3;
            i += 3;
        } else {
            i += 1;
        }
    }
    if s < n {
        out.push(&data[s..]);
    }
    out
}

pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// An access unit converted for MP4.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Converted {
    /// 4-byte length-prefixed NAL units (access unit delimiters dropped).
    pub data: Vec<u8>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub idr: bool,
}

/// Annex B access unit -> MP4 sample.
pub fn annexb_to_avcc(au: &[u8]) -> Converted {
    let mut c = Converted::default();
    for nal in split_annexb(au) {
        match nal_type(nal) {
            NAL_AUD => continue,
            NAL_SPS => c.sps = Some(nal.to_vec()),
            NAL_PPS => c.pps = Some(nal.to_vec()),
            NAL_IDR => c.idr = true,
            _ => {}
        }
        c.data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        c.data.extend_from_slice(nal);
    }
    c
}

/// MP4 sample -> Annex B, optionally prefixed with parameter sets (decoders
/// need them before the first IDR; harmless elsewhere).
pub fn avcc_to_annexb(sample: &[u8], len_size: usize, params: Option<(&[Vec<u8>], &[Vec<u8>])>) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len() + 64);
    let mut has_sps = false;
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut p = 0usize;
    let ls = len_size.clamp(1, 4);
    while p + ls <= sample.len() {
        let mut len = 0usize;
        for k in 0..ls {
            len = (len << 8) | sample[p + k] as usize;
        }
        p += ls;
        if len == 0 || p + len > sample.len() {
            break;
        }
        let nal = &sample[p..p + len];
        if nal_type(nal) == NAL_SPS {
            has_sps = true;
        }
        nals.push(nal);
        p += len;
    }
    if let (Some((sps, pps)), false) = (params, has_sps) {
        if nals.iter().any(|n| nal_type(n) == NAL_IDR) {
            for s in sps.iter().chain(pps.iter()) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(s);
            }
        }
    }
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// AVCDecoderConfigurationRecord for one SPS/PPS pair.
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = vec![1, sps.get(1).copied().unwrap_or(100), sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(40), 0xFF, 0xE1];
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1);
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

/// (NAL length size, SPS list, PPS list) from an avcC record.
pub fn parse_avcc(avcc: &[u8]) -> Option<(usize, Vec<Vec<u8>>, Vec<Vec<u8>>)> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return None;
    }
    let len_size = (avcc[4] & 3) as usize + 1;
    let mut p = 5;
    let nsps = (avcc[p] & 0x1F) as usize;
    p += 1;
    let mut sps = Vec::new();
    for _ in 0..nsps {
        let l = u16::from_be_bytes([*avcc.get(p)?, *avcc.get(p + 1)?]) as usize;
        p += 2;
        sps.push(avcc.get(p..p + l)?.to_vec());
        p += l;
    }
    let npps = *avcc.get(p)? as usize;
    p += 1;
    let mut pps = Vec::new();
    for _ in 0..npps {
        let l = u16::from_be_bytes([*avcc.get(p)?, *avcc.get(p + 1)?]) as usize;
        p += 2;
        pps.push(avcc.get(p..p + l)?.to_vec());
        p += l;
    }
    Some((len_size, sps, pps))
}

// ------------------------------------------------------------ SPS parsing

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let v = self.ue()?;
        Some(if v & 1 == 1 { (v as i32 + 1) / 2 } else { -(v as i32 / 2) })
    }
}

/// Remove emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Coded picture size (after cropping) from an SPS NAL unit.
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    let raw = unescape(sps);
    let mut b = Bits { d: &raw, pos: 8 };
    let profile = b.bits(8)?;
    b.bits(16)?; // constraints + level
    b.ue()?; // sps id
    let mut chroma = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?;
        }
        b.ue()?;
        b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            let lists = if chroma == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num
    let poc_type = b.ue()?;
    if poc_type == 0 {
        b.ue()?;
    } else if poc_type == 1 {
        b.bit()?;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        for _ in 0..n {
            b.se()?;
        }
    }
    b.ue()?; // max refs
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?;
    let mut w = w_mbs * 16;
    let mut h = h_units * 16 * (2 - frame_mbs_only);
    if b.bit()? == 1 {
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (cx, cy) = match chroma {
            0 => (1, 2 - frame_mbs_only),
            1 => (2, 2 * (2 - frame_mbs_only)),
            2 => (2, 2 - frame_mbs_only),
            _ => (1, 2 - frame_mbs_only),
        };
        w = w.saturating_sub((l + r) * cx);
        h = h.saturating_sub((t + bo) * cy);
    }
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real 1920x1080 High-profile SPS (with cropping 1088 -> 1080).
    // (Extracted from x264 output.)
    const SPS_1080: [u8; 27] = [0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58];

    #[test]
    fn annexb_roundtrip() {
        let au = [0, 0, 0, 1, 9, 0xF0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0xAA, 0xBB, 0];
        let c = annexb_to_avcc(&au);
        assert!(c.idr);
        assert_eq!(c.sps.as_deref(), Some(&[0x67, 1, 2][..]));
        assert_eq!(c.pps.as_deref(), Some(&[0x68, 3][..]));
        // AUD dropped; 3 NALs remain.
        assert_eq!(&c.data[..4], &[0, 0, 0, 3]);
        let back = avcc_to_annexb(&c.data, 4, None);
        assert_eq!(split_annexb(&back), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 0xAA, 0xBB, 0]]);
    }

    #[test]
    fn params_prepended_only_before_idr_without_sps() {
        let idr = [0, 0, 0, 2, 0x65, 1];
        let sps = vec![vec![0x67, 9]];
        let pps = vec![vec![0x68, 8]];
        let out = avcc_to_annexb(&idr, 4, Some((&sps, &pps)));
        assert_eq!(split_annexb(&out), vec![&[0x67, 9][..], &[0x68, 8], &[0x65, 1]]);
        let p = [0, 0, 0, 2, 0x41, 1];
        let out = avcc_to_annexb(&p, 4, Some((&sps, &pps)));
        assert_eq!(split_annexb(&out), vec![&[0x41, 1][..]]);
    }

    #[test]
    fn avcc_record() {
        let a = build_avcc(&SPS_1080, &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0]);
        let (ls, s, p) = parse_avcc(&a).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(s[0], SPS_1080.to_vec());
        assert_eq!(p[0].len(), 6);
        assert_eq!(a[1], 0x64);
    }

    #[test]
    fn sps_size() {
        assert_eq!(sps_dimensions(&SPS_1080), Some((1920, 1080)));
    }
}
