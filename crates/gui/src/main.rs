//! ClipVault desktop app.

#![windows_subsystem = "windows"]

mod app;
mod mpv;
mod player;
mod service;
mod theme;
mod thumbs;
mod views;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let _ = cv_core::paths::ensure_dirs();
    let cfg = cv_core::config::Config::load_or_default();
    let _log = cv_core::logging::init("gui", &cfg.advanced.log_level, cfg.advanced.log_retention_days);

    // Single instance: focus the existing window instead.
    let _guard = match single_instance() {
        Some(g) => g,
        None => {
            focus_existing();
            return Ok(());
        }
    };

    let icon = image::load_from_memory(include_bytes!("../../../assets/icon_256.png"))
        .ok()
        .map(|i| {
            let i = i.to_rgba8();
            egui::IconData { width: i.width(), height: i.height(), rgba: i.into_raw() }
        });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ClipVault")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([980.0, 640.0])
        .with_app_id("ClipVault");
    if let Some(icon) = icon {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native("ClipVault", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, cfg)))))
}

/// Holds the single-instance mutex for the life of the process.
#[allow(dead_code)]
struct InstanceGuard(windows::Win32::Foundation::HANDLE);

fn single_instance() -> Option<InstanceGuard> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\ClipVaultGui\0".encode_utf16().collect();
    unsafe {
        let h = CreateMutexW(None, true, PCWSTR(name.as_ptr())).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(InstanceGuard(h))
    }
}

fn focus_existing() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
    let title: Vec<u16> = "ClipVault\0".encode_utf16().collect();
    unsafe {
        if let Ok(h) = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = SetForegroundWindow(h);
        }
    }
}

/// Dark title bar for the main window (Windows 10 20H1+/11).
pub fn dark_title_bar() {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    let title: Vec<u16> = "ClipVault\0".encode_utf16().collect();
    unsafe {
        if let Ok(h) = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) {
            let on: i32 = 1;
            for attr in [20, 19] {
                if DwmSetWindowAttribute(h, DWMWINDOWATTRIBUTE(attr), &on as *const i32 as *const _, 4).is_ok() {
                    break;
                }
            }
            // Win11 caption colour (ignored on Win10).
            let color: u32 = 0x0013_1111;
            let _ = DwmSetWindowAttribute(h, DWMWINDOWATTRIBUTE(35), &color as *const u32 as *const _, 4);
        }
    }
}
