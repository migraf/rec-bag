//! Local catalog source and local primary integration slice.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use music_storage_local::{is_recognized_audio_path, LocalMusicStorage, LocalStorageError};
use rec_core::{
    AssetLocation, AssetLocationFilter, AssetRepository, AssetType, CanonicalPlaylist,
    CanonicalTrack, CatalogSource, CatalogSourceKind, ContentHash, EmbeddedTags, HashAlgorithm,
    HealthStatus, InventoryGeneration, InventoryGenerationAbort, InventoryGenerationAbortKind,
    InventoryGenerationKind, InventoryGenerationStart, InventoryMode, InventoryRequest,
    InventoryStagingPage, MetadataField, MetadataProvenance, MetadataValue, MusicAssetStore,
    MusicStorage, MusicStorageError, MusicStorageErrorKind, MusicStorageKind,
    MusicStorageRepository, OriginalAsset, OriginalAssetFilter, OriginalAssetRole,
    PlaylistRepository, ReconciliationRepository, ReconciliationResolution,
    ReconciliationResolutionFilter, ReconciliationSuppression, RepositoryError,
    RepositoryPageRequest, SourceAvailability, SourceCopyPolicy, SourceOccurrenceFilter,
    SourceRepository, SourceTrackOccurrence, StorageObject, StorageObjectTombstone, StorageRole,
    StorageVerificationStatus, SyncCheckpoint, SyncCursor, SyncError, SyncErrorKind, SyncRun,
    SyncRunKind, SyncRunRepository, TrackRepository, TrackStorageStatus, TransactionManager,
    UploadRequest,
};
use rec_persistence::TursoCatalog;
use rec_sync::{
    reconcile, InventoryAvailability, ReconciliationInput, ReconciliationReport, StorageInventory,
};
use sha2::{Digest, Sha256};
use ui::{MusicManagementEvent, MusicManagementView, PrimaryConfigurationView, ProviderChoice};
use uuid::Uuid;

use super::catalog_projection::{CatalogViewBuilder, StorageProviders};
use super::integration::{IntegrationFuture, IntegrationSliceHandler};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

const PAGE_SIZE: u32 = 500;
const CONFIG_PATH: &str = "path";
const CONFIG_ORGANIZE: &str = "organize_by_artist";
const CONFIG_VERIFY: &str = "verify_after_copy";
const META_RELATIVE_PATH: &str = "relative_path";
const META_SHA256: &str = "sha256";
const META_SIZE: &str = "size_bytes";

pub type SelectionFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<LocalSourceSelection>, LocalLibraryError>> + Send + 'a>,
>;

/// Native folder selection is injected because desktop dialogs are platform composition work.
pub trait LocalSelectionProvider: Send + Sync {
    fn select_source<'a>(&'a self) -> SelectionFuture<'a>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSourceSelection {
    pub name: String,
    pub root: PathBuf,
    pub copy_policy: SourceCopyPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyAuthorization {
    UserConfirmed,
    Automatic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceScanResult {
    pub source_id: Uuid,
    pub run_id: Uuid,
    pub discovered: u64,
    pub created_tracks: u64,
    pub reused_tracks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryResult {
    pub run_id: Uuid,
    pub generation: InventoryGeneration,
    pub object_count: u64,
    pub removal_count: u64,
    pub change_cursor: Option<SyncCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciliationProjection {
    pub run_id: Uuid,
    pub report: ReconciliationReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyResult {
    pub run_id: Uuid,
    pub track_id: Uuid,
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub object_id: String,
    pub in_place: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryMigrationResult {
    pub previous_primary_id: Option<Uuid>,
    pub active_primary_id: Uuid,
    pub inventory_generation_id: Uuid,
    pub reconciliation_run_id: Uuid,
    pub snapshot_id: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackOrigin {
    ActivePrimary,
    OtherLocalStorage,
    LocalSource,
    MigratedLegacyPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLocalPlayback {
    pub track_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
    pub object_id: Option<String>,
    pub path: PathBuf,
    pub origin: PlaybackOrigin,
    pub verified: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LocalLibraryError {
    #[error("native local-folder selection was cancelled")]
    SelectionCancelled,
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    #[error("local source was not found: {0}")]
    SourceNotFound(Uuid),
    #[error("storage was not found: {0}")]
    StorageNotFound(Uuid),
    #[error("track was not found: {0}")]
    TrackNotFound(Uuid),
    #[error("no original asset is registered for track: {0}")]
    AssetNotFound(Uuid),
    #[error("no active primary is configured")]
    NoActivePrimary,
    #[error("configuration is not a local folder")]
    NotLocal,
    #[error("local root is unavailable: {0}")]
    InvalidRoot(String),
    #[error("copy requires explicit confirmation")]
    CopyConfirmationRequired,
    #[error("source policy forbids copying")]
    CopyForbidden,
    #[error("primary migration cannot switch while assets remain unverified: {0}")]
    MigrationIncomplete(u64),
    #[error("catalog repository error: {0}")]
    Repository(#[from] RepositoryError),
    #[error("local storage configuration error: {0}")]
    LocalStorage(#[from] LocalStorageError),
    #[error("music storage operation failed: {0}")]
    MusicStorage(#[from] MusicStorageError),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("task failed: {0}")]
    Join(String),
    #[error("invalid integration request: {0}")]
    UnsupportedEvent(String),
}

impl LocalLibraryError {
    fn retryable(&self) -> bool {
        match self {
            Self::Repository(error) => error.is_retryable(),
            Self::MusicStorage(error) => error.is_retryable(),
            Self::Io(_) | Self::Join(_) => true,
            _ => false,
        }
    }
}

pub struct LocalLibraryService {
    catalog: Arc<TursoCatalog>,
    selection: Arc<dyn LocalSelectionProvider>,
}

impl LocalLibraryService {
    pub fn new(catalog: Arc<TursoCatalog>, selection: Arc<dyn LocalSelectionProvider>) -> Self {
        Self { catalog, selection }
    }

    pub fn catalog(&self) -> &Arc<TursoCatalog> {
        &self.catalog
    }

    pub async fn add_selected_source(&self) -> Result<SourceScanResult, LocalLibraryError> {
        tracing::info!("requesting local source folder selection");
        let selected = self
            .selection
            .select_source()
            .await?
            .ok_or(LocalLibraryError::SelectionCancelled)?;
        tracing::info!(source_name = %selected.name, root = %selected.root.display(), "local source folder selected");
        self.register_local_source(selected).await
    }

    pub async fn register_local_source(
        &self,
        selection: LocalSourceSelection,
    ) -> Result<SourceScanResult, LocalLibraryError> {
        let root = canonical_directory(&selection.root)?;
        tracing::info!(source_name = %selection.name, root = %root.display(), "registering local source");
        let existing = self.list_sources().await?.into_iter().find(|source| {
            source.kind == CatalogSourceKind::LocalFolder
                && source.config.get(CONFIG_PATH).is_some_and(|path| {
                    canonical_directory(Path::new(path)).ok().as_ref() == Some(&root)
                })
        });
        let mut source = existing.unwrap_or_else(|| {
            CatalogSource::local_folder(selection.name.clone(), root.to_string_lossy())
        });
        source.name = selection.name;
        source.copy_policy = selection.copy_policy;
        source
            .config
            .insert(CONFIG_PATH.into(), root.to_string_lossy().into_owned());
        SourceRepository::upsert(self.catalog.as_ref(), source.clone()).await?;
        tracing::debug!(source_id = %source.id, "local source registered; starting scan");
        self.scan_local_source(source.id).await
    }

    pub async fn scan_local_source(
        &self,
        source_id: Uuid,
    ) -> Result<SourceScanResult, LocalLibraryError> {
        let mut source = SourceRepository::get(self.catalog.as_ref(), source_id)
            .await?
            .ok_or(LocalLibraryError::SourceNotFound(source_id))?;
        if source.kind != CatalogSourceKind::LocalFolder {
            return Err(LocalLibraryError::NotLocal);
        }
        let root = source_root(&source)?;
        tracing::info!(source_id = %source_id, root = %root.display(), "local source scan started");
        let scanned = tokio::task::spawn_blocking(move || scan_files(&root))
            .await
            .map_err(|error| LocalLibraryError::Join(error.to_string()))??;
        tracing::info!(source_id = %source_id, discovered = scanned.len(), "local source filesystem scan completed");

        let existing_occurrences = self.list_occurrences(Some(source_id), None).await?;
        let existing_by_external: HashMap<_, _> = existing_occurrences
            .iter()
            .map(|value| (value.identity.external_id.clone(), value.clone()))
            .collect();
        let mut existing_by_hash = HashMap::new();
        for occurrence in &existing_occurrences {
            if let Some(hash) = metadata_text(&occurrence.provider_metadata, META_SHA256) {
                existing_by_hash
                    .entry(hash.to_owned())
                    .or_insert_with(|| occurrence.clone());
            }
        }
        let legacy_by_path = self.legacy_track_paths().await?;

        let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source_id), None);
        run.start().map_err(transition_error)?;
        SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;

        let transaction = self.catalog.begin().await?;
        let mut occurrences = Vec::with_capacity(scanned.len());
        let mut created = 0_u64;
        let mut reused = 0_u64;
        for file in scanned {
            let prior = existing_by_external
                .get(&file.external_id)
                .or_else(|| existing_by_hash.get(file.hash.value()));
            let legacy_track_id = legacy_by_path.get(&file.absolute_path).copied();
            let track_id = prior
                .and_then(|value| value.canonical_track_id)
                .or(legacy_track_id);
            let mut track = match track_id {
                Some(id) => transaction
                    .tracks()
                    .get(id)
                    .await?
                    .unwrap_or_else(CanonicalTrack::new),
                None => CanonicalTrack::new(),
            };
            if track_id.is_some() {
                reused += 1;
            } else {
                created += 1;
            }

            let mut occurrence = SourceTrackOccurrence::new(source_id, file.external_id.clone());
            occurrence.canonical_track_id = Some(track.id);
            occurrence.availability = SourceAvailability::Obtainable;
            occurrence.last_seen_at = Utc::now();
            occurrence.provider_metadata.insert(
                META_RELATIVE_PATH.into(),
                MetadataValue::Text(file.relative_path.clone()),
            );
            occurrence.provider_metadata.insert(
                META_SHA256.into(),
                MetadataValue::Text(file.hash.value().to_owned()),
            );
            occurrence.provider_metadata.insert(
                META_SIZE.into(),
                MetadataValue::Integer(file.size_bytes as i64),
            );
            track.link_occurrence(occurrence.identity.clone());

            // The asset is settled first: tags are observations *of a file*, so they
            // can only be recorded once it is known which asset was read.
            let mut originals = transaction
                .assets()
                .list_originals_for_track(track.id)
                .await?;
            let matching_original = originals
                .iter()
                .position(|asset| asset.content_hash.as_ref() == Some(&file.hash))
                .map(|index| originals.swap_remove(index));
            let mut original = matching_original
                .or_else(|| originals.into_iter().next())
                .unwrap_or_else(|| {
                    OriginalAsset::new_unverified(
                        track.id,
                        OriginalAssetRole::Master,
                        file.absolute_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("audio"),
                    )
                });
            original.file_name = file
                .absolute_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("audio")
                .to_owned();
            original.size_bytes = Some(file.size_bytes);
            original.content_hash = Some(file.hash.clone());

            // A file that moved is the same file under a new name; the title its old
            // name gave it must go with the old name, not outrank the real one.
            let superseded = prior
                .map(|value| value.identity.external_id.as_str())
                .filter(|external_id| *external_id != file.external_id);
            record_observed_metadata(
                &mut track,
                &file,
                source_id,
                original.id(),
                superseded,
                Utc::now(),
            );
            transaction.tracks().upsert(track.clone()).await?;
            transaction.assets().upsert_original(original).await?;
            occurrences.push(occurrence);
        }
        transaction
            .sources()
            .upsert_occurrences(occurrences)
            .await?;
        source.last_synced_at = Some(Utc::now());
        transaction.sources().upsert(source).await?;
        run.progress.discover(scanned_len_u64(created, reused));
        for _ in 0..scanned_len_u64(created, reused) {
            run.progress.record_success();
        }
        run.succeed().map_err(transition_error)?;
        transaction.sync_runs().upsert(run.clone()).await?;
        transaction.commit().await?;

        let result = SourceScanResult {
            source_id,
            run_id: run.id,
            discovered: created + reused,
            created_tracks: created,
            reused_tracks: reused,
        };
        tracing::info!(source_id = %result.source_id, run_id = %result.run_id, discovered = result.discovered, created_tracks = result.created_tracks, reused_tracks = result.reused_tracks, "local source scan persisted");
        Ok(result)
    }

    pub async fn configure_local_storage(
        &self,
        configuration: &PrimaryConfigurationView,
    ) -> Result<MusicStorage, LocalLibraryError> {
        if configuration.provider != ProviderChoice::LocalFolder {
            return Err(LocalLibraryError::NotLocal);
        }
        tracing::info!(storage_name = %configuration.name, location = %configuration.location, "configuring local primary storage");
        let root = ensure_directory(Path::new(&configuration.location))?;
        let id = Uuid::new_v4();
        let adapter = LocalMusicStorage::new(id, &root)?;
        let health = adapter.health().await?;
        if health.status == HealthStatus::Unavailable
            || !health.capabilities.supports_role(StorageRole::Primary)
        {
            return Err(LocalLibraryError::InvalidRoot(
                health.message.unwrap_or_else(|| root.display().to_string()),
            ));
        }
        let mut storage = MusicStorage::new(
            configuration.name.clone(),
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            health.capabilities,
        )
        .with_root_identifier(root.to_string_lossy());
        storage.id = id;
        storage.config.insert(
            CONFIG_ORGANIZE.into(),
            configuration.organize_by_artist.to_string(),
        );
        storage.config.insert(
            CONFIG_VERIFY.into(),
            configuration.verify_after_copy.to_string(),
        );
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage.clone()).await?;
        tracing::info!(storage_id = %storage.id, root = %root.display(), "local primary storage configuration persisted");
        Ok(storage)
    }

    pub async fn add_selected_backup(&self) -> Result<InventoryResult, LocalLibraryError> {
        tracing::info!("requesting local backup folder selection");
        let selected = self
            .selection
            .select_source()
            .await?
            .ok_or(LocalLibraryError::SelectionCancelled)?;
        tracing::info!(backup_name = %selected.name, root = %selected.root.display(), "local backup folder selected");
        let root = ensure_directory(&selected.root)?;
        self.ensure_separate_local_root(&root).await?;
        let id = Uuid::new_v4();
        let adapter = LocalMusicStorage::new(id, &root)?;
        let health = adapter.health().await?;
        if health.status == HealthStatus::Unavailable
            || !health.capabilities.supports_role(StorageRole::Backup)
        {
            return Err(LocalLibraryError::InvalidRoot(
                health.message.unwrap_or_else(|| root.display().to_string()),
            ));
        }
        let mut storage = MusicStorage::new(
            selected.name,
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            health.capabilities,
        )
        .with_root_identifier(root.to_string_lossy());
        storage.id = id;
        storage.enabled = true;
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage).await?;
        self.inventory_storage(id, InventoryMode::Full, true).await
    }

    async fn ensure_separate_local_root(&self, candidate: &Path) -> Result<(), LocalLibraryError> {
        for source in self.list_sources().await? {
            if source.kind == CatalogSourceKind::LocalFolder {
                if let Ok(root) = source_root(&source) {
                    LocalMusicStorage::ensure_separate_roots(root, candidate)?;
                }
            }
        }
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.kind == MusicStorageKind::LocalDirectory {
                if let Some(root) = storage.root_identifier {
                    LocalMusicStorage::ensure_separate_roots(root, candidate)?;
                }
            }
        }
        Ok(())
    }

    pub async fn update_primary_metadata(
        &self,
        storage_id: Uuid,
        name: String,
        organize_by_artist: bool,
        verify_after_copy: bool,
    ) -> Result<(), LocalLibraryError> {
        let mut storage = MusicStorageRepository::get(self.catalog.as_ref(), storage_id)
            .await?
            .ok_or(LocalLibraryError::StorageNotFound(storage_id))?;
        storage.name = name;
        storage.updated_at = Utc::now();
        storage
            .config
            .insert(CONFIG_ORGANIZE.into(), organize_by_artist.to_string());
        storage
            .config
            .insert(CONFIG_VERIFY.into(), verify_after_copy.to_string());
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage).await?;
        Ok(())
    }

    pub async fn inventory_storage(
        &self,
        storage_id: Uuid,
        mode: InventoryMode,
        include_checksums: bool,
    ) -> Result<InventoryResult, LocalLibraryError> {
        let storage = self.local_storage_record(storage_id).await?;
        let adapter = local_adapter(&storage)?;
        tracing::info!(storage_id = %storage_id, mode = ?mode, include_checksums, "local storage inventory started");
        let mut run = SyncRun::new(SyncRunKind::StorageInventory, None, Some(storage_id));
        run.start().map_err(transition_error)?;
        SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;
        let generation_id = Uuid::new_v4();
        let kind = match mode {
            InventoryMode::Full => InventoryGenerationKind::Full,
            InventoryMode::Incremental { .. } => InventoryGenerationKind::Incremental,
        };
        self.catalog
            .begin_inventory_generation(InventoryGenerationStart {
                id: generation_id,
                storage_id,
                run_id: run.id,
                kind,
                started_at: Utc::now(),
            })
            .await?;

        let mut page_cursor = None;
        let mut change_cursor = None;
        let mut objects = 0_u64;
        let mut removals = 0_u64;
        loop {
            let page = match adapter
                .inventory_page(InventoryRequest {
                    mode: mode.clone(),
                    page_cursor: page_cursor.clone(),
                    page_size: Some(PAGE_SIZE),
                    prefix: None,
                    include_checksums,
                })
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    let abort_kind = if matches!(
                        error.kind,
                        MusicStorageErrorKind::TemporarilyUnavailable
                            | MusicStorageErrorKind::NotFound
                    ) {
                        InventoryGenerationAbortKind::Unavailable
                    } else {
                        InventoryGenerationAbortKind::Failed
                    };
                    let _ = self
                        .catalog
                        .abort_inventory_generation(
                            generation_id,
                            InventoryGenerationAbort {
                                kind: abort_kind,
                                finished_at: Utc::now(),
                                message: Some(error.to_string()),
                            },
                        )
                        .await;
                    let _ = fail_run(&self.catalog, &mut run, &error.to_string()).await;
                    return Err(error.into());
                }
            };
            objects += page.objects.len() as u64;
            removals += page.removals.len() as u64;
            let tombstones = page
                .removals
                .into_iter()
                .map(|removal| StorageObjectTombstone {
                    storage_id,
                    object_id: removal.object_id,
                    generation_id,
                    removed_at: removal.removed_at,
                    provider_version: removal.provider_version,
                })
                .collect();
            self.catalog
                .stage_inventory_page(
                    generation_id,
                    InventoryStagingPage {
                        objects: page.objects,
                        tombstones,
                    },
                )
                .await?;
            run.progress
                .discover(objects + removals - run.progress.discovered_items);
            page_cursor = page.next_page_cursor;
            if let Some(cursor) = page.next_change_cursor {
                change_cursor = Some(cursor);
            }
            if page_cursor.is_none() {
                break;
            }
            let checkpoint = SyncCheckpoint::new(run.progress.processed_items)
                .with_page_cursor(page_cursor.clone().expect("page cursor checked"));
            run.update_checkpoint(checkpoint);
            SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;
        }

        for _ in 0..objects + removals {
            run.progress.record_success();
        }
        if let Some(cursor) = change_cursor.clone() {
            run.update_checkpoint(
                SyncCheckpoint::new(run.progress.processed_items).with_completed_sequence(cursor),
            );
        }
        run.succeed().map_err(transition_error)?;
        let transaction = self.catalog.begin().await?;
        let generation = transaction
            .assets()
            .finalize_inventory_generation(generation_id, Utc::now())
            .await?;
        transaction.sync_runs().upsert(run.clone()).await?;
        transaction.commit().await?;
        let result = InventoryResult {
            run_id: run.id,
            generation,
            object_count: objects,
            removal_count: removals,
            change_cursor,
        };
        tracing::info!(storage_id = %storage_id, run_id = %result.run_id, objects = result.object_count, removals = result.removal_count, "local storage inventory completed");
        Ok(result)
    }

    pub async fn adopt_migrated_library(
        &self,
        source_id: Uuid,
        configuration: &PrimaryConfigurationView,
    ) -> Result<PrimaryMigrationResult, LocalLibraryError> {
        self.scan_local_source(source_id).await?;
        self.migrate_primary(configuration, true).await
    }

    pub async fn reconcile_primary(&self) -> Result<ReconciliationProjection, LocalLibraryError> {
        let active = self.catalog.get_active_primary().await?;
        self.reconcile_for(active.map(|storage| storage.id)).await
    }

    async fn reconcile_for(
        &self,
        primary_storage_id: Option<Uuid>,
    ) -> Result<ReconciliationProjection, LocalLibraryError> {
        let input = self.reconciliation_input(primary_storage_id).await?;
        let report = reconcile(&input);
        let mut run = SyncRun::new(SyncRunKind::Reconciliation, None, primary_storage_id);
        run.start().map_err(transition_error)?;
        run.progress
            .discover((report.results.len() + report.candidates.len()) as u64);
        let transaction = self.catalog.begin().await?;
        for candidate in report.candidates.iter().cloned() {
            transaction
                .reconciliation()
                .upsert_candidate(candidate)
                .await?;
            run.progress.record_success();
        }
        for result in report.results.iter().cloned() {
            transaction.reconciliation().upsert_result(result).await?;
            run.progress.record_success();
        }
        let originals: HashMap<_, _> = input
            .originals
            .iter()
            .map(|asset| (asset.id(), asset))
            .collect();
        let objects: HashMap<_, _> = input
            .inventories
            .iter()
            .flat_map(|inventory| inventory.objects.iter())
            .map(|object| ((object.storage_id, object.object_id.clone()), object))
            .collect();
        for matched in &report.verified_matches {
            let Some(asset) = originals.get(&matched.asset_id) else {
                continue;
            };
            let Some(object) =
                objects.get(&(matched.storage_id, matched.storage_object_id.clone()))
            else {
                continue;
            };
            let existing = transaction
                .assets()
                .get_location(matched.storage_id, matched.storage_object_id.clone())
                .await?;
            let now = Utc::now();
            transaction
                .assets()
                .upsert_location(AssetLocation {
                    id: existing
                        .as_ref()
                        .map(|value| value.id)
                        .unwrap_or_else(Uuid::new_v4),
                    asset_id: matched.asset_id,
                    asset_type: AssetType::Original,
                    storage_id: matched.storage_id,
                    object_id: matched.storage_object_id.clone(),
                    verification_status: StorageVerificationStatus::Verified,
                    expected_hash: asset.content_hash.clone(),
                    observed_hash: object.canonical_hash().cloned(),
                    provider_version: object.provider_version.clone(),
                    last_checked_at: Some(now),
                    verified_at: Some(now),
                    error_message: None,
                })
                .await?;
        }
        run.succeed().map_err(transition_error)?;
        transaction.sync_runs().upsert(run.clone()).await?;
        transaction.commit().await?;
        Ok(ReconciliationProjection {
            run_id: run.id,
            report,
        })
    }

    pub async fn copy_track_to_primary(
        &self,
        track_id: Uuid,
        authorization: CopyAuthorization,
    ) -> Result<CopyResult, LocalLibraryError> {
        let primary = self
            .catalog
            .get_active_primary()
            .await?
            .ok_or(LocalLibraryError::NoActivePrimary)?;
        self.copy_track_to_storage(track_id, primary.id, authorization)
            .await
    }

    async fn copy_track_to_storage(
        &self,
        track_id: Uuid,
        destination_storage_id: Uuid,
        authorization: CopyAuthorization,
    ) -> Result<CopyResult, LocalLibraryError> {
        let track = TrackRepository::get(self.catalog.as_ref(), track_id)
            .await?
            .ok_or(LocalLibraryError::TrackNotFound(track_id))?;
        let mut originals = self.catalog.list_originals_for_track(track_id).await?;
        let preferred = originals
            .iter()
            .position(|asset| asset.is_preferred)
            .map(|index| originals.swap_remove(index));
        let mut asset = preferred
            .or_else(|| originals.into_iter().next())
            .ok_or(LocalLibraryError::AssetNotFound(track_id))?;
        let occurrence = self
            .list_occurrences(None, Some(track_id))
            .await?
            .into_iter()
            .find(|value| value.availability == SourceAvailability::Obtainable)
            .ok_or(LocalLibraryError::AssetNotFound(track_id))?;
        let source = SourceRepository::get(self.catalog.as_ref(), occurrence.identity.source_id)
            .await?
            .ok_or(LocalLibraryError::SourceNotFound(
                occurrence.identity.source_id,
            ))?;
        enforce_copy_policy(source.copy_policy, authorization)?;
        let source_root = source_root(&source)?;
        let relative = metadata_text(&occurrence.provider_metadata, META_RELATIVE_PATH)
            .ok_or_else(|| {
                LocalLibraryError::InvalidRoot("source occurrence has no path".into())
            })?;
        let source_path = source_root.join(relative);
        let source_path = fs::canonicalize(source_path)?;
        let destination = self.local_storage_record(destination_storage_id).await?;
        let adapter = local_adapter(&destination)?;
        let source_hash = hash_file(&source_path)?;
        let size = fs::metadata(&source_path)?.len();
        asset.content_hash = Some(source_hash.clone());
        asset.size_bytes = Some(size);
        let in_place = source_path.starts_with(adapter.root());
        let object = if in_place {
            let relative = source_path
                .strip_prefix(adapter.root())
                .map_err(|_| LocalLibraryError::InvalidRoot(source_path.display().to_string()))?;
            adapter.stat(&path_to_object_id(relative)?).await?
        } else {
            LocalMusicStorage::ensure_separate_roots(&source_root, adapter.root())?;
            let artist = preferred_text(&track, MetadataField::Artist);
            let title = preferred_text(&track, MetadataField::Title);
            let object_id = adapter.available_managed_object_path(
                artist.as_deref(),
                title.as_deref(),
                track_id,
                &source_path,
            )?;
            adapter
                .upload(UploadRequest {
                    source_path: source_path.clone(),
                    object_path: object_id,
                    content_type: None,
                    content_length: Some(size),
                    expected_checksum: Some(source_hash.clone()),
                    metadata: BTreeMap::from([("asset_id".into(), asset.id().to_string())]),
                    idempotency_key: Some(format!(
                        "{}:{}:{}",
                        asset.id(),
                        asset.version,
                        destination_storage_id
                    )),
                })
                .await?
        };
        if object.canonical_hash() != Some(&source_hash) {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::ChecksumMismatch,
                "destination checksum differs from source",
            )
            .into());
        }
        let mut run = SyncRun::new(
            SyncRunKind::AssetTransfer,
            Some(source.id),
            Some(destination_storage_id),
        );
        run.start().map_err(transition_error)?;
        run.progress.discover(1);
        run.progress.add_bytes(size);
        run.progress.record_success();
        run.succeed().map_err(transition_error)?;
        let transaction = self.catalog.begin().await?;
        transaction.assets().upsert_original(asset.clone()).await?;
        transaction
            .assets()
            .upsert_storage_object(object.clone())
            .await?;
        let existing = transaction
            .assets()
            .get_location(destination_storage_id, object.object_id.clone())
            .await?;
        let now = Utc::now();
        transaction
            .assets()
            .upsert_location(AssetLocation {
                id: existing.map(|value| value.id).unwrap_or_else(Uuid::new_v4),
                asset_id: asset.id(),
                asset_type: AssetType::Original,
                storage_id: destination_storage_id,
                object_id: object.object_id.clone(),
                verification_status: StorageVerificationStatus::Verified,
                expected_hash: Some(source_hash.clone()),
                observed_hash: object.canonical_hash().cloned(),
                provider_version: object.provider_version.clone(),
                last_checked_at: Some(now),
                verified_at: Some(now),
                error_message: None,
            })
            .await?;
        transaction.sync_runs().upsert(run.clone()).await?;
        transaction.commit().await?;
        Ok(CopyResult {
            run_id: run.id,
            track_id,
            asset_id: asset.id(),
            storage_id: destination_storage_id,
            object_id: object.object_id,
            in_place,
        })
    }

    pub async fn migrate_primary(
        &self,
        configuration: &PrimaryConfigurationView,
        keep_old_as_backup: bool,
    ) -> Result<PrimaryMigrationResult, LocalLibraryError> {
        let candidate = self.configure_local_storage(configuration).await?;
        let inventory = self
            .inventory_storage(candidate.id, InventoryMode::Full, true)
            .await?;
        let mut reconciliation = self.reconcile_for(Some(candidate.id)).await?;
        let missing: Vec<_> = reconciliation
            .report
            .track_outcomes
            .iter()
            .filter(|outcome| outcome.status == TrackStorageStatus::MissingObtainable)
            .map(|outcome| outcome.track_id)
            .collect();
        for track_id in missing {
            self.copy_track_to_storage(track_id, candidate.id, CopyAuthorization::Automatic)
                .await?;
        }
        reconciliation = self.reconcile_for(Some(candidate.id)).await?;
        let unverified = reconciliation
            .report
            .track_outcomes
            .iter()
            .filter(|outcome| {
                outcome.status.is_archivable()
                    && outcome.status != TrackStorageStatus::StoredAndVerified
            })
            .count() as u64;
        if unverified != 0 {
            return Err(LocalLibraryError::MigrationIncomplete(unverified));
        }
        // PreparePrimaryMigration is the final user-confirmed action. The recovery
        // snapshot must be durable before the active-primary compare-and-swap begins.
        let snapshot = self.catalog.export_snapshot().await?;
        let previous = self.catalog.get_active_primary().await?;
        let transaction = self.catalog.begin().await?;
        let selected = transaction
            .music_storages()
            .switch_active_primary(previous.as_ref().map(|value| value.id), Some(candidate.id))
            .await?
            .ok_or(LocalLibraryError::StorageNotFound(candidate.id))?;
        if keep_old_as_backup {
            if let Some(mut old) = previous.clone() {
                if old.id != selected.id {
                    old.role = StorageRole::Backup;
                    old.updated_at = Utc::now();
                    transaction.music_storages().upsert(old).await?;
                }
            }
        }
        transaction.commit().await?;
        Ok(PrimaryMigrationResult {
            previous_primary_id: previous.map(|value| value.id),
            active_primary_id: selected.id,
            inventory_generation_id: inventory.generation.id,
            reconciliation_run_id: reconciliation.run_id,
            snapshot_id: snapshot.id,
        })
    }

    pub async fn resolve_playback(
        &self,
        track_id: Uuid,
    ) -> Result<ResolvedLocalPlayback, LocalLibraryError> {
        if TrackRepository::get(self.catalog.as_ref(), track_id)
            .await?
            .is_none()
        {
            return Err(LocalLibraryError::TrackNotFound(track_id));
        }
        let active = self.catalog.get_active_primary().await?;
        let originals = self.catalog.list_originals_for_track(track_id).await?;
        for asset in &originals {
            let mut locations = self.catalog.list_locations_for_asset(asset.id()).await?;
            locations.sort_by_key(|location| {
                let active_rank =
                    u8::from(active.as_ref().is_none_or(|s| s.id != location.storage_id));
                let verified_rank =
                    u8::from(location.verification_status != StorageVerificationStatus::Verified);
                (active_rank, verified_rank)
            });
            for location in locations {
                let Some(storage) =
                    MusicStorageRepository::get(self.catalog.as_ref(), location.storage_id).await?
                else {
                    continue;
                };
                if storage.kind != MusicStorageKind::LocalDirectory {
                    continue;
                }
                if let Ok(path) = local_adapter(&storage)?.playback_path(&location.object_id) {
                    return Ok(ResolvedLocalPlayback {
                        track_id,
                        asset_id: Some(asset.id()),
                        storage_id: Some(storage.id),
                        object_id: Some(location.object_id),
                        path,
                        origin: if active.as_ref().is_some_and(|value| value.id == storage.id) {
                            PlaybackOrigin::ActivePrimary
                        } else {
                            PlaybackOrigin::OtherLocalStorage
                        },
                        verified: location.verification_status
                            == StorageVerificationStatus::Verified,
                    });
                }
                let legacy = PathBuf::from(&location.object_id);
                if legacy.is_absolute() && legacy.is_file() {
                    return Ok(ResolvedLocalPlayback {
                        track_id,
                        asset_id: Some(asset.id()),
                        storage_id: Some(storage.id),
                        object_id: Some(location.object_id),
                        path: legacy,
                        origin: PlaybackOrigin::MigratedLegacyPath,
                        verified: false,
                    });
                }
            }
        }
        for occurrence in self.list_occurrences(None, Some(track_id)).await? {
            let Some(relative) = metadata_text(&occurrence.provider_metadata, META_RELATIVE_PATH)
            else {
                continue;
            };
            let Some(source) =
                SourceRepository::get(self.catalog.as_ref(), occurrence.identity.source_id).await?
            else {
                continue;
            };
            let path = source_root(&source)?.join(relative);
            if path.is_file() {
                return Ok(ResolvedLocalPlayback {
                    track_id,
                    asset_id: originals.first().map(OriginalAsset::id),
                    storage_id: None,
                    object_id: None,
                    path,
                    origin: PlaybackOrigin::LocalSource,
                    verified: false,
                });
            }
        }
        Err(LocalLibraryError::AssetNotFound(track_id))
    }

    /// Reconcile the primary storage and project the whole catalog into the dashboard view.
    ///
    /// The projection covers every Catalog Source and Music Storage regardless of
    /// provider; only storage health is limited to what this slice can probe.
    pub async fn project_view(&self) -> Result<MusicManagementView, LocalLibraryError> {
        let projection = self.reconcile_primary().await?;
        let providers = LocalStorageProviders;
        Ok(CatalogViewBuilder {
            catalog: self.catalog.as_ref(),
            report: &projection.report,
            providers: Some(&providers),
        }
        .build()
        .await?)
    }

    async fn reconciliation_input(
        &self,
        primary_storage_id: Option<Uuid>,
    ) -> Result<ReconciliationInput, LocalLibraryError> {
        let sources = self.list_sources().await?;
        let tracks = self.list_tracks().await?;
        let occurrences = self.list_occurrences(None, None).await?;
        let originals = self.list_originals().await?;
        let locations = self.list_locations().await?;
        let storages = MusicStorageRepository::list(self.catalog.as_ref()).await?;
        let playlists = self.list_playlists().await?;
        let inventories = if let Some(storage_id) = primary_storage_id {
            let storage = storages.iter().find(|value| value.id == storage_id);
            let availability = match storage {
                Some(storage) if storage.enabled => match local_adapter(storage) {
                    Ok(adapter) => match adapter.health().await {
                        Ok(health) if health.status != HealthStatus::Unavailable => {
                            InventoryAvailability::Available
                        }
                        Ok(_) => InventoryAvailability::Unavailable,
                        Err(_) => InventoryAvailability::Error,
                    },
                    Err(_) => InventoryAvailability::Unavailable,
                },
                _ => InventoryAvailability::Unavailable,
            };
            let objects = if availability == InventoryAvailability::Available {
                self.list_storage_objects(storage_id).await?
            } else {
                Vec::new()
            };
            vec![StorageInventory {
                storage_id,
                availability,
                objects,
            }]
        } else {
            Vec::new()
        };
        let resolutions = self.list_reconciliation_resolutions().await?;
        let suppressions = self.list_reconciliation_suppressions().await?;
        Ok(ReconciliationInput {
            primary_storage_id,
            sources,
            tracks,
            occurrences,
            originals,
            locations,
            storages,
            inventories,
            playlists,
            external_evidence: Vec::new(),
            resolutions,
            suppressions,
        })
    }

    async fn legacy_track_paths(&self) -> Result<HashMap<PathBuf, Uuid>, LocalLibraryError> {
        let originals = self.list_originals().await?;
        let track_by_asset: HashMap<_, _> = originals
            .iter()
            .map(|asset| (asset.id(), asset.track_id()))
            .collect();
        let mut paths = HashMap::new();
        for location in self.list_locations().await? {
            let path = PathBuf::from(&location.object_id);
            if path.is_absolute() {
                if let Some(track_id) = track_by_asset.get(&location.asset_id) {
                    paths.insert(path, *track_id);
                }
            }
        }
        Ok(paths)
    }

    async fn local_storage_record(&self, id: Uuid) -> Result<MusicStorage, LocalLibraryError> {
        let storage = MusicStorageRepository::get(self.catalog.as_ref(), id)
            .await?
            .ok_or(LocalLibraryError::StorageNotFound(id))?;
        if storage.kind != MusicStorageKind::LocalDirectory {
            return Err(LocalLibraryError::NotLocal);
        }
        Ok(storage)
    }
    async fn list_sources(&self) -> Result<Vec<CatalogSource>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = SourceRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(PAGE_SIZE),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_tracks(&self) -> Result<Vec<CanonicalTrack>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = TrackRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(PAGE_SIZE),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_playlists(&self) -> Result<Vec<CanonicalPlaylist>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = PlaylistRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(PAGE_SIZE),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_occurrences(
        &self,
        source_id: Option<Uuid>,
        track_id: Option<Uuid>,
    ) -> Result<Vec<SourceTrackOccurrence>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_occurrences(
                    SourceOccurrenceFilter {
                        source_id,
                        canonical_track_id: track_id,
                        account_external_id: None,
                    },
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_originals(&self) -> Result<Vec<OriginalAsset>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_originals(
                    OriginalAssetFilter::default(),
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_locations(&self) -> Result<Vec<AssetLocation>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_locations(
                    AssetLocationFilter::default(),
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_storage_objects(&self, id: Uuid) -> Result<Vec<StorageObject>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_storage_objects(
                    id,
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_reconciliation_resolutions(
        &self,
    ) -> Result<Vec<ReconciliationResolution>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_resolutions(
                    ReconciliationResolutionFilter::default(),
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_reconciliation_suppressions(
        &self,
    ) -> Result<Vec<ReconciliationSuppression>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .catalog
                .list_suppressions(
                    ReconciliationResolutionFilter::default(),
                    RepositoryPageRequest {
                        cursor,
                        limit: Some(PAGE_SIZE),
                    },
                )
                .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }
}

impl IntegrationSliceHandler for LocalLibraryService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::LocalLibrary
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            if request.slice != IntegrationSlice::LocalLibrary {
                return Err(IntegrationError::for_request(
                    &request,
                    "request was routed to the wrong integration slice",
                    false,
                ));
            }
            let operation: Result<String, LocalLibraryError> = async {
                match request.event.clone() {
                    MusicManagementEvent::AddSource(ProviderChoice::LocalFolder) => self
                        .add_selected_source()
                        .await
                        .map(|result| format!("Scanned {} local audio files", result.discovered)),
                    MusicManagementEvent::AddLocalFolderSource { path } => self
                        .register_local_source(local_source_at(&path))
                        .await
                        .map(|result| format!("Scanned {} local audio files", result.discovered)),
                    MusicManagementEvent::SyncSource(id) => {
                        let id = parse_uuid(&id)?;
                        self.scan_local_source(id).await.map(|result| {
                            format!("Scanned {} local audio files", result.discovered)
                        })
                    }
                    MusicManagementEvent::CreatePrimary(configuration) => {
                        if let Some(active) = self.catalog.get_active_primary().await? {
                            let requested_root =
                                canonical_directory(Path::new(&configuration.location))?;
                            let matches_active_primary = active.kind
                                == MusicStorageKind::LocalDirectory
                                && active.root_identifier.as_deref().is_some_and(|root| {
                                    canonical_directory(Path::new(root)).ok().as_ref()
                                        == Some(&requested_root)
                                });
                            if matches_active_primary {
                                Ok(format!("Primary {} is already configured", active.id))
                            } else {
                                Err(LocalLibraryError::UnsupportedEvent(
                                    "an active primary already exists; use primary migration"
                                        .into(),
                                ))
                            }
                        } else {
                            self.migrate_primary(&configuration, false)
                                .await
                                .map(|result| {
                                    format!("Configured primary {}", result.active_primary_id)
                                })
                        }
                    }
                    MusicManagementEvent::UpdatePrimaryMetadata(update) => {
                        let id = parse_uuid(&update.storage_id)?;
                        self.update_primary_metadata(
                            id,
                            update.name,
                            update.organize_by_artist,
                            update.verify_after_copy,
                        )
                        .await
                        .map(|_| "Updated primary metadata".into())
                    }
                    MusicManagementEvent::CopyToPrimary(track_id) => {
                        let track_id = parse_uuid(&track_id)?;
                        let copied = self
                            .copy_track_to_primary(track_id, CopyAuthorization::UserConfirmed)
                            .await?;
                        self.reconcile_primary().await?;
                        Ok(format!("Copied {} to the active primary", copied.object_id))
                    }
                    MusicManagementEvent::RetryStorage(storage_id) => {
                        let storage_id = parse_uuid(&storage_id)?;
                        self.local_storage_record(storage_id).await?;
                        let inventory = self
                            .inventory_storage(storage_id, InventoryMode::Full, true)
                            .await?;
                        self.reconcile_primary().await?;
                        Ok(format!(
                            "Inventoried {} local storage objects",
                            inventory.object_count
                        ))
                    }
                    MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder) => {
                        self.add_selected_backup().await.map(|inventory| {
                            format!(
                                "Configured local backup with {} objects",
                                inventory.object_count
                            )
                        })
                    }
                    MusicManagementEvent::PreparePrimaryMigration(plan) => self
                        .migrate_primary(&plan.destination, plan.keep_old_as_backup)
                        .await
                        .map(|result| {
                            format!(
                                "Migrated primary to {} after snapshot {}",
                                result.active_primary_id, result.snapshot_id
                            )
                        }),
                    other => Err(LocalLibraryError::UnsupportedEvent(format!("{other:?}"))),
                }
            }
            .await;
            match operation {
                Ok(message) => self
                    .project_view()
                    .await
                    .map(|view| IntegrationOutcome {
                        view: Some(view),
                        message: Some(message),
                    })
                    .map_err(|error| {
                        IntegrationError::for_request(
                            &request,
                            error.to_string(),
                            error.retryable(),
                        )
                    }),
                Err(error) => Err(IntegrationError::for_request(
                    &request,
                    error.to_string(),
                    error.retryable(),
                )),
            }
        })
    }
}

#[derive(Debug)]
struct ScannedFile {
    absolute_path: PathBuf,
    relative_path: String,
    external_id: String,
    size_bytes: u64,
    hash: ContentHash,
    /// What the file's own tags say about it, read while walking the folder.
    tags: EmbeddedTags,
}

fn scan_files(root: &Path) -> Result<Vec<ScannedFile>, LocalLibraryError> {
    let mut files = Vec::new();
    scan_directory(root, root, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<ScannedFile>,
) -> Result<(), LocalLibraryError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if entry.file_name() != ".rec_bag_history" {
                scan_directory(root, &path, files)?;
            }
            continue;
        }
        if !metadata.is_file() || !is_recognized_audio_path(&path) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| LocalLibraryError::InvalidRoot(path.display().to_string()))?;
        let relative_path = path_to_object_id(relative)?;
        let hash = hash_file(&path)?;
        // A file that will not give up its tags is still a file worth cataloguing.
        let tags = rec_core::read_embedded_tags(&path).unwrap_or_else(|error| {
            tracing::debug!(path = %path.display(), %error, "could not read embedded tags");
            EmbeddedTags::default()
        });
        files.push(ScannedFile {
            absolute_path: path,
            external_id: filesystem_identity(&metadata)
                .unwrap_or_else(|| format!("local-v1:path:{relative_path}")),
            relative_path,
            size_bytes: metadata.len(),
            hash,
            tags,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!(
        "local-v1:file:{}:{}",
        metadata.dev(),
        metadata.ino()
    ))
}

#[cfg(not(unix))]
fn filesystem_identity(_metadata: &fs::Metadata) -> Option<String> {
    None
}

fn hash_file(path: &Path) -> Result<ContentHash, LocalLibraryError> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(bytes);
    ContentHash::new(HashAlgorithm::Sha256, format!("{digest:x}"))
        .map_err(|error| LocalLibraryError::InvalidRoot(error.to_string()))
}

/// A source configuration for a folder the user named directly. The folder's own name
/// is the source's name, which is what the native picker would also produce.
fn local_source_at(path: &str) -> LocalSourceSelection {
    let root = PathBuf::from(path);
    LocalSourceSelection {
        name: root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_owned()),
        root,
        copy_policy: SourceCopyPolicy::default(),
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, LocalLibraryError> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(LocalLibraryError::InvalidRoot(path.display().to_string()));
    }
    Ok(path)
}

fn ensure_directory(path: &Path) -> Result<PathBuf, LocalLibraryError> {
    fs::create_dir_all(path)?;
    canonical_directory(path)
}

fn source_root(source: &CatalogSource) -> Result<PathBuf, LocalLibraryError> {
    if source.kind != CatalogSourceKind::LocalFolder {
        return Err(LocalLibraryError::NotLocal);
    }
    source
        .config
        .get(CONFIG_PATH)
        .map(PathBuf::from)
        .ok_or_else(|| LocalLibraryError::InvalidRoot("local source has no path".into()))
}

/// Supplies stores for locally-backed Music Storages so the projection can probe
/// their health. Storages of any other kind are left to the caller's fallback.
struct LocalStorageProviders;

impl StorageProviders for LocalStorageProviders {
    fn store(&self, storage: &MusicStorage) -> Option<Arc<dyn MusicAssetStore>> {
        local_adapter(storage)
            .ok()
            .map(|adapter| Arc::new(adapter) as Arc<dyn MusicAssetStore>)
    }
}

fn local_adapter(storage: &MusicStorage) -> Result<LocalMusicStorage, LocalLibraryError> {
    if storage.kind != MusicStorageKind::LocalDirectory {
        return Err(LocalLibraryError::NotLocal);
    }
    let root = storage
        .root_identifier
        .as_ref()
        .ok_or_else(|| LocalLibraryError::InvalidRoot("local storage has no root".into()))?;
    Ok(LocalMusicStorage::new(storage.id, root)?)
}

fn path_to_object_id(path: &Path) -> Result<String, LocalLibraryError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_string_lossy()),
            _ => return Err(LocalLibraryError::InvalidRoot(path.display().to_string())),
        }
    }
    if components.is_empty() {
        return Err(LocalLibraryError::InvalidRoot(path.display().to_string()));
    }
    Ok(components.join("/"))
}

fn metadata_text<'a>(metadata: &'a BTreeMap<String, MetadataValue>, key: &str) -> Option<&'a str> {
    match metadata.get(key) {
        Some(MetadataValue::Text(value)) => Some(value),
        _ => None,
    }
}

/// Record what one scanned file says about its canonical track.
///
/// Every field the file carries is recorded, because a track whose length the catalog
/// does not know cannot be seeked and a track whose artist it does not know cannot be
/// found. Tags describe one asset, so they are recorded against that asset and restated
/// on each scan: rescanning a folder updates what the file says instead of accumulating
/// another copy of it, and a tag that has since been removed is withdrawn.
///
/// The filename is only a fallback title. It is recorded as the source's own
/// observation when the file carries no title of its own, and withdrawn as soon as it
/// does, so a real title is never shadowed by a path — including the name the file went
/// by before it moved.
fn record_observed_metadata(
    track: &mut CanonicalTrack,
    file: &ScannedFile,
    source_id: Uuid,
    asset_id: Uuid,
    superseded_external_id: Option<&str>,
    observed_at: DateTime<Utc>,
) {
    let tags = &file.tags;
    let embedded = MetadataProvenance::embedded_tag(asset_id, None, observed_at);
    let text = |value: &Option<String>| value.clone().map(MetadataValue::Text);
    let count = |value: Option<u32>| value.map(|value| MetadataValue::Integer(value as i64));

    for (field, value) in [
        (MetadataField::Title, text(&tags.title)),
        (MetadataField::Artist, text(&tags.artist)),
        (MetadataField::Album, text(&tags.album)),
        (MetadataField::AlbumArtist, text(&tags.album_artist)),
        (MetadataField::Genre, text(&tags.genre)),
        (MetadataField::MusicalKey, text(&tags.musical_key)),
        (MetadataField::Isrc, text(&tags.isrc)),
        (MetadataField::ReleaseYear, count(tags.year)),
        (MetadataField::TrackNumber, count(tags.track_number)),
        (MetadataField::DiscNumber, count(tags.disc_number)),
        (
            MetadataField::DurationMs,
            tags.duration.and_then(|duration| {
                i64::try_from(duration.as_millis())
                    .ok()
                    .map(MetadataValue::Integer)
            }),
        ),
        (
            MetadataField::Bpm,
            tags.bpm.map(|bpm| MetadataValue::Decimal(bpm.to_string())),
        ),
    ] {
        track
            .metadata
            .set_observation(field, value, embedded.clone());
    }

    let filename_title = tags.title.is_none().then(|| {
        MetadataValue::Text(
            file.absolute_path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Unknown track")
                .to_owned(),
        )
    });
    track.metadata.set_observation(
        MetadataField::Title,
        filename_title,
        MetadataProvenance::provider(source_id, &file.external_id, observed_at),
    );
    if let Some(external_id) = superseded_external_id {
        track.metadata.set_observation(
            MetadataField::Title,
            None,
            MetadataProvenance::provider(source_id, external_id, observed_at),
        );
    }
}

fn preferred_text(track: &CanonicalTrack, field: MetadataField) -> Option<String> {
    match &track.metadata.preferred(&field)?.value {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

fn enforce_copy_policy(
    policy: SourceCopyPolicy,
    authorization: CopyAuthorization,
) -> Result<(), LocalLibraryError> {
    match (policy, authorization) {
        (SourceCopyPolicy::CheckOnly, _) => Err(LocalLibraryError::CopyForbidden),
        (SourceCopyPolicy::AskBeforeCopying, CopyAuthorization::Automatic) => {
            Err(LocalLibraryError::CopyConfirmationRequired)
        }
        _ => Ok(()),
    }
}
fn parse_uuid(value: &str) -> Result<Uuid, LocalLibraryError> {
    Uuid::parse_str(value).map_err(|_| LocalLibraryError::InvalidIdentifier(value.into()))
}

fn transition_error(error: impl std::fmt::Display) -> LocalLibraryError {
    LocalLibraryError::InvalidRoot(error.to_string())
}

fn scanned_len_u64(created: u64, reused: u64) -> u64 {
    created + reused
}

async fn fail_run(
    catalog: &TursoCatalog,
    run: &mut SyncRun,
    message: &str,
) -> Result<(), RepositoryError> {
    let error = SyncError::new(SyncErrorKind::Io, "local_storage", message, true);
    let _ = run.fail(error);
    SyncRunRepository::upsert(catalog, run.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{MatchConfidence, ReconciliationRepository};
    use std::sync::Mutex;
    use std::time::Duration;

    struct TestSelector {
        selection: Mutex<Option<LocalSourceSelection>>,
    }

    impl LocalSelectionProvider for TestSelector {
        fn select_source<'a>(&'a self) -> SelectionFuture<'a> {
            Box::pin(async move { Ok(self.selection.lock().unwrap().take()) })
        }
    }

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("rec-bag-local-library-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn service(selection: Option<LocalSourceSelection>) -> LocalLibraryService {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        LocalLibraryService::new(
            catalog,
            Arc::new(TestSelector {
                selection: Mutex::new(selection),
            }),
        )
    }

    fn primary_view(path: &Path) -> PrimaryConfigurationView {
        PrimaryConfigurationView {
            provider: ProviderChoice::LocalFolder,
            name: "Primary".into(),
            location: path.to_string_lossy().into_owned(),
            organize_by_artist: true,
            verify_after_copy: true,
        }
    }

    #[test]
    fn scan_skips_symlinks_history_and_non_audio() {
        let root = TempDirectory::new("scan");
        fs::write(root.path().join("one.mp3"), b"one").unwrap();
        fs::write(root.path().join("notes.txt"), b"text").unwrap();
        fs::create_dir(root.path().join(".rec_bag_history")).unwrap();
        fs::write(root.path().join(".rec_bag_history/old.mp3"), b"old").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("one.mp3"), root.path().join("link.mp3"))
            .unwrap();
        let files = scan_files(root.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "one.mp3");
        assert!(files[0].hash.is_canonical_sha256());
    }

    #[test]
    fn copy_policy_requires_the_right_authorization() {
        assert!(matches!(
            enforce_copy_policy(
                SourceCopyPolicy::CheckOnly,
                CopyAuthorization::UserConfirmed
            ),
            Err(LocalLibraryError::CopyForbidden)
        ));
        assert!(matches!(
            enforce_copy_policy(
                SourceCopyPolicy::AskBeforeCopying,
                CopyAuthorization::Automatic
            ),
            Err(LocalLibraryError::CopyConfirmationRequired)
        ));
        assert!(enforce_copy_policy(
            SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            CopyAuthorization::Automatic
        )
        .is_ok());
    }

    /// Write a real, tagged audio file of a known length.
    ///
    /// A placeholder byte string would test nothing here: the point is what a scan reads
    /// out of a file that genuinely carries tags and audio properties.
    fn write_tagged_audio(path: &Path, length: Duration, tag: impl FnOnce(&mut rec_core::Track)) {
        const SAMPLE_RATE: u32 = 44_100;
        let samples = (length.as_secs_f64() * SAMPLE_RATE as f64).round() as u32;
        let data_len = samples * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        fs::write(path, wav).unwrap();

        let mut track = rec_core::Track::new_without_metadata(path.to_path_buf());
        tag(&mut track);
        track.save_metadata().unwrap();
    }

    fn preferred_value(track: &CanonicalTrack, field: MetadataField) -> Option<MetadataValue> {
        track
            .metadata
            .preferred(&field)
            .map(|candidate| candidate.value.clone())
    }

    /// A track whose length the catalog does not know cannot be seeked on a deck, so
    /// what a file says about itself has to survive the scan.
    #[tokio::test]
    async fn a_scan_records_what_the_files_own_tags_say() {
        let root = TempDirectory::new("tags");
        write_tagged_audio(
            &root.path().join("filename.wav"),
            Duration::from_secs(3),
            |track| {
                track.title = "Tagged title".into();
                track.artist = Some("Tagged artist".into());
                track.album = Some("Tagged album".into());
                track.genre = Some("Tagged genre".into());
                track.year = Some(1999);
            },
        );
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        let track = service.list_tracks().await.unwrap().remove(0);

        assert_eq!(
            preferred_value(&track, MetadataField::DurationMs),
            Some(MetadataValue::Integer(3_000)),
            "the deck's trackbar spans this value; without it seeking is impossible"
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Tagged title".into())),
            "the file's own title outranks its filename"
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Artist),
            Some(MetadataValue::Text("Tagged artist".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Album),
            Some(MetadataValue::Text("Tagged album".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Genre),
            Some(MetadataValue::Text("Tagged genre".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::ReleaseYear),
            Some(MetadataValue::Integer(1999))
        );
    }

    /// A file with nothing to say still gets a name, and still gets a length.
    #[tokio::test]
    async fn an_untagged_file_falls_back_to_its_filename_but_keeps_its_length() {
        let root = TempDirectory::new("untagged");
        write_tagged_audio(
            &root.path().join("Some Artist - Some Title.wav"),
            Duration::from_secs(1),
            |_| {},
        );
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        let track = service.list_tracks().await.unwrap().remove(0);

        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Some Artist - Some Title".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::DurationMs),
            Some(MetadataValue::Integer(1_000))
        );
    }

    /// Rescanning restates what a file says; it does not stack another copy of it.
    #[tokio::test]
    async fn rescanning_restates_a_files_tags_rather_than_accumulating_them() {
        let root = TempDirectory::new("restate");
        let file = root.path().join("track.wav");
        write_tagged_audio(&file, Duration::from_secs(2), |track| {
            track.title = "First".into();
        });
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        write_tagged_audio(&file, Duration::from_secs(2), |track| {
            track.title = "Second".into();
        });
        service.scan_local_source(first.source_id).await.unwrap();

        let track = service.list_tracks().await.unwrap().remove(0);
        let titles: Vec<_> = track
            .metadata
            .values
            .iter()
            .filter(|candidate| candidate.field == MetadataField::Title)
            .collect();
        assert_eq!(titles.len(), 1, "one file says one thing about its title");
        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Second".into()))
        );
    }

    #[tokio::test]
    async fn stable_scan_reuses_track_and_occurrence() {
        let root = TempDirectory::new("stable");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        let second = service.scan_local_source(first.source_id).await.unwrap();
        assert_eq!(first.created_tracks, 1);
        assert_eq!(second.created_tracks, 0);
        assert_eq!(second.reused_tracks, 1);
        assert_eq!(service.list_tracks().await.unwrap().len(), 1);
        assert_eq!(
            service
                .list_occurrences(Some(first.source_id), None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn inode_identity_survives_a_move() {
        let root = TempDirectory::new("move");
        fs::write(root.path().join("before.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        fs::rename(
            root.path().join("before.mp3"),
            root.path().join("after.mp3"),
        )
        .unwrap();
        let second = service.scan_local_source(first.source_id).await.unwrap();
        assert_eq!(second.created_tracks, 0);
        assert_eq!(service.list_tracks().await.unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configure_local_storage_rejects_a_read_only_primary_root() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDirectory::new("read-only-primary");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let service = service(None).await;

        let error = service
            .configure_local_storage(&primary_view(root.path()))
            .await
            .unwrap_err();

        assert!(
            matches!(error, LocalLibraryError::InvalidRoot(message) if message == "local storage is read-only")
        );
    }

    #[tokio::test]
    async fn full_inventory_is_staged_and_finalized() {
        let root = TempDirectory::new("inventory");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let storage = service
            .configure_local_storage(&primary_view(root.path()))
            .await
            .unwrap();
        let result = service
            .inventory_storage(storage.id, InventoryMode::Full, true)
            .await
            .unwrap();
        assert_eq!(result.object_count, 1);
        assert_eq!(
            result.generation.status,
            rec_core::InventoryGenerationStatus::Finalized
        );
        let objects = service.list_storage_objects(storage.id).await.unwrap();
        assert_eq!(objects.len(), 1);
        assert!(objects[0].canonical_hash().is_some());
    }

    #[tokio::test]
    async fn in_place_adoption_verifies_without_copying() {
        let root = TempDirectory::new("in-place");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let scan = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            })
            .await
            .unwrap();
        let migration = service
            .adopt_migrated_library(scan.source_id, &primary_view(root.path()))
            .await
            .unwrap();
        assert_eq!(
            service
                .catalog
                .get_active_primary()
                .await
                .unwrap()
                .unwrap()
                .id,
            migration.active_primary_id
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        let track = service.list_tracks().await.unwrap().remove(0);
        let resolved = service.resolve_playback(track.id).await.unwrap();
        assert_eq!(resolved.origin, PlaybackOrigin::ActivePrimary);
        assert_eq!(resolved.path, root.path().join("track.mp3"));
    }

    #[tokio::test]
    async fn local_to_local_copy_uses_managed_path_and_verified_location() {
        let source_root = TempDirectory::new("copy-source");
        let primary_root = TempDirectory::new("copy-primary");
        fs::write(source_root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: source_root.path().into(),
                copy_policy: SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            })
            .await
            .unwrap();
        let migration = service
            .migrate_primary(&primary_view(primary_root.path()), false)
            .await
            .unwrap();
        let objects = service
            .list_storage_objects(migration.active_primary_id)
            .await
            .unwrap();
        assert_eq!(objects.len(), 1);
        assert!(objects[0].object_id.contains("Unknown Artist"));
        let location = service
            .catalog
            .get_location(migration.active_primary_id, objects[0].object_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            location.verification_status,
            StorageVerificationStatus::Verified
        );
    }

    #[tokio::test]
    async fn reconciliation_persists_weak_candidates_and_projects_view() {
        let root = TempDirectory::new("projection");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        let storage = service
            .configure_local_storage(&primary_view(root.path()))
            .await
            .unwrap();
        service
            .inventory_storage(storage.id, InventoryMode::Full, false)
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(storage.id))
            .await
            .unwrap();
        let projection = service.reconcile_primary().await.unwrap();
        assert_eq!(projection.report.summary.total_tracks, 1);
        let candidates = service
            .catalog
            .list_candidates(
                rec_core::MatchCandidateFilter {
                    confidence: Some(MatchConfidence::ReviewRequired),
                    ..Default::default()
                },
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap();
        assert!(!candidates.items.is_empty());
        let view = service.project_view().await.unwrap();
        assert_eq!(view.sources.len(), 1);
        assert_eq!(view.tracks.len(), 1);
    }

    #[tokio::test]
    async fn handler_uses_injected_selection_and_returns_replacement_view() {
        let root = TempDirectory::new("handler");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(Some(LocalSourceSelection {
            name: "Selected".into(),
            root: root.path().into(),
            copy_policy: SourceCopyPolicy::AskBeforeCopying,
        }))
        .await;
        let outcome = service
            .handle(IntegrationRequest {
                id: 7,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::AddSource(ProviderChoice::LocalFolder),
            })
            .await
            .unwrap();
        assert!(outcome.message.unwrap().contains("Scanned 1"));
        assert_eq!(outcome.view.unwrap().sources.len(), 1);
    }

    #[tokio::test]
    async fn copy_to_primary_event_uses_user_confirmation_and_refreshes_view() {
        let source_root = TempDirectory::new("event-copy-source");
        let primary_root = TempDirectory::new("event-copy-primary");
        fs::write(source_root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: source_root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        let primary = service
            .configure_local_storage(&primary_view(primary_root.path()))
            .await
            .unwrap();
        service
            .inventory_storage(primary.id, InventoryMode::Full, true)
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(primary.id))
            .await
            .unwrap();
        let track_id = service.list_tracks().await.unwrap()[0].id;

        let outcome = service
            .handle(IntegrationRequest {
                id: 20,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::CopyToPrimary(track_id.to_string()),
            })
            .await
            .unwrap();

        assert!(outcome.message.unwrap().contains("Copied"));
        let view = outcome.view.unwrap();
        assert_eq!(view.primary_storage.unwrap().id, primary.id.to_string());
        assert_eq!(view.completeness.verified_assets, 1);
        assert_eq!(
            service
                .list_storage_objects(primary.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn retry_storage_event_inventories_local_storage_and_reconciles() {
        let root = TempDirectory::new("event-retry");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let storage = service
            .configure_local_storage(&primary_view(root.path()))
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(storage.id))
            .await
            .unwrap();

        let outcome = service
            .handle(IntegrationRequest {
                id: 21,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::RetryStorage(storage.id.to_string()),
            })
            .await
            .unwrap();

        assert!(outcome.message.unwrap().contains("Inventoried 1"));
        assert!(outcome
            .view
            .unwrap()
            .primary_storage
            .unwrap()
            .last_inventory
            .is_some());
        assert!(service
            .catalog
            .latest_finalized_inventory_generation(storage.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn add_local_backup_event_configures_enabled_backup_and_rejects_overlap() {
        let primary_root = TempDirectory::new("event-backup-primary");
        let backup_root = TempDirectory::new("event-backup-target");
        let service = service(Some(LocalSourceSelection {
            name: "Local backup".into(),
            root: backup_root.path().into(),
            copy_policy: SourceCopyPolicy::CheckOnly,
        }))
        .await;
        let primary = service
            .configure_local_storage(&primary_view(primary_root.path()))
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(primary.id))
            .await
            .unwrap();

        let outcome = service
            .handle(IntegrationRequest {
                id: 22,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder),
            })
            .await
            .unwrap();
        let view = outcome.view.unwrap();
        assert_eq!(view.backup_storages.len(), 1);
        let backup_id = parse_uuid(&view.backup_storages[0].id).unwrap();
        let backup = MusicStorageRepository::get(service.catalog.as_ref(), backup_id)
            .await
            .unwrap()
            .unwrap();
        assert!(backup.enabled);
        assert_eq!(backup.role, StorageRole::Backup);

        let unsafe_service = LocalLibraryService::new(
            service.catalog.clone(),
            Arc::new(TestSelector {
                selection: Mutex::new(Some(LocalSourceSelection {
                    name: "Unsafe backup".into(),
                    root: primary_root.path().into(),
                    copy_policy: SourceCopyPolicy::CheckOnly,
                })),
            }),
        );
        let error = unsafe_service
            .handle(IntegrationRequest {
                id: 23,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder),
            })
            .await
            .unwrap_err();
        assert!(error.message.contains("distinct and non-nested"));
    }

    #[tokio::test]
    async fn prepare_primary_migration_event_snapshots_before_final_cas() {
        let source_root = TempDirectory::new("event-migrate-source");
        let old_root = TempDirectory::new("event-migrate-old");
        let new_root = TempDirectory::new("event-migrate-new");
        fs::write(source_root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: source_root.path().into(),
                copy_policy: SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            })
            .await
            .unwrap();
        let old = service
            .configure_local_storage(&primary_view(old_root.path()))
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(old.id))
            .await
            .unwrap();

        let outcome = service
            .handle(IntegrationRequest {
                id: 24,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::PreparePrimaryMigration(
                    ui::PrimaryMigrationPlanView {
                        destination: primary_view(new_root.path()),
                        verify_checksums: true,
                        keep_old_as_backup: true,
                    },
                ),
            })
            .await
            .unwrap();

        assert!(outcome.message.unwrap().contains("after snapshot"));
        let snapshot = rec_core::CatalogSnapshotRepository::latest(service.catalog.as_ref())
            .await
            .unwrap();
        assert!(snapshot.is_some());
        let active = service.catalog.get_active_primary().await.unwrap().unwrap();
        assert_eq!(
            PathBuf::from(active.root_identifier.unwrap()),
            fs::canonicalize(new_root.path()).unwrap()
        );
        let old = MusicStorageRepository::get(service.catalog.as_ref(), old.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.role, StorageRole::Backup);
        assert_eq!(outcome.view.unwrap().completeness.verified_assets, 1);
    }

    #[tokio::test]
    async fn primary_switch_uses_expected_active_storage() {
        let first_root = TempDirectory::new("cas-first");
        let second_root = TempDirectory::new("cas-second");
        let service = service(None).await;
        let first = service
            .configure_local_storage(&primary_view(first_root.path()))
            .await
            .unwrap();
        service
            .catalog
            .switch_active_primary(None, Some(first.id))
            .await
            .unwrap();
        let second = service
            .configure_local_storage(&primary_view(second_root.path()))
            .await
            .unwrap();
        let error = service
            .catalog
            .switch_active_primary(None, Some(second.id))
            .await
            .unwrap_err();
        assert_eq!(error.kind, rec_core::RepositoryErrorKind::Conflict);
    }
}
