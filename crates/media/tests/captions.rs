use anyhow::{ensure, Context, Result};
use cv_core::editor::{Canvas, CaptionPosition, Project, Span, TICKS_PER_SECOND as S};
use cv_media::{mf::h264dec::nv12_to_rgb, mp4::reader::Mp4Reader, thumb::frame_at_time};
use cv_media::editor::{probe, render, thumbnail, RenderControl};
use std::path::PathBuf;

#[test]
#[ignore = "requires a local Windows media profile and native encoders"]
fn caption_panel_and_text_exist_only_during_cue_in_preview_and_export() -> Result<()> {
    let base = PathBuf::from(std::env::var_os("CLIPVAULT_DATA_DIR").context("choose a test profile")?);
    std::fs::create_dir_all(&base)?;
    let out = tempfile::Builder::new().prefix("caption-check-").tempdir_in(&base)?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 640, height: 360, fps: 25 };
    project.color([24, 150, 55], 4 * S)?;
    project.caption("Caption timing".into(), Span { start: S, end: 2 * S })?;
    project.caption("Second line".into(), Span { start: 3 * S, end: 18 * S / 5 })?;
    let movie = out.path().join("captioned.mp4");
    render(&project, &movie, &RenderControl::default())?;
    let mut published = Project::default();
    published.canvas = project.canvas;
    let id = published.import(movie.clone(), probe(&movie)?)?;
    published.append(id)?;
    let reader = Mp4Reader::open(&movie)?;
    let video = reader.video().context("Missing subtitle-test video")?;
    for (input, expected) in [(1.160, 1.160), (1.159999, 1.120), (1.160001, 1.160)] {
        let (_, actual) = frame_at_time(&reader, video, input)?;
        ensure!((actual - expected).abs() < 0.000001, "At {input}s decoded {actual}s, expected {expected}s");
    }
    let mut incorrect = Vec::new();
    for (kind, p) in [("preview", &project), ("export", &published)] {
        for (n, (time, visible)) in [(S / 2, false), (24*S/25, false), (S, true), (3*S/2, true), (49*S/25, true), (2*S, false), (51*S/25, false), (5*S/2, false), (13*S/4, true), (18*S/5, false), (15*S/4, false)].into_iter().enumerate() {
            let image = if kind == "export" {
                let (frame, _) = frame_at_time(&reader, video, time as f64 / S as f64)?;
                image::DynamicImage::ImageRgb8(nv12_to_rgb(&frame, 640)).to_rgba8()
            } else { thumbnail(p, time, 640, 360)? };
            let top_changed = (10..235).flat_map(|y| (25..615).map(move |x| (x,y)))
                .filter(|&(x,y)| image.get_pixel(x,y)[1] < 50).count();
            let mut ink = 0;
            let mut panel = 0;
            for y in 253..354 { for x in 22..618 {
                let [r,g,b,_] = image.get_pixel(x,y).0;
                if r < 45 && g < 45 && b < 45 {panel+=1;}
                if r > 178 && g > 178 && b > 178 {ink+=1;}
            }}
            println!("{kind} {time} ticks: panel {panel}, text {ink}, upper changes {top_changed}");
            if visible { image.save(base.join(format!("caption-{kind}-{n}.png")))?; }
            if top_changed >= 800 || panel > 15000 || image.get_pixel(560, 320)[1] < 50 || ((panel > 500 && ink > 60) != visible) { incorrect.push((kind, time)); }
        }
    }
    project.caption_position = CaptionPosition::Top;
    let top_mp4 = out.path().join("top.mp4");
    render(&project, &top_mp4, &RenderControl::default())?;
    let top_reader = Mp4Reader::open(&top_mp4)?;
    let (decoded, _) = frame_at_time(&top_reader, top_reader.video().context("Missing top-caption video")?, 1.501)?;
    let scrub = thumbnail(&project, 3 * S / 2, 640, 360)?;
    for image in [image::DynamicImage::ImageRgba8(scrub).to_rgb8(), nv12_to_rgb(&decoded, 640)] {
        let top_ink = (0..110).flat_map(|y| (80..560).map(move |x| (x,y)))
            .filter(|&(x,y)| image.get_pixel(x,y)[0] > 178).count();
        ensure!(top_ink > 60 && image.get_pixel(320, 325)[1] > 65, "Top placement must leave the bottom of the picture visible");
    }
    ensure!(incorrect.is_empty(), "Unexpected caption coverage: {incorrect:?}");
    Ok(())
}

#[test]
#[ignore = "requires the synthetic numbered source and native Windows encoders"]
fn scrub_before_and_during_a_credit_after_the_last_picture() -> Result<()> {
    let kit = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES").context("choose the synthetic editor kit")?);
    let path = kit.join("inputs").join("Frame counter.mp4");
    let profile = PathBuf::from(std::env::var_os("CLIPVAULT_DATA_DIR").context("choose a test profile")?);
    let out = tempfile::Builder::new().prefix("end-credit-").tempdir_in(profile)?;
    for legacy in [false, true] {
        let mut project = Project::default();
        project.canvas = Canvas { width: 640, height: 360, fps: 25 };
        let mut info = probe(&path)?;
        if legacy { info.duration = 4 * S + S / 100; }
        let id = project.import(path.clone(), info)?;
        project.append(id)?;
        project.caption("End credit".into(), Span { start: 4 * S + S / 25, end: 5 * S })?;
        let movie = out.path().join(format!("credit-{legacy}.mp4"));
        render(&project, &movie, &RenderControl::default())?;
        let reader = Mp4Reader::open(&movie)?;
        let video = reader.video().context("Missing credit video")?;
        for (at, expected_text) in [(4 * S, false), (4 * S + S / 25, true), (4 * S + S / 2, true)] {
            let scrub = thumbnail(&project, at, 640, 360)?;
            let (decoded, _) = frame_at_time(&reader, video, at as f64 / S as f64)?;
            let exported = nv12_to_rgb(&decoded, 640);
            for image in [image::DynamicImage::ImageRgba8(scrub).to_rgb8(), exported] {
                let ink = image.pixels().filter(|p| p[0] > 130 && p[1] > 130 && p[2] > 130).count();
                ensure!((ink > 40) == expected_text, "legacy {legacy}, {at} ticks: {ink} bright pixels");
                ensure!(image.get_pixel(16, 16)[1] < 20, "expected black after the last picture");
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires the synthetic numbered source and native Windows encoders"]
fn a_single_frame_credit_switches_on_the_requested_presentation_frame() -> Result<()> {
    let kit = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES").context("choose the synthetic editor kit")?);
    let path = kit.join("inputs").join("Frame counter.mp4");
    let base = PathBuf::from(std::env::var_os("CLIPVAULT_DATA_DIR").context("choose a test profile")?);
    let temp = tempfile::Builder::new().prefix("cue-splice-").tempdir_in(&base)?;
    let mut failures = Vec::new();
    for fps in [24, 30, 60] {
        let mut project = Project::default();
        project.canvas = Canvas { width: 640, height: 360, fps };
        project.caption_position = CaptionPosition::Top;
        let asset = project.import(path.clone(), probe(&path)?)?;
        project.append(asset)?;
        let splice = 55 * S / 10;
        let end = splice + 15 * S / 1000;
        project.caption("I".into(), Span { start: 4 * S, end: splice })?;
        project.caption("WIDE OPEN SCREENING CREDIT".into(), Span { start: splice, end })?;
        project.caption("I".into(), Span { start: end, end: 6 * S })?;
        let movie = temp.path().join(format!("credit-{fps}.mp4"));
        render(&project, &movie, &RenderControl::default())?;
        let reader = Mp4Reader::open(&movie)?;
        let video = reader.video().context("No exported video")?;
        let half_frame = S / (2 * u64::from(fps));
        for (sample_time, expected_wide) in [(splice - half_frame, false), (splice, true), (splice + 3 * half_frame, false)] {
            let (frame, pts) = frame_at_time(&reader, video, sample_time as f64 / S as f64)?;
            let image = nv12_to_rgb(&frame, 640);
            let xs = (12..110).flat_map(|y| (0..640).filter(|&x| image.get_pixel(x, y)[0] > 150).collect::<Vec<_>>());
            let bounds = xs.fold(None::<(u32, u32)>, |bounds, x| Some(bounds.map_or((x,x), |(l,r)| (l.min(x), r.max(x)))));
            let width = bounds.map_or(0, |(left,right)| right-left+1);
            if width == 0 || (width > 90) != expected_wide { failures.push((fps, sample_time, pts, width, expected_wide)); }
        }
    }
    ensure!(failures.is_empty(), "Caption changes landed on the wrong published frame: {failures:?}");
    Ok(())
}
