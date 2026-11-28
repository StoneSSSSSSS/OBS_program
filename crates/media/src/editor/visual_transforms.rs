use super::{positive_ticks, profile, storage_file, ticks, RenderControl};
use anyhow::{ensure, Context, Result};
use cv_core::editor::{Canvas, Rotation, Span, VisualTransform};
use image::{imageops, RgbaImage};
use std::{fs, path::Path, thread, time::Duration};
use windows::{
    Foundation::{AsyncStatus, Size},
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

/// Bake only the trimmed picture. MediaClip's effect pipeline rejects the transform
/// on Windows desktop. The file transcoder accepts the same native effect.
pub(super) fn video(
    source: &StorageFile,
    range: Span,
    transform: VisualTransform,
    canvas: Canvas,
    path: &Path,
    control: &RenderControl,
) -> Result<(MediaClip, Span)> {
    control.check()?;
    let effect = VideoTransformEffectDefinition::new()?;
    effect.SetRotation(match transform.rotation {
        Rotation::None => MediaRotation::None,
        Rotation::Clockwise90 => MediaRotation::Clockwise90Degrees,
        Rotation::Clockwise180 => MediaRotation::Clockwise180Degrees,
        Rotation::Clockwise270 => MediaRotation::Clockwise270Degrees,
    })?;
    effect.SetOutputSize(Size { Width: canvas.width as f32, Height: canvas.height as f32 })?;
    if transform.mirror {
        // The Windows effect mirrors before rotating. Reflect the source
        // vertically for a horizontal flip in the rotated picture.
        let direction = match transform.rotation {
            Rotation::Clockwise90 | Rotation::Clockwise270 => MediaMirroringOptions::Vertical,
            Rotation::None | Rotation::Clockwise180 => MediaMirroringOptions::Horizontal,
        };
        effect.SetMirror(direction)?;
    }

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
        .get().context("Prepare the rotated and mirrored video")?;
    ensure!(prepared.CanTranscode()?, "Windows could not prepare the video orientation ({:?})",
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
    result.context("Encode the rotated and mirrored video")?;
    let clip = MediaClip::CreateFromFileAsync(&destination)?.get()?;
    let actual = positive_ticks(clip.OriginalDuration()?)?;
    Ok((clip, Span { start: 0, end: range.len().min(actual) }))
}
