//! Core library for rec_bag DJ playlist manager.
//!
//! This crate provides:
//! - Multi-channel audio playback engine using rodio (requires `audio` feature)
//! - Track and playlist data models
//! - Storage abstraction for local/cloud backends
//! - Waveform extraction for visualization

#[cfg(feature = "audio")]
pub mod audio;
pub mod playlist;
pub mod storage;
pub mod track;
#[cfg(feature = "audio")]
pub mod waveform;

#[cfg(feature = "audio")]
pub use audio::{AudioEngine, Deck, DeckState};
pub use playlist::{Library, Playlist};
pub use storage::{LocalStorage, SqliteStorage, StorageBackend, StorageError};
pub use track::{Track, WaveformPeaks};
#[cfg(feature = "audio")]
pub use waveform::{extract_waveform, WaveformData, WaveformError};

