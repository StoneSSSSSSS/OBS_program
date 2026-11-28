use anyhow::{bail, ensure, Context, Result};
use cv_core::editor::{Canvas, Framing, Project, Rotation, SoundRole, Span, VisualTransform, TICKS_PER_SECOND as S};
use cv_media::{
    audioread::read_range,
    editor::{probe, render, thumbnail, RenderControl},
    mf::h264dec::nv12_to_rgb,
    mp4::reader::Mp4Reader,
    thumb::frame_at_time,
};
use image::{Rgb, RgbImage};
use std::{fs, path::{Path, PathBuf}};

const LANDSCAPE: Canvas = Canvas { width: 320, height: 180, fps: 25 };
const PORTRAIT: Canvas = Canvas { width: 180, height: 320, fps: 25 };
const LENGTH: u64 = 6 * S / 5;
const COLORS: [[u8; 3]; 16] = [
    [221, 41, 64], [45, 208, 71], [48, 79, 216], [235, 194, 51],
    [42, 169, 223], [169, 63, 190], [212, 117, 41], [151, 184, 57],
    [59, 73, 163], [201, 166, 209], [60, 120, 48], [207, 60, 121],
    [102, 64, 151], [171, 132, 44], [52, 161, 119], [226, 204, 147],
];

fn axis(value: f64) -> usize {
    [0.23, 0.47, 0.76].iter().filter(|&&stop| value >= stop).count()
}

fn color(x: f64, y: f64) -> [u8; 3] {
    COLORS[4 * axis(y) + axis(x)]
}

fn fixture(work: &Path, audio: &Path, canvas: Canvas) -> Result<[PathBuf; 2]> {
    let png = work.join(format!("landmarks-{}.png", canvas.width));
    let mp4 = work.join(format!("landmarks-{}.mp4", canvas.width));
    RgbImage::from_fn(canvas.width, canvas.height, |x, y| {
        Rgb(color(f64::from(x) / f64::from(canvas.width), f64::from(y) / f64::from(canvas.height)))
    }).save(&png)?;
    let mut project = Project::default();
    project.canvas = canvas;
    let picture = project.import(png.clone(), probe(&png)?)?;
    project.append(picture)?;
    project.shots[0].range.end = 2 * S;
    let music = project.import(audio.to_owned(), probe(audio)?)?;
    project.add_sound(music, SoundRole::Music, 0)?;
    project.sounds[0].range.end = 2 * S;
    render(&project, &mp4, &RenderControl::default())?;
    Ok([png, mp4])
}

fn expected(source: Canvas, canvas: Canvas, transform: VisualTransform, x: u32, y: u32) -> [u8; 3] {
    let quarter_turn = matches!(transform.rotation, Rotation::Clockwise90 | Rotation::Clockwise270);
    let (width, height) = if quarter_turn { (source.height, source.width) } else { (source.width, source.height) };
    let (scale_x, scale_y) = (f64::from(canvas.width) / f64::from(width), f64::from(canvas.height) / f64::from(height));
    let (scale, focus_x, focus_y) = match transform.framing {
        Framing::Fit => (scale_x.min(scale_y), 0.5, 0.5),
        Framing::Fill { horizontal, vertical } => (scale_x.max(scale_y), f64::from(horizontal) / 100.0, f64::from(vertical) / 100.0),
    };
    let (width, height) = (f64::from(width) * scale, f64::from(height) * scale);
    let u = (f64::from(x) + 0.5 + (width - f64::from(canvas.width)) * focus_x) / width;
    let v = (f64::from(y) + 0.5 + (height - f64::from(canvas.height)) * focus_y) / height;
    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
        return [0; 3];
    }
    let u = if transform.mirror { 1.0 - u } else { u };
    let (u, v) = match transform.rotation {
        Rotation::None => (u, v),
        Rotation::Clockwise90 => (v, 1.0 - u),
        Rotation::Clockwise180 => (1.0 - u, 1.0 - v),
        Rotation::Clockwise270 => (1.0 - v, u),
    };
    color(u, v)
}

#[test]
#[ignore = "requires Windows media codecs and CLIPVAULT_EDITOR_TEST_FIXTURES"]
fn fit_and_fill_keep_landmarks_in_preview_and_export() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES")
        .context("Set CLIPVAULT_EDITOR_TEST_FIXTURES to the in-project QA kit")?).canonicalize()?;
    let qa = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    ensure!(root.starts_with(qa), "Framing QA must use an in-project QA folder");
    let work = tempfile::Builder::new().prefix("shot-framing-").tempdir_in(&root)?;
    let audio = root.join("inputs/Night drive music.wav");
    let landscape = fixture(work.path(), &audio, LANDSCAPE)?;
    let portrait = fixture(work.path(), &audio, PORTRAIT)?;
    let originals = landscape.iter().chain(&portrait).map(fs::read).collect::<std::io::Result<Vec<_>>>()?;
    let legacy_transform: VisualTransform = serde_json::from_str(r#"{"rotation":"Clockwise180","mirror":true}"#)?;
    ensure!(legacy_transform.framing == Framing::Fit, "Existing orientations must default to Fit");
    let mut errors = Vec::new();

    for (canvas, opposite, matching, opposite_paths) in [
        (PORTRAIT, LANDSCAPE, &portrait, &landscape),
        (LANDSCAPE, PORTRAIT, &landscape, &portrait),
    ] {
        let mut project = Project::default();
        project.canvas = canvas;
        let mut expected_sizes = Vec::new();
        for (source, paths, transform) in [
            (opposite, opposite_paths, VisualTransform::default()),
            (opposite, opposite_paths, VisualTransform {
                framing: Framing::Fill { horizontal: 0, vertical: 0 }, ..VisualTransform::default()
            }),
            (opposite, opposite_paths, VisualTransform {
                framing: Framing::Fill { horizontal: 100, vertical: 100 }, ..VisualTransform::default()
            }),
            (canvas, matching, VisualTransform {
                rotation: if canvas == PORTRAIT { Rotation::Clockwise90 } else { Rotation::Clockwise270 },
                mirror: true,
                // Keep the sample grid away from color transitions and chroma edges.
                framing: Framing::Fill { horizontal: 71, vertical: 55 },
            }),
        ] {
            for path in paths {
                let id = project.import(path.clone(), probe(path)?)?;
                project.append(id)?;
                let shot = project.shots.last_mut().context("Missing fixture shot")?;
                shot.range = Span { start: S / 5, end: S / 5 + LENGTH };
                shot.visual_transform = transform;
                shot.gain = 0.5;
                expected_sizes.push(source);
            }
        }
        let last = project.shots.last().context("Missing video shot")?.clone();
        project.split(last.id, 7 * LENGTH + LENGTH / 2)?;
        expected_sizes.push(canvas);
        let cue_at = 3 * LENGTH + 9 * S / 10;
        project.caption("Framing".to_owned(), Span { start: cue_at - S / 10, end: cue_at + S / 10 })?;
        let edit = work.path().join(format!("framing-{}.cvproj", canvas.width));
        project.save(&edit)?;
        let reopened = Project::load(&edit)?;
        ensure!(reopened.shots == project.shots && reopened.captions == project.captions
            && reopened.shots[7..].iter().all(|shot| shot.visual_transform == last.visual_transform),
            "Split framing, trim, gain or captions changed after reopening");

        let output = work.path().join(format!("framed-{}.mp4", canvas.width));
        render(&reopened, &output, &RenderControl::default())
            .with_context(|| format!("Render {}×{} fit/fill montage", canvas.width, canvas.height))?;
        let exported = Mp4Reader::open(&output)?;
        let exported_video = exported.video().context("Missing reframed MP4 video")?;
        let cue_preview = thumbnail(&reopened, cue_at, canvas.width, canvas.height)?;
        let (cue_frame, _) = frame_at_time(&exported, exported_video, cue_at as f64 / S as f64)?;
        let cue_export = nv12_to_rgb(&cue_frame, 0);
        let caption_ink = |rgb: &RgbImage| rgb.enumerate_pixels().filter(|(_, y, pixel)| {
            *y >= canvas.height * 3 / 4 && pixel.0.into_iter().all(|channel| channel > 220)
        }).count();
        let cue_preview = image::DynamicImage::ImageRgba8(cue_preview).to_rgb8();
        ensure!(caption_ink(&cue_preview) > 20 && caption_ink(&cue_export) > 20,
            "Caption disappeared over a filled video shot");

        let mut at = 0;
        for (index, (shot, source)) in reopened.shots.iter().zip(expected_sizes).enumerate() {
            let tick = at + shot.range.len() / 2;
            let preview = thumbnail(&reopened, tick, canvas.width, canvas.height)?;
            let (decoded, _) = frame_at_time(&exported, exported_video, tick as f64 / S as f64)?;
            let mp4 = nv12_to_rgb(&decoded, 0);
            for x in [2, canvas.width / 3, 2 * canvas.width / 3, canvas.width - 3] {
                for y in [2, canvas.height * 29 / 100, canvas.height * 68 / 100, canvas.height - 3] {
                    let want = expected(source, canvas, shot.visual_transform, x, y);
                    let before = preview.get_pixel(x, y).0;
                    let after = mp4.get_pixel(x, y).0;
                    for (kind, actual) in [("preview", &before[..3]), ("MP4", &after[..])] {
                        if actual.iter().zip(want).any(|(&a, b)| a.abs_diff(b) > 38) {
                            errors.push(format!("{}×{} shot {} {kind} at {x},{y}: expected {want:?}, got {actual:?}",
                                canvas.width, canvas.height, index + 1));
                        }
                    }
                }
            }
            at += shot.range.len();
        }

        let source_mp4 = Mp4Reader::open(&opposite_paths[1])?;
        let source_audio = *source_mp4.audio().first().context("Fixture has no audio")?;
        let output_audio = *exported.audio().first().context("Reframed MP4 lost embedded audio")?;
        let rms = |samples: &[f32]| (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt();
        let unscaled = rms(&read_range(&source_mp4, source_audio, 0.4, 0.55)?);
        let start = (3 * LENGTH) as f64 / S as f64;
        let reframed = rms(&read_range(&exported, output_audio, start + 0.2, start + 0.35)?);
        ensure!(unscaled > 0.01 && (reframed / unscaled - 0.5).abs() < 0.15, "Video Fill changed embedded audio gain");
        eprintln!("{}×{}: 9 shots, split/reopen and 50% video audio gain verified", canvas.width, canvas.height);
    }
    let remaining = landscape.iter().chain(&portrait).map(fs::read).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(remaining == originals, "Framing changed original media bytes");
    if !errors.is_empty() { bail!("{}", errors.join("\n")); }
    Ok(())
}
