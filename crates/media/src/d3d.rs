//! Direct3D 11: adapters and monitors, textures, and the GPU video processor
//! used to convert/scale between BGRA (desktop) and NV12 (video).

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

/// A monitor and the GPU driving it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    /// e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    pub rect: (i32, i32, i32, i32),
    pub primary: bool,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub luid: u64,
}

impl OutputInfo {
    pub fn width(&self) -> u32 {
        (self.rect.2 - self.rect.0).max(0) as u32
    }
    pub fn height(&self) -> u32 {
        (self.rect.3 - self.rect.1).max(0) as u32
    }
}

pub fn vendor_name(id: u32) -> &'static str {
    match id {
        0x8086 => "Intel",
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x1414 => "Microsoft",
        _ => "GPU",
    }
}

fn luid_u64(l: windows::Win32::Foundation::LUID) -> u64 {
    ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64
}

fn wide(s: &[u16]) -> String {
    let n = s.iter().position(|c| *c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

fn factory() -> Result<IDXGIFactory1> {
    unsafe { CreateDXGIFactory1().context("creating DXGI factory") }
}

/// Every monitor attached to a GPU.
pub fn list_outputs() -> Vec<OutputInfo> {
    let mut out = Vec::new();
    let Ok(f) = factory() else { return out };
    unsafe {
        let mut ai = 0;
        while let Ok(adapter) = f.EnumAdapters1(ai) {
            let desc = adapter.GetDesc1().unwrap_or_default();
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                if let Ok(d) = output.GetDesc() {
                    if d.AttachedToDesktop.as_bool() {
                        let r = d.DesktopCoordinates;
                        out.push(OutputInfo {
                            adapter_index: ai,
                            output_index: oi,
                            device_name: wide(&d.DeviceName),
                            rect: (r.left, r.top, r.right, r.bottom),
                            primary: r.left == 0 && r.top == 0,
                            adapter_name: wide(&desc.Description),
                            vendor_id: desc.VendorId,
                            luid: luid_u64(desc.AdapterLuid),
                        });
                    }
                }
                oi += 1;
            }
            ai += 1;
        }
    }
    out
}

/// A D3D11 device on one adapter, safe to share with Media Foundation.
pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub luid: u64,
    pub vendor_id: u32,
    pub name: String,
    manager: parking_lot::Mutex<Option<(IMFDXGIDeviceManager, u32)>>,
}

// D3D11 devices are free-threaded and we enable multithread protection on
// the immediate context.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    /// Device on the adapter with this index (from [`list_outputs`]).
    pub fn on_adapter(index: u32) -> Result<Gpu> {
        let f = factory()?;
        let adapter = unsafe { f.EnumAdapters1(index) }.context("adapter not found")?;
        Gpu::create(adapter)
    }

    /// Device on the adapter that drives the primary monitor.
    pub fn primary() -> Result<Gpu> {
        let outs = list_outputs();
        let idx = outs.iter().find(|o| o.primary).or(outs.first()).map(|o| o.adapter_index).unwrap_or(0);
        Gpu::on_adapter(idx)
    }

    fn create(adapter: IDXGIAdapter1) -> Result<Gpu> {
        unsafe {
            let desc = adapter.GetDesc1()?;
            let mut device = None;
            let mut ctx = None;
            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("creating Direct3D 11 device")?;
            let device: ID3D11Device = device.context("no device")?;
            let ctx: ID3D11DeviceContext = ctx.context("no context")?;
            if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(BOOL(1));
            }
            Ok(Gpu {
                device,
                ctx,
                adapter,
                luid: luid_u64(desc.AdapterLuid),
                vendor_id: desc.VendorId,
                name: wide(&desc.Description),
                manager: parking_lot::Mutex::new(None),
            })
        }
    }

    /// Media Foundation device manager wrapping this device (created once).
    pub fn mf_manager(&self) -> Result<IMFDXGIDeviceManager> {
        let mut m = self.manager.lock();
        if let Some((mgr, _)) = m.as_ref() {
            return Ok(mgr.clone());
        }
        unsafe {
            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.context("no device manager")?;
            mgr.ResetDevice(&self.device, token)?;
            *m = Some((mgr.clone(), token));
            Ok(mgr)
        }
    }

    pub fn texture(&self, w: u32, h: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG, usage: D3D11_USAGE, cpu: D3D11_CPU_ACCESS_FLAG) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: cpu.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.with_context(|| format!("creating {w}x{h} texture"))?;
        t.context("no texture")
    }

    /// NV12 texture the video processor can write and encoders can read.
    pub fn nv12_target(&self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        self.texture(w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0))
    }

    /// Copy an NV12 texture to the CPU: Y plane then interleaved UV, tightly packed.
    pub fn read_nv12(&self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<Vec<u8>> {
        let staging = self.texture(w, h, DXGI_FORMAT_NV12, D3D11_BIND_FLAG(0), D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ)?;
        unsafe {
            self.ctx.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let pitch = m.RowPitch as usize;
            let (w, h) = (w as usize, h as usize);
            let mut out = Vec::with_capacity(w * h * 3 / 2);
            let base = m.pData as *const u8;
            for row in 0..h + h / 2 {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w));
            }
            self.ctx.Unmap(&staging, 0);
            Ok(out)
        }
    }

    /// Upload tightly packed NV12 into a new texture.
    pub fn upload_nv12(&self, data: &[u8], w: u32, h: u32, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
        if data.len() < (w * h * 3 / 2) as usize {
            bail!("NV12 buffer too small");
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: w * h * 3 / 2 };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.context("uploading NV12")?;
        t.context("no texture")
    }

    /// Upload tightly packed BGRA into an existing texture.
    pub fn write_bgra(&self, tex: &ID3D11Texture2D, data: &[u8], w: u32) {
        unsafe { self.ctx.UpdateSubresource(tex, 0, None, data.as_ptr() as *const _, w * 4, 0) }
    }
}

/// GPU colour conversion + scaling (`VideoProcessorBlt`).
pub struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    inputs: Vec<(usize, ID3D11VideoProcessorInputView)>,
    outputs: Vec<(usize, ID3D11VideoProcessorOutputView)>,
    pub in_size: (u32, u32),
    pub out_size: (u32, u32),
}

unsafe impl Send for Converter {}

impl Converter {
    /// `rgb_to_yuv`: BGRA -> NV12 (capture). Otherwise NV12 -> NV12/BGRA.
    pub fn new(gpu: &Gpu, in_size: (u32, u32), out_size: (u32, u32), rgb_to_yuv: bool) -> Result<Converter> {
        unsafe {
            let vdev: ID3D11VideoDevice = gpu.device.cast().context("GPU has no video processor")?;
            let vctx: ID3D11VideoContext = gpu.ctx.cast().context("GPU has no video context")?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: in_size.0,
                InputHeight: in_size.1,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: out_size.0,
                OutputHeight: out_size.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&desc).context("video processor enumerator")?;
            let vp = vdev.CreateVideoProcessor(&en, 0).context("video processor")?;
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, BOOL(0));
            let full = RECT { left: 0, top: 0, right: in_size.0 as i32, bottom: in_size.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: out_size.0 as i32, bottom: out_size.1 as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, BOOL(1), Some(&full));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, BOOL(1), Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&vp, BOOL(1), Some(&dst));
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                if rgb_to_yuv {
                    v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                    v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                } else {
                    v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                    v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
                }
            }
            Ok(Converter { vdev, vctx, en, vp, inputs: Vec::new(), outputs: Vec::new(), in_size, out_size })
        }
    }

    fn input_view(&mut self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = tex.as_raw() as usize;
        if let Some((_, v)) = self.inputs.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut v = None;
        unsafe { self.vdev.CreateVideoProcessorInputView(tex, &self.en, &desc, Some(&mut v)) }.context("video processor input view")?;
        let v = v.context("no input view")?;
        if self.inputs.len() >= 16 {
            self.inputs.remove(0);
        }
        self.inputs.push((key, v.clone()));
        Ok(v)
    }

    fn output_view(&mut self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        let key = tex.as_raw() as usize;
        if let Some((_, v)) = self.outputs.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut v = None;
        unsafe { self.vdev.CreateVideoProcessorOutputView(tex, &self.en, &desc, Some(&mut v)) }.context("video processor output view")?;
        let v = v.context("no output view")?;
        if self.outputs.len() >= 16 {
            self.outputs.remove(0);
        }
        self.outputs.push((key, v.clone()));
        Ok(v)
    }

    /// Convert/scale `src` into `dst`.
    pub fn blit(&mut self, src: &ID3D11Texture2D, dst: &ID3D11Texture2D) -> Result<()> {
        let iv = self.input_view(src)?;
        let ov = self.output_view(dst)?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(iv)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.vp, &ov, 0, &streams) };
        unsafe {
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
        }
        r.context("VideoProcessorBlt")
    }
}

/// Output size for scaling `src` to fit `max_h` (0 = keep), keeping the
/// aspect ratio and rounding to even numbers (H.264 needs even sizes).
pub fn fit_size(src: (u32, u32), max_h: u32) -> (u32, u32) {
    let (w, h) = src;
    let (w, h) = if max_h > 0 && h > max_h {
        let nw = (w as u64 * max_h as u64 / h.max(1) as u64) as u32;
        (nw, max_h)
    } else {
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(fit_size((2880, 1800), 1080), (1728, 1080));
        assert_eq!(fit_size((1920, 1080), 1080), (1920, 1080));
        assert_eq!(fit_size((2560, 1440), 0), (2560, 1440));
        assert_eq!(fit_size((1366, 767), 0), (1366, 766));
    }

    #[test]
    fn gpu_and_monitors() {
        let outs = list_outputs();
        eprintln!("{outs:#?}");
        assert!(!outs.is_empty(), "no monitors found");
        let gpu = Gpu::primary().unwrap();
        eprintln!("primary GPU: {} ({})", gpu.name, vendor_name(gpu.vendor_id));
        // BGRA -> NV12 conversion of a solid colour.
        let (w, h) = (64u32, 32u32);
        let src = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0)).unwrap();
        let px: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 255, 255, 255]).collect();
        gpu.write_bgra(&src, &px, w);
        let dst = gpu.nv12_target(w, h).unwrap();
        let mut c = Converter::new(&gpu, (w, h), (w, h), true).unwrap();
        c.blit(&src, &dst).unwrap();
        let nv = gpu.read_nv12(&dst, w, h).unwrap();
        // White in limited-range BT.709 is Y=235, U=V=128.
        assert!((nv[0] as i32 - 235).abs() <= 2, "Y={}", nv[0]);
        assert!((nv[(w * h) as usize] as i32 - 128).abs() <= 2, "U={}", nv[(w * h) as usize]);
        gpu.mf_manager().unwrap();
    }
}
