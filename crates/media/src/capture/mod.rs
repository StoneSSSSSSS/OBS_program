//! Capturing the screen and sound.

pub mod audio;
pub mod display;
pub mod hub;
pub mod sessions;
pub mod wgc;

use crate::d3d::Gpu;
use anyhow::Result;
use display::{CaptureError, DxgiCapture};
use std::sync::Arc;
use wgc::WgcCapture;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMethod {
    /// Desktop duplication if it works, otherwise Windows Graphics Capture.
    Auto,
    Duplication,
    GraphicsCapture,
}

/// A monitor being captured, by whichever method works on this PC.
pub enum DisplayCapture {
    Dxgi(DxgiCapture),
    Wgc(WgcCapture),
}

impl DisplayCapture {
    pub fn open(gpu: Arc<Gpu>, output_index: u32, method: CaptureMethod) -> Result<DisplayCapture> {
        if method != CaptureMethod::GraphicsCapture {
            match DxgiCapture::new(gpu.clone(), output_index) {
                Ok(c) => return Ok(DisplayCapture::Dxgi(c)),
                Err(e) if method == CaptureMethod::Duplication => return Err(e),
                Err(e) => tracing::info!("desktop duplication unavailable ({e:#}); using Windows Graphics Capture"),
            }
        }
        Ok(DisplayCapture::Wgc(WgcCapture::new(gpu, output_index)?))
    }

    pub fn method_name(&self) -> &'static str {
        match self {
            DisplayCapture::Dxgi(_) => "desktop duplication",
            DisplayCapture::Wgc(_) => "Windows Graphics Capture",
        }
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            DisplayCapture::Dxgi(c) => c.size,
            DisplayCapture::Wgc(c) => c.size,
        }
    }

    pub fn has_image(&self) -> bool {
        match self {
            DisplayCapture::Dxgi(c) => c.has_image,
            DisplayCapture::Wgc(c) => c.has_image,
        }
    }

    pub fn new_frames(&self) -> u64 {
        match self {
            DisplayCapture::Dxgi(c) => c.new_frames,
            DisplayCapture::Wgc(c) => c.new_frames,
        }
    }

    pub fn poll(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        match self {
            DisplayCapture::Dxgi(c) => c.poll(timeout_ms),
            DisplayCapture::Wgc(c) => c.poll(timeout_ms),
        }
    }

    pub fn reopen(&mut self) -> Result<()> {
        match self {
            DisplayCapture::Dxgi(c) => c.reopen(),
            DisplayCapture::Wgc(c) => c.reopen(),
        }
    }

    pub fn frame(&mut self, cursor: bool) -> &ID3D11Texture2D {
        match self {
            DisplayCapture::Dxgi(c) => c.frame(cursor),
            DisplayCapture::Wgc(c) => c.frame(cursor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_the_screen_by_any_method() {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        crate::mf::init();
        let outs = crate::d3d::list_outputs();
        let o = outs.iter().find(|o| o.primary).unwrap_or(&outs[0]).clone();
        let gpu = Arc::new(Gpu::on_adapter(o.adapter_index).unwrap());
        let mut cap = DisplayCapture::open(gpu.clone(), o.output_index, CaptureMethod::Auto).unwrap();
        eprintln!("capturing {} at {:?} via {}", o.device_name, cap.size(), cap.method_name());
        let start = std::time::Instant::now();
        while !cap.has_image() && start.elapsed() < std::time::Duration::from_secs(3) {
            match cap.poll(100) {
                Ok(_) => {}
                Err(CaptureError::Lost) => cap.reopen().unwrap(),
                Err(e) => panic!("{e}"),
            }
        }
        assert!(cap.has_image(), "no desktop image within 3 s");
        let (w, h) = crate::d3d::fit_size(cap.size(), 360);
        let mut conv = crate::d3d::Converter::new(&gpu, cap.size(), (w, h), true).unwrap();
        let dst = gpu.nv12_target(w, h).unwrap();
        let src = cap.frame(true).clone();
        conv.blit(&src, &dst).unwrap();
        let nv = gpu.read_nv12(&dst, w, h).unwrap();
        let mut seen = [false; 256];
        for v in &nv[..(w * h) as usize] {
            seen[*v as usize] = true;
        }
        let n = seen.iter().filter(|b| **b).count();
        eprintln!("{n} distinct luma values in a {w}x{h} capture");
        assert!(n > 4, "screen capture looks blank");
    }

    /// Plays a short quiet tone from a child PowerShell process and records
    /// that process's audio on its own (per-app capture) plus the default mic.
    #[test]
    fn app_audio_capture() {
        crate::mf::init();
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("tone.wav");
        // 1.5 s, 440 Hz, quiet.
        let n = 48_000 * 3 / 2;
        let mut bytes = Vec::new();
        let data_len = n * 4;
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..n {
            let v = (0.08 * (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 32767.0) as i16;
            bytes.extend_from_slice(&v.to_le_bytes());
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&wav, bytes).unwrap();
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Start-Sleep -Milliseconds 700; (New-Object Media.SoundPlayer '{}').PlaySync(); Start-Sleep -Milliseconds 300", wav.display())])
            .spawn()
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let app = audio::AudioSource::start(1, audio::SourceKind::App(child.id()), tx.clone());
        let _mic = audio::AudioSource::start(2, audio::SourceKind::Mic(None), tx.clone());
        let epoch = crate::clock::now_hns();
        let mut hub = hub::Hub::new(epoch, 200);
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(!app.failed(), "per-app capture failed to start");
        while let Ok(c) = rx.try_recv() {
            hub.push(c);
        }
        let blocks = hub.pull(crate::clock::now_hns());
        let peak = |id: u32| blocks.iter().flat_map(|b| b.sources.iter().filter(|(s, _)| *s == id).flat_map(|(_, v)| v.iter())).fold(0.0f32, |m, v| m.max(v.abs()));
        let loud_blocks = blocks.iter().filter(|b| b.sources.iter().any(|(s, v)| *s == 1 && v.iter().any(|x| x.abs() > 0.03))).count();
        eprintln!("{} blocks; app peak {:.3} over {} blocks; mic peak {:.3}", blocks.len(), peak(1), loud_blocks, peak(2));
        assert!(peak(1) > 0.04, "the app's tone was not captured");
        // ~1.5 s of tone = ~150 blocks.
        assert!((120..=170).contains(&loud_blocks), "tone length off: {loud_blocks} blocks");
    }
}
