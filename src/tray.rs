//! System tray icon and menu (Windows and macOS).

use std::sync::mpsc::Sender;

use crate::AppWindow;
use crate::deck::{Cmd, Target};

/// What the tray reflects from the latest snapshot.
pub struct TrayState<'a> {
    pub target: Option<Target>,
    pub headset_name: &'a str,
    pub speaker_name: &'a str,
    pub auto_switch: bool,
    pub launch_at_login: bool,
    pub connected: bool,
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use std::cell::RefCell;
    use std::time::Duration;

    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::*;

    struct Tray {
        icon: TrayIcon,
        headset: CheckMenuItem,
        speakers: CheckMenuItem,
        auto: CheckMenuItem,
        login: CheckMenuItem,
        _poll: slint::Timer,
    }

    thread_local! {
        static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    }

    pub fn active() -> bool {
        TRAY.with(|t| t.borrow().is_some())
    }

    /// Creates the tray icon. Must run on the UI thread once the event loop is running.
    pub fn install(ui: slint::Weak<AppWindow>, tx: Sender<Cmd>) -> anyhow::Result<()> {
        let icon = Icon::from_rgba(include_bytes!("../assets/tray.rgba").to_vec(), 32, 32)?;
        let open = MenuItem::new("Open Sonar Deck", true, None);
        let headset = CheckMenuItem::new("Headset", true, false, None);
        let speakers = CheckMenuItem::new("Speakers", true, false, None);
        let auto = CheckMenuItem::new("Auto switch", true, false, None);
        let login = CheckMenuItem::new("Launch at login", true, crate::autostart::is_enabled(), None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &headset,
            &speakers,
            &auto,
            &PredefinedMenuItem::separator(),
            &login,
            &quit,
        ])?;
        let icon = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("Sonar Deck")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        let ids =
            (open.id().clone(), headset.id().clone(), speakers.id().clone(), auto.id().clone(), login.id().clone(), quit.id().clone());
        let poll = slint::Timer::default();
        poll.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
            while let Ok(e) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e
                    && let Some(ui) = ui.upgrade()
                {
                    crate::show_window(&ui);
                }
            }
            while let Ok(e) = MenuEvent::receiver().try_recv() {
                let (open, headset, speakers, auto, login, quit) = &ids;
                if e.id == *open {
                    if let Some(ui) = ui.upgrade() {
                        crate::show_window(&ui);
                    }
                } else if e.id == *headset {
                    let _ = tx.send(Cmd::Output(Target::Headset));
                } else if e.id == *speakers {
                    let _ = tx.send(Cmd::Output(Target::Speakers));
                } else if e.id == *auto {
                    let on = TRAY.with(|t| t.borrow().as_ref().is_some_and(|t| t.auto.is_checked()));
                    let _ = tx.send(Cmd::SetAutoSwitch(on));
                } else if e.id == *login {
                    let on = TRAY.with(|t| t.borrow().as_ref().is_some_and(|t| t.login.is_checked()));
                    if let Some(ui) = ui.upgrade() {
                        crate::set_launch_at_login(&ui, on);
                    }
                } else if e.id == *quit {
                    let _ = slint::quit_event_loop();
                }
            }
        });

        TRAY.with(|t| *t.borrow_mut() = Some(Tray { icon, headset, speakers, auto, login, _poll: poll }));
        Ok(())
    }

    pub fn update(s: &TrayState) {
        TRAY.with(|t| {
            let t = t.borrow();
            let Some(t) = t.as_ref() else { return };
            t.headset.set_text(format!("Headset: {}", s.headset_name));
            t.speakers.set_text(format!("Speakers: {}", s.speaker_name));
            t.headset.set_checked(s.target == Some(Target::Headset));
            t.speakers.set_checked(s.target == Some(Target::Speakers));
            t.auto.set_checked(s.auto_switch);
            t.login.set_checked(s.launch_at_login);
            let now = match s.target {
                _ if !s.connected => "Sonar offline".to_string(),
                Some(Target::Headset) => format!("Output: {}", s.headset_name),
                Some(Target::Speakers) => format!("Output: {}", s.speaker_name),
                _ => "Output: custom routing".to_string(),
            };
            let _ = t.icon.set_tooltip(Some(format!("Sonar Deck\n{now}")));
        });
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;

    pub fn active() -> bool {
        false
    }
    pub fn install(_ui: slint::Weak<AppWindow>, _tx: Sender<Cmd>) -> anyhow::Result<()> {
        anyhow::bail!("tray icons are not supported on this platform")
    }
    pub fn update(_s: &TrayState) {}
}

pub use imp::{active, install, update};
