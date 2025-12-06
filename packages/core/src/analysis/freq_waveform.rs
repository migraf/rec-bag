//! Frequency-band waveform extraction for visualization.
//!
//! Extracts three frequency bands (low/mid/high) for Serato/rekordbox-style
//! colored waveform display.

use super::fft::{normalize, FftProcessor, FrequencyBands, WindowType};
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
            fft_size: 2048,
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
    fn extract_bands(
        &self,
        samples: &[f32],
        sample_rate: u32,
    ) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let samples_per_point = sample_rate as usize / self.points_per_second;
        let hop_size = self.fft_size / 4; // 75% overlap for smooth results

        let mut processor = FftProcessor::with_bands(self.fft_size, WindowType::Hann, self.bands);

        let mut low_peaks = Vec::new();
        let mut mid_peaks = Vec::new();
        let mut high_peaks = Vec::new();

        // Process in chunks corresponding to output points
        let num_points = samples.len() / samples_per_point;

        for point_idx in 0..num_points {
            let chunk_start = point_idx * samples_per_point;
            let chunk_end = (chunk_start + samples_per_point).min(samples.len());

            // Accumulate band energies over multiple FFT frames in this chunk
            let mut low_acc = 0.0f32;
            let mut mid_acc = 0.0f32;
            let mut high_acc = 0.0f32;
            let mut frame_count = 0;

            let mut frame_start = chunk_start;
            while frame_start + self.fft_size <= chunk_end {
                let frame = &samples[frame_start..frame_start + self.fft_size];
                let energies = processor.process_frame(frame, sample_rate);

                low_acc = low_acc.max(energies.low);
                mid_acc = mid_acc.max(energies.mid);
                high_acc = high_acc.max(energies.high);
                frame_count += 1;

                frame_start += hop_size;
            }

            // If no complete frames in chunk, use partial frame
            if frame_count == 0 && chunk_end > chunk_start {
                let mut padded = vec![0.0f32; self.fft_size];
                let available = chunk_end - chunk_start;
                padded[..available].copy_from_slice(&samples[chunk_start..chunk_end]);
                let energies = processor.process_frame(&padded, sample_rate);
                low_acc = energies.low;
                mid_acc = energies.mid;
                high_acc = energies.high;
            }

            low_peaks.push(low_acc);
            mid_peaks.push(mid_acc);
            high_peaks.push(high_acc);
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

