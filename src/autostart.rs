//! Launch at login (HKCU Run key on Windows, a LaunchAgent on macOS, an autostart entry on Linux).

use anyhow::{Result, anyhow};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

fn launcher() -> Result<AutoLaunch> {
    let exe = std::env::current_exe()?;
    let path = exe.to_str().ok_or_else(|| anyhow!("executable path is not valid UTF-8"))?;
    Ok(AutoLaunchBuilder::new().set_app_name("Sonar Deck").set_app_path(path).set_args(&["--tray"]).build()?)
}

pub fn is_enabled() -> bool {
    launcher().and_then(|l| Ok(l.is_enabled()?)).unwrap_or(false)
}

pub fn set(enabled: bool) -> Result<()> {
    let l = launcher()?;
    if enabled {
        l.enable()?
    } else {
        l.disable()?
    }
    Ok(())
}
