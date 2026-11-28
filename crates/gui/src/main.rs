//! ClipVault desktop app.

#![windows_subsystem = "windows"]

mod app;
mod lanes;
mod native_player;
mod player;
mod service;
mod theme;
mod thumbs;
mod views;

use cv_core::{config::Config, paths};
use eframe::egui;
use windows::Win32::Foundation::HWND;

fn main() -> eframe::Result<()> {
    let cfg = match startup_config() {
        Ok(config) => config,
        Err(error) => {
            let message = format!("ClipVault could not open its profile at {}.\n\n{error:#}", paths::data_dir().display());
            report_startup_error(&message);
            return Err(eframe::Error::AppCreation(error.into()));
        }
    };
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
        persistence_path: paths::uses_local_storage().then(|| paths::data_dir().join("gui.ron")),
        ..Default::default()
    };
    eframe::run_native("ClipVault", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, cfg)))))
}

fn startup_config() -> anyhow::Result<Config> {
    paths::ensure_dirs()?;
    if paths::uses_local_storage() {
        Config::load()
    } else {
        Ok(Config::load_or_default())
    }
}

fn report_startup_error(message: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "ClipVault".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(HWND::default(), PCWSTR(message.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

/// Holds the single-instance mutex for the life of the process.
#[allow(dead_code)]
struct InstanceGuard(windows::Win32::Foundation::HANDLE);

fn single_instance() -> Option<InstanceGuard> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = paths::instance_name("Gui").encode_utf16().chain(Some(0)).collect();
    unsafe {
        let h = CreateMutexW(None, true, PCWSTR(name.as_ptr())).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(InstanceGuard(h))
    }
}

fn window_property() -> Vec<u16> {
    paths::instance_name("Gui").encode_utf16().chain(Some(0)).collect()
}

enum WindowMatch<'a> {
    Process(u32),
    Profile(&'a [u16]),
}

fn is_clipvault_caption(title: &[u16], reported: i32) -> bool {
    usize::try_from(reported).ok()
        .and_then(|len| title.get(..len))
        .is_some_and(|text| text.iter().copied().eq("ClipVault".encode_utf16()))
}

fn find_window(matcher: WindowMatch<'_>) -> Option<HWND> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetPropW, GetWindowTextW, GetWindowThreadProcessId};

    struct Search<'a> {
        matcher: WindowMatch<'a>,
        result: Option<HWND>,
    }

    unsafe extern "system" fn inspect(window: HWND, arg: LPARAM) -> BOOL {
        let search = &mut *(arg.0 as *mut Search<'_>);
        let matched = match search.matcher {
            WindowMatch::Process(current) => {
                let mut process = 0;
                GetWindowThreadProcessId(window, Some(&mut process));
                if process != current {
                    return BOOL(1);
                }
                let mut title = [0; 64];
                let reported = GetWindowTextW(window, &mut title);
                is_clipvault_caption(&title, reported)
            }
            WindowMatch::Profile(name) => !GetPropW(window, PCWSTR(name.as_ptr())).is_invalid(),
        };
        if matched {
            search.result = Some(window);
        }
        BOOL::from(!matched)
    }

    let mut search = Search { matcher, result: None };
    unsafe {
        let _ = EnumWindows(Some(inspect), LPARAM(&mut search as *mut _ as isize));
    }
    search.result
}

fn focus_existing() {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};

    if let Some(window) = find_window(WindowMatch::Profile(&window_property())) {
        unsafe {
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            let _ = SetForegroundWindow(window);
        }
    }
}

/// Dark title bar for the main window (Windows 10 20H1+/11).
pub fn dark_title_bar() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
    use windows::Win32::UI::WindowsAndMessaging::SetPropW;

    if let Some(window) = find_window(WindowMatch::Process(std::process::id())) {
        unsafe {
            let name = window_property();
            let _ = SetPropW(window, PCWSTR(name.as_ptr()), HANDLE(1_usize as *mut _));
            let on: i32 = 1;
            for attr in [20, 19] {
                if DwmSetWindowAttribute(window, DWMWINDOWATTRIBUTE(attr), &on as *const i32 as *const _, 4).is_ok() {
                    break;
                }
            }
            let color: u32 = 0x0013_1111;
            let _ = DwmSetWindowAttribute(window, DWMWINDOWATTRIBUTE(35), &color as *const u32 as *const _, 4);
        }
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn window_caption_checks_reported_length_without_panicking() {
        let mut title = [0; 16];
        let exact: Vec<u16> = "ClipVault".encode_utf16().collect();
        title[..exact.len()].copy_from_slice(&exact);
        assert!(is_clipvault_caption(&title, exact.len() as i32));
        // A custom window may claim a length larger than the supplied buffer.
        assert!(!is_clipvault_caption(&title, 22));
        assert!(!is_clipvault_caption(&title, -1));
        assert!(!is_clipvault_caption(&title, 0));
        let longer: Vec<u16> = "ClipVault – a different window".encode_utf16().collect();
        assert!(!is_clipvault_caption(&longer, longer.len() as i32));
    }
}
