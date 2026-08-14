//! Provider-independent contracts for catalog sources and music asset stores.
//!
//! Implementations live in provider crates. This module intentionally contains no
//! HTTP client, filesystem, credential, or UI dependencies.

use crate::asset::{ContentHash, StorageCapabilities, StorageObject};
use crate::catalog::{
    ExternalPlaylist, ExternalPlaylistEntry, SourceAccount, SourceCapabilities,
    SourceTrackIdentity, SourceTrackOccurrence,
};
pub use crate::catalog::{
    SourceCollection, SourceCollectionRelationship, SourceCollectionRelationshipIdentity,
    SourceEntityKind,
};
pub use crate::sync::{SyncCursor, SyncProgress};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// A boxed, sendable future used to keep async contracts object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The current operational state reported by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unavailable,
    AuthenticationRequired,
}

/// Health and capability information for a catalog connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHealth {
    pub status: HealthStatus,
    pub checked_at: DateTime<Utc>,
    pub message: Option<String>,
    pub capabilities: SourceCapabilities,
}

/// Selects a full synchronization or continuation from a durable checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncMode {
    Full,
    Incremental { cursor: SyncCursor },
}

/// Requests one bounded page from a catalog source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSyncRequest {
    pub mode: SourceSyncMode,
    /// Empty means every collection supported by the connector.
    pub collections: Vec<SourceCollection>,
    /// Continuation cursor from the preceding page in this run.
    pub page_cursor: Option<SyncCursor>,
    /// A hint; providers may return fewer records.
    pub page_size: Option<u32>,
}

/// A provider record emitted during catalog synchronization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum SourceSyncItem {
    Account(SourceAccount),
    Track(SourceTrackOccurrence),
    Playlist(ExternalPlaylist),
    /// An entry emitted separately from its playlist for bounded pagination.
    PlaylistEntry {
        /// Provider playlist identifier in the connector's source namespace.
        playlist_external_id: String,
        /// Opaque playlist revision to which this page of entries belongs.
        snapshot: Option<String>,
        entry: ExternalPlaylistEntry,
    },
    /// A current collection membership observation.
    Relationship(SourceCollectionRelationship),
    /// Removal of collection membership without removal of the referenced entity.
    RelationshipRemoval(SourceCollectionRelationshipIdentity),
}

/// A source-side entity removal or loss-of-access notice.
///
/// This targets only account, track, or playlist entities. Collection membership and
/// playlist-entry changes use [`SourceSyncItem::RelationshipRemoval`] so removing one
/// like or playlist position cannot remove the source track occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRemoval {
    pub entity_kind: SourceEntityKind,
    pub external_id: String,
}

/// One page of normalized source records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSyncPage {
    pub items: Vec<SourceSyncItem>,
    pub removals: Vec<SourceRemoval>,
    /// Present when another page belongs to this synchronization run.
    pub next_page_cursor: Option<SyncCursor>,
    /// Durable checkpoint. Consumers should persist it only after committing the page.
    pub next_sync_cursor: Option<SyncCursor>,
    pub progress: Option<SyncProgress>,
}

/// Health and capability information for a music asset store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageHealth {
    pub status: HealthStatus,
    pub checked_at: DateTime<Utc>,
    pub message: Option<String>,
    pub capabilities: StorageCapabilities,
}

/// Selects a complete inventory or changes after a committed high-water mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InventoryMode {
    Full,
    Incremental { change_cursor: SyncCursor },
}

/// Requests one bounded page of objects beneath the configured storage root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryRequest {
    pub mode: InventoryMode,
    pub page_cursor: Option<SyncCursor>,
    pub page_size: Option<u32>,
    /// Provider-independent relative path prefix.
    pub prefix: Option<String>,
    /// Hint that checksums should be populated when available cheaply.
    pub include_checksums: bool,
}

/// A provider-confirmed storage object removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObjectRemoval {
    pub object_id: String,
    pub provider_version: Option<String>,
    pub removed_at: DateTime<Utc>,
}

/// One page from a music-storage inventory or change feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryPage {
    pub objects: Vec<StorageObject>,
    /// Explicit removals are normally populated by incremental change feeds.
    pub removals: Vec<StorageObjectRemoval>,
    pub next_page_cursor: Option<SyncCursor>,
    /// New durable high-water mark. Consumers commit it only after the complete page
    /// sequence and its staged inventory generation have committed successfully.
    pub next_change_cursor: Option<SyncCursor>,
    pub progress: Option<SyncProgress>,
}

/// Requests one page of retained provider versions for an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageVersionRequest {
    pub object_id: String,
    pub page_cursor: Option<SyncCursor>,
    pub page_size: Option<u32>,
}

/// Provider-neutral metadata for one retained object version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObjectVersion {
    pub object_id: String,
    pub version_id: String,
    pub modified_at: Option<DateTime<Utc>>,
    pub size_bytes: Option<u64>,
    pub hashes: Vec<ContentHash>,
    pub is_current: bool,
}

/// One page of retained object versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageVersionPage {
    pub versions: Vec<StorageObjectVersion>,
    pub next_page_cursor: Option<SyncCursor>,
}

/// Uploads a file staged on the host into a provider-relative object path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRequest {
    pub source_path: PathBuf,
    pub object_path: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub expected_checksum: Option<ContentHash>,
    /// Stable rec_bag metadata, not provider-specific API fields.
    pub metadata: BTreeMap<String, String>,
    /// Allows implementations to make retries idempotent when supported.
    pub idempotency_key: Option<String>,
}

/// An optional inclusive-start, exclusive-end byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end_exclusive: Option<u64>,
}

/// Downloads an object to a host staging path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub object_id: String,
    pub destination_path: PathBuf,
    pub range: Option<ByteRange>,
    pub expected_checksum: Option<ContentHash>,
    pub overwrite: bool,
}

/// Result of a completed download and any requested verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadResult {
    pub object: StorageObject,
    pub destination_path: PathBuf,
    pub bytes_written: u64,
    pub checksum: Option<ContentHash>,
    pub checksum_verified: bool,
}

/// Requests an obtainable source track file at a host destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogAssetReadRequest {
    pub track: SourceTrackIdentity,
    pub destination_path: PathBuf,
    pub expected_checksum: Option<ContentHash>,
}

/// Result of reading an obtainable source track file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogAssetReadResult {
    pub destination_path: PathBuf,
    pub bytes_written: u64,
    pub checksum: Option<ContentHash>,
}

/// Stable classifications for catalog-provider failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceErrorKind {
    AuthenticationRequired,
    PermissionDenied,
    RateLimited,
    TemporarilyUnavailable,
    NotFound,
    DownloadProhibited,
    ChecksumMismatch,
    Io,
    InvalidData,
    Provider,
    Permanent,
}

/// A structured catalog-provider error safe to pass across orchestration layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceError {
    pub kind: SourceErrorKind,
    pub message: String,
    pub provider_code: Option<String>,
    pub request_id: Option<String>,
    pub retry_after_seconds: Option<u64>,
}

impl SourceError {
    pub fn new(kind: SourceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_code: None,
            request_id: None,
            retry_after_seconds: None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            SourceErrorKind::RateLimited
                | SourceErrorKind::TemporarilyUnavailable
                | SourceErrorKind::Provider
        )
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl Error for SourceError {}

/// Stable classifications for music-storage failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicStorageErrorKind {
    AuthenticationRequired,
    PermissionDenied,
    RateLimited,
    TemporarilyUnavailable,
    NotFound,
    DownloadProhibited,
    InvalidData,
    ChecksumMismatch,
    Io,
    Provider,
    Permanent,
}

/// A structured music-storage error safe to pass across orchestration layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MusicStorageError {
    pub kind: MusicStorageErrorKind,
    pub message: String,
    pub provider_code: Option<String>,
    pub request_id: Option<String>,
    pub retry_after_seconds: Option<u64>,
}

impl MusicStorageError {
    pub fn new(kind: MusicStorageErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_code: None,
            request_id: None,
            retry_after_seconds: None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            MusicStorageErrorKind::RateLimited
                | MusicStorageErrorKind::TemporarilyUnavailable
                | MusicStorageErrorKind::Io
                | MusicStorageErrorKind::Provider
        )
    }
}

impl fmt::Display for MusicStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl Error for MusicStorageError {}

/// Dynamically dispatchable catalog-source connector.
pub trait CatalogSourceConnector: Send + Sync {
    fn health(&self) -> BoxFuture<'_, Result<SourceHealth, SourceError>>;

    fn sync_page(
        &self,
        request: SourceSyncRequest,
    ) -> BoxFuture<'_, Result<SourceSyncPage, SourceError>>;
}

/// Dynamically dispatchable reader for legitimately obtainable catalog-source files.
///
/// This is intentionally separate from [`CatalogSourceConnector`]: metadata-only and
/// streaming-only connectors do not need to implement file acquisition.
pub trait CatalogAssetReader: Send + Sync {
    fn read_asset(
        &self,
        request: CatalogAssetReadRequest,
    ) -> BoxFuture<'_, Result<CatalogAssetReadResult, SourceError>>;
}

/// Dynamically dispatchable music asset store.
///
/// Destructive deletion is deliberately absent. Cleanup requires a later,
/// explicit retention contract and confirmation flow.
pub trait MusicAssetStore: Send + Sync {
    fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>>;

    fn inventory_page(
        &self,
        request: InventoryRequest,
    ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>>;

    fn stat<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>>;

    /// Enumerate retained versions when `supports_version_history` is advertised.
    fn versions_page(
        &self,
        request: StorageVersionRequest,
    ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>>;

    fn upload(
        &self,
        request: UploadRequest,
    ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>>;

    fn download(
        &self,
        request: DownloadRequest,
    ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts_catalog_trait_object(_: &dyn CatalogSourceConnector) {}
    fn accepts_catalog_asset_reader_trait_object(_: &dyn CatalogAssetReader) {}
    fn accepts_storage_trait_object(_: &dyn MusicAssetStore) {}

    #[test]
    fn errors_round_trip_and_preserve_classification() {
        let mut error = SourceError::new(SourceErrorKind::RateLimited, "slow down");
        error.retry_after_seconds = Some(30);

        let json = serde_json::to_string(&error).unwrap();
        let decoded: SourceError = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, error);
        assert!(decoded.is_retryable());
    }

    #[test]
    fn traits_are_object_safe() {
        let _ = accepts_catalog_trait_object;
        let _ = accepts_catalog_asset_reader_trait_object;
        let _ = accepts_storage_trait_object;
    }
}
