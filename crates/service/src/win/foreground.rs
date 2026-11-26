//! What window is in the foreground, and is it full-screen?

use super::{process::exe_path_of, wide};
use cv_core::games::{exe_name_of, WindowInfo};
use std::collections::HashMap;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible,
};

/// Caches exe paths per pid (pids get reused, so entries expire).
#[derive(Default)]
pub struct ExeCache {
    map: HashMap<u32, (String, std::time::Instant)>,
}

impl ExeCache {
    fn get(&mut self, pid: u32) -> Option<String> {
        if let Some((p, t)) = self.map.get(&pid) {
            if t.elapsed().as_secs() < 30 {
                return Some(p.clone());
            }
        }
        let p = exe_path_of(pid)?;
        self.map.insert(pid, (p.clone(), std::time::Instant::now()));
        if self.map.len() > 512 {
            self.map.retain(|_, (_, t)| t.elapsed().as_secs() < 30);
        }
        Some(p)
    }
}

pub fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Does the window cover its whole monitor?
pub fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

/// Snapshot of the foreground window. None for the desktop/shell or when
/// nothing is focused.
pub fn foreground(cache: &mut ExeCache) -> Option<(WindowInfo, HWND)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let class = window_class(hwnd);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return None;
        }
        let exe_path = cache.get(pid).unwrap_or_default();
        let exe_name = exe_name_of(&exe_path);
        Some((
            WindowInfo {
                exe_path,
                exe_name,
                title: window_title(hwnd),
                class,
                pid,
                fullscreen: is_fullscreen(hwnd),
            },
            hwnd,
        ))
    }
}

/// ProductName / FileDescription from an exe's version resource.
pub fn version_strings(exe_path: &str) -> (Option<String>, Option<String>) {
    if exe_path.is_empty() {
        return (None, None);
    }
    let path = wide(exe_path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 {
            return (None, None);
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(PCWSTR(path.as_ptr()), 0, size, data.as_mut_ptr() as *mut _).is_err() {
            return (None, None);
        }
        // Language/codepage pairs.
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let q = wide("\\VarFileInfo\\Translation");
        let mut langs: Vec<(u16, u16)> = Vec::new();
        if VerQueryValueW(data.as_ptr() as *const _, PCWSTR(q.as_ptr()), &mut ptr, &mut len).as_bool() && !ptr.is_null() {
            let n = len as usize / 4;
            let words = std::slice::from_raw_parts(ptr as *const u16, n * 2);
            for i in 0..n {
                langs.push((words[i * 2], words[i * 2 + 1]));
            }
        }
        langs.push((0x0409, 0x04B0));
        langs.push((0x0409, 0x04E4));
        let read = |key: &str| -> Option<String> {
            for (lang, cp) in &langs {
                let q = wide(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\{key}"));
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut l = 0u32;
                if VerQueryValueW(data.as_ptr() as *const _, PCWSTR(q.as_ptr()), &mut p, &mut l).as_bool()
                    && !p.is_null()
                    && l > 0
                {
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p as *const u16, l as usize));
                    let s = s.trim_end_matches('\0').trim().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            None
        };
        (read("ProductName"), read("FileDescription"))
    }
}
