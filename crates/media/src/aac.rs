//! AAC configuration helpers.

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// AudioSpecificConfig for AAC-LC.
pub fn asc_lc(sample_rate: u32, channels: u16) -> Vec<u8> {
    let idx = RATES.iter().position(|r| *r == sample_rate).unwrap_or(3) as u16;
    let v: u16 = (2 << 11) | (idx << 7) | ((channels.min(7)) << 3);
    v.to_be_bytes().to_vec()
}

/// (audio object type, sample rate, channels) from an AudioSpecificConfig.
pub fn parse_asc(asc: &[u8]) -> Option<(u8, u32, u16)> {
    if asc.len() < 2 {
        return None;
    }
    let v = u16::from_be_bytes([asc[0], asc[1]]);
    let aot = (v >> 11) as u8;
    let idx = ((v >> 7) & 0xF) as usize;
    let ch = (v >> 3) & 0xF;
    let rate = if idx == 15 {
        if asc.len() < 5 {
            return None;
        }
        // 24-bit explicit rate follows the index.
        let bits = u64::from_be_bytes([0, 0, 0, asc[0], asc[1], asc[2], asc[3], asc[4]]);
        ((bits >> 7) & 0xFF_FFFF) as u32
    } else {
        *RATES.get(idx)?
    };
    Some((aot, rate, ch))
}

/// `MF_MT_USER_DATA` for Media Foundation's AAC decoder: the tail of an
/// `HEAACWAVEINFO` (raw AAC payload) followed by the AudioSpecificConfig.
pub fn mf_user_data(asc: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 12];
    v[2] = 0xFE; // audio profile/level: unknown
    v.extend_from_slice(asc);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_roundtrip() {
        assert_eq!(asc_lc(48000, 2), vec![0x11, 0x90]);
        assert_eq!(asc_lc(44100, 2), vec![0x12, 0x10]);
        assert_eq!(parse_asc(&[0x11, 0x90]), Some((2, 48000, 2)));
        assert_eq!(parse_asc(&[0x12, 0x08]), Some((2, 44100, 1)));
        assert_eq!(parse_asc(&[0x11]), None);
        assert_eq!(mf_user_data(&[0x11, 0x90]).len(), 14);
    }
}
