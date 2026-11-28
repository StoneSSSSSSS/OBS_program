use anyhow::{Context, Result};
use cv_core::editor::{AssetInfo, AssetKind, Canvas, Framing, VisualTransform};
use image::{imageops, Rgba, RgbaImage};
use std::{fs::File, io::{BufWriter, Write}, path::Path};
use windows::{
    Foundation::TimeSpan,
    Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
        ExifOrientationMode,
    },
    Media::Editing::MediaClip,
    Storage::{FileAccessMode, StorageFile},
};

fn decoder(file: &StorageFile) -> Result<BitmapDecoder> {
    let stream = file.OpenAsync(FileAccessMode::Read)?.get()?;
    Ok(BitmapDecoder::CreateAsync(&stream)?.get()?)
}

pub(super) fn probe(file: &StorageFile) -> Result<AssetInfo> {
    let decoder = decoder(file)?;
    Ok(AssetInfo {
        kind: AssetKind::Image,
        fps_milli: None,
        duration: 0,
        width: decoder.OrientedPixelWidth()?,
        height: decoder.OrientedPixelHeight()?,
    })
}

pub(super) fn decode(file: &StorageFile, canvas: Canvas, orientation: VisualTransform) -> Result<RgbaImage> {
    let decoder = decoder(file)?;
    let width = decoder.OrientedPixelWidth()?;
    let height = decoder.OrientedPixelHeight()?;
    let transform = BitmapTransform::new()?;
    let data = decoder.GetPixelDataTransformedAsync(
        BitmapPixelFormat::Rgba8,
        BitmapAlphaMode::Straight,
        &transform,
        ExifOrientationMode::RespectExifOrientation,
        ColorManagementMode::ColorManageToSRgb,
    )?.get()?.DetachPixelData()?;
    let image = RgbaImage::from_raw(width, height, data.to_vec()).context("Invalid decoded image dimensions")?;
    let image = super::visual_transforms::image(image, orientation);
    let (width, height) = image.dimensions();
    if let Framing::Fill { horizontal, vertical } = orientation.framing {
        let crop = super::visual_transforms::fill_rect((width, height), canvas, (horizontal, vertical), 1.0);
        let left = (crop.X.round() as u32).min(width - 1);
        let top = (crop.Y.round() as u32).min(height - 1);
        let right = ((crop.X + crop.Width).round() as u32).clamp(left + 1, width);
        let bottom = ((crop.Y + crop.Height).round() as u32).clamp(top + 1, height);
        let cropped = imageops::crop_imm(&image, left, top, right - left, bottom - top).to_image();
        let fitted = imageops::resize(&cropped, canvas.width, canvas.height, imageops::FilterType::Triangle);
        let mut frame = solid(canvas, [0, 0, 0]);
        imageops::overlay(&mut frame, &fitted, 0, 0);
        return Ok(frame);
    }
    let scale = (canvas.width as f64 / width as f64).min(canvas.height as f64 / height as f64);
    let target = (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    );
    let fitted = imageops::resize(&image, target.0, target.1, imageops::FilterType::Triangle);
    let mut frame = solid(canvas, [0, 0, 0]);
    imageops::overlay(&mut frame, &fitted, ((canvas.width - target.0) / 2).into(), ((canvas.height - target.1) / 2).into());
    Ok(frame)
}

pub(super) fn solid(canvas: Canvas, [r, g, b]: [u8; 3]) -> RgbaImage {
    RgbaImage::from_pixel(canvas.width, canvas.height, Rgba([r, g, b, 255]))
}

/// The composition API accepts a JPEG reliably on Windows desktop. Stage an
/// opaque canvas so picture previews and exports use the same framing.
pub(super) fn clip(frame: RgbaImage, duration: TimeSpan, path: &Path) -> Result<MediaClip> {
    let rgb = image::DynamicImage::ImageRgba8(frame).into_rgb8();
    let mut file = BufWriter::new(File::create_new(path)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 95).encode_image(&rgb)?;
    file.flush()?;
    drop(file);
    let file = super::storage_file(path)?;
    Ok(MediaClip::CreateFromImageFileAsync(&file, duration)?.get()?)
}
