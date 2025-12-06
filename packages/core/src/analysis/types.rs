//! Analysis types and error definitions.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during audio analysis.
#[derive(Debug, Error)]
pub enum AnalysisError {
    #[error("Failed to open audio file: {0}")]
    FileOpen(#[from] std::io::Error),
    #[error("Failed to probe audio format: {0}")]
    Probe(String),
    #[error("No audio track found")]
    NoTrack,
    #[error("Failed to create decoder: {0}")]
    Decoder(String),
    #[error("Failed to decode audio: {0}")]
    Decode(String),
    #[error("Insufficient audio data for analysis")]
    InsufficientData,
    #[error("Analysis failed: {0}")]
    AnalysisFailed(String),
}

/// Musical key representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MusicalKey {
    // Major keys
    CMajor,
    DbMajor,
    DMajor,
    EbMajor,
    EMajor,
    FMajor,
    GbMajor,
    GMajor,
    AbMajor,
    AMajor,
    BbMajor,
    BMajor,
    // Minor keys
    CMinor,
    DbMinor,
    DMinor,
    EbMinor,
    EMinor,
    FMinor,
    GbMinor,
    GMinor,
    AbMinor,
    AMinor,
    BbMinor,
    BMinor,
}

impl MusicalKey {
    /// Convert to standard notation string (e.g., "Am", "C", "F#m").
    pub fn to_notation(&self) -> &'static str {
        match self {
            MusicalKey::CMajor => "C",
            MusicalKey::DbMajor => "Db",
            MusicalKey::DMajor => "D",
            MusicalKey::EbMajor => "Eb",
            MusicalKey::EMajor => "E",
            MusicalKey::FMajor => "F",
            MusicalKey::GbMajor => "Gb",
            MusicalKey::GMajor => "G",
            MusicalKey::AbMajor => "Ab",
            MusicalKey::AMajor => "A",
            MusicalKey::BbMajor => "Bb",
            MusicalKey::BMajor => "B",
            MusicalKey::CMinor => "Cm",
            MusicalKey::DbMinor => "C#m",
            MusicalKey::DMinor => "Dm",
            MusicalKey::EbMinor => "Ebm",
            MusicalKey::EMinor => "Em",
            MusicalKey::FMinor => "Fm",
            MusicalKey::GbMinor => "F#m",
            MusicalKey::GMinor => "Gm",
            MusicalKey::AbMinor => "G#m",
            MusicalKey::AMinor => "Am",
            MusicalKey::BbMinor => "Bbm",
            MusicalKey::BMinor => "Bm",
        }
    }

    /// Convert to Camelot notation (used by many DJ software).
    pub fn to_camelot(&self) -> &'static str {
        match self {
            MusicalKey::CMajor => "8B",
            MusicalKey::DbMajor => "3B",
            MusicalKey::DMajor => "10B",
            MusicalKey::EbMajor => "5B",
            MusicalKey::EMajor => "12B",
            MusicalKey::FMajor => "7B",
            MusicalKey::GbMajor => "2B",
            MusicalKey::GMajor => "9B",
            MusicalKey::AbMajor => "4B",
            MusicalKey::AMajor => "11B",
            MusicalKey::BbMajor => "6B",
            MusicalKey::BMajor => "1B",
            MusicalKey::CMinor => "5A",
            MusicalKey::DbMinor => "12A",
            MusicalKey::DMinor => "7A",
            MusicalKey::EbMinor => "2A",
            MusicalKey::EMinor => "9A",
            MusicalKey::FMinor => "4A",
            MusicalKey::GbMinor => "11A",
            MusicalKey::GMinor => "6A",
            MusicalKey::AbMinor => "1A",
            MusicalKey::AMinor => "8A",
            MusicalKey::BbMinor => "3A",
            MusicalKey::BMinor => "10A",
        }
    }
}

impl std::fmt::Display for MusicalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_notation())
    }
}

/// Result from BPM detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BpmResult {
    /// Detected BPM value.
    pub bpm: f32,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f32,
}

/// Result from key detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyResult {
    /// Detected musical key.
    pub key: MusicalKey,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f32,
}

/// Frequency-band waveform data for colored visualization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FrequencyWaveform {
    /// Low frequency (bass) peaks.
    pub low: Vec<f32>,
    /// Mid frequency peaks.
    pub mid: Vec<f32>,
    /// High frequency (treble) peaks.
    pub high: Vec<f32>,
}

impl FrequencyWaveform {
    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.low.is_empty() && self.mid.is_empty() && self.high.is_empty()
    }

    /// Get length (number of points).
    pub fn len(&self) -> usize {
        self.low.len()
    }

    /// Create combined peaks by summing bands (for backward compatibility).
    pub fn combined_peaks(&self) -> Vec<f32> {
        if self.is_empty() {
            return Vec::new();
        }
        self.low
            .iter()
            .zip(self.mid.iter())
            .zip(self.high.iter())
            .map(|((l, m), h)| (l + m + h) / 3.0)
            .collect()
    }
}

/// Beat grid data for visualization and synchronization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BeatGridResult {
    /// Time of the first beat in milliseconds.
    pub first_beat_ms: f64,
    /// Time between beats in milliseconds (derived from BPM).
    pub beat_interval_ms: f64,
    /// All beat positions in milliseconds.
    pub beats: Vec<f64>,
}

impl BeatGridResult {
    /// Check if the beat grid is empty.
    pub fn is_empty(&self) -> bool {
        self.beats.is_empty()
    }

    /// Get beat count.
    pub fn beat_count(&self) -> usize {
        self.beats.len()
    }

    /// Get bar count (4 beats per bar).
    pub fn bar_count(&self) -> usize {
        self.beats.len() / 4
    }

    /// Check if a beat is the start of a bar (every 4th beat).
    pub fn is_bar_start(&self, beat_index: usize) -> bool {
        beat_index % 4 == 0
    }

    /// Get beat position as fraction of total duration.
    pub fn beat_position_fraction(&self, beat_index: usize, duration_ms: f64) -> f64 {
        if beat_index >= self.beats.len() || duration_ms <= 0.0 {
            return 0.0;
        }
        self.beats[beat_index] / duration_ms
    }
}

/// Complete analysis result for a track.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisResult {
    /// Waveform peaks for visualization (0.0 to 1.0) - legacy/fallback.
    pub waveform_peaks: Vec<f32>,
    /// Frequency-band waveform for colored visualization.
    pub freq_waveform: Option<FrequencyWaveform>,
    /// Beat grid for beat markers and synchronization.
    pub beat_grid: Option<BeatGridResult>,
    /// Duration of the audio in seconds.
    pub duration_secs: f64,
    /// Sample rate of the source audio.
    pub sample_rate: u32,
    /// Detected BPM (if successful).
    pub bpm: Option<BpmResult>,
    /// Detected key (if successful).
    pub key: Option<KeyResult>,
}

impl AnalysisResult {
    /// Create an empty analysis result.
    pub fn empty() -> Self {
        Self {
            waveform_peaks: Vec::new(),
            freq_waveform: None,
            beat_grid: None,
            duration_secs: 0.0,
            sample_rate: 0,
            bpm: None,
            key: None,
        }
    }
}

