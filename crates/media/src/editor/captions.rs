//! Render the same caption card in the native export and the scrub preview.
use super::{stills, title, RenderControl};
use anyhow::Result;
use cv_core::editor::{Canvas, Project};
use std::path::Path;
use windows::Foundation::{Rect, TimeSpan};
use windows::Media::Editing::{MediaComposition, MediaOverlay, MediaOverlayLayer};

fn card(canvas: Canvas, text: &str) -> Result<image::RgbaImage> {
    let size = Canvas {
        width: (canvas.width * 88 / 100).max(1),
        height: (canvas.height * 18 / 100).max(1),
        fps: canvas.fps,
    };
    title::draw_sized(size, text, (canvas.height / 22).max(12) as i32)
}

pub(super) fn draw_at(frame: &mut image::RgbaImage, project: &Project, at: u64) -> Result<()> {
    let active: Vec<_> = project.captions.iter()
        .filter(|cue| cue.range.start <= at && at < cue.range.end)
        .collect();
    if active.is_empty() { return Ok(()); }
    let text = active.iter().map(|cue| cue.text.as_str()).collect::<Vec<_>>().join("\n");
    let original = card(project.canvas, &text)?;
    let (w, h) = frame.dimensions();
    let panel = image::imageops::resize(
        &original, (w * 88 / 100).max(1), (h * 18 / 100).max(1), image::imageops::FilterType::Triangle,
    );
    image::imageops::replace(frame, &panel, i64::from(w * 6 / 100), i64::from(h * 76 / 100));
    Ok(())
}

pub(super) fn apply(composition: &MediaComposition, project: &Project, work: &Path, control: &RenderControl) -> Result<()> {
    if project.captions.is_empty() { return Ok(()); }
    let (w, h) = (project.canvas.width, project.canvas.height);
    let area = Rect { X: w as f32 * 0.06, Y: h as f32 * 0.76, Width: w as f32 * 0.88, Height: h as f32 * 0.18 };
    let layer = MediaOverlayLayer::new()?;
    let overlays = layer.Overlays()?;
    for (i, run) in project.caption_runs().iter().enumerate() {
        control.check()?;
        let frame = card(project.canvas, &run.text)?;
        let clip = stills::clip(frame, TimeSpan { Duration: run.range.len() as i64 }, &work.join(format!("caption-{i}.jpg")))?;
        let overlay = MediaOverlay::CreateWithPositionAndOpacity(&clip, area, 1.0)?;
        overlay.SetDelay(TimeSpan { Duration: run.range.start as i64 })?;
        overlays.Append(&overlay)?;
    }
    composition.OverlayLayers()?.Append(&layer)?;
    Ok(())
}
