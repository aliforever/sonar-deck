//! App state: snapshots of the system's audio, user commands, output switching and the worker thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::audio::{Audio, Device, Session};
use crate::headset::{self, Headset};

/// Fast enough for smooth level meters.
const TICK: Duration = Duration::from_millis(80);
const SESSION_REFRESH: Duration = Duration::from_millis(1000);
const HEADSET_REFRESH: Duration = Duration::from_millis(1000);
const HEADSET_SETTINGS_REFRESH: Duration = Duration::from_secs(5);
/// Volume changes (knob turns, slider drags) are written to disk at most this often.
const SETTINGS_SAVE_INTERVAL: Duration = Duration::from_secs(2);
/// Wait for a burst of headset changes to finish before writing them to flash.
const HEADSET_SAVE_DELAY: Duration = Duration::from_millis(1200);

/// Which devices the quick switch uses. Empty fields are picked automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub auto_switch: bool,
    pub headset_out: String,
    pub speaker_out: String,
    pub headset_mic: String,
    /// Mic to use with the speakers; empty leaves the mic unchanged.
    pub speaker_mic: String,
    /// Let the headset handle volume itself instead of scaling apps in software.
    /// The Arctis Nova Elite ignores Windows' hardware volume, so this is off by default.
    pub use_device_volume: bool,
    /// Don't let the base station's volume knob act as a master volume.
    pub ignore_base_knob: bool,
    /// Per-app volume the user chose, before the software master is applied.
    pub app_levels: HashMap<String, f32>,
    /// App (lowercase exe name) -> group id; apps not listed use `default_group`.
    pub app_groups: HashMap<String, String>,
    pub group_levels: HashMap<String, f32>,
    pub group_mutes: HashMap<String, bool>,
    /// Session volume Sonar Deck last set per app, kept across restarts so its own
    /// scaling is never mistaken for a volume the user chose.
    pub app_applied: HashMap<String, f32>,
    /// Look for new releases on GitHub once a day.
    pub skip_update_check: bool,
}

/// Mixer groups, like Sonar's channels: (id, label).
pub const GROUPS: [(&str, &str); 4] = [("game", "Game"), ("chat", "Chat"), ("media", "Media"), ("aux", "Aux")];

/// Where an app goes until the user moves it.
pub fn default_group(key: &str) -> &'static str {
    const CHAT: [&str; 12] =
        ["discord", "teamspeak", "ts3client", "mumble", "telegram", "whatsapp", "zoom", "teams", "ms-teams", "slack", "skype", "signal"];
    const MEDIA: [&str; 14] = [
        "chrome",
        "msedge",
        "firefox",
        "opera",
        "brave",
        "vivaldi",
        "spotify",
        "vlc",
        "mpv",
        "music",
        "itunes",
        "foobar2000",
        "potplayer",
        "wmplayer",
    ];
    const AUX: [&str; 4] = ["system", "obs64", "msrdc", "mstsc"];
    let stem = key.trim_end_matches(".exe");
    if CHAT.iter().any(|c| stem.starts_with(c)) {
        "chat"
    } else if MEDIA.iter().any(|m| stem.starts_with(m)) {
        "media"
    } else if AUX.contains(&stem) {
        "aux"
    } else {
        "game"
    }
}

impl Settings {
    pub fn group_of(&self, key: &str) -> String {
        self.app_groups.get(key).cloned().unwrap_or_else(|| default_group(key).to_string())
    }

    /// Volume factor a group applies to its apps (0 when muted).
    pub fn group_factor(&self, group: &str) -> f32 {
        if self.group_mutes.get(group).copied().unwrap_or(false) { 0.0 } else { self.group_levels.get(group).copied().unwrap_or(1.0) }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("SonarDeck").join("settings.json"))
    }
    pub fn load() -> Self {
        Self::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        if let Some(p) = Self::path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Headset,
    Speakers,
    Toggle,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    HeadsetOut,
    SpeakerOut,
    HeadsetMic,
    SpeakerMic,
}

#[derive(Debug, Clone)]
pub enum Cmd {
    SetDefault(String),
    DeviceVolume(String, f32),
    DeviceMute(String, bool),
    AppVolume(String, f32),
    AppMute(String, bool),
    Output(Target),
    SetAutoSwitch(bool),
    SetRole(Role, String),
    /// A headset setting by name (see `headset::Headset::apply`) and its new value.
    Headset(String, u8),
    ApplyEq(String),
    GroupVolume(String, f32),
    GroupMute(String, bool),
    SetAppGroup(String, String),
    SetDeviceVolumeMode(bool),
    SetIgnoreKnob(bool),
    SetUpdateCheck(bool),
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub error: String,
    pub outputs: Vec<Device>,
    pub inputs: Vec<Device>,
    pub apps: Vec<Session>,
    pub headset: Option<headset::Status>,
    pub headset_settings: Option<headset::Settings>,
    /// Name of the EQ preset loaded on the headset.
    pub eq_name: Option<String>,
    /// True while app volumes are being scaled by the software master.
    pub software_volume: bool,
    pub settings: Settings,
    /// Device ids the quick switch resolves to right now.
    pub headset_out: String,
    pub speaker_out: String,
    pub headset_mic: String,
    pub speaker_mic: String,
}

fn find(list: &[Device], chosen: &str, pick: impl Fn(&Device) -> bool) -> String {
    list.iter()
        .find(|d| !chosen.is_empty() && d.id == chosen)
        .or_else(|| list.iter().find(|d| pick(d)))
        .map(|d| d.id.clone())
        .unwrap_or_default()
}

impl Snapshot {
    fn resolve(&mut self) {
        let s = &self.settings;
        let model = self.headset.as_ref().map(|h| h.model.to_lowercase());
        let is_headset = |d: &Device| {
            let n = d.name.to_lowercase();
            model.as_ref().is_some_and(|m| n.contains(m)) || n.contains("arctis")
        };
        self.headset_out = find(&self.outputs, &s.headset_out, |d| is_headset(d));
        self.headset_mic = find(&self.inputs, &s.headset_mic, |d| is_headset(d));
        let headset = self.headset_out.clone();
        self.speaker_out = find(&self.outputs, &s.speaker_out, |d| d.id != headset && d.name.to_lowercase().contains("speaker"));
        if self.speaker_out.is_empty() {
            self.speaker_out = self.outputs.iter().find(|d| d.id != headset).map(|d| d.id.clone()).unwrap_or_default();
        }
        self.speaker_mic = self.inputs.iter().find(|d| d.id == s.speaker_mic).map(|d| d.id.clone()).unwrap_or_default();
    }

    pub fn default_output(&self) -> Option<&Device> {
        self.outputs.iter().find(|d| d.is_default)
    }

    pub fn current_target(&self) -> Option<Target> {
        let id = &self.default_output()?.id;
        if *id == self.headset_out {
            Some(Target::Headset)
        } else if *id == self.speaker_out {
            Some(Target::Speakers)
        } else {
            None
        }
    }

    pub fn headset_connected(&self) -> Option<bool> {
        self.headset.as_ref().and_then(|h| h.connected)
    }
}

/// Makes the headset or the speakers the Windows default output (and switches the mic with it).
pub fn switch_output(audio: &Audio, snap: &Snapshot, target: Target) -> Result<Target> {
    let target = match target {
        Target::Toggle if snap.current_target() == Some(Target::Headset) => Target::Speakers,
        Target::Toggle => Target::Headset,
        Target::Auto => match snap.headset_connected() {
            Some(true) => Target::Headset,
            Some(false) => Target::Speakers,
            None => return Err(anyhow!("the headset's power state is unknown")),
        },
        t => t,
    };
    let (out, mic) = match target {
        Target::Headset => (&snap.headset_out, &snap.headset_mic),
        _ => (&snap.speaker_out, &snap.speaker_mic),
    };
    if out.is_empty() {
        return Err(anyhow!("no {} device found", if target == Target::Headset { "headset" } else { "speaker" }));
    }
    audio.set_default(out)?;
    if !mic.is_empty() {
        audio.set_default(mic)?;
    }
    Ok(target)
}

/// Reads everything once (used by the CLI and the worker).
pub fn read(audio: &mut Audio, headset: &mut Headset, settings: &Settings, sessions: bool) -> Result<Snapshot> {
    if sessions {
        audio.refresh_sessions()?;
    }
    let state = audio.state()?;
    let mut snap = Snapshot {
        outputs: state.outputs,
        inputs: state.inputs,
        apps: state.sessions,
        headset: headset.poll(),
        settings: settings.clone(),
        ..Default::default()
    };
    snap.resolve();
    Ok(snap)
}

/// Scales every app by its group, the base station knob and (for a headset that ignores it)
/// the Windows volume. The session volumes Sonar Deck sets live in `Settings::app_applied`.
#[derive(Default)]
struct SoftVolume {
    active: bool,
    last_master: f32,
    /// Settings changed and should be written soon (volume changes are written at most every 2 s).
    dirty: bool,
}

impl SoftVolume {
    /// The software master: the Windows volume of a headset that ignores it, times the base
    /// station's knob. Like Sonar, the knob is a master for whatever output is active.
    /// `None` when neither applies.
    fn master(snap: &Snapshot) -> Option<f32> {
        let out = snap.default_output()?;
        let endpoint = (!snap.settings.use_device_volume && out.hw_volume && snap.headset.is_some() && out.id == snap.headset_out)
            .then_some(if out.muted { 0.0 } else { out.volume });
        // The knob only reports its position (dB below the top); apply it here.
        let knob = snap
            .headset
            .as_ref()
            .and_then(|h| h.knob_db)
            .filter(|_| !snap.settings.ignore_base_knob)
            .map(|db| 10f32.powf(-(db as f32) / 20.0));
        match (endpoint, knob) {
            (None, None) => None,
            (e, k) => Some(e.unwrap_or(1.0) * k.unwrap_or(1.0)),
        }
    }

    /// Applies `level * master` to every app and reports the user's levels in the snapshot.
    fn sync(&mut self, audio: &Audio, snap: &mut Snapshot, settings: &mut Settings) {
        let master = Self::master(snap);
        let m = master.unwrap_or(1.0);
        if (m - self.last_master).abs() > 0.0005 {
            crate::debug_log(&format!("software master {m:.4} (active {})", master.is_some()));
            self.last_master = m;
        }
        let mut learned = false;
        for app in &mut snap.apps {
            let f = m * settings.group_factor(&settings.group_of(&app.key));
            let actual = app.volume;
            let ours = settings.app_applied.get(&app.key).is_some_and(|a| (actual - a).abs() < 0.01);
            let level = match settings.app_levels.get(&app.key) {
                Some(&l) if ours || (actual - l * f).abs() < 0.02 => l,
                // A new app, or its volume was changed elsewhere: take that as the user's level.
                _ => {
                    learned = true;
                    settings.app_levels.insert(app.key.clone(), actual);
                    actual
                }
            };
            let desired = level * f;
            if (actual - desired).abs() > 0.004
                && audio
                    .set_session_volume(&app.key, desired)
                    .inspect_err(|e| crate::debug_log(&format!("set {} failed: {e:#}", app.key)))
                    .is_ok()
            {
                settings.app_applied.insert(app.key.clone(), desired);
                learned = true;
            } else if !settings.app_applied.contains_key(&app.key) {
                settings.app_applied.insert(app.key.clone(), actual);
                learned = true;
            }
            app.volume = level;
        }
        self.dirty |= learned;
        self.active = master.is_some();
        snap.software_volume = self.active;
    }

    fn set_app(&mut self, audio: &Audio, snap: &Snapshot, settings: &mut Settings, key: &str, v: f32) -> Result<()> {
        let f = Self::master(snap).unwrap_or(1.0) * settings.group_factor(&settings.group_of(key));
        settings.app_levels.insert(key.to_string(), v);
        audio.set_session_volume(key, v * f)?;
        settings.app_applied.insert(key.to_string(), v * f);
        self.dirty = true;
        Ok(())
    }
}

fn run(audio: &Audio, hid: &mut Headset, soft: &mut SoftVolume, snap: &Snapshot, settings: &mut Settings, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::SetDefault(id) => audio.set_default(&id)?,
        Cmd::DeviceVolume(id, v) => audio.set_device_volume(&id, v)?,
        Cmd::DeviceMute(id, m) => audio.set_device_mute(&id, m)?,
        Cmd::AppVolume(key, v) => soft.set_app(audio, snap, settings, &key, v)?,
        Cmd::AppMute(key, m) => audio.set_session_mute(&key, m)?,
        Cmd::Output(t) => {
            switch_output(audio, snap, t)?;
        }
        Cmd::SetAutoSwitch(on) => {
            settings.auto_switch = on;
            settings.save();
            if on && snap.headset_connected().is_some() {
                switch_output(audio, snap, Target::Auto)?;
            }
        }
        Cmd::SetRole(role, id) => {
            match role {
                Role::HeadsetOut => settings.headset_out = id,
                Role::SpeakerOut => settings.speaker_out = id,
                Role::HeadsetMic => settings.headset_mic = id,
                Role::SpeakerMic => settings.speaker_mic = id,
            }
            settings.save();
        }
        Cmd::Headset(key, value) => hid.apply(&key, value)?,
        Cmd::ApplyEq(name) => {
            let preset = if name == "Flat" {
                headset::flat_preset()
            } else {
                headset::eq_presets().iter().find(|p| p.name == name).cloned().ok_or_else(|| anyhow!("no preset {name}"))?
            };
            hid.apply_eq(&preset)?;
        }
        // Group and app-group changes take effect on the next sync, which rescales the apps.
        Cmd::GroupVolume(g, v) => {
            settings.group_levels.insert(g, v);
            soft.dirty = true;
        }
        Cmd::GroupMute(g, m) => {
            settings.group_mutes.insert(g, m);
            settings.save();
        }
        Cmd::SetAppGroup(key, g) => {
            settings.app_groups.insert(key, g);
            settings.save();
        }
        Cmd::SetDeviceVolumeMode(on) => {
            settings.use_device_volume = on;
            settings.save();
        }
        Cmd::SetIgnoreKnob(on) => {
            settings.ignore_base_knob = on;
            settings.save();
        }
        Cmd::SetUpdateCheck(on) => {
            settings.skip_update_check = !on;
            settings.save();
        }
    }
    Ok(())
}

/// Collapses a burst of slider moves to the last value per slider, keeping other commands in order.
fn coalesce(cmds: Vec<Cmd>) -> Vec<Cmd> {
    let key = |c: &Cmd| match c {
        Cmd::DeviceVolume(id, _) => Some(format!("d/{id}")),
        Cmd::AppVolume(k, _) => Some(format!("a/{k}")),
        Cmd::Headset(k, _) => Some(format!("h/{k}")),
        Cmd::GroupVolume(g, _) => Some(format!("g/{g}")),
        _ => None,
    };
    let mut out: Vec<Cmd> = Vec::new();
    for c in cmds {
        if let Some(k) = key(&c)
            && let Some(pos) = out.iter().position(|o| key(o).as_deref() == Some(&k))
        {
            out[pos] = c;
            continue;
        }
        out.push(c);
    }
    out
}

/// Owns the audio and HID handles, executes commands and publishes a snapshot every tick.
pub fn worker(rx: Receiver<Cmd>, publish: impl Fn(Snapshot)) {
    let mut audio = match Audio::new() {
        Ok(a) => a,
        Err(e) => {
            publish(Snapshot { error: format!("{e:#}"), ..Default::default() });
            return;
        }
    };
    let mut hid = Headset::new();
    let mut settings = Settings::load();
    let mut snap = Snapshot::default();
    let mut last_sessions: Option<Instant> = None;
    let mut last_headset: Option<Instant> = None;
    let mut last_connected: Option<bool> = None;
    let mut last_hs_settings: Option<Instant> = None;
    let mut save_due: Option<Instant> = None;
    let mut last_knob_poll: Option<Instant> = None;
    let mut eq_name: Option<String> = None;
    let mut last_save: Option<Instant> = None;
    let mut soft = SoftVolume::default();
    let mut error = String::new();

    loop {
        let mut cmds = Vec::new();
        match rx.recv_timeout(TICK) {
            Ok(c) => cmds.push(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        cmds.extend(rx.try_iter());
        let mut changed_default = false;
        for cmd in coalesce(cmds) {
            changed_default |= matches!(cmd, Cmd::SetDefault(_) | Cmd::Output(_) | Cmd::SetAutoSwitch(_));
            let is_headset = matches!(cmd, Cmd::Headset(..) | Cmd::ApplyEq(_));
            match run(&audio, &mut hid, &mut soft, &snap, &mut settings, cmd) {
                Ok(()) => {
                    error.clear();
                    if is_headset {
                        save_due = Some(Instant::now() + HEADSET_SAVE_DELAY);
                        last_hs_settings = None;
                    }
                }
                Err(e) => error = format!("{e:#}"),
            }
        }
        if save_due.is_some_and(|t| Instant::now() >= t) {
            save_due = None;
            if let Err(e) = hid.save() {
                error = format!("Saving headset settings: {e:#}");
            }
        }

        let refresh_sessions = changed_default || last_sessions.is_none_or(|t| t.elapsed() >= SESSION_REFRESH);
        if refresh_sessions {
            let _ = audio.refresh_sessions();
            last_sessions = Some(Instant::now());
        }
        // Knob events arrive any time; read them every tick so the volume follows the dial.
        hid.drain_events();
        let mut headset = if last_headset.is_none_or(|t| t.elapsed() >= HEADSET_REFRESH) {
            last_headset = Some(Instant::now());
            hid.poll()
        } else {
            snap.headset.clone()
        };
        if let Some(h) = headset.as_mut() {
            h.knob_db = hid.knob_db;
        }
        let connected_now = headset.as_ref().and_then(|h| h.connected) == Some(true);
        // Backup for the knob: the settings block mirrors it, so read it once a second.
        if connected_now && save_due.is_none() && last_knob_poll.is_none_or(|t| t.elapsed() >= HEADSET_REFRESH) {
            last_knob_poll = Some(Instant::now());
            let before = hid.knob_db;
            if hid.read_knob() != before {
                crate::debug_log(&format!("knob poll {:?}", hid.knob_db));
            }
            if let Some(h) = headset.as_mut() {
                h.knob_db = hid.knob_db;
            }
        }
        if connected_now && save_due.is_none() && last_hs_settings.is_none_or(|t| t.elapsed() >= HEADSET_SETTINGS_REFRESH) {
            last_hs_settings = Some(Instant::now());
            hid.read_settings();
            eq_name = hid.read_eq_name().or(eq_name);
        }
        let headset_settings = if connected_now { hid.settings } else { None };

        match audio.state() {
            Ok(state) => {
                let mut fresh = Snapshot {
                    outputs: state.outputs,
                    inputs: state.inputs,
                    apps: state.sessions,
                    headset,
                    headset_settings,
                    eq_name: if connected_now { eq_name.clone() } else { None },
                    settings: settings.clone(),
                    ..Default::default()
                };
                fresh.resolve();
                soft.sync(&audio, &mut fresh, &mut settings);
                if soft.dirty && last_save.is_none_or(|t| t.elapsed() >= SETTINGS_SAVE_INTERVAL) {
                    settings.save();
                    soft.dirty = false;
                    last_save = Some(Instant::now());
                }
                // Auto switch only when the headset powers on or off, so manual choices stick.
                let connected = fresh.headset_connected();
                if settings.auto_switch
                    && connected.is_some()
                    && last_connected.is_some()
                    && connected != last_connected
                    && let Err(e) = switch_output(&audio, &fresh, Target::Auto)
                {
                    error = format!("{e:#}");
                }
                if connected.is_some() {
                    last_connected = connected;
                }
                fresh.error = error.clone();
                snap = fresh.clone();
                publish(fresh);
            }
            Err(e) => publish(Snapshot { error: format!("{e:#}"), settings: settings.clone(), ..Default::default() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_groups() {
        assert_eq!(default_group("discord.exe"), "chat");
        assert_eq!(default_group("telegram.exe"), "chat");
        assert_eq!(default_group("chrome.exe"), "media");
        assert_eq!(default_group("spotify.exe"), "media");
        assert_eq!(default_group("system"), "aux");
        assert_eq!(default_group("dota2.exe"), "game");
    }

    #[test]
    fn group_factor_and_overrides() {
        let mut s = Settings::default();
        assert_eq!(s.group_of("chrome.exe"), "media");
        s.app_groups.insert("chrome.exe".into(), "game".into());
        assert_eq!(s.group_of("chrome.exe"), "game");
        assert_eq!(s.group_factor("game"), 1.0);
        s.group_levels.insert("game".into(), 0.4);
        assert_eq!(s.group_factor("game"), 0.4);
        s.group_mutes.insert("game".into(), true);
        assert_eq!(s.group_factor("game"), 0.0);
    }

    #[test]
    fn coalesce_keeps_last_slider_value_and_order() {
        let cmds = vec![
            Cmd::AppVolume("a".into(), 0.1),
            Cmd::SetDefault("x".into()),
            Cmd::AppVolume("a".into(), 0.5),
            Cmd::AppVolume("b".into(), 0.3),
        ];
        let out = coalesce(cmds);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Cmd::AppVolume(k, v) if k == "a" && *v == 0.5));
        assert!(matches!(&out[1], Cmd::SetDefault(_)));
        assert!(matches!(&out[2], Cmd::AppVolume(k, _) if k == "b"));
    }

    fn snapshot_with(knob: Option<u8>, out_volume: f32, use_device_volume: bool) -> Snapshot {
        let mut snap = Snapshot {
            outputs: vec![Device {
                id: "hs".into(),
                name: "Headphones (Arctis Nova Elite)".into(),
                is_default: true,
                volume: out_volume,
                hw_volume: true,
                ..Default::default()
            }],
            headset: Some(headset::Status { knob_db: knob, model: "Arctis Nova Elite".into(), ..Default::default() }),
            ..Default::default()
        };
        snap.settings.use_device_volume = use_device_volume;
        snap.resolve();
        snap
    }

    #[test]
    fn master_combines_endpoint_and_knob() {
        // Headset output: Windows volume times the knob (-20 dB = 0.1).
        let m = SoftVolume::master(&snapshot_with(Some(20), 0.5, false)).unwrap();
        assert!((m - 0.05).abs() < 1e-4);
        // The knob still applies when the headset handles its own volume.
        let m = SoftVolume::master(&snapshot_with(Some(20), 0.5, true)).unwrap();
        assert!((m - 0.1).abs() < 1e-4);
        // Knob at the top and device volume mode: nothing to scale except the knob factor 1.
        assert_eq!(SoftVolume::master(&snapshot_with(Some(0), 1.0, true)), Some(1.0));
    }
}
