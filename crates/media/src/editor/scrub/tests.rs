
use super::*;
use crate::editor::{thumbnail, thumbnail_winrt};
use cv_core::editor::{Framing, Rotation};
use std::{fs, path::{Path, PathBuf}, process::Command, time::Instant};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const LEFT: u64 = 17_600_000;
const RIGHT: u64 = 18_000_000;
const AFTER: u64 = 18_400_000;

fn fixture() -> Result<(Project, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.clipvault-local/qa/editor-reviews/sports-delivery-two-339d4bfe");
    let project = Project::load(&root.join("output/Center Court Replay.cvproj"))?;
    Ok((project, root))
}

fn fitted_pixel(frame: &RgbaImage, x: u32, y: u32) -> Rgba<u8> {
    // The test pictures have a 320x192 aperture and occupy 600x360 of the preview.
    let (x, y) = if frame.dimensions() == (320, 192) {
        (x, y)
    } else {
        (20 + (2 * x + 1) * 15 / 16, (2 * y + 1) * 15 / 16)
    };
    *frame.get_pixel(x, y)
}

fn number(frame: &RgbaImage) -> Option<u32> {
    const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let mut number = 0;
    for digit in 0..3 {
        let mut bits = 0;
        for (bit, (x, y)) in [(2, 0), (4, 2), (4, 6), (2, 8), (0, 6), (0, 2), (2, 4)]
            .into_iter().enumerate()
        {
            let [r, g, b, _] = fitted_pixel(frame, 255 + (digit * 7 + x) * 2, 8 + y * 2).0;
            if u16::from(r) + u16::from(g) + u16::from(b) > 405 {
                bits |= 1 << bit;
            }
        }
        let value = DIGITS.iter().position(|&known| known == bits)?;
        number = number * 10 + value as u32;
    }
    Some(number)
}

fn ffmpeg(source: &Path, index: u32) -> Result<RgbaImage> {
    let binary = std::env::var_os("CLIPVAULT_EDITOR_FFMPEG")
        .expect("Set CLIPVAULT_EDITOR_FFMPEG to the independent decoder");
    let output = Command::new(binary)
        .args(["-hide_banner", "-loglevel", "error", "-i"]).arg(source)
        .args(["-map", "0:v:0", "-vf", &format!(r"select=eq(n\,{index})"),
            "-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "pipe:1"])
        .output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    Ok(image::load_from_memory(&output.stdout)?.to_rgba8())
}

#[test]
#[ignore = "Uses the local two-angle fixture and independent FFmpeg decoder"]
fn direct_angle_join_matches_independent_source_frames() -> Result<()> {
    let (project, _) = fixture()?;
    for (time, asset, index) in [(LEFT, 0, 69), (RIGHT, 1, 70), (AFTER, 1, 71), (LEFT, 0, 69)] {
        let reference = ffmpeg(&project.assets[asset].path, index)?;
        assert_eq!(number(&reference), Some(index));
        assert!(picture(&project, time, WIDTH, HEIGHT)?.is_some(), "at {time}");
        let frame = thumbnail(&project, time, WIDTH, HEIGHT)?;
        assert_eq!(number(&frame), Some(index), "at {time}");
        assert_eq!(*frame.get_pixel(2, 180), Rgba([0, 0, 0, 255]));
        assert_eq!(*frame.get_pixel(637, 180), Rgba([0, 0, 0, 255]));
        for (x, y) in [(100, 25), (79, 140), (170, 72), (281, 160)] {
            let actual = fitted_pixel(&frame, x, y).0;
            let expected = reference.get_pixel(x, y).0;
            for (left, right) in actual.into_iter().zip(expected) {
                assert!(left.abs_diff(right) <= 5, "at {time}: {x},{y} {actual:?} {expected:?}");
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "Uses the local two-angle fixture"]
fn uncertain_scrubs_fall_back_with_legacy_tail_guard() -> Result<()> {
    let (project, root) = fixture()?;
    assert!(picture(&project, LEFT + 200_000, WIDTH, HEIGHT)?.is_none());
    assert!(picture(&project, RIGHT - 1, WIDTH, HEIGHT)?.is_none());
    assert!(picture(&project, LEFT, WIDTH, HEIGHT - 20)?.is_none());
    let mut altered = project.clone();
    altered.shots[0].visual_transform.rotation = Rotation::Clockwise90;
    assert!(picture(&altered, LEFT, WIDTH, HEIGHT)?.is_none());
    altered.shots[0].visual_transform.rotation = Rotation::None;
    altered.shots[0].visual_transform.framing = Framing::Fill { horizontal: 50, vertical: 50 };
    assert!(picture(&altered, LEFT, WIDTH, HEIGHT)?.is_none());

    // A legacy project can still extend the visual range over an audio-only tail.
    let mut legacy = project.clone();
    let source_end = legacy.assets[0].info.duration;
    legacy.assets[0].info.duration += 100_000;
    legacy.shots[0].range.end = source_end + 100_000;
    let tail = source_end - legacy.shots[0].range.start;
    legacy.validate()?;
    assert!(picture(&legacy, tail, WIDTH, HEIGHT)?.is_none());
    assert!(super::super::past_source_picture(&legacy, tail));

    let work = tempfile::tempdir_in(root)?;
    let original = fs::read(&project.assets[0].path)?;
    for tag in [b"pasp", b"colr", b"tkhd", b"mvhd", b"tapt", b"aperture".as_slice()] {
        let mut bytes = original.clone();
        let find = |tag: &[u8]| original.windows(tag.len()).position(|item| item == tag).unwrap();
        match tag {
            b"pasp" => bytes[find(tag) + 7] = 2,
            b"colr" => bytes[find(tag) + 14] = 0x80,
            b"tapt" => {
                // Insert an aperture box and keep the clip's chunk offsets valid.
                let trak = find(b"trak") - 4;
                let moov = find(b"moov") - 4;
                let read_u32 = |at| u32::from_be_bytes(original[at..at + 4].try_into().unwrap());
                let end = trak + read_u32(trak) as usize;
                for (table, tag) in original[..find(b"mdat")].windows(4).enumerate() {
                    if tag == b"stco" {
                        let entries = read_u32(table + 8) as usize;
                        for at in (table + 12..table + 12 + entries * 4).step_by(4) {
                            bytes[at..at + 4].copy_from_slice(&(read_u32(at) + 8).to_be_bytes());
                        }
                    }
                }
                bytes.splice(end..end, *b"\x00\x00\x00\x08tapt");
                for at in [trak, moov] {
                    bytes[at..at + 4].copy_from_slice(&(read_u32(at) + 8).to_be_bytes());
                }
            }
            b"tkhd" | b"mvhd" => {
                let body = find(tag) + 4;
                let offset = if tag == b"tkhd" { 40 } else { 36 };
                let matrix = body + offset + if bytes[body] == 1 { 12 } else { 0 };
                bytes[matrix + 1] = 0;
            }
            _ => {
                let at = find(b"pasp");
                bytes[at..at + 4].copy_from_slice(b"clap");
            }
        }
        let path = work.path().join("variant.mp4");
        fs::write(&path, bytes)?;
        altered = project.clone();
        assert!(Mp4Reader::open(&path)?.editor_scrub_track().is_none(), "{tag:?}");
        altered.assets[0].path = path;
        assert!(picture(&altered, LEFT, WIDTH, HEIGHT)?.is_none(), "{tag:?}");
    }
    Ok(())
}

fn benchmark(label: &str, preview: fn(&Project, u64, u32, u32) -> Result<RgbaImage>) -> Result<()> {
    let (project, _) = fixture()?;
    let now = Instant::now();
    let initial = preview(&project, 0, WIDTH, HEIGHT)?;
    let cold = now.elapsed();
    let mut times = Vec::new();
    let mut counters = Vec::new();
    for time in [LEFT, RIGHT, LEFT, RIGHT, LEFT, RIGHT, LEFT, RIGHT] {
        let now = Instant::now();
        let frame = preview(&project, time, WIDTH, HEIGHT)?;
        times.push(now.elapsed());
        counters.push(number(&frame));
    }
    times.sort_unstable();
    eprintln!("{label}: initial {cold:?} (frame {:?}), stepped median {:?}, slowest {:?}, counters {counters:?}",
        number(&initial), times[times.len() / 2], times.last().unwrap());
    Ok(())
}

#[test]
#[ignore = "Measures the unchanged Windows composition path with the local two-angle fixture"]
fn bench_winrt_angle_join() -> Result<()> {
    benchmark("WinRT", thumbnail_winrt)
}

#[test]
#[ignore = "Measures the editor preview with the local two-angle fixture"]
fn bench_direct_angle_join() -> Result<()> {
    benchmark("Direct", thumbnail)
}
