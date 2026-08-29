//! Core library for rec_bag DJ playlist manager.
//!
//! This crate provides:
//! - Multi-channel audio playback engine using rodio (requires `audio` feature)
//! - Track and playlist data models
//! - Audio analysis (waveform, BPM, key detection)
//! - Fuzzy search for tracks
//! - Track diagnostics (Doctor) for detecting and fixing compatibility issues

#[cfg(feature = "audio")]
pub mod analysis;
pub mod asset;
#[cfg(feature = "audio")]
pub mod audio;
pub mod catalog;
pub mod doctor;
pub mod folder;
pub mod playlist;
pub mod provider;
pub mod reconciliation;
pub mod repository;
pub mod schema;
pub mod search;
pub mod source;
pub mod sync;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod track;
#[cfg(feature = "audio")]
pub mod waveform;

#[cfg(feature = "audio")]
pub use analysis::{
    analyze_track, AnalysisError, AnalysisResult, Analyzer, AnalyzerOutput, BeatGridAnalyzer,
    BeatGridResult, BpmAnalyzer, BpmResult, FrequencyWaveform, FrequencyWaveformAnalyzer,
    KeyAnalyzer, KeyResult, MusicalKey, WaveformAnalyzer,
};
pub use asset::{
    AssetLocation, AssetRecord, AssetType, ContentHash, ContentHashError, DerivedAsset,
    DerivedAssetRole, HashAlgorithm, MusicStorage, MusicStorageKind, OriginalAsset,
    OriginalAssetRole, ReplicationMode, RetentionPolicy, SourceCopyPolicy, StorageCapabilities,
    StorageObject, StorageRole, StorageVerificationStatus,
};
#[cfg(feature = "audio")]
pub use audio::{AudioEngine, Deck, DeckState};
pub use catalog::{
    CanonicalMetadata, CanonicalPlaylist, CanonicalPlaylistEntry, CanonicalTrack, CatalogSource,
    CatalogSourceKind, ExternalPlaylist, ExternalPlaylistEntry, LibraryEntry, MetadataField,
    MetadataFieldValue, MetadataOrigin, MetadataProvenance, MetadataValue, SourceAccount,
    SourceAvailability, SourceCapabilities, SourceCollection, SourceCollectionRelationship,
    SourceCollectionRelationshipIdentity, SourceEntityKind, SourceTrackIdentity,
    SourceTrackOccurrence, TrackAnalysis,
};
pub use doctor::{
    Diagnosis, DiagnosisGroup, DiagnosisSummary, DiagnosticCheck, Doctor, DoctorError,
    DuplicateCheck, DuplicateGroup, DuplicateMatchType, DuplicateReport, MetadataCheck, Severity,
    WavExtensibleCheck,
};
pub use folder::{build_folder_tree, is_audio_extension, FolderNode};
pub use playlist::Playlist;
pub use provider::{
    BoxFuture, ByteRange, CatalogAssetReadRequest, CatalogAssetReadResult, CatalogAssetReader,
    CatalogSourceConnector, DownloadRequest, DownloadResult, HealthStatus, InventoryMode,
    InventoryPage, InventoryRequest, MusicAssetStore, MusicStorageError, MusicStorageErrorKind,
    RetryGuidance, SourceError, SourceErrorKind, SourceHealth, SourceRemoval, SourceSyncItem,
    SourceSyncMode, SourceSyncPage, SourceSyncRequest, StorageHealth, StorageObjectRemoval,
    StorageObjectVersion, StorageVersionPage, StorageVersionRequest, UploadRequest,
};
pub use reconciliation::{
    AssetStorageOutcome, AssetStorageStatus, ConflictingAssetSummaryOutcome, MatchCandidate,
    MatchConfidence, MatchEvidence, MatchMethod, PlaylistStorageOutcome, ReconciliationAction,
    ReconciliationDecision, ReconciliationOutcome, ReconciliationResolution, ReconciliationResult,
    ReconciliationSummary, ReconciliationSummaryError, ReconciliationSummaryScope,
    ReconciliationSuppression, TrackStorageOutcome, TrackStorageStatus,
};
pub use repository::{
    AssetLocationFilter, AssetRepository, CatalogSnapshot, CatalogSnapshotRepository,
    ExternalPlaylistEntryStagingScope, ExternalPlaylistFilter, InventoryGeneration,
    InventoryGenerationAbort, InventoryGenerationAbortKind, InventoryGenerationKind,
    InventoryGenerationStart, InventoryGenerationStatus, InventoryStagingPage,
    LibraryEntryRepository, MatchCandidateFilter, MusicStorageRepository, OriginalAssetFilter,
    PlaylistRepository, ReconciliationRepository, ReconciliationResolutionFilter,
    ReconciliationResultFilter, RepositoryCursor, RepositoryError, RepositoryErrorKind,
    RepositoryPage, RepositoryPageRequest, RepositoryResult, RepositoryTransaction, Setting,
    SettingsRepository, SourceCollectionRelationshipFilter, SourceOccurrenceFilter,
    SourceRepository, StorageObjectTombstone, SyncRunFilter, SyncRunRepository,
    TrackAnalysisRepository, TrackRepository, TransactionManager,
};
pub use schema::{
    CatalogSchemaVersion, CatalogSchemaVersionError, MigrationPlan, MigrationPlanError,
    MigrationReport, MigrationSafety, MigrationStatus, MigrationStep, MigrationStepReport,
    RequiredCatalogTable, CURRENT_CATALOG_SCHEMA_VERSION, LEGACY_CATALOG_SCHEMA_VERSION,
    REQUIRED_CATALOG_TABLES,
};
pub use search::SearchEngine;
pub use source::{LocalSourceConfig, MusicSource, SourceType};
pub use sync::{
    SyncCheckpoint, SyncCursor, SyncError, SyncErrorKind, SyncIssue, SyncIssueSeverity, SyncJob,
    SyncJobKind, SyncJobKindMismatch, SyncJobPayload, SyncProgress, SyncRun, SyncRunKind,
    SyncStatus, SyncTransitionError, TransferCheckpoint, TransferCheckpointError, TransferStage,
    DEFAULT_SYNC_JOB_MAX_ATTEMPTS, SYNC_JOB_PAYLOAD_VERSION,
};
pub use track::{BeatGridData, FrequencyWaveformData, MetadataError, Track, WaveformPeaks};
#[cfg(feature = "audio")]
pub use waveform::{extract_waveform, WaveformData, WaveformError};
