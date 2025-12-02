//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{AudioEngine, Library, Playlist, SqliteStorage, Track};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Global application state shared across all views.
#[derive(Clone, Copy)]
pub struct AppState {
    /// Audio engine (wrapped in Arc<Mutex> for thread safety)
    pub engine: Signal<Arc<Mutex<AudioEngine>>>,
    /// SQLite storage backend
    pub storage: Signal<Option<Arc<SqliteStorage>>>,
    /// All playlists
    pub playlists: Signal<Vec<Playlist>>,
    /// Scanned library tracks (not in any playlist yet)
    pub library_tracks: Signal<Vec<Track>>,
    /// Currently selected playlist ID
    pub selected_playlist_id: Signal<Option<Uuid>>,
    /// Flag to trigger save
    save_pending: Signal<bool>,
}

impl AppState {
    /// Create a new app state with initialized audio engine.
    pub fn new() -> Self {
        let engine = AudioEngine::two_deck().expect("Failed to initialize audio engine");

        Self {
            engine: Signal::new(Arc::new(Mutex::new(engine))),
            storage: Signal::new(None),
            playlists: Signal::new(Vec::new()),
            library_tracks: Signal::new(Vec::new()),
            selected_playlist_id: Signal::new(None),
            save_pending: Signal::new(false),
        }
    }

    /// Initialize storage and load data asynchronously.
    pub async fn init_storage(mut self) {
        match SqliteStorage::default_location().await {
            Ok(storage) => {
                let storage = Arc::new(storage);
                
                // Load existing data
                match storage.load_library_async().await {
                    Ok(library) => {
                        self.playlists.set(library.playlists);
                        self.library_tracks.set(library.loose_tracks);
                    }
                    Err(e) => {
                        eprintln!("Failed to load library from storage: {}", e);
                    }
                }
                
                self.storage.set(Some(storage));
            }
            Err(e) => {
                eprintln!("Failed to initialize storage: {}", e);
            }
        }
    }

    /// Save the current library state to storage.
    pub async fn save_to_storage(self) {
        if let Some(storage) = self.storage.read().as_ref() {
            let library = Library {
                playlists: self.playlists.read().clone(),
                loose_tracks: self.library_tracks.read().clone(),
            };
            
            if let Err(e) = storage.save_library_async(&library).await {
                eprintln!("Failed to save library: {}", e);
            }
        }
    }

    /// Mark that a save is pending.
    fn mark_save_pending(mut self) {
        self.save_pending.set(true);
    }

    /// Check and clear save pending flag.
    pub fn take_save_pending(mut self) -> bool {
        let pending = *self.save_pending.read();
        if pending {
            self.save_pending.set(false);
        }
        pending
    }

    /// Add a track to a playlist.
    pub fn add_track_to_playlist(mut self, track: Track, playlist_id: Uuid) {
        let mut playlists = self.playlists.write();
        if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
            playlist.add_track(track);
        }
        drop(playlists);
        self.mark_save_pending();
    }

    /// Create a new playlist.
    pub fn create_playlist(mut self, name: String) -> Uuid {
        let playlist = Playlist::new(name);
        let id = playlist.id;
        self.playlists.write().push(playlist);
        self.mark_save_pending();
        id
    }

    /// Delete a playlist.
    pub fn delete_playlist(mut self, id: Uuid) {
        self.playlists.write().retain(|p| p.id != id);
        if *self.selected_playlist_id.read() == Some(id) {
            self.selected_playlist_id.set(None);
        }
        self.mark_save_pending();
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
            self.mark_save_pending();
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

/// Hook to initialize storage and set up auto-save.
pub fn use_storage_init() {
    let app_state = use_app_state();
    
    // Initialize storage on mount
    use_future(move || async move {
        app_state.init_storage().await;
    });
    
    // Auto-save when changes are pending (debounced)
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if app_state.take_save_pending() {
                app_state.save_to_storage().await;
            }
        }
    });
}
