//! Track model representing an audio file with metadata.

use chrono::{DateTime, Utc};
use lofty::prelude::*;
use lofty::probe::Probe;
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
    /// Sample rate (Hz)
    pub sample_rate: Option<u32>,
    /// Bitrate (kbps)
    pub bitrate: Option<u32>,
    /// Year/date of release
    pub year: Option<u32>,
    /// Track number in album
    pub track_number: Option<u32>,
}

impl Track {
    /// Create a new track from a file path with automatic metadata extraction.
    pub fn new(path: PathBuf) -> Self {
        let mut track = Self::new_without_metadata(path.clone());
        track.load_metadata();
        track
    }

    /// Create a new track without reading metadata (faster, for bulk operations).
    pub fn new_without_metadata(path: PathBuf) -> Self {
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
            sample_rate: None,
            bitrate: None,
            year: None,
            track_number: None,
        }
    }

    /// Load metadata from the audio file.
    pub fn load_metadata(&mut self) {
        if let Err(e) = self.try_load_metadata() {
            // Log error but don't fail - metadata is optional
            eprintln!("Failed to load metadata for {:?}: {}", self.path, e);
        }
    }

    /// Try to load metadata, returning an error if it fails.
    fn try_load_metadata(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let tagged_file = Probe::open(&self.path)?.read()?;

        // Get audio properties (duration, sample rate, bitrate)
        let properties = tagged_file.properties();
        self.duration = Some(properties.duration());
        self.sample_rate = properties.sample_rate();
        self.bitrate = properties.audio_bitrate();

        // Get tags (try primary tag first, then any available)
        if let Some(tag) = tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
            // Title
            if let Some(title) = tag.title() {
                let title_str = title.to_string();
                if !title_str.is_empty() {
                    self.title = title_str;
                }
            }

            // Artist
            if let Some(artist) = tag.artist() {
                let artist_str = artist.to_string();
                if !artist_str.is_empty() {
                    self.artist = Some(artist_str);
                }
            }

            // Album
            if let Some(album) = tag.album() {
                let album_str = album.to_string();
                if !album_str.is_empty() {
                    self.album = Some(album_str);
                }
            }

            // Genre
            if let Some(genre) = tag.genre() {
                let genre_str = genre.to_string();
                if !genre_str.is_empty() {
                    self.genre = Some(genre_str);
                }
            }

            // Year
            self.year = tag.year();

            // Track number
            self.track_number = tag.track();

            // Comment
            if let Some(comment) = tag.comment() {
                let comment_str = comment.to_string();
                if !comment_str.is_empty() {
                    self.comment = Some(comment_str);
                }
            }

            // Try to get BPM from various tag formats
            self.bpm = self.extract_bpm(tag);

            // Try to get musical key
            self.key = self.extract_key(tag);
        }

        Ok(())
    }

    /// Extract BPM from tag (stored in different ways by different software).
    fn extract_bpm(&self, _tag: &dyn Accessor) -> Option<f32> {
        // BPM is often stored as a custom tag
        // Common tag names: TBPM (ID3v2), BPM, TEMPO
        // lofty doesn't have direct BPM accessor, would need to check custom tags
        // For now, return None - can be enhanced later
        None
    }

    /// Extract musical key from tag.
    fn extract_key(&self, _tag: &dyn Accessor) -> Option<String> {
        // Key is often stored as TKEY (ID3v2) or INITIALKEY
        // lofty doesn't have direct key accessor
        // For now, return None - can be enhanced later
        None
    }

    /// Create a track with title and artist (manual override).
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

    /// Get a formatted duration string (MM:SS).
    pub fn duration_string(&self) -> String {
        match self.duration {
            Some(d) => {
                let total_secs = d.as_secs();
                let mins = total_secs / 60;
                let secs = total_secs % 60;
                format!("{:02}:{:02}", mins, secs)
            }
            None => "--:--".to_string(),
        }
    }

    /// Get a formatted bitrate string.
    pub fn bitrate_string(&self) -> String {
        match self.bitrate {
            Some(b) => format!("{}kbps", b),
            None => "".to_string(),
        }
    }
}

/// Analyze multiple tracks in parallel (for bulk imports).
pub fn analyze_tracks(paths: Vec<PathBuf>) -> Vec<Track> {
    paths.into_iter().map(Track::new).collect()
}
