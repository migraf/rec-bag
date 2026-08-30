//! Backup replication, database backups, retention review, and verification orchestration.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use chrono::{DateTime, Duration, Utc};
use rec_core::{
    AssetLocation, AssetRepository, AssetType, BoxFuture, ContentHash, DownloadRequest,
    HealthStatus, InventoryMode, InventoryRequest, MusicAssetStore, MusicStorage,
    MusicStorageError, MusicStorageErrorKind, MusicStorageRepository, OriginalAsset,
    OriginalAssetFilter, RepositoryError, RepositoryErrorKind, RepositoryPageRequest,
    RetentionPolicy, Setting, SettingsRepository, StorageObject, StorageObjectVersion, StorageRole,
    StorageVerificationStatus, StorageVersionRequest, SyncCheckpoint, SyncError, SyncErrorKind,
    SyncJob, SyncJobPayload, SyncProgress, SyncRun, SyncRunKind, SyncStatus, TransferCheckpoint,
    TransferStage, UploadRequest, SYNC_JOB_PAYLOAD_VERSION,
};
use rec_persistence::{
    CleanupApproval as PersistedCleanupApproval, DurableTransferResult, TursoCatalog,
};
use rec_sync::{
    evaluate_retention, BackupAvailability, BackupTarget, CancellationRequest, JobCompletion,
    JobEngine, JobEvent, JobEventKind, JobExecutor, JobPlanner, JobStep, JobStore, JobStoreError,
    ReplicationDecision, ReplicationError, ReplicationExecutor, ReplicationPlanner,
    ReplicationRequest, ReplicationStage, ReplicationStep, ReplicationTrigger, RetentionEvaluation,
    StagedReplicationRequest, VerificationPlan, VerificationPlanner, VerificationRequest,
};

use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use ui::{BackupPolicyView, MusicManagementEvent, ProviderChoice};
use uuid::Uuid;

use super::integration::{IntegrationFuture, IntegrationSliceHandler};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

const DEFAULT_DEFER: Duration = Duration::minutes(15);
/// Where a backup storage keeps copies of the catalog database.
const DATABASE_BACKUP_DIRECTORY: &str = ".rec_bag/database-backups/v1";
const CLEANUP_KEY_SEPARATOR: char = '|';

/// Runtime registry for configured local and Google Drive stores. Both adapters are
/// represented by `MusicAssetStore`, so every transfer follows the same staged path.
#[derive(Default)]
pub struct BackupProviderRegistry {
    stores: RwLock<BTreeMap<Uuid, Arc<dyn MusicAssetStore>>>,
}

impl crate::services::catalog_projection::StorageProviders for BackupProviderRegistry {
    fn store(&self, storage: &MusicStorage) -> Option<Arc<dyn MusicAssetStore>> {
        self.get(storage.id).ok()
    }
}

impl BackupProviderRegistry {
    pub fn register(
        &self,
        storage_id: Uuid,
        store: Arc<dyn MusicAssetStore>,
    ) -> Option<Arc<dyn MusicAssetStore>> {
        self.stores
            .write()
            .expect("backup provider registry lock poisoned")
            .insert(storage_id, store)
    }

    pub fn remove(&self, storage_id: Uuid) -> Option<Arc<dyn MusicAssetStore>> {
        self.stores
            .write()
            .expect("backup provider registry lock poisoned")
            .remove(&storage_id)
    }

    pub fn get(&self, storage_id: Uuid) -> Result<Arc<dyn MusicAssetStore>, BackupError> {
        self.stores
            .read()
            .expect("backup provider registry lock poisoned")
            .get(&storage_id)
            .cloned()
            .ok_or(BackupError::ProviderNotRegistered(storage_id))
    }
}

/// Cloneable adapter allowing the job engine and service repositories to share one catalog.
#[derive(Clone)]
struct CatalogJobStore(Arc<TursoCatalog>);

impl JobStore for CatalogJobStore {
    fn create_run_with_jobs(
        &self,
        run: SyncRun,
        jobs: Vec<SyncJob>,
    ) -> BoxFuture<'_, Result<(), JobStoreError>> {
        self.0.create_run_with_jobs(run, jobs)
    }

    fn claim_due(
        &self,
        worker_id: String,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>> {
        self.0.claim_due(worker_id, claimed_at, lease_expires_at)
    }

    fn load_run(&self, run_id: Uuid) -> BoxFuture<'_, Result<Option<SyncRun>, JobStoreError>> {
        self.0.load_run(run_id)
    }

    fn load_job(&self, job_id: Uuid) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>> {
        self.0.load_job(job_id)
    }

    fn list_run_jobs(&self, run_id: Uuid) -> BoxFuture<'_, Result<Vec<SyncJob>, JobStoreError>> {
        self.0.list_run_jobs(run_id)
    }

    fn save_run(&self, run: SyncRun) -> BoxFuture<'_, Result<(), JobStoreError>> {
        self.0.save_run(run)
    }

    fn save_job_and_event(
        &self,
        job: SyncJob,
        event: JobEvent,
    ) -> BoxFuture<'_, Result<(), JobStoreError>> {
        self.0.save_job_and_event(job, event)
    }

    fn append_event(&self, event: JobEvent) -> BoxFuture<'_, Result<(), JobStoreError>> {
        self.0.append_event(event)
    }

    fn list_events(&self, run_id: Uuid) -> BoxFuture<'_, Result<Vec<JobEvent>, JobStoreError>> {
        self.0.list_events(run_id)
    }

    fn request_cancellation(
        &self,
        run_id: Uuid,
        requested_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<CancellationRequest, JobStoreError>> {
        self.0.request_cancellation(run_id, requested_at)
    }
}

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("music store {0} is not registered")]
    ProviderNotRegistered(Uuid),
    #[error("storage {0} was not found")]
    StorageNotFound(Uuid),
    #[error("no enabled active primary storage is configured")]
    ActivePrimaryMissing,
    #[error("asset {0} has no verified primary location")]
    VerifiedPrimaryMissing(Uuid),
    #[error("replication source does not have a canonical SHA-256 checksum")]
    MissingChecksum,
    #[error("invalid backup request: {0}")]
    InvalidRequest(String),
    #[error("storage operation failed: {0}")]
    Storage(#[from] MusicStorageError),
    #[error("repository operation failed: {0}")]
    Repository(#[from] RepositoryError),
    #[error("job orchestration failed: {0}")]
    JobStore(#[from] JobStoreError),
    #[error("replication failed: {0}")]
    Replication(#[from] ReplicationError),
    #[error("staging I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl BackupError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::Storage(error) => error.is_retryable(),
            Self::Repository(error) => matches!(
                error.kind,
                RepositoryErrorKind::TemporarilyUnavailable | RepositoryErrorKind::Io
            ),
            Self::JobStore(JobStoreError::TemporarilyUnavailable(_)) | Self::Io(_) => true,
            Self::Replication(ReplicationError::Storage { source, .. }) => source.is_retryable(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedReplication {
    pub run_id: Option<Uuid>,
    pub decisions: Vec<ReplicationDecision>,
    pub queued_jobs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetHandoffDecision {
    pub asset_id: Uuid,
    pub run_id: Option<Uuid>,
    pub decisions: Vec<ReplicationDecision>,
    pub queued_jobs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogReplicationHandoff {
    pub considered_assets: usize,
    pub queued_jobs: usize,
    pub deferred_targets: usize,
    pub assets: Vec<AssetHandoffDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupProgress {
    pub run_id: Uuid,
    pub status: SyncStatus,
    pub progress: SyncProgress,
    pub latest_message: Option<String>,
    pub latest_error: Option<SyncError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReviewItem {
    pub storage_id: Uuid,
    pub object_id: String,
    pub evaluation: RetentionEvaluation,
    pub approved_candidate_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReview {
    pub generated_at: DateTime<Utc>,
    pub items: Vec<RetentionReviewItem>,
}

impl RetentionReview {
    pub fn candidate_keys(&self) -> BTreeSet<String> {
        self.items
            .iter()
            .flat_map(|item| {
                item.evaluation.cleanup_candidates.iter().map(|candidate| {
                    cleanup_key(item.storage_id, &candidate.object_id, &candidate.version_id)
                })
            })
            .collect()
    }
}

/// One copy of the catalog database held by a backup storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseBackup {
    pub storage_id: Uuid,
    pub storage_name: String,
    pub object_id: String,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
}

/// Where a downloaded backup waits for startup to move it into place.
pub fn staged_restore_path(catalog_path: &Path) -> PathBuf {
    catalog_path.with_extension("db.incoming")
}

async fn hash_file(path: &Path) -> Result<ContentHash, SyncError> {
    let bytes = tokio::fs::read(path).await.map_err(io_sync_error)?;
    let digest = Sha256::digest(&bytes);
    ContentHash::sha256(format!("{digest:x}")).map_err(|error| {
        sync_error(
            SyncErrorKind::Integrity,
            "backup.hash",
            error.to_string(),
            false,
        )
    })
}

/// Scheduler-facing hooks owned by Slice 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSchedulerHook {
    WorkerWake,
    VerifiedPrimaryWrite(ReplicationRequest),
    ScheduledReplication(ReplicationRequest),
    Verification {
        requests: Vec<VerificationRequest>,
        sample_modulus: u64,
    },
    MonthlyInventory,
    RetentionReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSchedulerOutcome {
    Idle,
    JobEvent(JobEvent),
    Replication(PlannedReplication),
    VerificationRun(Option<Uuid>),
    Retention(RetentionReview),
}

/// Desktop Slice 4 composition service.
pub struct BackupService {
    catalog: Arc<TursoCatalog>,
    providers: Arc<BackupProviderRegistry>,
    engine: JobEngine<CatalogJobStore>,
    staging_root: PathBuf,
}

impl BackupService {
    pub fn new(
        catalog: Arc<TursoCatalog>,
        providers: Arc<BackupProviderRegistry>,
        staging_root: impl Into<PathBuf>,
    ) -> Self {
        let store = CatalogJobStore(catalog.clone());
        Self {
            catalog,
            providers,
            engine: JobEngine::new(store),
            staging_root: staging_root.into(),
        }
    }

    pub fn providers(&self) -> &Arc<BackupProviderRegistry> {
        &self.providers
    }

    pub async fn plan_replication(
        &self,
        request: ReplicationRequest,
        now: DateTime<Utc>,
    ) -> Result<PlannedReplication, BackupError> {
        if request.source_storage.id == Uuid::nil() {
            return Err(BackupError::InvalidRequest(
                "source storage ID cannot be nil".into(),
            ));
        }
        if AssetRepository::get_original(self.catalog.as_ref(), request.asset_id)
            .await?
            .is_none()
        {
            return Err(BackupError::VerifiedPrimaryMissing(request.asset_id));
        }
        if request
            .backups
            .iter()
            .any(|target| target.storage.id == request.source_storage.id)
        {
            return Err(BackupError::InvalidRequest(
                "source and backup storage must differ".into(),
            ));
        }

        let decisions = ReplicationPlanner::plan(&request);
        let run = SyncRun::at(SyncRunKind::BackupReplication, None, None, now);
        let mut jobs = ReplicationPlanner::durable_jobs(&request, run.id, now, now + DEFAULT_DEFER);
        let mut unique = Vec::with_capacity(jobs.len());
        for job in jobs.drain(..) {
            if self
                .catalog
                .get_transfer_result(&job.idempotency_key)
                .await?
                .is_some()
                || self
                    .catalog
                    .find_active_job_by_idempotency_key(&job.idempotency_key)
                    .await?
                    .is_some()
            {
                continue;
            }
            unique.push(job);
        }
        if unique.is_empty() {
            return Ok(PlannedReplication {
                run_id: None,
                decisions,
                queued_jobs: 0,
            });
        }
        let count = unique.len();
        self.engine.persist_plan(run.clone(), unique).await?;
        Ok(PlannedReplication {
            run_id: Some(run.id),
            decisions,
            queued_jobs: count,
        })
    }

    /// Convenience planner for callers that have resolved one verified primary object.
    pub async fn plan_verified_primary_write(
        &self,
        asset_id: Uuid,
        source_storage: MusicStorage,
        source_object: StorageObject,
        source_location: AssetLocation,
        destination_object_key: String,
        now: DateTime<Utc>,
    ) -> Result<PlannedReplication, BackupError> {
        let mut backups = Vec::new();
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.role != StorageRole::Backup || !storage.enabled {
                continue;
            }
            let availability = match self.providers.get(storage.id) {
                Ok(store) => match store.health().await {
                    Ok(health) if health.status == HealthStatus::Healthy => {
                        BackupAvailability::Available
                    }
                    _ => BackupAvailability::Unavailable,
                },
                Err(_) => BackupAvailability::Unavailable,
            };
            let existing_location =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), asset_id)
                    .await?
                    .into_iter()
                    .find(|location| location.storage_id == storage.id);
            backups.push(BackupTarget {
                storage,
                availability,
                existing_location,
            });
        }
        self.plan_replication(
            ReplicationRequest {
                asset_id,
                source_storage,
                source_object,
                source_location,
                destination_object_key,
                trigger: ReplicationTrigger::VerifiedPrimaryWrite,
                backups,
            },
            now,
        )
        .await
    }

    /// Resolve one asset, or every currently verified primary asset, from the catalog and
    /// hand each immutable source version to the existing idempotent replication planner.
    ///
    /// Passing `Some(asset_id)` is used after a verified write. Passing `None` is used after
    /// primary migration to close every remaining backup gap. Assets without a verified
    /// location on the active primary are skipped during an all-assets handoff and rejected
    /// for a targeted handoff.
    pub async fn handoff_verified_primary_assets(
        &self,
        asset_id: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<CatalogReplicationHandoff, BackupError> {
        let primary = MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .filter(|storage| storage.enabled && storage.role == StorageRole::Primary)
            .ok_or(BackupError::ActivePrimaryMissing)?;
        let source_provider = self.providers.get(primary.id)?;
        let originals = match asset_id {
            Some(id) => vec![AssetRepository::get_original(self.catalog.as_ref(), id)
                .await?
                .ok_or(BackupError::VerifiedPrimaryMissing(id))?],
            None => list_original_assets(self.catalog.as_ref()).await?,
        };

        let mut aggregate = CatalogReplicationHandoff::default();
        for original in originals {
            let resolved = resolve_verified_primary_asset(
                self.catalog.as_ref(),
                source_provider.as_ref(),
                &primary,
                &original,
            )
            .await?;
            let Some((source_object, source_location)) = resolved else {
                if asset_id.is_some() {
                    return Err(BackupError::VerifiedPrimaryMissing(original.id()));
                }
                continue;
            };
            let planned = self
                .plan_verified_primary_write(
                    original.id(),
                    primary.clone(),
                    source_object,
                    source_location,
                    backup_destination_key(&original),
                    now,
                )
                .await?;
            aggregate.considered_assets += 1;
            aggregate.queued_jobs += planned.queued_jobs;
            aggregate.deferred_targets += planned
                .decisions
                .iter()
                .filter(|decision| {
                    matches!(decision, ReplicationDecision::DeferredUnavailable { .. })
                })
                .count();
            aggregate.assets.push(AssetHandoffDecision {
                asset_id: original.id(),
                run_id: planned.run_id,
                decisions: planned.decisions,
                queued_jobs: planned.queued_jobs,
            });
        }
        aggregate.assets.sort_by_key(|asset| asset.asset_id);
        Ok(aggregate)
    }

    pub async fn run_worker_once(
        &self,
        worker_id: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Option<JobEvent>, BackupError> {
        let executor = BackupExecutor {
            catalog: self.catalog.clone(),
            providers: self.providers.clone(),
            staging_root: self.staging_root.clone(),
        };
        let event = self.engine.run_once(worker_id, now, &executor).await?;
        if let Some(event) = &event {
            if matches!(event.kind, JobEventKind::Succeeded | JobEventKind::Skipped) {
                self.reconcile_terminal_receipt(event, now).await?;
            }
        }
        Ok(event)
    }

    pub async fn restart_run(
        &self,
        run_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>, BackupError> {
        let previous = self
            .engine
            .store()
            .load_run(run_id)
            .await?
            .ok_or(JobStoreError::NotFound(format!("run {run_id}")))?;
        let jobs = self.engine.store().list_run_jobs(run_id).await?;
        let plan = JobPlanner::restart(&previous, &jobs, now)?;
        let mut unfinished = Vec::new();
        for job in plan.jobs {
            if self
                .catalog
                .get_transfer_result(&job.idempotency_key)
                .await?
                .is_none()
                && self
                    .catalog
                    .find_active_job_by_idempotency_key(&job.idempotency_key)
                    .await?
                    .is_none()
            {
                unfinished.push(job);
            }
        }
        if unfinished.is_empty() {
            return Ok(None);
        }
        let resumed_id = plan.run.id;
        self.engine.persist_plan(plan.run, unfinished).await?;
        Ok(Some(resumed_id))
    }

    pub async fn cancel_run(
        &self,
        run_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<SyncRun, BackupError> {
        Ok(self.engine.request_cancellation(run_id, now).await?)
    }

    pub async fn project_progress(&self, run_id: Uuid) -> Result<BackupProgress, BackupError> {
        let run = self
            .engine
            .store()
            .load_run(run_id)
            .await?
            .ok_or(JobStoreError::NotFound(format!("run {run_id}")))?;
        let events = self.engine.store().list_events(run_id).await?;
        let latest_message = events.iter().rev().find_map(|event| event.message.clone());
        Ok(BackupProgress {
            run_id,
            status: run.status,
            progress: run.progress,
            latest_message,
            latest_error: run.error,
        })
    }

    /// Copies the catalog database to every enabled backup storage.
    ///
    /// The database file *is* the backup, so there is nothing to coalesce or compare:
    /// asking for one makes one. Delivery runs through the same durable jobs as asset
    /// replication, so a partial upload resumes rather than starting over.
    pub async fn back_up_database(&self, now: DateTime<Utc>) -> Result<Option<Uuid>, BackupError> {
        let run = SyncRun::at(SyncRunKind::DatabaseBackup, None, None, now);
        let stamp = now.format("%Y%m%dT%H%M%SZ");
        let mut jobs = Vec::new();
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.role != StorageRole::Backup || !storage.enabled {
                continue;
            }
            let object_key = format!("{DATABASE_BACKUP_DIRECTORY}/library-{stamp}.db");
            let key = format!("database-backup:{}:{object_key}", storage.id);
            if self
                .catalog
                .find_active_job_by_idempotency_key(&key)
                .await?
                .is_some()
            {
                continue;
            }
            jobs.push(SyncJob::with_retry_policy_at(
                run.id,
                SyncJobPayload::BackUpDatabase {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    destination_storage_id: storage.id,
                    destination_object_key: object_key,
                },
                3,
                Some(key),
                now,
            ));
        }
        if jobs.is_empty() {
            return Ok(None);
        }
        self.engine.persist_plan(run.clone(), jobs).await?;
        Ok(Some(run.id))
    }

    /// Every catalog database copy held by an enabled backup storage, newest first.
    pub async fn list_database_backups(&self) -> Result<Vec<DatabaseBackup>, BackupError> {
        let mut backups = Vec::new();
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.role != StorageRole::Backup || !storage.enabled {
                continue;
            }
            let Ok(provider) = self.providers.get(storage.id) else {
                continue;
            };
            let mut cursor = None;
            loop {
                let page = provider
                    .inventory_page(InventoryRequest {
                        mode: InventoryMode::Full,
                        page_cursor: cursor.clone(),
                        page_size: None,
                        prefix: Some(DATABASE_BACKUP_DIRECTORY.into()),
                        include_checksums: false,
                    })
                    .await
                    .map_err(BackupError::Storage)?;
                for object in page.objects {
                    if !object.name.ends_with(".db") {
                        continue;
                    }
                    backups.push(DatabaseBackup {
                        storage_id: storage.id,
                        storage_name: storage.name.clone(),
                        object_id: object.object_id,
                        size_bytes: object.size_bytes,
                        created_at: object.modified_at,
                    });
                }
                cursor = page.next_page_cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        backups.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(backups)
    }

    /// Downloads one backup and stages it to replace the catalog on the next start.
    ///
    /// The running process holds the live database open, so the swap cannot happen now.
    /// Staging it and letting startup do the move keeps the exchange to one rename
    /// against a database nothing has opened yet.
    pub async fn restore_database(
        &self,
        backup: &DatabaseBackup,
        catalog_path: &Path,
    ) -> Result<PathBuf, BackupError> {
        let provider = self.providers.get(backup.storage_id)?;
        let staged = staged_restore_path(catalog_path);
        ensure_staging_parent(&staged)
            .await
            .map_err(|error| BackupError::InvalidRequest(error.to_string()))?;
        provider
            .download(DownloadRequest {
                object_id: backup.object_id.clone(),
                destination_path: staged.clone(),
                range: None,
                expected_checksum: None,
                overwrite: true,
            })
            .await
            .map_err(BackupError::Storage)?;
        // A file that will not open as a catalog must never displace the live one.
        TursoCatalog::open(&staged).await.map_err(|error| {
            BackupError::InvalidRequest(format!(
                "the downloaded backup is not a usable catalog: {error}"
            ))
        })?;
        Ok(staged)
    }

    pub async fn review_retention(
        &self,
        storage_filter: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<RetentionReview, BackupError> {
        let mut items = Vec::new();
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.role != StorageRole::Backup
                || !storage.enabled
                || storage_filter.is_some_and(|id| id != storage.id)
            {
                continue;
            }
            let provider = self.providers.get(storage.id)?;
            let health = provider.health().await?;
            let objects = list_catalog_objects(self.catalog.as_ref(), storage.id).await?;
            for object in objects {
                let mut versions = list_versions(provider.as_ref(), &object.object_id).await?;
                mark_current_version(&object, &mut versions);
                let initial = evaluate_retention(
                    object.object_id.clone(),
                    &versions,
                    storage.retention_policy,
                    now,
                    health.capabilities.supports_version_history,
                    false,
                );
                let expected: BTreeSet<_> = initial
                    .cleanup_candidates
                    .iter()
                    .map(|candidate| {
                        cleanup_key(storage.id, &candidate.object_id, &candidate.version_id)
                    })
                    .collect();
                let mut approved = BTreeSet::new();
                for key in &expected {
                    if active_approval(self.catalog.as_ref(), storage.id, key, now).await? {
                        approved.insert(key.clone());
                    }
                }
                let evaluation = if !expected.is_empty() && approved == expected {
                    evaluate_retention(
                        object.object_id.clone(),
                        &versions,
                        storage.retention_policy,
                        now,
                        health.capabilities.supports_version_history,
                        true,
                    )
                } else {
                    initial
                };
                items.push(RetentionReviewItem {
                    storage_id: storage.id,
                    object_id: object.object_id,
                    evaluation,
                    approved_candidate_keys: approved,
                });
            }
        }
        items.sort_by(|left, right| {
            (left.storage_id, &left.object_id).cmp(&(right.storage_id, &right.object_id))
        });
        let review = RetentionReview {
            generated_at: now,
            items,
        };
        SettingsRepository::set(
            self.catalog.as_ref(),
            Setting {
                key: "backup.retention_review.v1".into(),
                value: retention_review_json(&review),
                updated_at: now,
            },
        )
        .await?;
        Ok(review)
    }

    /// Persists exact candidate approvals. No deletion operation is exposed by this service.
    pub async fn approve_retention_candidates(
        &self,
        review: &RetentionReview,
        candidate_keys: &BTreeSet<String>,
        approved_by: &str,
        now: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), BackupError> {
        if approved_by.trim().is_empty() {
            return Err(BackupError::InvalidRequest(
                "cleanup approval must identify the approver".into(),
            ));
        }
        let expected = review.candidate_keys();
        if candidate_keys != &expected {
            return Err(BackupError::InvalidRequest(
                "cleanup approval does not match the reviewed candidate set".into(),
            ));
        }
        let mut validated = Vec::with_capacity(candidate_keys.len());
        for key in candidate_keys {
            let (storage_id, _, _) = parse_cleanup_key(key)?;
            let storage = MusicStorageRepository::get(self.catalog.as_ref(), storage_id)
                .await?
                .ok_or(BackupError::StorageNotFound(storage_id))?;
            if storage.role != StorageRole::Backup || !storage.enabled {
                return Err(BackupError::InvalidRequest(format!(
                    "cleanup candidate storage {storage_id} is not an enabled backup"
                )));
            }
            validated.push((key, storage_id));
        }
        for (key, storage_id) in validated {
            self.catalog
                .put_cleanup_approval(PersistedCleanupApproval {
                    id: deterministic_uuid(&format!("cleanup-approval:{key}:{approved_by}:{now}")),
                    storage_id,
                    object_id: key.clone(),
                    reason: Some(format!("retention review approved by {approved_by}")),
                    approved_at: now,
                    expires_at,
                    consumed_at: None,
                })
                .await?;
        }
        Ok(())
    }

    pub async fn schedule_verification(
        &self,
        requests: impl IntoIterator<Item = VerificationRequest>,
        now: DateTime<Utc>,
        sample_modulus: u64,
    ) -> Result<Option<Uuid>, BackupError> {
        let mut payloads = Vec::new();
        for request in requests {
            if let Some(plan) = VerificationPlanner::schedule(&request, now, sample_modulus) {
                payloads.push(match plan {
                    VerificationPlan::Verify(payload) | VerificationPlan::Inventory(payload) => {
                        payload
                    }
                });
            }
        }
        self.persist_unique_payloads(SyncRunKind::BackupReplication, payloads, now)
            .await
    }

    pub async fn schedule_monthly_inventories(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>, BackupError> {
        let mut payloads = Vec::new();
        for storage in MusicStorageRepository::list(self.catalog.as_ref()).await? {
            if storage.role == StorageRole::Backup && storage.enabled {
                if let Some(VerificationPlan::Inventory(payload)) =
                    VerificationPlanner::monthly_inventory(
                        storage.id,
                        now,
                        storage.root_identifier.clone(),
                    )
                {
                    payloads.push(payload);
                }
            }
        }
        self.persist_unique_payloads(SyncRunKind::StorageInventory, payloads, now)
            .await
    }

    pub async fn handle_scheduler_hook(
        &self,
        hook: BackupSchedulerHook,
        worker_id: &str,
        now: DateTime<Utc>,
    ) -> Result<BackupSchedulerOutcome, BackupError> {
        match hook {
            BackupSchedulerHook::WorkerWake => {
                Ok(self.run_worker_once(worker_id, now).await?.map_or(
                    BackupSchedulerOutcome::Idle,
                    BackupSchedulerOutcome::JobEvent,
                ))
            }
            BackupSchedulerHook::VerifiedPrimaryWrite(mut request) => {
                request.trigger = ReplicationTrigger::VerifiedPrimaryWrite;
                Ok(BackupSchedulerOutcome::Replication(
                    self.plan_replication(request, now).await?,
                ))
            }
            BackupSchedulerHook::ScheduledReplication(mut request) => {
                request.trigger = ReplicationTrigger::Schedule;
                Ok(BackupSchedulerOutcome::Replication(
                    self.plan_replication(request, now).await?,
                ))
            }
            BackupSchedulerHook::Verification {
                requests,
                sample_modulus,
            } => Ok(BackupSchedulerOutcome::VerificationRun(
                self.schedule_verification(requests, now, sample_modulus)
                    .await?,
            )),
            BackupSchedulerHook::MonthlyInventory => Ok(BackupSchedulerOutcome::VerificationRun(
                self.schedule_monthly_inventories(now).await?,
            )),
            BackupSchedulerHook::RetentionReview => Ok(BackupSchedulerOutcome::Retention(
                self.review_retention(None, now).await?,
            )),
        }
    }

    async fn persist_unique_payloads(
        &self,
        kind: SyncRunKind,
        payloads: Vec<SyncJobPayload>,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>, BackupError> {
        let mut plan = JobPlanner::plan(kind, None, None, payloads, now);
        let mut unique = Vec::new();
        for job in plan.jobs {
            if self
                .catalog
                .find_active_job_by_idempotency_key(&job.idempotency_key)
                .await?
                .is_none()
            {
                unique.push(job);
            }
        }
        if unique.is_empty() {
            return Ok(None);
        }
        let run_id = plan.run.id;
        plan.jobs = unique;
        self.engine.persist_durable_plan(plan).await?;
        Ok(Some(run_id))
    }

    async fn reconcile_terminal_receipt(
        &self,
        event: &JobEvent,
        now: DateTime<Utc>,
    ) -> Result<(), BackupError> {
        let Some(job_id) = event.job_id else {
            return Ok(());
        };
        let Some(job) = self.engine.store().load_job(job_id).await? else {
            return Ok(());
        };
        if job.status != SyncStatus::Succeeded
            || self
                .catalog
                .get_transfer_result(&job.idempotency_key)
                .await?
                .is_some()
        {
            return Ok(());
        }
        let (asset_id, storage_id, object_id) = receipt_scope(&job);
        let Some(asset_id) = asset_id else {
            return Ok(());
        };
        let receipt = DurableTransferResult {
            idempotency_key: job.idempotency_key.clone(),
            job_id: job.id,
            asset_id,
            storage_id,
            object_id,
            bytes_transferred: job.progress.bytes_transferred,
            completed_at: job.finished_at.unwrap_or(now),
            metadata: json!({ "kind": format!("{:?}", job.kind) }),
        };
        self.catalog
            .save_transfer_result_and_terminal_job(receipt, job, event.clone())
            .await?;
        Ok(())
    }
}

struct BackupExecutor {
    catalog: Arc<TursoCatalog>,
    providers: Arc<BackupProviderRegistry>,
    staging_root: PathBuf,
}

impl JobExecutor for BackupExecutor {
    fn execute_step(&self, job: SyncJob) -> BoxFuture<'_, Result<JobStep, SyncError>> {
        Box::pin(async move {
            match job.payload.clone() {
                SyncJobPayload::ReplicateAsset {
                    asset_id,
                    source_storage_id,
                    source_object_id,
                    source_provider_version,
                    expected_checksum,
                    destination_storage_id,
                    destination_object_key,
                    ..
                } => {
                    let expected_checksum = expected_checksum.ok_or_else(|| {
                        sync_error(
                            SyncErrorKind::Integrity,
                            "backup.missing_checksum",
                            "replication source has no canonical SHA-256 checksum",
                            false,
                        )
                    })?;
                    self.execute_replication(
                        &job,
                        asset_id,
                        source_storage_id,
                        source_object_id,
                        source_provider_version,
                        expected_checksum,
                        destination_storage_id,
                        destination_object_key,
                    )
                    .await
                }
                SyncJobPayload::BackUpDatabase {
                    destination_storage_id,
                    destination_object_key,
                    ..
                } => {
                    self.execute_database_backup(
                        &job,
                        destination_storage_id,
                        destination_object_key,
                    )
                    .await
                }
                SyncJobPayload::VerifyAsset {
                    asset_id,
                    storage_id,
                    object_id,
                    expected_checksum,
                    ..
                } => {
                    self.execute_verification(
                        &job,
                        asset_id,
                        storage_id,
                        object_id,
                        expected_checksum,
                    )
                    .await
                }
                SyncJobPayload::InventoryStoragePage {
                    storage_id,
                    inventory_root,
                    page_cursor,
                    ..
                } => {
                    self.execute_inventory(&job, storage_id, inventory_root, page_cursor)
                        .await
                }
                payload => Err(sync_error(
                    SyncErrorKind::InvalidData,
                    "backup.unsupported_job",
                    format!("backup worker cannot execute {:?}", payload.kind()),
                    false,
                )),
            }
        })
    }
}

impl BackupExecutor {
    #[allow(clippy::too_many_arguments)]
    async fn execute_replication(
        &self,
        job: &SyncJob,
        asset_id: Uuid,
        source_storage_id: Uuid,
        source_object_id: String,
        source_provider_version: Option<String>,
        expected_checksum: ContentHash,
        destination_storage_id: Uuid,
        destination_object_key: String,
    ) -> Result<JobStep, SyncError> {
        let source = self
            .providers
            .get(source_storage_id)
            .map_err(backup_sync_error)?;
        let destination = self
            .providers
            .get(destination_storage_id)
            .map_err(backup_sync_error)?;
        let source_object = source
            .stat(&source_object_id)
            .await
            .map_err(|error| storage_sync_error("backup.source_stat", error))?;
        if source_object.provider_version != source_provider_version
            || source_object.canonical_hash() != Some(&expected_checksum)
        {
            return Err(sync_error(
                SyncErrorKind::Integrity,
                "backup.source_identity_changed",
                "replication source changed after the durable job was planned",
                false,
            ));
        }
        let path = staging_path(&self.staging_root, job.id, "payload");
        ensure_staging_parent(&path).await.map_err(io_sync_error)?;
        let transfer = checkpoint_for(
            job,
            expected_checksum.clone(),
            source_provider_version.clone(),
        )?;
        let executor = ReplicationExecutor::new(source.as_ref(), destination.as_ref());
        let request = StagedReplicationRequest {
            asset_id,
            source_object_id,
            destination_object_key,
            staging_path: path,
            expected_checksum: expected_checksum.clone(),
            expected_source_version: source_provider_version.clone(),
            idempotency_key: job.idempotency_key.clone(),
            content_type: source_object.media_type.clone(),
            content_length: Some(source_object.size_bytes),
        };
        let stage = replication_stage(transfer.stage);
        match executor
            .execute_step(&request, stage, transfer.destination_object_id.clone())
            .await
            .map_err(replication_sync_error)?
        {
            ReplicationStep::Checkpoint {
                stage,
                destination_object_id,
                bytes_transferred,
            } => checkpoint_step(
                job,
                transfer,
                transfer_stage(stage),
                &expected_checksum,
                source_provider_version.as_deref(),
                destination_object_id,
                bytes_transferred,
            ),
            ReplicationStep::Complete(execution) => {
                persist_verified_location(
                    self.catalog.as_ref(),
                    asset_id,
                    destination_storage_id,
                    execution.object,
                    expected_checksum,
                    Utc::now(),
                )
                .await
                .map_err(repository_sync_error)?;
                Ok(JobStep::Complete(JobCompletion::Succeeded {
                    bytes_transferred: execution.bytes_downloaded,
                }))
            }
        }
    }

    async fn execute_verification(
        &self,
        job: &SyncJob,
        asset_id: Uuid,
        storage_id: Uuid,
        object_id: String,
        expected_checksum: Option<String>,
    ) -> Result<JobStep, SyncError> {
        let provider = self.providers.get(storage_id).map_err(backup_sync_error)?;
        let object = provider
            .stat(&object_id)
            .await
            .map_err(|error| storage_sync_error("backup.verify_stat", error))?;
        let expected = expected_checksum
            .as_deref()
            .and_then(parse_expected_checksum)
            .or_else(|| object.canonical_hash().cloned())
            .ok_or_else(|| {
                sync_error(
                    SyncErrorKind::Integrity,
                    "backup.verify_missing_checksum",
                    "backup verification has no canonical SHA-256 checksum",
                    false,
                )
            })?;
        let verified = verify_destination(
            provider.as_ref(),
            &object_id,
            &expected,
            &staging_path(&self.staging_root, job.id, "verify"),
        )
        .await?;
        persist_verified_location(
            self.catalog.as_ref(),
            asset_id,
            storage_id,
            verified,
            expected,
            Utc::now(),
        )
        .await
        .map_err(repository_sync_error)?;
        Ok(JobStep::Complete(JobCompletion::Succeeded {
            bytes_transferred: 0,
        }))
    }

    async fn execute_inventory(
        &self,
        job: &SyncJob,
        storage_id: Uuid,
        inventory_root: Option<String>,
        payload_cursor: Option<rec_core::SyncCursor>,
    ) -> Result<JobStep, SyncError> {
        let provider = self.providers.get(storage_id).map_err(backup_sync_error)?;
        let page_cursor = job
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.page_cursor.clone())
            .or(payload_cursor);
        let page = provider
            .inventory_page(InventoryRequest {
                mode: InventoryMode::Full,
                prefix: inventory_root,
                page_cursor,
                page_size: Some(500),
                include_checksums: true,
            })
            .await
            .map_err(|error| storage_sync_error("backup.inventory", error))?;
        let item_count = page.objects.len() as u64;
        for object in page.objects {
            AssetRepository::upsert_storage_object(self.catalog.as_ref(), object)
                .await
                .map_err(repository_sync_error)?;
        }
        if let Some(next) = page.next_page_cursor {
            let mut progress = job.progress;
            progress.discover(item_count);
            for _ in 0..item_count {
                progress.record_success();
            }
            return Ok(JobStep::Checkpoint {
                checkpoint: SyncCheckpoint::at(progress.processed_items, Utc::now())
                    .with_page_cursor(next),
                progress,
                message: Some(format!("Inventoried {item_count} backup objects")),
            });
        }
        Ok(JobStep::Complete(JobCompletion::Succeeded {
            bytes_transferred: 0,
        }))
    }

    /// Copies the catalog database into staging, uploads it, then verifies the copy.
    ///
    /// The staged file is hashed once and that hash drives both the upload and the
    /// read-back, so a backup that arrives corrupted fails the job instead of passing as
    /// a usable restore point.
    async fn execute_database_backup(
        &self,
        job: &SyncJob,
        destination_storage_id: Uuid,
        destination_object_key: String,
    ) -> Result<JobStep, SyncError> {
        let destination = self
            .providers
            .get(destination_storage_id)
            .map_err(backup_sync_error)?;
        let path = staging_path(&self.staging_root, job.id, "library.db");
        ensure_staging_parent(&path).await.map_err(io_sync_error)?;

        let checkpoint = job
            .checkpoint
            .as_ref()
            .and_then(|value| value.transfer.clone());
        match checkpoint {
            // A retry before the copy landed re-copies: the live database has moved on,
            // so the previous attempt's checksum describes a file that no longer exists.
            None
            | Some(TransferCheckpoint {
                stage: TransferStage::PendingDownload,
                ..
            }) => {
                let bytes = self
                    .catalog
                    .copy_database_to(&path)
                    .await
                    .map_err(repository_sync_error)?;
                let hash = hash_file(&path).await?;
                checkpoint_step(
                    job,
                    TransferCheckpoint::new(hash.clone(), None),
                    TransferStage::DownloadVerified,
                    &hash,
                    None,
                    None,
                    bytes,
                )
            }
            Some(transfer) => {
                let expected = transfer.expected_checksum.clone();
                match transfer.stage {
                    TransferStage::PendingDownload => unreachable!("handled above"),
                    TransferStage::DownloadVerified => {
                        let length = tokio::fs::metadata(&path)
                            .await
                            .map_err(io_sync_error)?
                            .len();
                        let object = destination
                            .upload(UploadRequest {
                                source_path: path,
                                object_path: destination_object_key,
                                content_type: Some("application/vnd.sqlite3".into()),
                                content_length: Some(length),
                                expected_checksum: Some(expected.clone()),
                                metadata: BTreeMap::from([(
                                    "rec_bag.operation_key".into(),
                                    job.idempotency_key.clone(),
                                )]),
                                idempotency_key: Some(job.idempotency_key.clone()),
                            })
                            .await
                            .map_err(|error| storage_sync_error("database_backup.upload", error))?;
                        checkpoint_step(
                            job,
                            transfer,
                            TransferStage::Uploaded,
                            &expected,
                            None,
                            Some(object.object_id),
                            0,
                        )
                    }
                    TransferStage::Uploaded => {
                        let object_id =
                            transfer.destination_object_id.clone().ok_or_else(|| {
                                sync_error(
                                    SyncErrorKind::InvalidData,
                                    "database_backup.missing_object_id",
                                    "database backup checkpoint has no destination object ID",
                                    false,
                                )
                            })?;
                        verify_destination(
                            destination.as_ref(),
                            &object_id,
                            &expected,
                            &staging_path(&self.staging_root, job.id, "library.verify"),
                        )
                        .await?;
                        checkpoint_step(
                            job,
                            transfer,
                            TransferStage::DestinationVerified,
                            &expected,
                            None,
                            Some(object_id),
                            0,
                        )
                    }
                    TransferStage::DestinationVerified => {
                        Ok(JobStep::Complete(JobCompletion::Succeeded {
                            bytes_transferred: 0,
                        }))
                    }
                }
            }
        }
    }
}

impl IntegrationSliceHandler for BackupService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::Backup
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            if request.slice != IntegrationSlice::Backup {
                return Err(IntegrationError::for_request(
                    &request,
                    "request was routed to the wrong integration slice",
                    false,
                ));
            }
            let result: Result<IntegrationOutcome, BackupError> = match &request.event {
                MusicManagementEvent::SaveBackupPolicy(policy) => {
                    self.save_backup_policy(policy).await.map(|storage| IntegrationOutcome {
                        view: None,
                        message: Some(format!("Saved backup policy for {}", storage.name)),
                    })
                }
                MusicManagementEvent::StartCleanupReview => {
                    self.review_retention(None, Utc::now()).await.map(|review| {
                        let candidates = review.candidate_keys().len();
                        IntegrationOutcome {
                            view: None,
                            message: Some(format!(
                                "Retention review found {candidates} cleanup candidates; no files were deleted"
                            )),
                        }
                    })
                }
                MusicManagementEvent::ConfirmCleanup(keys) => {
                    let keys: BTreeSet<_> = keys.iter().cloned().collect();
                    match self.review_retention(None, Utc::now()).await {
                        Ok(review) => self
                            .approve_retention_candidates(
                                &review,
                                &keys,
                                "desktop-user",
                                Utc::now(),
                                None,
                            )
                            .await
                            .map(|()| IntegrationOutcome {
                                view: None,
                                message: Some(format!(
                                    "Approved {} exact retention candidates; permanent deletion remains disabled",
                                    keys.len()
                                )),
                            }),
                        Err(error) => Err(error),
                    }
                }
                MusicManagementEvent::AddBackup(provider) => Err(BackupError::InvalidRequest(
                    format!(
                        "{} backup configuration requires its provider setup flow",
                        provider_name(*provider)
                    ),
                )),
                _ => Err(BackupError::InvalidRequest(
                    "unsupported backup integration event".into(),
                )),
            };
            result.map_err(|error| {
                IntegrationError::for_request(&request, error.to_string(), error.retryable())
            })
        })
    }
}

impl BackupService {
    async fn save_backup_policy(
        &self,
        policy: &BackupPolicyView,
    ) -> Result<MusicStorage, BackupError> {
        if !policy.replicate_verified_only {
            return Err(BackupError::InvalidRequest(
                "backups may replicate only verified primary assets".into(),
            ));
        }
        let storage_id = Uuid::parse_str(&policy.target_id)
            .map_err(|_| BackupError::InvalidRequest("invalid backup storage ID".into()))?;
        let mut storage = MusicStorageRepository::get(self.catalog.as_ref(), storage_id)
            .await?
            .ok_or(BackupError::StorageNotFound(storage_id))?;
        if storage.role != StorageRole::Backup {
            return Err(BackupError::InvalidRequest(
                "retention policy can only be saved for a backup".into(),
            ));
        }
        storage.retention_policy = RetentionPolicy {
            retention_days: policy.retention_days.max(1),
            minimum_versions: policy.minimum_versions.max(1),
            require_cleanup_confirmation: true,
        };
        storage.updated_at = Utc::now();
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage.clone()).await?;
        Ok(storage)
    }
}

fn checkpoint_for(
    job: &SyncJob,
    expected_checksum: ContentHash,
    source_provider_version: Option<String>,
) -> Result<TransferCheckpoint, SyncError> {
    match job
        .checkpoint
        .as_ref()
        .and_then(|value| value.transfer.clone())
    {
        Some(checkpoint)
            if checkpoint.expected_checksum == expected_checksum
                && checkpoint.source_provider_version == source_provider_version =>
        {
            Ok(checkpoint)
        }
        Some(_) => Err(sync_error(
            SyncErrorKind::Integrity,
            "backup.checkpoint_identity_changed",
            "durable transfer checkpoint does not match the planned source identity",
            false,
        )),
        None => Ok(TransferCheckpoint::new(
            expected_checksum,
            source_provider_version,
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn checkpoint_step(
    job: &SyncJob,
    previous: TransferCheckpoint,
    stage: TransferStage,
    expected_checksum: &ContentHash,
    source_provider_version: Option<&str>,
    destination_object_id: Option<String>,
    bytes_transferred: u64,
) -> Result<JobStep, SyncError> {
    let transfer = previous
        .advance(
            stage,
            expected_checksum,
            source_provider_version,
            destination_object_id,
        )
        .map_err(|error| {
            sync_error(
                SyncErrorKind::Integrity,
                "backup.checkpoint_rejected",
                error.to_string(),
                false,
            )
        })?;
    let mut progress = job.progress;
    progress.discovered_items = progress.discovered_items.max(1);
    progress.total_items = Some(1);
    progress.add_bytes(bytes_transferred);
    let checkpoint = SyncCheckpoint::at(progress.processed_items, Utc::now())
        .with_last_item(format!("backup:{:?}", transfer.stage))
        .with_transfer(transfer);
    Ok(JobStep::Checkpoint {
        checkpoint,
        progress,
        message: Some(format!("Backup transfer reached {stage:?}")),
    })
}

async fn persist_verified_location(
    catalog: &TursoCatalog,
    asset_id: Uuid,
    storage_id: Uuid,
    mut object: StorageObject,
    checksum: ContentHash,
    now: DateTime<Utc>,
) -> Result<(), RepositoryError> {
    object.storage_id = storage_id;
    AssetRepository::upsert_storage_object(catalog, object.clone()).await?;
    let existing =
        AssetRepository::get_location(catalog, storage_id, object.object_id.clone()).await?;
    AssetRepository::upsert_location(
        catalog,
        AssetLocation {
            id: existing
                .as_ref()
                .map_or_else(Uuid::new_v4, |value| value.id),
            asset_id,
            asset_type: existing.map_or(AssetType::Original, |value| value.asset_type),
            storage_id,
            object_id: object.object_id,
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: Some(checksum.clone()),
            observed_hash: Some(checksum),
            provider_version: object.provider_version,
            last_checked_at: Some(now),
            verified_at: Some(now),
            error_message: None,
        },
    )
    .await
}

async fn verify_destination(
    destination: &dyn MusicAssetStore,
    object_id: &str,
    expected: &ContentHash,
    verification_path: &Path,
) -> Result<StorageObject, SyncError> {
    let object = destination
        .stat(object_id)
        .await
        .map_err(|error| storage_sync_error("backup.destination_stat", error))?;
    if object.canonical_hash() == Some(expected) {
        return Ok(object);
    }
    ensure_staging_parent(verification_path)
        .await
        .map_err(io_sync_error)?;
    let result = destination
        .download(DownloadRequest {
            object_id: object_id.into(),
            destination_path: verification_path.to_path_buf(),
            range: None,
            expected_checksum: Some(expected.clone()),
            overwrite: true,
        })
        .await
        .map_err(|error| storage_sync_error("backup.destination_download", error))?;
    if !result.checksum_verified || result.checksum.as_ref() != Some(expected) {
        return Err(sync_error(
            SyncErrorKind::Integrity,
            "backup.destination_verification_failed",
            "destination did not verify the expected canonical checksum",
            false,
        ));
    }
    Ok(object)
}

async fn list_original_assets(
    catalog: &TursoCatalog,
) -> Result<Vec<OriginalAsset>, RepositoryError> {
    let mut cursor = None;
    let mut originals = Vec::new();
    loop {
        let page = AssetRepository::list_originals(
            catalog,
            OriginalAssetFilter { track_id: None },
            RepositoryPageRequest {
                cursor,
                limit: Some(500),
            },
        )
        .await?;
        originals.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    originals.sort_by_key(OriginalAsset::id);
    Ok(originals)
}

async fn resolve_verified_primary_asset(
    catalog: &TursoCatalog,
    source_provider: &dyn MusicAssetStore,
    primary: &MusicStorage,
    original: &OriginalAsset,
) -> Result<Option<(StorageObject, AssetLocation)>, BackupError> {
    let Some(expected_hash) = original.content_hash.as_ref() else {
        return Ok(None);
    };
    let location = AssetRepository::list_locations_for_asset(catalog, original.id())
        .await?
        .into_iter()
        .find(|location| {
            location.storage_id == primary.id
                && location.asset_id == original.id()
                && location.asset_type == AssetType::Original
                && location.verification_status == StorageVerificationStatus::Verified
                && location.expected_hash.as_ref() == Some(expected_hash)
                && location.observed_hash.as_ref() == Some(expected_hash)
        });
    let Some(location) = location else {
        return Ok(None);
    };
    let catalog_object =
        AssetRepository::get_storage_object(catalog, primary.id, location.object_id.clone())
            .await?
            .ok_or_else(|| {
                BackupError::InvalidRequest(format!(
                    "verified primary location {} has no catalog object",
                    location.object_id
                ))
            })?;
    let live_object = source_provider.stat(&location.object_id).await?;
    if live_object.storage_id != primary.id
        || live_object.object_id != location.object_id
        || live_object.is_trashed
        || live_object.canonical_hash() != Some(expected_hash)
        || live_object.provider_version != location.provider_version
        || catalog_object.provider_version != location.provider_version
        || catalog_object.canonical_hash() != Some(expected_hash)
    {
        return Err(BackupError::InvalidRequest(format!(
            "verified primary object {} changed before backup handoff",
            location.object_id
        )));
    }
    Ok(Some((live_object, location)))
}

fn backup_destination_key(original: &OriginalAsset) -> String {
    let file_name: String = original
        .file_name
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect();
    let file_name = file_name.trim().trim_matches('.');
    let file_name = if file_name.is_empty() {
        "asset.bin"
    } else {
        file_name
    };
    format!("assets/{}/{file_name}", original.id())
}

async fn list_catalog_objects(
    catalog: &TursoCatalog,
    storage_id: Uuid,
) -> Result<Vec<StorageObject>, RepositoryError> {
    let mut cursor = None;
    let mut objects = Vec::new();
    loop {
        let page = AssetRepository::list_storage_objects(
            catalog,
            storage_id,
            RepositoryPageRequest {
                cursor,
                limit: Some(500),
            },
        )
        .await?;
        objects.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(objects)
}

async fn list_versions(
    provider: &dyn MusicAssetStore,
    object_id: &str,
) -> Result<Vec<StorageObjectVersion>, MusicStorageError> {
    let mut cursor = None;
    let mut versions = Vec::new();
    loop {
        let page = provider
            .versions_page(StorageVersionRequest {
                object_id: object_id.into(),
                page_cursor: cursor,
                page_size: Some(500),
            })
            .await?;
        versions.extend(page.versions);
        match page.next_page_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(versions)
}

fn mark_current_version(object: &StorageObject, versions: &mut [StorageObjectVersion]) {
    if versions.iter().any(|version| version.is_current) {
        return;
    }
    if let Some(current) = object.provider_version.as_deref() {
        if let Some(version) = versions
            .iter_mut()
            .find(|version| version.version_id == current)
        {
            version.is_current = true;
            return;
        }
    }
    if let Some(version) = versions
        .iter_mut()
        .max_by_key(|version| version.modified_at)
    {
        version.is_current = true;
    }
}

async fn active_approval(
    catalog: &TursoCatalog,
    storage_id: Uuid,
    key: &str,
    now: DateTime<Utc>,
) -> Result<bool, RepositoryError> {
    Ok(catalog
        .get_cleanup_approval(storage_id, key)
        .await?
        .is_some_and(|approval| {
            approval.consumed_at.is_none()
                && approval
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now)
        }))
}

fn receipt_scope(job: &SyncJob) -> (Option<Uuid>, Option<Uuid>, Option<String>) {
    match &job.payload {
        SyncJobPayload::ReplicateAsset {
            asset_id,
            destination_storage_id,
            ..
        } => (
            Some(*asset_id),
            Some(*destination_storage_id),
            job.checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.transfer.as_ref())
                .and_then(|transfer| transfer.destination_object_id.clone()),
        ),
        _ => (None, None, None),
    }
}

fn cleanup_key(storage_id: Uuid, object_id: &str, version_id: &str) -> String {
    format!("{storage_id}{CLEANUP_KEY_SEPARATOR}{object_id}:{version_id}")
}

fn parse_cleanup_key(key: &str) -> Result<(Uuid, String, String), BackupError> {
    let (storage, candidate) = key.split_once(CLEANUP_KEY_SEPARATOR).ok_or_else(|| {
        BackupError::InvalidRequest(format!("invalid cleanup candidate key: {key}"))
    })?;
    let (object, version) = candidate.rsplit_once(':').ok_or_else(|| {
        BackupError::InvalidRequest(format!("invalid cleanup candidate key: {key}"))
    })?;
    let storage_id = Uuid::parse_str(storage).map_err(|_| {
        BackupError::InvalidRequest(format!("invalid cleanup candidate storage: {key}"))
    })?;
    Ok((storage_id, object.into(), version.into()))
}

fn retention_review_json(review: &RetentionReview) -> serde_json::Value {
    json!({
        "generated_at": review.generated_at,
        "items": review.items.iter().map(|item| json!({
            "storage_id": item.storage_id,
            "object_id": item.object_id,
            "status": format!("{:?}", item.evaluation.status),
            "retained_version_ids": item.evaluation.retained_version_ids,
            "cleanup_candidates": item.evaluation.cleanup_candidates.iter().map(|candidate| json!({
                "object_id": candidate.object_id,
                "version_id": candidate.version_id,
                "modified_at": candidate.modified_at,
                "reason": candidate.reason,
            })).collect::<Vec<_>>(),
            "approved_candidate_keys": item.approved_candidate_keys,
        })).collect::<Vec<_>>(),
    })
}

fn parse_expected_checksum(value: &str) -> Option<ContentHash> {
    let digest = value.strip_prefix("sha256:").unwrap_or(value);
    ContentHash::sha256(digest).ok()
}

fn deterministic_uuid(value: &str) -> Uuid {
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn staging_path(root: &Path, job_id: Uuid, name: &str) -> PathBuf {
    root.join(job_id.to_string()).join(name)
}

async fn ensure_staging_parent(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

fn replication_stage(stage: TransferStage) -> ReplicationStage {
    match stage {
        TransferStage::PendingDownload => ReplicationStage::PendingDownload,
        TransferStage::DownloadVerified => ReplicationStage::DownloadVerified,
        TransferStage::Uploaded => ReplicationStage::Uploaded,
        TransferStage::DestinationVerified => ReplicationStage::DestinationVerified,
    }
}

fn transfer_stage(stage: ReplicationStage) -> TransferStage {
    match stage {
        ReplicationStage::PendingDownload => TransferStage::PendingDownload,
        ReplicationStage::DownloadVerified => TransferStage::DownloadVerified,
        ReplicationStage::Uploaded => TransferStage::Uploaded,
        ReplicationStage::DestinationVerified => TransferStage::DestinationVerified,
    }
}

fn sync_error(
    kind: SyncErrorKind,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> SyncError {
    SyncError::new(kind, code, message, retryable)
}

fn backup_sync_error(error: BackupError) -> SyncError {
    sync_error(
        if error.retryable() {
            SyncErrorKind::TemporarilyUnavailable
        } else {
            SyncErrorKind::Provider
        },
        "backup.runtime",
        error.to_string(),
        error.retryable(),
    )
}

fn repository_sync_error(error: RepositoryError) -> SyncError {
    let retryable = matches!(
        error.kind,
        RepositoryErrorKind::TemporarilyUnavailable | RepositoryErrorKind::Io
    );
    sync_error(
        if retryable {
            SyncErrorKind::TemporarilyUnavailable
        } else {
            SyncErrorKind::Internal
        },
        "backup.persistence",
        error.message,
        retryable,
    )
}

fn io_sync_error(error: std::io::Error) -> SyncError {
    sync_error(
        SyncErrorKind::Io,
        "backup.staging_io",
        error.to_string(),
        true,
    )
}

fn replication_sync_error(error: ReplicationError) -> SyncError {
    match error {
        ReplicationError::Storage { stage, source } => {
            storage_sync_error(&format!("backup.{stage}"), source)
        }
        ReplicationError::MissingChecksum => sync_error(
            SyncErrorKind::Integrity,
            "backup.missing_checksum",
            error.to_string(),
            false,
        ),
        ReplicationError::Verification(_) | ReplicationError::SourceVersionChanged => sync_error(
            SyncErrorKind::Integrity,
            "backup.integrity",
            error.to_string(),
            false,
        ),
        ReplicationError::MissingDestinationObject(_) => sync_error(
            SyncErrorKind::InvalidData,
            "backup.checkpoint",
            error.to_string(),
            false,
        ),
    }
}

fn storage_sync_error(code: &str, error: MusicStorageError) -> SyncError {
    let kind = match error.kind {
        MusicStorageErrorKind::AuthenticationRequired => SyncErrorKind::Authentication,
        MusicStorageErrorKind::PermissionDenied | MusicStorageErrorKind::DownloadProhibited => {
            SyncErrorKind::Authorization
        }
        MusicStorageErrorKind::RateLimited => SyncErrorKind::RateLimited,
        MusicStorageErrorKind::TemporarilyUnavailable => SyncErrorKind::TemporarilyUnavailable,
        MusicStorageErrorKind::NotFound => SyncErrorKind::NotFound,
        MusicStorageErrorKind::InvalidData => SyncErrorKind::InvalidData,
        MusicStorageErrorKind::ChecksumMismatch => SyncErrorKind::Integrity,
        MusicStorageErrorKind::Io => SyncErrorKind::Io,
        MusicStorageErrorKind::Provider | MusicStorageErrorKind::Permanent => {
            SyncErrorKind::Provider
        }
    };
    let retryable = error.is_retryable();
    sync_error(kind, code, error.message, retryable)
}

fn provider_name(provider: ProviderChoice) -> &'static str {
    match provider {
        ProviderChoice::LocalFolder => "Local folder",
        ProviderChoice::GoogleDrive => "Google Drive",
        ProviderChoice::SoundCloud => "SoundCloud",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{
        CanonicalTrack, HashAlgorithm, InventoryPage, InventoryRequest, MusicStorageKind,
        OriginalAsset, OriginalAssetRole, StorageCapabilities, StorageHealth, StorageVersionPage,
        TrackRepository,
    };
    use std::{fs, sync::Mutex as StdMutex};

    struct FakeStore {
        storage_id: Uuid,
        objects: StdMutex<BTreeMap<String, Vec<u8>>>,
        fail_uploads: StdMutex<u32>,
        version: String,
    }

    impl FakeStore {
        fn new(storage_id: Uuid) -> Self {
            Self {
                storage_id,
                objects: StdMutex::new(BTreeMap::new()),
                fail_uploads: StdMutex::new(0),
                version: "v1".into(),
            }
        }

        fn with_object(self, id: &str, bytes: &[u8]) -> Self {
            self.objects
                .lock()
                .unwrap()
                .insert(id.into(), bytes.to_vec());
            self
        }

        fn fail_next_upload(&self) {
            *self.fail_uploads.lock().unwrap() += 1;
        }

        fn object(&self, id: &str) -> StorageObject {
            let objects = self.objects.lock().unwrap();
            let bytes = objects.get(id).expect("fake object exists");
            StorageObject {
                storage_id: self.storage_id,
                object_id: id.into(),
                name: id.into(),
                display_path: Some(id.into()),
                size_bytes: bytes.len() as u64,
                media_type: Some("audio/flac".into()),
                modified_at: None,
                provider_version: Some(self.version.clone()),
                hashes: vec![hash_bytes(bytes)],
                embedded_asset_id: None,
                is_trashed: false,
            }
        }
    }

    impl MusicAssetStore for FakeStore {
        fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
            Box::pin(async move {
                Ok(StorageHealth {
                    status: HealthStatus::Healthy,
                    checked_at: Utc::now(),
                    message: None,
                    capabilities: StorageCapabilities {
                        can_inventory: true,
                        can_stat: true,
                        can_upload: true,
                        can_download: true,
                        can_create_containers: true,
                        supports_atomic_replace: true,
                        supports_change_tracking: true,
                        supports_version_history: true,
                        provider_hash_algorithms: vec![HashAlgorithm::Sha256],
                    },
                })
            })
        }

        fn inventory_page(
            &self,
            _request: InventoryRequest,
        ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
            Box::pin(async move {
                Ok(InventoryPage {
                    objects: Vec::new(),
                    removals: Vec::new(),
                    next_page_cursor: None,
                    next_change_cursor: None,
                    progress: None,
                })
            })
        }

        fn stat<'a>(
            &'a self,
            object_id: &'a str,
        ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
            Box::pin(async move {
                if !self.objects.lock().unwrap().contains_key(object_id) {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::NotFound,
                        "fake object missing",
                    ));
                }
                Ok(self.object(object_id))
            })
        }

        fn versions_page(
            &self,
            request: StorageVersionRequest,
        ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
            Box::pin(async move {
                let object = self.object(&request.object_id);
                Ok(StorageVersionPage {
                    versions: vec![StorageObjectVersion {
                        object_id: object.object_id,
                        version_id: self.version.clone(),
                        modified_at: object.modified_at,
                        size_bytes: Some(object.size_bytes),
                        hashes: object.hashes,
                        is_current: true,
                    }],
                    next_page_cursor: None,
                })
            })
        }

        fn upload(
            &self,
            request: UploadRequest,
        ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
            Box::pin(async move {
                let mut failures = self.fail_uploads.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::TemporarilyUnavailable,
                        "injected upload failure",
                    ));
                }
                drop(failures);
                let bytes = fs::read(&request.source_path).map_err(|error| {
                    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
                })?;
                self.objects
                    .lock()
                    .unwrap()
                    .insert(request.object_path.clone(), bytes);
                Ok(self.object(&request.object_path))
            })
        }

        fn download(
            &self,
            request: DownloadRequest,
        ) -> BoxFuture<'_, Result<rec_core::DownloadResult, MusicStorageError>> {
            Box::pin(async move {
                let object = self.object(&request.object_id);
                let bytes = self.objects.lock().unwrap()[&request.object_id].clone();
                if let Some(parent) = request.destination_path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
                    })?;
                }
                fs::write(&request.destination_path, &bytes).map_err(|error| {
                    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
                })?;
                let checksum = hash_bytes(&bytes);
                if request
                    .expected_checksum
                    .as_ref()
                    .is_some_and(|expected| expected != &checksum)
                {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::ChecksumMismatch,
                        "fake checksum mismatch",
                    ));
                }
                Ok(rec_core::DownloadResult {
                    object,
                    destination_path: request.destination_path,
                    bytes_written: bytes.len() as u64,
                    checksum: Some(checksum),
                    checksum_verified: request.expected_checksum.is_some(),
                })
            })
        }
    }

    fn hash_bytes(bytes: &[u8]) -> ContentHash {
        ContentHash::sha256(format!("{:x}", Sha256::digest(bytes))).unwrap()
    }

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rec-bag-backup-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn configured_storage(id: Uuid, kind: MusicStorageKind, role: StorageRole) -> MusicStorage {
        let mut storage = MusicStorage::new(
            format!("{kind:?}"),
            kind,
            role,
            StorageCapabilities {
                can_inventory: true,
                can_stat: true,
                can_upload: true,
                can_download: true,
                can_create_containers: true,
                supports_atomic_replace: true,
                supports_change_tracking: true,
                supports_version_history: true,
                provider_hash_algorithms: vec![HashAlgorithm::Sha256],
            },
        );
        storage.id = id;
        storage
    }

    async fn persist_verified_primary_asset(
        catalog: &TursoCatalog,
        source: &FakeStore,
        primary_id: Uuid,
        object_id: &str,
    ) -> Uuid {
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog, track.clone())
            .await
            .unwrap();
        let object = source.object(object_id);
        let checksum = object.canonical_hash().unwrap().clone();
        let mut original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            object_id,
            object.size_bytes,
            checksum.value(),
        )
        .unwrap();
        original.media_type = object.media_type.clone();
        let asset_id = original.id();
        AssetRepository::upsert_original(catalog, original)
            .await
            .unwrap();
        AssetRepository::upsert_storage_object(catalog, object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog,
            AssetLocation {
                id: Uuid::new_v4(),
                asset_id,
                asset_type: AssetType::Original,
                storage_id: primary_id,
                object_id: object.object_id,
                verification_status: StorageVerificationStatus::Verified,
                expected_hash: Some(checksum.clone()),
                observed_hash: Some(checksum),
                provider_version: object.provider_version,
                last_checked_at: Some(Utc::now()),
                verified_at: Some(Utc::now()),
                error_message: None,
            },
        )
        .await
        .unwrap();
        asset_id
    }

    async fn persist_handoff_storages(
        catalog: &TursoCatalog,
        primary: MusicStorage,
        backup: Option<MusicStorage>,
    ) {
        let primary_id = primary.id;
        MusicStorageRepository::upsert(catalog, primary)
            .await
            .unwrap();
        if let Some(backup) = backup {
            MusicStorageRepository::upsert(catalog, backup)
                .await
                .unwrap();
        }
        MusicStorageRepository::switch_active_primary(catalog, None, Some(primary_id))
            .await
            .unwrap();
    }

    async fn execute_matrix_case(
        source_kind: MusicStorageKind,
        destination_kind: MusicStorageKind,
    ) {
        let source_id = Uuid::new_v4();
        let destination_id = Uuid::new_v4();
        let bytes = b"portable transfer matrix";
        let source = Arc::new(FakeStore::new(source_id).with_object("source.flac", bytes));
        let destination = Arc::new(FakeStore::new(destination_id));
        let request = StagedReplicationRequest {
            asset_id: Uuid::new_v4(),
            source_object_id: "source.flac".into(),
            destination_object_key: "backup.flac".into(),
            staging_path: temp_root("matrix").join("payload"),
            expected_checksum: hash_bytes(bytes),
            expected_source_version: Some("v1".into()),
            idempotency_key: format!("{source_kind:?}-{destination_kind:?}"),
            content_type: Some("audio/flac".into()),
            content_length: Some(bytes.len() as u64),
        };
        ReplicationExecutor::new(source.as_ref(), destination.as_ref())
            .replicate(&request)
            .await
            .unwrap();
        assert_eq!(destination.objects.lock().unwrap()["backup.flac"], bytes);
        let _ = fs::remove_dir_all(request.staging_path.parent().unwrap());
    }

    #[tokio::test]
    async fn generic_path_supports_the_complete_local_drive_transfer_matrix() {
        execute_matrix_case(
            MusicStorageKind::LocalDirectory,
            MusicStorageKind::GoogleDrive,
        )
        .await;
        execute_matrix_case(
            MusicStorageKind::GoogleDrive,
            MusicStorageKind::LocalDirectory,
        )
        .await;
        execute_matrix_case(
            MusicStorageKind::LocalDirectory,
            MusicStorageKind::LocalDirectory,
        )
        .await;
        execute_matrix_case(MusicStorageKind::GoogleDrive, MusicStorageKind::GoogleDrive).await;
    }

    #[tokio::test]
    async fn catalog_handoff_plans_one_specified_verified_asset() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let primary_id = Uuid::new_v4();
        let backup_id = Uuid::new_v4();
        let source = Arc::new(
            FakeStore::new(primary_id)
                .with_object("one.flac", b"one")
                .with_object("two.flac", b"two"),
        );
        providers.register(primary_id, source.clone());
        providers.register(backup_id, Arc::new(FakeStore::new(backup_id)));
        persist_handoff_storages(
            catalog.as_ref(),
            configured_storage(
                primary_id,
                MusicStorageKind::LocalDirectory,
                StorageRole::Primary,
            ),
            Some(configured_storage(
                backup_id,
                MusicStorageKind::GoogleDrive,
                StorageRole::Backup,
            )),
        )
        .await;
        let first = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "one.flac",
        )
        .await;
        let _second = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "two.flac",
        )
        .await;
        let service = BackupService::new(catalog.clone(), providers, temp_root("handoff-one"));

        let handoff = service
            .handoff_verified_primary_assets(Some(first), Utc::now())
            .await
            .unwrap();

        assert_eq!(handoff.considered_assets, 1);
        assert_eq!(handoff.queued_jobs, 1);
        assert_eq!(handoff.deferred_targets, 0);
        assert_eq!(handoff.assets[0].asset_id, first);
        let jobs = service
            .engine
            .store()
            .list_run_jobs(handoff.assets[0].run_id.unwrap())
            .await
            .unwrap();
        assert!(matches!(
            jobs[0].payload,
            SyncJobPayload::ReplicateAsset { asset_id, ref destination_object_key, .. }
                if asset_id == first
                    && destination_object_key == &format!("assets/{first}/one.flac")
        ));
    }

    #[tokio::test]
    async fn catalog_handoff_plans_all_verified_primary_assets() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let primary_id = Uuid::new_v4();
        let backup_id = Uuid::new_v4();
        let source = Arc::new(
            FakeStore::new(primary_id)
                .with_object("one.flac", b"one")
                .with_object("two.flac", b"two"),
        );
        providers.register(primary_id, source.clone());
        providers.register(backup_id, Arc::new(FakeStore::new(backup_id)));
        persist_handoff_storages(
            catalog.as_ref(),
            configured_storage(
                primary_id,
                MusicStorageKind::LocalDirectory,
                StorageRole::Primary,
            ),
            Some(configured_storage(
                backup_id,
                MusicStorageKind::GoogleDrive,
                StorageRole::Backup,
            )),
        )
        .await;
        let first = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "one.flac",
        )
        .await;
        let second = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "two.flac",
        )
        .await;
        let service = BackupService::new(catalog, providers, temp_root("handoff-all"));

        let handoff = service
            .handoff_verified_primary_assets(None, Utc::now())
            .await
            .unwrap();

        assert_eq!(handoff.considered_assets, 2);
        assert_eq!(handoff.queued_jobs, 2);
        assert_eq!(
            handoff
                .assets
                .iter()
                .map(|asset| asset.asset_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([first, second])
        );
    }

    #[tokio::test]
    async fn catalog_handoff_with_no_backups_is_a_successful_noop() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let primary_id = Uuid::new_v4();
        let source = Arc::new(FakeStore::new(primary_id).with_object("one.flac", b"one"));
        providers.register(primary_id, source.clone());
        persist_handoff_storages(
            catalog.as_ref(),
            configured_storage(
                primary_id,
                MusicStorageKind::LocalDirectory,
                StorageRole::Primary,
            ),
            None,
        )
        .await;
        let asset_id = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "one.flac",
        )
        .await;
        let service = BackupService::new(catalog, providers, temp_root("handoff-none"));

        let handoff = service
            .handoff_verified_primary_assets(Some(asset_id), Utc::now())
            .await
            .unwrap();

        assert_eq!(handoff.considered_assets, 1);
        assert_eq!(handoff.queued_jobs, 0);
        assert_eq!(handoff.deferred_targets, 0);
        assert!(handoff.assets[0].decisions.is_empty());
        assert!(handoff.assets[0].run_id.is_none());
    }

    #[tokio::test]
    async fn repeated_catalog_handoff_deduplicates_active_replication() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let primary_id = Uuid::new_v4();
        let backup_id = Uuid::new_v4();
        let source = Arc::new(FakeStore::new(primary_id).with_object("one.flac", b"one"));
        providers.register(primary_id, source.clone());
        providers.register(backup_id, Arc::new(FakeStore::new(backup_id)));
        persist_handoff_storages(
            catalog.as_ref(),
            configured_storage(
                primary_id,
                MusicStorageKind::LocalDirectory,
                StorageRole::Primary,
            ),
            Some(configured_storage(
                backup_id,
                MusicStorageKind::GoogleDrive,
                StorageRole::Backup,
            )),
        )
        .await;
        let asset_id = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "one.flac",
        )
        .await;
        let service = BackupService::new(catalog, providers, temp_root("handoff-dedup"));

        let first = service
            .handoff_verified_primary_assets(Some(asset_id), Utc::now())
            .await
            .unwrap();
        let repeated = service
            .handoff_verified_primary_assets(Some(asset_id), Utc::now())
            .await
            .unwrap();

        assert_eq!(first.queued_jobs, 1);
        assert_eq!(repeated.considered_assets, 1);
        assert_eq!(repeated.queued_jobs, 0);
        assert!(repeated.assets[0].run_id.is_none());
        assert!(matches!(
            repeated.assets[0].decisions.as_slice(),
            [ReplicationDecision::Enqueue(_)]
        ));
    }

    #[tokio::test]
    async fn catalog_handoff_queues_unavailable_backup_as_deferred() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let primary_id = Uuid::new_v4();
        let backup_id = Uuid::new_v4();
        let source = Arc::new(FakeStore::new(primary_id).with_object("one.flac", b"one"));
        providers.register(primary_id, source.clone());
        persist_handoff_storages(
            catalog.as_ref(),
            configured_storage(
                primary_id,
                MusicStorageKind::LocalDirectory,
                StorageRole::Primary,
            ),
            Some(configured_storage(
                backup_id,
                MusicStorageKind::GoogleDrive,
                StorageRole::Backup,
            )),
        )
        .await;
        let asset_id = persist_verified_primary_asset(
            catalog.as_ref(),
            source.as_ref(),
            primary_id,
            "one.flac",
        )
        .await;
        let service = BackupService::new(catalog, providers, temp_root("handoff-deferred"));

        let handoff = service
            .handoff_verified_primary_assets(Some(asset_id), Utc::now())
            .await
            .unwrap();

        assert_eq!(handoff.queued_jobs, 1);
        assert_eq!(handoff.deferred_targets, 1);
        assert!(matches!(
            handoff.assets[0].decisions.as_slice(),
            [ReplicationDecision::DeferredUnavailable { backup_storage_id }]
                if *backup_storage_id == backup_id
        ));
        let jobs = service
            .engine
            .store()
            .list_run_jobs(handoff.assets[0].run_id.unwrap())
            .await
            .unwrap();
        assert!(jobs[0].next_attempt_at.is_some());
    }

    #[tokio::test]
    async fn durable_worker_restarts_after_upload_failure_without_repeating_download() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let providers = Arc::new(BackupProviderRegistry::default());
        let source_id = Uuid::new_v4();
        let destination_id = Uuid::new_v4();
        let bytes = b"restartable";
        let source = Arc::new(FakeStore::new(source_id).with_object("source.flac", bytes));
        let destination = Arc::new(FakeStore::new(destination_id));
        destination.fail_next_upload();
        providers.register(source_id, source.clone());
        providers.register(destination_id, destination.clone());
        let service = BackupService::new(catalog.clone(), providers, temp_root("restart"));
        let asset_id = Uuid::new_v4();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let checksum = hash_bytes(bytes);
        let mut original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "source.flac",
            bytes.len() as u64,
            checksum.value(),
        )
        .unwrap();
        original.record.id = asset_id;
        AssetRepository::upsert_original(catalog.as_ref(), original)
            .await
            .unwrap();
        let source_storage = configured_storage(
            source_id,
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
        );
        let backup = configured_storage(
            destination_id,
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
        );
        MusicStorageRepository::upsert(catalog.as_ref(), source_storage.clone())
            .await
            .unwrap();
        MusicStorageRepository::upsert(catalog.as_ref(), backup.clone())
            .await
            .unwrap();
        let source_object = source.object("source.flac");
        let request = ReplicationRequest {
            asset_id,
            source_storage,
            source_object: source_object.clone(),
            source_location: AssetLocation {
                id: Uuid::new_v4(),
                asset_id,
                asset_type: AssetType::Original,
                storage_id: source_id,
                object_id: source_object.object_id.clone(),
                verification_status: StorageVerificationStatus::Verified,
                expected_hash: Some(checksum.clone()),
                observed_hash: Some(checksum),
                provider_version: Some("v1".into()),
                last_checked_at: None,
                verified_at: None,
                error_message: None,
            },
            destination_object_key: "backup.flac".into(),
            trigger: ReplicationTrigger::Manual,
            backups: vec![BackupTarget {
                storage: backup,
                availability: BackupAvailability::Available,
                existing_location: None,
            }],
        };
        let planned = service.plan_replication(request, Utc::now()).await.unwrap();
        let run_id = planned.run_id.unwrap();
        let first = service
            .run_worker_once("worker", Utc::now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.kind, JobEventKind::Retrying);
        let jobs = service.engine.store().list_run_jobs(run_id).await.unwrap();
        assert_eq!(
            jobs[0]
                .checkpoint
                .as_ref()
                .and_then(|value| value.transfer.as_ref())
                .map(|value| value.stage),
            Some(TransferStage::DownloadVerified)
        );
        let retry_at = jobs[0].next_attempt_at.unwrap();
        let second = service
            .run_worker_once("worker", retry_at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.kind, JobEventKind::Succeeded);
        assert_eq!(destination.objects.lock().unwrap()["backup.flac"], bytes);
        let terminal = service.engine.store().list_run_jobs(run_id).await.unwrap();
        assert_eq!(terminal[0].status, SyncStatus::Succeeded);
        assert!(catalog
            .get_transfer_result(&terminal[0].idempotency_key)
            .await
            .unwrap()
            .is_some());
    }
}
