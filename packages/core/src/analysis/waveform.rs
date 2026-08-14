//! Waveform extraction analyzer.

use super::traits::{Analyzer, AnalyzerOutput};
use super::types::AnalysisError;

/// Number of waveform points to generate per second of audio.
const POINTS_PER_SECOND: usize = 100;

/// Waveform analyzer - extracts peak values for visualization.
pub struct WaveformAnalyzer {
    points_per_second: usize,
}

impl Default for WaveformAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveformAnalyzer {
    /// Create a new waveform analyzer with default settings.
    pub fn new() -> Self {
        Self {
            points_per_second: POINTS_PER_SECOND,
        }
    }

    /// Create a waveform analyzer with custom resolution.
    pub fn with_resolution(points_per_second: usize) -> Self {
        Self { points_per_second }
    }
}

impl Analyzer for WaveformAnalyzer {
    fn name(&self) -> &'static str {
        "Waveform"
    }

    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError> {
        if samples.is_empty() {
            return Err(AnalysisError::InsufficientData);
        }

        let samples_per_peak = sample_rate as usize / self.points_per_second;
        if samples_per_peak == 0 {
            return Err(AnalysisError::AnalysisFailed(
                "Sample rate too low for waveform extraction".to_string(),
            ));
        }

        // Compute peaks
        let mut peaks: Vec<f32> = samples
            .chunks(samples_per_peak)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|s| s.abs())
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0)
            })
            .collect();

        // Normalize peaks to 0.0-1.0 range
        let max_peak = peaks
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .copied()
            .unwrap_or(1.0)
            .max(0.001); // Avoid division by zero

        for peak in &mut peaks {
            *peak /= max_peak;
        }

        Ok(AnalyzerOutput::Waveform { peaks })
    }
}
