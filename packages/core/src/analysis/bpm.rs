//! BPM detection using onset detection and autocorrelation.
//!
//! The algorithm works in several stages:
//! 1. Compute spectral flux (onset strength) using FFT
//! 2. Peak-pick the onset strength to find beat candidates
//! 3. Use autocorrelation to find the dominant periodicity
//! 4. Convert periodicity to BPM

use super::traits::{Analyzer, AnalyzerOutput};
use super::types::AnalysisError;
use rustfft::{num_complex::Complex, FftPlanner};

/// BPM analyzer using spectral flux and autocorrelation.
pub struct BpmAnalyzer {
    /// Minimum BPM to detect.
    min_bpm: f32,
    /// Maximum BPM to detect.
    max_bpm: f32,
    /// FFT window size for spectral analysis.
    fft_size: usize,
    /// Hop size between FFT frames.
    hop_size: usize,
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
            min_bpm: 60.0,
            max_bpm: 200.0,
            fft_size: 2048,
            hop_size: 512,
        }
    }

    /// Create a BPM analyzer with custom BPM range.
    pub fn with_bpm_range(min_bpm: f32, max_bpm: f32) -> Self {
        Self {
            min_bpm,
            max_bpm,
            ..Self::new()
        }
    }

    /// Compute spectral flux (onset strength) from audio samples.
    fn compute_spectral_flux(&self, samples: &[f32], sample_rate: u32) -> Vec<f32> {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(self.fft_size);

        let mut prev_magnitudes = vec![0.0f32; self.fft_size / 2 + 1];
        let mut onset_strength = Vec::new();

        // Hann window
        let window: Vec<f32> = (0..self.fft_size)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / self.fft_size as f32).cos())
            })
            .collect();

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

            // Compute magnitudes (only positive frequencies)
            let magnitudes: Vec<f32> = buffer[..self.fft_size / 2 + 1]
                .iter()
                .map(|c| c.norm())
                .collect();

            // Compute spectral flux (sum of positive differences)
            let flux: f32 = magnitudes
                .iter()
                .zip(prev_magnitudes.iter())
                .map(|(curr, prev)| (curr - prev).max(0.0))
                .sum();

            onset_strength.push(flux);
            prev_magnitudes = magnitudes;
            pos += self.hop_size;
        }

        // Normalize onset strength
        let max_onset = onset_strength
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .copied()
            .unwrap_or(1.0)
            .max(0.001);

        for v in &mut onset_strength {
            *v /= max_onset;
        }

        // Apply a simple low-pass filter to smooth the onset curve
        let smoothed = self.smooth_signal(&onset_strength, 3);

        // Subtract local mean to enhance peaks
        self.enhance_peaks(&smoothed, sample_rate)
    }

    /// Smooth signal with a simple moving average.
    fn smooth_signal(&self, signal: &[f32], window_size: usize) -> Vec<f32> {
        if signal.len() < window_size {
            return signal.to_vec();
        }

        let mut smoothed = Vec::with_capacity(signal.len());
        let half_window = window_size / 2;

        for i in 0..signal.len() {
            let start = i.saturating_sub(half_window);
            let end = (i + half_window + 1).min(signal.len());
            let sum: f32 = signal[start..end].iter().sum();
            smoothed.push(sum / (end - start) as f32);
        }

        smoothed
    }

    /// Enhance peaks by subtracting local mean.
    fn enhance_peaks(&self, signal: &[f32], sample_rate: u32) -> Vec<f32> {
        // Use a window of about 0.5 seconds for local mean
        let onset_rate = sample_rate as f32 / self.hop_size as f32;
        let window_size = (onset_rate * 0.5) as usize;
        let window_size = window_size.max(3);

        let local_mean = self.smooth_signal(signal, window_size);

        signal
            .iter()
            .zip(local_mean.iter())
            .map(|(s, m)| (s - m).max(0.0))
            .collect()
    }

    /// Compute autocorrelation of onset strength to find tempo.
    fn compute_autocorrelation(&self, onset_strength: &[f32], sample_rate: u32) -> (f32, f32) {
        let onset_rate = sample_rate as f32 / self.hop_size as f32;

        // Convert BPM range to lag range (in onset frames)
        let min_lag = (onset_rate * 60.0 / self.max_bpm) as usize;
        let max_lag = (onset_rate * 60.0 / self.min_bpm) as usize;

        if max_lag >= onset_strength.len() || min_lag >= max_lag {
            return (120.0, 0.0); // Default to 120 BPM with zero confidence
        }

        // Compute autocorrelation for each lag
        let mut correlations = Vec::with_capacity(max_lag - min_lag + 1);
        let mut max_corr = 0.0f32;

        for lag in min_lag..=max_lag {
            let mut corr = 0.0;
            let mut count = 0;

            for i in 0..(onset_strength.len() - lag) {
                corr += onset_strength[i] * onset_strength[i + lag];
                count += 1;
            }

            if count > 0 {
                corr /= count as f32;
            }

            correlations.push((lag, corr));
            if corr > max_corr {
                max_corr = corr;
            }
        }

        // Find the best lag (highest correlation)
        let best = correlations
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(lag, corr)| (*lag, *corr))
            .unwrap_or((min_lag, 0.0));

        // Convert lag to BPM
        let bpm = onset_rate * 60.0 / best.0 as f32;

        // Calculate confidence based on correlation strength relative to average
        let avg_corr: f32 =
            correlations.iter().map(|(_, c)| c).sum::<f32>() / correlations.len() as f32;
        let confidence = if avg_corr > 0.0 {
            ((best.1 / avg_corr) - 1.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Check for double/half tempo
        let (bpm, confidence) = self.refine_tempo(bpm, confidence, &correlations, onset_rate);

        (bpm, confidence)
    }

    /// Refine tempo by checking for double/half tempo candidates.
    /// Uses perceptual weighting to prefer tempos in the typical 100-150 BPM range.
    fn refine_tempo(
        &self,
        bpm: f32,
        confidence: f32,
        correlations: &[(usize, f32)],
        onset_rate: f32,
    ) -> (f32, f32) {
        let mut final_bpm = bpm;

        // Get correlation for a given BPM
        let get_correlation = |target_bpm: f32| -> Option<f32> {
            let lag = (onset_rate * 60.0 / target_bpm) as usize;
            correlations
                .iter()
                .find(|(l, _)| *l == lag)
                .map(|(_, c)| *c)
        };

        // Perceptual weight: prefer tempos in the 100-150 BPM range (typical for most music)
        let perceptual_weight = |tempo: f32| -> f32 {
            // Gaussian-like weighting centered around 125 BPM
            let center = 125.0;
            let sigma = 35.0;
            let diff = tempo - center;
            (-(diff * diff) / (2.0 * sigma * sigma)).exp()
        };

        // Score a tempo candidate based on correlation and perceptual preference
        let score_tempo =
            |tempo: f32, corr: f32| -> f32 { corr * (1.0 + perceptual_weight(tempo) * 0.5) };

        let original_corr = get_correlation(bpm).unwrap_or(0.0);
        let mut best_score = score_tempo(bpm, original_corr);

        // Always check doubled tempo if it's in valid range
        let double_bpm = bpm * 2.0;
        if double_bpm <= self.max_bpm {
            if let Some(double_corr) = get_correlation(double_bpm) {
                let double_score = score_tempo(double_bpm, double_corr);
                // Bias towards doubling: accept if score is at least 60% as good
                // This helps because half-tempo often has stronger raw correlation
                if double_score > best_score * 0.6 {
                    // Additional check: if original is below 90 BPM and double is in sweet spot,
                    // be even more aggressive about doubling
                    let should_double = if bpm < 90.0 && double_bpm >= 100.0 && double_bpm <= 160.0
                    {
                        double_corr > original_corr * 0.4 // Very aggressive for suspiciously low tempos
                    } else {
                        double_score > best_score * 0.6
                    };

                    if should_double {
                        final_bpm = double_bpm;
                        best_score = double_score;
                    }
                }
            }
        }

        // Check halved tempo if it's in valid range and current tempo is very high
        let half_bpm = bpm / 2.0;
        if half_bpm >= self.min_bpm && bpm > 170.0 {
            if let Some(half_corr) = get_correlation(half_bpm) {
                let half_score = score_tempo(half_bpm, half_corr);
                if half_score > best_score {
                    final_bpm = half_bpm;
                }
            }
        }

        (final_bpm, confidence)
    }
}

impl Analyzer for BpmAnalyzer {
    fn name(&self) -> &'static str {
        "BPM"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        // Need at least a few seconds of audio
        let min_samples = sample_rate as usize * 5;
        if samples.len() < min_samples {
            return Err(AnalysisError::InsufficientData);
        }

        // Use middle portion of the track (skip intro/outro)
        let start = samples.len() / 4;
        let end = (samples.len() * 3) / 4;
        let analysis_samples = &samples[start..end];

        // Compute onset strength
        let onset_strength = self.compute_spectral_flux(analysis_samples, sample_rate);

        if onset_strength.len() < 100 {
            return Err(AnalysisError::InsufficientData);
        }

        // Find tempo via autocorrelation
        let (bpm, confidence) = self.compute_autocorrelation(&onset_strength, sample_rate);

        // Round BPM to one decimal place
        let bpm = (bpm * 10.0).round() / 10.0;

        Ok(AnalyzerOutput::Bpm { bpm, confidence })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bpm_analyzer_creation() {
        let analyzer = BpmAnalyzer::new();
        assert_eq!(analyzer.min_bpm, 60.0);
        assert_eq!(analyzer.max_bpm, 200.0);
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
}
