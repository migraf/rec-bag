//! Primary-to-backup replication planning and portable staged transfer.

use chrono::{DateTime, Utc};
use rec_core::{
    AssetLocation, ContentHash, DownloadRequest, DownloadResult, MusicAssetStore, MusicStorage,
    MusicStorageError, StorageObject, StorageRole, StorageVerificationStatus, SyncJob,
    SyncJobPayload, UploadRequest, SYNC_JOB_PAYLOAD_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationTrigger {
    VerifiedPrimaryWrite,
    Schedule,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupAvailability {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupTarget {
    pub storage: MusicStorage,
    pub availability: BackupAvailability,
    /// The current association, if the backup already contains this asset.
    pub existing_location: Option<AssetLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationRequest {
    pub asset_id: Uuid,
    pub source_storage: MusicStorage,
    pub source_object: StorageObject,
    pub source_location: AssetLocation,
    pub destination_object_key: String,
    pub trigger: ReplicationTrigger,
    pub backups: Vec<BackupTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationJobPlan {
    pub backup_storage_id: Uuid,
    pub payload: SyncJobPayload,
    /// Stable across a restarted transfer of the same source version.
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicationDecision {
    Enqueue(ReplicationJobPlan),
    AlreadyVerified {
        backup_storage_id: Uuid,
    },
    DeferredUnavailable {
        backup_storage_id: Uuid,
    },
    BlockedPendingApproval {
        primary_location_id: Uuid,
    },
    Skipped {
        backup_storage_id: Uuid,
        reason: String,
    },
}

pub struct ReplicationPlanner;

impl ReplicationPlanner {
    /// Materializes every enqueue/deferred decision as an independent durable job.
    /// Deferred jobs retain their idempotency key and become claimable at
    /// `deferred_until`; one target never blocks another.
    pub fn durable_jobs(
        request: &ReplicationRequest,
        run_id: Uuid,
        created_at: DateTime<Utc>,
        deferred_until: DateTime<Utc>,
    ) -> Vec<SyncJob> {
        if !source_is_approved(request) {
            return Vec::new();
        }
        let Some(source_hash) = verified_source_hash(request) else {
            return Vec::new();
        };
        let mut jobs = Vec::new();
        for target in &request.backups {
            if target.storage.role != StorageRole::Backup
                || !target.storage.enabled
                || !mode_allows(target.storage.replication_mode, request.trigger)
                || target.existing_location.as_ref().is_some_and(|location| {
                    location.verification_status == StorageVerificationStatus::Verified
                        && location.expected_hash.as_ref() == Some(source_hash)
                        && location.observed_hash.as_ref() == Some(source_hash)
                })
            {
                continue;
            }
            let plan = replication_job_plan(request, &target.storage);
            let mut job = SyncJob::with_retry_policy_at(
                run_id,
                plan.payload,
                3,
                Some(plan.idempotency_key),
                created_at,
            );
            if target.availability == BackupAvailability::Unavailable {
                job.next_attempt_at = Some(deferred_until.max(created_at));
            }
            jobs.push(job);
        }
        jobs.sort_by(|left, right| left.idempotency_key.cmp(&right.idempotency_key));
        jobs
    }

    /// Produces one independent decision per backup. A missing source is never a
    /// delete instruction; retention is evaluated independently from replication.
    pub fn plan(request: &ReplicationRequest) -> Vec<ReplicationDecision> {
        if !source_is_approved(request) {
            return vec![ReplicationDecision::BlockedPendingApproval {
                primary_location_id: request.source_location.id,
            }];
        }
        let Some(source_hash) = verified_source_hash(request) else {
            return vec![ReplicationDecision::BlockedPendingApproval {
                primary_location_id: request.source_location.id,
            }];
        };

        request
            .backups
            .iter()
            .map(|target| Self::plan_target(request, target, source_hash))
            .collect()
    }

    fn plan_target(
        request: &ReplicationRequest,
        target: &BackupTarget,
        source_hash: &ContentHash,
    ) -> ReplicationDecision {
        let storage = &target.storage;
        if storage.role != StorageRole::Backup || !storage.enabled {
            return ReplicationDecision::Skipped {
                backup_storage_id: storage.id,
                reason: "storage is not an enabled backup".into(),
            };
        }
        if !mode_allows(storage.replication_mode, request.trigger) {
            return ReplicationDecision::Skipped {
                backup_storage_id: storage.id,
                reason: "backup replication mode does not allow this trigger".into(),
            };
        }
        if target.availability == BackupAvailability::Unavailable {
            return ReplicationDecision::DeferredUnavailable {
                backup_storage_id: storage.id,
            };
        }
        if target.existing_location.as_ref().is_some_and(|location| {
            location.verification_status == StorageVerificationStatus::Verified
                && location.expected_hash.as_ref() == Some(source_hash)
                && location.observed_hash.as_ref() == Some(source_hash)
        }) {
            return ReplicationDecision::AlreadyVerified {
                backup_storage_id: storage.id,
            };
        }

        ReplicationDecision::Enqueue(replication_job_plan(request, storage))
    }
}

fn source_is_approved(request: &ReplicationRequest) -> bool {
    request.source_storage.role == StorageRole::Primary
        && request.source_storage.enabled
        && request.source_location.storage_id == request.source_storage.id
        && request.source_location.asset_id == request.asset_id
        && request.source_location.verification_status == StorageVerificationStatus::Verified
        && !request.source_object.is_trashed
}

fn replication_job_plan(
    request: &ReplicationRequest,
    storage: &MusicStorage,
) -> ReplicationJobPlan {
    let idempotency_key = replication_key(
        request.asset_id,
        request.source_storage.id,
        &request.source_object.object_id,
        request.source_object.provider_version.as_deref(),
        storage.id,
        &request.destination_object_key,
    );
    ReplicationJobPlan {
        backup_storage_id: storage.id,
        payload: SyncJobPayload::ReplicateAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: request.asset_id,
            source_storage_id: request.source_storage.id,
            source_object_id: request.source_object.object_id.clone(),
            source_object_key: request.source_object.display_path.clone(),
            source_provider_version: request.source_object.provider_version.clone(),
            expected_checksum: request.source_object.canonical_hash().cloned(),
            destination_storage_id: storage.id,
            destination_object_key: request.destination_object_key.clone(),
        },
        idempotency_key,
    }
}

fn mode_allows(mode: rec_core::ReplicationMode, trigger: ReplicationTrigger) -> bool {
    matches!(trigger, ReplicationTrigger::Manual)
        || matches!(
            (mode, trigger),
            (
                rec_core::ReplicationMode::Automatic,
                ReplicationTrigger::VerifiedPrimaryWrite
            ) | (
                rec_core::ReplicationMode::Scheduled,
                ReplicationTrigger::Schedule
            )
        )
}

fn verified_source_hash(request: &ReplicationRequest) -> Option<&ContentHash> {
    let hash = request.source_object.canonical_hash()?;
    (request.source_location.expected_hash.as_ref() == Some(hash)
        && request.source_location.observed_hash.as_ref() == Some(hash))
    .then_some(hash)
}

fn replication_key(
    asset_id: Uuid,
    source_storage_id: Uuid,
    source_object_id: &str,
    source_version: Option<&str>,
    destination_storage_id: Uuid,
    destination_key: &str,
) -> String {
    format!(
        "replicate:{asset_id}:{source_storage_id}:{source_object_id}:{}:{destination_storage_id}:{destination_key}",
        source_version.unwrap_or("unversioned")
    )
}

/// Input for a staged provider-independent transfer. The caller owns staging-file
/// lifecycle, allowing a failed upload to be resumed with the same job key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedReplicationRequest {
    pub asset_id: Uuid,
    pub source_object_id: String,
    pub destination_object_key: String,
    pub staging_path: PathBuf,
    pub expected_checksum: ContentHash,
    /// Immutable provider generation captured by the durable job payload.
    pub expected_source_version: Option<String>,
    pub idempotency_key: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
}

/// Durable stage reached by a staged replication worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationStage {
    PendingDownload,
    DownloadVerified,
    Uploaded,
    DestinationVerified,
}

/// One bounded transfer operation. Checkpoints must be persisted before invoking
/// the next stage, including the destination object ID returned by upload.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplicationStep {
    Checkpoint {
        stage: ReplicationStage,
        destination_object_id: Option<String>,
        bytes_transferred: u64,
    },
    Complete(ReplicationExecution),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplicationExecution {
    pub object: StorageObject,
    pub bytes_downloaded: u64,
    pub resumed_from: ReplicationStage,
}

#[derive(Debug, Error)]
pub enum ReplicationError {
    #[error("replication source does not provide a canonical SHA-256 checksum")]
    MissingChecksum,
    #[error("{stage} failed: {source}")]
    Storage {
        stage: &'static str,
        #[source]
        source: MusicStorageError,
    },
    #[error("backup verification failed: {0}")]
    Verification(String),
    #[error("{0} checkpoint requires a destination object ID")]
    MissingDestinationObject(&'static str),
    #[error("replication source version changed from the planned immutable version")]
    SourceVersionChanged,
}

/// Executes the only portable copy sequence supported by `MusicAssetStore`:
/// download to worker staging, upload with a stable idempotency key, and immediately
/// verify the destination by hash. It never propagates deletion or unapproved changes.
pub struct ReplicationExecutor<'a> {
    source: &'a dyn MusicAssetStore,
    destination: &'a dyn MusicAssetStore,
}

impl<'a> ReplicationExecutor<'a> {
    pub fn new(source: &'a dyn MusicAssetStore, destination: &'a dyn MusicAssetStore) -> Self {
        Self {
            source,
            destination,
        }
    }

    pub async fn replicate(
        &self,
        request: &StagedReplicationRequest,
    ) -> Result<StorageObject, ReplicationError> {
        Ok(self
            .replicate_from(request, ReplicationStage::PendingDownload)
            .await?
            .object)
    }

    /// Compatibility helper that drives all bounded stages in-process. Durable workers
    /// should call [`Self::execute_step`] and persist every returned checkpoint.
    pub async fn replicate_from(
        &self,
        request: &StagedReplicationRequest,
        stage: ReplicationStage,
    ) -> Result<ReplicationExecution, ReplicationError> {
        let resumed_from = stage;
        let mut stage = stage;
        let mut destination_object_id = None;
        let mut bytes_downloaded: u64 = 0;
        loop {
            match self
                .execute_step(request, stage, destination_object_id.clone())
                .await?
            {
                ReplicationStep::Checkpoint {
                    stage: next,
                    destination_object_id: object_id,
                    bytes_transferred,
                } => {
                    stage = next;
                    destination_object_id = object_id;
                    bytes_downloaded = bytes_downloaded.saturating_add(bytes_transferred);
                }
                ReplicationStep::Complete(mut execution) => {
                    execution.bytes_downloaded = bytes_downloaded;
                    execution.resumed_from = resumed_from;
                    return Ok(execution);
                }
            }
        }
    }

    /// Execute exactly one externally checkpointable transfer stage.
    pub async fn execute_step(
        &self,
        request: &StagedReplicationRequest,
        stage: ReplicationStage,
        destination_object_id: Option<String>,
    ) -> Result<ReplicationStep, ReplicationError> {
        match stage {
            ReplicationStage::PendingDownload => {
                let download = self
                    .source
                    .download(DownloadRequest {
                        object_id: request.source_object_id.clone(),
                        destination_path: request.staging_path.clone(),
                        range: None,
                        expected_checksum: Some(request.expected_checksum.clone()),
                        overwrite: true,
                    })
                    .await
                    .map_err(|source| ReplicationError::Storage {
                        stage: "download",
                        source,
                    })?;
                validate_download(&download, &request.expected_checksum)?;
                if request.expected_source_version.as_deref()
                    != download.object.provider_version.as_deref()
                {
                    return Err(ReplicationError::SourceVersionChanged);
                }
                Ok(ReplicationStep::Checkpoint {
                    stage: ReplicationStage::DownloadVerified,
                    destination_object_id: None,
                    bytes_transferred: download.bytes_written,
                })
            }
            ReplicationStage::DownloadVerified => {
                let mut metadata = BTreeMap::new();
                metadata.insert("rec_bag.asset_id".into(), request.asset_id.to_string());
                metadata.insert(
                    "rec_bag.operation_key".into(),
                    request.idempotency_key.clone(),
                );
                let uploaded = self
                    .destination
                    .upload(UploadRequest {
                        source_path: request.staging_path.clone(),
                        object_path: request.destination_object_key.clone(),
                        content_type: request.content_type.clone(),
                        content_length: request.content_length,
                        expected_checksum: Some(request.expected_checksum.clone()),
                        metadata,
                        idempotency_key: Some(request.idempotency_key.clone()),
                    })
                    .await
                    .map_err(|source| ReplicationError::Storage {
                        stage: "upload",
                        source,
                    })?;
                Ok(ReplicationStep::Checkpoint {
                    stage: ReplicationStage::Uploaded,
                    destination_object_id: Some(uploaded.object_id),
                    bytes_transferred: 0,
                })
            }
            ReplicationStage::Uploaded | ReplicationStage::DestinationVerified => {
                let object_id = destination_object_id
                    .ok_or(ReplicationError::MissingDestinationObject("uploaded"))?;
                let stated = self.destination.stat(&object_id).await.map_err(|source| {
                    ReplicationError::Storage {
                        stage: "post-copy stat",
                        source,
                    }
                })?;
                if stated.canonical_hash() != Some(&request.expected_checksum) {
                    // Providers without hash-bearing stat results must prove the byte stream.
                    let verification_path = request.staging_path.with_extension(format!(
                        "verify-{}",
                        Utc::now().timestamp_nanos_opt().unwrap_or_default()
                    ));
                    let verification = self
                        .destination
                        .download(DownloadRequest {
                            object_id,
                            destination_path: verification_path,
                            range: None,
                            expected_checksum: Some(request.expected_checksum.clone()),
                            overwrite: true,
                        })
                        .await
                        .map_err(|source| ReplicationError::Storage {
                            stage: "post-copy download",
                            source,
                        })?;
                    validate_download(&verification, &request.expected_checksum)?;
                }
                Ok(ReplicationStep::Complete(ReplicationExecution {
                    object: stated,
                    bytes_downloaded: 0,
                    resumed_from: stage,
                }))
            }
        }
    }
}

fn validate_download(
    result: &DownloadResult,
    expected: &ContentHash,
) -> Result<(), ReplicationError> {
    if result.checksum_verified && result.checksum.as_ref() == Some(expected) {
        Ok(())
    } else {
        Err(ReplicationError::Verification(
            "provider did not verify the expected canonical checksum".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{
        AssetType, BoxFuture, InventoryPage, InventoryRequest, MusicStorageErrorKind,
        MusicStorageKind, ReplicationMode, StorageCapabilities, StorageHealth, StorageVersionPage,
        StorageVersionRequest,
    };
    use std::sync::Mutex;

    fn hash() -> ContentHash {
        ContentHash::sha256("a".repeat(64)).unwrap()
    }

    fn storage(role: StorageRole, mode: ReplicationMode) -> MusicStorage {
        let mut storage = MusicStorage::new(
            "test storage",
            MusicStorageKind::LocalDirectory,
            role,
            StorageCapabilities::default(),
        );
        storage.replication_mode = mode;
        storage
    }

    fn request(backups: Vec<BackupTarget>) -> ReplicationRequest {
        let asset_id = Uuid::new_v4();
        let primary = storage(StorageRole::Primary, ReplicationMode::Automatic);
        let checksum = hash();
        let source = StorageObject {
            storage_id: primary.id,
            object_id: "primary-object".into(),
            name: "track.flac".into(),
            display_path: Some("music/track.flac".into()),
            size_bytes: 10,
            media_type: Some("audio/flac".into()),
            modified_at: None,
            provider_version: Some("v1".into()),
            hashes: vec![checksum.clone()],
            embedded_asset_id: Some(asset_id),
            is_trashed: false,
        };
        let source_location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id,
            asset_type: AssetType::Original,
            storage_id: primary.id,
            object_id: source.object_id.clone(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: Some(checksum.clone()),
            observed_hash: Some(checksum),
            provider_version: Some("v1".into()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };
        ReplicationRequest {
            asset_id,
            source_storage: primary,
            source_object: source,
            source_location,
            destination_object_key: "backup/track.flac".into(),
            trigger: ReplicationTrigger::VerifiedPrimaryWrite,
            backups,
        }
    }

    #[test]
    fn plans_independent_backups_and_defers_only_unavailable_target() {
        let available = storage(StorageRole::Backup, ReplicationMode::Automatic);
        let unavailable = storage(StorageRole::Backup, ReplicationMode::Automatic);
        let decisions = ReplicationPlanner::plan(&request(vec![
            BackupTarget {
                storage: available.clone(),
                availability: BackupAvailability::Available,
                existing_location: None,
            },
            BackupTarget {
                storage: unavailable.clone(),
                availability: BackupAvailability::Unavailable,
                existing_location: None,
            },
        ]));
        assert!(matches!(decisions[0], ReplicationDecision::Enqueue(_)));
        assert_eq!(
            decisions[1],
            ReplicationDecision::DeferredUnavailable {
                backup_storage_id: unavailable.id
            }
        );
    }

    #[test]
    fn unavailable_target_materializes_as_a_durable_deferred_job() {
        let backup = storage(StorageRole::Backup, ReplicationMode::Automatic);
        let request = request(vec![BackupTarget {
            storage: backup.clone(),
            availability: BackupAvailability::Unavailable,
            existing_location: None,
        }]);
        let now = Utc::now();
        let due = now + chrono::Duration::hours(1);
        let jobs = ReplicationPlanner::durable_jobs(&request, Uuid::new_v4(), now, due);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].next_attempt_at, Some(due));
        assert!(jobs[0].idempotency_key.contains(&backup.id.to_string()));
    }

    #[test]
    fn verified_backup_is_idempotent_and_changed_primary_is_blocked() {
        let backup = storage(StorageRole::Backup, ReplicationMode::Automatic);
        let first_request = request(vec![BackupTarget {
            storage: backup.clone(),
            availability: BackupAvailability::Available,
            existing_location: None,
        }]);
        let ReplicationDecision::Enqueue(first) = &ReplicationPlanner::plan(&first_request)[0]
        else {
            panic!("expected initial replication plan");
        };
        let mut replay = first_request.clone();
        let checksum = hash();
        replay.backups[0].existing_location = Some(AssetLocation {
            id: Uuid::new_v4(),
            asset_id: replay.asset_id,
            asset_type: AssetType::Original,
            storage_id: backup.id,
            object_id: "backup-object".into(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: Some(checksum.clone()),
            observed_hash: Some(checksum),
            provider_version: Some("v1".into()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        });
        assert_eq!(
            ReplicationPlanner::plan(&replay),
            vec![ReplicationDecision::AlreadyVerified {
                backup_storage_id: backup.id
            }]
        );

        replay.source_location.verification_status =
            StorageVerificationStatus::ChangedPendingReview;
        assert_eq!(
            ReplicationPlanner::plan(&replay),
            vec![ReplicationDecision::BlockedPendingApproval {
                primary_location_id: replay.source_location.id
            }]
        );
        assert!(first.idempotency_key.contains("primary-object:v1"));
    }

    #[test]
    fn manual_now_overrides_each_enabled_backup_schedule_mode() {
        for mode in [
            ReplicationMode::Automatic,
            ReplicationMode::Scheduled,
            ReplicationMode::Manual,
        ] {
            let backup = storage(StorageRole::Backup, mode);
            let mut request = request(vec![BackupTarget {
                storage: backup,
                availability: BackupAvailability::Available,
                existing_location: None,
            }]);
            request.trigger = ReplicationTrigger::Manual;
            assert!(matches!(
                ReplicationPlanner::plan(&request).as_slice(),
                [ReplicationDecision::Enqueue(_)]
            ));
        }
    }

    struct FakeAssetStore {
        object: StorageObject,
        downloads: Mutex<u32>,
        uploads: Mutex<u32>,
    }

    impl FakeAssetStore {
        fn new(object: StorageObject) -> Self {
            Self {
                object,
                downloads: Mutex::new(0),
                uploads: Mutex::new(0),
            }
        }
    }

    impl MusicAssetStore for FakeAssetStore {
        fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
            Box::pin(async {
                Err(MusicStorageError::new(
                    MusicStorageErrorKind::Permanent,
                    "unused",
                ))
            })
        }

        fn inventory_page(
            &self,
            _: InventoryRequest,
        ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
            Box::pin(async {
                Err(MusicStorageError::new(
                    MusicStorageErrorKind::Permanent,
                    "unused",
                ))
            })
        }

        fn stat<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
            let object = self.object.clone();
            Box::pin(async move { Ok(object) })
        }

        fn versions_page(
            &self,
            _: StorageVersionRequest,
        ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
            Box::pin(async {
                Err(MusicStorageError::new(
                    MusicStorageErrorKind::Permanent,
                    "unused",
                ))
            })
        }

        fn upload(
            &self,
            _: UploadRequest,
        ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
            *self.uploads.lock().unwrap() += 1;
            let object = self.object.clone();
            Box::pin(async move { Ok(object) })
        }

        fn download(
            &self,
            request: DownloadRequest,
        ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
            *self.downloads.lock().unwrap() += 1;
            let object = self.object.clone();
            Box::pin(async move {
                Ok(DownloadResult {
                    object,
                    destination_path: request.destination_path,
                    bytes_written: 10,
                    checksum: request.expected_checksum,
                    checksum_verified: true,
                })
            })
        }
    }

    #[tokio::test]
    async fn executor_exposes_download_and_upload_as_separate_durable_steps() {
        let storage_id = Uuid::new_v4();
        let checksum = hash();
        let object = StorageObject {
            storage_id,
            object_id: "object".into(),
            name: "track.flac".into(),
            display_path: None,
            size_bytes: 10,
            media_type: None,
            modified_at: None,
            provider_version: Some("v1".into()),
            hashes: vec![checksum.clone()],
            embedded_asset_id: None,
            is_trashed: false,
        };
        let source = FakeAssetStore::new(object.clone());
        let destination = FakeAssetStore::new(object);
        let executor = ReplicationExecutor::new(&source, &destination);
        let request = StagedReplicationRequest {
            asset_id: Uuid::new_v4(),
            source_object_id: "source".into(),
            destination_object_key: "backup/track.flac".into(),
            staging_path: PathBuf::from("staging/track.flac"),
            expected_checksum: checksum,
            expected_source_version: Some("v1".into()),
            idempotency_key: "stable-key".into(),
            content_type: None,
            content_length: Some(10),
        };

        let first = executor
            .execute_step(&request, ReplicationStage::PendingDownload, None)
            .await
            .unwrap();
        assert!(matches!(
            first,
            ReplicationStep::Checkpoint {
                stage: ReplicationStage::DownloadVerified,
                bytes_transferred: 10,
                ..
            }
        ));
        assert_eq!(*source.downloads.lock().unwrap(), 1);
        assert_eq!(*destination.uploads.lock().unwrap(), 0);

        let second = executor
            .execute_step(&request, ReplicationStage::DownloadVerified, None)
            .await
            .unwrap();
        assert!(matches!(
            second,
            ReplicationStep::Checkpoint {
                stage: ReplicationStage::Uploaded,
                destination_object_id: Some(_),
                ..
            }
        ));
        assert_eq!(*destination.uploads.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn executor_resumes_after_verified_download_without_repeating_it() {
        let storage_id = Uuid::new_v4();
        let checksum = hash();
        let object = StorageObject {
            storage_id,
            object_id: "object".into(),
            name: "track.flac".into(),
            display_path: None,
            size_bytes: 10,
            media_type: None,
            modified_at: None,
            provider_version: Some("v1".into()),
            hashes: vec![checksum.clone()],
            embedded_asset_id: None,
            is_trashed: false,
        };
        let source = FakeAssetStore::new(object.clone());
        let destination = FakeAssetStore::new(object);
        let executor = ReplicationExecutor::new(&source, &destination);
        let result = executor
            .replicate_from(
                &StagedReplicationRequest {
                    asset_id: Uuid::new_v4(),
                    source_object_id: "source".into(),
                    destination_object_key: "backup/track.flac".into(),
                    staging_path: PathBuf::from("staging/track.flac"),
                    expected_checksum: checksum,
                    expected_source_version: Some("v1".into()),
                    idempotency_key: "stable-key".into(),
                    content_type: None,
                    content_length: Some(10),
                },
                ReplicationStage::DownloadVerified,
            )
            .await
            .unwrap();
        assert_eq!(result.resumed_from, ReplicationStage::DownloadVerified);
        assert_eq!(*source.downloads.lock().unwrap(), 0);
        assert_eq!(*destination.uploads.lock().unwrap(), 1);
    }
}
