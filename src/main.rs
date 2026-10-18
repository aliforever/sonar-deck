#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod deck;
mod headset;
mod instance;
mod tray;
mod update;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use slint::{Color, Model, ModelRc, SharedString, VecModel};

use deck::{Cmd, Role, Snapshot, Target};

slint::include_modules!();

/// Accent colors handed out to app strips.
const PALETTE: [u32; 6] = [0xffff7a1a, 0xff33d6a6, 0xff4da3ff, 0xffb36bff, 0xffffc53d, 0xff4dd9ff];
const DEVICE_STRIP: &str = "device";

#[derive(Default)]
struct UiState {
    snap: Snapshot,
    /// Last time a slider moved; fresh snapshots don't overwrite levels right after.
    touched: Option<Instant>,
    /// Smoothed meter values, so peaks fall off gently.
    meters: HashMap<String, f32>,
    /// Last time a headset setting changed; the device needs a moment before it reports it.
    hs_touched: Option<Instant>,
    eq_filter: String,
    eq_touched: Option<Instant>,
    /// EQ name the preset list was last built for.
    eq_shown: Option<String>,
}

/// Fills the EQ preset list, filtered by the search text, current preset first.
fn show_eq(ui: &AppWindow, st: &UiState) {
    let current = ui.get_eq_current().to_string();
    let needle = st.eq_filter.to_lowercase();
    let mut items: Vec<EqItem> = headset::eq_presets()
        .iter()
        .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
        .map(|p| EqItem { name: p.name.as_str().into(), active: p.name == current })
        .collect();
    items.sort_by_key(|e| !e.active);
    ui.set_eq_count(format!("{} presets", items.len()).into());
    sync(&ui.get_eq_items(), items);
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::new(UiState::default());
}

/// "Speakers (Realtek USB Audio)" -> "Speakers", "Headphones (Arctis Nova Elite)" -> "Arctis Nova Elite".
fn short_name(name: &str) -> String {
    let (head, tail) = match name.split_once(" (") {
        Some((h, t)) => (h, t.trim_end_matches(')')),
        None => return name.to_string(),
    };
    if ["Headphones", "Headset", "Headset Earphone", "Microphone", "Headset Microphone"].contains(&head) {
        tail.to_string()
    } else {
        head.to_string()
    }
}

fn accent(key: &str) -> Color {
    let h = key.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    Color::from_argb_encoded(PALETTE[(h as usize) % PALETTE.len()])
}

/// Updates a VecModel in place so ComboBoxes and lists keep their state between polls.
fn sync<T: Clone + PartialEq + 'static>(model: &ModelRc<T>, items: Vec<T>) {
    let Some(vm) = model.as_any().downcast_ref::<VecModel<T>>() else { return };
    if vm.row_count() == items.len() {
        for (i, item) in items.into_iter().enumerate() {
            if vm.row_data(i).as_ref() != Some(&item) {
                vm.set_row_data(i, item);
            }
        }
    } else {
        vm.set_vec(items);
    }
}

fn meter(st: &mut UiState, key: &str, peak: f32) -> f32 {
    let prev = st.meters.get(key).copied().unwrap_or(0.0);
    let v = peak.max(prev * 0.78);
    st.meters.insert(key.to_string(), v);
    // Peak amplitude reads low for normal audio; a gentle curve makes the meter lively.
    v.sqrt()
}

fn apply(ui: &AppWindow, snap: Snapshot) {
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let hold = st.touched.is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        ui.set_error_text(snap.error.as_str().into());

        let out = snap.default_output().cloned().unwrap_or_default();
        let mic = snap.inputs.iter().find(|d| d.is_default).cloned().unwrap_or_default();
        ui.set_output_name(short_name(&out.name).into());

        // Mixer strips: the default output device, then every app with audio.
        let current: Vec<Strip> = ui.get_strips().iter().collect();
        let held = |key: &str, v: f32| if hold { current.iter().find(|s| s.key == key).map_or(v, |s| s.volume) } else { v };
        let mut strips = vec![Strip {
            key: DEVICE_STRIP.into(),
            name: short_name(&out.name).into(),
            initial: "".into(),
            accent: Color::from_argb_encoded(0xffeef1f7),
            volume: held(DEVICE_STRIP, out.volume),
            muted: out.muted,
            level: meter(&mut st, DEVICE_STRIP, out.peak),
            is_device: true,
            active: true,
            detail: "".into(),
        }];
        let mut apps = snap.apps.clone();
        apps.sort_by_key(|a| (a.key == "system", !a.active, a.name.to_lowercase()));
        // Group strips (Game/Chat/Media/Aux), like Sonar's channels.
        const GROUP_COLORS: [u32; 4] = [0xffff7a1a, 0xff33d6a6, 0xff4da3ff, 0xffb36bff];
        for (i, (id, label)) in deck::GROUPS.iter().enumerate() {
            let members: Vec<&audio::Session> = apps.iter().filter(|a| snap.settings.group_of(&a.key) == *id).collect();
            let peak = members.iter().filter(|a| !a.muted).map(|a| a.peak).fold(0.0, f32::max);
            let names: Vec<&str> = members.iter().map(|a| a.name.as_str()).collect();
            strips.push(Strip {
                key: (*id).into(),
                name: (*label).into(),
                initial: "".into(),
                accent: Color::from_argb_encoded(GROUP_COLORS[i]),
                volume: held(id, snap.settings.group_levels.get(*id).copied().unwrap_or(1.0)),
                muted: snap.settings.group_mutes.get(*id).copied().unwrap_or(false),
                level: meter(&mut st, &format!("group:{id}"), peak),
                is_device: false,
                active: members.iter().any(|a| a.active),
                detail: if names.is_empty() { "No apps".into() } else { names.join(", ").into() },
            });
        }
        // Apps page
        let current_apps: Vec<AppItem> = ui.get_apps().iter().collect();
        let items: Vec<AppItem> = apps
            .iter()
            .map(|a| {
                let group = deck::GROUPS.iter().position(|(id, _)| *id == snap.settings.group_of(&a.key)).unwrap_or(0) as i32;
                let volume = if hold { current_apps.iter().find(|x| x.key == a.key).map_or(a.volume, |x| x.volume) } else { a.volume };
                AppItem {
                    key: a.key.as_str().into(),
                    name: a.name.as_str().into(),
                    initial: a.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                    accent: accent(&a.key),
                    group,
                    volume,
                    muted: a.muted,
                    level: meter(&mut st, &a.key, a.peak),
                    active: a.active,
                }
            })
            .collect();
        sync(&ui.get_apps(), items);
        sync(&ui.get_strips(), strips);

        if !hold {
            ui.set_mic_volume(mic.volume);
        }
        ui.set_mic_name(mic.name.as_str().into());
        ui.set_mic_muted(mic.muted);
        let mic_level = meter(&mut st, "mic", mic.peak);
        ui.set_mic_level(mic_level);

        // Quick switch
        let name_of = |id: &str| snap.outputs.iter().find(|d| d.id == id).map(|d| short_name(&d.name));
        let connected = snap.headset_connected();
        ui.set_headset_name(
            name_of(&snap.headset_out).or(snap.headset.as_ref().map(|h| h.model.clone())).unwrap_or("Headset".into()).into(),
        );
        ui.set_speaker_name(name_of(&snap.speaker_out).unwrap_or("Speakers".into()).into());
        ui.set_quick_target(
            match snap.current_target() {
                Some(Target::Headset) => "headset",
                Some(Target::Speakers) => "speakers",
                _ => "",
            }
            .into(),
        );
        ui.set_headset_present(snap.headset.is_some());
        ui.set_headset_state(
            match (connected, snap.headset.as_ref().and_then(|h| h.battery)) {
                (Some(true), Some(b)) => format!("HEADSET ON · {b}%"),
                (Some(true), None) => "HEADSET ON".into(),
                (Some(false), _) => "HEADSET OFF".into(),
                (None, _) => "DONGLE CONNECTED".into(),
            }
            .into(),
        );
        ui.set_headset_on(connected == Some(true));
        ui.set_auto_available(connected.is_some());
        ui.set_auto_switch(snap.settings.auto_switch);

        // Devices
        let devs = |list: &[audio::Device], st: &mut UiState| -> Vec<Dev> {
            list.iter()
                .map(|d| Dev {
                    id: d.id.as_str().into(),
                    name: d.name.as_str().into(),
                    is_default: d.is_default,
                    volume: d.volume,
                    muted: d.muted,
                    level: meter(st, &d.id, d.peak),
                })
                .collect()
        };
        let outputs = devs(&snap.outputs, &mut st);
        let inputs = devs(&snap.inputs, &mut st);
        if !hold {
            sync(&ui.get_outputs(), outputs);
            sync(&ui.get_inputs(), inputs);
        }

        // Settings
        let names = |list: &[audio::Device]| list.iter().map(|d| SharedString::from(d.name.as_str())).collect::<Vec<_>>();
        sync(&ui.get_output_names(), names(&snap.outputs));
        sync(&ui.get_input_names(), names(&snap.inputs));
        let mut choices = vec![SharedString::from("Don't change")];
        choices.extend(names(&snap.inputs));
        sync(&ui.get_mic_choices(), choices);
        let index = |list: &[audio::Device], id: &str| list.iter().position(|d| d.id == id).map_or(-1, |i| i as i32);
        ui.set_headset_out_index(index(&snap.outputs, &snap.headset_out));
        ui.set_speaker_out_index(index(&snap.outputs, &snap.speaker_out));
        ui.set_headset_mic_index(index(&snap.inputs, &snap.headset_mic));
        ui.set_speaker_mic_index(index(&snap.inputs, &snap.speaker_mic) + 1);
        ui.set_headset_raw(
            snap.headset
                .as_ref()
                .map(|h| {
                    let mut parts = vec![h.model.clone()];
                    match (h.connected, h.battery) {
                        (Some(true), Some(b)) => parts.push(format!("headset {b}% ({})", h.charging.unwrap_or("unknown"))),
                        (Some(false), _) => parts.push("headset off".into()),
                        _ => {}
                    }
                    if let Some(b) = h.spare_battery {
                        parts.push(format!("spare battery {b}%"));
                    }
                    parts.join(" · ")
                })
                .unwrap_or_default()
                .into(),
        );

        // Headset settings (held briefly after a change so the UI doesn't flick back).
        ui.set_use_device_volume(snap.settings.use_device_volume);
        ui.set_use_base_knob(!snap.settings.ignore_base_knob);
        ui.set_update_check(!snap.settings.skip_update_check);
        ui.set_software_volume(snap.software_volume);
        ui.set_base_knob(
            snap.headset
                .as_ref()
                .and_then(|h| h.knob_db)
                .filter(|_| snap.software_volume && !snap.settings.ignore_base_knob)
                .map(|db| if db == 0 { "Base 0 dB".to_string() } else { format!("Base −{db} dB") })
                .unwrap_or_default()
                .into(),
        );
        ui.set_hs_available(snap.headset_settings.is_some());
        ui.set_hs_battery(
            snap.headset
                .as_ref()
                .and_then(|h| {
                    h.battery.map(|b| match h.spare_battery {
                        Some(s) => format!("{b}% · spare {s}%"),
                        None => format!("{b}%"),
                    })
                })
                .unwrap_or_default()
                .into(),
        );
        let hs_hold = st.hs_touched.is_some_and(|t| t.elapsed() < Duration::from_millis(2500));
        if let (Some(h), false) = (snap.headset_settings, hs_hold) {
            ui.set_hs(Hs {
                anc_mode: h.anc_mode as i32,
                anc_level: h.anc_level as i32,
                transparency: h.transparency as i32,
                mic_volume: h.mic_volume as i32,
                boom_sidetone: h.boom_sidetone.0,
                boom_sidetone_level: h.boom_sidetone.1 as i32,
                ear_sidetone: h.ear_sidetone.0,
                ear_sidetone_level: h.ear_sidetone.1 as i32,
                volume_limiter: h.volume_limiter,
                nr_boom: h.nr_boom as i32,
                nr_ear: h.nr_ear as i32,
                mic_led: h.mic_led as i32,
                inactivity: h.inactivity as i32,
                bt_power: h.bt_power_default,
                bt_call: h.bt_call as i32,
                oled_brightness: h.oled_brightness as i32,
                oled_saver_timer: h.oled_saver_timer as i32,
                oled_saver_mode: h.oled_saver_mode as i32,
                home_view: h.home_view as i32,
                home_option: h.home_option as i32,
            });
        }

        // Onboard EQ
        if !st.eq_touched.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            ui.set_eq_current(snap.eq_name.clone().unwrap_or_default().into());
        }
        let eq_now = ui.get_eq_current().to_string();
        if st.eq_shown.as_deref() != Some(eq_now.as_str()) {
            st.eq_shown = Some(eq_now);
            show_eq(ui, &st);
        }

        tray::update(&tray::TrayState {
            target: snap.current_target(),
            headset_name: &ui.get_headset_name(),
            speaker_name: &ui.get_speaker_name(),
            auto_switch: snap.settings.auto_switch,
            launch_at_login: ui.get_launch_at_login(),
            connected: true,
        });
        st.snap = snap;
    });
}

/// Appends a line to %TEMP%\sonar-deck.log when SONAR_DECK_LOG is set.
pub fn debug_log(msg: &str) {
    if std::env::var_os("SONAR_DECK_LOG").is_none() {
        return;
    }
    use std::io::Write;
    let path = std::env::temp_dir().join("sonar-deck.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(f, "{}.{:03} {msg}", t.as_secs(), t.subsec_millis());
    }
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Checks GitHub for a newer release at startup and then once a day.
fn spawn_update_check(ui: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        loop {
            if !deck::Settings::load().skip_update_check
                && let Ok((version, url)) = update::latest()
                && update::is_newer(&version, env!("CARGO_PKG_VERSION"))
            {
                let ui = ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.set_update_version(version.into());
                        ui.set_update_url(url.into());
                    }
                });
            }
            std::thread::sleep(Duration::from_secs(24 * 3600));
        }
    });
}

/// Brings the window back from the tray (or from another instance being launched).
pub fn show_window(ui: &AppWindow) {
    let _ = ui.show();
    ui.window().set_minimized(false);
}

pub fn set_launch_at_login(ui: &AppWindow, on: bool) {
    match autostart::set(on) {
        Ok(()) => ui.set_launch_at_login(on),
        Err(e) => {
            ui.set_launch_at_login(autostart::is_enabled());
            ui.set_error_text(format!("Launch at login: {e:#}").into());
        }
    }
}

fn edit<T: Clone + 'static>(model: &ModelRc<T>, find: impl Fn(&T) -> bool, f: impl FnOnce(&mut T)) -> Option<T> {
    let i = model.iter().position(|x| find(&x))?;
    let mut item = model.row_data(i)?;
    f(&mut item);
    model.set_row_data(i, item.clone());
    Some(item)
}

fn default_id(input: bool) -> Option<String> {
    STATE.with(|st| {
        let st = st.borrow();
        let list = if input { &st.snap.inputs } else { &st.snap.outputs };
        list.iter().find(|d| d.is_default).map(|d| d.id.clone())
    })
}

fn wire(ui: &AppWindow, tx: Sender<Cmd>) {
    let send = move |c: Cmd| {
        let _ = tx.send(c);
    };
    let touch = || STATE.with(|st| st.borrow_mut().touched = Some(Instant::now()));

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_strip_volume(move |key, v| {
        touch();
        if let Some(ui) = w.upgrade() {
            edit(&ui.get_strips(), |x| x.key == key, |x| x.volume = v);
        }
        if key == DEVICE_STRIP {
            if let Some(id) = default_id(false) {
                s(Cmd::DeviceVolume(id, v));
            }
        } else {
            s(Cmd::GroupVolume(key.into(), v));
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_strip_mute(move |key| {
        let Some(ui) = w.upgrade() else { return };
        let Some(strip) = edit(&ui.get_strips(), |x| x.key == key, |x| x.muted = !x.muted) else { return };
        if key == DEVICE_STRIP {
            if let Some(id) = default_id(false) {
                s(Cmd::DeviceMute(id, strip.muted));
            }
        } else {
            s(Cmd::GroupMute(key.into(), strip.muted));
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_quick_switch(move |target| {
        if let Some(ui) = w.upgrade() {
            ui.set_quick_target(target.clone());
        }
        s(Cmd::Output(if target == "headset" { Target::Headset } else { Target::Speakers }));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_auto_switch_toggled(move |on| {
        if let Some(ui) = w.upgrade() {
            ui.set_auto_switch(on);
        }
        s(Cmd::SetAutoSwitch(on));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_make_default(move |id, input| {
        if let Some(ui) = w.upgrade() {
            let model = if input { ui.get_inputs() } else { ui.get_outputs() };
            for i in 0..model.row_count() {
                if let Some(mut d) = model.row_data(i) {
                    d.is_default = d.id == id;
                    model.set_row_data(i, d);
                }
            }
        }
        s(Cmd::SetDefault(id.into()));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_device_volume(move |id, input, v| {
        touch();
        if let Some(ui) = w.upgrade() {
            let model = if input { ui.get_inputs() } else { ui.get_outputs() };
            edit(&model, |d| d.id == id, |d| d.volume = v);
        }
        s(Cmd::DeviceVolume(id.into(), v));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_device_mute(move |id, input| {
        let Some(ui) = w.upgrade() else { return };
        let model = if input { ui.get_inputs() } else { ui.get_outputs() };
        if let Some(d) = edit(&model, |d| d.id == id, |d| d.muted = !d.muted) {
            s(Cmd::DeviceMute(id.into(), d.muted));
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_mic_volume_moved(move |v| {
        touch();
        if let Some(ui) = w.upgrade() {
            ui.set_mic_volume(v);
        }
        if let Some(id) = default_id(true) {
            s(Cmd::DeviceVolume(id, v));
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_mic_mute_toggled(move || {
        let Some(ui) = w.upgrade() else { return };
        let muted = !ui.get_mic_muted();
        ui.set_mic_muted(muted);
        if let Some(id) = default_id(true) {
            s(Cmd::DeviceMute(id, muted));
        }
    });

    let s = send.clone();
    ui.on_role_chosen(move |role, index| {
        let (role, input, offset) = match role.as_str() {
            "headset-out" => (Role::HeadsetOut, false, 0),
            "speaker-out" => (Role::SpeakerOut, false, 0),
            "headset-mic" => (Role::HeadsetMic, true, 0),
            _ => (Role::SpeakerMic, true, 1),
        };
        let id = STATE.with(|st| {
            let st = st.borrow();
            let list = if input { &st.snap.inputs } else { &st.snap.outputs };
            usize::try_from(index - offset).ok().and_then(|i| list.get(i)).map(|d| d.id.clone()).unwrap_or_default()
        });
        s(Cmd::SetRole(role, id));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_hs_set(move |key, value| {
        STATE.with(|st| st.borrow_mut().hs_touched = Some(Instant::now()));
        if let Some(ui) = w.upgrade() {
            let mut h = ui.get_hs();
            match key.as_str() {
                "anc-mode" => h.anc_mode = value,
                "anc-level" => h.anc_level = value,
                "transparency" => h.transparency = value,
                "mic-volume" => h.mic_volume = value,
                "boom-sidetone" => h.boom_sidetone = value != 0,
                "boom-sidetone-level" => h.boom_sidetone_level = value,
                "ear-sidetone" => h.ear_sidetone = value != 0,
                "ear-sidetone-level" => h.ear_sidetone_level = value,
                "volume-limiter" => h.volume_limiter = value != 0,
                "nr-boom" => h.nr_boom = value,
                "nr-ear" => h.nr_ear = value,
                "mic-led" => h.mic_led = value,
                "inactivity" => h.inactivity = value,
                "bt-power" => h.bt_power = value != 0,
                "bt-call" => h.bt_call = value,
                "oled-brightness" => h.oled_brightness = value,
                "oled-saver-timer" => h.oled_saver_timer = value,
                "oled-saver-mode" => h.oled_saver_mode = value,
                "home-view" => h.home_view = value,
                "home-option" => h.home_option = value,
                _ => {}
            }
            ui.set_hs(h);
        }
        s(Cmd::Headset(key.into(), value.clamp(0, 255) as u8));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_device_volume_mode_toggled(move |on| {
        if let Some(ui) = w.upgrade() {
            ui.set_use_device_volume(on);
        }
        s(Cmd::SetDeviceVolumeMode(on));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_app_volume(move |key, v| {
        touch();
        if let Some(ui) = w.upgrade() {
            edit(&ui.get_apps(), |a| a.key == key, |a| a.volume = v);
        }
        s(Cmd::AppVolume(key.into(), v));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_app_mute(move |key| {
        let Some(ui) = w.upgrade() else { return };
        if let Some(a) = edit(&ui.get_apps(), |a| a.key == key, |a| a.muted = !a.muted) {
            s(Cmd::AppMute(key.into(), a.muted));
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_app_group(move |key, index| {
        let Some((id, _)) = deck::GROUPS.get(index as usize) else { return };
        if let Some(ui) = w.upgrade() {
            edit(&ui.get_apps(), |a| a.key == key, |a| a.group = index);
        }
        s(Cmd::SetAppGroup(key.into(), (*id).into()));
    });

    let w = ui.as_weak();
    ui.on_eq_filter(move |text| {
        if let Some(ui) = w.upgrade() {
            STATE.with(|st| {
                st.borrow_mut().eq_filter = text.into();
                show_eq(&ui, &st.borrow());
            });
        }
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_eq_apply(move |name| {
        if let Some(ui) = w.upgrade() {
            ui.set_eq_current(name.clone());
            STATE.with(|st| {
                st.borrow_mut().eq_touched = Some(Instant::now());
                show_eq(&ui, &st.borrow());
            });
        }
        s(Cmd::ApplyEq(name.into()));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_base_knob_toggled(move |on| {
        if let Some(ui) = w.upgrade() {
            ui.set_use_base_knob(on);
        }
        s(Cmd::SetIgnoreKnob(!on));
    });

    let (s, w) = (send.clone(), ui.as_weak());
    ui.on_update_check_toggled(move |on| {
        if let Some(ui) = w.upgrade() {
            ui.set_update_check(on);
        }
        s(Cmd::SetUpdateCheck(on));
    });

    ui.on_open_url(|url| open_url(&url));

    let w = ui.as_weak();
    ui.on_launch_at_login_toggled(move |on| {
        if let Some(ui) = w.upgrade() {
            set_launch_at_login(&ui, on);
        }
    });
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Release builds use the GUI subsystem; borrow the terminal's console for CLI output.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_console() {}

/// `sonar-deck status|toggle|headset|speakers|auto` or `sonar-deck login [on|off]`
fn cli(command: &str, arg: Option<&str>) -> i32 {
    attach_console();
    if command == "uninstall" {
        // Used by the installer's uninstaller: remove the login entry and the settings folder.
        let _ = autostart::set(false);
        if let Some(dir) = dirs::config_dir().map(|d| d.join("SonarDeck")) {
            let _ = std::fs::remove_dir_all(dir);
        }
        println!("removed launch-at-login entry and settings");
        return 0;
    }
    if command == "login" {
        let result = match arg {
            Some("on") => autostart::set(true),
            Some("off") => autostart::set(false),
            _ => Ok(()),
        };
        return match result {
            Ok(()) => {
                println!("launch at login: {}", if autostart::is_enabled() { "on" } else { "off" });
                0
            }
            Err(e) => {
                eprintln!("sonar-deck: {e:#}");
                1
            }
        };
    }
    if command == "headset" {
        let result = (|| -> anyhow::Result<String> {
            let mut hid = headset::Headset::new();
            let status = hid.poll().ok_or_else(|| anyhow::anyhow!("no supported headset dongle found"))?;
            if status.connected != Some(true) {
                anyhow::bail!("the headset is off");
            }
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let [key, value] = args.as_slice() {
                hid.read_settings();
                hid.apply(key, value.parse()?)?;
                std::thread::sleep(Duration::from_millis(600));
                hid.save()?;
                std::thread::sleep(Duration::from_millis(300));
            }
            let s = hid.read_settings().ok_or_else(|| anyhow::anyhow!("could not read headset settings"))?;
            Ok(format!("{s:#?}\n"))
        })();
        return match result {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(e) => {
                eprintln!("sonar-deck: {e:#}");
                1
            }
        };
    }
    if command == "eq" {
        let result = (|| -> anyhow::Result<String> {
            let mut hid = headset::Headset::new();
            let status = hid.poll().ok_or_else(|| anyhow::anyhow!("no supported headset dongle found"))?;
            if status.connected != Some(true) {
                anyhow::bail!("the headset is off");
            }
            let wanted: Vec<String> = std::env::args().skip(2).collect();
            if !wanted.is_empty() {
                let name = wanted.join(" ");
                let preset = if name.eq_ignore_ascii_case("flat") {
                    headset::flat_preset()
                } else {
                    headset::eq_presets()
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(&name))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no preset named {name:?}"))?
                };
                hid.apply_eq(&preset)?;
                std::thread::sleep(Duration::from_millis(300));
            }
            Ok(format!("current EQ: {}\n", hid.read_eq_name().unwrap_or_else(|| "unknown".into())))
        })();
        return match result {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(e) => {
                eprintln!("sonar-deck: {e:#}");
                1
            }
        };
    }
    if command == "volume" {
        let result = (|| -> anyhow::Result<String> {
            let mut audio = audio::Audio::new()?;
            let state = audio.state()?;
            let out = state.outputs.iter().find(|d| d.is_default).ok_or_else(|| anyhow::anyhow!("no default output"))?;
            if let Some(v) = arg {
                let v: f32 = v.parse()?;
                audio.set_device_volume(&out.id, v / 100.0)?;
            }
            let now = audio.state()?.outputs.into_iter().find(|d| d.is_default).map_or(0.0, |d| d.volume);
            Ok(format!("{}: {:.0}%\n", out.name, now * 100.0))
        })();
        return match result {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(e) => {
                eprintln!("sonar-deck: {e:#}");
                1
            }
        };
    }
    let target = match command {
        "status" => None,
        "toggle" => Some(Target::Toggle),
        "headset" => Some(Target::Headset),
        "speakers" => Some(Target::Speakers),
        "auto" => Some(Target::Auto),
        _ => {
            eprintln!(
                "usage: sonar-deck [status|toggle|headset|speakers|auto|volume [0-100]|headset [key value]|eq [preset]|login [on|off]|uninstall|--version]"
            );
            return 2;
        }
    };
    let result = (|| -> anyhow::Result<String> {
        let settings = deck::Settings::load();
        let mut audio = audio::Audio::new()?;
        let mut hid = headset::Headset::new();
        let mut snap = deck::read(&mut audio, &mut hid, &settings, target.is_none())?;
        if let Some(t) = target {
            deck::switch_output(&audio, &snap, t)?;
            snap = deck::read(&mut audio, &mut hid, &settings, false)?;
        }
        let mut out = String::new();
        for d in &snap.outputs {
            out += &format!(
                "output {} {} ({:.0}%{}{})\n",
                if d.is_default { "*" } else { " " },
                d.name,
                d.volume * 100.0,
                if d.muted { ", muted" } else { "" },
                if d.hw_volume { ", hardware volume" } else { "" }
            );
        }
        for d in &snap.inputs {
            out += &format!(
                "input  {} {} ({:.0}%{})\n",
                if d.is_default { "*" } else { " " },
                d.name,
                d.volume * 100.0,
                if d.muted { ", muted" } else { "" }
            );
        }
        for a in &snap.apps {
            out += &format!("app      {} ({:.0}%{})\n", a.name, a.volume * 100.0, if a.muted { ", muted" } else { "" });
        }
        if let Some(h) = &snap.headset {
            out += &format!(
                "headset  {} power={}\n",
                h.model,
                h.connected.map_or("unknown".into(), |c| if c { "on".to_string() } else { "off".to_string() })
            );
        }
        Ok(out)
    })();
    match result {
        Ok(s) => {
            print!("{s}");
            0
        }
        Err(e) => {
            eprintln!("sonar-deck: {e:#}");
            1
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        attach_console();
        println!("sonar-deck {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| !a.starts_with("--")) {
        std::process::exit(cli(&args[i], args.get(i + 1).map(String::as_str)));
    }
    let start_hidden = args.iter().any(|a| a == "--tray");

    // A second launch just brings the running window forward.
    let Some(listener) = instance::acquire() else { return Ok(()) };

    let ui = AppWindow::new()?;
    ui.set_strips(ModelRc::new(VecModel::<Strip>::default()));
    ui.set_outputs(ModelRc::new(VecModel::<Dev>::default()));
    ui.set_inputs(ModelRc::new(VecModel::<Dev>::default()));
    ui.set_output_names(ModelRc::new(VecModel::<SharedString>::default()));
    ui.set_input_names(ModelRc::new(VecModel::<SharedString>::default()));
    ui.set_mic_choices(ModelRc::new(VecModel::<SharedString>::default()));
    ui.set_apps(ModelRc::new(VecModel::<AppItem>::default()));
    ui.set_eq_items(ModelRc::new(VecModel::<EqItem>::default()));
    ui.set_launch_at_login(autostart::is_enabled());
    // Open on a given page (0 Mixer, 1 Apps, 2 Headset, 3 Devices, 4 Settings).
    if let Some(page) = std::env::var("SONAR_DECK_PAGE").ok().and_then(|p| p.parse().ok()) {
        ui.set_page(page);
    }
    ui.window().set_size(slint::LogicalSize::new(1240.0, 800.0));

    let (tx, rx) = mpsc::channel();
    wire(&ui, tx.clone());

    let weak = ui.as_weak();
    std::thread::spawn(move || {
        deck::worker(rx, move |snap| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply(&ui, snap);
                }
            });
        })
    });

    spawn_update_check(ui.as_weak());

    if let Some(listener) = listener {
        let weak = ui.as_weak();
        instance::serve(listener, move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    show_window(&ui);
                }
            });
        });
    }

    // The tray needs a running event loop (macOS), so create it on the first tick.
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::ZERO, move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Err(e) = tray::install(ui.as_weak(), tx) {
            eprintln!("sonar-deck: no tray icon: {e:#}");
            show_window(&ui);
        }
    });

    // Closing the window keeps Sonar Deck in the tray; without a tray it quits.
    ui.window().on_close_requested(|| {
        if !tray::active() {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    if !start_hidden {
        ui.show()?;
    }
    slint::run_event_loop_until_quit()
}

#[cfg(test)]
mod tests {
    use super::short_name;

    #[test]
    fn short_device_names() {
        assert_eq!(short_name("Speakers (Realtek USB Audio)"), "Speakers");
        assert_eq!(short_name("Headphones (Arctis Nova Elite)"), "Arctis Nova Elite");
        assert_eq!(short_name("Microphone (Arctis Nova Elite)"), "Arctis Nova Elite");
        assert_eq!(short_name("Line Out"), "Line Out");
    }
}
