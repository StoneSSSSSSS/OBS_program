//! Sound capture with WASAPI: microphones, whole-device loopback, and
//! per-application loopback (Windows 10 2004+), all converted by Windows to
//! 48 kHz stereo float. Each source runs on its own thread and delivers
//! timestamped chunks.

use anyhow::{bail, Context, Result};
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{implement, Interface, IUnknown, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, IAgileObject, IAgileObject_Impl, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// A batch of captured audio.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub source: u32,
    /// Capture time of the first frame (performance counter, 100 ns).
    pub time_hns: i64,
    /// Interleaved stereo f32.
    pub data: Vec<f32>,
}

/// What to capture.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    /// A capture device by endpoint id (None = Windows default).
    Mic(Option<String>),
    /// Everything played on a render device (None = default).
    DeviceLoopback(Option<String>),
    /// One process and its children.
    App(u32),
}

/// A running capture; stops when dropped.
pub struct AudioSource {
    pub id: u32,
    pub kind: SourceKind,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioSource {
    pub fn start(id: u32, kind: SourceKind, tx: Sender<Chunk>) -> AudioSource {
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (s, f, k) = (stop.clone(), failed.clone(), kind.clone());
        let thread = std::thread::Builder::new()
            .name(format!("audio-{id}"))
            .spawn(move || {
                crate::mf::init();
                if let Err(e) = run(id, &k, &tx, &s) {
                    tracing::debug!("audio source {id} ({k:?}) ended: {e:#}");
                    f.store(true, Ordering::SeqCst);
                }
            })
            .expect("audio thread");
        AudioSource { id, kind, stop, failed, thread: Some(thread) }
    }

    /// The device went away or capture failed.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

impl Drop for AudioSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn float_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
        nBlockAlign: CHANNELS as u16 * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device list") }
}

fn device(id: Option<&str>, flow: EDataFlow) -> Result<IMMDevice> {
    let e = enumerator()?;
    unsafe {
        match id {
            Some(id) => {
                let w: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                e.GetDevice(PCWSTR(w.as_ptr())).context("audio device not found")
            }
            None => e.GetDefaultAudioEndpoint(flow, eConsole).context("no default audio device"),
        }
    }
}

/// A Win32 event closed when the last user lets go (the activation callback
/// may fire after we stopped waiting).
struct OwnedEvent(HANDLE);
unsafe impl Send for OwnedEvent {}
unsafe impl Sync for OwnedEvent {}
impl Drop for OwnedEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated {
    event: Arc<OwnedEvent>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe {
            let _ = SetEvent(self.event.0);
        }
        Ok(())
    }
}

impl IAgileObject_Impl for Activated_Impl {}

/// IAudioClient capturing one process tree.
fn app_client(pid: u32) -> Result<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE },
            },
        };
        let mut raw: windows_core::imp::PROPVARIANT = std::mem::zeroed();
        raw.Anonymous.Anonymous.vt = 65; // VT_BLOB
        raw.Anonymous.Anonymous.Anonymous.blob = windows_core::imp::BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: &params as *const _ as *mut u8 };
        // Not ours to free: the blob points at `params` on the stack.
        let pv = std::mem::ManuallyDrop::new(windows_core::PROPVARIANT::from_raw(raw));
        let event = Arc::new(OwnedEvent(CreateEventW(None, true, false, None)?));
        let handler: IActivateAudioInterfaceCompletionHandler = Activated { event: event.clone() }.into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv as *const _), &handler)
            .context("app audio capture is not supported (needs Windows 10 2004+)")?;
        let w = WaitForSingleObject(event.0, 5000);
        if w != WAIT_OBJECT_0 {
            bail!("app audio capture didn't start");
        }
        let mut hr = HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok().context("app audio capture refused")?;
        unk.context("no audio client")?.cast().context("audio client")
    }
}

fn run(id: u32, kind: &SourceKind, tx: &Sender<Chunk>, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let (client, flags) = match kind {
            SourceKind::Mic(dev) => (device(dev.as_deref(), eCapture)?.Activate::<IAudioClient>(CLSCTX_ALL, None)?, 0),
            SourceKind::DeviceLoopback(dev) => (device(dev.as_deref(), eRender)?.Activate::<IAudioClient>(CLSCTX_ALL, None)?, AUDCLNT_STREAMFLAGS_LOOPBACK),
            SourceKind::App(pid) => (app_client(*pid)?, AUDCLNT_STREAMFLAGS_LOOPBACK),
        };
        let fmt = float_format();
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000, // 200 ms buffer
                0,
                &fmt,
                None,
            )
            .context("starting audio capture")?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let result = (|| -> Result<()> {
            while !stop.load(Ordering::SeqCst) {
                WaitForSingleObject(event, 100);
                loop {
                    let n = cap.GetNextPacketSize()?;
                    if n == 0 {
                        break;
                    }
                    let mut p = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut fl = 0u32;
                    let mut qpc = 0u64;
                    cap.GetBuffer(&mut p, &mut frames, &mut fl, None, Some(&mut qpc))?;
                    let count = frames as usize * CHANNELS;
                    let data = if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || p.is_null() {
                        vec![0.0f32; count]
                    } else {
                        std::slice::from_raw_parts(p as *const f32, count).to_vec()
                    };
                    cap.ReleaseBuffer(frames)?;
                    // The QPC position is when the first frame was captured.
                    let time_hns = if qpc > 0 { qpc as i64 } else { crate::clock::now_hns() - frames as i64 * 10_000_000 / RATE as i64 };
                    if frames > 0 && tx.send(Chunk { source: id, time_hns, data }).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        let _ = CloseHandle(event);
        result
    }
}

/// Capture devices: (endpoint id, friendly name).
pub fn list_devices(capture: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(e) = enumerator() else { return out };
    unsafe {
        let Ok(coll) = e.EnumAudioEndpoints(if capture { eCapture } else { eRender }, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..coll.GetCount().unwrap_or(0) {
            let Ok(d) = coll.Item(i) else { continue };
            if let (Some(id), Some(name)) = (device_id(&d), device_name(&d)) {
                out.push((id, name));
            }
        }
    }
    out
}

pub fn device_id(d: &IMMDevice) -> Option<String> {
    unsafe {
        let p = d.GetId().ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

pub fn device_name(d: &IMMDevice) -> Option<String> {
    unsafe {
        let store = d.OpenPropertyStore(windows::Win32::System::Com::STGM_READ).ok()?;
        let pv = store.GetValue(&windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName).ok()?;
        let s = pv.to_string();
        (!s.is_empty()).then_some(s)
    }
}
