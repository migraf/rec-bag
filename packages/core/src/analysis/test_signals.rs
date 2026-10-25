//! Synthetic audio for the analysis tests: kicks, hats and tones placed at
//! known times, so a test can state exactly where the beats are.

use std::f32::consts::PI;

/// Silence of `secs` seconds.
pub fn silence(rate: u32, secs: f64) -> Vec<f32> {
    vec![0.0; (secs * rate as f64) as usize]
}

/// A kick on every beat from `first_beat` seconds.
pub fn click_track(rate: u32, bpm: f64, first_beat: f64, secs: f64) -> Vec<f32> {
    let mut samples = silence(rate, secs);
    for t in beat_times(bpm, first_beat, secs) {
        add_kick(&mut samples, rate, t, 0.8);
    }
    samples
}

/// Beat times from `first_beat` up to `secs`.
pub fn beat_times(bpm: f64, first_beat: f64, secs: f64) -> Vec<f64> {
    let period = 60.0 / bpm;
    (0..)
        .map(|k| first_beat + k as f64 * period)
        .take_while(|&t| t < secs)
        .collect()
}

/// A kick drum starting at `at`: a short broadband click on top of a body
/// whose pitch falls from 120 Hz to 50 Hz.
pub fn add_kick(samples: &mut [f32], rate: u32, at: f64, gain: f32) {
    let start = (at * rate as f64).round() as usize;
    let len = (0.25 * rate as f64) as usize;
    let mut noise = Noise::new(start as u32);
    let mut phase = 0.0f32;
    for n in 0..len {
        let Some(s) = samples.get_mut(start + n) else { break };
        let t = n as f32 / rate as f32;
        let freq = 50.0 + 70.0 * (-t / 0.03).exp();
        phase += 2.0 * PI * freq / rate as f32;
        let body = phase.sin() * (-t / 0.12).exp();
        let click = noise.next() * (-t / 0.002).exp() * 0.6;
        *s += gain * (body + click);
    }
}

/// A closed hi-hat at `at`: a burst of high-passed noise.
pub fn add_hat(samples: &mut [f32], rate: u32, at: f64, gain: f32) {
    let start = (at * rate as f64).round() as usize;
    let len = (0.06 * rate as f64) as usize;
    let mut noise = Noise::new(start as u32 ^ 0x5eed);
    let mut prev = 0.0;
    for n in 0..len {
        let Some(s) = samples.get_mut(start + n) else { break };
        let t = n as f32 / rate as f32;
        let white = noise.next();
        *s += gain * (white - prev) * (-t / 0.015).exp();
        prev = white;
    }
}

/// A sustained sine from `from` to `to` seconds.
pub fn add_tone(samples: &mut [f32], rate: u32, from: f64, to: f64, freq: f32, gain: f32) {
    let start = (from * rate as f64) as usize;
    let end = ((to * rate as f64) as usize).min(samples.len());
    for (n, s) in samples[start.min(end)..end].iter_mut().enumerate() {
        *s += gain * (2.0 * PI * freq * n as f32 / rate as f32).sin();
    }
}

/// Deterministic white noise in [-1, 1].
struct Noise(u32);

impl Noise {
    fn new(seed: u32) -> Self {
        Self(seed.wrapping_mul(747_796_405).wrapping_add(1))
    }

    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    }
}
