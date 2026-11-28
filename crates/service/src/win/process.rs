//! Process utilities: exe paths and the single-instance mutex.

use super::WStr;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

/// Keeps a named mutex alive for the lifetime of the process.
pub struct SingleInstance(HANDLE);

unsafe impl Send for SingleInstance {}

impl SingleInstance {
    /// Returns None if another instance already holds the mutex.
    pub fn acquire(name: &str) -> Option<SingleInstance> {
        let w = WStr::new(name);
        unsafe {
            let h = CreateMutexW(None, true, w.pcwstr()).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return None;
            }
            Some(SingleInstance(h))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Full exe path of a running process (None if it exited or access is denied).
pub fn exe_path_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}
