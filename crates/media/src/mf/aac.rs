//! AAC-LC encoding and decoding with Windows' built-in Media Foundation codecs.

use super::*;

/// Windows' AAC encoder (not in the metadata the bindings are generated from).
const CLSID_AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);

/// AAC frame size in samples per channel.
pub const FRAME: usize = 1024;

/// The bitrates Windows' encoder supports, in kbit/s.
pub const BITRATES: [u32; 4] = [96, 128, 160, 192];

pub fn nearest_bitrate(kbps: u32) -> u32 {
    *BITRATES.iter().min_by_key(|b| (**b as i64 - kbps as i64).abs()).unwrap()
}

pub struct AacEncoder {
    m: SyncMft,
    rate: u32,
    channels: u16,
    pending: Vec<i16>,
    samples_in: u64,
    pub asc: Vec<u8>,
    pub bitrate_kbps: u32,
}

impl AacEncoder {
    pub fn new(rate: u32, channels: u16, bitrate_kbps: u32) -> Result<AacEncoder> {
        init();
        let kbps = nearest_bitrate(bitrate_kbps);
        unsafe {
            let mft = create_mft(&CLSID_AAC_ENCODER)?;
            let out = new_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, kbps * 1000 / 8)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            mft.SetOutputType(0, &out, 0).context("AAC encoder output type")?;
            let inp = new_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels as u32 * 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels as u32 * 2)?;
            mft.SetInputType(0, &inp, 0).context("AAC encoder input type")?;
            let mut m = SyncMft::new(mft);
            m.begin()?;
            Ok(AacEncoder { m, rate, channels, pending: Vec::new(), samples_in: 0, asc: crate::aac::asc_lc(rate, channels), bitrate_kbps: kbps })
        }
    }

    /// Feed interleaved float PCM; returns any AAC frames completed.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend(pcm.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16));
        let block = FRAME * self.channels as usize;
        let mut out = Vec::new();
        while self.pending.len() >= block {
            let chunk: Vec<i16> = self.pending.drain(..block).collect();
            self.push_chunk(&chunk, &mut out)?;
        }
        Ok(out)
    }

    fn push_chunk(&mut self, chunk: &[i16], out: &mut Vec<Vec<u8>>) -> Result<()> {
        let bytes: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
        let frames = (chunk.len() / self.channels as usize) as i64;
        let t = self.samples_in as i64 * HNS / self.rate as i64;
        let s = sample_from(&bytes, t, frames * HNS / self.rate as i64)?;
        self.samples_in += frames as u64;
        let mut got = Vec::new();
        while !self.m.push(&s)? {
            self.m.pull_all(&mut got)?;
        }
        self.m.pull_all(&mut got)?;
        for g in got {
            out.push(sample_bytes(&g)?);
        }
        Ok(())
    }

    /// Encode what's left (padded with silence to a whole frame) and drain.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let block = FRAME * self.channels as usize;
            let mut chunk = std::mem::take(&mut self.pending);
            chunk.resize(block, 0);
            self.push_chunk(&chunk, &mut out)?;
        }
        self.m.drain()?;
        let mut got = Vec::new();
        self.m.pull_all(&mut got)?;
        for g in got {
            out.push(sample_bytes(&g)?);
        }
        Ok(out)
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
}

pub(crate) struct DecodedPcm {
    pub time_hns: i64,
    pub samples: Vec<f32>,
}

pub struct AacDecoder {
    m: SyncMft,
    float_out: bool,
    pub channels: u16,
    pub rate: u32,
}

impl AacDecoder {
    pub fn new(asc: &[u8]) -> Result<AacDecoder> {
        init();
        let (_, rate, channels) = crate::aac::parse_asc(asc).context("bad AudioSpecificConfig")?;
        let channels = channels.max(1);
        unsafe {
            let mft = create_mft(&CLSID_MSAACDecMFT)?;
            let inp = new_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            inp.SetBlob(&MF_MT_USER_DATA, &crate::aac::mf_user_data(asc))?;
            mft.SetInputType(0, &inp, 0).context("AAC decoder input type")?;
            let (out, float_out) = match find_output_type(&mft, &MFAudioFormat_Float) {
                Ok(t) => (t, true),
                Err(_) => (find_output_type(&mft, &MFAudioFormat_PCM)?, false),
            };
            mft.SetOutputType(0, &out, 0).context("AAC decoder output type")?;
            let mut m = SyncMft::new(mft);
            m.begin()?;
            Ok(AacDecoder { m, float_out, channels, rate })
        }
    }

    /// Decode one raw AAC frame to interleaved f32 (may return nothing while
    /// the decoder primes).
    pub fn decode(&mut self, frame: &[u8]) -> Result<Vec<f32>> {
        let s = sample_from(frame, 0, 0)?;
        let mut out = Vec::new();
        for sample in self.decode_sample(&s)? {
            self.append_pcm(&sample, &mut out)?;
        }
        Ok(out)
    }

    pub(crate) fn decode_timed(
        &mut self,
        frame: &[u8],
        time_hns: i64,
        duration_hns: i64,
    ) -> Result<Vec<DecodedPcm>> {
        let sample = sample_from(frame, time_hns, duration_hns)?;
        let output = self.decode_sample(&sample)?;
        self.timed_pcm(output)
    }

    /// Release PCM still buffered by the transform at a discontinuity or EOF.
    pub(crate) fn drain(&mut self) -> Result<Vec<DecodedPcm>> {
        self.m.drain()?;
        let mut output = Vec::new();
        self.pull_output(&mut output)?;
        self.timed_pcm(output)
    }

    fn decode_sample(&mut self, sample: &IMFSample) -> Result<Vec<IMFSample>> {
        let mut output = Vec::new();
        while !self.m.push(sample)? {
            self.pull_output(&mut output)?;
        }
        self.pull_output(&mut output)?;
        Ok(output)
    }

    fn pull_output(&mut self, output: &mut Vec<IMFSample>) -> Result<()> {
        if self.m.pull_all(output)? {
            self.m.refresh_out_info();
        }
        Ok(())
    }

    fn timed_pcm(&self, output: Vec<IMFSample>) -> Result<Vec<DecodedPcm>> {
        output
            .into_iter()
            .map(|sample| {
                let time_hns = unsafe { sample.GetSampleTime()? };
                let mut samples = Vec::new();
                self.append_pcm(&sample, &mut samples)?;
                Ok(DecodedPcm { time_hns, samples })
            })
            .collect()
    }

    fn append_pcm(&self, sample: &IMFSample, out: &mut Vec<f32>) -> Result<()> {
        let bytes = sample_bytes(sample)?;
        if self.float_out {
            out.extend(bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
        } else {
            out.extend(bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0));
        }
        Ok(())
    }

    /// Forget codec state when starting a new run after a seek or gap.
    pub fn reset(&mut self) {
        self.m.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n).flat_map(|i| {
            let v = amp * (i as f32 * freq * std::f32::consts::TAU / 48000.0).sin();
            [v, v]
        }).collect()
    }

    #[test]
    fn encode_decode_roundtrip_and_latency() {
        let mut enc = AacEncoder::new(48000, 2, 160).unwrap();
        assert_eq!(enc.asc, vec![0x11, 0x90]);
        // 1 s of silence with a loud 10 ms burst at exactly 0.5 s.
        let mut pcm = vec![0.0f32; 48000 * 2];
        for i in 24000..24480 {
            let v = 0.8 * (i as f32 * 1000.0 * std::f32::consts::TAU / 48000.0).sin();
            pcm[i * 2] = v;
            pcm[i * 2 + 1] = v;
        }
        let mut frames = Vec::new();
        for chunk in pcm.chunks(960) {
            frames.extend(enc.encode(chunk).unwrap());
        }
        frames.extend(enc.finish().unwrap());
        assert!((46..=50).contains(&frames.len()), "{} frames", frames.len());
        let mut dec = AacDecoder::new(&enc.asc).unwrap();
        let mut out = Vec::new();
        for f in &frames {
            out.extend(dec.decode(f).unwrap());
        }
        assert!(out.len() >= 46 * 1024 * 2);
        // Where does the burst come out? (This is the codec delay we must
        // compensate for when timestamping.)
        let first_loud = out.chunks(2).position(|c| c[0].abs() > 0.2).unwrap();
        let delay = first_loud as i64 - 24000;
        eprintln!("AAC round-trip delay: {delay} samples");
        assert!((0..=2200).contains(&delay), "delay {delay}");
        std::fs::write(std::env::temp_dir().join("cv_aac_delay.txt"), delay.to_string()).ok();
    }

    #[test]
    fn tone_survives() {
        let mut enc = AacEncoder::new(48000, 2, 128).unwrap();
        let mut frames = enc.encode(&tone(48000, 440.0, 0.5)).unwrap();
        frames.extend(enc.finish().unwrap());
        let mut dec = AacDecoder::new(&enc.asc).unwrap();
        let mut out = Vec::new();
        for f in &frames {
            out.extend(dec.decode(f).unwrap());
        }
        let mid = &out[20000..60000];
        let rms = (mid.iter().map(|v| v * v).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "rms {rms}");
    }
}
