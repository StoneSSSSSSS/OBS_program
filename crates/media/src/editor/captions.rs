//! Render the same caption card in the native export and the scrub preview.
use super::{stills, title, RenderControl};
use anyhow::Result;
use cv_core::editor::{Canvas, CaptionPosition, Project};
use std::path::Path;
use windows::Foundation::{Rect, TimeSpan};
use windows::Media::Editing::{MediaComposition, MediaOverlay, MediaOverlayLayer};

fn panel(project: &Project, text: &str) -> Result<(image::RgbaImage, u32, u32)> {
    let canvas = project.canvas;
    let size = Canvas {
        width: (canvas.width * 88 / 100).max(1),
        height: (canvas.height * 18 / 100).max(1),
        fps: canvas.fps,
    };
    let frame = title::draw_sized(size, text, (canvas.height / 22).max(12) as i32)?;
    let (mut left, mut top, mut right, mut bottom) = (size.width, size.height, 0, 0);
    for (x, y, pixel) in frame.enumerate_pixels() {
        if pixel[0] > 12 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    let cropped = if right > left && bottom > top {
        let x = left.saturating_sub((canvas.height / 45).max(2));
        let y = top.saturating_sub((canvas.height / 95).max(2));
        let w = right.saturating_add((canvas.height / 45).max(2)).min(size.width) - x;
        let h = bottom.saturating_add((canvas.height / 95).max(2)).min(size.height) - y;
        image::imageops::crop_imm(&frame, x, y, w, h).to_image()
    } else { frame };
    let x = (canvas.width - cropped.width()) / 2;
    let margin = canvas.height * 6 / 100;
    let y = match project.caption_position {
        CaptionPosition::Top => margin,
        CaptionPosition::Bottom => canvas.height.saturating_sub(margin + cropped.height()),
    };
    Ok((cropped, x, y))
}

pub(super) fn draw_at(frame: &mut image::RgbaImage, project: &Project, at: u64) -> Result<()> {
    let active: Vec<_> = project.captions.iter()
        .filter(|cue| cue.range.start <= at && at < cue.range.end)
        .collect();
    if active.is_empty() { return Ok(()); }
    let text = active.iter().map(|cue| cue.text.as_str()).collect::<Vec<_>>().join("\n");
    let (original, left, top) = panel(project, &text)?;
    let (w, h) = frame.dimensions();
    let sx = |n: u32| (u64::from(n) * u64::from(w) / u64::from(project.canvas.width)) as u32;
    let sy = |n: u32| (u64::from(n) * u64::from(h) / u64::from(project.canvas.height)) as u32;
    let card = image::imageops::resize(&original, sx(original.width()).max(1), sy(original.height()).max(1), image::imageops::FilterType::Triangle);
    image::imageops::replace(frame, &card, i64::from(sx(left)), i64::from(sy(top)));
    Ok(())
}

pub(super) fn apply(composition: &MediaComposition, project: &Project, work: &Path, control: &RenderControl) -> Result<()> {
    if project.captions.is_empty() { return Ok(()); }
    let layer = MediaOverlayLayer::new()?;
    let overlays = layer.Overlays()?;
    for (i, run) in project.caption_runs().iter().enumerate() {
        control.check()?;
        let (frame, x, y) = panel(project, &run.text)?;
        let area = Rect { X: x as f32, Y: y as f32, Width: frame.width() as f32, Height: frame.height() as f32 };
        // WinRT paints the overlay at its end timestamp as well as its start.
        let duration = run.range.len().saturating_sub(1).max(1);
        let clip = stills::clip(frame, TimeSpan { Duration: duration as i64 }, &work.join(format!("caption-{i}.jpg")))?;
        let overlay = MediaOverlay::CreateWithPositionAndOpacity(&clip, area, 1.0)?;
        overlay.SetDelay(TimeSpan { Duration: run.range.start as i64 })?;
        overlays.Append(&overlay)?;
    }
    composition.OverlayLayers()?.Append(&layer)?;
    Ok(())
}
