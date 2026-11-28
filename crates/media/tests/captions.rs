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
    let mut incorrect = Vec::new();
    for (kind, p) in [("preview", &project), ("export", &published)] {
        for (n, (time, visible)) in [(S / 2, false), (24*S/25, false), (S, true), (3*S/2, true), (49*S/25, true), (2*S, false), (51*S/25, false), (5*S/2, false), (13*S/4, true), (18*S/5, false), (15*S/4, false)].into_iter().enumerate() {
            let image = if kind == "export" {
                let (frame, _) = frame_at_time(&reader, video, time as f64 / S as f64 + 0.001)?;
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
