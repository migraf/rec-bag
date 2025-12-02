//! Playlist and Library models for organizing tracks.

use crate::Track;
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
        }
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
        self.tracks
            .iter()
            .filter_map(|t| t.duration)
            .sum()
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

/// A library containing all playlists and loose tracks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Library {
    /// All playlists in the library
    pub playlists: Vec<Playlist>,
    /// Tracks not in any playlist
    pub loose_tracks: Vec<Track>,
}

impl Library {
    /// Create a new empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new playlist to the library.
    pub fn add_playlist(&mut self, playlist: Playlist) {
        self.playlists.push(playlist);
    }

    /// Create a new empty playlist and add it to the library.
    pub fn create_playlist(&mut self, name: impl Into<String>) -> &mut Playlist {
        self.playlists.push(Playlist::new(name));
        self.playlists.last_mut().unwrap()
    }

    /// Remove a playlist by ID.
    pub fn remove_playlist(&mut self, playlist_id: Uuid) -> Option<Playlist> {
        if let Some(index) = self.playlists.iter().position(|p| p.id == playlist_id) {
            Some(self.playlists.remove(index))
        } else {
            None
        }
    }

    /// Get a playlist by ID.
    pub fn get_playlist(&self, playlist_id: Uuid) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == playlist_id)
    }

    /// Get a mutable playlist by ID.
    pub fn get_playlist_mut(&mut self, playlist_id: Uuid) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.id == playlist_id)
    }

    /// Add a loose track to the library.
    pub fn add_loose_track(&mut self, track: Track) {
        self.loose_tracks.push(track);
    }

    /// Remove a loose track by ID.
    pub fn remove_loose_track(&mut self, track_id: Uuid) -> Option<Track> {
        if let Some(index) = self.loose_tracks.iter().position(|t| t.id == track_id) {
            Some(self.loose_tracks.remove(index))
        } else {
            None
        }
    }

    /// Get all tracks across all playlists and loose tracks.
    pub fn all_tracks(&self) -> impl Iterator<Item = &Track> {
        self.playlists
            .iter()
            .flat_map(|p| p.tracks.iter())
            .chain(self.loose_tracks.iter())
    }

    /// Find a track by ID anywhere in the library.
    pub fn find_track(&self, track_id: Uuid) -> Option<&Track> {
        self.all_tracks().find(|t| t.id == track_id)
    }

    /// Get the total number of unique tracks.
    pub fn track_count(&self) -> usize {
        self.playlists
            .iter()
            .map(|p| p.tracks.len())
            .sum::<usize>()
            + self.loose_tracks.len()
    }

    /// Get the number of playlists.
    pub fn playlist_count(&self) -> usize {
        self.playlists.len()
    }
}

