use super::{positive_ticks, profile, storage_file, ticks, RenderControl};
use anyhow::{ensure, Context, Result};
use cv_core::editor::{Canvas, Framing, Rotation, Span, VisualTransform};
use image::{imageops, RgbaImage};
use std::{fs, path::Path, thread, time::Duration};
use windows::{
    Foundation::{AsyncStatus, Rect, Size},
    Media::{
        Editing::MediaClip,
        Effects::VideoTransformEffectDefinition,
        MediaProperties::{AudioEncodingProperties, MediaMirroringOptions, MediaRotation},
        Transcoding::{MediaTranscoder, MediaVideoProcessingAlgorithm},
    },
    Storage::StorageFile,
};

pub(super) fn image(frame: RgbaImage, transform: VisualTransform) -> RgbaImage {
    let mut frame = match transform.rotation {
        Rotation::None => frame,
        Rotation::Clockwise90 => imageops::rotate90(&frame),
        Rotation::Clockwise180 => imageops::rotate180(&frame),
        Rotation::Clockwise270 => imageops::rotate270(&frame),
    };
    if transform.mirror {
        imageops::flip_horizontal_in_place(&mut frame);
    }
    frame
}

/// Crop in the displayed picture's coordinates. Focus moves only along the overflow.
pub(super) fn fill_rect(
    (width, height): (u32, u32),
    canvas: Canvas,
    (horizontal, vertical): (u8, u8),
    pixel_aspect: f64,
) -> Rect {
    let (width, height) = (f64::from(width), f64::from(height));
    let aspect = f64::from(canvas.width) / f64::from(canvas.height) / pixel_aspect;
    let crop_width = width.min(height * aspect);
    let crop_height = height.min(width / aspect);
    Rect {
        X: ((width - crop_width) * f64::from(horizontal) / 100.0) as f32,
        Y: ((height - crop_height) * f64::from(vertical) / 100.0) as f32,
        Width: crop_width as f32,
        Height: crop_height as f32,
    }
}

/// Bake only the trimmed picture. MediaClip's effect pipeline rejects the transform
/// on Windows desktop. The file transcoder accepts the same native effect.
pub(super) fn video(
    source: &StorageFile,
    input: &MediaClip,
    range: Span,
    transform: VisualTransform,
    canvas: Canvas,
    path: &Path,
    control: &RenderControl,
) -> Result<(MediaClip, Span)> {
    if let Framing::Fill { horizontal, vertical } = transform.framing {
        let properties = input.GetVideoEncodingProperties()?;
        let (width, height) = (properties.Width()?, properties.Height()?);
        let ratio = properties.PixelAspectRatio()?;
        let pixel_aspect = f64::from(ratio.Numerator()?.max(1)) / f64::from(ratio.Denominator()?.max(1));
        let quarter_turn = matches!(transform.rotation, Rotation::Clockwise90 | Rotation::Clockwise270);
        let (size, pixel_aspect) = if quarter_turn {
            ((height, width), pixel_aspect.recip())
        } else {
            ((width, height), pixel_aspect)
        };

        if quarter_turn && transform.mirror {
            // WinRT applies a different crop origin with a quarter turn and a
            // mirror. Apply that orientation first, then crop the displayed frame.
            let even = |value: f64| ((value / 2.0).round().max(8.0) as u32).saturating_mul(2);
            let oriented = Canvas {
                width: even(f64::from(size.0) * pixel_aspect),
                height: even(f64::from(size.1)),
                fps: canvas.fps,
            };
            let staged = path.with_extension("oriented.mp4");
            let effect = picture_effect(oriented, transform, None)?;
            let (_, range) = transcode(source, range, oriented, &effect, &staged, control)?;
            let source = storage_file(&staged)?;
            let crop = fill_rect((oriented.width, oriented.height), canvas, (horizontal, vertical), 1.0);
            let effect = picture_effect(canvas, VisualTransform::default(), Some(crop))?;
            return transcode(&source, range, canvas, &effect, path, control);
        }

        let crop = fill_rect(size, canvas, (horizontal, vertical), pixel_aspect);
        let effect = picture_effect(canvas, transform, Some(crop))?;
        return transcode(source, range, canvas, &effect, path, control);
    }
    let effect = picture_effect(canvas, transform, None)?;
    transcode(source, range, canvas, &effect, path, control)
}

fn picture_effect(
    canvas: Canvas,
    transform: VisualTransform,
    crop: Option<Rect>,
) -> Result<VideoTransformEffectDefinition> {
    let effect = VideoTransformEffectDefinition::new()?;
    effect.SetRotation(match transform.rotation {
        Rotation::None => MediaRotation::None,
        Rotation::Clockwise90 => MediaRotation::Clockwise90Degrees,
        Rotation::Clockwise180 => MediaRotation::Clockwise180Degrees,
        Rotation::Clockwise270 => MediaRotation::Clockwise270Degrees,
    })?;
    effect.SetOutputSize(Size { Width: canvas.width as f32, Height: canvas.height as f32 })?;
    if let Some(crop) = crop { effect.SetCropRectangle(crop)?; }
    if transform.mirror {
        // The Windows effect mirrors before rotating. Reflect the source
        // vertically for a horizontal flip in the rotated picture.
        let direction = match transform.rotation {
            Rotation::Clockwise90 | Rotation::Clockwise270 => MediaMirroringOptions::Vertical,
            Rotation::None | Rotation::Clockwise180 => MediaMirroringOptions::Horizontal,
        };
        effect.SetMirror(direction)?;
    }
    Ok(effect)
}

fn transcode(
    source: &StorageFile,
    range: Span,
    canvas: Canvas,
    effect: &VideoTransformEffectDefinition,
    path: &Path,
    control: &RenderControl,
) -> Result<(MediaClip, Span)> {
    control.check()?;
    let profile = profile(canvas)?;
    profile.SetAudio(None::<&AudioEncodingProperties>)?;
    let transcoder = MediaTranscoder::new()?;
    transcoder.SetAlwaysReencode(true)?;
    transcoder.SetVideoProcessingAlgorithm(MediaVideoProcessingAlgorithm::MrfCrf444)?;
    transcoder.SetTrimStartTime(ticks(range.start)?)?;
    transcoder.SetTrimStopTime(ticks(range.end)?)?;
    transcoder.AddVideoEffectWithSettings(&effect.ActivatableClassId()?, true, &effect.Properties()?)?;

    fs::File::create_new(path)?;
    let destination = storage_file(path)?;
    let prepared = transcoder.PrepareFileTranscodeAsync(source, &destination, &profile)?
        .get().context("Prepare the video framing")?;
    ensure!(prepared.CanTranscode()?, "Windows could not prepare the video framing ({:?})",
        prepared.FailureReason()?);
    control.check()?;
    let operation = prepared.TranscodeAsync()?;
    let mut cancellation_sent = false;
    loop {
        if operation.Status()? != AsyncStatus::Started {
            break;
        }
        if !cancellation_sent && control.check().is_err() {
            let _ = operation.Cancel();
            cancellation_sent = true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let result = operation.GetResults();
    let _ = operation.Close();
    control.check()?;
    result.context("Encode the framed video")?;
    let clip = MediaClip::CreateFromFileAsync(&destination)?.get()?;
    let actual = positive_ticks(clip.OriginalDuration()?)?;
    Ok((clip, Span { start: 0, end: range.len().min(actual) }))
}
