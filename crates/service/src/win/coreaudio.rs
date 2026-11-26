//! Windows Core Audio: endpoint enumeration, change notifications, and which
//! processes are playing audio on which output device.

use cv_core::audio_names::Endpoint;
use crossbeam_channel::Sender;
use std::collections::{HashMap, HashSet};
use windows::core::{implement, Interface, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, AudioSessionStateExpired, EDataFlow, ERole, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// Initialise COM (multithreaded) on the current thread.
pub fn com_init_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

pub struct AudioSystem {
    enumerator: IMMDeviceEnumerator,
    _client: Option<IMMNotificationClient>,
}

#[implement(IMMNotificationClient)]
struct DeviceWatcher {
    tx: Sender<()>,
}

impl IMMNotificationClient_Impl for DeviceWatcher_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        let _ = self.tx.try_send(());
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.try_send(());
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.try_send(());
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, _role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.try_send(());
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

fn device_id(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let p = dev.GetId().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn device_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let p = PropVariantToStringAlloc(&pv).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

impl AudioSystem {
    /// `change_tx` receives a unit message whenever devices change.
    pub fn new(change_tx: Option<Sender<()>>) -> windows::core::Result<AudioSystem> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client = match change_tx {
                Some(tx) => {
                    let c: IMMNotificationClient = DeviceWatcher { tx }.into();
                    enumerator.RegisterEndpointNotificationCallback(&c)?;
                    Some(c)
                }
                None => None,
            };
            Ok(AudioSystem { enumerator, _client: client })
        }
    }

    fn list(&self, flow: EDataFlow) -> Vec<(Endpoint, IMMDevice)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(coll) = self.enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return out };
            let n = coll.GetCount().unwrap_or(0);
            for i in 0..n {
                let Ok(dev) = coll.Item(i) else { continue };
                let (Some(id), Some(name)) = (device_id(&dev), device_name(&dev)) else { continue };
                out.push((Endpoint { id, name }, dev));
            }
        }
        out
    }

    pub fn inputs(&self) -> Vec<Endpoint> {
        self.list(eCapture).into_iter().map(|(e, _)| e).collect()
    }

    pub fn outputs(&self) -> Vec<Endpoint> {
        self.list(eRender).into_iter().map(|(e, _)| e).collect()
    }

    pub fn default_endpoint(&self, capture: bool) -> Option<Endpoint> {
        unsafe {
            let dev = self
                .enumerator
                .GetDefaultAudioEndpoint(if capture { eCapture } else { eRender }, eConsole)
                .ok()?;
            Some(Endpoint { id: device_id(&dev)?, name: device_name(&dev)? })
        }
    }

    /// For every active output device: the set of process ids with a live
    /// (non-expired) audio session on it.
    pub fn sessions_by_output(&self) -> HashMap<String, HashSet<u32>> {
        let mut out = HashMap::new();
        for (ep, dev) in self.list(eRender) {
            let mut pids = HashSet::new();
            unsafe {
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(en) = mgr.GetSessionEnumerator() else { continue };
                let n = en.GetCount().unwrap_or(0);
                for i in 0..n {
                    let Ok(ctl) = en.GetSession(i) else { continue };
                    if ctl.GetState().map(|s| s == AudioSessionStateExpired).unwrap_or(true) {
                        continue;
                    }
                    let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                    if let Ok(pid) = ctl2.GetProcessId() {
                        if pid != 0 {
                            pids.insert(pid);
                        }
                    }
                }
            }
            out.insert(ep.id, pids);
        }
        out
    }
}
