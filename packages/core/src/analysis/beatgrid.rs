//! Beat grid detection using onset detection and BPM.
//!
//! The algorithm works in several stages:
//! 1. Compute onset strength using spectral flux
//! 2. Find prominent onset peaks
//! 3. Estimate the first beat position by aligning to detected onsets
//! 4. Generate beat positions array based on BPM

use super::fft::{normalize, spectral_flux, FftProcessor, WindowType};
use super::traits::{Analyzer, AnalyzerOutput};
use super::types::AnalysisError;

/// Beat grid analyzer - detects first beat and generates beat positions.
pub struct BeatGridAnalyzer {
    /// FFT window size for onset detection.
    fft_size: usize,
    /// Hop size between FFT frames.
    hop_size: usize,
    /// Minimum peak prominence for onset detection.
    peak_threshold: f32,
}

impl Default for BeatGridAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatGridAnalyzer {
    /// Create a new beat grid analyzer with default settings.
    pub fn new() -> Self {
        Self {
            fft_size: 2048,
            hop_size: 512,
            peak_threshold: 0.3,
        }
    }

    /// Compute onset strength curve using spectral flux.
    fn compute_onset_strength(&self, samples: &[f32], sample_rate: u32) -> Vec<f32> {
        let mut processor = FftProcessor::new(self.fft_size, WindowType::Hann);
        let mut prev_magnitudes = vec![0.0f32; self.fft_size / 2 + 1];
        let mut onset_strength = Vec::new();

        let mut pos = 0;
        while pos + self.fft_size <= samples.len() {
            let frame = &samples[pos..pos + self.fft_size];
            let fft_result = processor.compute_fft(frame);
            let magnitudes = processor.compute_magnitudes(&fft_result);

            // Compute spectral flux (sum of positive differences)
            let flux = spectral_flux(&prev_magnitudes, &magnitudes);
            onset_strength.push(flux);

            prev_magnitudes = magnitudes;
            pos += self.hop_size;
        }

        // Normalize
        normalize(&mut onset_strength);

        // Apply peak enhancement (subtract local mean)
        self.enhance_peaks(&onset_strength, sample_rate)
    }

    /// Enhance peaks by subtracting local mean.
    fn enhance_peaks(&self, signal: &[f32], sample_rate: u32) -> Vec<f32> {
        let onset_rate = sample_rate as f32 / self.hop_size as f32;
        let window_size = (onset_rate * 0.3) as usize; // 300ms window
        let window_size = window_size.max(3);

        let local_mean = self.smooth_signal(signal, window_size);

        signal
            .iter()
            .zip(local_mean.iter())
            .map(|(s, m)| (s - m).max(0.0))
            .collect()
    }

    /// Smooth signal with moving average.
    fn smooth_signal(&self, signal: &[f32], window_size: usize) -> Vec<f32> {
        if signal.len() < window_size {
            return signal.to_vec();
        }

        let half_window = window_size / 2;
        let mut smoothed = Vec::with_capacity(signal.len());

        for i in 0..signal.len() {
            let start = i.saturating_sub(half_window);
            let end = (i + half_window + 1).min(signal.len());
            let sum: f32 = signal[start..end].iter().sum();
            smoothed.push(sum / (end - start) as f32);
        }

        smoothed
    }

    /// Find onset peaks above threshold.
    fn find_onset_peaks(&self, onset_strength: &[f32]) -> Vec<usize> {
        let mut peaks = Vec::new();

        for i in 1..onset_strength.len().saturating_sub(1) {
            // Local maximum above threshold
            if onset_strength[i] > self.peak_threshold
                && onset_strength[i] > onset_strength[i - 1]
                && onset_strength[i] >= onset_strength[i + 1]
            {
                peaks.push(i);
            }
        }

        peaks
    }

    /// Convert onset frame index to time in milliseconds.
    fn frame_to_ms(&self, frame: usize, sample_rate: u32) -> f64 {
        let sample_pos = frame * self.hop_size;
        (sample_pos as f64 / sample_rate as f64) * 1000.0
    }

    /// Estimate first beat position by finding best alignment with onsets.
    fn estimate_first_beat(
        &self,
        onset_peaks: &[usize],
        beat_interval_frames: f64,
        total_frames: usize,
        sample_rate: u32,
    ) -> f64 {
        if onset_peaks.is_empty() {
            return 0.0;
        }

        // Try different starting positions and score them by onset alignment
        let search_range = beat_interval_frames as usize;
        let mut best_offset = 0;
        let mut best_score = 0.0;

        for offset in 0..search_range.min(onset_peaks.len().max(1)) {
            let mut score = 0.0;
            let mut beat_pos = offset as f64;

            while beat_pos < total_frames as f64 {
                // Find nearest onset to this beat position
                let beat_frame = beat_pos as usize;
                let mut min_dist = beat_interval_frames;

                for &peak in onset_peaks {
                    let dist = (peak as f64 - beat_pos).abs();
                    if dist < min_dist {
                        min_dist = dist;
                    }
                }

                // Score inversely proportional to distance
                // Onsets within 1/8 of beat interval get full score
                let tolerance = beat_interval_frames / 8.0;
                if min_dist < tolerance {
                    score += 1.0 - (min_dist / tolerance);
                }

                beat_pos += beat_interval_frames;
            }

            if score > best_score {
                best_score = score;
                best_offset = offset;
            }
        }

        // Find the first strong onset near the best offset
        let target_frame = best_offset;
        let nearest_onset = onset_peaks
            .iter()
            .min_by(|a, b| {
                let dist_a = (**a as i64 - target_frame as i64).abs();
                let dist_b = (**b as i64 - target_frame as i64).abs();
                dist_a.cmp(&dist_b)
            })
            .copied()
            .unwrap_or(0);

        self.frame_to_ms(nearest_onset, sample_rate)
    }

    /// Generate beat positions from first beat and BPM.
    pub fn generate_beat_positions(
        first_beat_ms: f64,
        beat_interval_ms: f64,
        duration_ms: f64,
    ) -> Vec<f64> {
        let mut beats = Vec::new();
        let mut pos = first_beat_ms;

        // Also include beats before first_beat_ms if applicable
        while pos > beat_interval_ms {
            pos -= beat_interval_ms;
        }

        while pos < duration_ms {
            if pos >= 0.0 {
                beats.push(pos);
            }
            pos += beat_interval_ms;
        }

        beats
    }

    /// Analyze with known BPM (more accurate when BPM is pre-computed).
    pub fn analyze_with_bpm(
        &self,
        samples: &[f32],
        sample_rate: u32,
        bpm: f32,
    ) -> Result<AnalyzerOutput, AnalysisError> {
        if samples.is_empty() {
            return Err(AnalysisError::InsufficientData);
        }

        let beat_interval_ms = 60000.0 / bpm as f64;
        let duration_ms = (samples.len() as f64 / sample_rate as f64) * 1000.0;

        // Compute onset strength
        let onset_strength = self.compute_onset_strength(samples, sample_rate);

        if onset_strength.len() < 10 {
            return Err(AnalysisError::InsufficientData);
        }

        // Find onset peaks
        let onset_peaks = self.find_onset_peaks(&onset_strength);

        // Calculate beat interval in frames
        let onset_rate = sample_rate as f64 / self.hop_size as f64;
        let beat_interval_frames = (bpm as f64 / 60.0) / onset_rate;
        let beat_interval_frames = onset_rate * 60.0 / bpm as f64;

        // Estimate first beat
        let first_beat_ms = self.estimate_first_beat(
            &onset_peaks,
            beat_interval_frames,
            onset_strength.len(),
            sample_rate,
        );

        // Generate all beat positions
        let beats = Self::generate_beat_positions(first_beat_ms, beat_interval_ms, duration_ms);

        Ok(AnalyzerOutput::BeatGrid {
            first_beat_ms,
            beat_interval_ms,
            beats,
        })
    }
}

impl Analyzer for BeatGridAnalyzer {
    fn name(&self) -> &'static str {
        "BeatGrid"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        // When called without BPM, use a default of 120
        // In practice, this should be called via analyze_with_bpm after BPM detection
        self.analyze_with_bpm(samples, sample_rate, 120.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_beat_grid_analyzer_creation() {
        let analyzer = BeatGridAnalyzer::new();
        assert_eq!(analyzer.fft_size, 2048);
    }

    #[test]
    fn test_generate_beat_positions() {
        // 120 BPM = 500ms per beat
        let beats = BeatGridAnalyzer::generate_beat_positions(0.0, 500.0, 5000.0);

        // Should have 10 beats in 5 seconds at 120 BPM
        assert_eq!(beats.len(), 10);
        assert!((beats[0] - 0.0).abs() < 0.01);
        assert!((beats[1] - 500.0).abs() < 0.01);
    }

    #[test]
    fn test_frame_to_ms() {
        let analyzer = BeatGridAnalyzer::new();

        // At 44100 Hz with hop size 512, frame 86 ≈ 1000ms
        let ms = analyzer.frame_to_ms(86, 44100);
        assert!((ms - 1000.0).abs() < 20.0);
    }

    #[test]
    fn test_insufficient_data() {
        let analyzer = BeatGridAnalyzer::new();
        let samples = vec![0.0f32; 100];
        let result = analyzer.analyze(&samples, 44100);
        assert!(matches!(result, Err(AnalysisError::InsufficientData)));
    }
}
