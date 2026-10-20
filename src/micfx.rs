//! Mic processing that replaces Sonar's mic chain, plus a broadcast-style voice chain:
//! DeepFilterNet3 noise suppression (in place of ClearCast) -> rumble filter -> gate -> EQ ->
//! leveler -> de-esser -> limiter. Works on mono audio at 48 kHz, the model's rate. Everything after
//! the model is zero-latency.

use std::f32::consts::{FRAC_1_SQRT_2, PI};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub const RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilterKind {
    LowShelf,
    Peak,
    HighShelf,
    HighPass,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub kind: FilterKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// Voice presets: an EQ curve plus how hard the leveler works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// No EQ coloring, light leveling.
    Natural,
    /// Sonar's "Less Nasal".
    #[default]
    Clear,
    /// Fuller low end, presence and air, firmer leveling: the radio/podcast sound.
    Broadcast,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Natural, Preset::Clear, Preset::Broadcast];

    pub fn bands(self) -> Vec<Band> {
        use FilterKind::*;
        let band = |kind, freq, gain_db, q| Band { kind, freq, gain_db, q };
        match self {
            Preset::Natural => Vec::new(),
            Preset::Clear => less_nasal(),
            Preset::Broadcast => vec![
                band(LowShelf, 120.0, 3.0, FRAC_1_SQRT_2),
                band(Peak, 350.0, -3.0, 1.0),
                band(Peak, 3000.0, 2.5, 1.0),
                band(HighShelf, 10000.0, 3.0, FRAC_1_SQRT_2),
            ],
        }
    }

    pub fn leveler(self) -> f32 {
        match self {
            Preset::Natural => 0.3,
            Preset::Clear => 0.5,
            Preset::Broadcast => 0.75,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fx {
    pub denoise: bool,
    /// 0..1, like Sonar's ClearCast slider. 1 removes as much noise as the model can.
    pub denoise_strength: f32,
    /// DeepFilterNet3's low-latency model: no look-ahead, about 20 ms less delay.
    pub low_latency: bool,
    pub preset: Preset,
    pub eq: bool,
    pub bands: Vec<Band>,
    /// High-pass at 80 Hz against rumble, desk thumps and breath pops.
    pub highpass: bool,
    pub gate: bool,
    /// The gate closes below this level, in dBFS.
    pub gate_db: f32,
    /// Compressor that evens out loud and quiet words.
    pub leveler: bool,
    /// 0..1: gentle to firm.
    pub leveler_amount: f32,
    pub deesser: bool,
    /// Skip all processing, for A/B comparisons. Not saved.
    #[serde(skip)]
    pub bypass: bool,
}

impl Default for Fx {
    /// Starts from the Sonar mic preset this replaces (ClearCast at 90% and the "Less Nasal" EQ),
    /// plus the zero-latency broadcast stages.
    fn default() -> Self {
        Self {
            denoise: true,
            denoise_strength: 0.9,
            low_latency: false,
            preset: Preset::Clear,
            eq: true,
            bands: less_nasal(),
            highpass: true,
            gate: false,
            gate_db: -50.0,
            leveler: true,
            leveler_amount: Preset::Clear.leveler(),
            deesser: true,
            bypass: false,
        }
    }
}

impl Fx {
    pub fn apply_preset(&mut self, preset: Preset) {
        self.preset = preset;
        self.bands = preset.bands();
        self.leveler_amount = preset.leveler();
    }
}

const LL_MODEL: &str = "DeepFilterNet3_ll_onnx.tar.gz";

/// The low-latency model ships as a separate file next to the exe (it is 36 MB).
pub fn ll_model_path() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [dir.join("models").join(LL_MODEL), dir.join("../../assets/models").join(LL_MODEL)].into_iter().find(|p| p.exists())
}

/// Sonar's "Less Nasal" mic preset: rumble cut, a dip around 1 kHz, lifted presence.
pub fn less_nasal() -> Vec<Band> {
    let band = |kind, freq, gain_db| Band { kind, freq, gain_db, q: FRAC_1_SQRT_2 };
    use FilterKind::*;
    vec![
        band(LowShelf, 31.0, -12.0),
        band(Peak, 62.0, 0.0),
        band(Peak, 125.0, 0.0),
        band(Peak, 250.0, 0.0),
        band(Peak, 500.0, -2.0),
        band(Peak, 1000.0, -4.0),
        band(Peak, 2000.0, -2.0),
        band(Peak, 4000.0, 2.0),
        band(Peak, 8000.0, 4.0),
        band(HighShelf, 16000.0, 0.0),
    ]
}

/// Settings shared with the processing thread; `version` changes whenever `fx` does.
#[derive(Default)]
pub struct Live {
    fx: Mutex<Fx>,
    version: AtomicU32,
    /// Slowest model time per 10 ms block over the last second, in microseconds (0 = not measured yet).
    pub cost_us: AtomicU32,
    /// Low latency was asked for but the model file is missing, so the standard model runs.
    pub ll_missing: AtomicBool,
}

impl Live {
    pub fn set(&self, fx: &Fx) {
        let mut cur = self.fx.lock().unwrap();
        if *cur != *fx {
            *cur = fx.clone();
            self.version.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn get(&self) -> (u32, Fx) {
        let fx = self.fx.lock().unwrap();
        (self.version.load(Ordering::Relaxed), fx.clone())
    }
}

/// RBJ cookbook biquad, transposed direct form II.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn new(band: &Band, rate: f32) -> Self {
        let a = 10f32.powf(band.gain_db / 40.0);
        let w0 = 2.0 * PI * band.freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * band.q);
        let sa = 2.0 * a.sqrt() * alpha;
        let (b, a0, a1, a2) = match band.kind {
            FilterKind::Peak => ([1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a], 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            FilterKind::LowShelf => (
                [a * ((a + 1.0) - (a - 1.0) * cos + sa), 2.0 * a * ((a - 1.0) - (a + 1.0) * cos), a * ((a + 1.0) - (a - 1.0) * cos - sa)],
                (a + 1.0) + (a - 1.0) * cos + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sa,
            ),
            FilterKind::HighShelf => (
                [a * ((a + 1.0) + (a - 1.0) * cos + sa), -2.0 * a * ((a - 1.0) + (a + 1.0) * cos), a * ((a + 1.0) + (a - 1.0) * cos - sa)],
                (a + 1.0) - (a - 1.0) * cos + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sa,
            ),
            FilterKind::HighPass => ([(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0], 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        };
        Self { b: b.map(|x| x / a0), a: [a1 / a0, a2 / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Downward gate with a short hold, so word endings aren't chopped.
struct Gate {
    env: f32,
    gain: f32,
    hold: u32,
}

impl Gate {
    const HOLD: u32 = RATE / 5;

    fn run(&mut self, x: f32, threshold: f32) -> f32 {
        let coef = |ms: f32| (-1.0 / (ms * 0.001 * RATE as f32)).exp();
        let lvl = x.abs();
        self.env = if lvl > self.env { lvl } else { self.env * coef(50.0) + lvl * (1.0 - coef(50.0)) };
        if self.env >= threshold {
            self.hold = Self::HOLD;
        } else {
            self.hold = self.hold.saturating_sub(1);
        }
        let (target, c) = if self.hold > 0 { (1.0, coef(2.0)) } else { (0.0, coef(120.0)) };
        self.gain = target + (self.gain - target) * c;
        x * self.gain
    }
}

/// One-pole smoothing coefficient for a time constant in milliseconds.
fn coef(ms: f32) -> f32 {
    (-1.0 / (ms * 0.001 * RATE as f32)).exp()
}

fn db(x: f32) -> f32 {
    20.0 * (x.abs() + 1e-9).log10()
}

fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Compressor with makeup gain: quiet words come up, loud ones come down.
struct Leveler {
    env_db: f32,
}

impl Leveler {
    /// Unity gain for speech around -20 dBFS; the threshold and ratio grow with `amount`.
    fn params(amount: f32) -> (f32, f32, f32) {
        let a = amount.clamp(0.0, 1.0);
        let threshold = -18.0 - 14.0 * a;
        let ratio = 2.0 + 2.0 * a;
        let makeup = (-20.0 - threshold) * (1.0 - 1.0 / ratio);
        (threshold, ratio, makeup)
    }

    fn run(&mut self, x: f32, (threshold, ratio, makeup): (f32, f32, f32)) -> f32 {
        let level = db(x);
        let c = if level > self.env_db { coef(5.0) } else { coef(150.0) };
        self.env_db = level + (self.env_db - level) * c;
        let over = (self.env_db - threshold).max(0.0);
        // No makeup gain in pauses (below -55 dBFS, fading in up to -45), so leftover noise isn't raised.
        let makeup = makeup * ((self.env_db + 55.0) / 10.0).clamp(0.0, 1.0);
        x * lin(makeup - over * (1.0 - 1.0 / ratio))
    }
}

/// Dynamic-EQ de-esser: while the band above 5 kHz is loud ("s", "sh", "t"), a high shelf turns it
/// down. A shelf keeps the phase coherent, unlike subtracting a filtered band.
struct DeEsser {
    detect: Biquad,
    shelf: Biquad,
    env_db: f32,
    cut_db: f32,
    tick: u32,
}

impl DeEsser {
    const THRESHOLD_DB: f32 = -30.0;
    const MAX_CUT_DB: f32 = 10.0;

    fn new() -> Self {
        Self {
            detect: Biquad::new(&Band { kind: FilterKind::HighPass, freq: 5000.0, gain_db: 0.0, q: FRAC_1_SQRT_2 }, RATE as f32),
            shelf: Self::shelf(0.0),
            env_db: -120.0,
            cut_db: 0.0,
            tick: 0,
        }
    }

    fn shelf(cut_db: f32) -> Biquad {
        Biquad::new(&Band { kind: FilterKind::HighShelf, freq: 6000.0, gain_db: -cut_db, q: FRAC_1_SQRT_2 }, RATE as f32)
    }

    fn run(&mut self, x: f32) -> f32 {
        let level = db(self.detect.run(x));
        let c = if level > self.env_db { coef(1.0) } else { coef(60.0) };
        self.env_db = level + (self.env_db - level) * c;
        // Retune the shelf every 16 samples (0.33 ms), keeping its state so nothing clicks.
        self.tick += 1;
        if self.tick == 16 {
            self.tick = 0;
            let cut = ((self.env_db - Self::THRESHOLD_DB) * 0.75).clamp(0.0, Self::MAX_CUT_DB);
            if (cut - self.cut_db).abs() > 0.1 {
                self.cut_db = cut;
                let z = self.shelf.z;
                self.shelf = Self::shelf(cut);
                self.shelf.z = z;
            }
        }
        self.shelf.run(x)
    }
}

/// Zero-latency peak limiter at -1 dBFS: instant attack, smooth release.
struct Limiter {
    gain: f32,
}

impl Limiter {
    const CEILING: f32 = 0.891;

    fn run(&mut self, x: f32) -> f32 {
        let peak = x.abs();
        if peak * self.gain > Self::CEILING {
            self.gain = Self::CEILING / peak;
        }
        let y = x * self.gain;
        self.gain += (1.0 - self.gain) * (1.0 - coef(50.0));
        y
    }
}

/// DeepFilterNet's attenuation limit for a 0..1 strength; 100 dB or more means no limit.
fn atten_db(strength: f32) -> f32 {
    if strength >= 0.999 { 100.0 } else { strength.clamp(0.0, 1.0) * 60.0 }
}

/// The whole chain for one mono stream. Feed it any amount of 48 kHz audio; output comes back in
/// 10 ms blocks.
pub struct Chain {
    version: u32,
    fx: Fx,
    eq: Vec<Biquad>,
    highpass: Biquad,
    gate: Gate,
    leveler: Leveler,
    deesser: DeEsser,
    limiter: Limiter,
    df: Option<df::tract::DfTract>,
    df_failed: bool,
    pending: Vec<f32>,
    block: Vec<f32>,
    enh: Vec<f32>,
    /// Slowest block and block count in the current one-second window.
    window: (u32, u32),
}

impl Chain {
    pub const HOP: usize = 480;

    pub fn new() -> Self {
        Self {
            version: u32::MAX,
            fx: Fx::default(),
            eq: Vec::new(),
            highpass: Biquad::new(&Band { kind: FilterKind::HighPass, freq: 80.0, gain_db: 0.0, q: FRAC_1_SQRT_2 }, RATE as f32),
            gate: Gate { env: 0.0, gain: 1.0, hold: 0 },
            leveler: Leveler { env_db: -120.0 },
            deesser: DeEsser::new(),
            limiter: Limiter { gain: 1.0 },
            df: None,
            df_failed: false,
            pending: Vec::new(),
            block: vec![0.0; Self::HOP],
            enh: vec![0.0; Self::HOP],
            window: (0, 0),
        }
    }

    fn configure(&mut self, live: &Live) {
        let (version, fx) = live.get();
        if version == self.version {
            return;
        }
        self.version = version;
        self.eq = fx.bands.iter().filter(|b| b.freq > 0.0 && b.freq < RATE as f32 / 2.0).map(|b| Biquad::new(b, RATE as f32)).collect();
        if let Some(df) = &mut self.df {
            df.set_atten_lim(atten_db(fx.denoise_strength));
        }
        // Switching models means loading the other one.
        if fx.low_latency != self.fx.low_latency {
            self.df = None;
            self.df_failed = false;
        }
        self.fx = fx;
    }

    /// Loads the model on first use; the first load takes a moment.
    fn load_model(&mut self, live: &Live) {
        if self.df.is_some() || self.df_failed {
            return;
        }
        let ll_path = if self.fx.low_latency { ll_model_path() } else { None };
        live.ll_missing.store(self.fx.low_latency && ll_path.is_none(), Ordering::Relaxed);
        let params = match &ll_path {
            Some(p) => df::tract::DfParams::new(p.clone()),
            None => Ok(df::tract::DfParams::default()),
        };
        let rp = df::tract::RuntimeParams::default_with_ch(1).with_atten_lim(atten_db(self.fx.denoise_strength));
        match params.and_then(|p| df::tract::DfTract::new(p, &rp)) {
            Ok(m) => self.df = Some(m),
            Err(e) => {
                crate::debug_log(&format!("mic noise suppression failed to load: {e:#}"));
                self.df_failed = true;
            }
        }
    }

    pub fn process(&mut self, input: &[f32], live: &Live, out: &mut impl FnMut(&[f32])) {
        self.pending.extend_from_slice(input);
        let mut start = 0;
        while self.pending.len() - start >= Self::HOP {
            self.configure(live);
            self.block.copy_from_slice(&self.pending[start..start + Self::HOP]);
            start += Self::HOP;
            if self.fx.bypass {
                out(&self.block);
                continue;
            }
            if self.fx.denoise {
                self.load_model(live);
                if let Some(df) = &mut self.df {
                    let t = Instant::now();
                    let noisy = ndarray::ArrayView2::from_shape((1, Self::HOP), &self.block).unwrap();
                    let enh = ndarray::ArrayViewMut2::from_shape((1, Self::HOP), &mut self.enh).unwrap();
                    match df.process(noisy, enh) {
                        Ok(_) => self.block.copy_from_slice(&self.enh),
                        Err(e) => crate::debug_log(&format!("mic noise suppression error: {e:#}")),
                    }
                    // Publish once a second so the readout is steady; silent blocks finish almost instantly.
                    let (worst, n) = &mut self.window;
                    *worst = (*worst).max(t.elapsed().as_micros() as u32);
                    *n += 1;
                    if *n == 100 {
                        live.cost_us.store(*worst, Ordering::Relaxed);
                        self.window = (0, 0);
                    }
                }
            } else {
                live.cost_us.store(0, Ordering::Relaxed);
                self.window = (0, 0);
            }
            let threshold = lin(self.fx.gate_db);
            let leveler = Leveler::params(self.fx.leveler_amount);
            for s in self.block.iter_mut() {
                let mut x = *s;
                if self.fx.highpass {
                    x = self.highpass.run(x);
                }
                if self.fx.gate {
                    x = self.gate.run(x, threshold);
                }
                if self.fx.eq {
                    x = self.eq.iter_mut().fold(x, |v, f| f.run(v));
                }
                if self.fx.leveler {
                    x = self.leveler.run(x, leveler);
                }
                if self.fx.deesser {
                    x = self.deesser.run(x);
                }
                *s = self.limiter.run(x);
            }
            out(&self.block);
        }
        self.pending.drain(..start);
    }
}

/// Reads a 16-bit PCM WAV file as mono samples (channels are averaged). Returns (rate, samples).
pub fn read_wav(path: &str) -> anyhow::Result<(u32, Vec<f32>)> {
    let b = std::fs::read(path)?;
    anyhow::ensure!(b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WAVE", "{path} is not a WAV file");
    let (mut pos, mut fmt) = (12, None);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into()?) as usize;
        let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
        if id == b"fmt " {
            let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
            fmt = Some((u16_at(0), u16_at(2) as usize, u32::from_le_bytes(body[4..8].try_into()?), u16_at(14)));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt.ok_or_else(|| anyhow::anyhow!("WAV data before format"))?;
            anyhow::ensure!(format == 1 && bits == 16, "only 16-bit PCM WAV is supported");
            let samples = body
                .chunks_exact(2 * channels)
                .map(|f| f.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s) as f32 / 32768.0).sum::<f32>() / channels as f32)
                .collect();
            return Ok((rate, samples));
        }
        pos += 8 + len + (len & 1);
    }
    anyhow::bail!("{path} has no audio data")
}

/// Writes mono 16-bit PCM.
pub fn write_wav(path: &str, rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let data = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend(b"RIFF");
    b.extend((36 + data).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * 2).to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data.to_le_bytes());
    for s in samples {
        b.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, secs: f32) -> Vec<f32> {
        (0..(RATE as f32 * secs) as usize).map(|i| (2.0 * PI * freq * i as f32 / RATE as f32).sin() * 0.5).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Gain of the EQ at `freq` in dB, measured on a steady tone.
    fn eq_gain_db(bands: &[Band], freq: f32) -> f32 {
        let mut f: Vec<Biquad> = bands.iter().map(|b| Biquad::new(b, RATE as f32)).collect();
        let x = tone(freq, 1.0);
        let y: Vec<f32> = x.iter().map(|&s| f.iter_mut().fold(s, |v, b| b.run(v))).collect();
        let half = x.len() / 2;
        20.0 * (rms(&y[half..]) / rms(&x[half..])).log10()
    }

    #[test]
    fn less_nasal_shape() {
        let b = less_nasal();
        assert!(eq_gain_db(&b, 20.0) < -8.0, "rumble is cut");
        assert!((eq_gain_db(&b, 1000.0) + 5.0).abs() < 2.0, "dip near 1 kHz: {}", eq_gain_db(&b, 1000.0));
        assert!((eq_gain_db(&b, 8000.0) - 4.5).abs() < 2.0, "presence lift: {}", eq_gain_db(&b, 8000.0));
    }

    #[test]
    fn flat_band_is_transparent() {
        let flat = [Band { kind: FilterKind::Peak, freq: 1000.0, gain_db: 0.0, q: FRAC_1_SQRT_2 }];
        assert!(eq_gain_db(&flat, 440.0).abs() < 0.01);
    }

    #[test]
    fn highpass_cuts_rumble() {
        let hp = [Band { kind: FilterKind::HighPass, freq: 80.0, gain_db: 0.0, q: FRAC_1_SQRT_2 }];
        assert!(eq_gain_db(&hp, 30.0) < -15.0);
        assert!(eq_gain_db(&hp, 300.0).abs() < 0.5);
    }

    #[test]
    fn leveler_evens_out_levels() {
        let p = Leveler::params(0.75);
        let out_level = |amp: f32| {
            let mut l = Leveler { env_db: -120.0 };
            let y: Vec<f32> = tone(300.0, 1.0).iter().map(|s| l.run(s * amp * 2.0, p)).collect();
            db(rms(&y[y.len() / 2..]))
        };
        let (quiet, loud) = (out_level(0.03), out_level(0.5));
        // The input differs by 24 dB; the leveler narrows that a lot.
        assert!(loud - quiet < 14.0, "quiet {quiet:.1} dB, loud {loud:.1} dB");
    }

    #[test]
    fn deesser_only_touches_loud_highs() {
        let level = |freq: f32, amp: f32| {
            let mut d = DeEsser::new();
            let x: Vec<f32> = tone(freq, 0.5).iter().map(|s| s * amp * 2.0).collect();
            let y: Vec<f32> = x.iter().map(|&s| d.run(s)).collect();
            db(rms(&y[y.len() / 2..])) - db(rms(&x[x.len() / 2..]))
        };
        assert!(level(7000.0, 0.3) < -5.0, "loud sibilance is cut");
        assert!(level(300.0, 0.3).abs() < 0.5, "voice body untouched");
        assert!(level(7000.0, 0.005).abs() < 0.5, "quiet highs untouched");
    }

    #[test]
    fn limiter_holds_ceiling() {
        let mut l = Limiter { gain: 1.0 };
        let y: Vec<f32> = tone(200.0, 0.2).iter().map(|s| l.run(s * 3.0)).collect();
        assert!(y.iter().all(|v| v.abs() <= Limiter::CEILING + 1e-6));
    }

    #[test]
    fn old_settings_files_get_new_stages() {
        let old = r#"{"denoise":true,"denoise_strength":0.9,"eq":true,"gate":false,"gate_db":-50.0}"#;
        let fx: Fx = serde_json::from_str(old).unwrap();
        assert!(fx.leveler && fx.deesser && fx.highpass && !fx.low_latency);
        assert_eq!(fx.preset, Preset::Clear);
    }

    #[test]
    fn gate_closes_on_quiet_input() {
        let mut g = Gate { env: 0.0, gain: 1.0, hold: 0 };
        let quiet: Vec<f32> = tone(200.0, 1.0).iter().map(|s| s * 0.001).collect();
        let out: Vec<f32> = quiet.iter().map(|&s| g.run(s, 10f32.powf(-50.0 / 20.0))).collect();
        assert!(rms(&out[out.len() - 4800..]) < rms(&quiet) * 0.05);
    }

    #[test]
    fn wav_round_trip() {
        let path = std::env::temp_dir().join("sonar-deck-wav-test.wav");
        let path = path.to_str().unwrap();
        let x: Vec<f32> = tone(440.0, 0.1);
        write_wav(path, RATE, &x).unwrap();
        let (rate, y) = read_wav(path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!((rate, y.len()), (RATE, x.len()));
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    /// Pure noise comes out (near) silent. Needs the model, so it is slow in debug builds:
    /// `cargo test --release`. Real speech is checked with `sonar-deck micfx in.wav out.wav`.
    #[test]
    fn denoise_removes_noise_in_real_time() {
        let live = Live::default();
        live.set(&Fx { eq: false, denoise_strength: 1.0, ..Fx::default() });
        let mut chain = Chain::new();
        // Deterministic white noise at about -26 dBFS.
        let mut seed = 1u32;
        let noise: Vec<f32> = (0..RATE as usize * 2)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.1
            })
            .collect();
        let mut out = Vec::new();
        let t = Instant::now();
        for chunk in noise.chunks(441) {
            chain.process(chunk, &live, &mut |b| out.extend_from_slice(b));
        }
        let elapsed = t.elapsed().as_secs_f32();
        assert_eq!(out.len(), noise.len() / Chain::HOP * Chain::HOP);
        let reduction = 20.0 * (rms(&out[RATE as usize..]) / rms(&noise[RATE as usize..])).log10();
        assert!(reduction < -20.0, "noise only reduced by {reduction:.1} dB");
        eprintln!("2 s of audio in {elapsed:.3} s, noise {reduction:.1} dB, worst block {} us", live.cost_us.load(Ordering::Relaxed));
        if !cfg!(debug_assertions) {
            assert!(elapsed < 1.0, "slower than real time: {elapsed:.3} s for 2 s");
        }
    }
}
