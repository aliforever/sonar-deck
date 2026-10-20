//! Mic Mixer: mixes a microphone, desktop audio (loopback capture of an output device) and a voice
//! changer into a virtual cable such as VB-CABLE. Other apps then use the cable's capture side
//! ("CABLE Output") as their microphone.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};

/// Sources, in UI order.
pub const MIC: usize = 0;
pub const DESKTOP: usize = 1;
pub const CHANGER: usize = 2;
const SOURCES: usize = 3;

/// Audio buffered per source before older samples are dropped, in seconds. Keeps latency low.
const MAX_BUFFER: f32 = 0.06;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    /// Device friendly name; empty = off.
    pub device: String,
    /// Capture what an output device plays (loopback) instead of an input device.
    pub loopback: bool,
    /// Linear gain, 0..2.
    pub gain: f32,
    pub muted: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self { device: String::new(), loopback: false, gain: 1.0, muted: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    /// Output (render) device the mix is played into, e.g. "CABLE Input (VB-Audio Virtual Cable)".
    pub output: String,
    pub sources: [Source; SOURCES],
    /// Processing for the microphone source.
    pub fx: crate::micfx::Fx,
}

impl Config {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("SonarDeck").join("mic.json"))
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

/// Values shared lock-free between the UI and the audio callbacks.
#[derive(Default)]
pub struct Shared {
    gains: [AtomicU32; SOURCES],
    mutes: [AtomicBool; SOURCES],
    /// Peak levels: one per source plus the mixed output (index 3).
    peaks: [AtomicU32; SOURCES + 1],
    pub running: AtomicBool,
    pub status: Mutex<String>,
    pub fx: crate::micfx::Live,
    pub monitor: Monitor,
}

/// Length of the record-and-play-back test, in seconds.
pub const TEST_SECS: f32 = 5.0;

/// Listen mode: plays the finished mix (what apps get from the cable) to the default output.
#[derive(Default)]
pub struct Monitor {
    /// Live listening.
    pub live: AtomicBool,
    /// 0 idle, 1 recording a test clip, 2 playing it back.
    pub test: AtomicU8,
    /// Stereo frames at the mix rate, waiting to be played.
    queue: Mutex<VecDeque<f32>>,
    clip: Mutex<Vec<f32>>,
    /// Sources left out of what you hear because they record the device you listen on.
    excluded: [AtomicBool; SOURCES],
    rate: AtomicU32,
}

impl Monitor {
    pub fn start_test(&self) {
        self.clip.lock().unwrap().clear();
        self.queue.lock().unwrap().clear();
        self.test.store(1, Ordering::Relaxed);
    }
    fn wanted(&self) -> bool {
        self.live.load(Ordering::Relaxed) || self.test.load(Ordering::Relaxed) != 0
    }
    /// Takes one callback's worth of mixed frames from the mix output.
    fn feed(&self, frames: &[[f32; 2]]) {
        match self.test.load(Ordering::Relaxed) {
            1 => {
                let mut clip = self.clip.lock().unwrap();
                clip.extend(frames.iter().flatten());
                if clip.len() as f32 >= TEST_SECS * self.rate.load(Ordering::Relaxed) as f32 * 2.0 {
                    self.queue.lock().unwrap().extend(clip.drain(..));
                    self.test.store(2, Ordering::Relaxed);
                }
            }
            0 if self.live.load(Ordering::Relaxed) => {
                let max = (self.rate.load(Ordering::Relaxed) as f32 * MAX_BUFFER) as usize * 2;
                let mut q = self.queue.lock().unwrap();
                q.extend(frames.iter().flatten());
                while q.len() > max {
                    q.pop_front();
                }
            }
            _ => {}
        }
    }
}

impl Shared {
    fn gain(&self, i: usize) -> f32 {
        if self.mutes[i].load(Ordering::Relaxed) { 0.0 } else { f32::from_bits(self.gains[i].load(Ordering::Relaxed)) }
    }
    fn set_peak(&self, i: usize, v: f32) {
        // Keep the highest value until the UI reads it.
        let cur = f32::from_bits(self.peaks[i].load(Ordering::Relaxed));
        if v > cur {
            self.peaks[i].store(v.to_bits(), Ordering::Relaxed);
        }
    }
    /// Returns and resets the peak since the last call (index 3 = output).
    pub fn take_peak(&self, i: usize) -> f32 {
        f32::from_bits(self.peaks[i].swap(0f32.to_bits(), Ordering::Relaxed))
    }
    pub fn apply_levels(&self, cfg: &Config) {
        for (i, s) in cfg.sources.iter().enumerate() {
            self.gains[i].store(s.gain.to_bits(), Ordering::Relaxed);
            self.mutes[i].store(s.muted, Ordering::Relaxed);
        }
        self.fx.set(&cfg.fx);
    }
}

/// Linear resampler for interleaved stereo frames.
struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, prev: [0.0; 2] }
    }
    fn push(&mut self, cur: [f32; 2], out: &mut VecDeque<f32>) {
        while self.pos < 1.0 {
            let t = self.pos as f32;
            out.push_back(self.prev[0] + (cur[0] - self.prev[0]) * t);
            out.push_back(self.prev[1] + (cur[1] - self.prev[1]) * t);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = cur;
    }
}

/// Linear resampler for mono audio, used to bring the mic to the noise model's 48 kHz.
struct MonoResampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl MonoResampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, prev: 0.0 }
    }
    fn push(&mut self, cur: f32, out: &mut Vec<f32>) {
        while self.pos < 1.0 {
            out.push(self.prev + (cur - self.prev) * self.pos as f32);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = cur;
    }
}

type Buffer = Arc<Mutex<VecDeque<f32>>>;

/// Runs the mic chain off the audio thread: takes mono blocks at `in_rate` and pushes processed
/// stereo at `rate` into `buf`. Ends when the sending side (the input stream) is dropped.
fn spawn_mic_worker(rx: Receiver<Vec<f32>>, in_rate: u32, rate: u32, buf: Buffer, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut chain = crate::micfx::Chain::new();
        let mut to_model = (in_rate != crate::micfx::RATE).then(|| MonoResampler::new(in_rate, crate::micfx::RATE));
        let mut to_mix = Resampler::new(crate::micfx::RATE, rate);
        let max = (rate as f32 * MAX_BUFFER) as usize * 2;
        let mut block = Vec::new();
        while let Ok(first) = rx.recv() {
            // Catch up in one go if we fell behind (e.g. while the model loaded), keeping latency low.
            let mut input = first;
            while let Ok(more) = rx.try_recv() {
                input.extend(more);
            }
            let keep = (in_rate as f32 * MAX_BUFFER) as usize;
            if input.len() > keep * 2 {
                input.drain(..input.len() - keep);
            }
            let input = match &mut to_model {
                Some(rs) => {
                    block.clear();
                    for &s in &input {
                        rs.push(s, &mut block);
                    }
                    &block
                }
                None => &input,
            };
            chain.process(input, &shared.fx, &mut |out| {
                let mut q = buf.lock().unwrap();
                for &s in out {
                    to_mix.push([s, s], &mut q);
                }
                while q.len() > max {
                    q.pop_front();
                }
            });
        }
    });
}

fn find_device(host: &cpal::Host, name: &str, output: bool) -> Result<cpal::Device> {
    let mut devices = if output { host.output_devices()? } else { host.input_devices()? };
    devices.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false)).ok_or_else(|| anyhow!("device not found: {name}"))
}

/// Opens one source and feeds its audio, converted to stereo at `rate`, into `buf`.
fn open_source(host: &cpal::Host, idx: usize, src: &Source, rate: u32, buf: Buffer, shared: Arc<Shared>) -> Result<cpal::Stream> {
    let dev = find_device(host, &src.device, src.loopback)?;
    let supported = if src.loopback { dev.default_output_config()? } else { dev.default_input_config()? };
    let cfg = supported.config();
    let channels = cfg.channels as usize;
    let max = (rate as f32 * MAX_BUFFER) as usize * 2;
    let mut rs = Resampler::new(cfg.sample_rate, rate);
    let err = |e| crate::debug_log(&format!("mic mixer source error: {e}"));
    // The microphone goes through the processing chain on its own thread.
    let to_fx = (idx == MIC).then(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        spawn_mic_worker(rx, cfg.sample_rate, rate, buf.clone(), shared.clone());
        tx
    });
    let mut handle = move |frame_iter: &mut dyn Iterator<Item = [f32; 2]>| {
        let mut peak = 0f32;
        if let Some(tx) = &to_fx {
            let mono: Vec<f32> = frame_iter.map(|f| (f[0] + f[1]) * 0.5).inspect(|v| peak = peak.max(v.abs())).collect();
            let _ = tx.send(mono);
        } else {
            let mut q = buf.lock().unwrap();
            for f in frame_iter {
                peak = peak.max(f[0].abs()).max(f[1].abs());
                rs.push(f, &mut q);
            }
            while q.len() > max {
                q.pop_front();
            }
        }
        shared.set_peak(idx, peak);
    };
    let stereo = move |c: &[f32]| if channels == 1 { [c[0], c[0]] } else { [c[0], c[1]] };
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            dev.build_input_stream(cfg, move |data: &[f32], _: &_| handle(&mut data.chunks_exact(channels).map(stereo)), err, None)?
        }
        cpal::SampleFormat::I16 => dev.build_input_stream(
            cfg,
            move |data: &[i16], _: &_| {
                let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                handle(&mut f.chunks_exact(channels).map(stereo))
            },
            err,
            None,
        )?,
        f => return Err(anyhow!("unsupported sample format {f:?} on {}", src.device)),
    };
    stream.play()?;
    Ok(stream)
}

/// Builds the whole graph for `cfg`. The returned streams run until dropped.
fn start(cfg: &Config, shared: &Arc<Shared>) -> Result<Vec<cpal::Stream>> {
    let host = cpal::default_host();
    let out_dev = find_device(&host, &cfg.output, true).context("mix output")?;
    let supported = out_dev.default_output_config()?;
    let out_cfg = supported.config();
    let rate = out_cfg.sample_rate;
    let out_channels = out_cfg.channels as usize;

    let mut streams = Vec::new();
    let mut buffers: Vec<(usize, Buffer)> = Vec::new();
    for (i, src) in cfg.sources.iter().enumerate() {
        if src.device.is_empty() {
            continue;
        }
        // Never record the mix output itself, which would feed back.
        if src.loopback && src.device == cfg.output {
            continue;
        }
        let buf: Buffer = Arc::new(Mutex::new(VecDeque::new()));
        match open_source(&host, i, src, rate, buf.clone(), shared.clone()) {
            Ok(s) => {
                streams.push(s);
                buffers.push((i, buf));
            }
            Err(e) => *shared.status.lock().unwrap() = format!("{}: {e:#}", src.device),
        }
    }

    shared.monitor.rate.store(rate, Ordering::Relaxed);
    let mix_shared = shared.clone();
    let mut heard: Vec<[f32; 2]> = Vec::new();
    let mut mix = move |out: &mut dyn FnMut(usize, f32), frames: usize| {
        let monitor = &mix_shared.monitor;
        let listening = monitor.wanted();
        let mut guards: Vec<(bool, f32, std::sync::MutexGuard<VecDeque<f32>>)> =
            buffers.iter().map(|(i, b)| (monitor.excluded[*i].load(Ordering::Relaxed), mix_shared.gain(*i), b.lock().unwrap())).collect();
        let mut peak = 0f32;
        heard.clear();
        for f in 0..frames {
            let (mut l, mut r, mut ml, mut mr) = (0f32, 0f32, 0f32, 0f32);
            for (excluded, g, q) in guards.iter_mut() {
                if q.len() >= 2 {
                    let (a, b) = (q.pop_front().unwrap() * *g, q.pop_front().unwrap() * *g);
                    l += a;
                    r += b;
                    if !*excluded {
                        ml += a;
                        mr += b;
                    }
                }
            }
            let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
            peak = peak.max(l.abs()).max(r.abs());
            out(f * 2, l);
            out(f * 2 + 1, r);
            if listening {
                heard.push([ml.clamp(-1.0, 1.0), mr.clamp(-1.0, 1.0)]);
            }
        }
        drop(guards);
        if listening {
            monitor.feed(&heard);
        }
        mix_shared.set_peak(SOURCES, peak);
    };
    let err = |e| crate::debug_log(&format!("mic mixer output error: {e}"));
    let out_stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => out_dev.build_output_stream(
            out_cfg,
            move |data: &mut [f32], _: &_| {
                let frames = data.len() / out_channels;
                data.fill(0.0);
                mix(&mut |i, v| data[(i / 2) * out_channels + (i % 2).min(out_channels - 1)] = v, frames);
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => out_dev.build_output_stream(
            out_cfg,
            move |data: &mut [i16], _: &_| {
                let frames = data.len() / out_channels;
                data.fill(0);
                mix(&mut |i, v| data[(i / 2) * out_channels + (i % 2).min(out_channels - 1)] = (v * 32767.0) as i16, frames);
            },
            err,
            None,
        )?,
        f => return Err(anyhow!("unsupported output sample format {f:?}")),
    };
    out_stream.play()?;
    streams.push(out_stream);
    Ok(streams)
}

/// Runs the mixer on its own thread (audio streams aren't Send). Send it a new config to restart it.
pub fn spawn(shared: Arc<Shared>) -> Sender<Config> {
    let (tx, rx): (Sender<Config>, Receiver<Config>) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut streams: Vec<cpal::Stream> = Vec::new();
        let mut current: Option<Config> = None;
        let mut monitor: Option<(String, cpal::Stream)> = None;
        loop {
            // Wake up regularly to start, stop or move the listen stream.
            let mut cfg = match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(cfg) => cfg,
                Err(RecvTimeoutError::Timeout) => {
                    manage_monitor(&shared, &mut monitor, current.as_ref());
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Only the newest config matters.
            while let Ok(newer) = rx.try_recv() {
                cfg = newer;
            }
            shared.apply_levels(&cfg);
            // Gain and mute changes don't need a restart.
            let topology =
                |c: &Config| (c.enabled, c.output.clone(), c.sources.iter().map(|s| (s.device.clone(), s.loopback)).collect::<Vec<_>>());
            if current.as_ref().map(topology) == Some(topology(&cfg)) {
                current = Some(cfg);
                continue;
            }
            streams.clear();
            *shared.status.lock().unwrap() = String::new();
            shared.running.store(false, Ordering::Relaxed);
            if cfg.enabled && !cfg.output.is_empty() {
                match start(&cfg, &shared) {
                    Ok(s) => {
                        streams = s;
                        shared.running.store(true, Ordering::Relaxed);
                    }
                    Err(e) => *shared.status.lock().unwrap() = format!("{e:#}"),
                }
            }
            current = Some(cfg);
            manage_monitor(&shared, &mut monitor, current.as_ref());
        }
    });
    tx
}

/// Opens, moves or closes the listen stream to match what was asked for and the current default output.
fn manage_monitor(shared: &Arc<Shared>, monitor: &mut Option<(String, cpal::Stream)>, cfg: Option<&Config>) {
    let m = &shared.monitor;
    let running = shared.running.load(Ordering::Relaxed);
    if !(running && m.wanted()) {
        if monitor.take().is_some() {
            m.queue.lock().unwrap().clear();
        }
        if !running {
            m.test.store(0, Ordering::Relaxed);
        }
        return;
    }
    let host = cpal::default_host();
    let Some(dev) = host.default_output_device() else { return };
    let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
    if let Some(cfg) = cfg {
        // Playing into the cable would feed the mix back into itself.
        if name == cfg.output {
            *monitor = None;
            return;
        }
        // A source that records the device we play to would hear itself.
        for (i, s) in cfg.sources.iter().enumerate() {
            m.excluded[i].store(s.loopback && s.device == name, Ordering::Relaxed);
        }
    }
    if monitor.as_ref().is_some_and(|(n, _)| *n == name) {
        return;
    }
    *monitor = None;
    match open_monitor(&dev, shared.clone()) {
        Ok(s) => *monitor = Some((name, s)),
        Err(e) => crate::debug_log(&format!("mic mixer listen error: {e:#}")),
    }
}

/// Plays the monitor queue on `dev`, converting from the mix rate to the device's rate.
fn open_monitor(dev: &cpal::Device, shared: Arc<Shared>) -> Result<cpal::Stream> {
    let supported = dev.default_output_config()?;
    let cfg = supported.config();
    let channels = cfg.channels as usize;
    let dst = cfg.sample_rate as f64;
    let (mut pos, mut prev, mut cur) = (0f64, [0f32; 2], [0f32; 2]);
    let mut fill = move |frames: usize, put: &mut dyn FnMut(usize, [f32; 2])| {
        let m = &shared.monitor;
        let step = m.rate.load(Ordering::Relaxed) as f64 / dst;
        let mut q = m.queue.lock().unwrap();
        for f in 0..frames {
            while pos >= 1.0 {
                prev = cur;
                cur = if q.len() >= 2 { [q.pop_front().unwrap(), q.pop_front().unwrap()] } else { [0.0; 2] };
                pos -= 1.0;
            }
            let t = pos as f32;
            put(f, [prev[0] + (cur[0] - prev[0]) * t, prev[1] + (cur[1] - prev[1]) * t]);
            pos += step;
        }
        if q.is_empty() && m.test.load(Ordering::Relaxed) == 2 {
            m.test.store(0, Ordering::Relaxed);
        }
    };
    let place = move |frame: [f32; 2], out: &mut dyn FnMut(usize, f32)| {
        if channels == 1 {
            out(0, (frame[0] + frame[1]) * 0.5);
        } else {
            out(0, frame[0]);
            out(1, frame[1]);
        }
    };
    let err = |e| crate::debug_log(&format!("mic mixer listen error: {e}"));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => dev.build_output_stream(
            cfg,
            move |data: &mut [f32], _: &_| {
                data.fill(0.0);
                fill(data.len() / channels, &mut |f, fr| place(fr, &mut |c, v| data[f * channels + c] = v));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => dev.build_output_stream(
            cfg,
            move |data: &mut [i16], _: &_| {
                data.fill(0);
                fill(data.len() / channels, &mut |f, fr| place(fr, &mut |c, v| data[f * channels + c] = (v * 32767.0) as i16));
            },
            err,
            None,
        )?,
        f => return Err(anyhow!("unsupported listen sample format {f:?}")),
    };
    stream.play()?;
    Ok(stream)
}

/// The virtual cable's playback side, if one is installed.
pub fn find_cable(outputs: &[String]) -> Option<String> {
    // Prefer the plain stereo input over variants such as "CABLE In 16ch".
    let by = |f: &dyn Fn(&String) -> bool| outputs.iter().find(|n| f(n)).cloned();
    by(&|n| n.starts_with("CABLE Input")).or_else(|| by(&|n| n.contains("VB-Audio Virtual Cable")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratio() {
        // 44.1 kHz -> 48 kHz produces ~48000 frames per second of input.
        let mut rs = Resampler::new(44_100, 48_000);
        let mut out = VecDeque::new();
        for _ in 0..44_100 {
            rs.push([0.5, -0.5], &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        assert!(out.iter().skip(4).all(|v| v.abs() == 0.5));
    }

    #[test]
    fn test_clip_records_then_plays() {
        let m = Monitor::default();
        m.rate.store(1000, Ordering::Relaxed);
        m.start_test();
        let block = vec![[0.5f32, 0.5]; 1000];
        for _ in 0..4 {
            m.feed(&block);
        }
        assert_eq!(m.test.load(Ordering::Relaxed), 1, "still recording before {TEST_SECS} s");
        m.feed(&block);
        assert_eq!(m.test.load(Ordering::Relaxed), 2);
        assert_eq!(m.queue.lock().unwrap().len(), 5000 * 2);
    }

    #[test]
    fn live_listen_keeps_latency_low() {
        let m = Monitor::default();
        m.rate.store(48_000, Ordering::Relaxed);
        m.live.store(true, Ordering::Relaxed);
        m.feed(&vec![[0.1f32, 0.1]; 48_000]);
        assert!(m.queue.lock().unwrap().len() <= (48_000.0 * MAX_BUFFER) as usize * 2);
    }

    #[test]
    fn mono_resampler_ratio() {
        let mut rs = MonoResampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..44_100 {
            rs.push(0.25, &mut out);
        }
        assert!((47_990..=48_010).contains(&out.len()), "{}", out.len());
    }

    #[test]
    fn finds_vb_cable() {
        let outs = vec!["Speakers (Realtek USB Audio)".to_string(), "CABLE Input (VB-Audio Virtual Cable)".to_string()];
        assert_eq!(find_cable(&outs).as_deref(), Some("CABLE Input (VB-Audio Virtual Cable)"));
        assert_eq!(find_cable(&outs[..1]), None);
        let with_16ch = vec!["CABLE In 16ch (VB-Audio Virtual Cable)".to_string(), outs[1].clone()];
        assert_eq!(find_cable(&with_16ch).as_deref(), Some("CABLE Input (VB-Audio Virtual Cable)"));
    }
}
