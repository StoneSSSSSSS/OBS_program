//! Screen capture with Windows.Graphics.Capture (Windows 10 1903+).
//!
//! Used when DXGI desktop duplication isn't available, which is common on
//! laptops where the discrete GPU drives the panel directly. Windows draws
//! the cursor into the frames for us.

use super::display::CaptureError;
use crate::d3d::Gpu;
use anyhow::{Context, Result};
use std::sync::Arc;
use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

pub struct WgcCapture {
    gpu: Arc<Gpu>,
    output_index: u32,
    item: GraphicsCaptureItem,
    device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    event: Arc<Event>,
    pub size: (u32, u32),
    desktop: ID3D11Texture2D,
    cursor: bool,
    pub has_image: bool,
    pub new_frames: u64,
}

unsafe impl Send for WgcCapture {}

pub fn supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

impl WgcCapture {
    pub fn new(gpu: Arc<Gpu>, output_index: u32) -> Result<WgcCapture> {
        unsafe {
            let output = gpu.adapter.EnumOutputs(output_index).context("monitor not found")?;
            let desc = output.GetDesc()?;
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().context("screen capture is not available")?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(desc.Monitor).context("capturing this monitor")?;
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let s = item.Size()?;
            let size = (s.Width.max(2) as u32, s.Height.max(2) as u32);
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, s)?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(true);
            // Windows 11: no yellow "being captured" border.
            let _ = session.SetIsBorderRequired(false);
            let event = Arc::new(Event(CreateEventW(None, false, false, None)?));
            let ev = event.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                let _ = SetEvent(ev.0);
                Ok(())
            }))?;
            session.StartCapture()?;
            let desktop = gpu.texture(size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0))?;
            Ok(WgcCapture { gpu, output_index, item, device, pool, session, event, size, desktop, cursor: true, has_image: false, new_frames: 0 })
        }
    }

    pub fn output_index(&self) -> u32 {
        self.output_index
    }

    pub fn poll(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let mut got = false;
        // Take every queued frame (keep the newest).
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    let cs = frame.ContentSize().map_err(|e| CaptureError::Other(e.into()))?;
                    if (cs.Width as u32, cs.Height as u32) != self.size && cs.Width > 0 {
                        let _ = frame.Close();
                        return Err(CaptureError::Lost); // monitor resolution changed
                    }
                    let tex: Option<ID3D11Texture2D> = frame.Surface().ok().and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>().ok()).and_then(|a| unsafe { a.GetInterface().ok() });
                    if let Some(t) = tex {
                        unsafe { self.gpu.ctx.CopyResource(&self.desktop, &t) };
                        self.has_image = true;
                        self.new_frames += 1;
                        got = true;
                    }
                    let _ = frame.Close();
                }
                Err(_) => break,
            }
        }
        if got || timeout_ms == 0 {
            return Ok(got);
        }
        let r = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
        if r == WAIT_OBJECT_0 {
            return self.poll(0);
        }
        Ok(false)
    }

    /// Recreate the frame pool (after a lost/changed capture).
    pub fn reopen(&mut self) -> Result<()> {
        let s = self.item.Size()?;
        let size = (s.Width.max(2) as u32, s.Height.max(2) as u32);
        if size != self.size {
            anyhow::bail!("monitor size changed to {}x{}", size.0, size.1);
        }
        self.pool.Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, s)?;
        Ok(())
    }

    pub fn frame(&mut self, draw_cursor: bool) -> &ID3D11Texture2D {
        if draw_cursor != self.cursor {
            let _ = self.session.SetIsCursorCaptureEnabled(draw_cursor);
            self.cursor = draw_cursor;
        }
        &self.desktop
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        unsafe {
            let _ = CloseHandle(self.event.0);
        }
    }
}
