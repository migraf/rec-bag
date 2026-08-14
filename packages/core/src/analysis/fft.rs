//! Reusable FFT utilities for audio analysis.
//!
//! This module provides shared FFT functionality used by multiple analyzers:
//! - Windowed FFT computation
//! - Frequency band extraction
//! - Spectral magnitude calculation

use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

/// Window function types for FFT.
#[derive(Debug, Clone, Copy, Default)]
pub enum WindowType {
    /// Hann window - good general purpose, smooth transitions
    #[default]
    Hann,
    /// Hamming window - slightly better frequency resolution
    Hamming,
    /// Blackman window - excellent sidelobe suppression
    Blackman,
}

/// Frequency band boundaries in Hz.
#[derive(Debug, Clone, Copy)]
pub struct FrequencyBands {
    /// Low frequency cutoff (bass below this)
    pub low_cutoff: f32,
    /// High frequency cutoff (treble above this)
    pub high_cutoff: f32,
}

impl Default for FrequencyBands {
    fn default() -> Self {
        Self {
            low_cutoff: 200.0,   // Bass: < 200 Hz
            high_cutoff: 4000.0, // Treble: > 4 kHz
        }
    }
}

/// Result of frequency band energy extraction.
#[derive(Debug, Clone, Default)]
pub struct BandEnergies {
    /// Low frequency (bass) energy
    pub low: f32,
    /// Mid frequency energy
    pub mid: f32,
    /// High frequency (treble) energy
    pub high: f32,
}

/// Shared FFT processor for audio analysis.
///
/// This struct caches the FFT plan and window coefficients for efficiency
/// when processing multiple frames.
pub struct FftProcessor {
    fft_size: usize,
    window: Vec<f32>,
    planner: FftPlanner<f32>,
    bands: FrequencyBands,
}

impl FftProcessor {
    /// Create a new FFT processor with specified size and window type.
    pub fn new(fft_size: usize, window_type: WindowType) -> Self {
        let window = Self::compute_window(fft_size, window_type);
        Self {
            fft_size,
            window,
            planner: FftPlanner::new(),
            bands: FrequencyBands::default(),
        }
    }

    /// Create an FFT processor with custom frequency band boundaries.
    pub fn with_bands(fft_size: usize, window_type: WindowType, bands: FrequencyBands) -> Self {
        let window = Self::compute_window(fft_size, window_type);
        Self {
            fft_size,
            window,
            planner: FftPlanner::new(),
            bands,
        }
    }

    /// Get the FFT size.
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// Compute window coefficients for a given window type.
    fn compute_window(size: usize, window_type: WindowType) -> Vec<f32> {
        (0..size)
            .map(|i| {
                let x = i as f32 / size as f32;
                match window_type {
                    WindowType::Hann => 0.5 * (1.0 - (2.0 * PI * x).cos()),
                    WindowType::Hamming => 0.54 - 0.46 * (2.0 * PI * x).cos(),
                    WindowType::Blackman => {
                        0.42 - 0.5 * (2.0 * PI * x).cos() + 0.08 * (4.0 * PI * x).cos()
                    }
                }
            })
            .collect()
    }

    /// Apply window function to samples.
    pub fn apply_window(&self, samples: &[f32]) -> Vec<f32> {
        samples
            .iter()
            .zip(self.window.iter())
            .map(|(s, w)| s * w)
            .collect()
    }

    /// Compute FFT of windowed samples and return complex spectrum.
    pub fn compute_fft(&mut self, samples: &[f32]) -> Vec<Complex<f32>> {
        let fft = self.planner.plan_fft_forward(self.fft_size);

        // Apply window and create complex buffer
        let mut buffer: Vec<Complex<f32>> = samples
            .iter()
            .zip(self.window.iter())
            .map(|(s, w)| Complex::new(s * w, 0.0))
            .collect();

        // Pad with zeros if needed
        buffer.resize(self.fft_size, Complex::new(0.0, 0.0));

        // Compute FFT in-place
        fft.process(&mut buffer);

        buffer
    }

    /// Compute magnitude spectrum from FFT result.
    /// Returns only positive frequencies (0 to Nyquist).
    pub fn compute_magnitudes(&self, fft_result: &[Complex<f32>]) -> Vec<f32> {
        fft_result[..self.fft_size / 2 + 1]
            .iter()
            .map(|c| c.norm())
            .collect()
    }

    /// Compute power spectrum (magnitude squared) from FFT result.
    pub fn compute_power_spectrum(&self, fft_result: &[Complex<f32>]) -> Vec<f32> {
        fft_result[..self.fft_size / 2 + 1]
            .iter()
            .map(|c| c.norm_sqr())
            .collect()
    }

    /// Extract energy in frequency bands from magnitude spectrum.
    pub fn extract_band_energies(&self, magnitudes: &[f32], sample_rate: u32) -> BandEnergies {
        let freq_resolution = sample_rate as f32 / self.fft_size as f32;
        let low_bin = (self.bands.low_cutoff / freq_resolution) as usize;
        let high_bin = (self.bands.high_cutoff / freq_resolution) as usize;

        let low_bin = low_bin.min(magnitudes.len());
        let high_bin = high_bin.min(magnitudes.len());

        // Sum energies in each band
        let low: f32 = magnitudes[..low_bin].iter().map(|m| m * m).sum();
        let mid: f32 = magnitudes[low_bin..high_bin].iter().map(|m| m * m).sum();
        let high: f32 = magnitudes[high_bin..].iter().map(|m| m * m).sum();

        // Normalize by number of bins in each band
        let low_count = low_bin.max(1) as f32;
        let mid_count = (high_bin - low_bin).max(1) as f32;
        let high_count = (magnitudes.len() - high_bin).max(1) as f32;

        BandEnergies {
            low: (low / low_count).sqrt(),
            mid: (mid / mid_count).sqrt(),
            high: (high / high_count).sqrt(),
        }
    }

    /// Process a frame and return band energies in one call.
    pub fn process_frame(&mut self, samples: &[f32], sample_rate: u32) -> BandEnergies {
        let fft_result = self.compute_fft(samples);
        let magnitudes = self.compute_magnitudes(&fft_result);
        self.extract_band_energies(&magnitudes, sample_rate)
    }

    /// Convert frequency in Hz to FFT bin index.
    pub fn freq_to_bin(&self, freq: f32, sample_rate: u32) -> usize {
        let freq_resolution = sample_rate as f32 / self.fft_size as f32;
        (freq / freq_resolution).round() as usize
    }

    /// Convert FFT bin index to frequency in Hz.
    pub fn bin_to_freq(&self, bin: usize, sample_rate: u32) -> f32 {
        let freq_resolution = sample_rate as f32 / self.fft_size as f32;
        bin as f32 * freq_resolution
    }
}

/// Compute spectral flux between two magnitude spectra.
/// Used for onset detection.
pub fn spectral_flux(prev_magnitudes: &[f32], curr_magnitudes: &[f32]) -> f32 {
    prev_magnitudes
        .iter()
        .zip(curr_magnitudes.iter())
        .map(|(prev, curr)| (curr - prev).max(0.0))
        .sum()
}

/// Normalize a vector of values to 0.0-1.0 range.
pub fn normalize(values: &mut [f32]) {
    let max = values
        .iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .copied()
        .unwrap_or(1.0)
        .max(0.001);

    for v in values {
        *v /= max;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_creation() {
        let processor = FftProcessor::new(1024, WindowType::Hann);
        assert_eq!(processor.window.len(), 1024);

        // Hann window should be 0 at endpoints
        assert!(processor.window[0].abs() < 0.01);

        // Hann window should be 1 at center
        assert!((processor.window[512] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_fft_size() {
        let processor = FftProcessor::new(2048, WindowType::Hann);
        assert_eq!(processor.fft_size(), 2048);
    }

    #[test]
    fn test_freq_bin_conversion() {
        let processor = FftProcessor::new(1024, WindowType::Hann);
        let sample_rate = 44100;

        // At 44100 Hz with 1024 FFT, each bin is ~43 Hz
        let bin = processor.freq_to_bin(440.0, sample_rate);
        let freq = processor.bin_to_freq(bin, sample_rate);

        // Should be close to 440 Hz
        assert!((freq - 440.0).abs() < 50.0);
    }

    #[test]
    fn test_band_energies() {
        let processor = FftProcessor::new(1024, WindowType::Hann);

        // Create fake magnitudes with energy in low band
        let mut magnitudes = vec![0.0f32; 513];
        magnitudes[1..10].fill(1.0); // Low frequency bins

        let energies = processor.extract_band_energies(&magnitudes, 44100);
        assert!(energies.low > energies.mid);
        assert!(energies.low > energies.high);
    }
}
