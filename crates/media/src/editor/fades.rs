//! Bake a linear, source-time fade into an isolated PCM track.

use super::{media_path, storage_file, ticks, RenderControl};
use crate::{
    audioread::{TrackPcm, OUT_RATE},
    mf,
    mp4::reader::Mp4Reader,
    probe::{probe_reader, TrackRole},
};
use anyhow::{ensure, Context, Result};
use cv_core::editor::{AudioFade, Span, TICKS_PER_SECOND};
use std::{fs::File, io::{BufWriter, Write}, path::Path};
use windows::{
    core::{GUID, PCWSTR, PROPVARIANT},
    Media::Editing::BackgroundAudioTrack,
    Win32::Media::MediaFoundation::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PcmFormat {
    rate: u32,
    channels: u16,
    channel_mask: u32,
    bytes_per_sample: usize,
}

fn frames_at(tick: i64, rate: u32) -> i64 {
    let ticks = i128::from(TICKS_PER_SECOND);
    ((i128::from(tick) * i128::from(rate) + ticks / 2).div_euclid(ticks)) as i64
}

struct FadedWave {
    file: BufWriter<File>,
    format: PcmFormat,
    fade: AudioFade,
    pos: i64,
    end: i64,
}

impl FadedWave {
    fn start(path: &Path, range: Span, format: PcmFormat, fade: AudioFade) -> Result<Self> {
        ensure!((1..=768_000).contains(&format.rate) && format.channels > 0, "The source has an invalid PCM format");
        let pos = frames_at(ticks(range.start)?.Duration, format.rate);
        let end = frames_at(ticks(range.end)?.Duration, format.rate).max(pos + 1);
        let block = format.channels.checked_mul(2).context("The source has too many audio channels")?;
        let bytes = ((end - pos) as u64).checked_mul(u64::from(block))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .context("Faded audio exceeds the WAV size limit")?;
        let byte_rate = format.rate.checked_mul(u32::from(block)).context("The audio rate is too high")?;
        let extra = if format.channels > 2 { 24 } else { 0 };
        let riff_size = bytes.checked_add(36 + extra).context("Faded audio exceeds the WAV size limit")?;
        let mut file = BufWriter::new(File::create_new(path)?);
        file.write_all(b"RIFF")?;
        file.write_all(&riff_size.to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&(16u32 + extra).to_le_bytes())?;
        let tag = if extra == 0 { 1u16 } else { 0xfffe };
        file.write_all(&tag.to_le_bytes())?;
        file.write_all(&format.channels.to_le_bytes())?;
        file.write_all(&format.rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&block.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        if extra != 0 {
            file.write_all(&22u16.to_le_bytes())?;
            file.write_all(&16u16.to_le_bytes())?;
            file.write_all(&format.channel_mask.to_le_bytes())?;
            file.write_all(&[1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113])?;
        }
        file.write_all(b"data")?;
        file.write_all(&bytes.to_le_bytes())?;
        Ok(Self { file, format, fade, pos, end })
    }

    fn silence_to(&mut self, end: i64, control: &RenderControl) -> Result<()> {
        let end = end.min(self.end);
        let block = usize::from(self.format.channels) * 2;
        let silence = [0u8; 8192];
        while self.pos < end {
            control.check()?;
            let frames = ((end - self.pos) as usize).min(silence.len() / block);
            self.file.write_all(&silence[..frames * block])?;
            self.pos += frames as i64;
        }
        Ok(())
    }

    fn samples(&mut self, frames: usize, mut samples: impl Iterator<Item = i16>) -> Result<()> {
        let rate = u128::from(self.format.rate);
        let scale = u128::from(TICKS_PER_SECOND);
        for _ in 0..frames {
            let source_tick = ((self.pos as u128 * scale + rate / 2) / rate) as u64;
            let gain = self.fade.gain_at(source_tick);
            for _ in 0..self.format.channels {
                let value = f64::from(samples.next().context("A decoded PCM packet was incomplete")?);
                self.file.write_all(&((value * gain).round() as i16).to_le_bytes())?;
            }
            self.pos += 1;
        }
        Ok(())
    }

    fn finish(mut self, control: &RenderControl) -> Result<()> {
        self.silence_to(self.end, control)?;
        self.file.flush()?;
        Ok(())
    }
}

fn create_fragment_wave(
    reader: &Mp4Reader,
    range: Span,
    fade: AudioFade,
    destination: &Path,
    control: &RenderControl,
) -> Result<bool> {
    let info = probe_reader(reader);
    let Some(track) = info.audio.iter().find(|track| track.role == TrackRole::Full)
        .or_else(|| info.audio.first()) else { return Ok(false) };
    let index = reader.track_index(track.track_id).context("The recording's audio track is missing")?;
    let mut pcm = TrackPcm::new(reader, index)?;
    let format = PcmFormat { rate: OUT_RATE, channels: 2, channel_mask: 3, bytes_per_sample: 2 };
    let mut wave = FadedWave::start(destination, range, format, fade)?;
    let edit = frames_at((reader.tracks[index].edit_start_s() * TICKS_PER_SECOND as f64).round() as i64, OUT_RATE);
    pcm.seek(reader, wave.pos + edit)?;
    let mut buffer = vec![0.0f32; 4096 * 2];
    while wave.pos < wave.end {
        control.check()?;
        let frames = (wave.end - wave.pos).min(4096) as usize;
        let samples = &mut buffer[..frames * 2];
        pcm.read(reader, wave.pos + edit, samples)?;
        wave.samples(frames, samples.iter().map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16))?;
    }
    wave.finish(control)?;
    Ok(true)
}

fn audio_stream(reader: &IMFSourceReader, ordinal: usize) -> Result<Option<u32>> {
    let first_audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    if ordinal == 0 {
        return match unsafe { reader.GetNativeMediaType(first_audio, 0) } {
            Ok(_) => Ok(Some(first_audio)),
            Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => Ok(None),
            Err(error) => Err(error.into()),
        };
    }
    // Source Reader's numbered stream indexes include video and other tracks;
    // the selected embedded index only counts audio streams.
    let mut audio = 0;
    for index in 0.. {
        let native = match unsafe { reader.GetNativeMediaType(index, 0) } {
            Ok(native) => native,
            Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if mf::guid_eq(&native, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio) {
            if audio == ordinal {
                return Ok(Some(index));
            }
            audio += 1;
        }
    }
    unreachable!()
}

fn pcm_format(reader: &IMFSourceReader, stream: u32) -> Result<PcmFormat> {
    unsafe {
        let output = reader.GetCurrentMediaType(stream)?;
        let bits = output.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE)?;
        ensure!([8, 16, 24, 32].contains(&bits), "Windows returned an unsupported PCM sample size");
        let channels = output.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?;
        ensure!((1..=64).contains(&channels), "Windows returned an unsupported PCM channel count");
        Ok(PcmFormat {
            rate: output.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?,
            channels: channels as u16,
            channel_mask: output.GetUINT32(&MF_MT_AUDIO_CHANNEL_MASK).unwrap_or(0),
            bytes_per_sample: bits as usize / 8,
        })
    }
}

fn pcm16(value: &[u8]) -> i16 {
    match value {
        [a] => (i16::from(*a) - 128) << 8,
        [a, b] => i16::from_le_bytes([*a, *b]),
        [_, a, b] | [_, _, a, b] => i16::from_le_bytes([*a, *b]),
        _ => unreachable!("PCM sample size checked by decoder"),
    }
}

fn create_native_wave(
    path: &Path,
    track_ordinal: usize,
    range: Span,
    fade: AudioFade,
    destination: &Path,
    control: &RenderControl,
) -> Result<bool> {
    let path = media_path(path)?;
    let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR(path.as_ptr()), None) }?;
    let Some(stream) = audio_stream(&reader, track_ordinal)? else { return Ok(false) };
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        let pcm = mf::new_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
        reader.SetCurrentMediaType(stream, None, &pcm)?;
        let preroll = range.start.saturating_sub(TICKS_PER_SECOND / 4);
        if preroll > 0 {
            // Some decoders seek approximately. Timestamps below define the exact trim.
            let _ = reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(ticks(preroll)?.Duration));
        }
    }
    let format = pcm_format(&reader, stream)?;
    let mut wave = FadedWave::start(destination, range, format, fade)?;
    let block = format.bytes_per_sample * usize::from(format.channels);
    while wave.pos < wave.end {
        control.check()?;
        let (mut flags, mut timestamp, mut sample) = (0, 0, None);
        unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample)) }?;
        ensure!(flags & MF_SOURCE_READERF_ERROR.0 as u32 == 0, "Windows reported an audio decode error");
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            ensure!(pcm_format(&reader, stream)? == format, "The source PCM format changed during the fade");
        }
        if let Some(sample) = sample {
            let mut start = frames_at(timestamp, format.rate);
            if start.abs_diff(wave.pos) <= 1 {
                start = wave.pos;
            }
            if start >= wave.end {
                break;
            }
            let data = mf::sample_bytes(&sample)?;
            ensure!(data.len() % block == 0, "Windows returned an incomplete PCM frame");
            let decoded = (data.len() / block) as i64;
            let end = (start + decoded).min(wave.end);
            if end > wave.pos {
                wave.silence_to(start, control)?;
                let first = (wave.pos - start) as usize * block;
                let last = (end - start) as usize * block;
                let values = data[first..last].chunks_exact(format.bytes_per_sample).map(pcm16);
                wave.samples((end - wave.pos) as usize, values)?;
            }
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
            wave.silence_to(frames_at(timestamp, format.rate), control)?;
        }
    }
    wave.finish(control)?;
    Ok(true)
}

pub(super) fn stage(
    source: &Path,
    video: bool,
    range: Span,
    fade: AudioFade,
    destination: &Path,
    control: &RenderControl,
) -> Result<Option<BackgroundAudioTrack>> {
    control.check()?;
    mf::startup();
    let write = || -> Result<bool> {
        if video {
            if let Ok(reader) = Mp4Reader::open(source) {
                if reader.is_fragmented() {
                    return create_fragment_wave(&reader, range, fade, destination, control);
                }
                let index = probe_reader(&reader).audio.iter()
                    .find(|track| track.role == TrackRole::Full).map_or(0, |track| track.index);
                return create_native_wave(source, index, range, fade, destination, control);
            }
        }
        create_native_wave(source, 0, range, fade, destination, control)
    };
    if !write().with_context(|| format!("Prepare the fade for {}", source.display()))? {
        return Ok(None);
    }
    let file = storage_file(destination)?;
    Ok(Some(BackgroundAudioTrack::CreateFromFileAsync(&file)?.get()?))
}
