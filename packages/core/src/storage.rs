//! Storage abstraction for local and cloud backends.

use crate::{Library, Playlist, Track};
use chrono::{DateTime, Utc};
use libsql::{params, Builder, Connection, Database};
use serde_json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

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
    #[error("Database error: {0}")]
    Database(#[from] libsql::Error),
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

/// SQLite storage backend using libsql (compatible with Turso for future sync).
pub struct SqliteStorage {
    /// Database handle (kept for future Turso sync functionality)
    #[allow(dead_code)]
    db: Database,
    conn: Connection,
    db_path: PathBuf,
}

impl SqliteStorage {
    /// Create a new SQLite storage backend.
    pub async fn new(data_dir: impl AsRef<Path>) -> Result<Self, StorageError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;

        let db_path = data_dir.join("library.db");
        let db = Builder::new_local(db_path.to_string_lossy().as_ref())
            .build()
            .await?;
        let conn = db.connect()?;

        let storage = Self { db, conn, db_path };
        storage.run_migrations().await?;

        Ok(storage)
    }

    /// Create storage in the default location.
    pub async fn default_location() -> Result<Self, StorageError> {
        let data_dir = dirs_default();
        Self::new(data_dir).await
    }

    /// Run database migrations to create/update schema.
    async fn run_migrations(&self) -> Result<(), StorageError> {
        // Create tracks table
        self.conn
            .execute(
                r#"
                CREATE TABLE IF NOT EXISTS tracks (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    artist TEXT,
                    album TEXT,
                    duration_ms INTEGER,
                    bpm REAL,
                    key TEXT,
                    genre TEXT,
                    rating INTEGER,
                    comment TEXT,
                    added_at TEXT NOT NULL,
                    last_played TEXT,
                    play_count INTEGER DEFAULT 0,
                    sample_rate INTEGER,
                    bitrate INTEGER,
                    year INTEGER,
                    track_number INTEGER
                )
                "#,
                (),
            )
            .await?;

        // Create playlists table
        self.conn
            .execute(
                r#"
                CREATE TABLE IF NOT EXISTS playlists (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT,
                    created_at TEXT NOT NULL,
                    modified_at TEXT NOT NULL
                )
                "#,
                (),
            )
            .await?;

        // Create playlist_tracks junction table
        self.conn
            .execute(
                r#"
                CREATE TABLE IF NOT EXISTS playlist_tracks (
                    playlist_id TEXT NOT NULL,
                    track_id TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    PRIMARY KEY (playlist_id, track_id),
                    FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE,
                    FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
                )
                "#,
                (),
            )
            .await?;

        // Create indexes for common queries
        self.conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist ON playlist_tracks(playlist_id)",
                (),
            )
            .await?;

        Ok(())
    }

    /// Load the library from SQLite.
    pub async fn load_library_async(&self) -> Result<Library, StorageError> {
        let mut library = Library::new();

        // Load all tracks into a map for quick lookup
        let mut tracks_map = std::collections::HashMap::new();
        let mut rows = self
            .conn
            .query("SELECT * FROM tracks", ())
            .await?;

        while let Some(row) = rows.next().await? {
            let track = self.row_to_track(&row)?;
            tracks_map.insert(track.id, track);
        }

        // Load playlists
        let mut playlist_rows = self
            .conn
            .query("SELECT * FROM playlists ORDER BY created_at", ())
            .await?;

        while let Some(row) = playlist_rows.next().await? {
            let playlist = self.row_to_playlist(&row)?;
            library.playlists.push(playlist);
        }

        // Load playlist tracks and associate them
        for playlist in &mut library.playlists {
            let mut track_rows = self
                .conn
                .query(
                    "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
                    params![playlist.id.to_string()],
                )
                .await?;

            while let Some(row) = track_rows.next().await? {
                let track_id_str: String = row.get(0)?;
                if let Ok(track_id) = Uuid::parse_str(&track_id_str) {
                    if let Some(track) = tracks_map.get(&track_id) {
                        playlist.tracks.push(track.clone());
                    }
                }
            }
        }

        // Load loose tracks (tracks not in any playlist)
        let mut loose_rows = self
            .conn
            .query(
                r#"
                SELECT t.* FROM tracks t
                WHERE NOT EXISTS (
                    SELECT 1 FROM playlist_tracks pt WHERE pt.track_id = t.id
                )
                "#,
                (),
            )
            .await?;

        while let Some(row) = loose_rows.next().await? {
            let track = self.row_to_track(&row)?;
            library.loose_tracks.push(track);
        }

        Ok(library)
    }

    /// Save the entire library to SQLite.
    pub async fn save_library_async(&self, library: &Library) -> Result<(), StorageError> {
        // Collect all tracks from playlists
        let mut all_tracks: Vec<&Track> = library
            .playlists
            .iter()
            .flat_map(|p| p.tracks.iter())
            .collect();
        all_tracks.extend(library.loose_tracks.iter());

        // Upsert all tracks
        for track in all_tracks {
            self.upsert_track(track).await?;
        }

        // Clear and rebuild playlists
        self.conn.execute("DELETE FROM playlist_tracks", ()).await?;
        self.conn.execute("DELETE FROM playlists", ()).await?;

        for playlist in &library.playlists {
            self.insert_playlist(playlist).await?;

            // Insert playlist-track associations
            for (position, track) in playlist.tracks.iter().enumerate() {
                self.conn
                    .execute(
                        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?1, ?2, ?3)",
                        params![playlist.id.to_string(), track.id.to_string(), position as i64],
                    )
                    .await?;
            }
        }

        Ok(())
    }

    /// Insert or update a track.
    async fn upsert_track(&self, track: &Track) -> Result<(), StorageError> {
        self.conn
            .execute(
                r#"
                INSERT INTO tracks (
                    id, path, title, artist, album, duration_ms, bpm, key, genre,
                    rating, comment, added_at, last_played, play_count, sample_rate,
                    bitrate, year, track_number
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                ON CONFLICT(id) DO UPDATE SET
                    path = excluded.path,
                    title = excluded.title,
                    artist = excluded.artist,
                    album = excluded.album,
                    duration_ms = excluded.duration_ms,
                    bpm = excluded.bpm,
                    key = excluded.key,
                    genre = excluded.genre,
                    rating = excluded.rating,
                    comment = excluded.comment,
                    last_played = excluded.last_played,
                    play_count = excluded.play_count,
                    sample_rate = excluded.sample_rate,
                    bitrate = excluded.bitrate,
                    year = excluded.year,
                    track_number = excluded.track_number
                "#,
                params![
                    track.id.to_string(),
                    track.path.to_string_lossy().to_string(),
                    track.title.clone(),
                    track.artist.clone(),
                    track.album.clone(),
                    track.duration.map(|d| d.as_millis() as i64),
                    track.bpm.map(|b| b as f64),
                    track.key.clone(),
                    track.genre.clone(),
                    track.rating.map(|r| r as i64),
                    track.comment.clone(),
                    track.added_at.to_rfc3339(),
                    track.last_played.map(|d| d.to_rfc3339()),
                    track.play_count as i64,
                    track.sample_rate.map(|s| s as i64),
                    track.bitrate.map(|b| b as i64),
                    track.year.map(|y| y as i64),
                    track.track_number.map(|t| t as i64),
                ],
            )
            .await?;
        Ok(())
    }

    /// Insert a playlist (without tracks).
    async fn insert_playlist(&self, playlist: &Playlist) -> Result<(), StorageError> {
        self.conn
            .execute(
                r#"
                INSERT INTO playlists (id, name, description, created_at, modified_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    playlist.id.to_string(),
                    playlist.name.clone(),
                    playlist.description.clone(),
                    playlist.created_at.to_rfc3339(),
                    playlist.modified_at.to_rfc3339(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Convert a database row to a Track.
    fn row_to_track(&self, row: &libsql::Row) -> Result<Track, StorageError> {
        let id_str: String = row.get(0)?;
        let path_str: String = row.get(1)?;
        let title: String = row.get(2)?;
        let artist: Option<String> = row.get(3)?;
        let album: Option<String> = row.get(4)?;
        let duration_ms: Option<i64> = row.get(5)?;
        let bpm: Option<f64> = row.get(6)?;
        let key: Option<String> = row.get(7)?;
        let genre: Option<String> = row.get(8)?;
        let rating: Option<i64> = row.get(9)?;
        let comment: Option<String> = row.get(10)?;
        let added_at_str: String = row.get(11)?;
        let last_played_str: Option<String> = row.get(12)?;
        let play_count: i64 = row.get(13)?;
        let sample_rate: Option<i64> = row.get(14)?;
        let bitrate: Option<i64> = row.get(15)?;
        let year: Option<i64> = row.get(16)?;
        let track_number: Option<i64> = row.get(17)?;

        Ok(Track {
            id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
            path: PathBuf::from(path_str),
            title,
            artist,
            album,
            duration: duration_ms.map(|ms| Duration::from_millis(ms as u64)),
            bpm: bpm.map(|b| b as f32),
            key,
            genre,
            rating: rating.map(|r| r as u8),
            comment,
            added_at: DateTime::parse_from_rfc3339(&added_at_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            last_played: last_played_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.with_timezone(&Utc))
                    .ok()
            }),
            play_count: play_count as u32,
            sample_rate: sample_rate.map(|s| s as u32),
            bitrate: bitrate.map(|b| b as u32),
            year: year.map(|y| y as u32),
            track_number: track_number.map(|t| t as u32),
        })
    }

    /// Convert a database row to a Playlist (without tracks).
    fn row_to_playlist(&self, row: &libsql::Row) -> Result<Playlist, StorageError> {
        let id_str: String = row.get(0)?;
        let name: String = row.get(1)?;
        let description: Option<String> = row.get(2)?;
        let created_at_str: String = row.get(3)?;
        let modified_at_str: String = row.get(4)?;

        Ok(Playlist {
            id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
            name,
            description,
            tracks: Vec::new(),
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            modified_at: DateTime::parse_from_rfc3339(&modified_at_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    /// Get the database path.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Check if the database is available.
    pub fn is_available(&self) -> bool {
        self.db_path.exists()
    }

    /// Get the storage location description.
    pub fn location(&self) -> String {
        self.db_path.display().to_string()
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

