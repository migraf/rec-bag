//! Tempo estimation from the onset envelope of the whole track.
//!
//! Two views of the same envelope vote on each candidate tempo:
//!
//! 1. **Autocorrelation** asks "how often does the pattern repeat?". It finds
//!    the beat period, but every multiple of it repeats too, and a pattern of
//!    kick plus off-beat hat also repeats every one and a half beats.
//! 2. **Fourier magnitude** at the candidate's frequency asks "how much
//!    energy pulses at exactly this rate?". A sub-multiple of the true tempo
//!    (half or two-thirds) puts every other hit in antiphase, so it scores low
//!    where autocorrelation scored high.
//!
//! Their product, times a broad preference for dance-music tempos that only
//! settles near-ties between octaves, picks the tempo. The grid fit later
//! refines it far below the envelope's resolution.

use super::onset::{onset_envelope, OnsetEnvelope};
use super::traits::{Analyzer, AnalyzerOutput};
use super::types::AnalysisError;
use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

/// Tempo the octave preference is centred on.
const PREFERRED_BPM: f64 = 130.0;
/// Width of the octave preference, in octaves (one standard deviation).
const PREFERENCE_OCTAVES: f64 = 1.0;
/// Length of the windows the Fourier magnitude is averaged over.
const FOURIER_WINDOW_SECS: f64 = 10.0;

/// Tempo estimate before grid fitting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoEstimate {
    pub bpm: f64,
    /// How clearly the winner beat the runner-up (0.0 to 1.0).
    pub confidence: f32,
}

/// BPM analyzer using spectral flux, autocorrelation and Fourier magnitude.
pub struct BpmAnalyzer {
    /// Minimum BPM to detect.
    min_bpm: f32,
    /// Maximum BPM to detect.
    max_bpm: f32,
}

impl Default for BpmAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl BpmAnalyzer {
    /// Create a new BPM analyzer with default settings.
    pub fn new() -> Self {
        Self {
            min_bpm: 70.0,
            max_bpm: 200.0,
        }
    }

    /// Create a BPM analyzer with custom BPM range.
    pub fn with_bpm_range(min_bpm: f32, max_bpm: f32) -> Self {
        Self { min_bpm, max_bpm }
    }

    /// Estimate the tempo of an onset envelope.
    pub fn estimate(&self, env: &OnsetEnvelope) -> Option<TempoEstimate> {
        let (min_bpm, max_bpm) = (self.min_bpm as f64, self.max_bpm as f64);
        let min_lag = env.rate * 60.0 / max_bpm;
        let max_lag = env.rate * 60.0 / min_bpm;
        if env.len() < (max_lag * 4.0) as usize {
            return None;
        }

        let acf = autocorrelation(&env.values);
        let lag_range = (min_lag.floor() as usize).max(1)..(max_lag.ceil() as usize).min(acf.len() - 1);

        // The strongest repetition periods, refined between lags.
        let mut peaks: Vec<(f64, f32)> = lag_range
            .filter(|&l| acf[l] > 0.0 && acf[l] >= acf[l - 1] && acf[l] > acf[l + 1])
            .map(|l| (l as f64 + parabolic_offset(acf[l - 1], acf[l], acf[l + 1]), acf[l]))
            .collect();
        peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
        peaks.truncate(5);

        // Each strong period and its simple relatives.
        let mut candidates: Vec<f64> = Vec::new();
        for &(lag, _) in &peaks {
            let bpm = env.rate * 60.0 / lag;
            for ratio in [1.0, 2.0, 0.5, 1.5, 2.0 / 3.0, 4.0 / 3.0, 0.75] {
                let c = bpm * ratio;
                if (min_bpm..=max_bpm).contains(&c)
                    && !candidates.iter().any(|&o| (o / c - 1.0).abs() < 0.01)
                {
                    candidates.push(c);
                }
            }
        }

        let mut scored: Vec<(f64, f64)> = candidates
            .into_iter()
            .map(|bpm| {
                let lag = env.rate * 60.0 / bpm;
                let repeat = acf_near(&acf, lag) as f64;
                let pulse = fourier_magnitude(env, bpm / 60.0);
                let octaves = (bpm / PREFERRED_BPM).log2() / PREFERENCE_OCTAVES;
                let preference = (-0.5 * octaves * octaves).exp();
                (bpm, repeat.max(0.0) * pulse.sqrt() * preference)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));

        let &(bpm, best) = scored.first()?;
        if best <= 0.0 {
            return None;
        }
        let runner_up = scored.get(1).map_or(0.0, |s| s.1);
        Some(TempoEstimate {
            bpm,
            confidence: ((best - runner_up) / best) as f32,
        })
    }
}

/// Autocorrelation of a mean-removed signal, normalised so lag 0 is 1 and
/// unbiased (each lag divided by how many products it summed).
fn autocorrelation(signal: &[f32]) -> Vec<f32> {
    let n = signal.len();
    let len = (2 * n).next_power_of_two();
    let mean = signal.iter().sum::<f32>() / n as f32;

    let mut planner = RealFftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(len);
    let inverse = planner.plan_fft_inverse(len);

    let mut input = forward.make_input_vec();
    for (slot, &v) in input.iter_mut().zip(signal) {
        *slot = v - mean;
    }
    let mut spectrum = forward.make_output_vec();
    // Lengths are the planner's own, so neither transform can fail.
    let _ = forward.process(&mut input, &mut spectrum);
    for c in &mut spectrum {
        *c = Complex::new(c.norm_sqr(), 0.0);
    }
    let mut acf = inverse.make_output_vec();
    let _ = inverse.process(&mut spectrum, &mut acf);

    acf.truncate(n);
    let zero = acf[0] / n as f32;
    if zero <= 0.0 {
        return vec![0.0; n];
    }
    for (lag, v) in acf.iter_mut().enumerate() {
        *v /= (n - lag) as f32 * zero;
    }
    acf
}

/// The highest autocorrelation within 1.5% of `lag`, refined between lags.
fn acf_near(acf: &[f32], lag: f64) -> f32 {
    let lo = ((lag * 0.985).floor() as usize).max(1);
    let hi = ((lag * 1.015).ceil() as usize).min(acf.len() - 2);
    (lo..=hi)
        .map(|l| {
            let (a, b, c) = (acf[l - 1], acf[l], acf[l + 1]);
            let p = parabolic_offset(a, b, c) as f32;
            b - 0.25 * (a - c) * p
        })
        .fold(f32::MIN, f32::max)
}

/// Offset in (-0.5, 0.5) of the vertex of the parabola through three points.
fn parabolic_offset(a: f32, b: f32, c: f32) -> f64 {
    let denom = a - 2.0 * b + c;
    if denom.abs() < f32::EPSILON {
        0.0
    } else {
        (0.5 * (a - c) / denom).clamp(-0.5, 0.5) as f64
    }
}

/// How much of the envelope pulses at `hz`, averaged over windows of the
/// track: 1.0 for a pure pulse train at that rate, near 0 for one at half.
fn fourier_magnitude(env: &OnsetEnvelope, hz: f64) -> f64 {
    let window = (FOURIER_WINDOW_SECS * env.rate) as usize;
    let step = std::f64::consts::TAU * hz / env.rate;
    let rotate = (step.cos(), -step.sin());

    let (mut total, mut weight) = (0.0, 0.0);
    for chunk in env.values.chunks(window.max(1)) {
        let mass: f64 = chunk.iter().map(|&v| v as f64).sum();
        if mass <= 0.0 {
            continue;
        }
        // Rotate a phasor sample by sample instead of calling sin/cos for each.
        let (mut re, mut im) = (0.0, 0.0);
        let (mut pr, mut pi) = (1.0f64, 0.0f64);
        for &v in chunk {
            re += v as f64 * pr;
            im += v as f64 * pi;
            (pr, pi) = (pr * rotate.0 - pi * rotate.1, pr * rotate.1 + pi * rotate.0);
        }
        total += (re * re + im * im).sqrt();
        weight += mass;
    }
    if weight > 0.0 {
        total / weight
    } else {
        0.0
    }
}

impl Analyzer for BpmAnalyzer {
    fn name(&self) -> &'static str {
        "BPM"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        let env = onset_envelope(samples, sample_rate);
        let estimate = self.estimate(&env).ok_or(AnalysisError::InsufficientData)?;
        Ok(AnalyzerOutput::Bpm {
            bpm: estimate.bpm as f32,
            confidence: estimate.confidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::{add_hat, add_kick, beat_times, click_track, silence};

    fn estimate(samples: &[f32], rate: u32) -> f64 {
        let env = onset_envelope(samples, rate);
        BpmAnalyzer::new().estimate(&env).unwrap().bpm
    }

    #[test]
    fn test_bpm_with_custom_range() {
        let analyzer = BpmAnalyzer::with_bpm_range(80.0, 160.0);
        assert_eq!(analyzer.min_bpm, 80.0);
        assert_eq!(analyzer.max_bpm, 160.0);
    }

    #[test]
    fn test_insufficient_data() {
        let analyzer = BpmAnalyzer::new();
        let samples = vec![0.0f32; 1000]; // Too short
        let result = analyzer.analyze(&samples, 44100);
        assert!(matches!(result, Err(AnalysisError::InsufficientData)));
    }

    #[test]
    fn finds_tempos_between_envelope_frames() {
        for bpm in [124.0, 128.0, 140.0, 145.0, 150.0, 174.0] {
            let found = estimate(&click_track(44_100, bpm, 0.1, 40.0), 44_100);
            assert!((found - bpm).abs() < 0.6, "{bpm}: {found}");
        }
    }

    #[test]
    fn off_beat_hats_do_not_make_one_and_a_half_beats() {
        let rate = 44_100;
        let mut samples = silence(rate, 40.0);
        for t in beat_times(128.0, 0.1, 40.0) {
            add_kick(&mut samples, rate, t, 0.8);
            add_hat(&mut samples, rate, t + 60.0 / 128.0 / 2.0, 0.9);
        }
        let found = estimate(&samples, rate);
        assert!((found - 128.0).abs() < 0.6, "{found}");
    }
}
