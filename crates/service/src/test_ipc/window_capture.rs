//! Opt-in genuine recorder/IPC smoke test. Only this process's synthetic window is captured.

#[path = "retention_soak.rs"]
mod retention_soak;

use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static FRAME: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn paint_window(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        WM_TIMER => {
            FRAME.fetch_add(1, Ordering::Relaxed);
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut area = RECT::default();
            let _ = GetClientRect(hwnd, &mut area);
            let f = FRAME.load(Ordering::Relaxed) as i32;
            let width = (area.right - area.left).max(2);
            let height = (area.bottom - area.top).max(2);
            let x = (f * 5).rem_euclid((width - 86).max(1));
            let y = (f * 3).rem_euclid((height - 70).max(1));
            for (color, rect) in [
                (0x60351a, area),
                (0xd76e44, RECT { left: 0, right: width, top: height / 3, bottom: height * 2 / 3 }),
                (0x19e3ee, RECT { left: x, right: x + 86, top: 0, bottom: height }),
                (0xb02d93, RECT { left: 0, right: width, top: y, bottom: y + 70 }),
            ] {
                let brush = CreateSolidBrush(COLORREF(color));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush);
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, w, l),
    }
}

struct SyntheticWindow {
    hwnd: isize,
    worker: Option<thread::JoinHandle<()>>,
}

impl SyntheticWindow {
    fn open() -> Result<Self> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let worker = spawn("synthetic-qa-window", move || unsafe {
            let create = || -> Result<HWND> {
                let instance: HINSTANCE = GetModuleHandleW(None)?.into();
                let class = w!("ClipVaultOwnedSyntheticQaWindow");
                ensure!(RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(paint_window),
                    hInstance: instance,
                    lpszClassName: class,
                    ..Default::default()
                }) != 0, "register synthetic window");
                let hwnd = CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class,
                    w!("ClipVault synthetic moving window — no desktop or sound capture"),
                    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, 25, 65, 660, 410,
                    None, None, instance, None,
                )?;
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                ensure!(SetTimer(hwnd, 1, 30, None) != 0, "animate synthetic window");
                Ok(hwnd)
            };
            match create() {
                Ok(hwnd) => {
                    if tx.send(Ok(hwnd.0 as isize)).is_err() {
                        let _ = DestroyWindow(hwnd);
                        return;
                    }
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                Err(error) => { let _ = tx.send(Err(error)); }
            }
        });
        Ok(Self { hwnd: rx.recv_timeout(REQUEST_TIMEOUT)??, worker: Some(worker) })
    }
}

impl Drop for SyntheticWindow {
    fn drop(&mut self) {
        let hwnd = HWND(self.hwnd as *mut std::ffi::c_void);
        let _ = unsafe { PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
#[ignore = "Shows and captures only a synthetic owned window in a new .clipvault-local profile"]
fn serve_owned_window_capture() -> Result<()> {
    ensure!(
        std::env::var("CLIPVAULT_QA_CAPTURE_WINDOW").as_deref() == Ok("1"),
        "Set CLIPVAULT_QA_CAPTURE_WINDOW=1 in the isolated GUI and test service"
    );
    let root = std::env::current_dir()?.join(".clipvault-local").canonicalize()?;
    let profile_dir = cv_core::paths::data_dir();
    ensure!(profile_dir.parent().context("QA profile parent")?.canonicalize()?.starts_with(&root),
        "QA profile must be under the Stone project's .clipvault-local");
    let fixture = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("synthetic archive fixture required")?);
    ensure!(fixture.canonicalize()?.starts_with(&root),
        "QA seed must be a local synthetic fixture under .clipvault-local");

    // Drop the profile (including its recorder thread) before destroying the window.
    let mut window = None;
    let mut profile = TestProfile::new()?;
    profile.cut_sample()?;
    println!("Ready for genuine window-capture UI QA in {}", profile.dir.display());
    let began = Instant::now();
    while !profile.dir.join("qa-arm-live").exists() {
        ensure!(began.elapsed() < Duration::from_secs(150), "GUI did not arm the live recorder");
        thread::sleep(Duration::from_millis(100));
    }

    let qa = SyntheticWindow::open()?;
    let hwnd = qa.hwnd;
    window.replace(qa);
    let mut config = profile.shared.cfg();
    config.recording.enabled = true;
    config.recording.history_mode = HistoryMode::Archive;
    config.recording.max_height = 360;
    config.recording.fps = 20;
    config.recording.keyframe_interval_secs = 1;
    config.recording.encoder = "software".into();
    config.recording.app_tracks = false;
    config.recording.show_cursor = false;
    config.audio.record_mic = false;
    config.audio.mic_guard = false;
    *profile.shared.cfg.write() = config;

    let rx = profile.engine_rx.take().context("test window engine channel")?;
    let shared = profile.shared.clone();
    profile.engine = Some(spawn("window-capture-engine", move || engine::run_test_window(shared, rx, hwnd)));
    let mut status_log = Vec::new();
    let mut segments = BTreeMap::<String, (i64, i64)>::new();
    let run = Instant::now();
    let mut latest = Status::default();
    while !profile.dir.join("qa-live-window-done").exists() {
        let s: Status = serde_json::from_value(profile.client.call(Request::Status, REQUEST_TIMEOUT)?)?;
        if s.recording.active && s.recording.written_until_ms > s.recording.started_ms {
            ensure!(s.mic.state == "disabled", "the microphone is unexpectedly active");
            let up_to = s.recording.written_until_ms;
            let entry = segments.entry(s.recording.path.clone()).or_insert((up_to, 0));
            if entry.1 != up_to {
                status_log.push(format!(
                    "IPC path={} started={} written={} span={:.3}s wall-lag={}ms {}x{} {:.1}fps {} / {}",
                    s.recording.path, s.recording.started_ms, up_to,
                    (up_to - s.recording.started_ms) as f64 / 1000.0, cv_core::util::now_ms() - up_to,
                    s.engine.width, s.engine.height, s.engine.fps, s.engine.capture, s.engine.encoder,
                ));
            }
            entry.1 = up_to;
        }
        latest = s;
        if run.elapsed() >= Duration::from_secs(55) {
            std::fs::write(profile.dir.join("qa-window-ipc.txt"), status_log.join("\n"))?;
            anyhow::bail!("No GUI live completion ({} segments): {:?}", segments.len(), latest.engine);
        }
        thread::sleep(Duration::from_millis(125));
    }
    ensure!(segments.len() >= 2 && segments.values().any(|(first, last)| last - first >= 1_000),
        "IPC did not show multiple durable fragments and rollover");
    ensure!(latest.engine.capture == "Windows Graphics Capture", "wrong capture method");
    let oldest = segments.keys().next().context("first captured segment")?;
    let reader = cv_media::mp4::reader::Mp4Reader::open(Path::new(oldest))?;
    ensure!(reader.is_fragmented(), "expected an actual fragmented recording");
    ensure!((reader.duration_secs() - 8.0).abs() < 0.25, "first rollover was {:.3}s", reader.duration_secs());
    let video = reader.video().context("QA window video")?;
    ensure!(reader.sync_times(video)?.len() >= 2, "QA window recording did not contain multiple fragments");
    ensure!(reader.audio().len() == 2, "found unexpected application audio tracks");
    for track in reader.audio() {
        let pcm = cv_media::audioread::read_range(&reader, track, 0.25, 1.25)?;
        ensure!(pcm.iter().all(|x| x.abs() < 0.001), "QA fixture audio must be silent");
    }
    let initial = cv_media::thumb::keyframe_at(&reader, 1.05)?;
    let later = cv_media::thumb::keyframe_at(&reader, 3.05)?;
    let changed = initial.nv12.iter().zip(later.nv12).filter(|(a, b)| a.abs_diff(*b) > 25).count();
    ensure!(changed > initial.nv12.len() / 20, "the synthetic window was not visibly animated");
    status_log.push(format!(
        "PASS: verified {} captured segments, {:.3}s first rollover, {changed} substantially changed image samples, both audio channels silent.",
        segments.len(), reader.duration_secs()
    ));
    std::fs::write(profile.dir.join("qa-window-ipc.txt"), status_log.join("\n"))?;
    println!("{}", status_log.join("\n"));
    profile.shutdown()?;
    Ok(())
}