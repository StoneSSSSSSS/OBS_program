//! The service's (invisible) UI thread: a hidden window that owns the global
//! hotkeys, raw mouse input (for side-button hotkeys), the tray icon, the
//! clip overlay (your overlay_img.png) and small text toasts.

use super::{shell, wide};
use crossbeam_channel::{Receiver, Sender};
use cv_core::config::Config;
use cv_core::hotkey::{Hotkey, MouseButton, Trigger};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW,
    FillRect, GetDC, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_WORDBREAK, FW_NORMAL,
    FW_SEMIBOLD, HBITMAP, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, TRANSPARENT, FF_DONTCARE, DEFAULT_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RIDEV_REMOVE, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONDATAW_0, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_CMD: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;

const TIMER_OVERLAY: usize = 1;
const TIMER_TOAST: usize = 2;
/// Global hotkeys suspended for hotkey capture come back after this long,
/// even if the GUI never says so (closed mid-capture, crashed...).
const TIMER_HOTKEY_RESUME: usize = 3;
const HOTKEY_SUSPEND_MS: u32 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SaveClip,
    SaveShortClip,
    AddMarker,
    ToggleRecording,
    OpenGui,
    RestartObs,
    OpenClipsFolder,
    OpenRecordingsFolder,
    QuitService,
    SessionEnding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Recording,
    Idle,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub enum UiCmd {
    ShowOverlay,
    Toast { level: ToastLevel, text: String },
    Balloon { level: ToastLevel, title: String, text: String },
    Tray { state: TrayState, tooltip: String, recording_paused: bool },
    ApplyConfig,
    CopyFile(PathBuf),
    SuspendHotkeys(bool),
    Quit,
}

/// Cheap handle other threads use to talk to the UI thread.
#[derive(Clone)]
pub struct UiHandle {
    tx: Sender<UiCmd>,
    hwnd: Arc<AtomicIsize>,
}

impl UiHandle {
    pub fn send(&self, cmd: UiCmd) {
        let _ = self.tx.send(cmd);
        let h = self.hwnd.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut _), WM_APP_CMD, WPARAM(0), LPARAM(0));
            }
        }
    }
    pub fn toast(&self, level: ToastLevel, text: impl Into<String>) {
        self.send(UiCmd::Toast { level, text: text.into() });
    }
}

pub fn channel() -> (UiHandle, Receiver<UiCmd>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (UiHandle { tx, hwnd: Arc::new(AtomicIsize::new(0)) }, rx)
}

struct Layered {
    hwnd: HWND,
}

struct UiState {
    cfg: Arc<parking_lot::RwLock<Config>>,
    cmd_rx: Receiver<UiCmd>,
    actions: Sender<Action>,
    main: HWND,
    overlay: Layered,
    toast: Layered,
    toast_queue: std::collections::VecDeque<(ToastLevel, String)>,
    toast_visible: bool,
    tray_added: bool,
    tray_state: TrayState,
    tray_tip: String,
    recording_paused: bool,
    hotkeys: Vec<(i32, Action, Hotkey)>,
    mouse_hotkeys: Vec<(Action, Hotkey)>,
    raw_mouse: bool,
    taskbar_created: u32,
    overlay_cache: Option<(String, f32, Vec<u8>, u32, u32)>,
    icons: [HICON; 3],
    pub hotkey_errors: Arc<Mutex<Vec<String>>>,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut UiState) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

/// Run the UI thread's message loop. Returns when `UiCmd::Quit` is processed.
pub fn run(
    handle: UiHandle,
    cmd_rx: Receiver<UiCmd>,
    actions: Sender<Action>,
    cfg: Arc<parking_lot::RwLock<Config>>,
    hotkey_errors: Arc<Mutex<Vec<String>>>,
) -> anyhow::Result<()> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = wide("ClipVaultServiceWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let layered_class = wide("ClipVaultOverlay");
        let wc2 = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW_wrapper),
            hInstance: hinst,
            lpszClassName: PCWSTR(layered_class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc2);

        let title = wide("ClipVault Service");
        let main = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        )?;
        let mk_layered = || -> anyhow::Result<HWND> {
            Ok(CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(layered_class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                hinst,
                None,
            )?)
        };
        let overlay = Layered { hwnd: mk_layered()? };
        let toast = Layered { hwnd: mk_layered()? };
        let tb = wide("TaskbarCreated");
        let taskbar_created = RegisterWindowMessageW(PCWSTR(tb.as_ptr()));

        let icons = [
            icon_from_png(include_bytes!("../../../../assets/tray_rec.png")),
            icon_from_png(include_bytes!("../../../../assets/tray_idle.png")),
            icon_from_png(include_bytes!("../../../../assets/tray_warn.png")),
        ];

        STATE.with(|s| {
            *s.borrow_mut() = Some(UiState {
                cfg,
                cmd_rx,
                actions,
                main,
                overlay,
                toast,
                toast_queue: Default::default(),
                toast_visible: false,
                tray_added: false,
                tray_state: TrayState::Idle,
                tray_tip: "ClipVault".into(),
                recording_paused: false,
                hotkeys: Vec::new(),
                mouse_hotkeys: Vec::new(),
                raw_mouse: false,
                taskbar_created,
                overlay_cache: None,
                icons,
                hotkey_errors,
            })
        });
        handle.hwnd.store(main.0 as isize, Ordering::SeqCst);
        with_state(|st| {
            st.apply_config();
        });
        // Drain anything queued before the window existed.
        let _ = PostMessageW(main, WM_APP_CMD, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        with_state(|st| st.cleanup());
        handle.hwnd.store(0, Ordering::SeqCst);
    }
    Ok(())
}

#[allow(non_snake_case)]
unsafe extern "system" fn DefWindowProcW_wrapper(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCHITTEST {
        return LRESULT(-1); // HTTRANSPARENT: never take the mouse
    }
    DefWindowProcW(h, m, w, l)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = STATE.with(|s| {
        // Re-entrancy guard: some calls (TrackPopupMenu) pump messages.
        let Ok(mut guard) = s.try_borrow_mut() else { return None };
        let st = guard.as_mut()?;
        st.handle(hwnd, msg, wp, lp)
    });
    match handled {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

impl UiState {
    fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.main {
            return None;
        }
        match msg {
            WM_APP_CMD => {
                while let Ok(cmd) = self.cmd_rx.try_recv() {
                    if !self.command(cmd) {
                        unsafe { PostQuitMessage(0) };
                        return Some(LRESULT(0));
                    }
                }
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                let id = wp.0 as i32;
                if let Some((_, action, _)) = self.hotkeys.iter().find(|(i, _, _)| *i == id) {
                    let _ = self.actions.send(*action);
                }
                Some(LRESULT(0))
            }
            WM_INPUT => {
                self.raw_input(HRAWINPUT(lp.0 as *mut _));
                None // must call DefWindowProc for WM_INPUT cleanup
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_OVERLAY => unsafe {
                        let _ = KillTimer(self.main, TIMER_OVERLAY);
                        let _ = ShowWindow(self.overlay.hwnd, SW_HIDE);
                    },
                    TIMER_HOTKEY_RESUME => {
                        unsafe {
                            let _ = KillTimer(self.main, TIMER_HOTKEY_RESUME);
                        }
                        if self.hotkeys.is_empty() && self.mouse_hotkeys.is_empty() {
                            tracing::info!("hotkey capture timed out; restoring global hotkeys");
                            self.apply_config();
                        }
                    }
                    TIMER_TOAST => unsafe {
                        let _ = KillTimer(self.main, TIMER_TOAST);
                        let _ = ShowWindow(self.toast.hwnd, SW_HIDE);
                        self.toast_visible = false;
                        self.next_toast();
                    },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_APP_TRAY => {
                // NOTIFYICON_VERSION_4: event in LOWORD(lParam).
                let event = (lp.0 & 0xFFFF) as u32;
                match event {
                    WM_LBUTTONUP | NIN_SELECT_COMPAT | NIN_KEYSELECT_COMPAT => {
                        let _ = self.actions.send(Action::OpenGui);
                    }
                    WM_RBUTTONUP | WM_CONTEXTMENU => self.tray_menu(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_QUERYENDSESSION => Some(LRESULT(1)),
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    let _ = self.actions.send(Action::SessionEnding);
                }
                Some(LRESULT(0))
            }
            m if m == self.taskbar_created && m != 0 => {
                self.tray_added = false;
                self.sync_tray();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// Returns false when the UI should quit.
    fn command(&mut self, cmd: UiCmd) -> bool {
        match cmd {
            UiCmd::ShowOverlay => self.show_overlay(),
            UiCmd::Toast { level, text } => {
                if self.cfg.read().overlay.show_text_toasts {
                    self.toast_queue.push_back((level, text));
                    if self.toast_queue.len() > 5 {
                        self.toast_queue.pop_front();
                    }
                    if !self.toast_visible {
                        self.next_toast();
                    }
                }
            }
            UiCmd::Balloon { level, title, text } => self.balloon(level, &title, &text),
            UiCmd::Tray { state, tooltip, recording_paused } => {
                let changed = state != self.tray_state || tooltip != self.tray_tip || recording_paused != self.recording_paused;
                self.tray_state = state;
                self.tray_tip = tooltip;
                self.recording_paused = recording_paused;
                if changed {
                    self.sync_tray();
                }
            }
            UiCmd::ApplyConfig => self.apply_config(),
            UiCmd::CopyFile(p) => {
                if let Err(e) = shell::copy_file_to_clipboard(&p) {
                    tracing::warn!("clipboard: {e:#}");
                }
            }
            UiCmd::SuspendHotkeys(suspend) => {
                if suspend {
                    unsafe {
                        for (id, _, _) in self.hotkeys.drain(..) {
                            let _ = UnregisterHotKey(self.main, id);
                        }
                        let _ = SetTimer(self.main, TIMER_HOTKEY_RESUME, HOTKEY_SUSPEND_MS, None);
                    }
                    self.mouse_hotkeys.clear();
                } else {
                    unsafe {
                        let _ = KillTimer(self.main, TIMER_HOTKEY_RESUME);
                    }
                    self.apply_config();
                }
            }
            UiCmd::Quit => return false,
        }
        true
    }

    fn apply_config(&mut self) {
        let cfg = self.cfg.read().clone();
        self.overlay_cache = None;
        // Hotkeys
        unsafe {
            for (id, _, _) in self.hotkeys.drain(..) {
                let _ = UnregisterHotKey(self.main, id);
            }
        }
        self.mouse_hotkeys.clear();
        let mut errors = Vec::new();
        let wanted = [
            (Action::SaveClip, cfg.hotkeys.save_clip.clone(), "Save clip"),
            (Action::SaveShortClip, cfg.hotkeys.save_short_clip.clone(), "Save short clip"),
            (Action::AddMarker, cfg.hotkeys.add_marker.clone(), "Add marker"),
            (Action::ToggleRecording, cfg.hotkeys.toggle_recording.clone(), "Pause/resume recording"),
            (Action::OpenGui, cfg.hotkeys.open_gui.clone(), "Open ClipVault"),
        ];
        let mut next_id = 1;
        for (action, text, label) in wanted {
            match Hotkey::parse(&text) {
                Ok(None) => {}
                Ok(Some(hk)) => match hk.trigger {
                    Trigger::Key(vk) => {
                        let mods = HOT_KEY_MODIFIERS(hk.win32_modifiers() | 0x4000 /* MOD_NOREPEAT */);
                        let ok = unsafe { RegisterHotKey(self.main, next_id, mods, vk) }.is_ok();
                        if ok {
                            self.hotkeys.push((next_id, action, hk));
                            tracing::info!("hotkey {label}: {hk}");
                        } else {
                            errors.push(format!("{label} hotkey {hk} is already used by another program"));
                        }
                        next_id += 1;
                    }
                    Trigger::Mouse(_) => {
                        self.mouse_hotkeys.push((action, hk));
                        tracing::info!("mouse hotkey {label}: {hk}");
                    }
                },
                Err(e) => errors.push(format!("{label} hotkey \"{text}\": {e}")),
            }
        }
        for e in &errors {
            tracing::warn!("{e}");
        }
        *self.hotkey_errors.lock() = errors;
        // Raw mouse input only when a mouse hotkey is configured.
        let want_raw = !self.mouse_hotkeys.is_empty();
        if want_raw != self.raw_mouse {
            let dev = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: if want_raw { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
                hwndTarget: if want_raw { self.main } else { HWND::default() },
            };
            unsafe {
                if RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32).is_ok() {
                    self.raw_mouse = want_raw;
                }
            }
        }
        self.sync_tray();
    }

    fn raw_input(&mut self, h: HRAWINPUT) {
        if self.mouse_hotkeys.is_empty() {
            return;
        }
        unsafe {
            let mut raw = RAWINPUT::default();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let r = GetRawInputData(
                h,
                RID_INPUT,
                Some(&mut raw as *mut _ as *mut _),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if r == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
                return;
            }
            let flags = raw.data.mouse.Anonymous.Anonymous.usButtonFlags as u32;
            let pressed = if flags & RI_MOUSE_BUTTON_4_DOWN != 0 {
                Some(MouseButton::X1)
            } else if flags & RI_MOUSE_BUTTON_5_DOWN != 0 {
                Some(MouseButton::X2)
            } else if flags & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 {
                Some(MouseButton::Middle)
            } else {
                None
            };
            let Some(btn) = pressed else { return };
            let down = |vk: u16| (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0;
            let ctrl = down(VK_CONTROL.0);
            let shift = down(VK_SHIFT.0);
            let alt = down(VK_MENU.0);
            let win = down(VK_LWIN.0) || down(VK_RWIN.0);
            for (action, hk) in &self.mouse_hotkeys {
                if hk.trigger == Trigger::Mouse(btn) && hk.ctrl == ctrl && hk.shift == shift && hk.alt == alt && hk.win == win {
                    let _ = self.actions.send(*action);
                }
            }
        }
    }

    // ------------------------------------------------------------- tray

    fn sync_tray(&mut self) {
        let show = self.cfg.read().general.show_tray_icon;
        unsafe {
            let mut nid = self.nid();
            if !show {
                if self.tray_added {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                    self.tray_added = false;
                }
                return;
            }
            nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
            nid.uCallbackMessage = WM_APP_TRAY;
            nid.hIcon = self.icons[match self.tray_state {
                TrayState::Recording => 0,
                TrayState::Idle => 1,
                TrayState::Warning => 2,
            }];
            copy_wide(&mut nid.szTip, &self.tray_tip);
            if self.tray_added {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            } else if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                nid.Anonymous = NOTIFYICONDATAW_0 { uVersion: NOTIFYICON_VERSION_4 };
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
                self.tray_added = true;
            }
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.main,
            uID: 1,
            ..Default::default()
        }
    }

    fn balloon(&mut self, level: ToastLevel, title: &str, text: &str) {
        if !self.cfg.read().general.notifications || !self.tray_added {
            // Fall back to a toast so the message isn't lost.
            self.toast_queue.push_back((level, format!("{title}: {text}")));
            if !self.toast_visible {
                self.next_toast();
            }
            return;
        }
        let mut nid = self.nid();
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = match level {
            ToastLevel::Error => NIIF_ERROR,
            ToastLevel::Warning => NIIF_WARNING,
            _ => NIIF_INFO,
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn tray_menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let items: [(u32, &str); 9] = [
                (1, "Open ClipVault"),
                (2, "Save clip now"),
                (3, "Add marker"),
                (0, ""),
                (4, if self.recording_paused { "Resume 24/7 recording" } else { "Pause 24/7 recording" }),
                (5, "Restart OBS"),
                (0, ""),
                (6, "Open clips folder"),
                (7, "Open recordings folder"),
            ];
            for (id, text) in items {
                if id == 0 {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                } else {
                    let w = wide(text);
                    let _ = AppendMenuW(menu, MF_STRING, id as usize, PCWSTR(w.as_ptr()));
                }
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let q = wide("Quit ClipVault service");
            let _ = AppendMenuW(menu, MF_STRING, 9, PCWSTR(q.as_ptr()));
            let _ = SetMenuDefaultItem(menu, 1, 0);
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.main);
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, self.main, None);
            let _ = DestroyMenu(menu);
            let action = match cmd.0 {
                1 => Some(Action::OpenGui),
                2 => Some(Action::SaveClip),
                3 => Some(Action::AddMarker),
                4 => Some(Action::ToggleRecording),
                5 => Some(Action::RestartObs),
                6 => Some(Action::OpenClipsFolder),
                7 => Some(Action::OpenRecordingsFolder),
                9 => Some(Action::QuitService),
                _ => None,
            };
            if let Some(a) = action {
                let _ = self.actions.send(a);
            }
        }
    }

    // ---------------------------------------------------------- overlay

    fn show_overlay(&mut self) {
        let cfg = self.cfg.read().clone();
        if !cfg.overlay.enabled {
            return;
        }
        let path = if cfg.overlay.image_path.trim().is_empty() {
            cv_core::paths::find_resource("overlay_img.png").map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            cfg.overlay.image_path.trim().to_string()
        };
        let scale = cfg.overlay.scale;
        let cached = matches!(&self.overlay_cache, Some((p, s, ..)) if *p == path && *s == scale);
        if !cached {
            let img = if path.is_empty() {
                None
            } else {
                image::open(&path).ok().map(|i| i.to_rgba8())
            };
            let img = img.unwrap_or_else(|| {
                image::load_from_memory(include_bytes!("../../../../assets/overlay_img.png")).unwrap().to_rgba8()
            });
            let img = if (scale - 1.0).abs() > 0.01 {
                let w = ((img.width() as f32 * scale).round() as u32).max(1);
                let h = ((img.height() as f32 * scale).round() as u32).max(1);
                image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3)
            } else {
                img
            };
            let (w, h) = img.dimensions();
            self.overlay_cache = Some((path, scale, rgba_to_premul_bgra(img.as_raw()), w, h));
        }
        let Some((_, _, ref bgra, w, h)) = self.overlay_cache else { return };
        let work = monitor_rect(&cfg.overlay.monitor, false);
        let m = cfg.overlay.margin_px as i32;
        let (w, h) = (w as i32, h as i32);
        let (x, y) = match cfg.overlay.position.as_str() {
            "top_left" => (work.left + m, work.top + m),
            "top_center" => (work.left + (work.right - work.left - w) / 2, work.top + m),
            "bottom_left" => (work.left + m, work.bottom - h - m),
            "bottom_right" => (work.right - w - m, work.bottom - h - m),
            _ => (work.right - w - m, work.top + m),
        };
        let alpha = (cfg.overlay.opacity.clamp(0.0, 1.0) * 255.0) as u8;
        unsafe {
            update_layered(self.overlay.hwnd, bgra, w, h, x, y, alpha);
            let _ = SetWindowPos(self.overlay.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let _ = SetTimer(self.main, TIMER_OVERLAY, cfg.overlay.duration_ms, None);
        }
        if cfg.overlay.play_sound {
            shell::play_sound(&cfg.overlay.sound_path);
        }
    }

    // ----------------------------------------------------------- toasts

    fn next_toast(&mut self) {
        let Some((level, text)) = self.toast_queue.pop_front() else { return };
        let accent = match level {
            ToastLevel::Info => accent_rgb(self.cfg.read().general.accent),
            ToastLevel::Success => (70, 200, 120),
            ToastLevel::Warning => (245, 166, 35),
            ToastLevel::Error => (229, 72, 77),
        };
        let Some((bgra, w, h)) = render_toast(&text, accent) else { return };
        let work = monitor_rect("primary", true);
        let x = work.right - w - 16;
        let y = work.bottom - h - 16;
        unsafe {
            update_layered(self.toast.hwnd, &bgra, w, h, x, y, 245);
            let _ = SetWindowPos(self.toast.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let _ = SetTimer(self.main, TIMER_TOAST, 3500, None);
        }
        self.toast_visible = true;
    }

    fn cleanup(&mut self) {
        unsafe {
            if self.tray_added {
                let nid = self.nid();
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            for (id, _, _) in self.hotkeys.drain(..) {
                let _ = UnregisterHotKey(self.main, id);
            }
            let _ = DestroyWindow(self.overlay.hwnd);
            let _ = DestroyWindow(self.toast.hwnd);
        }
    }
}

const NIN_SELECT_COMPAT: u32 = WM_USER;
const NIN_KEYSELECT_COMPAT: u32 = WM_USER + 1;
const RI_MOUSE_MIDDLE_BUTTON_DOWN: u32 = 0x0010;
const RI_MOUSE_BUTTON_4_DOWN: u32 = 0x0040;
const RI_MOUSE_BUTTON_5_DOWN: u32 = 0x0100;

fn copy_wide(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

pub fn accent_rgb(a: cv_core::config::Accent) -> (u8, u8, u8) {
    use cv_core::config::Accent::*;
    match a {
        Mono => (220, 220, 225),
        Red => (229, 72, 77),
        Blue => (59, 130, 246),
        Green => (34, 197, 94),
        Purple => (168, 85, 247),
        Orange => (249, 115, 22),
    }
}

/// Work area (or full area) of the primary monitor, or the monitor holding the
/// foreground window.
fn monitor_rect(which: &str, work_area: bool) -> RECT {
    unsafe {
        let mon = if which == "active" {
            let fg = GetForegroundWindow();
            MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            if work_area {
                mi.rcWork
            } else {
                mi.rcMonitor
            }
        } else {
            let _ = MONITOR_DEFAULTTONEAREST;
            RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }
        }
    }
}

fn rgba_to_premul_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let a = px[3] as u32;
        out.push(((px[2] as u32 * a + 127) / 255) as u8);
        out.push(((px[1] as u32 * a + 127) / 255) as u8);
        out.push(((px[0] as u32 * a + 127) / 255) as u8);
        out.push(a as u8);
    }
    out
}

/// Push premultiplied BGRA pixels to a layered window.
unsafe fn update_layered(hwnd: HWND, bgra: &[u8], w: i32, h: i32, x: i32, y: i32, alpha: u8) {
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    if let Ok(dib) = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, (w * h * 4) as usize);
        let old = SelectObject(mem, dib);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        let pos = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(hwnd, screen, Some(&pos), Some(&size), mem, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        SelectObject(mem, old);
        let _ = DeleteObject(dib);
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

/// Render a dark rounded toast with an accent bar. Returns (bgra, w, h).
fn render_toast(text: &str, accent: (u8, u8, u8)) -> Option<(Vec<u8>, i32, i32)> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let face = wide("Segoe UI");
        let font: HFONT = CreateFontW(
            -15, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(face.as_ptr()),
        );
        let _ = FW_SEMIBOLD;
        let old_font = SelectObject(mem, font);
        let mut wtext: Vec<u16> = text.encode_utf16().collect();
        let max_w = 360;
        let mut r = RECT { left: 0, top: 0, right: max_w, bottom: 0 };
        DrawTextW(mem, &mut wtext, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_LEFT);
        let pad_x = 18;
        let pad_y = 12;
        let bar = 4;
        let w = (r.right - r.left) + pad_x * 2 + bar;
        let h = (r.bottom - r.top) + pad_y * 2;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib: HBITMAP = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old_bmp = SelectObject(mem, dib);
        let bg = CreateSolidBrush(COLORREF(0x0024_2220)); // RGB(32,34,36)
        let full = RECT { left: 0, top: 0, right: w, bottom: h };
        FillRect(mem, &full, bg);
        let _ = DeleteObject(bg);
        let acc = CreateSolidBrush(COLORREF((accent.2 as u32) << 16 | (accent.1 as u32) << 8 | accent.0 as u32));
        FillRect(mem, &RECT { left: 0, top: 0, right: bar, bottom: h }, acc);
        let _ = DeleteObject(acc);
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, COLORREF(0x00F0_F0F0));
        let mut tr = RECT { left: bar + pad_x, top: pad_y, right: w - pad_x, bottom: h - pad_y };
        DrawTextW(mem, &mut wtext, &mut tr, DT_WORDBREAK | DT_NOPREFIX | DT_LEFT);
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        // Rounded-corner alpha mask (anti-aliased), then premultiply.
        let radius = 10.0f32;
        for y in 0..h {
            for x in 0..w {
                let a = corner_alpha(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
                let i = ((y * w + x) * 4) as usize;
                px[i] = (px[i] as f32 * a) as u8;
                px[i + 1] = (px[i + 1] as f32 * a) as u8;
                px[i + 2] = (px[i + 2] as f32 * a) as u8;
                px[i + 3] = (255.0 * a) as u8;
            }
        }
        let out = px.to_vec();
        SelectObject(mem, old_bmp);
        SelectObject(mem, old_font);
        let _ = DeleteObject(dib);
        let _ = DeleteObject(font);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let _: Option<HDC> = None;
        Some((out, w, h))
    }
}

fn corner_alpha(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let cx = if x < r { r } else if x > w - r { w - r } else { x };
    let cy = if y < r { r } else if y > h - r { h - r } else { y };
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    (r - d + 0.5).clamp(0.0, 1.0)
}

/// Build an HICON from PNG bytes.
fn icon_from_png(png: &[u8]) -> HICON {
    let Ok(img) = image::load_from_memory(png) else { return HICON::default() };
    let img = img.to_rgba8();
    let (w, h) = img.dimensions();
    unsafe {
        let screen = GetDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            ReleaseDC(None, screen);
            return HICON::default();
        };
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for (i, px) in img.as_raw().chunks_exact(4).enumerate() {
            dst[i * 4] = px[2];
            dst[i * 4 + 1] = px[1];
            dst[i * 4 + 2] = px[0];
            dst[i * 4 + 3] = px[3];
        }
        let mask = windows::Win32::Graphics::Gdi::CreateBitmap(w as i32, h as i32, 1, 1, None);
        let info = ICONINFO { fIcon: BOOL(1), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        ReleaseDC(None, screen);
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply() {
        let out = rgba_to_premul_bgra(&[255, 128, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0]);
        assert_eq!(out, vec![0, 64, 128, 128, 30, 20, 10, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn rounded_corners() {
        assert_eq!(corner_alpha(50.0, 20.0, 100.0, 40.0, 10.0), 1.0);
        assert_eq!(corner_alpha(0.0, 0.0, 100.0, 40.0, 10.0), 0.0);
        let edge = corner_alpha(10.0, 0.5, 100.0, 40.0, 10.0);
        assert!(edge > 0.0 && edge <= 1.0);
    }

    #[test]
    fn wide_copy_truncates() {
        let mut buf = [1u16; 5];
        copy_wide(&mut buf, "abcdefgh");
        assert_eq!(&buf, &[97, 98, 99, 100, 0]);
    }
}
