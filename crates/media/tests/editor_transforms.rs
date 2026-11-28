use anyhow::{bail, ensure, Context, Result};
use cv_core::editor::{Canvas, Project, Rotation, SoundRole, Span, VisualTransform, TICKS_PER_SECOND};
use cv_media::{
    audioread::read_range,
    editor::{probe, render, thumbnail, RenderControl},
    mp4::reader::Mp4Reader,
};
use image::{Rgb, RgbImage, RgbaImage};
use std::{fs, path::{Path, PathBuf}};

const S: u64 = TICKS_PER_SECOND;
const SHOT_LENGTH: u64 = 6 * S / 5;
const RED: [u8; 3] = [225, 32, 32];
const GREEN: [u8; 3] = [32, 196, 55];
const BLUE: [u8; 3] = [37, 68, 217];
const YELLOW: [u8; 3] = [244, 205, 43];
const CANVAS: Canvas = Canvas { width: 320, height: 180, fps: 25 };

fn expect_picture(frame: &RgbaImage, transform: VisualTransform, description: &str) -> Result<()> {
    ensure!(frame.dimensions() == (CANVAS.width, CANVAS.height),
        "{description}: unexpected dimensions {:?}", frame.dimensions());
    let (mut colors, rotated) = match transform.rotation {
        Rotation::None => ([RED, GREEN, BLUE, YELLOW], false),
        Rotation::Clockwise90 => ([BLUE, RED, YELLOW, GREEN], true),
        Rotation::Clockwise180 => ([YELLOW, BLUE, GREEN, RED], false),
        Rotation::Clockwise270 => ([GREEN, YELLOW, RED, BLUE], true),
    };
    if transform.mirror {
        colors.swap(0, 1);
        colors.swap(2, 3);
    }
    let half_width = if rotated { 50 } else { CANVAS.width / 2 };
    let center = CANVAS.width / 2;
    let [left, right] = [center - half_width / 2, center + half_width / 2];
    let points = [(left, 45), (right, 45), (left, 135), (right, 135)];
    for ((x, y), expected) in points.into_iter().zip(colors) {
        let pixel = frame.get_pixel(x, y).0;
        ensure!(expected.into_iter().zip(pixel).all(|(a, b)| a.abs_diff(b) < 28),
            "{description} {transform:?} at {x},{y}: expected {expected:?}, found {pixel:?}");
    }
    let edge = frame.get_pixel(60, 60).0;
    ensure!(edge[..3].iter().all(|&channel| channel < 18) == rotated,
        "{description} {transform:?}: unexpected letterbox pixel {edge:?}");
    Ok(())
}

#[test]
#[ignore = "requires Windows media codecs and CLIPVAULT_EDITOR_TEST_FIXTURES"]
fn imported_shots_rotate_and_mirror_in_preview_and_mp4() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES")
        .context("Set CLIPVAULT_EDITOR_TEST_FIXTURES to an in-project QA folder")?).canonicalize()?;
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    ensure!(root.starts_with(local), "Orientation QA must use an in-project QA folder");
    let music = root.join("inputs/Night drive music.wav");
    let work = tempfile::Builder::new().prefix("shot-orientation-").tempdir_in(root)?;
    let png = work.path().join("quadrants.png");
    let mp4 = work.path().join("quadrants.mp4");
    let frame = RgbImage::from_fn(CANVAS.width, CANVAS.height, |x, y| Rgb(match (x < 160, y < 90) {
        (true, true) => RED,
        (false, true) => GREEN,
        (true, false) => BLUE,
        (false, false) => YELLOW,
    }));
    frame.save(&png)?;

    let mut source = Project::default();
    source.canvas = CANVAS;
    let still = source.import(png.clone(), probe(&png)?)?;
    source.append(still)?;
    source.shots[0].range.end = SHOT_LENGTH + S;
    let audio = source.import(music.clone(), probe(&music)?)?;
    source.add_sound(audio, SoundRole::Music, 0)?;
    source.sounds[0].range.end = SHOT_LENGTH + S;
    render(&source, &mp4, &RenderControl::default()).context("Render source video fixture")?;
    let media = source.import(mp4.clone(), probe(&mp4)?)?;
    let originals = [fs::read(&png)?, fs::read(&mp4)?];

    let legacy = work.path().join("no-orientation.cvproj");
    source.save(&legacy)?;
    ensure!(!fs::read_to_string(&legacy)?.contains("visual_transform"),
        "An identity shot should retain its legacy serialized shape");
    let mut project = Project::load(&legacy)?;
    ensure!(project.shots[0].visual_transform.is_identity(), "Legacy shots default to identity");
    project.shots.clear();
    project.sounds.clear();
    for asset in [still, media] {
        for rotation in [Rotation::None, Rotation::Clockwise90, Rotation::Clockwise180, Rotation::Clockwise270] {
            for mirror in [false, true] {
                project.append(asset)?;
                let shot = project.shots.last_mut().context("Missing appended fixture")?;
                shot.range = Span { start: S / 5, end: S / 5 + SHOT_LENGTH };
                shot.visual_transform = VisualTransform { rotation, mirror, ..VisualTransform::default() };
                if asset == media && mirror { shot.gain = 0.5; }
            }
        }
    }
    let split_shot = project.shots[11].clone();
    let split_at = 11 * SHOT_LENGTH + SHOT_LENGTH / 2;
    let split = project.split(split_shot.id, split_at)?;
    let edited = work.path().join("orientation.cvproj");
    project.save(&edited)?;
    let expected_shots = project.shots;
    let project = Project::load(&edited)?;
    ensure!(project.shots == expected_shots, "Shot data must round-trip");
    ensure!(project.shots[11].visual_transform == split_shot.visual_transform
        && project.shots[12].visual_transform == split_shot.visual_transform
        && project.shots[12].id == split, "Both split shots must keep their orientation through save and reopen");

    let output = work.path().join("transformed-edit.mp4");
    render(&project, &output, &RenderControl::default()).context("Render the orientation project")?;
    let mut reimport = Project::default();
    reimport.canvas = CANVAS;
    let rendered = reimport.import(output.clone(), probe(&output)?)?;
    reimport.append(rendered)?;
    let mut failures = Vec::new();
    let mut at = 0;
    for (index, shot) in project.shots.iter().enumerate() {
        let tick = at + shot.range.len() / 2;
        for (timeline, kind) in [(&project, "preview"), (&reimport, "MP4")] {
            let description = format!("shot {} {kind}", index + 1);
            let result = thumbnail(timeline, tick, CANVAS.width, CANVAS.height)
                .and_then(|frame| expect_picture(&frame, shot.visual_transform, &description));
            if let Err(err) = result { failures.push(format!("{description}: {err:#}")); }
        }
        at += shot.range.len();
    }
    if !failures.is_empty() { bail!("{}", failures.join("\n")); }
    ensure!([fs::read(&png)?, fs::read(&mp4)?] == originals, "Source media bytes changed");

    let source_mp4 = Mp4Reader::open(&mp4)?;
    let output_mp4 = Mp4Reader::open(&output)?;
    let source_audio = *source_mp4.audio().first().context("Source video dropped the fixture music")?;
    let output_audio = *output_mp4.audio().first().context("Rotated export dropped the embedded audio")?;
    let rms = |pcm: &[f32]| (pcm.iter().map(|sample| sample * sample).sum::<f32>() / pcm.len() as f32).sqrt();
    let reference = rms(&read_range(&source_mp4, source_audio, 0.45, 0.6)?);
    let at = 9.0 * SHOT_LENGTH as f64 / S as f64;
    let mirrored = rms(&read_range(&output_mp4, output_audio, at + 0.25, at + 0.4)?);
    ensure!(reference > 0.01 && (mirrored / reference - 0.5).abs() < 0.13,
        "Transformed clip audio at 50% gain: expected RMS near {}, found {mirrored}", reference * 0.5);
    Ok(())
}
