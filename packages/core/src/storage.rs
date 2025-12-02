//! Storage abstraction for local and cloud backends.

use crate::Library;
use serde_json;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Storage not initialized")]
    NotInitialized,
    #[error("Item not found: {0}")]
    NotFound(String),
}

/// Trait for storage backends (local, cloud, etc.).
pub trait StorageBackend {
    /// Load the library from storage.
    fn load_library(&self) -> Result<Library, StorageError>;

    /// Save the library to storage.
    fn save_library(&self, library: &Library) -> Result<(), StorageError>;

    /// Check if storage is available/initialized.
    fn is_available(&self) -> bool;

    /// Get the storage location description.
    fn location(&self) -> String;
}

/// Local filesystem storage backend using JSON.
pub struct LocalStorage {
    /// Directory for storing data
    data_dir: PathBuf,
    /// Path to the library file
    library_path: PathBuf,
}

impl LocalStorage {
    /// Create a new local storage backend.
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self, StorageError> {
        let data_dir = data_dir.as_ref().to_path_buf();

        // Create directory if it doesn't exist
        fs::create_dir_all(&data_dir)?;

        let library_path = data_dir.join("library.json");

        Ok(Self {
            data_dir,
            library_path,
        })
    }

    /// Create storage in the default location (platform-specific).
    pub fn default_location() -> Result<Self, StorageError> {
        let data_dir = dirs_default();
        Self::new(data_dir)
    }

    /// Get the data directory path.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// Get the default data directory for the current platform.
fn dirs_default() -> PathBuf {
    // Use XDG on Linux, AppData on Windows, Library on macOS
    if let Some(data_dir) = dirs_next::data_dir() {
        data_dir.join("rec_bag")
    } else {
        // Fallback to current directory
        PathBuf::from(".rec_bag")
    }
}

impl StorageBackend for LocalStorage {
    fn load_library(&self) -> Result<Library, StorageError> {
        if !self.library_path.exists() {
            // Return empty library if file doesn't exist
            return Ok(Library::new());
        }

        let contents = fs::read_to_string(&self.library_path)?;
        let library: Library = serde_json::from_str(&contents)?;
        Ok(library)
    }

    fn save_library(&self, library: &Library) -> Result<(), StorageError> {
        let contents = serde_json::to_string_pretty(library)?;
        fs::write(&self.library_path, contents)?;
        Ok(())
    }

    fn is_available(&self) -> bool {
        self.data_dir.exists() || fs::create_dir_all(&self.data_dir).is_ok()
    }

    fn location(&self) -> String {
        self.data_dir.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Playlist, Track};
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn test_local_storage_roundtrip() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();

        // Create a library with some data
        let mut library = Library::new();
        let mut playlist = Playlist::new("Test Playlist");
        playlist.add_track(Track::new(PathBuf::from("/music/track1.mp3")));
        library.add_playlist(playlist);

        // Save and reload
        storage.save_library(&library).unwrap();
        let loaded = storage.load_library().unwrap();

        assert_eq!(loaded.playlists.len(), 1);
        assert_eq!(loaded.playlists[0].name, "Test Playlist");
        assert_eq!(loaded.playlists[0].tracks.len(), 1);
    }

    #[test]
    fn test_empty_library_on_missing_file() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();

        let library = storage.load_library().unwrap();
        assert!(library.playlists.is_empty());
        assert!(library.loose_tracks.is_empty());
    }
}

