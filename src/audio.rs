//! Native audio control through Windows Core Audio: devices, default endpoints,
//! endpoint volume, per-app sessions and peak meters.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
    /// The driver applies volume in hardware (Windows passes it to the device).
    pub hw_volume: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// Lowercase executable name (`chrome.exe`), or `system` for system sounds.
    pub key: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
    pub active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub outputs: Vec<Device>,
    pub inputs: Vec<Device>,
    pub sessions: Vec<Session>,
}

#[cfg(windows)]
pub use imp::Audio;

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::c_void;

    use anyhow::{Result, anyhow};
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{CloseHandle, S_OK};
    use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eCapture, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::{GUID, Interface, PCWSTR, PWSTR};

    use super::{Device, Session, State};

    /// Undocumented interface used by Windows' own sound settings to change default devices.
    #[allow(non_snake_case)]
    mod policy {
        use std::ffi::c_void;
        use windows::Win32::Media::Audio::ERole;
        use windows::core::{HRESULT, PCWSTR};

        #[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
        pub unsafe trait IPolicyConfig: windows::core::IUnknown {
            fn GetMixFormat(&self, id: PCWSTR, format: *mut *mut c_void) -> HRESULT;
            fn GetDeviceFormat(&self, id: PCWSTR, default: i32, format: *mut *mut c_void) -> HRESULT;
            fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
            fn SetDeviceFormat(&self, id: PCWSTR, endpoint: *mut c_void, mix: *mut c_void) -> HRESULT;
            fn GetProcessingPeriod(&self, id: PCWSTR, default: i32, period: *mut i64, min: *mut i64) -> HRESULT;
            fn SetProcessingPeriod(&self, id: PCWSTR, period: *mut i64) -> HRESULT;
            fn GetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
            fn SetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
            fn GetPropertyValue(&self, id: PCWSTR, key: *const c_void, value: *mut c_void) -> HRESULT;
            fn SetPropertyValue(&self, id: PCWSTR, key: *const c_void, value: *mut c_void) -> HRESULT;
            fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> HRESULT;
            fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
        }

        pub fn set_default_endpoint(policy: &IPolicyConfig, id: PCWSTR, role: ERole) -> windows::core::Result<()> {
            unsafe { policy.SetDefaultEndpoint(id, role).ok() }
        }
    }
    use policy::IPolicyConfig;
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    struct Endpoint {
        name: String,
        volume: IAudioEndpointVolume,
        meter: IAudioMeterInformation,
    }

    struct AppSession {
        key: String,
        name: String,
        volume: ISimpleAudioVolume,
        meter: IAudioMeterInformation,
        active: bool,
    }

    /// Owns COM objects, so it must stay on the thread that created it.
    pub struct Audio {
        enumerator: IMMDeviceEnumerator,
        endpoints: HashMap<String, Endpoint>,
        sessions: Vec<AppSession>,
        names: HashMap<u32, (String, String)>,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    unsafe fn take(p: PWSTR) -> String {
        let s = unsafe { p.to_string().unwrap_or_default() };
        unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
        s
    }

    /// Friendly process name: the executable's FileDescription, else its file stem.
    fn process_name(pid: u32) -> Option<(String, String)> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
            let _ = CloseHandle(h);
            if !ok {
                return None;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let file = path.rsplit('\\').next().unwrap_or(&path).to_lowercase();
            let stem = file.trim_end_matches(".exe");
            let mut nice = stem[..1].to_uppercase() + &stem[1..];

            let wpath = wide(&path);
            let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None);
            if size > 0 {
                let mut data = vec![0u8; size as usize];
                if GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void).is_ok() {
                    // Use the first language/codepage listed in the translation table.
                    let mut ptr: *mut c_void = std::ptr::null_mut();
                    let mut n = 0u32;
                    let q = wide("\\VarFileInfo\\Translation");
                    if VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(q.as_ptr()), &mut ptr, &mut n).as_bool() && n >= 4 {
                        let lang = *(ptr as *const u16);
                        let cp = *((ptr as *const u16).add(1));
                        let q = wide(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\FileDescription"));
                        if VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(q.as_ptr()), &mut ptr, &mut n).as_bool() && n > 1 {
                            let desc = String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16, n as usize - 1));
                            let desc = desc.trim();
                            if !desc.is_empty() {
                                nice = desc.to_string();
                            }
                        }
                    }
                }
            }
            Some((file, nice))
        }
    }

    impl Audio {
        pub fn new() -> Result<Self> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                Ok(Self { enumerator, endpoints: HashMap::new(), sessions: Vec::new(), names: HashMap::new() })
            }
        }

        fn list(&mut self, flow: windows::Win32::Media::Audio::EDataFlow) -> Result<Vec<Device>> {
            unsafe {
                let default_id = self.enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok().and_then(|d| d.GetId().ok()).map(|p| take(p));
                let coll = self.enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
                let mut out = Vec::new();
                for i in 0..coll.GetCount()? {
                    let dev: IMMDevice = coll.Item(i)?;
                    let id = take(dev.GetId()?);
                    if !self.endpoints.contains_key(&id) {
                        let props = dev.OpenPropertyStore(STGM_READ)?;
                        let name = props.GetValue(&PKEY_Device_FriendlyName)?.to_string();
                        let volume: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
                        let meter: IAudioMeterInformation = dev.Activate(CLSCTX_ALL, None)?;
                        self.endpoints.insert(id.clone(), Endpoint { name, volume, meter });
                    }
                    let ep = &self.endpoints[&id];
                    out.push(Device {
                        is_default: default_id.as_deref() == Some(id.as_str()),
                        name: ep.name.clone(),
                        volume: ep.volume.GetMasterVolumeLevelScalar().unwrap_or(0.0),
                        muted: ep.volume.GetMute().map(|b| b.as_bool()).unwrap_or(false),
                        peak: ep.meter.GetPeakValue().unwrap_or(0.0),
                        hw_volume: ep.volume.QueryHardwareSupport().map(|f| f & 1 != 0).unwrap_or(false),
                        id,
                    });
                }
                Ok(out)
            }
        }

        /// Re-enumerates the audio sessions on the default output device.
        pub fn refresh_sessions(&mut self) -> Result<()> {
            unsafe {
                let dev = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let manager: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
                let list = manager.GetSessionEnumerator()?;
                let mut sessions = Vec::new();
                for i in 0..list.GetCount()? {
                    let control = list.GetSession(i)?;
                    let c2: IAudioSessionControl2 = control.cast()?;
                    let state = control.GetState()?;
                    if state == AudioSessionStateExpired {
                        continue;
                    }
                    let (key, name) = if c2.IsSystemSoundsSession() == S_OK {
                        ("system".to_string(), "System sounds".to_string())
                    } else {
                        let pid = c2.GetProcessId()?;
                        match self.names.get(&pid).cloned().or_else(|| process_name(pid)) {
                            Some(n) => {
                                self.names.insert(pid, n.clone());
                                n
                            }
                            None => continue,
                        }
                    };
                    sessions.push(AppSession {
                        key,
                        name,
                        volume: control.cast()?,
                        meter: control.cast()?,
                        active: state == AudioSessionStateActive,
                    });
                }
                self.sessions = sessions;
                Ok(())
            }
        }

        /// Reads devices and per-app levels. Sessions come from the last `refresh_sessions`.
        pub fn state(&mut self) -> Result<State> {
            let outputs = self.list(eRender)?;
            let inputs = self.list(eCapture)?;
            // One strip per app: merge sessions that belong to the same executable.
            let mut apps: Vec<super::Session> = Vec::new();
            for s in &self.sessions {
                let (volume, muted, peak) = unsafe {
                    (
                        s.volume.GetMasterVolume().unwrap_or(0.0),
                        s.volume.GetMute().map(|b| b.as_bool()).unwrap_or(false),
                        s.meter.GetPeakValue().unwrap_or(0.0),
                    )
                };
                match apps.iter_mut().find(|a| a.key == s.key) {
                    Some(a) => {
                        a.peak = a.peak.max(peak);
                        a.active |= s.active;
                    }
                    None => apps.push(Session { key: s.key.clone(), name: s.name.clone(), volume, muted, peak, active: s.active }),
                }
            }
            Ok(State { outputs, inputs, sessions: apps })
        }

        pub fn set_default(&self, id: &str) -> Result<()> {
            unsafe {
                let policy: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
                let w = wide(id);
                for role in [eConsole, eMultimedia, eCommunications] {
                    policy::set_default_endpoint(&policy, PCWSTR(w.as_ptr()), role)?;
                }
                Ok(())
            }
        }

        fn endpoint(&self, id: &str) -> Result<&Endpoint> {
            self.endpoints.get(id).ok_or_else(|| anyhow!("unknown device {id}"))
        }

        pub fn set_device_volume(&self, id: &str, v: f32) -> Result<()> {
            unsafe { Ok(self.endpoint(id)?.volume.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null())?) }
        }

        pub fn set_device_mute(&self, id: &str, mute: bool) -> Result<()> {
            unsafe { Ok(self.endpoint(id)?.volume.SetMute(mute, std::ptr::null())?) }
        }

        pub fn set_session_volume(&self, key: &str, v: f32) -> Result<()> {
            for s in self.sessions.iter().filter(|s| s.key == key) {
                unsafe { s.volume.SetMasterVolume(v.clamp(0.0, 1.0), std::ptr::null())? };
            }
            Ok(())
        }

        pub fn set_session_mute(&self, key: &str, mute: bool) -> Result<()> {
            for s in self.sessions.iter().filter(|s| s.key == key) {
                unsafe { s.volume.SetMute(mute, std::ptr::null())? };
            }
            Ok(())
        }
    }
}

/// Non-Windows platforms: no native backend yet.
#[cfg(not(windows))]
pub struct Audio;

#[cfg(not(windows))]
impl Audio {
    pub fn new() -> anyhow::Result<Self> {
        anyhow::bail!("native audio control is only implemented for Windows")
    }
    pub fn refresh_sessions(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn state(&mut self) -> anyhow::Result<State> {
        Ok(State::default())
    }
    pub fn set_default(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn set_device_volume(&self, _id: &str, _v: f32) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn set_device_mute(&self, _id: &str, _m: bool) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn set_session_volume(&self, _k: &str, _v: f32) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn set_session_mute(&self, _k: &str, _m: bool) -> anyhow::Result<()> {
        Ok(())
    }
}
