//! Core library for rec_bag DJ playlist manager.
//!
//! This crate provides:
//! - Multi-channel audio playback engine using rodio (requires `audio` feature)
//! - Track and playlist data models
//! - Storage abstraction for local/cloud backends
//! - Audio analysis (waveform, BPM, key detection)
//! - Fuzzy search for tracks
//! - Track diagnostics (Doctor) for detecting and fixing compatibility issues

#[cfg(feature = "audio")]
pub mod analysis;
#[cfg(feature = "audio")]
pub mod audio;
pub mod doctor;
pub mod folder;
pub mod playlist;
pub mod search;
pub mod source;
pub mod storage;
pub mod track;
#[cfg(feature = "audio")]
pub mod waveform;

#[cfg(feature = "audio")]
pub use analysis::{
    analyze_track, Analyzer, AnalyzerOutput, AnalysisError, AnalysisResult,
    BpmAnalyzer, BpmResult, KeyAnalyzer, KeyResult, MusicalKey, WaveformAnalyzer,
};
#[cfg(feature = "audio")]
pub use audio::{AudioEngine, Deck, DeckState};
pub use doctor::{
    Diagnosis, DiagnosisGroup, DiagnosisSummary, DiagnosticCheck, Doctor, DoctorError,
    MetadataCheck, Severity, WavExtensibleCheck,
};
pub use folder::{build_folder_tree, is_audio_extension, FolderNode};
pub use playlist::{Library, Playlist};
pub use search::SearchEngine;
pub use source::{LocalSourceConfig, MusicSource, SourceType};
pub use storage::{LocalStorage, SqliteStorage, StorageBackend, StorageError};
pub use track::{MetadataError, Track, WaveformPeaks};
#[cfg(feature = "audio")]
pub use waveform::{extract_waveform, WaveformData, WaveformError};

