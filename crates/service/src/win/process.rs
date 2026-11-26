//! Process utilities: finding/launching/closing OBS, single-instance mutex.

use super::{from_wide, WStr};
use std::path::{Path, PathBuf};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

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

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub exe_name: String,
}

pub fn list_processes() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                out.push(ProcInfo { pid: e.th32ProcessID, exe_name: from_wide(&e.szExeFile) });
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
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

/// PIDs of processes whose exe path equals `exe` (case-insensitive), or whose
/// exe name matches when the full path can't be read.
pub fn find_by_exe(exe: &Path) -> Vec<u32> {
    let want_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let want_path = exe.to_string_lossy().to_lowercase();
    list_processes()
        .into_iter()
        .filter(|p| p.exe_name.to_lowercase() == want_name)
        .filter(|p| match exe_path_of(p.pid) {
            Some(path) => path.to_lowercase() == want_path,
            None => true,
        })
        .map(|p| p.pid)
        .collect()
}

pub fn is_running(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code).is_ok();
        let _ = CloseHandle(h);
        ok && code == 259 // STILL_ACTIVE
    }
}

pub fn terminate(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

struct EnumCtx {
    pid: u32,
    found: Vec<(HWND, String, String)>,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let ctx = &mut *(lp.0 as *mut EnumCtx);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == ctx.pid {
        let mut t = [0u16; 256];
        let mut c = [0u16; 256];
        let tl = GetWindowTextW(hwnd, &mut t);
        let cl = GetClassNameW(hwnd, &mut c);
        ctx.found.push((
            hwnd,
            String::from_utf16_lossy(&t[..tl.max(0) as usize]),
            String::from_utf16_lossy(&c[..cl.max(0) as usize]),
        ));
    }
    BOOL(1)
}

/// Top-level windows of a process: (hwnd, title, class).
pub fn windows_of(pid: u32) -> Vec<(HWND, String, String)> {
    let mut ctx = EnumCtx { pid, found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

/// Ask OBS to close by sending WM_CLOSE to its main window.
pub fn request_close_obs(pid: u32) -> bool {
    let wins = windows_of(pid);
    let main = wins
        .iter()
        .find(|(_, title, class)| title.starts_with("OBS ") && class.starts_with("Qt"))
        .or_else(|| wins.iter().find(|(_, title, _)| title.starts_with("OBS")));
    match main {
        Some((hwnd, _, _)) => unsafe { PostMessageW(*hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() },
        None => false,
    }
}

/// Launch OBS (optionally minimised), auto-starting recording / replay buffer.
pub fn launch_obs(exe: &Path, minimize: bool, start_recording: bool, start_replay: bool) -> anyhow::Result<u32> {
    let dir: PathBuf = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut cmd = std::process::Command::new(exe);
    cmd.current_dir(&dir).args(["--disable-shutdown-check", "--disable-updater"]);
    if minimize {
        cmd.arg("--minimize-to-tray");
    }
    if start_recording {
        cmd.arg("--startrecording");
    }
    if start_replay {
        cmd.arg("--startreplaybuffer");
    }
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("could not start OBS ({}): {e}", exe.display()))?;
    Ok(child.id())
}
