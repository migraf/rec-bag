//! Track model representing an audio file with metadata.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Represents a single audio track with its metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    /// Unique identifier for the track
    pub id: Uuid,
    /// File path to the audio file
    pub path: PathBuf,
    /// Track title (from metadata or filename)
    pub title: String,
    /// Artist name
    pub artist: Option<String>,
    /// Album name
    pub album: Option<String>,
    /// Track duration
    pub duration: Option<Duration>,
    /// Beats per minute (for DJ mixing)
    pub bpm: Option<f32>,
    /// Musical key (e.g., "Am", "C#m")
    pub key: Option<String>,
    /// Genre
    pub genre: Option<String>,
    /// User-defined rating (0-5)
    pub rating: Option<u8>,
    /// User comments/notes
    pub comment: Option<String>,
    /// When the track was added to the library
    pub added_at: DateTime<Utc>,
    /// Last time the track was played
    pub last_played: Option<DateTime<Utc>>,
    /// Number of times played
    pub play_count: u32,
}

impl Track {
    /// Create a new track from a file path.
    /// Title is extracted from the filename if not provided.
    pub fn new(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        Self {
            id: Uuid::new_v4(),
            path,
            title,
            artist: None,
            album: None,
            duration: None,
            bpm: None,
            key: None,
            genre: None,
            rating: None,
            comment: None,
            added_at: Utc::now(),
            last_played: None,
            play_count: 0,
        }
    }

    /// Create a track with title and artist.
    pub fn with_metadata(path: PathBuf, title: String, artist: Option<String>) -> Self {
        let mut track = Self::new(path);
        track.title = title;
        track.artist = artist;
        track
    }

    /// Set the BPM value.
    pub fn set_bpm(&mut self, bpm: f32) {
        self.bpm = Some(bpm);
    }

    /// Set the musical key.
    pub fn set_key(&mut self, key: String) {
        self.key = Some(key);
    }

    /// Set the duration.
    pub fn set_duration(&mut self, duration: Duration) {
        self.duration = Some(duration);
    }

    /// Record that the track was played.
    pub fn record_play(&mut self) {
        self.last_played = Some(Utc::now());
        self.play_count += 1;
    }

    /// Set the rating (clamped to 0-5).
    pub fn set_rating(&mut self, rating: u8) {
        self.rating = Some(rating.min(5));
    }
}

