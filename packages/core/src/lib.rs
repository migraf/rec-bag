//! Core library for rec_bag DJ playlist manager.
//!
//! This crate provides:
//! - Multi-channel audio playback engine using rodio (requires `audio` feature)
//! - Track and playlist data models
//! - Storage abstraction for local/cloud backends

#[cfg(feature = "audio")]
pub mod audio;
pub mod playlist;
pub mod storage;
pub mod track;

#[cfg(feature = "audio")]
pub use audio::{AudioEngine, Deck, DeckState};
pub use playlist::{Library, Playlist};
pub use storage::{LocalStorage, StorageBackend, StorageError};
pub use track::Track;

