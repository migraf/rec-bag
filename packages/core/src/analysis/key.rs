//! Key detection using chromagram analysis and key profile matching.
//!
//! The algorithm works in several stages:
//! 1. Compute chromagram (12 pitch class profile) from FFT
//! 2. Average chromagram across all frames
//! 3. Correlate with Krumhansl-Kessler key profiles
//! 4. Return the key with highest correlation

use super::traits::{Analyzer, AnalyzerOutput};
use super::types::{AnalysisError, MusicalKey};
use rustfft::{num_complex::Complex, FftPlanner};

/// Key analyzer using chromagram and key profile correlation.
pub struct KeyAnalyzer {
    /// FFT window size for spectral analysis.
    fft_size: usize,
    /// Hop size between FFT frames.
    hop_size: usize,
}

impl Default for KeyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyAnalyzer {
    /// Create a new key analyzer with default settings.
    pub fn new() -> Self {
        Self {
            fft_size: 4096,
            hop_size: 2048,
        }
    }

    /// Krumhansl-Kessler major key profile.
    /// Weights for each pitch class (C, C#, D, ..., B) in a major key.
    const MAJOR_PROFILE: [f32; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];

    /// Krumhansl-Kessler minor key profile.
    /// Weights for each pitch class (C, C#, D, ..., B) in a minor key.
    const MINOR_PROFILE: [f32; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];

    /// Compute chromagram from audio samples.
    fn compute_chromagram(&self, samples: &[f32], sample_rate: u32) -> [f32; 12] {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(self.fft_size);

        // Hann window
        let window: Vec<f32> = (0..self.fft_size)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / self.fft_size as f32).cos())
            })
            .collect();

        let mut chroma_sum = [0.0f32; 12];
        let mut frame_count = 0;

        // Frequency resolution
        let freq_resolution = sample_rate as f32 / self.fft_size as f32;

        // Process frames
        let mut pos = 0;
        while pos + self.fft_size <= samples.len() {
            // Apply window and prepare complex buffer
            let mut buffer: Vec<Complex<f32>> = samples[pos..pos + self.fft_size]
                .iter()
                .zip(window.iter())
                .map(|(s, w)| Complex::new(s * w, 0.0))
                .collect();

            // Compute FFT
            fft.process(&mut buffer);

            // Compute magnitudes and accumulate into pitch classes
            let frame_chroma = self.fft_to_chroma(&buffer, freq_resolution);

            for (i, val) in frame_chroma.iter().enumerate() {
                chroma_sum[i] += val;
            }

            frame_count += 1;
            pos += self.hop_size;
        }

        // Average chromagram
        if frame_count > 0 {
            for val in &mut chroma_sum {
                *val /= frame_count as f32;
            }
        }

        // Normalize chromagram
        let max_val = chroma_sum
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .copied()
            .unwrap_or(1.0)
            .max(0.001);

        for val in &mut chroma_sum {
            *val /= max_val;
        }

        chroma_sum
    }

    /// Convert FFT magnitudes to chroma bins.
    fn fft_to_chroma(&self, fft_buffer: &[Complex<f32>], freq_resolution: f32) -> [f32; 12] {
        let mut chroma = [0.0f32; 12];

        // Only process positive frequencies up to Nyquist
        let num_bins = self.fft_size / 2;

        for bin in 1..num_bins {
            let freq = bin as f32 * freq_resolution;

            // Only consider frequencies in musical range (roughly 50Hz - 5000Hz)
            if freq < 50.0 || freq > 5000.0 {
                continue;
            }

            // Convert frequency to pitch class (0 = C, 1 = C#, etc.)
            let pitch_class = self.freq_to_pitch_class(freq);
            let magnitude = fft_buffer[bin].norm();

            // Use magnitude squared for more weight on strong frequencies
            chroma[pitch_class] += magnitude * magnitude;
        }

        chroma
    }

    /// Convert frequency (Hz) to pitch class (0-11).
    fn freq_to_pitch_class(&self, freq: f32) -> usize {
        // A4 = 440 Hz, A is pitch class 9
        // MIDI note number = 69 + 12 * log2(freq / 440)
        // Pitch class = MIDI note number % 12

        let midi_note = 69.0 + 12.0 * (freq / 440.0).log2();
        let pitch_class = (midi_note.round() as i32).rem_euclid(12) as usize;
        pitch_class
    }

    /// Correlate chromagram with key profiles and find best match.
    fn find_best_key(&self, chroma: &[f32; 12]) -> (MusicalKey, f32) {
        let mut best_key = MusicalKey::CMajor;
        let mut best_correlation = f32::NEG_INFINITY;

        // Test all 24 keys (12 major + 12 minor)
        for root in 0..12 {
            // Rotate the chromagram so root becomes index 0
            let rotated = self.rotate_chroma(chroma, root);

            // Test major key
            let major_corr = self.pearson_correlation(&rotated, &Self::MAJOR_PROFILE);
            let major_key = Self::pitch_class_to_major_key(root);

            if major_corr > best_correlation {
                best_correlation = major_corr;
                best_key = major_key;
            }

            // Test minor key
            let minor_corr = self.pearson_correlation(&rotated, &Self::MINOR_PROFILE);
            let minor_key = Self::pitch_class_to_minor_key(root);

            if minor_corr > best_correlation {
                best_correlation = minor_corr;
                best_key = minor_key;
            }
        }

        // Convert correlation to confidence (correlation is typically 0.3-0.9)
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

    /// Compute Pearson correlation coefficient.
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

        // Use middle portion of the track (skip intro/outro which may be in different keys)
        let start = samples.len() / 4;
        let end = (samples.len() * 3) / 4;
        let analysis_samples = &samples[start..end];

        // Compute chromagram
        let chromagram = self.compute_chromagram(analysis_samples, sample_rate);

        // Find best matching key
        let (key, confidence) = self.find_best_key(&chromagram);

        Ok(AnalyzerOutput::Key { key, confidence })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_analyzer_creation() {
        let analyzer = KeyAnalyzer::new();
        assert_eq!(analyzer.fft_size, 4096);
    }

    #[test]
    fn test_freq_to_pitch_class() {
        let analyzer = KeyAnalyzer::new();

        // A4 = 440 Hz should map to pitch class 9 (A)
        assert_eq!(analyzer.freq_to_pitch_class(440.0), 9);

        // C4 = 261.63 Hz should map to pitch class 0 (C)
        assert_eq!(analyzer.freq_to_pitch_class(261.63), 0);

        // E4 = 329.63 Hz should map to pitch class 4 (E)
        assert_eq!(analyzer.freq_to_pitch_class(329.63), 4);
    }

    #[test]
    fn test_pearson_correlation() {
        let analyzer = KeyAnalyzer::new();

        // Identical arrays should have correlation of 1.0
        let a = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0];
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

