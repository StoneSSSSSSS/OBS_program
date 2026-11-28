//! Set the editor movie's presentation end without changing compressed samples.

use super::{reader::children, Buf};
use anyhow::{ensure, Context, Result};
use cv_core::editor::TICKS_PER_SECOND;
use std::{fs::{File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, path::Path};


struct MovieLocation {
    start: u64,
    body: u64,
    end: u64,
    reserved_end: u64,
    explicit_end: bool,
}

fn movie_location(file: &mut File, length: u64) -> Result<MovieLocation> {
    let mut movie: Option<MovieLocation> = None;
    let mut position = 0;
    while position < length {
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let kind = &header[4..];
        let short = u32::from_be_bytes(header[..4].try_into()?);
        let (header_len, size) = match short {
            0 => (8, length - position),
            1 => {
                let mut extended = [0; 8];
                file.read_exact(&mut extended)?;
                (16, u64::from_be_bytes(extended))
            }
            size => (8, u64::from(size)),
        };
        ensure!(size >= header_len && size <= length - position, "Invalid MP4 box size");
        if kind == b"moov" {
            ensure!(movie.is_none(), "The staged MP4 contains more than one movie header");
            movie = Some(MovieLocation {
                start: position,
                body: position + header_len,
                end: position + size,
                reserved_end: position + size,
                explicit_end: true,
            });
        } else if let Some(movie) = &mut movie {
            if kind == b"free" && movie.reserved_end == position {
                movie.reserved_end = position + size;
            }
            if short == 0 {
                movie.explicit_end = false;
            }
        }
        position += size;
    }
    movie.context("The staged MP4 has no movie header")
}

/// Only the movie metadata changes. Audio samples keep their original media
/// header, chunk offsets, decode/composition clocks, and encoded bytes.
pub(crate) fn limit(path: &Path, duration: u64) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let location = movie_location(&mut file, len)?;
    let mut movie = vec![0; usize::try_from(location.end - location.body)?];
    file.seek(SeekFrom::Start(location.body))?;
    file.read_exact(&mut movie)?;
    let Some(replacement) = capped_movie(&movie, duration)? else { return Ok(()) };
    let replacement_len = replacement.len() as u64;
    let slack = (location.reserved_end - location.start).checked_sub(replacement_len);

    if location.reserved_end == len {
        // Windows writes its native movie header last.
        file.seek(SeekFrom::Start(location.start))?;
        file.write_all(&replacement)?;
        file.set_len(location.start + replacement_len)?;
    } else if let Some(slack) = slack.filter(|&bytes| bytes == 0 || bytes >= 8) {
        // The editor's cadence remux reserves a free box so its index can stay first.
        file.seek(SeekFrom::Start(location.start))?;
        file.write_all(&replacement)?;
        if slack != 0 {
            file.write_all(&u32::try_from(slack)?.to_be_bytes())?;
            file.write_all(b"free")?;
        }
    } else {
        // Keep every compressed chunk at its original absolute file offset.
        ensure!(location.explicit_end, "Cannot append a movie header after an open-ended MP4 box");
        file.seek(SeekFrom::End(0))?;
        file.write_all(&replacement)?;
        file.seek(SeekFrom::Start(location.start + 4))?;
        file.write_all(b"free")?;
    }
    file.flush()?;
    Ok(())
}

fn capped_movie(movie: &[u8], duration: u64) -> Result<Option<Vec<u8>>> {
    let movie_boxes = children(movie);
    let movie_header = movie_boxes.iter().find(|(kind, _)| kind == b"mvhd").context("Missing MP4 movie duration")?.1;
    let time_fields = time_fields(movie_header)?;
    let old_scale = read32(movie_header, time_fields)?;
    ensure!(old_scale != 0 && duration != 0, "Invalid MP4 movie timescale or presentation duration");
    let old_duration = duration_field(movie_header, time_fields + 4)?;
    let (new_scale, end) = movie_end(old_scale, duration)?;
    let multiplier = u64::from(new_scale / old_scale);

    let mut changed = old_scale != new_scale || old_duration != end;
    let mut output = Buf::default();
    let moov = output.begin(b"moov");
    for (kind, payload) in movie_boxes {
        match &kind {
            b"mvhd" => append(&mut output, &kind, &patch_header(payload, true, new_scale, end)?),
            b"trak" => {
                let (track, trimmed) = capped_track(payload, multiplier, end)?;
                changed |= trimmed;
                append(&mut output, &kind, &track);
            }
            _ => append(&mut output, &kind, payload),
        }
    }
    output.end(moov);
    Ok(changed.then_some(output.0))
}

fn movie_end(old_scale: u32, duration: u64) -> Result<(u32, u64)> {
    let ticks = u128::from(duration) * u128::from(old_scale);
    let clock = u128::from(TICKS_PER_SECOND);
    if ticks % clock == 0 {
        return Ok((old_scale, u64::try_from(ticks / clock)?));
    }
    // Refine the movie clock only when needed; existing movie-timed edits then
    // scale by an integer and their leading offsets remain exact.
    let (mut a, mut b) = (u64::from(old_scale), TICKS_PER_SECOND);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let scale = u32::try_from(u64::from(old_scale) / a * TICKS_PER_SECOND)
        .context("The project duration cannot be represented on this MP4 movie clock")?;
    let end = u128::from(duration) * u128::from(scale) / clock;
    Ok((scale, u64::try_from(end)?))
}

fn capped_track(track: &[u8], multiplier: u64, movie_end: u64) -> Result<(Vec<u8>, bool)> {
    let track_boxes = children(track);
    let tkhd = track_boxes.iter().find(|(kind, _)| kind == b"tkhd").context("Missing MP4 track header")?.1;
    let track_end = duration_field(tkhd, time_fields(tkhd)? + 8)?
        .checked_mul(multiplier).context("MP4 track duration overflow")?;
    let end = track_end.min(movie_end);
    let trimmed = track_end > end;
    if multiplier == 1 && !trimmed {
        return Ok((track.to_owned(), false));
    }

    let has_edts = track_boxes.iter().any(|(kind, _)| kind == b"edts");
    let mut output = Buf::default();
    for (kind, payload) in track_boxes {
        match &kind {
            b"tkhd" => {
                append(&mut output, &kind, &patch_header(payload, false, 0, end)?);
                if trimmed && !has_edts {
                    append(&mut output, b"edts", &capped_edits(&[], multiplier, end, true)?);
                }
            }
            b"edts" => append(&mut output, &kind, &capped_edits(payload, multiplier, end, trimmed)?),
            _ => append(&mut output, &kind, payload),
        }
    }
    Ok((output.0, trimmed))
}

fn capped_edits(edit: &[u8], multiplier: u64, end: u64, trimmed: bool) -> Result<Vec<u8>> {
    let mut result = Buf::default();
    let mut found = false;
    for (kind, body) in children(edit) {
        if &kind == b"elst" {
            ensure!(!found, "Multiple edit lists in the MP4 audio track");
            found = true;
            let flags = read32(body, 0)? & 0x00ff_ffff;
            let version = body[0];
            ensure!(version <= 1, "Unsupported MP4 edit-list version");
            let count = read32(body, 4)? as usize;
            let entry_size = if version == 0 { 12usize } else { 20 };
            ensure!(count <= (body.len() - 8) / entry_size, "Truncated MP4 edit list");
            let entries = body[8..].chunks_exact(entry_size).take(count);
            let offset = result.begin_full(b"elst", 1, flags);
            let count_offset = result.len();
            result.u32(0);
            let mut keep = 0;
            let mut remaining = end;
            for entry in entries {
                let (length, media, rate) = if version == 0 {
                    (u64::from(read32(entry, 0)?), read32(entry, 4)? as i32 as i64, read32(entry, 8)?)
                } else {
                    (read64(entry, 0)?, read64(entry, 8)? as i64, read32(entry, 16)?)
                };
                let scaled = length.checked_mul(multiplier).context("MP4 edit duration overflow")?;
                if trimmed && remaining == 0 {
                    break;
                }
                let visible = if trimmed { scaled.min(remaining) } else { scaled };
                result.u64(visible);
                result.u64(media as u64);
                result.u32(rate);
                keep += 1;
                remaining = remaining.saturating_sub(visible);
            }
            result.patch_u32(count_offset, keep);
            result.end(offset);
        } else {
            append(&mut result, &kind, body);
        }
    }
    if trimmed && !found {
        let atom = result.begin_full(b"elst", 1, 0);
        result.u32(1);
        result.u64(end);
        result.u64(0);
        result.u32(0x0001_0000);
        result.end(atom);
    }
    Ok(result.0)
}

fn append(output: &mut Buf, kind: &[u8; 4], payload: &[u8]) {
    let offset = output.begin(kind);
    output.bytes(payload);
    output.end(offset);
}

fn time_fields(header: &[u8]) -> Result<usize> {
    match header.first() {
        Some(0) => Ok(12),
        Some(1) => Ok(20),
        _ => anyhow::bail!("Unsupported MP4 duration header"),
    }
}

fn duration_field(header: &[u8], offset: usize) -> Result<u64> {
    if time_fields(header)? == 12 {
        Ok(u64::from(read32(header, offset)?))
    } else {
        read64(header, offset)
    }
}

fn patch_header(header: &[u8], movie: bool, scale: u32, duration: u64) -> Result<Vec<u8>> {
    let fields = time_fields(header)?;
    let duration_at = fields + if movie { 4 } else { 8 };
    let _ = duration_field(header, duration_at)?;
    if fields == 12 && duration > u64::from(u32::MAX) {
        // Promote only these full boxes when a precise movie clock needs
        // 64-bit durations. Preserve flags, creation times, and other fields.
        let mut promoted = Vec::with_capacity(header.len() + 12);
        promoted.extend_from_slice(&header[..4]);
        promoted[0] = 1;
        promoted.extend(u64::from(read32(header, 4)?).to_be_bytes());
        promoted.extend(u64::from(read32(header, 8)?).to_be_bytes());
        promoted.extend_from_slice(&header[fields..duration_at]);
        promoted.extend(duration.to_be_bytes());
        promoted.extend_from_slice(&header[duration_at + 4..]);
        return patch_header(&promoted, movie, scale, duration);
    }
    let mut result = header.to_owned();
    if movie {
        result[fields..fields + 4].copy_from_slice(&scale.to_be_bytes());
    }
    if fields == 12 {
        result[duration_at..duration_at + 4].copy_from_slice(&u32::try_from(duration)?.to_be_bytes());
    } else {
        result[duration_at..duration_at + 8].copy_from_slice(&duration.to_be_bytes());
    }
    Ok(result)
}

fn read32(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes.get(at..at + 4).context("Truncated MP4 metadata")?.try_into()?))
}

fn read64(bytes: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(bytes.get(at..at + 8).context("Truncated MP4 metadata")?.try_into()?))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::{write_mvhd, write_tkhd, Codec, TrackSpec};

    #[test]
    fn preserves_leading_empty_edit_and_media_priming() -> Result<()> {
        let mut movie = Buf::default();
        write_mvhd(&mut movie, 0, 48_000, 50_464, 2);
        let track = movie.begin(b"trak");
        let spec = TrackSpec {
            id: 1, timescale: 48_000, codec: Codec::Aac { asc: vec![], channels: 2, sample_rate: 48_000, avg_bitrate: 0 },
            width: 0, height: 0, name: "Test audio".into(), enabled: true, default_duration: 1024,
        };
        write_tkhd(&mut movie, &spec, 0, 50_464);
        let edit = movie.begin(b"edts");
        let list = movie.begin_full(b"elst", 0, 0);
        movie.u32(2);
        movie.u32(2400);
        movie.u32(u32::MAX);
        movie.u32(0x0001_0000);
        movie.u32(48_064);
        movie.u32(1024);
        movie.u32(0x0001_0000);
        movie.end(list);
        movie.end(edit);
        movie.end(track);

        let expected = TICKS_PER_SECOND + 1;
        let capped = capped_movie(&movie.0, expected)?.context("Expected a modified movie")?;
        let entries = children(&capped[8..]);
        let header = entries.iter().find(|(k, _)| k == b"mvhd").unwrap().1;
        let (scale, end) = movie_end(48_000, expected)?;
        assert_eq!(scale, 30_000_000);
        assert_eq!(read32(header, 20)?, scale);
        assert_eq!(read64(header, 24)?, end);
        let track = entries.iter().find(|(k, _)| k == b"trak").unwrap().1;
        let track_boxes = children(track);
        let tkhd = track_boxes.iter().find(|(k, _)| k == b"tkhd").unwrap().1;
        assert_eq!(read64(tkhd, 28)?, end);
        let edits = track_boxes.iter().find(|(k, _)| k == b"edts").unwrap().1;
        let list = children(edits)[0].1;
        let original_delay = 2400 * u64::from(scale / 48_000);
        assert_eq!(read32(list, 4)?, 2);
        assert_eq!(read64(list, 8)?, original_delay);
        assert_eq!(read64(list, 16)? as i64, -1);
        assert_eq!(read64(list, 28)?, end - original_delay);
        assert_eq!(read64(list, 36)?, 1024);
        assert_eq!(read32(list, 44)?, 0x0001_0000);
        Ok(())
    }
}
