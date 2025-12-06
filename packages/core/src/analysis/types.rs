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

/// Complete analysis result for a track.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisResult {
    /// Waveform peaks for visualization (0.0 to 1.0).
    pub waveform_peaks: Vec<f32>,
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
            duration_secs: 0.0,
            sample_rate: 0,
            bpm: None,
            key: None,
        }
    }
}

