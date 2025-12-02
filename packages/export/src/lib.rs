//! Export plugins for rec_bag DJ playlist manager.
//!
//! This crate provides extensible export functionality for playlists,
//! supporting various DJ software formats.

pub mod rekordbox;

use rec_core::Playlist;
use std::path::Path;
use thiserror::Error;

pub use rekordbox::RekordboxExporter;

/// Errors that can occur during export operations.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] quick_xml::DeError),
    #[error("Export failed: {0}")]
    ExportFailed(String),
}

/// Trait for playlist exporters.
///
/// Implement this trait to add support for new export formats.
pub trait PlaylistExporter {
    /// Get the name of this exporter (e.g., "Rekordbox XML").
    fn name(&self) -> &str;

    /// Get the file extension for this format (e.g., "xml").
    fn extension(&self) -> &str;

    /// Export a single playlist to the given path.
    fn export_playlist(&self, playlist: &Playlist, path: &Path) -> Result<(), ExportError>;

    /// Export multiple playlists to the given path.
    fn export_playlists(&self, playlists: &[Playlist], path: &Path) -> Result<(), ExportError>;
}

/// Registry of available exporters.
pub struct ExporterRegistry {
    exporters: Vec<Box<dyn PlaylistExporter>>,
}

impl Default for ExporterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExporterRegistry {
    /// Create a new registry with default exporters.
    pub fn new() -> Self {
        let mut registry = Self {
            exporters: Vec::new(),
        };

        // Register built-in exporters
        registry.register(Box::new(RekordboxExporter::new()));

        registry
    }

    /// Register a new exporter.
    pub fn register(&mut self, exporter: Box<dyn PlaylistExporter>) {
        self.exporters.push(exporter);
    }

    /// Get all available exporters.
    pub fn exporters(&self) -> &[Box<dyn PlaylistExporter>] {
        &self.exporters
    }

    /// Find an exporter by name.
    pub fn find_by_name(&self, name: &str) -> Option<&dyn PlaylistExporter> {
        self.exporters
            .iter()
            .find(|e| e.name() == name)
            .map(|e| e.as_ref())
    }

    /// Find an exporter by file extension.
    pub fn find_by_extension(&self, ext: &str) -> Option<&dyn PlaylistExporter> {
        self.exporters
            .iter()
            .find(|e| e.extension() == ext)
            .map(|e| e.as_ref())
    }

    /// Get list of available exporter names.
    pub fn available_formats(&self) -> Vec<&str> {
        self.exporters.iter().map(|e| e.name()).collect()
    }
}
