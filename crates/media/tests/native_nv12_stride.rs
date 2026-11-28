use anyhow::{ensure, Context, Result};
use cv_core::editor::{Canvas, Project, TICKS_PER_SECOND};
use cv_media::{editor::{probe, render, thumbnail, RenderControl}, thumb::picture};
use image::{Rgb, RgbImage};
use std::path::{Path, PathBuf};

#[test]
#[ignore = "requires Windows codecs and CLIPVAULT_EDITOR_TEST_FIXTURES"]
fn native_nv12_matches_windows_at_padded_frame_sizes() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES")
        .context("Set CLIPVAULT_EDITOR_TEST_FIXTURES to an in-project QA folder")?).canonicalize()?;
    let qa = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    ensure!(root.starts_with(qa), "Stride QA must use an in-project QA folder");
    let work = tempfile::Builder::new().prefix("nv12-stride-").tempdir_in(root)?;
    let colors = [
        [223u8, 33, 44], [41, 201, 74], [52, 71, 214], [238, 194, 47],
        [48, 179, 220], [175, 58, 188], [218, 117, 39], [153, 181, 59],
        [47, 70, 165], [211, 190, 217], [66, 114, 38], [218, 54, 121],
        [99, 69, 151], [159, 123, 45], [47, 159, 119], [224, 205, 153],
    ];
    for (width, height) in [(320, 192), (1080, 1920), (1728, 1080), (1920, 1080), (1234, 778)] {
        let canvas = Canvas { width, height, fps: 30 };
        let source_image = work.path().join(format!("source-{width}x{height}.png"));
        RgbImage::from_fn(width, height, |x, y| {
            Rgb(colors[((4 * y / height) * 4 + 4 * x / width) as usize])
        }).save(&source_image)?;
        let mut project = Project::default();
        project.canvas = canvas;
        let id = project.import(source_image.clone(), probe(&source_image)?)?;
        project.append(id)?;
        project.shots[0].range.end = TICKS_PER_SECOND / 2;
        let path = work.path().join(format!("native-{width}x{height}.mp4"));
        render(&project, &path, &RenderControl::default())
            .with_context(|| format!("Encode the {width}×{height} test card"))?;

        let native = picture(&path, 0.0, 0)
            .with_context(|| format!("Decode {width}×{height} through ClipVault's native player path"))?;
        ensure!(native.dimensions() == (width, height),
            "Native decode returned {:?} for {width}×{height}", native.dimensions());
        let mut oracle = Project::default();
        oracle.canvas = canvas;
        let id = oracle.import(path.clone(), probe(&path)?)?;
        oracle.append(id)?;
        let windows = thumbnail(&oracle, 0, width, height)
            .with_context(|| format!("Decode {width}×{height} through Windows composition"))?;
        ensure!(windows.dimensions() == (width, height),
            "Windows returned {:?} for {width}×{height}", windows.dimensions());
        let mut peak_channel_error = 0;
        for row in 0..4 {
            for col in 0..4 {
                for (dx, dy) in [(1, 1), (2, 2), (3, 3)] {
                    let x = (4 * col + dx) * width / 16;
                    let y = (4 * row + dy) * height / 16;
                    let expected = colors[(row * 4 + col) as usize];
                    let native = native.get_pixel(x, y).0;
                    let oracle = windows.get_pixel(x, y).0;
                    for channel in 0..3 {
                        let error = oracle[channel].abs_diff(native[channel]);
                        peak_channel_error = peak_channel_error.max(error);
                        ensure!(error <= 40 && native[channel].abs_diff(expected[channel]) <= 24,
                            "{width}×{height} at ({x}, {y}): native {native:?}, Windows {oracle:?}, fixture {expected:?}");
                    }
                }
            }
        }
        eprintln!("{width}×{height}: 48 color samples matched, peak channel difference {peak_channel_error}");
    }
    Ok(())
}
