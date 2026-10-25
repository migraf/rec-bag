//! Analyzer trait definition for extensible audio analysis.

use super::types::AnalysisError;

/// Output from an analyzer - can be different types depending on the analyzer.
#[derive(Debug, Clone)]
pub enum AnalyzerOutput {
    /// BPM value with confidence.
    Bpm { bpm: f32, confidence: f32 },
    /// Musical key with confidence.
    Key {
        key: super::types::MusicalKey,
        confidence: f32,
    },
    /// Waveform peaks (simple amplitude-based).
    Waveform { peaks: Vec<f32> },
    /// Frequency-band waveform (low/mid/high).
    FrequencyWaveform {
        low: Vec<f32>,
        mid: Vec<f32>,
        high: Vec<f32>,
    },
    /// Beat grid with first beat and beat positions.
    BeatGrid {
        first_beat_ms: f64,
        beat_interval_ms: f64,
        beats: Vec<f64>,
        downbeat_index: usize,
    },
}

/// Trait for audio analyzers.
///
/// Implement this trait to create custom analyzers that can be plugged into
/// the analysis pipeline.
pub trait Analyzer: Send + Sync {
    /// Name of this analyzer (for logging/debugging).
    fn name(&self) -> &'static str;

    /// Analyze audio samples and return the result.
    ///
    /// # Arguments
    /// * `samples` - Mono audio samples (normalized to -1.0 to 1.0)
    /// * `sample_rate` - Sample rate in Hz
    ///
    /// # Returns
    /// The analysis output or an error if analysis failed.
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, AnalysisError>;
}
