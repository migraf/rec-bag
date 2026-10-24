//! Playlist model for organizing tracks.

use crate::{SourceType, Track};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A playlist containing an ordered list of tracks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Playlist {
    /// Unique identifier
    pub id: Uuid,
    /// Playlist name
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Ordered list of tracks
    pub tracks: Vec<Track>,
    /// When the playlist was created
    pub created_at: DateTime<Utc>,
    /// When the playlist was last modified
    pub modified_at: DateTime<Utc>,
    /// The catalog source this playlist mirrors, or `None` for the user's own playlist.
    #[serde(default)]
    pub origin: Option<PlaylistOrigin>,
}

/// A playlist mirrored from a catalog source, such as a SoundCloud playlist.
///
/// The source owns its membership, so a mirrored playlist is read-only here: the next sync
/// would undo any local edit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaylistOrigin {
    /// The catalog source the playlist was synchronized from.
    pub source_id: Uuid,
    /// Which kind of source it is, for the badge naming it.
    pub source_type: SourceType,
    /// The playlist's public page at the source, when it has one.
    pub external_url: Option<String>,
}

impl Playlist {
    /// Create a new empty playlist.
    pub fn new(name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: None,
            tracks: Vec::new(),
            created_at: now,
            modified_at: now,
            origin: None,
        }
    }

    /// Whether the user can rename, edit, or delete this playlist.
    pub fn is_editable(&self) -> bool {
        self.origin.is_none()
    }

    /// Create a playlist with tracks.
    pub fn with_tracks(name: impl Into<String>, tracks: Vec<Track>) -> Self {
        let mut playlist = Self::new(name);
        playlist.tracks = tracks;
        playlist
    }

    /// Add a track to the end of the playlist.
    pub fn add_track(&mut self, track: Track) {
        self.tracks.push(track);
        self.modified_at = Utc::now();
    }

    /// Insert a track at a specific position.
    pub fn insert_track(&mut self, index: usize, track: Track) {
        let index = index.min(self.tracks.len());
        self.tracks.insert(index, track);
        self.modified_at = Utc::now();
    }

    /// Remove a track by index.
    pub fn remove_track(&mut self, index: usize) -> Option<Track> {
        if index < self.tracks.len() {
            self.modified_at = Utc::now();
            Some(self.tracks.remove(index))
        } else {
            None
        }
    }

    /// Remove a track by ID.
    pub fn remove_track_by_id(&mut self, track_id: Uuid) -> Option<Track> {
        if let Some(index) = self.tracks.iter().position(|t| t.id == track_id) {
            self.remove_track(index)
        } else {
            None
        }
    }

    /// Move a track from one position to another.
    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }

        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);
        self.modified_at = Utc::now();
        true
    }

    /// Get the total duration of the playlist.
    pub fn total_duration(&self) -> std::time::Duration {
        self.tracks.iter().filter_map(|t| t.duration).sum()
    }

    /// Get the number of tracks.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Check if the playlist is empty.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Clear all tracks from the playlist.
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.modified_at = Utc::now();
    }

    /// Get a track by index.
    pub fn get(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }

    /// Get a mutable track by index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Track> {
        self.tracks.get_mut(index)
    }

    /// Find a track by ID.
    pub fn find_by_id(&self, track_id: Uuid) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == track_id)
    }
}
