//! Music source model for managing folder locations.
//!
//! Designed to support both local and cloud storage backends in the future.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Type of music source - extensible for future cloud backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    /// Local filesystem folder
    #[default]
    Local,
    /// Amazon S3 bucket (future)
    S3,
    /// Google Drive folder (future)
    GoogleDrive,
    /// Dropbox folder (future)
    Dropbox,
    /// WebDAV server (future)
    WebDav,
}

impl SourceType {
    /// Get string representation for database storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceType::Local => "local",
            SourceType::S3 => "s3",
            SourceType::GoogleDrive => "google_drive",
            SourceType::Dropbox => "dropbox",
            SourceType::WebDav => "webdav",
        }
    }

    /// Parse from string (database).
    pub fn from_str(s: &str) -> Self {
        match s {
            "local" => SourceType::Local,
            "s3" => SourceType::S3,
            "google_drive" => SourceType::GoogleDrive,
            "dropbox" => SourceType::Dropbox,
            "webdav" => SourceType::WebDav,
            _ => SourceType::Local,
        }
    }
}

/// A music source representing a location containing audio files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicSource {
    /// Unique identifier
    pub id: Uuid,
    /// Display name (defaults to folder name)
    pub name: String,
    /// Source type (local, cloud, etc.)
    pub source_type: SourceType,
    /// URI/path to the source
    /// - Local: absolute filesystem path (e.g., "/home/user/Music")
    /// - S3: s3://bucket/prefix
    /// - Google Drive: gdrive://folder_id
    /// - etc.
    pub uri: String,
    /// Type-specific configuration as JSON (credentials, options, etc.)
    /// For local sources, this is typically empty or contains scan options.
    pub config: Option<String>,
    /// Number of tracks found in this source
    pub track_count: u32,
    /// Whether this source is enabled for scanning
    pub enabled: bool,
    /// When the source was added
    pub created_at: DateTime<Utc>,
    /// When the source was last scanned
    pub last_scanned: Option<DateTime<Utc>>,
}

impl MusicSource {
    /// Create a new local filesystem source.
    pub fn new_local(path: impl Into<String>) -> Self {
        let path = path.into();
        let name = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Music")
            .to_string();

        Self {
            id: Uuid::new_v4(),
            name,
            source_type: SourceType::Local,
            uri: path,
            config: None,
            track_count: 0,
            enabled: true,
            created_at: Utc::now(),
            last_scanned: None,
        }
    }

    /// Create a source with a custom name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Check if this is a local filesystem source.
    pub fn is_local(&self) -> bool {
        self.source_type == SourceType::Local
    }

    /// Get the path for local sources.
    pub fn local_path(&self) -> Option<&str> {
        if self.is_local() {
            Some(&self.uri)
        } else {
            None
        }
    }

    /// Update the track count after a scan.
    pub fn set_track_count(&mut self, count: u32) {
        self.track_count = count;
        self.last_scanned = Some(Utc::now());
    }

    /// Mark the source as just scanned.
    pub fn mark_scanned(&mut self) {
        self.last_scanned = Some(Utc::now());
    }
}

/// Configuration for local source scanning.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalSourceConfig {
    /// Whether to scan subdirectories recursively
    pub recursive: bool,
    /// File extensions to include (empty = all supported)
    pub extensions: Vec<String>,
    /// Paths to exclude (relative to source root)
    pub exclude_paths: Vec<String>,
}

impl LocalSourceConfig {
    /// Create default config for recursive scanning.
    pub fn recursive() -> Self {
        Self {
            recursive: true,
            ..Default::default()
        }
    }
}

