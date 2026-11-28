//! Media Foundation plumbing shared by the encoders and decoders.

pub mod aac;
pub mod h264;
pub mod h264dec;

use anyhow::{bail, Context, Result};
use std::sync::Once;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

static STARTUP: Once = Once::new();

/// Initialise COM (multithreaded) for this thread and Media Foundation for the
/// process. Cheap to call repeatedly.
pub fn init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    STARTUP.call_once(|| unsafe {
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
            tracing::error!("Media Foundation failed to start: {e}");
        }
    });
}

/// 100-nanosecond units per second (Media Foundation's clock).
pub const HNS: i64 = 10_000_000;

pub(crate) fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub(crate) fn new_type(major: &GUID, sub: &GUID) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        t.SetGUID(&MF_MT_SUBTYPE, sub)?;
        Ok(t)
    }
}

/// A sample wrapping a copy of `data`.
pub(crate) fn sample_from(data: &[u8], time_hns: i64, dur_hns: i64) -> Result<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(data.len().max(1) as u32)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.Unlock()?;
        buf.SetCurrentLength(data.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        s.SetSampleTime(time_hns)?;
        s.SetSampleDuration(dur_hns)?;
        Ok(s)
    }
}

/// Copy a sample's bytes out.
pub(crate) fn sample_bytes(s: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buf = s.ConvertToContiguousBuffer()?;
        let mut p = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut p, None, Some(&mut len))?;
        let v = std::slice::from_raw_parts(p, len as usize).to_vec();
        buf.Unlock()?;
        Ok(v)
    }
}

pub(crate) fn is_keyframe(s: &IMFSample) -> bool {
    unsafe { s.GetUINT32(&MFSampleExtension_CleanPoint).map(|v| v != 0).unwrap_or(false) }
}

/// Drives a synchronous MFT: feed input, collect all output it can produce.
pub(crate) struct SyncMft {
    pub mft: IMFTransform,
    provides_samples: bool,
    out_size: u32,
}

pub(crate) enum Pulled {
    Sample(IMFSample),
    NeedInput,
    StreamChange,
}

impl SyncMft {
    pub fn new(mft: IMFTransform) -> SyncMft {
        SyncMft { mft, provides_samples: false, out_size: 0 }
    }

    /// Call after the types are set.
    pub fn begin(&mut self) -> Result<()> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(0)?;
            self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            self.out_size = info.cbSize.max(1 << 16);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(())
    }

    pub fn refresh_out_info(&mut self) {
        unsafe {
            if let Ok(info) = self.mft.GetOutputStreamInfo(0) {
                self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
                self.out_size = info.cbSize.max(1 << 16);
            }
        }
    }

    /// Returns false if the MFT wants its output drained first.
    pub fn push(&mut self, s: &IMFSample) -> Result<bool> {
        unsafe {
            match self.mft.ProcessInput(0, s, 0) {
                Ok(()) => Ok(true),
                Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
                Err(e) => Err(e).context("ProcessInput"),
            }
        }
    }

    pub fn pull(&mut self) -> Result<Pulled> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let buf = MFCreateMemoryBuffer(self.out_size)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(sample), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut out, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut out[0].pSample);
            let _events = std::mem::ManuallyDrop::take(&mut out[0].pEvents);
            match r {
                Ok(()) => match sample {
                    Some(s) => Ok(Pulled::Sample(s)),
                    None => Ok(Pulled::NeedInput),
                },
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Pulled::StreamChange),
                Err(e) => Err(e).context("ProcessOutput"),
            }
        }
    }

    /// Everything currently available (stops at NeedInput / StreamChange).
    pub fn pull_all(&mut self, out: &mut Vec<IMFSample>) -> Result<bool> {
        loop {
            match self.pull()? {
                Pulled::Sample(s) => out.push(s),
                Pulled::NeedInput => return Ok(false),
                Pulled::StreamChange => return Ok(true),
            }
        }
    }

    pub fn drain(&mut self) -> Result<()> {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).context("drain")?;
        }
        Ok(())
    }

    pub fn flush(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// Every MFT of a category matching the input/output subtypes.
pub(crate) fn enum_mfts(category: GUID, flags: MFT_ENUM_FLAG, input: Option<(GUID, GUID)>, output: Option<(GUID, GUID)>) -> Vec<IMFActivate> {
    let mut out = Vec::new();
    unsafe {
        let inp = input.map(|(m, s)| MFT_REGISTER_TYPE_INFO { guidMajorType: m, guidSubtype: s });
        let outp = output.map(|(m, s)| MFT_REGISTER_TYPE_INFO { guidMajorType: m, guidSubtype: s });
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        let r = MFTEnumEx(
            category,
            flags,
            inp.as_ref().map(|p| p as *const _),
            outp.as_ref().map(|p| p as *const _),
            &mut arr,
            &mut n,
        );
        if r.is_err() || arr.is_null() {
            return out;
        }
        for i in 0..n as usize {
            if let Some(a) = std::ptr::read(arr.add(i)) {
                out.push(a);
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(arr as *const _));
    }
    out
}

pub(crate) fn activate_string(a: &IMFActivate, key: &GUID) -> String {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(key, &mut p, &mut len).is_ok() && !p.is_null() {
            let s = p.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
        String::new()
    }
}

pub(crate) fn create_mft(clsid: &GUID) -> Result<IMFTransform> {
    unsafe {
        let m: IMFTransform = windows::Win32::System::Com::CoCreateInstance(clsid, None, windows::Win32::System::Com::CLSCTX_INPROC_SERVER)
            .with_context(|| format!("creating Media Foundation component {clsid:?}"))?;
        Ok(m)
    }
}

pub(crate) fn guid_eq(t: &IMFMediaType, key: &GUID, want: &GUID) -> bool {
    unsafe { t.GetGUID(key).map(|g| g == *want).unwrap_or(false) }
}

/// Pick the first available output type with this subtype.
pub(crate) fn find_output_type(mft: &IMFTransform, sub: &GUID) -> Result<IMFMediaType> {
    unsafe {
        for i in 0..64 {
            match mft.GetOutputAvailableType(0, i) {
                Ok(t) => {
                    if guid_eq(&t, &MF_MT_SUBTYPE, sub) {
                        return Ok(t);
                    }
                }
                Err(_) => break,
            }
        }
    }
    bail!("no matching output type")
}

