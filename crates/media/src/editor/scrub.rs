//! Seek a directly representable editor frame without building a composition.

use crate::{
    h264,
    mf::h264dec::{nv12_to_rgb, DecodedFrame, VideoDecoder},
    mp4::{reader::Mp4Reader, Codec},
};
use anyhow::Result;
use cv_core::editor::{AssetKind, Project, Source, Span, TICKS_PER_SECOND};
use image::{imageops, DynamicImage, Rgba, RgbaImage};

pub(super) fn picture(project: &Project, at: u64, width: u32, height: u32) -> Result<Option<RgbaImage>> {
    let ratio_error = (u64::from(width) * u64::from(project.canvas.height))
        .abs_diff(u64::from(height) * u64::from(project.canvas.width));
    if ratio_error > u64::from(project.canvas.width.max(project.canvas.height)) {
        return Ok(None);
    }

    let mut start = 0;
    for shot in &project.shots {
        let end = start + shot.range.len();
        if at >= end {
            start = end;
            continue;
        }
        if !shot.visual_transform.is_identity() {
            return Ok(None);
        }
        let Source::Asset(id) = shot.source else { return Ok(None); };
        let Some(asset) = project.asset(id).filter(|asset| asset.info.kind == AssetKind::Video) else {
            return Ok(None);
        };
        let reader = Mp4Reader::open(&asset.path)?;
        let time = shot.range.start + (at - start);
        return Ok(direct_frame(&reader, time, shot.range, project.canvas.fps)?
            .map(|frame| fit(&frame, width, height)));
    }
    Ok(None)
}

fn direct_frame(reader: &Mp4Reader, time: u64, range: Span, fps: u32) -> Result<Option<DecodedFrame>> {
    let Some((track, samples)) = reader.editor_scrub_track() else { return Ok(None); };
    let Some(first) = samples.first() else { return Ok(None); };
    let step = u64::from(first.duration);
    if step == 0 || step * u64::from(fps) != u64::from(track.timescale) {
        return Ok(None);
    }
    if samples.iter().enumerate().any(|(i, sample)| {
        sample.cts != 0 || u64::from(sample.duration) != step || sample.dts != i as u64 * step || sample.size == 0
    }) {
        return Ok(None);
    }
    let (_, source_in) = frame_position(range.start, fps);
    let (index, source_time) = frame_position(time, fps);
    // A subtraction and addition of rounded timeline ticks can move a
    // fractional-rate grid position by one tick (100 ns).
    let rounding = u64::from(TICKS_PER_SECOND % u64::from(fps) != 0);
    if range.start != source_in || time.abs_diff(source_time) > rounding
        || source_time < range.start || source_time >= range.end
    {
        return Ok(None);
    }
    let index = index as usize;
    if index >= samples.len() {
        return Ok(None);
    }
    let Codec::H264 { avcc } = &track.codec else { return Ok(None); };
    let Some((length, sps, pps)) = h264::parse_avcc(avcc) else { return Ok(None); };
    let ([sps], [pps]) = (sps.as_slice(), pps.as_slice()) else { return Ok(None); };
    if h264::editor_preview_dimensions(sps) != Some((track.width, track.height)) {
        return Ok(None);
    }
    let Some(key) = samples[..=index].iter().rposition(|sample| sample.sync) else { return Ok(None); };
    let key_bytes = reader.read(&samples[key])?;
    if h264::editor_preview_packet(&key_bytes, length, sps, pps) != Some(true) {
        return Ok(None);
    }

    let mut decoder = VideoDecoder::new(avcc, None, true)?;
    let to_hns = |n| {
        ((u128::from(n) * u128::from(TICKS_PER_SECOND) + u128::from(track.timescale / 2))
            / u128::from(track.timescale)) as i64
    };
    let wanted = to_hns(samples[index].dts);
    let matching = |frame: &DecodedFrame| {
        frame.pts_hns == wanted && frame.width == track.width && frame.height == track.height
    };
    for (i, sample) in samples[key..=index].iter().enumerate() {
        let packet;
        let bytes = if i == 0 {
            &key_bytes
        } else {
            packet = reader.read(sample)?;
            if h264::editor_preview_packet(&packet, length, sps, pps).is_none() {
                return Ok(None);
            }
            &packet
        };
        let frames = decoder.decode(bytes, to_hns(sample.dts))?;
        if let Some(frame) = frames.into_iter().find(matching) {
            return Ok(Some(frame));
        }
    }
    Ok(decoder.drain()?.into_iter().find(matching))
}

fn frame_position(tick: u64, fps: u32) -> (u64, u64) {
    let ticks = u128::from(TICKS_PER_SECOND);
    let rate = u128::from(fps);
    let frame = (u128::from(tick) * rate + ticks / 2) / ticks;
    let position = (frame * ticks + rate / 2) / rate;
    (frame as u64, position as u64)
}

fn fit(frame: &DecodedFrame, width: u32, height: u32) -> RgbaImage {
    let scale = (f64::from(width) / f64::from(frame.width)).min(f64::from(height) / f64::from(frame.height));
    let w = (f64::from(frame.width) * scale).round().max(1.0) as u32;
    let h = (f64::from(frame.height) * scale).round().max(1.0) as u32;
    let image = nv12_to_rgb(frame, 0);
    let image = imageops::resize(&image, w, h, imageops::FilterType::Triangle);
    let image = DynamicImage::ImageRgb8(image).into_rgba8();
    let mut fitted = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 255]));
    imageops::replace(&mut fitted, &image, ((width - w) / 2).into(), ((height - h) / 2).into());
    fitted
}

#[cfg(test)]
mod tests;
