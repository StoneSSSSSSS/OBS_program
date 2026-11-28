//! Preserve the project frame grid when Windows ends a cut on a partial
//! source frame and carries the fractional duration into later samples.

#[cfg(test)]
mod tests;

use super::{reframe, RenderControl};
use crate::mp4::{
    flat::FlatWriter,
    reader::{Mp4Reader, Sample, TrackKind},
    SampleRef, TrackSpec,
};
use anyhow::{ensure, Context, Result};
use cv_core::editor::TICKS_PER_SECOND;
use std::{fs::OpenOptions, io::{Seek, SeekFrom, Write}, path::Path};

/// The native encoder may assign a short interior sample at an off-grid trim.
/// Repair the staging file in place when possible. Return a compressed-packet
/// remux only when the original MP4 timing representation cannot hold the fix.
pub(super) fn normalize(
    path: &Path,
    remuxed: &Path,
    fps: u32,
    duration: u64,
    control: &RenderControl,
) -> Result<bool> {
    let reader = Mp4Reader::open(path)?;
    let index = reader.video().context("Windows rendered no video track")?;
    let track = &reader.tracks[index];
    let mut samples = reader.samples(index, 0, u64::MAX)?;
    let original_count = samples.len();
    let end = (u128::from(duration) * u128::from(track.timescale)).div_ceil(u128::from(TICKS_PER_SECOND)) as i64;
    while samples.last().is_some_and(|sample| sample.duration == 0 && sample.pts() >= end) {
        samples.pop();
    }
    let removed_empty_tail = samples.len() != original_count;
    let presentation = presentation_order(&samples);
    let frame_count = usize::try_from((u128::from(duration) * u128::from(fps)).div_ceil(u128::from(TICKS_PER_SECOND)))?;
    let track_start = track.edit.map_or(0, |(start, _)| start);
    let max_drift = presentation.iter().enumerate().map(|(i, &sample)| {
        let expected = ((i as u128 * u128::from(track.timescale) + u128::from(fps / 2)) / u128::from(fps)) as i64;
        (samples[sample].pts() - track_start).abs_diff(expected)
    }).max().unwrap_or(0);
    if max_drift <= 1 && samples.len() == frame_count && !removed_empty_tail {
        return Ok(false);
    }

    control.check()?;
    let duplicates = presentation.windows(2).any(|pair| samples[pair[0]].pts() == samples[pair[1]].pts());
    let half_frame = u64::from(track.timescale).div_ceil(u64::from(fps) * 2);
    // An ordinal shift beyond the nearest grid point would move the picture
    // too far relative to audio. Sample actual frames at project times instead.
    if samples.len() != frame_count || duplicates || max_drift > half_frame + 1 {
        let picture = remuxed.with_file_name("video-frame-grid.mp4");
        reframe::video(&reader, index, &samples, &picture, fps, frame_count, duration, control)
            .context("Re-sample the Windows pictures at the project frame rate")?;
        let video = Mp4Reader::open(&picture)?;
        let repaired_index = video.video().context("Frame grid repair produced no video")?;
        let packets = video.samples(repaired_index, 0, u64::MAX)?;
        copy_with_grid(&video, repaired_index, packets, &reader, remuxed, fps, duration, control)?;
        return Ok(true);
    }
    if !removed_empty_tail && track_start == 0 && track.timescale % fps == 0 && samples.iter().all(|sample| sample.cts == 0) {
        let step = track.timescale / fps;
        let trailing = samples.last().context("Windows rendered no video samples")?.end()
            .saturating_sub((samples.len() as u64 - 1) * u64::from(step));
        // Leave the original track/movie duration and every chunk offset alone.
        if (1..=u64::from(step)).contains(&trailing) {
            if let Some((offset, mut table)) = reader.video_decode_timing_table(track.id)? {
                if align_decode_table(&mut table, samples.len(), step, trailing as u32)? {
                    drop(reader);
                    control.check()?;
                    let mut file = OpenOptions::new().write(true).open(path)?;
                    file.seek(SeekFrom::Start(offset))?;
                    file.write_all(&table)?;
                    return Ok(false);
                }
            }
        }
    }

    copy_with_grid(&reader, index, samples, &reader, remuxed, fps, duration, control)?;
    Ok(true)
}

fn align_decode_table(table: &mut [u8], frames: usize, step: u32, last: u32) -> Result<bool> {
    let count = u32::from_be_bytes(table.get(4..8).context("Truncated video timing table")?.try_into()?) as usize;
    let runs = table.get_mut(8..).context("Truncated video timing body")?;
    ensure!(runs.len() == count.checked_mul(8).context("Video timing table is too large")?,
        "Invalid video timing table");
    let distinct_last = step != last;
    if count < 1 + usize::from(distinct_last) || count > frames {
        return Ok(false);
    }
    ensure!(runs.chunks_exact(8).map(|run| u32::from_be_bytes(run[..4].try_into().unwrap()) as u64)
        .sum::<u64>() == frames as u64, "Video timing and sample counts differ");
    // Keep the original box length by using adjacent runs with identical
    // durations. Only the final frame may hold for a fraction of a period.
    for (i, run) in runs.chunks_exact_mut(8).enumerate() {
        let duration = if i == count - 1 { last } else { step };
        let run_count = if i == count - 1 - usize::from(distinct_last) { frames - count + 1 } else { 1 };
        run[..4].copy_from_slice(&u32::try_from(run_count)?.to_be_bytes());
        run[4..].copy_from_slice(&duration.to_be_bytes());
    }
    Ok(true)
}

fn presentation_order(samples: &[Sample]) -> Vec<usize> {
    let mut order: Vec<_> = (0..samples.len()).collect();
    order.sort_by_key(|&index| samples[index].pts());
    order
}

pub(super) fn step_and_tail(fps: u32, count: usize, duration: u64) -> Result<(u32, u32)> {
    let step = 1000u32;
    let total = (u128::from(duration) * u128::from(fps * step)).div_ceil(u128::from(TICKS_PER_SECOND));
    let starts_before_end = count.checked_sub(1).context("The output has no video frames")?;
    let tail = total.checked_sub(starts_before_end as u128 * u128::from(step))
        .context("Frame timestamps exceed the timeline")?;
    ensure!((1..=u128::from(step)).contains(&tail), "The last frame extends beyond the timeline");
    Ok((step, u32::try_from(tail)?))
}

fn copy_with_grid(
    video_reader: &Mp4Reader,
    video: usize,
    samples: Vec<Sample>,
    audio_reader: &Mp4Reader,
    destination: &Path,
    fps: u32,
    duration: u64,
    control: &RenderControl,
) -> Result<()> {
    let presentation = presentation_order(&samples);
    let (step, tail) = step_and_tail(fps, samples.len(), duration)?;
    let audio = audio_reader.tracks.iter().enumerate().filter(|(_, track)| track.kind == TrackKind::Audio);
    let tracks: Vec<_> = std::iter::once((video_reader, video, &video_reader.tracks[video]))
        .chain(audio.map(|(i, track)| (audio_reader, i, track))).collect();
    let specs: Vec<_> = tracks.iter().enumerate().map(|(i, (_, _, track))| TrackSpec {
        id: i as u32 + 1,
        timescale: if i == 0 { fps * step } else { track.timescale },
        codec: track.codec.clone(),
        width: track.width as u16,
        height: track.height as u16,
        name: track.name.clone(),
        enabled: track.enabled,
        default_duration: if i == 0 { step } else { 0 },
    }).collect();
    let mut ranks = vec![0i64; samples.len()];
    for (rank, &index) in presentation.iter().enumerate() {
        ranks[index] = rank as i64;
    }
    let video_count = samples.len();
    let mut packet_tracks = vec![samples];
    for &(reader, i, _) in &tracks[1..] {
        packet_tracks.push(reader.samples(i, 0, u64::MAX)?);
    }
    let mut positions = vec![0; tracks.len()];
    let mut writer = FlatWriter::new(destination, &specs, 0)?;
    loop {
        control.check()?;
        let next = packet_tracks.iter().enumerate().filter_map(|(i, packets)| {
            packets.get(positions[i]).map(|sample| {
                let dts = if i == 0 { positions[i] as u64 * u64::from(step) } else { sample.dts };
                (i, dts as u128 * u128::from(TICKS_PER_SECOND) / u128::from(specs[i].timescale))
            })
        }).min_by_key(|&(_, pts)| pts);
        let Some((track, _)) = next else { break };
        let i = positions[track];
        let sample = &packet_tracks[track][i];
        let data = tracks[track].0.read(sample)?;
        let (duration, cts) = if track == 0 {
            let duration = if i + 1 == video_count { tail } else { step };
            (duration, i32::try_from((ranks[i] - i as i64) * i64::from(step))?)
        } else {
            (sample.duration, sample.cts)
        };
        writer.add(track, &SampleRef { data: &data, duration, sync: sample.sync, cts })?;
        positions[track] += 1;
    }
    for (i, (_, _, track)) in tracks.iter().enumerate().skip(1) {
        if let Some((start, duration)) = track.edit {
            writer.set_edit(i, start.max(0) as u64, duration);
        }
    }
    writer.finish(destination, None)?;
    Ok(())
}
