//! H.264 decoding with Windows' built-in decoder (GPU-accelerated when a
//! device is given), producing tightly packed NV12 frames on the CPU.

use super::*;
use crate::d3d::Gpu;
use anyhow::ensure;
use std::sync::Arc;
use windows::core::VARIANT;

#[derive(Clone)]
pub struct DecodedFrame {
    pub pts_hns: i64,
    pub width: u32,
    pub height: u32,
    /// Y plane (width*height) then interleaved UV (width*height/2).
    pub nv12: Vec<u8>,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DecodedFrame({}x{} @ {})", self.width, self.height, self.pts_hns)
    }
}

pub struct VideoDecoder {
    m: SyncMft,
    len_size: usize,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    layout: Nv12Layout,
    /// Display size (from the SPS, after cropping).
    pub width: u32,
    pub height: u32,
    _gpu: Option<Arc<Gpu>>,
    pub hardware: bool,
}

unsafe impl Send for VideoDecoder {}

#[derive(Clone, Copy, Debug)]
struct Nv12Layout {
    frame_width: usize,
    frame_height: usize,
    default_stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Nv12Layout {
    fn from_type(media_type: &IMFMediaType, visible_size: (u32, u32)) -> Result<Self> {
        let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }?;
        let frame_width = (size >> 32) as usize;
        let frame_height = (size as u32) as usize;
        let default_stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map_or(frame_width, |value| value as usize);
        let aperture = display_aperture(media_type, &MF_MT_MINIMUM_DISPLAY_APERTURE)
            .or_else(|| display_aperture(media_type, &MF_MT_GEOMETRIC_APERTURE));
        let (x, y, width, height) = aperture.unwrap_or((
            0, 0, frame_width.min(visible_size.0 as usize), frame_height.min(visible_size.1 as usize),
        ));
        ensure!(frame_width >= 2 && frame_height >= 2 && frame_width % 2 == 0 && frame_height % 2 == 0,
            "Invalid NV12 frame size {frame_width}×{frame_height}");
        ensure!(width >= 2 && height >= 2 && x % 2 == 0 && y % 2 == 0 && width % 2 == 0 && height % 2 == 0
            && x.checked_add(width).is_some_and(|right| right <= frame_width)
            && y.checked_add(height).is_some_and(|bottom| bottom <= frame_height),
            "Invalid NV12 display aperture ({x}, {y}) {width}×{height} inside {frame_width}×{frame_height}");
        Ok(Self { frame_width, frame_height, default_stride, x, y, width, height })
    }

    fn pack(self, bytes: &[u8], stride: usize) -> Result<Vec<u8>> {
        ensure!(stride >= self.frame_width, "Invalid NV12 stride {stride}");
        let y_size = stride.checked_mul(self.frame_height).context("NV12 frame size overflow")?;
        let uv_size = stride.checked_mul(self.frame_height / 2).context("NV12 plane size overflow")?;
        let required = y_size.checked_add(uv_size).context("NV12 buffer size overflow")?;
        ensure!(bytes.len() >= required, "Truncated NV12 buffer: {} bytes, need {required}", bytes.len());
        if self.x == 0 && self.y == 0 && self.width == stride && self.height == self.frame_height {
            return Ok(bytes[..required].to_vec());
        }
        let mut packed = Vec::with_capacity(self.width * (self.height + self.height / 2));
        let (y, uv) = bytes[..required].split_at(y_size);
        for row in y.chunks_exact(stride).skip(self.y).take(self.height) {
            packed.extend_from_slice(&row[self.x..self.x + self.width]);
        }
        for row in uv.chunks_exact(stride).skip(self.y / 2).take(self.height / 2) {
            packed.extend_from_slice(&row[self.x..self.x + self.width]);
        }
        Ok(packed)
    }

    fn copy_sample(self, buffer: &IMFMediaBuffer) -> Result<Vec<u8>> {
        unsafe {
            if let Ok(surface) = buffer.cast::<IMF2DBuffer>() {
                // Ask MF to normalize surface pitch. GetContiguousLength
                // describes this packed copy, never the Lock2D memory layout.
                let mut bytes = vec![0; surface.GetContiguousLength()? as usize];
                surface.ContiguousCopyTo(&mut bytes)?;
                return self.pack(&bytes, self.frame_width);
            }
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let packed = if data.is_null() || length == 0 {
                Err(anyhow::anyhow!("Empty NV12 decoder buffer"))
            } else {
                // The exact Lock length bounds all plane and aperture reads.
                self.pack(std::slice::from_raw_parts(data, length as usize), self.default_stride)
            };
            let unlocked = buffer.Unlock();
            let packed = packed?;
            unlocked?;
            Ok(packed)
        }
    }
}

fn display_aperture(media_type: &IMFMediaType, key: &GUID) -> Option<(usize, usize, usize, usize)> {
    let mut area = MFVideoArea::default();
    let size = std::mem::size_of::<MFVideoArea>();
    let mut actual = 0;
    unsafe {
        // MFVideoArea contains only integer fields and occupies the documented attribute layout.
        let bytes = std::slice::from_raw_parts_mut((&mut area as *mut MFVideoArea).cast::<u8>(), size);
        media_type.GetBlob(key, bytes, Some(&mut actual)).ok()?;
    }
    if actual as usize != size || area.OffsetX.fract != 0 || area.OffsetY.fract != 0 {
        return None;
    }
    Some((
        area.OffsetX.value.try_into().ok()?,
        area.OffsetY.value.try_into().ok()?,
        area.Area.cx.try_into().ok()?,
        area.Area.cy.try_into().ok()?,
    ))
}

impl VideoDecoder {
    /// `low_latency`: the stream has no B-frames (ClipVault's own files), so
    /// frames can come out immediately.
    pub fn new(avcc: &[u8], gpu: Option<Arc<Gpu>>, low_latency: bool) -> Result<VideoDecoder> {
        init();
        let (len_size, sps, pps) = crate::h264::parse_avcc(avcc).context("bad avcC")?;
        let (width, height) = sps.first().and_then(|s| crate::h264::sps_dimensions(s)).context("can't read the video size")?;
        unsafe {
            let mft = create_mft(&CLSID_MSH264DecoderMFT)?;
            let mut hardware = false;
            if let Some(g) = &gpu {
                let attrs = mft.GetAttributes()?;
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                    if let Ok(mgr) = g.mf_manager() {
                        hardware = mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize).is_ok();
                    }
                }
            }
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                if low_latency {
                    let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                }
                if !hardware {
                    let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4).clamp(2, 8);
                    let _ = api.SetValue(&CODECAPI_AVDecNumWorkerThreads, &VARIANT::from(threads));
                }
            }
            let inp = new_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &inp, 0).context("decoder input type")?;
            let out = find_output_type(&mft, &MFVideoFormat_NV12).context("decoder can't output NV12")?;
            mft.SetOutputType(0, &out, 0).context("decoder output type")?;
            let layout = Nv12Layout::from_type(&mft.GetOutputCurrentType(0)?, (width, height))?;
            let mut m = SyncMft::new(mft);
            m.begin()?;
            Ok(VideoDecoder { m, len_size, sps, pps, layout, width, height, _gpu: gpu, hardware })
        }
    }

    /// Decode one MP4 sample (length-prefixed NAL units).
    pub fn decode(&mut self, sample: &[u8], pts_hns: i64) -> Result<Vec<DecodedFrame>> {
        let annexb = crate::h264::avcc_to_annexb(sample, self.len_size, Some((&self.sps, &self.pps)));
        let s = sample_from(&annexb, pts_hns, 0)?;
        let mut got = Vec::new();
        let mut tries = 0;
        while !self.m.push(&s)? {
            self.collect(&mut got)?;
            tries += 1;
            if tries > 8 {
                bail!("decoder stuck");
            }
        }
        self.collect(&mut got)?;
        Ok(got)
    }

    fn collect(&mut self, out: &mut Vec<DecodedFrame>) -> Result<()> {
        loop {
            let mut samples = Vec::new();
            let changed = self.m.pull_all(&mut samples)?;
            for s in samples {
                out.push(self.frame(&s)?);
            }
            if !changed {
                return Ok(());
            }
            unsafe {
                let t = find_output_type(&self.m.mft, &MFVideoFormat_NV12)?;
                self.m.mft.SetOutputType(0, &t, 0)?;
                self.layout = Nv12Layout::from_type(
                    &self.m.mft.GetOutputCurrentType(0)?, (self.width, self.height),
                )?;
            }
            self.m.refresh_out_info();
        }
    }

    fn frame(&self, sample: &IMFSample) -> Result<DecodedFrame> {
        let pts_hns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
        let nv12 = self.layout.copy_sample(&buffer)?;
        Ok(DecodedFrame {
            pts_hns,
            width: self.layout.width as u32,
            height: self.layout.height as u32,
            nv12,
        })
    }

    /// Forget buffered frames (after a seek).
    pub fn flush(&mut self) {
        self.m.flush();
    }

    /// Output the frames still inside the decoder (end of stream).
    pub fn drain(&mut self) -> Result<Vec<DecodedFrame>> {
        self.m.drain()?;
        let mut out = Vec::new();
        self.collect(&mut out)?;
        // Be ready for more input afterwards.
        unsafe {
            let _ = self.m.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(out)
    }
}

/// NV12 -> RGB (BT.709 limited range), then scaled down to `max_w` wide
/// (0 = keep) with a smooth filter.
pub fn nv12_to_rgb(f: &DecodedFrame, max_w: u32) -> image::RgbImage {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut img = image::RgbImage::new(f.width, f.height);
    for y in 0..h {
        for x in 0..w {
            let yy = f.nv12[y * w + x] as f32;
            let ui = w * h + (y / 2) * w + (x & !1);
            let u = f.nv12[ui] as f32 - 128.0;
            let v = f.nv12[ui + 1] as f32 - 128.0;
            let c = (yy - 16.0) * 1.164_383;
            let r = c + 1.792_741 * v;
            let g = c - 0.213_249 * u - 0.532_909 * v;
            let b = c + 2.112_402 * u;
            img.put_pixel(x as u32, y as u32, image::Rgb([r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8]));
        }
    }
    if max_w > 0 && f.width > max_w {
        let nh = ((f.height as u64 * max_w as u64) / f.width.max(1) as u64).max(1) as u32;
        image::imageops::thumbnail(&img, max_w, nh)
    } else {
        img
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_aperture_from_padded_nv12_and_rejects_short_buffers() -> Result<()> {
        let layout = Nv12Layout {
            frame_width: 6,
            frame_height: 4,
            default_stride: 8,
            x: 2,
            y: 2,
            width: 4,
            height: 2,
        };
        let backing: Vec<u8> = (0..48).collect();
        let packed = layout.pack(&backing, layout.default_stride)?;
        assert_eq!(packed, [18, 19, 20, 21, 26, 27, 28, 29, 42, 43, 44, 45]);
        assert!(layout.pack(&backing[..47], layout.default_stride).is_err());
        assert!(layout.pack(&backing, 4).is_err());
        Ok(())
    }
}
