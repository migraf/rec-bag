//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{AudioEngine, Playlist, Track};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Global application state shared across all views.
#[derive(Clone, Copy)]
pub struct AppState {
    /// Audio engine (wrapped in Arc<Mutex> for thread safety)
    pub engine: Signal<Arc<Mutex<AudioEngine>>>,
    /// All playlists
    pub playlists: Signal<Vec<Playlist>>,
    /// Scanned library tracks (not in any playlist yet)
    pub library_tracks: Signal<Vec<Track>>,
    /// Currently selected playlist ID
    pub selected_playlist_id: Signal<Option<Uuid>>,
}

impl AppState {
    /// Create a new app state with initialized audio engine.
    pub fn new() -> Self {
        let engine = AudioEngine::two_deck().expect("Failed to initialize audio engine");

        Self {
            engine: Signal::new(Arc::new(Mutex::new(engine))),
            playlists: Signal::new(Vec::new()),
            library_tracks: Signal::new(Vec::new()),
            selected_playlist_id: Signal::new(None),
        }
    }

    /// Add a track to a playlist.
    pub fn add_track_to_playlist(mut self, track: Track, playlist_id: Uuid) {
        let mut playlists = self.playlists.write();
        if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
            playlist.add_track(track);
        }
    }

    /// Create a new playlist.
    pub fn create_playlist(mut self, name: String) -> Uuid {
        let playlist = Playlist::new(name);
        let id = playlist.id;
        self.playlists.write().push(playlist);
        id
    }

    /// Delete a playlist.
    pub fn delete_playlist(mut self, id: Uuid) {
        self.playlists.write().retain(|p| p.id != id);
        if *self.selected_playlist_id.read() == Some(id) {
            self.selected_playlist_id.set(None);
        }
    }

    /// Get tracks for the selected playlist.
    pub fn selected_playlist_tracks(&self) -> Vec<Track> {
        if let Some(id) = *self.selected_playlist_id.read() {
            self.playlists
                .read()
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.tracks.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Scan a folder for audio files.
    pub fn scan_folder(mut self, path: &str) {
        if path.is_empty() {
            return;
        }

        if let Ok(entries) = std::fs::read_dir(path) {
            let mut found_tracks = Vec::new();
            for entry in entries.flatten() {
                let file_path = entry.path();
                if file_path.is_file() {
                    if let Some(ext) = file_path.extension() {
                        let ext = ext.to_string_lossy().to_lowercase();
                        if matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac") {
                            found_tracks.push(Track::new(file_path));
                        }
                    }
                }
            }
            self.library_tracks.set(found_tracks);
        }
    }
}

/// Provide the app state to the component tree.
pub fn use_app_state_provider() {
    use_context_provider(AppState::new);
}

/// Get the app state from context.
pub fn use_app_state() -> AppState {
    use_context::<AppState>()
}
