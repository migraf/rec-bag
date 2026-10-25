//! Frequency-band waveform extraction for visualization.
//!
//! Extracts three frequency bands (low/mid/high) for Serato/rekordbox-style
//! colored waveform display.

use super::fft::{normalize, FrequencyBands, Stft};
use super::traits::{Analyzer, AnalyzerOutput};
use super::types::AnalysisError;

/// Number of waveform points to generate per second of audio.
const POINTS_PER_SECOND: usize = 100;

/// Frequency waveform analyzer - extracts low/mid/high frequency bands.
pub struct FrequencyWaveformAnalyzer {
    /// Number of output points per second.
    points_per_second: usize,
    /// FFT size for frequency analysis.
    fft_size: usize,
    /// Frequency band boundaries.
    bands: FrequencyBands,
}

impl Default for FrequencyWaveformAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrequencyWaveformAnalyzer {
    /// Create a new frequency waveform analyzer with default settings.
    pub fn new() -> Self {
        Self {
            points_per_second: POINTS_PER_SECOND,
            fft_size: 1024,
            bands: FrequencyBands::default(),
        }
    }

    /// Create with custom resolution.
    pub fn with_resolution(points_per_second: usize) -> Self {
        Self {
            points_per_second,
            ..Self::new()
        }
    }

    /// Create with custom frequency band boundaries.
    pub fn with_bands(low_cutoff: f32, high_cutoff: f32) -> Self {
        Self {
            bands: FrequencyBands {
                low_cutoff,
                high_cutoff,
            },
            ..Self::new()
        }
    }

    /// Extract frequency bands from audio samples.
    ///
    /// One frame is centred on each output point. The frame is longer than a
    /// point so the bass band still spans a few bins; neighbouring frames
    /// overlap, which also keeps the drawing smooth.
    fn extract_bands(&self, samples: &[f32], sample_rate: u32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let samples_per_point = sample_rate as usize / self.points_per_second;
        let num_points = samples.len() / samples_per_point;

        let mut stft = Stft::new(self.fft_size, samples_per_point);
        let bin_hz = stft.bin_hz(sample_rate);
        let bins = stft.bins();
        let low_bin = ((self.bands.low_cutoff / bin_hz) as usize).clamp(1, bins);
        let high_bin = ((self.bands.high_cutoff / bin_hz) as usize).clamp(low_bin, bins);
        let rms = |band: &[f32]| (band.iter().map(|m| m * m).sum::<f32>() / band.len().max(1) as f32).sqrt();

        let mut low_peaks = Vec::with_capacity(num_points);
        let mut mid_peaks = Vec::with_capacity(num_points);
        let mut high_peaks = Vec::with_capacity(num_points);
        let mut mags = vec![0.0f32; bins];
        for point_idx in 0..num_points {
            let centre = point_idx * samples_per_point + samples_per_point / 2;
            stft.magnitudes_at(samples, centre, &mut mags);
            low_peaks.push(rms(&mags[..low_bin]));
            mid_peaks.push(rms(&mags[low_bin..high_bin]));
            high_peaks.push(rms(&mags[high_bin..]));
        }

        // Normalize each band independently
        normalize(&mut low_peaks);
        normalize(&mut mid_peaks);
        normalize(&mut high_peaks);

        (low_peaks, mid_peaks, high_peaks)
    }
}

impl Analyzer for FrequencyWaveformAnalyzer {
    fn name(&self) -> &'static str {
        "FrequencyWaveform"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        if samples.is_empty() {
            return Err(AnalysisError::InsufficientData);
        }

        let samples_per_point = sample_rate as usize / self.points_per_second;
        if samples_per_point == 0 {
            return Err(AnalysisError::AnalysisFailed(
                "Sample rate too low for waveform extraction".to_string(),
            ));
        }

        let (low, mid, high) = self.extract_bands(samples, sample_rate);

        if low.is_empty() {
            return Err(AnalysisError::InsufficientData);
        }

        Ok(AnalyzerOutput::FrequencyWaveform { low, mid, high })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frequency_waveform_analyzer_creation() {
        let analyzer = FrequencyWaveformAnalyzer::new();
        assert_eq!(analyzer.points_per_second, 100);
    }

    #[test]
    fn test_custom_resolution() {
        let analyzer = FrequencyWaveformAnalyzer::with_resolution(200);
        assert_eq!(analyzer.points_per_second, 200);
    }

    #[test]
    fn test_custom_bands() {
        let analyzer = FrequencyWaveformAnalyzer::with_bands(150.0, 5000.0);
        assert_eq!(analyzer.bands.low_cutoff, 150.0);
        assert_eq!(analyzer.bands.high_cutoff, 5000.0);
    }

    #[test]
    fn test_insufficient_data() {
        let analyzer = FrequencyWaveformAnalyzer::new();
        let samples: Vec<f32> = vec![];
        let result = analyzer.analyze(&samples, 44100);
        assert!(matches!(result, Err(AnalysisError::InsufficientData)));
    }

    #[test]
    fn test_basic_extraction() {
        let analyzer = FrequencyWaveformAnalyzer::new();

        // Generate 1 second of test signal (44100 samples)
        let sample_rate = 44100u32;
        let samples: Vec<f32> = (0..sample_rate as usize)
            .map(|i| {
                // Mix of low (100Hz) and high (8000Hz) frequencies
                let t = i as f32 / sample_rate as f32;
                0.5 * (2.0 * std::f32::consts::PI * 100.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 8000.0 * t).sin()
            })
            .collect();

        let result = analyzer.analyze(&samples, sample_rate);
        assert!(result.is_ok());

        if let Ok(AnalyzerOutput::FrequencyWaveform { low, mid, high }) = result {
            // Should have ~100 points for 1 second
            assert!(!low.is_empty());
            assert_eq!(low.len(), mid.len());
            assert_eq!(mid.len(), high.len());

            // With test signal, low and high should have more energy than mid
            let low_avg: f32 = low.iter().sum::<f32>() / low.len() as f32;
            let high_avg: f32 = high.iter().sum::<f32>() / high.len() as f32;

            // Both should be non-zero
            assert!(low_avg > 0.0);
            assert!(high_avg > 0.0);
        } else {
            panic!("Expected FrequencyWaveform output");
        }
    }
}
