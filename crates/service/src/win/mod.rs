//! Thin, safe-ish wrappers over the Win32 APIs the service needs.

pub mod coreaudio;
pub mod foreground;
pub mod process;
pub mod shell;
pub mod ui;

use windows::core::PCWSTR;

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated (or full) UTF-16 buffer.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Holds a `Vec<u16>` and hands out a PCWSTR into it.
pub struct WStr(Vec<u16>);

impl WStr {
    pub fn new(s: &str) -> Self {
        Self(wide(s))
    }
    pub fn path(p: &std::path::Path) -> Self {
        Self(wide_path(p))
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}
