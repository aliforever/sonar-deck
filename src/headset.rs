//! SteelSeries wireless headset status straight from the USB dongle (no SteelSeries GG needed).
//!
//! The Arctis Nova Elite dongle (VID 0x1038, PID 0x2244) answers a 0xB0 "wireless settings" query on
//! its vendor HID collection (usage page 0xFFC0, report ID 1). The reply layout comes from the
//! `wireless_settings` struct in SteelSeries GG's device spec for this headset
//! (deviceSpecifications/base_arctis_nova_elite_tx.edevice, an OpenPGP-encrypted Lisp file).

use hidapi::{HidApi, HidDevice};

const VID: u16 = 0x1038;
/// (product id, model name)
const DONGLES: [(u16, &str); 1] = [(0x2244, "Arctis Nova Elite")];
const USAGE_PAGE: u16 = 0xffc0;
/// Second vendor collection (report ID 7): the base station pushes live events here,
/// e.g. `07 25 NN` when the volume knob turns (NN = attenuation in dB, 0 = top of the dial).
const EVENTS_USAGE_PAGE: u16 = 0xff00;
const EVENT_KNOB: u8 = 0x25;
const REPORT_ID: u8 = 0x01;
const STATUS: u8 = 0xb0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub model: String,
    /// Raw status reply, shown in Settings for diagnostics.
    pub raw: Vec<u8>,
    /// Whether the headset itself (not just the dongle) is powered on and linked.
    pub connected: Option<bool>,
    /// Battery in the headset, percent.
    pub battery: Option<u8>,
    /// Spare hot-swap battery charging in the base station, percent.
    pub spare_battery: Option<u8>,
    pub charging: Option<&'static str>,
    /// Base station volume knob, in dB below the top of the dial.
    pub knob_db: Option<u8>,
}

/// Configurable headset and base-station settings (Nova Elite), read from the device.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Settings {
    /// 0 off, 1 transparency, 2 active noise cancellation
    pub anc_mode: u8,
    /// ANC strength 1..3
    pub anc_level: u8,
    /// Transparency level 1..10
    pub transparency: u8,
    pub mic_volume: u8,
    pub boom_sidetone: (bool, u8),
    pub ear_sidetone: (bool, u8),
    pub volume_limiter: bool,
    /// Noise reduction 0 (off)..3 for the boom mic and the on-ear mics.
    pub nr_boom: u8,
    pub nr_ear: u8,
    /// Muted-mic LED brightness 0..10
    pub mic_led: u8,
    /// Index into [never, 1, 5, 10, 15, 30, 60] minutes
    pub inactivity: u8,
    pub bt_power_default: bool,
    /// 0 do nothing, 1 lower other audio 12 dB, 2 mute other audio
    pub bt_call: u8,
    pub oled_brightness: u8,
    /// Index into [never, 1, 5, 10, 15, 30, 60] minutes
    pub oled_saver_timer: u8,
    /// 0 off, 1 dim
    pub oled_saver_mode: u8,
    /// 0 detailed, 1 simple
    pub home_view: u8,
    /// 0 stereo, 1 preset, 2 meters
    pub home_option: u8,
}

/// A 10-band EQ preset for the headset's onboard (2.4 GHz) EQ.
/// Bands are `[frequency Hz (20001 = off), filter type, gain in 0.1 dB, Q x 1000]`;
/// filter types: 1 peak, 2 low-pass, 3 high-pass, 4 low shelf, 5 high shelf, 6 notch.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EqPreset {
    pub name: String,
    pub short: String,
    pub bands: Vec<[i32; 4]>,
}

/// Game/music/movie presets for the onboard EQ, bundled with the app.
pub fn eq_presets() -> &'static [EqPreset] {
    static PRESETS: std::sync::OnceLock<Vec<EqPreset>> = std::sync::OnceLock::new();
    PRESETS.get_or_init(|| serde_json::from_str(include_str!("../assets/eq_presets.json")).unwrap_or_default())
}

/// Flat EQ, the headset's first built-in preset.
pub fn flat_preset() -> EqPreset {
    let freqs = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    EqPreset { name: "Flat".into(), short: "FLAT".into(), bands: freqs.iter().map(|&f| [f, 1, 0, 1414]).collect() }
}

pub struct Headset {
    api: Option<HidApi>,
    dev: Option<(HidDevice, &'static str)>,
    /// Last values read from the device; writes that set two fields at once need the other one.
    pub settings: Option<Settings>,
    last_status: Vec<u8>,
    events: Option<HidDevice>,
    /// Last knob position reported by the base station.
    pub knob_db: Option<u8>,
    /// Reports that arrived without being asked for (other than knob turns), for diagnostics.
    pub unsolicited: Vec<Vec<u8>>,
}

impl Headset {
    pub fn new() -> Self {
        Self {
            api: HidApi::new().ok(),
            dev: None,
            settings: None,
            last_status: Vec::new(),
            events: None,
            knob_db: None,
            unsolicited: Vec::new(),
        }
    }

    fn open(&mut self) -> Option<()> {
        let api = self.api.as_mut()?;
        api.refresh_devices().ok()?;
        for info in api.device_list() {
            let Some((_, model)) = DONGLES.iter().find(|(pid, _)| info.vendor_id() == VID && info.product_id() == *pid) else {
                continue;
            };
            if info.usage_page() == USAGE_PAGE && self.dev.is_none() {
                if let Ok(dev) = info.open_device(api) {
                    self.dev = Some((dev, model));
                }
            } else if info.usage_page() == EVENTS_USAGE_PAGE && self.events.is_none() {
                match info.open_device(api) {
                    Ok(dev) => {
                        let _ = dev.set_blocking_mode(false);
                        crate::debug_log("opened event collection");
                        self.events = Some(dev);
                    }
                    Err(e) => crate::debug_log(&format!("cannot open event collection: {e}")),
                }
            }
        }
        self.dev.is_some().then_some(())
    }

    /// Reads the knob position from the audio_settings block (byte 142 mirrors it).
    pub fn read_knob(&mut self) -> Option<u8> {
        let a = self.audio_settings()?;
        self.knob_db = Some(a[142]);
        self.knob_db
    }

    /// Reads the base station's pushed events (knob turns) without blocking.
    pub fn drain_events(&mut self) {
        let Some(dev) = self.events.as_ref() else { return };
        let mut buf = [0u8; 64];
        loop {
            match dev.read(&mut buf) {
                Ok(n) if n >= 3 => {
                    if buf[0] == 0x07 && buf[1] == EVENT_KNOB {
                        self.knob_db = Some(buf[2]);
                        crate::debug_log(&format!("knob event {}", buf[2]));
                    } else {
                        if self.unsolicited.len() < 64 {
                            self.unsolicited.push(buf[..n].to_vec());
                        }
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    crate::debug_log(&format!("event read failed: {e}"));
                    self.events = None;
                    break;
                }
            }
        }
    }

    /// Queries the dongle. `None` means no supported dongle is plugged in.
    pub fn poll(&mut self) -> Option<Status> {
        if self.dev.is_none() {
            self.open()?;
        }
        let (dev, model) = self.dev.as_ref()?;
        let mut out = [0u8; 64];
        out[0] = REPORT_ID;
        out[1] = STATUS;
        if dev.write(&out).is_err() {
            self.dev = None; // unplugged; reopen next time
            return None;
        }
        let mut buf = [0u8; 64];
        // Skip unrelated input reports until the status reply arrives.
        for _ in 0..5 {
            match dev.read_timeout(&mut buf, 250) {
                Ok(n) if n >= 8 && buf[0] == REPORT_ID && buf[1] == STATUS => {
                    let raw = buf[..n].to_vec();
                    self.last_status = raw.clone();
                    let mut status = decode(&raw);
                    status.knob_db = self.knob_db;
                    status.model = model.to_string();
                    status.raw = raw;
                    return Some(status);
                }
                Ok(n) => {
                    if n > 0 && self.unsolicited.len() < 64 {
                        self.unsolicited.push(buf[..n].to_vec());
                    }
                    continue;
                }
                Err(_) => {
                    self.dev = None;
                    return None;
                }
            }
        }
        Some(Status { model: model.to_string(), ..Default::default() })
    }
}

/// Opens every HID collection of a supported dongle for diagnostics: (usage page, usage, device).
pub fn open_all_collections() -> Vec<(u16, u16, HidDevice)> {
    let Ok(api) = HidApi::new() else { return Vec::new() };
    api.device_list()
        .filter(|i| i.vendor_id() == VID && DONGLES.iter().any(|(pid, _)| i.product_id() == *pid))
        .filter_map(|i| {
            let dev = i.open_device(&api).ok()?;
            let _ = dev.set_blocking_mode(false);
            Some((i.usage_page(), i.usage(), dev))
        })
        .collect()
}

impl Headset {
    /// Sends `[report id, command, args...]` as a 64-byte output report.
    fn send(&mut self, command: u8, args: &[u8]) -> anyhow::Result<()> {
        if self.dev.is_none() {
            self.open().ok_or_else(|| anyhow::anyhow!("headset dongle not found"))?;
        }
        let (dev, _) = self.dev.as_ref().unwrap();
        let mut out = [0u8; 64];
        out[0] = REPORT_ID;
        out[1] = command;
        out[2..2 + args.len()].copy_from_slice(args);
        dev.write(&out)?;
        Ok(())
    }

    /// Sends a command and returns its reply (an input report echoing the command).
    fn query(&mut self, command: u8) -> Option<Vec<u8>> {
        self.send(command, &[]).ok()?;
        let (dev, _) = self.dev.as_ref()?;
        let mut buf = [0u8; 64];
        for _ in 0..5 {
            match dev.read_timeout(&mut buf, 250) {
                Ok(n) if n >= 3 && buf[0] == REPORT_ID && buf[1] == command => return Some(buf[..n].to_vec()),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Reads the audio_settings block (command 0x20), returned as a feature report.
    fn audio_settings(&mut self) -> Option<Vec<u8>> {
        self.send(0x20, &[]).ok()?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        let (dev, _) = self.dev.as_ref()?;
        let mut buf = vec![0u8; 1036];
        buf[0] = REPORT_ID;
        let n = dev.get_feature_report(&mut buf).ok()?;
        (n > 171 && buf[0] == 0x20).then(|| buf[..n].to_vec())
    }

    /// Reads all configurable settings. Offsets follow the wireless_settings (0xB0),
    /// audio_settings (0x20) and ux_settings (0x80) structs in GG's device spec.
    pub fn read_settings(&mut self) -> Option<Settings> {
        let w = if self.last_status.len() >= 17 { self.last_status.clone() } else { self.query(STATUS)? };
        let a = self.audio_settings()?;
        let u = self.query(0x80)?;
        if w.len() < 17 || u.len() < 12 {
            return None;
        }
        // audio_settings: command byte, two 10-band EQ blocks (6 bytes per band), BT and mic gains,
        // then the scalar fields starting at offset 141.
        // Byte 142 mirrors the knob; use it until the first live event arrives.
        if self.knob_db.is_none() {
            self.knob_db = Some(a[142]);
        }
        let s = Settings {
            bt_power_default: w[2] == 1,
            bt_call: w[3],
            transparency: w[8],
            anc_mode: w[10],
            mic_led: w[11],
            inactivity: w[12],
            anc_level: w[16],
            volume_limiter: a[143] == 1,
            mic_volume: a[145],
            boom_sidetone: (a[169] == 1, a[146]),
            nr_boom: if a[164] & 0x01 != 0 { a[165] } else { 0 },
            nr_ear: if a[164] & 0x10 != 0 { a[166] } else { 0 },
            ear_sidetone: (a[171] == 1, a[170]),
            oled_saver_timer: u[2],
            oled_brightness: u[3],
            home_view: u[5],
            oled_saver_mode: u[10],
            home_option: u[11],
        };
        self.settings = Some(s);
        Some(s)
    }

    /// Applies one setting. `key` names a field of [`Settings`]; GG's command bytes per field.
    pub fn apply(&mut self, key: &str, value: u8) -> anyhow::Result<()> {
        let cur = self.settings.unwrap_or_default();
        let on = |b: bool| b as u8;
        match key {
            "anc-mode" => self.send(0xBD, &[value.min(2)])?,
            "anc-level" => self.send(0xB8, &[value.clamp(1, 3)])?,
            "transparency" => self.send(0xB9, &[value.clamp(1, 10)])?,
            "mic-volume" => self.send(0x37, &[value.clamp(1, 10)])?,
            "boom-sidetone" => self.send(0x38, &[value.min(1), cur.boom_sidetone.1.max(1)])?,
            "boom-sidetone-level" => self.send(0x38, &[on(cur.boom_sidetone.0), value.clamp(1, 10)])?,
            "ear-sidetone" => self.send(0x39, &[value.min(1), cur.ear_sidetone.1.max(1)])?,
            "ear-sidetone-level" => self.send(0x39, &[on(cur.ear_sidetone.0), value.clamp(1, 10)])?,
            "volume-limiter" => self.send(0x27, &[value.min(1)])?,
            "nr-boom" | "nr-ear" => {
                let (boom, ear) = if key == "nr-boom" { (value.min(3), cur.nr_ear) } else { (cur.nr_boom, value.min(3)) };
                // GG sends an on/off flag (bit 0 boom, bit 4 on-ear) followed by both levels (1..3).
                let flags = (boom != 0) as u8 | (((ear != 0) as u8) << 4);
                self.send(0x3C, &[flags, boom.max(1), ear.max(1)])?
            }
            "mic-led" => self.send(0xBF, &[value.min(10)])?,
            "inactivity" => self.send(0xC1, &[value.min(6)])?,
            "bt-power" => self.send(0xB2, &[value.min(1)])?,
            "bt-call" => self.send(0xB3, &[value.min(2)])?,
            "oled-brightness" => self.send(0x85, &[value.clamp(1, 10)])?,
            "oled-saver-timer" => self.send(0x83, &[value.min(6)])?,
            "oled-saver-mode" => self.send(0x88, &[value.min(1)])?,
            "home-view" => self.send(0x89, &[value.min(1)])?,
            "home-option" => self.send(0x8A, &[value.min(2)])?,
            _ => anyhow::bail!("unknown headset setting {key}"),
        }
        // Drop the stale status so the next read reflects the change.
        self.last_status.clear();
        Ok(())
    }

    /// Name of the EQ preset the headset is using (command 0x1A).
    pub fn read_eq_name(&mut self) -> Option<String> {
        self.send(0x1A, &[]).ok()?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        let (dev, _) = self.dev.as_ref()?;
        let mut buf = vec![0u8; 1036];
        buf[0] = REPORT_ID;
        let n = dev.get_feature_report(&mut buf).ok()?;
        if n < 69 || buf[0] != 0x1A {
            return None;
        }
        Some(String::from_utf8_lossy(&buf[8..69]).trim_end_matches('\0').to_string())
    }

    /// Loads a preset into the onboard EQ (feature report 0x1B, GG's `parametric_eq` command).
    /// Built-in names select their own slot; anything else goes to the game preset slot, so the
    /// user's Custom slot is never overwritten.
    pub fn apply_eq(&mut self, preset: &EqPreset) -> anyhow::Result<()> {
        if self.dev.is_none() {
            self.open().ok_or_else(|| anyhow::anyhow!("headset dongle not found"))?;
        }
        let slot = match preset.name.as_str() {
            "Flat" => 0,
            "Bass Boost" => 1,
            "Focus" => 2,
            "Smiley" => 3,
            _ => 5,
        };
        let mut r = vec![0u8; 1036];
        r[0] = REPORT_ID;
        r[1] = 0x1B;
        r[2] = slot;
        for (i, b) in preset.short.bytes().take(6).enumerate() {
            r[3 + i] = b;
        }
        for (i, b) in preset.name.bytes().take(60).enumerate() {
            r[9 + i] = b;
        }
        for (i, band) in preset.bands.iter().take(10).enumerate() {
            let o = 70 + i * 6;
            let [freq, kind, gain, q] = *band;
            r[o..o + 2].copy_from_slice(&(freq.clamp(20, 20001) as u16).to_le_bytes());
            r[o + 2] = kind.clamp(1, 6) as u8;
            r[o + 3] = gain.clamp(-120, 120) as i8 as u8;
            r[o + 4..o + 6].copy_from_slice(&(q.clamp(200, 10000) as u16).to_le_bytes());
        }
        let (dev, _) = self.dev.as_ref().unwrap();
        dev.send_feature_report(&r)?;
        Ok(())
    }

    /// Stores the current settings in the headset's flash so they survive a power cycle.
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.send(0x09, &[])
    }
}

/// Reply bytes (after report ID and command): 2 bt_power_default, 3 bt_call_default,
/// 4 bt_connection_mode, 5 bt_connection_status, 6 headset_batt_level, 7 charger_batt_level,
/// 8 transparent, 9 mic_mute (stays 0 on the Nova Elite: its mute button isn't reported over USB at all,
/// checked with `sonar-deck hidwatch`), 10 transparent_anc_mode, 11 muted_mic_brightness, 12 inactivity_timer,
/// 13 wireless_mode, 14 radio_connection_status, 15 charging_status, 16 active_noise_cancellation.
fn decode(raw: &[u8]) -> Status {
    if raw.len() < 16 {
        return Status::default();
    }
    // radio_connection_status: 1/2 = not paired, 4 = paired but disconnected (headset off), 8 = connected.
    let connected = raw[14] == 8;
    let percent = |b: u8| (b <= 100).then_some(b);
    Status {
        connected: Some(connected),
        battery: if connected { percent(raw[6]) } else { None },
        spare_battery: percent(raw[7]),
        charging: connected.then(|| match raw[15] {
            2 => "charging",
            4 => "plugged in",
            8 => "on battery",
            _ => "unknown",
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_connected_reply() {
        // Captured from a Nova Elite dongle with the headset on.
        let raw = [0x01, 0xb0, 0, 0, 1, 0, 0x64, 0x23, 0x0a, 0, 2, 0x0a, 5, 0, 8, 8, 3];
        let s = decode(&raw);
        assert_eq!(s.connected, Some(true));
        assert_eq!(s.battery, Some(100));
        assert_eq!(s.spare_battery, Some(35));
        assert_eq!(s.charging, Some("on battery"));
    }

    #[test]
    fn decodes_disconnected_reply() {
        // Synthesized from the spec: radio status 4 = paired, not connected (headset off).
        let raw = [0x01, 0xb0, 0, 0, 1, 0, 0x64, 0x23, 0x0a, 0, 2, 0x0a, 5, 0, 4, 1, 3];
        let s = decode(&raw);
        assert_eq!(s.connected, Some(false));
        assert_eq!(s.battery, None);
    }
}
