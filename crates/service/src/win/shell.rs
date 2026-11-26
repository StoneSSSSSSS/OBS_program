//! Shell helpers: Recycle Bin, shortcuts, clipboard, disk space, sounds.

use super::{wide, wide_path, WStr};
use anyhow::{bail, Result};
use std::path::Path;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT, SND_SYSTEM, SND_ALIAS};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{
    IShellLinkW, SHFileOperationW, ShellExecuteW, ShellLink, DROPFILES, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Send files to the Recycle Bin (silently).
pub fn recycle(paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // Double-NUL-terminated list of NUL-terminated paths.
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        list.extend(wide_path(&abs));
    }
    list.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(list.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        bail!("Recycle Bin delete failed (code {r})");
    }
    Ok(())
}

/// Create a .lnk shortcut.
pub fn create_shortcut(link: &Path, target: &Path, args: &str, workdir: &Path, description: &str) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let t = WStr::path(target);
        let a = WStr::new(args);
        let w = WStr::path(workdir);
        let d = WStr::new(description);
        sl.SetPath(t.pcwstr())?;
        sl.SetArguments(a.pcwstr())?;
        sl.SetWorkingDirectory(w.pcwstr())?;
        sl.SetDescription(d.pcwstr())?;
        sl.SetIconLocation(t.pcwstr(), 0)?;
        let pf: IPersistFile = sl.cast()?;
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let l = WStr::path(link);
        pf.Save(l.pcwstr(), true)?;
    }
    Ok(())
}

/// Put a file on the clipboard as a file drop (paste into Discord/Explorer).
pub fn copy_file_to_clipboard(path: &Path) -> Result<()> {
    let abs = std::path::absolute(path)?;
    let mut files = wide_path(&abs);
    files.push(0); // double NUL
    let header = std::mem::size_of::<DROPFILES>();
    let bytes = header + files.len() * 2;
    unsafe {
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let ptr = GlobalLock(hmem) as *mut u8;
        if ptr.is_null() {
            bail!("GlobalLock failed");
        }
        let df = DROPFILES { pFiles: header as u32, fWide: true.into(), ..Default::default() };
        std::ptr::copy_nonoverlapping(&df as *const DROPFILES as *const u8, ptr, header);
        std::ptr::copy_nonoverlapping(files.as_ptr() as *const u8, ptr.add(header), files.len() * 2);
        let _ = GlobalUnlock(hmem);
        // Retry: another app may briefly hold the clipboard.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(HWND::default()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !opened {
            bail!("clipboard is busy");
        }
        let _ = EmptyClipboard();
        let r = SetClipboardData(CF_HDROP.0 as u32, HANDLE(hmem.0));
        let _ = CloseClipboard();
        r?;
    }
    Ok(())
}

/// (free, total) bytes on the volume containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let mut p = path.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let w = WStr::path(&p);
    let mut free = 0u64;
    let mut total = 0u64;
    unsafe { GetDiskFreeSpaceExW(w.pcwstr(), Some(&mut free), Some(&mut total), None).ok()? };
    Some((free, total))
}

pub fn open_in_explorer(path: &Path) {
    let op = wide("open");
    let p = WStr::path(path);
    unsafe {
        ShellExecuteW(HWND::default(), PCWSTR(op.as_ptr()), p.pcwstr(), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Play a .wav (or the Windows notification sound when `path` is empty).
pub fn play_sound(path: &str) {
    unsafe {
        if path.trim().is_empty() {
            let alias = wide("Notification.Default");
            let _ = PlaySoundW(PCWSTR(alias.as_ptr()), None, SND_ALIAS | SND_ASYNC | SND_SYSTEM | SND_NODEFAULT);
        } else {
            let p = wide(path.trim());
            let _ = PlaySoundW(PCWSTR(p.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
        }
    }
}
