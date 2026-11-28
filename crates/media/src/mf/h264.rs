//! H.264 encoding with the GPU's hardware encoder (Intel Quick Sync, NVIDIA
//! NVENC, AMD VCE/VCN, exposed through Media Foundation) or Windows' software
//! encoder as a fallback.

use super::*;
use crate::d3d::Gpu;
use std::collections::VecDeque;
use std::sync::Arc;
use windows::core::VARIANT;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateControl {
    /// Variable bitrate around `kbps` (peaks up to `max_kbps`).
    Vbr { kbps: u32, max_kbps: u32 },
    Cbr { kbps: u32 },
    /// Constant quality 1-100 (higher = better, bigger).
    Quality { level: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    /// The GPU's encoder if it works, otherwise software.
    Auto,
    Software,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate: RateControl,
    /// Keyframe every this many frames.
    pub gop: u32,
    pub choice: EncoderChoice,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Annex B access unit.
    pub data: Vec<u8>,
    pub time_hns: i64,
    pub key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
    pub vendor: String,
}

enum Mode {
    Async { events: IMFMediaEventGenerator, need_input: u32, drained: bool },
    Sync(SyncMft),
}

pub struct VideoEncoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    mode: Mode,
    gpu: Option<Arc<Gpu>>,
    queue: VecDeque<(IMFSample, bool)>,
    pub info: EncoderInfo,
    cfg: EncoderConfig,
    frame_hns: i64,
    pub dropped: u64,
}

unsafe impl Send for VideoEncoder {}

/// List the H.264 encoders Windows offers (hardware first).
pub fn list_encoders() -> Vec<EncoderInfo> {
    init();
    let mut out = Vec::new();
    for (flags, hw) in [(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, true), (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, false)] {
        for a in enum_mfts(MFT_CATEGORY_VIDEO_ENCODER, flags, Some((MFMediaType_Video, MFVideoFormat_NV12)), Some((MFMediaType_Video, MFVideoFormat_H264))) {
            out.push(EncoderInfo {
                name: activate_string(&a, &MFT_FRIENDLY_NAME_Attribute),
                hardware: hw,
                vendor: activate_string(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
            });
        }
    }
    out
}

/// Hardware encoders that belong to `gpu`'s adapter.
fn hardware_activates(gpu: &Gpu) -> Vec<IMFActivate> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        if MFCreateAttributes(&mut attrs, 1).is_err() {
            return Vec::new();
        }
        let Some(attrs) = attrs else { return Vec::new() };
        let _ = attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, gpu.luid);
        let inp = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let outp = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        let r = MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&inp), Some(&outp), &attrs, &mut arr, &mut n);
        let mut out = Vec::new();
        if r.is_ok() && !arr.is_null() {
            for i in 0..n as usize {
                if let Some(a) = std::ptr::read(arr.add(i)) {
                    out.push(a);
                }
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(arr as *const _));
        }
        out
    }
}

fn set_video_common(t: &IMFMediaType, cfg: &EncoderConfig) -> Result<()> {
    unsafe {
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, 2)?; // BT.709
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, 5)?; // BT.709
        t.SetUINT32(&MF_MT_YUV_MATRIX, 1)?; // BT.709
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, 2)?; // 16-235
    }
    Ok(())
}

fn mean_kbps(rate: &RateControl, cfg: &EncoderConfig) -> u32 {
    match rate {
        RateControl::Vbr { kbps, .. } | RateControl::Cbr { kbps } => *kbps,
        RateControl::Quality { .. } => {
            // A nominal value; quality mode ignores it but types need one.
            ((cfg.width as u64 * cfg.height as u64 * cfg.fps as u64) / 8000).max(2000) as u32
        }
    }
}

fn codec_set(api: &ICodecAPI, key: &GUID, v: VARIANT, what: &str) {
    unsafe {
        if let Err(e) = api.SetValue(key, &v) {
            tracing::debug!("encoder ignores {what}: {e}");
        }
    }
}

fn configure_codec(api: &ICodecAPI, cfg: &EncoderConfig) {
    match cfg.rate {
        RateControl::Cbr { kbps } => {
            codec_set(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(0u32), "CBR");
            codec_set(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(kbps * 1000), "bitrate");
        }
        RateControl::Vbr { kbps, max_kbps } => {
            codec_set(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(1u32), "VBR");
            codec_set(api, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(kbps * 1000), "bitrate");
            codec_set(api, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(max_kbps.max(kbps) * 1000), "max bitrate");
        }
        RateControl::Quality { level } => {
            codec_set(api, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(3u32), "quality mode");
            codec_set(api, &CODECAPI_AVEncCommonQuality, VARIANT::from(level.clamp(1, 100)), "quality");
        }
    }
    codec_set(api, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(cfg.gop.max(1)), "GOP");
    codec_set(api, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "B-frames");
    codec_set(api, &CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(60u32), "quality/speed");
    codec_set(api, &CODECAPI_AVEncH264CABACEnable, VARIANT::from(true), "CABAC");
    codec_set(api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency");
}

impl VideoEncoder {
    /// Create an encoder. With a GPU, its own hardware encoder is tried
    /// first; input is then GPU textures. Without one (or if it fails), the
    /// software encoder takes CPU NV12 frames.
    pub fn new(gpu: Option<Arc<Gpu>>, cfg: EncoderConfig) -> Result<VideoEncoder> {
        init();
        if cfg.width % 2 != 0 || cfg.height % 2 != 0 || cfg.width < 16 || cfg.height < 16 {
            bail!("video size {}x{} is not encodable", cfg.width, cfg.height);
        }
        if let (Some(g), EncoderChoice::Auto) = (&gpu, cfg.choice) {
            for a in hardware_activates(g) {
                let name = activate_string(&a, &MFT_FRIENDLY_NAME_Attribute);
                match Self::hardware(g.clone(), &a, &cfg) {
                    Ok(mut e) => {
                        e.info = EncoderInfo { name, hardware: true, vendor: crate::d3d::vendor_name(g.vendor_id).into() };
                        tracing::info!("video encoder: {} ({}x{} @ {} fps)", e.info.name, cfg.width, cfg.height, cfg.fps);
                        return Ok(e);
                    }
                    Err(e) => tracing::warn!("hardware encoder {name} unusable: {e:#}"),
                }
            }
        }
        let mut e = Self::software(&cfg)?;
        e.gpu = gpu; // for reading GPU frames back to the CPU
        tracing::info!("video encoder: Windows software H.264 ({}x{} @ {} fps)", cfg.width, cfg.height, cfg.fps);
        Ok(e)
    }

    fn hardware(gpu: Arc<Gpu>, a: &IMFActivate, cfg: &EncoderConfig) -> Result<VideoEncoder> {
        unsafe {
            let mft: IMFTransform = a.ActivateObject().context("activating encoder")?;
            let attrs = mft.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if !is_async {
                bail!("expected an asynchronous hardware encoder");
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let mgr = gpu.mf_manager()?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize).context("encoder rejected the GPU")?;
            let codec: Option<ICodecAPI> = mft.cast().ok();
            if let Some(c) = &codec {
                configure_codec(c, cfg);
            }
            let out = new_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
            set_video_common(&out, cfg)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, mean_kbps(&cfg.rate, cfg) * 1000)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            mft.SetOutputType(0, &out, 0).context("encoder output type")?;
            let inp = new_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
            set_video_common(&inp, cfg)?;
            mft.SetInputType(0, &inp, 0).context("encoder input type")?;
            if let Some(c) = &codec {
                configure_codec(c, cfg); // some encoders only accept settings after types
            }
            let events: IMFMediaEventGenerator = mft.cast()?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(VideoEncoder {
                mft,
                codec,
                mode: Mode::Async { events, need_input: 0, drained: false },
                gpu: Some(gpu),
                queue: VecDeque::new(),
                info: EncoderInfo { name: String::new(), hardware: true, vendor: String::new() },
                frame_hns: HNS / cfg.fps.max(1) as i64,
                cfg: cfg.clone(),
                dropped: 0,
            })
        }
    }

    fn software(cfg: &EncoderConfig) -> Result<VideoEncoder> {
        unsafe {
            let mft = create_mft(&CLSID_MSH264EncoderMFT)?;
            let codec: Option<ICodecAPI> = mft.cast().ok();
            // B-frames and GOP must be set before the media types here.
            if let Some(c) = &codec {
                configure_codec(c, cfg);
            }
            let out = new_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
            set_video_common(&out, cfg)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, mean_kbps(&cfg.rate, cfg) * 1000)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            mft.SetOutputType(0, &out, 0).context("software encoder output type")?;
            let inp = new_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
            set_video_common(&inp, cfg)?;
            mft.SetInputType(0, &inp, 0).context("software encoder input type")?;
            if let Some(c) = &codec {
                configure_codec(c, cfg);
            }
            let mut m = SyncMft::new(mft.clone());
            m.begin()?;
            Ok(VideoEncoder {
                mft,
                codec,
                mode: Mode::Sync(m),
                gpu: None,
                queue: VecDeque::new(),
                info: EncoderInfo { name: "Windows software H.264".into(), hardware: false, vendor: "Microsoft".into() },
                frame_hns: HNS / cfg.fps.max(1) as i64,
                cfg: cfg.clone(),
                dropped: 0,
            })
        }
    }

    /// True if frames must be given as CPU NV12 ([`VideoEncoder::encode_cpu`]).
    pub fn wants_cpu_frames(&self) -> bool {
        matches!(self.mode, Mode::Sync(_))
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Encode a GPU NV12 texture (hardware encoders). The texture must not be
    /// overwritten until a few frames later (use a small pool).
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D, time_hns: i64, force_key: bool) -> Result<Vec<EncodedFrame>> {
        if self.wants_cpu_frames() {
            let gpu = self.gpu.clone().context("no GPU for readback")?;
            let data = gpu.read_nv12(tex, self.cfg.width, self.cfg.height)?;
            return self.encode_cpu(&data, time_hns, force_key);
        }
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buf.SetCurrentLength(len);
                }
            }
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(time_hns)?;
            s.SetSampleDuration(self.frame_hns)?;
            self.submit(s, force_key)
        }
    }

    /// Encode a tightly packed CPU NV12 frame.
    pub fn encode_cpu(&mut self, nv12: &[u8], time_hns: i64, force_key: bool) -> Result<Vec<EncodedFrame>> {
        if !self.wants_cpu_frames() {
            let gpu = self.gpu.clone().context("no GPU")?;
            let tex = gpu.upload_nv12(nv12, self.cfg.width, self.cfg.height, windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET)?;
            return self.encode_texture(&tex, time_hns, force_key);
        }
        let s = sample_from(nv12, time_hns, self.frame_hns)?;
        self.submit(s, force_key)
    }

    fn force_key(&self) {
        if let Some(c) = &self.codec {
            codec_set(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "force keyframe");
        }
    }

    fn submit(&mut self, s: IMFSample, force_key: bool) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        match &mut self.mode {
            Mode::Sync(_) => {
                if force_key {
                    self.force_key();
                }
                let Mode::Sync(m) = &mut self.mode else { unreachable!() };
                let mut got = Vec::new();
                while !m.push(&s)? {
                    m.pull_all(&mut got)?;
                }
                m.pull_all(&mut got)?;
                for g in got {
                    out.push(to_frame(&g)?);
                }
            }
            Mode::Async { .. } => {
                self.queue.push_back((s, force_key));
                // Never let a slow encoder build up a backlog: drop the oldest.
                while self.queue.len() > 4 {
                    self.queue.pop_front();
                    self.dropped += 1;
                }
                self.pump(&mut out, false)?;
            }
        }
        Ok(out)
    }

    /// Process pending encoder events without waiting.
    fn pump(&mut self, out: &mut Vec<EncodedFrame>, wait: bool) -> Result<()> {
        loop {
            let Mode::Async { events, need_input, drained } = &mut self.mode else { return Ok(()) };
            // Feed as much as the encoder asked for.
            while *need_input > 0 {
                let Some((s, key)) = self.queue.pop_front() else { break };
                if key {
                    if let Some(c) = &self.codec {
                        codec_set(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "force keyframe");
                    }
                }
                unsafe { self.mft.ProcessInput(0, &s, 0) }.context("encoder input")?;
                *need_input -= 1;
            }
            let flags = if wait { MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0) } else { MF_EVENT_FLAG_NO_WAIT };
            let ev = match unsafe { events.GetEvent(flags) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("encoder event"),
            };
            let kind = unsafe { ev.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                *need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(f) = self.async_output()? {
                    out.push(f);
                }
            } else if kind == METransformDrainComplete.0 as u32 {
                *drained = true;
                return Ok(());
            }
        }
    }

    fn async_output(&mut self) -> Result<Option<EncodedFrame>> {
        unsafe {
            let mut buf = [MFT_OUTPUT_DATA_BUFFER::default()];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buf[0].pSample);
            let _ev = std::mem::ManuallyDrop::take(&mut buf[0].pEvents);
            match r {
                Ok(()) => match sample {
                    Some(s) => Ok(Some(to_frame(&s)?)),
                    None => Ok(None),
                },
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Output type renegotiation: accept what it proposes.
                    if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                        let _ = self.mft.SetOutputType(0, &t, 0);
                    }
                    Ok(None)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) => Err(e).context("encoder output"),
            }
        }
    }

    /// Collect finished frames without submitting anything.
    pub fn poll(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        self.pump(&mut out, false)?;
        Ok(out)
    }

    /// Encode everything still queued and return the remaining frames.
    pub fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        match &mut self.mode {
            Mode::Sync(m) => {
                m.drain()?;
                let mut got = Vec::new();
                m.pull_all(&mut got)?;
                for g in got {
                    out.push(to_frame(&g)?);
                }
            }
            Mode::Async { .. } => {
                // Feed the queue, then drain.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while !self.queue.is_empty() && std::time::Instant::now() < deadline {
                    self.pump(&mut out, false)?;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                unsafe {
                    let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                    self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                }
                while std::time::Instant::now() < deadline {
                    self.pump(&mut out, false)?;
                    if let Mode::Async { drained: true, .. } = self.mode {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        Ok(out)
    }
}

fn to_frame(s: &IMFSample) -> Result<EncodedFrame> {
    let data = sample_bytes(s)?;
    let time_hns = unsafe { s.GetSampleTime().unwrap_or(0) };
    let key = is_keyframe(s) || crate::h264::split_annexb(&data).iter().any(|n| crate::h264::nal_type(n) == crate::h264::NAL_IDR);
    Ok(EncodedFrame { data, time_hns, key })
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d::{Converter, Gpu};
    use crate::mp4::frag::{FragWriter, TrackRun};
    use crate::mp4::{Codec, SampleRef, TrackSpec};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    fn pattern(w: u32, h: u32, f: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&[((x + f * 4) % 256) as u8, ((y + f * 2) % 256) as u8, ((x ^ y) % 256) as u8, 255]);
            }
        }
        v
    }

    /// Mux encoded frames into a fragmented MP4 (one fragment per GOP).
    fn mux(frames: &[EncodedFrame], w: u32, h: u32, path: &std::path::Path) -> usize {
        let first = crate::h264::annexb_to_avcc(&frames[0].data);
        let (sps, pps) = (first.sps.clone().expect("SPS in first keyframe"), first.pps.clone().expect("PPS"));
        let spec = TrackSpec { id: 1, timescale: 90_000, codec: Codec::H264 { avcc: crate::h264::build_avcc(&sps, &pps) }, width: w as u16, height: h as u16, name: "Video".into(), enabled: true, default_duration: 1500 };
        let mut wr = FragWriter::create(path, &[spec], 0, None).unwrap();
        let conv: Vec<(Vec<u8>, bool, i64)> = frames.iter().map(|f| (crate::h264::annexb_to_avcc(&f.data).data, f.key, f.time_hns)).collect();
        let mut keys = 0;
        let mut i = 0;
        while i < conv.len() {
            let mut j = i + 1;
            while j < conv.len() && !conv[j].1 {
                j += 1;
            }
            let samples: Vec<SampleRef> = conv[i..j].iter().map(|(d, k, _)| SampleRef { data: d, duration: 1500, sync: *k, cts: 0 }).collect();
            wr.write_fragment(&[TrackRun { track_id: 1, base_time: conv[i].2 as u64 * 9 / 1000, samples: &samples }]).unwrap();
            keys += 1;
            i = j;
        }
        wr.finish().unwrap();
        keys
    }

    fn ffmpeg_decodes(path: &std::path::Path) -> Option<String> {
        let exe = std::path::PathBuf::from(r"C:\ffmpeg-7.1-essentials_build\bin\ffprobe.exe");
        if !exe.is_file() {
            return None;
        }
        let out = std::process::Command::new(exe)
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames,width,height", "-of", "compact"])
            .arg(path)
            .output()
            .unwrap();
        Some(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    fn run(choice: EncoderChoice) {
        let encs = list_encoders();
        eprintln!("encoders: {encs:#?}");
        let gpu = Arc::new(Gpu::primary().unwrap());
        let (w, h) = (640u32, 360u32);
        let cfg = EncoderConfig { width: w, height: h, fps: 60, rate: RateControl::Vbr { kbps: 4000, max_kbps: 8000 }, gop: 60, choice };
        let mut enc = VideoEncoder::new(Some(gpu.clone()), cfg).unwrap();
        eprintln!("using {:?}", enc.info);
        let src = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0)).unwrap();
        let pool: Vec<_> = (0..6).map(|_| gpu.nv12_target(w, h).unwrap()).collect();
        let mut conv = Converter::new(&gpu, (w, h), (w, h), true).unwrap();
        let mut frames = Vec::new();
        for f in 0..120u32 {
            gpu.write_bgra(&src, &pattern(w, h, f), w);
            let dst = &pool[f as usize % pool.len()];
            conv.blit(&src, dst).unwrap();
            frames.extend(enc.encode_texture(dst, f as i64 * HNS / 60, f == 90).unwrap());
        }
        frames.extend(enc.finish().unwrap());
        assert_eq!(enc.dropped, 0);
        assert!(frames.len() >= 118, "only {} frames came out", frames.len());
        assert!(frames[0].key, "first frame must be a keyframe");
        let keys: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.key).map(|(i, _)| i).collect();
        eprintln!("keyframes at {keys:?}");
        assert!(keys.contains(&60) || keys.contains(&90), "{keys:?}");
        // Timestamps survive.
        eprintln!("times: {:?}", frames.iter().take(12).map(|f| f.time_hns).collect::<Vec<_>>());
        assert_eq!(frames[10].time_hns, 10 * HNS / 60);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("enc.mp4");
        mux(&frames, w, h, &p);
        if let Some(report) = ffmpeg_decodes(&p) {
            eprintln!("ffprobe: {report}");
            assert!(report.contains(&format!("width={w}|height={h}")), "{report}");
            assert!(report.contains(&format!("nb_read_frames={}", frames.len())), "{report}");
            assert!(!report.to_lowercase().contains("error"), "{report}");
        }
    }

    #[test]
    fn hardware_or_auto_encoder() {
        run(EncoderChoice::Auto);
    }

    #[test]
    fn software_encoder() {
        run(EncoderChoice::Software);
    }
}
