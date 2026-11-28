//! Re-sample ambiguous native picture timestamps while retaining original audio.

use super::{cadence::step_and_tail, profile, RenderControl};
use crate::{
    d3d::Gpu,
    h264,
    mf::{
        h264::{EncodedFrame, EncoderChoice, EncoderConfig, RateControl, VideoEncoder},
        h264dec::{DecodedFrame, VideoDecoder},
    },
    mp4::{flat::FlatWriter, reader::{Mp4Reader, Sample}, Codec, SampleRef, TrackSpec},
};
use anyhow::{ensure, Context, Result};
use cv_core::editor::{Canvas, TICKS_PER_SECOND};
use std::{path::Path, sync::Arc};

pub(super) fn video(
    reader: &Mp4Reader,
    index: usize,
    packets: &[Sample],
    path: &Path,
    fps: u32,
    frames: usize,
    duration: u64,
    control: &RenderControl,
) -> Result<()> {
    let track = &reader.tracks[index];
    let Codec::H264 { avcc } = &track.codec else { anyhow::bail!("Expected an H.264 staging video") };
    let mut decoder = VideoDecoder::new(avcc, None, false)?;
    let canvas = Canvas { width: decoder.width, height: decoder.height, fps };
    let kbps = profile(canvas)?.Video()?.Bitrate()? / 1000;
    let config = EncoderConfig {
        width: canvas.width, height: canvas.height, fps,
        rate: RateControl::Cbr { kbps }, gop: fps * 2, choice: EncoderChoice::Auto, realtime: false,
    };
    let gpu = Gpu::primary().ok().map(Arc::new);
    let mut encoder = GridEncoder {
        path,
        canvas,
        total: frames,
        durations: step_and_tail(fps, frames, duration)?,
        encoder: VideoEncoder::new(gpu, config)?,
        writer: None,
        pending: None,
        next: 0,
        written: 0,
    };
    let edit = track.edit.map_or(0, |(start, _)| start);
    let scale = i128::from(track.timescale);
    for packet in packets {
        control.check()?;
        if packet.size == 0 {
            continue;
        }
        let time = i64::try_from(((i128::from(packet.pts()) - i128::from(edit)) * i128::from(TICKS_PER_SECOND)
            + scale / 2).div_euclid(scale))?;
        for frame in decoder.decode(&reader.read(packet)?, time)? {
            encoder.accept(frame, control)?;
        }
    }
    for frame in decoder.drain()? {
        encoder.accept(frame, control)?;
    }
    encoder.finish(control)
}

struct GridEncoder<'a> {
    path: &'a Path,
    canvas: Canvas,
    total: usize,
    durations: (u32, u32),
    encoder: VideoEncoder,
    writer: Option<FlatWriter>,
    pending: Option<DecodedFrame>,
    next: usize,
    written: usize,
}

impl GridEncoder<'_> {
    fn time(&self, frame: usize) -> i64 {
        let fps = u128::from(self.canvas.fps);
        ((frame as u128 * u128::from(TICKS_PER_SECOND) + fps / 2) / fps) as i64
    }

    fn accept(&mut self, frame: DecodedFrame, control: &RenderControl) -> Result<()> {
        if let Some(previous) = self.pending.take() {
            ensure!(previous.pts_hns <= frame.pts_hns, "Windows decoded pictures out of presentation order");
            while self.next < self.total && self.time(self.next) < frame.pts_hns {
                self.emit(&previous, control)?;
            }
        } else {
            // If the stream opens after the initial grid point, hold its first picture.
            while self.next < self.total && self.time(self.next) < frame.pts_hns {
                self.emit(&frame, control)?;
            }
        }
        // A second picture with the same timestamp replaces the pending one
        // before any frame at that grid point is encoded.
        self.pending = Some(frame);
        Ok(())
    }

    fn emit(&mut self, frame: &DecodedFrame, control: &RenderControl) -> Result<()> {
        control.check()?;
        let time = self.time(self.next);
        let packets = self.encoder.encode_cpu(&frame.nv12, time, self.next == 0)?;
        self.next += 1;
        self.write(packets)
    }

    fn write(&mut self, packets: Vec<EncodedFrame>) -> Result<()> {
        let (step, tail) = self.durations;
        for packet in packets {
            let data = h264::annexb_to_avcc(&packet.data);
            if data.data.is_empty() {
                continue;
            }
            ensure!(self.written < self.total, "The frame-grid encoder emitted too many pictures");
            ensure!(packet.time_hns.abs_diff(self.time(self.written)) <= 1,
                "The frame-grid encoder changed the picture order");
            if self.writer.is_none() {
                let sps = data.sps.as_deref().context("The frame-grid encoder omitted its H.264 SPS")?;
                let pps = data.pps.as_deref().context("The frame-grid encoder omitted its H.264 PPS")?;
                let spec = TrackSpec {
                    id: 1, timescale: self.canvas.fps * step,
                    codec: Codec::H264 { avcc: h264::build_avcc(sps, pps) },
                    width: self.canvas.width.try_into()?, height: self.canvas.height.try_into()?,
                    name: "Video".into(), enabled: true, default_duration: step,
                };
                self.writer = Some(FlatWriter::new(self.path, &[spec], 0)?);
            }
            let duration = if self.written + 1 == self.total { tail } else { step };
            self.writer.as_mut().context("Missing frame-grid MP4 writer")?
                .add(0, &SampleRef { data: &data.data, duration, sync: packet.key || data.idr, cts: 0 })?;
            self.written += 1;
        }
        Ok(())
    }

    fn finish(mut self, control: &RenderControl) -> Result<()> {
        let last = self.pending.take().context("Windows returned no pictures to put on the frame grid")?;
        while self.next < self.total {
            self.emit(&last, control)?;
        }
        let packets = self.encoder.finish()?;
        self.write(packets)?;
        ensure!(self.written == self.total, "The frame-grid encoder omitted pictures");
        self.writer.take().context("Missing frame-grid MP4 writer")?.finish(self.path, None)?;
        Ok(())
    }
}
