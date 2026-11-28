//! Which programs are playing sound, and which process trees to capture so
//! every app's audio is recorded exactly once.

use std::collections::{HashMap, HashSet};
use windows::core::Interface;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    /// Lower-case exe file name, e.g. `discord.exe`.
    pub exe: String,
}

/// Snapshot of running processes.
pub fn processes() -> HashMap<u32, ProcInfo> {
    let mut out = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase();
                out.insert(e.th32ProcessID, ProcInfo { pid: e.th32ProcessID, parent: e.th32ParentProcessID, exe });
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// When a process started (FILETIME ticks), if we may ask.
pub fn process_created(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let r = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
}

/// A process with an audio session on some output device.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundSession {
    pub pid: u32,
    /// Currently playing (vs. merely having a session open).
    pub active: bool,
}

/// Every non-system audio session on every active output device.
pub fn sound_sessions() -> Vec<SoundSession> {
    let mut by_pid: HashMap<u32, bool> = HashMap::new();
    unsafe {
        let Ok(en): windows::core::Result<IMMDeviceEnumerator> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return Vec::new() };
        let Ok(coll) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        for i in 0..coll.GetCount().unwrap_or(0) {
            let Ok(dev) = coll.Item(i) else { continue };
            let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(list) = mgr.GetSessionEnumerator() else { continue };
            for k in 0..list.GetCount().unwrap_or(0) {
                let Ok(ctl) = list.GetSession(k) else { continue };
                let Ok(state) = ctl.GetState() else { continue };
                if state == AudioSessionStateExpired {
                    continue;
                }
                let Ok(c2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                if c2.IsSystemSoundsSession().0 == 0 {
                    continue; // S_OK means it *is* the system sounds session
                }
                let Ok(pid) = c2.GetProcessId() else { continue };
                if pid == 0 {
                    continue;
                }
                let active = state == AudioSessionStateActive;
                let e = by_pid.entry(pid).or_insert(false);
                *e |= active;
            }
        }
    }
    let mut v: Vec<SoundSession> = by_pid.into_iter().map(|(pid, active)| SoundSession { pid, active }).collect();
    v.sort_by_key(|s| s.pid);
    v
}

/// A process tree to capture as one app.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub root: u32,
    pub exe: String,
}

/// Programs never captured as apps: ClipVault itself (its own playback),
/// the audio engine, and the shell (whose tree contains most other apps).
pub const NEVER_CAPTURE: &[&str] = &["clipvault.exe", "clipvault-service.exe", "audiodg.exe", "explorer.exe", "svchost.exe", "dwm.exe"];

/// Decide which process trees to capture.
///
/// Each sounding process is walked up to its outermost ancestor with the same
/// exe (Chrome/Discord/Electron apps play from child processes). A tree that
/// contains another chosen tree is dropped, because per-app capture always
/// includes the whole tree and would record that app twice (e.g. a launcher
/// with a session whose game also plays sound).
///
/// `created(pid)` gives a process's start time; a recorded parent that started
/// after its child is a recycled pid, not the real parent, and is ignored.
pub fn choose_targets(session_pids: &[u32], procs: &HashMap<u32, ProcInfo>, own: &[u32], never: &[&str], created: &dyn Fn(u32) -> Option<u64>) -> Vec<Target> {
    let exe_of = |p: u32| procs.get(&p).map(|i| i.exe.as_str()).unwrap_or("");
    let parent_of = |p: u32| {
        procs.get(&p).map(|i| i.parent).filter(|pp| *pp != p && *pp != 0 && procs.contains_key(pp)).filter(|pp| match (created(*pp), created(p)) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        })
    };
    let mut roots: HashSet<u32> = HashSet::new();
    for &pid in session_pids {
        let exe = exe_of(pid);
        if exe.is_empty() || own.contains(&pid) || never.iter().any(|n| n.eq_ignore_ascii_case(exe)) {
            continue;
        }
        let mut r = pid;
        for _ in 0..64 {
            match parent_of(r) {
                Some(pp) if exe_of(pp) == exe => r = pp,
                _ => break,
            }
        }
        roots.insert(r);
    }
    let is_ancestor = |anc: u32, mut p: u32| -> bool {
        for _ in 0..64 {
            match parent_of(p) {
                Some(pp) if pp == anc => return true,
                Some(pp) => p = pp,
                None => return false,
            }
        }
        false
    };
    let mut out: Vec<Target> = roots
        .iter()
        .filter(|&&r| !roots.iter().any(|&o| o != r && is_ancestor(r, o)))
        .map(|&r| Target { root: r, exe: exe_of(r).to_string() })
        .collect();
    out.sort_by_key(|t| t.root);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, exe: &str) -> (u32, ProcInfo) {
        (pid, ProcInfo { pid, parent, exe: exe.into() })
    }

    #[test]
    fn picks_disjoint_app_trees() {
        let procs: HashMap<u32, ProcInfo> = [
            p(4, 0, "system"),
            p(100, 4, "explorer.exe"),
            p(200, 100, "chrome.exe"),
            p(201, 200, "chrome.exe"), // audio service child
            p(300, 100, "steam.exe"),
            p(301, 300, "cs2.exe"),
            p(400, 100, "discord.exe"),
            p(401, 400, "discord.exe"),
            p(402, 401, "discord.exe"),
            p(500, 100, "clipvault.exe"),
        ]
        .into_iter()
        .collect();
        // Sessions: chrome child, steam itself AND its game, discord grandchild,
        // explorer, and ClipVault's own player.
        let none = |_: u32| None;
        let t = choose_targets(&[201, 300, 301, 402, 100, 500], &procs, &[], NEVER_CAPTURE, &none);
        let exes: Vec<&str> = t.iter().map(|t| t.exe.as_str()).collect();
        assert_eq!(exes, vec!["chrome.exe", "cs2.exe", "discord.exe"]);
        assert_eq!(t[0].root, 200);
        assert_eq!(t[2].root, 400);
        // Without the game, Steam is captured.
        let t = choose_targets(&[300], &procs, &[], NEVER_CAPTURE, &none);
        assert_eq!(t, vec![Target { root: 300, exe: "steam.exe".into() }]);
        // Own pids are skipped.
        assert!(choose_targets(&[201], &procs, &[201], NEVER_CAPTURE, &none).is_empty());
        // A "parent" that started after the child is a reused pid: ignored,
        // so discord's tree isn't swallowed by an unrelated process.
        let late = |pid: u32| Some(if pid == 100 { 50 } else { 10 });
        let t = choose_targets(&[402], &procs, &[], NEVER_CAPTURE, &late);
        assert_eq!(t[0].root, 400);
    }

    #[test]
    fn live_session_list_works() {
        crate::mf::init();
        let s = sound_sessions();
        let procs = processes();
        let pids: Vec<u32> = s.iter().map(|x| x.pid).collect();
        let t = choose_targets(&pids, &procs, &[std::process::id()], NEVER_CAPTURE, &process_created);
        eprintln!("sessions: {:?}", s.iter().map(|x| (procs.get(&x.pid).map(|p| p.exe.clone()), x.active)).collect::<Vec<_>>());
        eprintln!("targets: {t:?}");
    }
}
