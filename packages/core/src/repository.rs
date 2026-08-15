//! Narrow persistence contracts for the canonical catalog.
//!
//! Repository implementations belong in the persistence crate. Methods take owned
//! values where practical so their boxed futures remain straightforward and
//! dynamically dispatchable.

use crate::asset::{
    AssetLocation, ContentHash, DerivedAsset, MusicStorage, OriginalAsset, StorageObject,
};
use crate::catalog::{
    CanonicalPlaylist, CanonicalTrack, CatalogSource, ExternalPlaylist, ExternalPlaylistEntry,
    SourceAccount, SourceCollection, SourceCollectionRelationship,
    SourceCollectionRelationshipIdentity, SourceEntityKind, SourceTrackOccurrence,
};
use crate::provider::BoxFuture;
use crate::reconciliation::{
    MatchCandidate, MatchConfidence, ReconciliationDecision, ReconciliationOutcome,
    ReconciliationResolution, ReconciliationResult, ReconciliationSuppression,
};
use crate::sync::{SyncJob, SyncRun, SyncRunKind, SyncStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

/// Opaque continuation state for repository list operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryCursor(pub String);

/// A bounded repository query. Implementations own cursor interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepositoryPageRequest {
    pub cursor: Option<RepositoryCursor>,
    pub limit: Option<u32>,
}

/// A page of persisted domain records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<RepositoryCursor>,
}

/// Stable persistence error classifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryErrorKind {
    NotFound,
    Conflict,
    ConstraintViolation,
    InvalidData,
    TemporarilyUnavailable,
    Io,
    Permanent,
}

/// A structured error returned by repository contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryError {
    pub kind: RepositoryErrorKind,
    pub message: String,
    /// Optional stable database or constraint code; never contains secrets.
    pub code: Option<String>,
}

impl RepositoryError {
    pub fn new(kind: RepositoryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            RepositoryErrorKind::TemporarilyUnavailable | RepositoryErrorKind::Io
        )
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl Error for RepositoryError {}

pub type RepositoryResult<T> = Result<T, RepositoryError>;

/// A non-secret application setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: Value,
    pub updated_at: DateTime<Utc>,
}

/// A versioned, integrity-addressed catalog recovery snapshot.
///
/// The payload must contain no credentials or access tokens. Its encoding is
/// identified by `content_type` and versioned independently from database schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub id: Uuid,
    pub format_version: u32,
    pub created_at: DateTime<Utc>,
    pub content_type: String,
    content_hash: ContentHash,
    payload: Vec<u8>,
}

impl CatalogSnapshot {
    /// Create a snapshot whose integrity hash is derived from its serialized payload.
    pub fn new(format_version: u32, content_type: impl Into<String>, payload: Vec<u8>) -> Self {
        let digest = Sha256::digest(&payload);
        let hash = format!("{digest:x}");
        let content_hash = ContentHash::sha256(hash)
            .expect("SHA-256 formatting always produces a valid canonical digest");

        Self {
            id: Uuid::new_v4(),
            format_version,
            created_at: Utc::now(),
            content_type: content_type.into(),
            content_hash,
            payload,
        }
    }

    pub fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Recompute and compare the payload hash before restore or replication.
    pub fn verify_integrity(&self) -> bool {
        let digest = Sha256::digest(&self.payload);
        let hash = format!("{digest:x}");
        ContentHash::sha256(hash).is_ok_and(|actual| actual == self.content_hash)
    }
}

/// Filters durable source collection relationships while retaining source scope as a
/// required boundary.
///
/// Account, collection, and entity filters are optional so callers can page through an
/// entire source or narrow a collection review/update operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCollectionRelationshipFilter {
    pub source_id: Uuid,
    pub account_external_id: Option<String>,
    pub collection: Option<SourceCollection>,
    pub entity_kind: Option<SourceEntityKind>,
}

/// Isolates staged external-playlist entries from both other runs and provider
/// snapshots. `snapshot` may be absent for providers without revision tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPlaylistEntryStagingScope {
    pub run_id: Uuid,
    pub source_id: Uuid,
    pub playlist_external_id: String,
    pub snapshot: Option<String>,
}

/// Whether an inventory generation is a complete view or an incremental change set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryGenerationKind {
    Full,
    Incremental,
}

/// Durable lifecycle of one attempted storage inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryGenerationStatus {
    Staging,
    Finalized,
    Unavailable,
    Failed,
}

/// Input for beginning an inventory generation. IDs are caller-assigned to make
/// retries idempotent and to correlate the generation with its sync run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryGenerationStart {
    pub id: Uuid,
    pub storage_id: Uuid,
    pub run_id: Uuid,
    pub kind: InventoryGenerationKind,
    pub started_at: DateTime<Utc>,
}

/// Persisted inventory generation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryGeneration {
    pub id: Uuid,
    pub storage_id: Uuid,
    pub run_id: Uuid,
    pub kind: InventoryGenerationKind,
    pub status: InventoryGenerationStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Non-secret diagnostic detail for unavailable or failed generations.
    pub message: Option<String>,
}

/// A durable removal observation for a provider object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObjectTombstone {
    pub storage_id: Uuid,
    pub object_id: String,
    pub generation_id: Uuid,
    pub removed_at: DateTime<Utc>,
    pub provider_version: Option<String>,
}

/// One bounded, idempotently staged inventory page.
///
/// Objects are keyed by `(storage_id, object_id)` within the generation. Tombstones
/// carry explicit removals for incremental inventories; a successful full-generation
/// finalize additionally tombstones previously current objects not observed anywhere
/// in the generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InventoryStagingPage {
    pub objects: Vec<StorageObject>,
    pub tombstones: Vec<StorageObjectTombstone>,
}

/// Why an incomplete inventory generation ended without replacing current inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryGenerationAbortKind {
    Unavailable,
    Failed,
}

/// Terminal information for an inventory generation that cannot be finalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryGenerationAbort {
    pub kind: InventoryGenerationAbortKind,
    pub finished_at: DateTime<Utc>,
    pub message: Option<String>,
}

/// Filters match candidates for bounded reconciliation and review queues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MatchCandidateFilter {
    pub track_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
    pub confidence: Option<MatchConfidence>,
    /// Filters by [`MatchCandidate::requires_confirmation`] when present.
    pub review_required: Option<bool>,
}

/// Filters persisted reconciliation findings for bounded review queues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReconciliationResultFilter {
    pub track_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
    pub storage_object_id: Option<String>,
    /// Empty accepts every outcome.
    pub outcomes: Vec<ReconciliationOutcome>,
    /// Selects findings with or without a pending review action.
    pub review_required: Option<bool>,
}

/// Filters source occurrences without requiring callers to load a complete source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceOccurrenceFilter {
    pub source_id: Option<Uuid>,
    pub canonical_track_id: Option<Uuid>,
    pub account_external_id: Option<String>,
}

/// Filters original assets for bounded reconciliation input assembly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OriginalAssetFilter {
    pub track_id: Option<Uuid>,
}

/// Filters asset locations for bounded verification and reconciliation input assembly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssetLocationFilter {
    pub asset_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
}

/// Filters external playlists within a source namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExternalPlaylistFilter {
    pub source_id: Option<Uuid>,
    pub owner_external_id: Option<String>,
}

/// Filters durable decisions and suppressions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReconciliationResolutionFilter {
    pub result_id: Option<Uuid>,
    pub candidate_id: Option<Uuid>,
    pub decision: Option<ReconciliationDecision>,
}

/// Filters sync-run history and latest-run lookups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncRunFilter {
    pub kind: Option<SyncRunKind>,
    pub status: Option<SyncStatus>,
    pub source_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
}

/// Persistence operations for canonical tracks.
pub trait TrackRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CanonicalTrack>>>;

    fn list(
        &self,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CanonicalTrack>>>;

    fn upsert(&self, track: CanonicalTrack) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for native and source-owned playlists.
pub trait PlaylistRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CanonicalPlaylist>>>;

    fn list(
        &self,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CanonicalPlaylist>>>;

    fn upsert(&self, playlist: CanonicalPlaylist) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_external(
        &self,
        source_id: Uuid,
        external_id: String,
    ) -> BoxFuture<'_, RepositoryResult<Option<ExternalPlaylist>>>;

    fn upsert_external(&self, playlist: ExternalPlaylist) -> BoxFuture<'_, RepositoryResult<()>>;

    fn list_external(
        &self,
        filter: ExternalPlaylistFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<ExternalPlaylist>>>;

    fn list_external_entries(
        &self,
        source_id: Uuid,
        external_id: String,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<ExternalPlaylistEntry>>>;

    /// Atomically replaces ordering for one external playlist when all entries are
    /// already available in memory.
    fn replace_external_entries(
        &self,
        source_id: Uuid,
        external_id: String,
        entries: Vec<ExternalPlaylistEntry>,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    /// Begins or idempotently resets an isolated staging set.
    fn begin_external_entry_staging(
        &self,
        scope: ExternalPlaylistEntryStagingScope,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    /// Upserts a bounded entry page into the staging set. Replaying a page must not
    /// duplicate entries; provider position is the entry identity within the scope.
    fn stage_external_entry_page(
        &self,
        scope: ExternalPlaylistEntryStagingScope,
        entries: Vec<ExternalPlaylistEntry>,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    /// Atomically replaces the visible entry set with the complete staged set and, if
    /// present, records the staged provider snapshot on the external playlist.
    fn finalize_external_entry_staging(
        &self,
        scope: ExternalPlaylistEntryStagingScope,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    fn discard_external_entry_staging(
        &self,
        scope: ExternalPlaylistEntryStagingScope,
    ) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for catalog sources and their normalized occurrences.
pub trait SourceRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CatalogSource>>>;

    fn list(
        &self,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CatalogSource>>>;

    fn upsert(&self, source: CatalogSource) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_account(
        &self,
        source_id: Uuid,
        external_id: String,
    ) -> BoxFuture<'_, RepositoryResult<Option<SourceAccount>>>;

    fn list_accounts(
        &self,
        source_id: Uuid,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<SourceAccount>>>;

    fn upsert_account(&self, account: SourceAccount) -> BoxFuture<'_, RepositoryResult<()>>;

    fn list_collection_relationships(
        &self,
        filter: SourceCollectionRelationshipFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<SourceCollectionRelationship>>>;

    fn upsert_collection_relationship(
        &self,
        relationship: SourceCollectionRelationship,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    fn remove_collection_relationship(
        &self,
        identity: SourceCollectionRelationshipIdentity,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_occurrence(
        &self,
        source_id: Uuid,
        external_id: String,
    ) -> BoxFuture<'_, RepositoryResult<Option<SourceTrackOccurrence>>>;

    fn list_occurrences(
        &self,
        filter: SourceOccurrenceFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<SourceTrackOccurrence>>>;

    /// Batch upsert is the idempotent persistence primitive for source pages.
    fn upsert_occurrences(
        &self,
        occurrences: Vec<SourceTrackOccurrence>,
    ) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for logical assets and provider objects.
pub trait AssetRepository: Send + Sync {
    fn get_original(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<OriginalAsset>>>;

    fn list_originals_for_track(
        &self,
        track_id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Vec<OriginalAsset>>>;

    fn list_originals(
        &self,
        filter: OriginalAssetFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<OriginalAsset>>>;

    fn upsert_original(&self, asset: OriginalAsset) -> BoxFuture<'_, RepositoryResult<()>>;

    fn upsert_derived(&self, asset: DerivedAsset) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_storage_object(
        &self,
        storage_id: Uuid,
        object_id: String,
    ) -> BoxFuture<'_, RepositoryResult<Option<StorageObject>>>;

    fn list_storage_objects(
        &self,
        storage_id: Uuid,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<StorageObject>>>;

    fn upsert_storage_object(&self, object: StorageObject) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_location(
        &self,
        storage_id: Uuid,
        object_id: String,
    ) -> BoxFuture<'_, RepositoryResult<Option<AssetLocation>>>;

    fn list_locations_for_asset(
        &self,
        asset_id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Vec<AssetLocation>>>;

    fn list_locations(
        &self,
        filter: AssetLocationFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<AssetLocation>>>;

    fn upsert_location(&self, location: AssetLocation) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_inventory_generation(
        &self,
        id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Option<InventoryGeneration>>>;

    fn latest_finalized_inventory_generation(
        &self,
        storage_id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Option<InventoryGeneration>>>;

    fn begin_inventory_generation(
        &self,
        start: InventoryGenerationStart,
    ) -> BoxFuture<'_, RepositoryResult<InventoryGeneration>>;

    /// Stages a page without changing the storage's current visible inventory.
    fn stage_inventory_page(
        &self,
        generation_id: Uuid,
        page: InventoryStagingPage,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    /// Atomically activates a fully traversed generation and applies its removals.
    ///
    /// For a full generation this also creates tombstones for objects absent from the
    /// completed traversal. This method must never be used to represent provider
    /// unavailability; an incomplete traversal must be aborted instead.
    fn finalize_inventory_generation(
        &self,
        generation_id: Uuid,
        finalized_at: DateTime<Utc>,
    ) -> BoxFuture<'_, RepositoryResult<InventoryGeneration>>;

    /// Terminates staging while preserving the prior finalized inventory unchanged.
    fn abort_inventory_generation(
        &self,
        generation_id: Uuid,
        abort: InventoryGenerationAbort,
    ) -> BoxFuture<'_, RepositoryResult<InventoryGeneration>>;

    fn list_storage_object_tombstones(
        &self,
        storage_id: Uuid,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<StorageObjectTombstone>>>;
}

/// Persistence operations for proposed matches and reconciliation findings.
pub trait ReconciliationRepository: Send + Sync {
    fn get_candidate(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<MatchCandidate>>>;

    fn list_candidates(
        &self,
        filter: MatchCandidateFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<MatchCandidate>>>;

    fn upsert_candidate(&self, candidate: MatchCandidate) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_result(&self, id: Uuid)
        -> BoxFuture<'_, RepositoryResult<Option<ReconciliationResult>>>;

    /// Finds the persisted finding that owns a candidate without scanning result blobs.
    fn get_result_by_candidate(
        &self,
        candidate_id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Option<ReconciliationResult>>>;

    fn list_results(
        &self,
        filter: ReconciliationResultFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<ReconciliationResult>>>;

    fn upsert_result(&self, result: ReconciliationResult) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_resolution(
        &self,
        id: Uuid,
    ) -> BoxFuture<'_, RepositoryResult<Option<ReconciliationResolution>>>;

    fn list_resolutions(
        &self,
        filter: ReconciliationResolutionFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<ReconciliationResolution>>>;

    /// Persists a decision and any derived suppression atomically.
    fn record_resolution(
        &self,
        resolution: ReconciliationResolution,
        suppression: Option<ReconciliationSuppression>,
    ) -> BoxFuture<'_, RepositoryResult<()>>;

    fn list_suppressions(
        &self,
        filter: ReconciliationResolutionFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<ReconciliationSuppression>>>;

    fn remove_suppression(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for configured primary and backup music storages.
pub trait MusicStorageRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<MusicStorage>>>;

    fn list(&self) -> BoxFuture<'_, RepositoryResult<Vec<MusicStorage>>>;

    /// Persists configuration without implicitly assigning it as active primary. A
    /// storage with `StorageRole::Unassigned` can therefore remain a migration
    /// candidate until an explicit switch succeeds.
    fn upsert(&self, storage: MusicStorage) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_active_primary(&self) -> BoxFuture<'_, RepositoryResult<Option<MusicStorage>>>;

    /// Atomically compare-and-swaps the profile's active primary assignment.
    ///
    /// `new_active_storage_id == None` explicitly leaves the profile unassigned. A new
    /// active ID may refer to an unassigned candidate; implementations atomically
    /// promote it to primary, demote the previous primary to unassigned, and update the
    /// profile setting. They return [`RepositoryErrorKind::Conflict`] when the current
    /// ID differs from `expected_active_storage_id`, without partially updating roles
    /// or settings on failure.
    fn switch_active_primary(
        &self,
        expected_active_storage_id: Option<Uuid>,
        new_active_storage_id: Option<Uuid>,
    ) -> BoxFuture<'_, RepositoryResult<Option<MusicStorage>>>;
}

/// Persistence operations for source, inventory, verification, and replication runs.
pub trait SyncRunRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<SyncRun>>>;

    fn list(
        &self,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<SyncRun>>>;

    fn list_filtered(
        &self,
        filter: SyncRunFilter,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<SyncRun>>>;

    /// Returns the most recently created run matching the complete filter.
    fn latest(&self, filter: SyncRunFilter) -> BoxFuture<'_, RepositoryResult<Option<SyncRun>>>;

    /// Inserts or updates a run, including its cursor, progress, and terminal state.
    fn upsert(&self, run: SyncRun) -> BoxFuture<'_, RepositoryResult<()>>;

    fn get_job(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<SyncJob>>>;

    fn list_jobs(&self, run_id: Uuid) -> BoxFuture<'_, RepositoryResult<Vec<SyncJob>>>;

    fn upsert_job(&self, job: SyncJob) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for non-secret settings.
pub trait SettingsRepository: Send + Sync {
    fn get(&self, key: String) -> BoxFuture<'_, RepositoryResult<Option<Setting>>>;

    fn list(&self) -> BoxFuture<'_, RepositoryResult<Vec<Setting>>>;

    fn set(&self, setting: Setting) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Persistence operations for catalog recovery snapshots.
pub trait CatalogSnapshotRepository: Send + Sync {
    fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CatalogSnapshot>>>;

    fn latest(&self) -> BoxFuture<'_, RepositoryResult<Option<CatalogSnapshot>>>;

    fn list(
        &self,
        page: RepositoryPageRequest,
    ) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CatalogSnapshot>>>;

    /// Stores a snapshot idempotently by its identifier or content hash.
    /// Implementations must reject snapshots for which `verify_integrity()` is false.
    fn put(&self, snapshot: CatalogSnapshot) -> BoxFuture<'_, RepositoryResult<()>>;
}

/// Repository views bound to one persistence transaction.
///
/// This unit-of-work shape makes applying a provider page and advancing its sync
/// cursor atomic without forcing transaction parameters into every repository API.
pub trait RepositoryTransaction: Send + Sync {
    fn tracks(&self) -> &dyn TrackRepository;
    fn playlists(&self) -> &dyn PlaylistRepository;
    fn sources(&self) -> &dyn SourceRepository;
    fn assets(&self) -> &dyn AssetRepository;
    fn reconciliation(&self) -> &dyn ReconciliationRepository;
    fn music_storages(&self) -> &dyn MusicStorageRepository;
    fn sync_runs(&self) -> &dyn SyncRunRepository;
    fn settings(&self) -> &dyn SettingsRepository;
    fn catalog_snapshots(&self) -> &dyn CatalogSnapshotRepository;

    fn commit<'a>(self: Box<Self>) -> BoxFuture<'a, RepositoryResult<()>>
    where
        Self: 'a;

    fn rollback<'a>(self: Box<Self>) -> BoxFuture<'a, RepositoryResult<()>>
    where
        Self: 'a;
}

/// Begins repository transactions without exposing a concrete database type.
pub trait TransactionManager: Send + Sync {
    fn begin<'a>(&'a self) -> BoxFuture<'a, RepositoryResult<Box<dyn RepositoryTransaction + 'a>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts_track_repository(_: &dyn TrackRepository) {}
    fn accepts_playlist_repository(_: &dyn PlaylistRepository) {}
    fn accepts_source_repository(_: &dyn SourceRepository) {}
    fn accepts_asset_repository(_: &dyn AssetRepository) {}
    fn accepts_reconciliation_repository(_: &dyn ReconciliationRepository) {}
    fn accepts_music_storage_repository(_: &dyn MusicStorageRepository) {}
    fn accepts_sync_run_repository(_: &dyn SyncRunRepository) {}
    fn accepts_settings_repository(_: &dyn SettingsRepository) {}
    fn accepts_catalog_snapshot_repository(_: &dyn CatalogSnapshotRepository) {}
    fn accepts_repository_transaction(_: &dyn RepositoryTransaction) {}
    fn accepts_transaction_manager(_: &dyn TransactionManager) {}

    #[test]
    fn repository_traits_are_object_safe() {
        let _ = accepts_track_repository;
        let _ = accepts_playlist_repository;
        let _ = accepts_source_repository;
        let _ = accepts_asset_repository;
        let _ = accepts_reconciliation_repository;
        let _ = accepts_music_storage_repository;
        let _ = accepts_sync_run_repository;
        let _ = accepts_settings_repository;
        let _ = accepts_catalog_snapshot_repository;
        let _ = accepts_repository_transaction;
        let _ = accepts_transaction_manager;
    }

    #[test]
    fn repository_error_round_trips() {
        let error = RepositoryError::new(RepositoryErrorKind::Conflict, "stale write");
        let json = serde_json::to_string(&error).unwrap();
        let decoded: RepositoryError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, error);
    }

    #[test]
    fn catalog_snapshot_hashes_and_verifies_its_payload() {
        let snapshot = CatalogSnapshot::new(1, "application/json", br#"{"tracks":[]}"#.to_vec());

        assert!(snapshot.content_hash().is_canonical_sha256());
        assert!(snapshot.verify_integrity());
        assert_eq!(snapshot.payload(), br#"{"tracks":[]}"#);
    }
}
