//! H.264 decoding with Windows' built-in decoder (GPU-accelerated when a
//! device is given), producing tightly packed NV12 frames on the CPU.

use super::*;
use crate::d3d::Gpu;
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
    /// Display size (from the SPS, after cropping).
    pub width: u32,
    pub height: u32,
    _gpu: Option<Arc<Gpu>>,
    pub hardware: bool,
}

unsafe impl Send for VideoDecoder {}

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
            let mut m = SyncMft::new(mft);
            m.begin()?;
            Ok(VideoDecoder { m, len_size, sps, pps, width, height, _gpu: gpu, hardware })
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
                if let Some(f) = self.frame(&s)? {
                    out.push(f);
                }
            }
            if !changed {
                return Ok(());
            }
            unsafe {
                let t = find_output_type(&self.m.mft, &MFVideoFormat_NV12)?;
                self.m.mft.SetOutputType(0, &t, 0)?;
            }
            self.m.refresh_out_info();
        }
    }

    fn frame(&self, s: &IMFSample) -> Result<Option<DecodedFrame>> {
        unsafe {
            let pts = s.GetSampleTime().unwrap_or(0);
            let buf = s.GetBufferByIndex(0)?;
            let (w, h) = (self.width as usize, self.height as usize);
            let mut nv12 = vec![0u8; w * h * 3 / 2];
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut p = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch)?;
                let pitch = pitch.unsigned_abs() as usize;
                // The UV plane starts after the (possibly padded) Y plane.
                let total = b2.GetContiguousLength().unwrap_or(0) as usize;
                let padded_h = if pitch > 0 && total > 0 { total * 2 / 3 / pitch } else { h };
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(p.add(y * pitch), nv12.as_mut_ptr().add(y * w), w);
                }
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(p.add((padded_h + y) * pitch), nv12.as_mut_ptr().add(w * h + y * w), w);
                }
                b2.Unlock2D()?;
            } else {
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut p, None, Some(&mut len))?;
                let stride = self.width as usize;
                let rows = len as usize / stride.max(1) * 2 / 3;
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(p.add(y * stride), nv12.as_mut_ptr().add(y * w), w);
                }
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(p.add((rows + y) * stride), nv12.as_mut_ptr().add(w * h + y * w), w);
                }
                buf.Unlock()?;
            }
            Ok(Some(DecodedFrame { pts_hns: pts, width: self.width, height: self.height, nv12 }))
        }
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
