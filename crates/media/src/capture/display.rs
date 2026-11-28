//! Screen capture with DXGI desktop duplication, including the mouse cursor.
//!
//! Duplication only delivers a new image when something on screen changed;
//! [`DisplayCapture`] keeps a copy of the latest desktop image so the recorder
//! can produce a steady frame rate. The cursor is drawn on top with a tiny
//! D3D11 shader pass (inverting "XOR" cursors like the text I-beam included).

use crate::d3d::Gpu;
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

/// Why a capture call failed.
#[derive(Debug)]
pub enum CaptureError {
    /// The duplication was lost (resolution change, fullscreen switch, UAC
    /// prompt, lock screen). Recreate the capture.
    Lost,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CaptureError {
    fn from(e: anyhow::Error) -> Self {
        CaptureError::Other(e)
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Lost => write!(f, "screen capture was interrupted"),
            CaptureError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// A cursor image split into a normal (alpha-blended) layer and an
/// "invert what's underneath" layer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CursorImage {
    pub w: u32,
    pub h: u32,
    /// BGRA, straight alpha.
    pub color: Vec<u8>,
    /// BGRA: white/alpha 255 where the screen should be inverted.
    pub invert: Option<Vec<u8>>,
}

/// Convert a DXGI pointer shape into [`CursorImage`].
pub fn convert_cursor(kind: u32, w: u32, h: u32, pitch: u32, data: &[u8]) -> Option<CursorImage> {
    let (w, h, pitch) = (w as usize, h as usize, pitch as usize);
    match kind {
        2 => {
            // Colour: BGRA with alpha.
            let mut color = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                color.extend_from_slice(data.get(y * pitch..y * pitch + w * 4)?);
            }
            Some(CursorImage { w: w as u32, h: h as u32, color, invert: None })
        }
        1 => {
            // Monochrome: AND mask on top, XOR mask below (height is doubled).
            let h = h / 2;
            let mut color = vec![0u8; w * h * 4];
            let mut inv = vec![0u8; w * h * 4];
            let mut any_inv = false;
            for y in 0..h {
                for x in 0..w {
                    let bit = |row: usize| -> u8 { (data.get(row * pitch + x / 8).copied().unwrap_or(0) >> (7 - x % 8)) & 1 };
                    let (and, xor) = (bit(y), bit(y + h));
                    let o = (y * w + x) * 4;
                    match (and, xor) {
                        (0, 0) => color[o..o + 4].copy_from_slice(&[0, 0, 0, 255]),
                        (0, _) => color[o..o + 4].copy_from_slice(&[255, 255, 255, 255]),
                        (_, 0) => {}
                        _ => {
                            inv[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                            any_inv = true;
                        }
                    }
                }
            }
            Some(CursorImage { w: w as u32, h: h as u32, color, invert: any_inv.then_some(inv) })
        }
        4 => {
            // Masked colour: alpha 0 = opaque colour, 0xFF = XOR with colour.
            let mut color = vec![0u8; w * h * 4];
            let mut inv = vec![0u8; w * h * 4];
            let mut any_inv = false;
            for y in 0..h {
                for x in 0..w {
                    let i = y * pitch + x * 4;
                    let px = data.get(i..i + 4)?;
                    let o = (y * w + x) * 4;
                    if px[3] == 0 {
                        color[o..o + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
                    } else if px[0] | px[1] | px[2] != 0 {
                        inv[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                        any_inv = true;
                    }
                }
            }
            Some(CursorImage { w: w as u32, h: h as u32, color, invert: any_inv.then_some(inv) })
        }
        _ => None,
    }
}

const SHADER: &str = r#"
cbuffer C : register(b0) { float4 rect; };
Texture2D tex : register(t0);
SamplerState smp : register(s0);
struct VO { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VO vs(uint id : SV_VertexID) {
    VO o;
    float2 t = float2(id & 1, id >> 1);
    float2 p = lerp(rect.xy, rect.zw, t);
    o.pos = float4(p.x, p.y, 0, 1);
    o.uv = t;
    return o;
}
float4 ps(VO i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

fn compile(entry: &str, target: &str) -> Result<Vec<u8>> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errs: Option<ID3DBlob> = None;
        let e = std::ffi::CString::new(entry)?;
        let t = std::ffi::CString::new(target)?;
        let r = D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), PCSTR::null(), None, None, PCSTR(e.as_ptr() as *const u8), PCSTR(t.as_ptr() as *const u8), 0, 0, &mut code, Some(&mut errs));
        if r.is_err() {
            let msg = errs.map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).to_string()).unwrap_or_default();
            bail!("shader compile failed: {msg}");
        }
        let b = code.context("no shader")?;
        Ok(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec())
    }
}

struct CursorPainter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_alpha: ID3D11BlendState,
    blend_invert: ID3D11BlendState,
    color: Option<(ID3D11ShaderResourceView, u32, u32)>,
    invert: Option<ID3D11ShaderResourceView>,
}

impl CursorPainter {
    fn new(gpu: &Gpu) -> Result<CursorPainter> {
        unsafe {
            let d = &gpu.device;
            let vsb = compile("vs", "vs_4_0")?;
            let psb = compile("ps", "ps_4_0")?;
            let mut vs = None;
            d.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps = None;
            d.CreatePixelShader(&psb, None, Some(&mut ps))?;
            let mut cb = None;
            d.CreateBuffer(
                &D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, CPUAccessFlags: 0, MiscFlags: 0, StructureByteStride: 0 },
                None,
                Some(&mut cb),
            )?;
            let mut sampler = None;
            d.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MipLODBias: 0.0,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    BorderColor: [0.0; 4],
                    MinLOD: 0.0,
                    MaxLOD: f32::MAX,
                },
                Some(&mut sampler),
            )?;
            let blend = |src: D3D11_BLEND, dst: D3D11_BLEND| -> Result<ID3D11BlendState> {
                let mut desc = D3D11_BLEND_DESC::default();
                desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: true.into(),
                    SrcBlend: src,
                    DestBlend: dst,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_ZERO,
                    DestBlendAlpha: D3D11_BLEND_ONE,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                let mut b = None;
                d.CreateBlendState(&desc, Some(&mut b))?;
                b.context("blend state")
            };
            Ok(CursorPainter {
                vs: vs.context("vs")?,
                ps: ps.context("ps")?,
                cb: cb.context("cb")?,
                sampler: sampler.context("sampler")?,
                blend_alpha: blend(D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA)?,
                blend_invert: blend(D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA)?,
                color: None,
                invert: None,
            })
        }
    }

    fn srv(gpu: &Gpu, data: &[u8], w: u32, h: u32) -> Result<ID3D11ShaderResourceView> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut t = None;
            gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut t))?;
            let mut v = None;
            gpu.device.CreateShaderResourceView(&t.context("cursor texture")?, None, Some(&mut v))?;
            v.context("cursor view")
        }
    }

    fn set_image(&mut self, gpu: &Gpu, img: &CursorImage) -> Result<()> {
        self.color = Some((Self::srv(gpu, &img.color, img.w, img.h)?, img.w, img.h));
        self.invert = match &img.invert {
            Some(inv) => Some(Self::srv(gpu, inv, img.w, img.h)?),
            None => None,
        };
        Ok(())
    }

    /// Draw the cursor with its top-left at (x, y) onto `target` (size tw x th).
    fn draw(&self, gpu: &Gpu, target: &ID3D11RenderTargetView, tw: u32, th: u32, x: i32, y: i32) {
        let Some((color, cw, ch)) = &self.color else { return };
        unsafe {
            let c = &gpu.ctx;
            let x0 = x as f32 / tw as f32 * 2.0 - 1.0;
            let x1 = (x + *cw as i32) as f32 / tw as f32 * 2.0 - 1.0;
            let y0 = 1.0 - y as f32 / th as f32 * 2.0;
            let y1 = 1.0 - (y + *ch as i32) as f32 / th as f32 * 2.0;
            let rect = [x0, y0, x1, y1];
            c.UpdateSubresource(&self.cb, 0, None, rect.as_ptr() as *const _, 0, 0);
            c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: tw as f32, Height: th as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            c.VSSetShader(&self.vs, None);
            c.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            c.PSSetShader(&self.ps, None);
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.RSSetState(None);
            c.PSSetShaderResources(0, Some(&[Some(color.clone())]));
            c.OMSetBlendState(&self.blend_alpha, None, 0xFFFF_FFFF);
            c.Draw(4, 0);
            if let Some(inv) = &self.invert {
                c.PSSetShaderResources(0, Some(&[Some(inv.clone())]));
                c.OMSetBlendState(&self.blend_invert, None, 0xFFFF_FFFF);
                c.Draw(4, 0);
            }
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
            c.OMSetBlendState(None, None, 0xFFFF_FFFF);
        }
    }
}

pub struct DxgiCapture {
    gpu: Arc<Gpu>,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    pub size: (u32, u32),
    desktop: ID3D11Texture2D,
    compose: ID3D11Texture2D,
    compose_rtv: ID3D11RenderTargetView,
    painter: Option<CursorPainter>,
    cursor_visible: bool,
    cursor_pos: (i32, i32),
    cursor_shape: bool,
    shape_buf: Vec<u8>,
    /// A desktop image has been received since (re)starting.
    pub has_image: bool,
    /// Frames delivered by Windows (not repeats).
    pub new_frames: u64,
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    /// Capture output `output_index` of the GPU's adapter.
    pub fn new(gpu: Arc<Gpu>, output_index: u32) -> Result<DxgiCapture> {
        unsafe {
            let output: IDXGIOutput1 = gpu.adapter.EnumOutputs(output_index).context("monitor not found")?.cast()?;
            let desc = output.GetDesc()?;
            let r = desc.DesktopCoordinates;
            let size = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
            let bind = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
            let desktop = gpu.texture(size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0))?;
            let compose = gpu.texture(size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0))?;
            let mut rtv = None;
            gpu.device.CreateRenderTargetView(&compose, None, Some(&mut rtv))?;
            let painter = match CursorPainter::new(&gpu) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("cursor won't be drawn: {e:#}");
                    None
                }
            };
            let mut me = DxgiCapture {
                gpu,
                output,
                dup: None,
                size,
                desktop,
                compose,
                compose_rtv: rtv.context("render target")?,
                painter,
                cursor_visible: false,
                cursor_pos: (0, 0),
                cursor_shape: false,
                shape_buf: Vec::new(),
                has_image: false,
                new_frames: 0,
            };
            me.reopen()?;
            Ok(me)
        }
    }

    /// (Re)start duplication after it was lost.
    pub fn reopen(&mut self) -> Result<()> {
        self.dup = None;
        unsafe {
            let dup = match self.output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5.DuplicateOutput1(&self.gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]).or_else(|_| self.output.DuplicateOutput(&self.gpu.device)),
                Err(_) => self.output.DuplicateOutput(&self.gpu.device),
            }
            .context("starting screen capture")?;
            let d = dup.GetDesc();
            let size = (d.ModeDesc.Width, d.ModeDesc.Height);
            if size != self.size {
                bail!("monitor size changed to {}x{}", size.0, size.1);
            }
            self.dup = Some(dup);
        }
        Ok(())
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    /// Wait up to `timeout_ms` for screen/cursor changes and take them in.
    /// Returns true if something changed.
    pub fn poll(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let Some(dup) = self.dup.clone() else { return Err(CaptureError::Lost) };
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    self.dup = None;
                    return Err(CaptureError::Lost);
                }
                Err(e) => return Err(CaptureError::Other(anyhow::anyhow!("screen capture: {e}"))),
            }
            let mut changed = false;
            if info.LastPresentTime != 0 {
                if let Some(tex) = res.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                    self.gpu.ctx.CopyResource(&self.desktop, &tex);
                    self.has_image = true;
                    self.new_frames += 1;
                    changed = true;
                }
            }
            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                let vis = p.Visible.as_bool();
                if vis != self.cursor_visible || (vis && (p.Position.x, p.Position.y) != self.cursor_pos) {
                    changed = true;
                }
                self.cursor_visible = vis;
                if vis {
                    self.cursor_pos = (p.Position.x, p.Position.y);
                }
                if info.PointerShapeBufferSize > 0 {
                    self.shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
                    let mut need = 0u32;
                    let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                    if dup.GetFramePointerShape(self.shape_buf.len() as u32, self.shape_buf.as_mut_ptr() as *mut _, &mut need, &mut si).is_ok() {
                        if let (Some(img), Some(p)) = (convert_cursor(si.Type, si.Width, si.Height, si.Pitch, &self.shape_buf), self.painter.as_mut()) {
                            if p.set_image(&self.gpu, &img).is_ok() {
                                self.cursor_shape = true;
                                changed = true;
                            }
                        }
                    }
                }
            }
            let _ = dup.ReleaseFrame();
            Ok(changed)
        }
    }

    /// The current image (desktop + cursor if wanted).
    pub fn frame(&mut self, draw_cursor: bool) -> &ID3D11Texture2D {
        if !(draw_cursor && self.cursor_visible && self.cursor_shape) {
            return &self.desktop;
        }
        let Some(p) = &self.painter else { return &self.desktop };
        unsafe {
            self.gpu.ctx.CopyResource(&self.compose, &self.desktop);
        }
        p.draw(&self.gpu, &self.compose_rtv, self.size.0, self.size.1, self.cursor_pos.0, self.cursor_pos.1);
        &self.compose
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_cursor_masks() {
        // 8x2 cursor: row 0 = black/white/transparent/invert pairs.
        // AND rows then XOR rows, 1 byte pitch.
        let and = [0b0011_0011u8, 0xFF];
        let xor = [0b0101_0101u8, 0x00];
        let data = [and[0], and[1], xor[0], xor[1]];
        let img = convert_cursor(1, 8, 4, 1, &data).unwrap();
        assert_eq!((img.w, img.h), (8, 2));
        let px = |x: usize| &img.color[x * 4..x * 4 + 4];
        assert_eq!(px(0), &[0, 0, 0, 255]); // and 0 xor 0
        assert_eq!(px(1), &[255, 255, 255, 255]); // and 0 xor 1
        assert_eq!(px(2), &[0, 0, 0, 0]); // and 1 xor 0: transparent
        let inv = img.invert.unwrap();
        assert_eq!(&inv[3 * 4..3 * 4 + 4], &[255, 255, 255, 255]); // and 1 xor 1: invert
        // Second row is fully transparent.
        assert!(img.color[8 * 4..].iter().all(|b| *b == 0));
    }

    #[test]
    fn masked_and_colour_cursors() {
        let data = [10u8, 20, 30, 0, 0, 0, 0, 255, 9, 9, 9, 255, 1, 2, 3, 4];
        let m = convert_cursor(4, 4, 1, 16, &data).unwrap();
        assert_eq!(&m.color[0..4], &[10, 20, 30, 255]);
        assert_eq!(&m.color[4..8], &[0, 0, 0, 0]);
        assert_eq!(&m.invert.as_ref().unwrap()[8..12], &[255, 255, 255, 255]);
        let c = convert_cursor(2, 2, 2, 8, &data).unwrap();
        assert_eq!(c.color.len(), 16);
        assert_eq!(c.invert, None);
        assert!(convert_cursor(9, 1, 1, 4, &data).is_none());
    }
}
