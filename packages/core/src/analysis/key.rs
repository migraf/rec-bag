//! Key detection from a harmonic pitch-class profile.
//!
//! The algorithm works in several stages:
//! 1. Find the spectral peaks of each frame. Only peaks that stand above the
//!    local spectral level count, so the broadband noise of kicks and hats,
//!    which in dance music carries most of the energy, adds nothing.
//! 2. Measure the track's tuning from where the peaks fall between semitones,
//!    so a master a few cents off concert pitch does not smear into its
//!    neighbouring keys.
//! 3. Fold each peak into a 12-bin pitch-class profile. Each frame is
//!    normalised before summing, so a loud drop does not outweigh a melodic
//!    breakdown.
//! 4. Correlate with Krumhansl-Kessler key profiles.

use super::fft::{moving_average, Stft};
use super::traits::{Analyzer, AnalyzerOutput};
use super::types::{AnalysisError, MusicalKey};
use std::f32::consts::PI;

/// ~186 ms at 44.1 kHz: 5.4 Hz bins, enough to separate notes down to ~90 Hz.
const FRAME: usize = 8192;
const HOP: usize = 4096;
/// Peaks are taken from this range. Below it bins are wider than semitones,
/// above it there is little but cymbal noise.
const MIN_HZ: f32 = 80.0;
const MAX_HZ: f32 = 5000.0;
/// Width in bins of the local spectral level a peak must stand above.
const LEVEL_WIDTH: usize = 41;
/// How far above the local level a peak must stand to count.
const PEAK_RATIO: f32 = 2.0;
/// Width in semitones over which a peak spreads into neighbouring pitch classes.
const SPREAD: f32 = 4.0 / 3.0;

/// Key analyzer using a harmonic pitch-class profile and key-profile matching.
pub struct KeyAnalyzer;

impl Default for KeyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// A spectral peak: frequency in Hz and how far it stands above its surroundings.
#[derive(Debug, Clone, Copy)]
struct Peak {
    hz: f32,
    weight: f32,
}

impl KeyAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Krumhansl-Kessler major key profile (C major)
    const MAJOR_PROFILE: [f32; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];

    /// Krumhansl-Kessler minor key profile (C minor)
    const MINOR_PROFILE: [f32; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];

    /// Spectral peaks of every frame.
    fn frame_peaks(&self, samples: &[f32], sample_rate: u32) -> Vec<Vec<Peak>> {
        let mut stft = Stft::new(FRAME, HOP);
        let bin_hz = stft.bin_hz(sample_rate);
        let lo = ((MIN_HZ / bin_hz) as usize).max(1);
        let hi = ((MAX_HZ / bin_hz) as usize).min(stft.bins() - 2);

        let mut frames = Vec::with_capacity(stft.frame_count(samples.len()));
        stft.for_each_frame(samples, |_, mags| {
            let level = moving_average(mags, LEVEL_WIDTH);
            let loudest = mags[lo..=hi].iter().copied().fold(0.0f32, f32::max);
            let mut peaks = Vec::new();
            if loudest > 1e-6 {
                for b in lo..=hi {
                    let m = mags[b];
                    if m > mags[b - 1] && m >= mags[b + 1] && m > PEAK_RATIO * level[b] && m > 1e-3 * loudest {
                        // Refine the frequency between bins on the log spectrum.
                        let (a, c) = (mags[b - 1].max(1e-12).ln(), mags[b + 1].max(1e-12).ln());
                        let mid = m.ln();
                        let denom = a - 2.0 * mid + c;
                        let offset = if denom.abs() > f32::EPSILON {
                            (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
                        } else {
                            0.0
                        };
                        peaks.push(Peak {
                            hz: (b as f32 + offset) * bin_hz,
                            weight: m / level[b],
                        });
                    }
                }
            }
            frames.push(peaks);
        });
        frames
    }

    /// The track's deviation from A440 in cents, from where its peaks fall
    /// between semitones (a circular mean: -50 and +50 cents are neighbours).
    fn tuning_cents(frames: &[Vec<Peak>]) -> f32 {
        let (mut x, mut y) = (0.0f64, 0.0f64);
        for peak in frames.iter().flatten() {
            let cents = 1200.0 * (peak.hz / 440.0).log2();
            let angle = 2.0 * PI * cents / 100.0;
            x += (peak.weight * angle.cos()) as f64;
            y += (peak.weight * angle.sin()) as f64;
        }
        if x == 0.0 && y == 0.0 {
            0.0
        } else {
            (y.atan2(x) as f32) * 100.0 / (2.0 * PI)
        }
    }

    /// Sum of per-frame normalised pitch-class profiles, normalised to a peak of 1.
    fn chromagram(frames: &[Vec<Peak>], tuning_cents: f32) -> [f32; 12] {
        let reference = 440.0 * 2f32.powf(tuning_cents / 1200.0);
        let mut total = [0.0f32; 12];
        for peaks in frames {
            let mut chroma = [0.0f32; 12];
            for peak in peaks {
                let semitone = 69.0 + 12.0 * (peak.hz / reference).log2();
                let nearest = semitone.round();
                for pitch in [nearest - 1.0, nearest, nearest + 1.0] {
                    let d = semitone - pitch;
                    if d.abs() < SPREAD / 2.0 {
                        let w = (PI * d / SPREAD).cos();
                        chroma[(pitch as i32).rem_euclid(12) as usize] += peak.weight * w * w;
                    }
                }
            }
            let max = chroma.iter().copied().fold(0.0f32, f32::max);
            if max > 0.0 {
                for (t, c) in total.iter_mut().zip(chroma) {
                    *t += c / max;
                }
            }
        }
        let max = total.iter().copied().fold(0.0f32, f32::max).max(1e-9);
        total.map(|t| t / max)
    }

    /// Find the key whose profile best matches the chromagram.
    fn find_best_key(&self, chroma: &[f32; 12]) -> (MusicalKey, f32) {
        let mut best_key = MusicalKey::CMajor;
        let mut best_correlation = f32::NEG_INFINITY;

        for root in 0..12 {
            let rotated = self.rotate_chroma(chroma, root);

            let major_corr = self.pearson_correlation(&rotated, &Self::MAJOR_PROFILE);
            if major_corr > best_correlation {
                best_correlation = major_corr;
                best_key = Self::pitch_class_to_major_key(root);
            }

            let minor_corr = self.pearson_correlation(&rotated, &Self::MINOR_PROFILE);
            if minor_corr > best_correlation {
                best_correlation = minor_corr;
                best_key = Self::pitch_class_to_minor_key(root);
            }
        }

        // Map correlation into a 0.0 to 1.0 confidence.
        let confidence = ((best_correlation - 0.3) / 0.6).clamp(0.0, 1.0);
        (best_key, confidence)
    }

    /// Rotate chromagram by a number of semitones.
    fn rotate_chroma(&self, chroma: &[f32; 12], semitones: usize) -> [f32; 12] {
        let mut rotated = [0.0f32; 12];
        for i in 0..12 {
            rotated[i] = chroma[(i + semitones) % 12];
        }
        rotated
    }

    /// Compute Pearson correlation coefficient between two arrays.
    fn pearson_correlation(&self, a: &[f32; 12], b: &[f32; 12]) -> f32 {
        let mean_a: f32 = a.iter().sum::<f32>() / 12.0;
        let mean_b: f32 = b.iter().sum::<f32>() / 12.0;

        let mut numerator = 0.0f32;
        let mut var_a = 0.0f32;
        let mut var_b = 0.0f32;

        for i in 0..12 {
            let diff_a = a[i] - mean_a;
            let diff_b = b[i] - mean_b;
            numerator += diff_a * diff_b;
            var_a += diff_a * diff_a;
            var_b += diff_b * diff_b;
        }

        let denominator = (var_a * var_b).sqrt();
        if denominator > 0.0 {
            numerator / denominator
        } else {
            0.0
        }
    }

    /// Convert pitch class (0-11) to major key.
    fn pitch_class_to_major_key(pitch_class: usize) -> MusicalKey {
        match pitch_class {
            0 => MusicalKey::CMajor,
            1 => MusicalKey::DbMajor,
            2 => MusicalKey::DMajor,
            3 => MusicalKey::EbMajor,
            4 => MusicalKey::EMajor,
            5 => MusicalKey::FMajor,
            6 => MusicalKey::GbMajor,
            7 => MusicalKey::GMajor,
            8 => MusicalKey::AbMajor,
            9 => MusicalKey::AMajor,
            10 => MusicalKey::BbMajor,
            11 => MusicalKey::BMajor,
            _ => MusicalKey::CMajor,
        }
    }

    /// Convert pitch class (0-11) to minor key.
    fn pitch_class_to_minor_key(pitch_class: usize) -> MusicalKey {
        match pitch_class {
            0 => MusicalKey::CMinor,
            1 => MusicalKey::DbMinor,
            2 => MusicalKey::DMinor,
            3 => MusicalKey::EbMinor,
            4 => MusicalKey::EMinor,
            5 => MusicalKey::FMinor,
            6 => MusicalKey::GbMinor,
            7 => MusicalKey::GMinor,
            8 => MusicalKey::AbMinor,
            9 => MusicalKey::AMinor,
            10 => MusicalKey::BbMinor,
            11 => MusicalKey::BMinor,
            _ => MusicalKey::CMinor,
        }
    }
}

impl Analyzer for KeyAnalyzer {
    fn name(&self) -> &'static str {
        "Key"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        // Need at least a few seconds of audio
        let min_samples = sample_rate as usize * 5;
        if samples.len() < min_samples {
            return Err(AnalysisError::InsufficientData);
        }

        let frames = self.frame_peaks(samples, sample_rate);
        if frames.iter().all(Vec::is_empty) {
            return Err(AnalysisError::InsufficientData);
        }
        let chroma = Self::chromagram(&frames, Self::tuning_cents(&frames));
        let (key, confidence) = self.find_best_key(&chroma);

        Ok(AnalyzerOutput::Key { key, confidence })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::test_signals::{add_kick, add_tone, beat_times, silence};

    const RATE: u32 = 44_100;

    fn key_of(samples: &[f32]) -> MusicalKey {
        match KeyAnalyzer::new().analyze(samples, RATE) {
            Ok(AnalyzerOutput::Key { key, .. }) => key,
            other => panic!("{other:?}"),
        }
    }

    fn hz(midi: f32, cents: f32) -> f32 {
        440.0 * 2f32.powf((midi - 69.0 + cents / 100.0) / 12.0)
    }

    /// Chords (as MIDI notes) for two seconds each, over a kick on every beat.
    fn progression(chords: &[&[f32]], cents: f32) -> Vec<f32> {
        let secs = 2.0 * chords.len() as f64 * 2.0;
        let mut samples = silence(RATE, secs);
        for (i, chord) in chords.iter().cycle().take(chords.len() * 2).enumerate() {
            let from = i as f64 * 2.0;
            for &note in chord.iter() {
                add_tone(&mut samples, RATE, from, from + 2.0, hz(note, cents), 0.1);
                add_tone(&mut samples, RATE, from, from + 2.0, hz(note + 12.0, cents), 0.04);
            }
        }
        for t in beat_times(128.0, 0.0, secs) {
            add_kick(&mut samples, RATE, t, 0.8);
        }
        samples
    }

    #[test]
    fn an_a_minor_progression_is_a_minor() {
        // Am - F - C - G - Am - Dm - E
        let chords: [&[f32]; 7] = [
            &[57.0, 60.0, 64.0],
            &[53.0, 57.0, 60.0],
            &[48.0, 52.0, 55.0],
            &[55.0, 59.0, 62.0],
            &[57.0, 60.0, 64.0],
            &[50.0, 53.0, 57.0],
            &[52.0, 56.0, 59.0],
        ];
        assert_eq!(key_of(&progression(&chords, 0.0)), MusicalKey::AMinor);
    }

    #[test]
    fn a_d_major_progression_is_d_major() {
        // D - G - A - D - Bm - G - A - D
        let chords: [&[f32]; 8] = [
            &[50.0, 54.0, 57.0],
            &[55.0, 59.0, 62.0],
            &[57.0, 61.0, 64.0],
            &[50.0, 54.0, 57.0],
            &[59.0, 62.0, 66.0],
            &[55.0, 59.0, 62.0],
            &[57.0, 61.0, 64.0],
            &[50.0, 54.0, 57.0],
        ];
        assert_eq!(key_of(&progression(&chords, 0.0)), MusicalKey::DMajor);
    }

    #[test]
    fn a_detuned_master_keeps_its_key() {
        // Fm - Bbm - C - Fm, 40 cents sharp: nearly a semitone up to F#m.
        let chords: [&[f32]; 4] = [
            &[53.0, 56.0, 60.0],
            &[58.0, 61.0, 65.0],
            &[48.0, 52.0, 55.0],
            &[53.0, 56.0, 60.0],
        ];
        let samples = progression(&chords, 40.0);
        let frames = KeyAnalyzer::new().frame_peaks(&samples, RATE);
        let tuning = KeyAnalyzer::tuning_cents(&frames);
        assert!((tuning - 40.0).abs() < 8.0, "{tuning}");
        assert_eq!(key_of(&samples), MusicalKey::FMinor);
    }

    #[test]
    fn test_pearson_correlation() {
        let analyzer = KeyAnalyzer::new();

        // Identical arrays should have correlation of 1.0
        let a = [
            1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0,
        ];
        let corr = analyzer.pearson_correlation(&a, &a);
        assert!((corr - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_insufficient_data() {
        let analyzer = KeyAnalyzer::new();
        let samples = vec![0.0f32; 1000]; // Too short
        let result = analyzer.analyze(&samples, 44100);
        assert!(matches!(result, Err(AnalysisError::InsufficientData)));
    }
}
