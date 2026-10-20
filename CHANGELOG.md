# Changelog

## Unreleased

- Removed the headset mic-mute flag from `status`: the Nova Elite doesn't report its mute button over USB.
- `sonar-deck hidwatch [seconds]` prints the headset's raw HID traffic, for finding out what a button does.

## 0.1.2

- **Virtual microphone** (Mic page): mixes your mic with desktop audio and a voice changer into VB-CABLE, so
  apps can use "CABLE Output" as their microphone.
- **Mic processing**, replacing SteelSeries Sonar's mic chain: DeepFilterNet3 AI noise suppression (like
  ClearCast), voice presets (Natural, Clear = Sonar's "Less Nasal", Broadcast), leveler, de-esser, rumble and
  pop filter, noise gate and a -1 dBFS limiter. Only the noise model adds delay (about 30 ms, or 10 ms with
  the optional low-latency model).
- **Listen mode**: hear the final mic live, or record 5 s and play it back; Bypass for A/B comparisons.
- Command line: `sonar-deck default <device>` sets the Windows default device; `sonar-deck micfx in.wav
  out.wav` runs the mic processing over a recording.
- Prefers the stereo "CABLE Input" over VB-CABLE's 16-channel variant.

## 0.1.1

- The exe no longer needs the Visual C++ Redistributable (the C runtime is linked statically).
- The installer writes the launch-at-login entry itself instead of running the app, so silent installs
  (winget) can't stall.

## 0.1.0

First public release.

- Headset/speaker quick switch (window, tray, Ctrl+Alt+F12, command line) for output and microphone.
- Mixer with Game, Chat, Media and Aux groups; per-app volume, mute and level meters; device page.
- SteelSeries Arctis Nova Elite support without SteelSeries GG: auto switch on headset power, battery levels,
  base station knob as master volume, software volume, 383 onboard EQ presets, and headset/base settings.
- Tray icon, launch at login, single instance, daily update check.
