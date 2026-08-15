//! Reconciliation review action integration slice.

use std::{path::Path, sync::Arc};

use chrono::Utc;
use rec_core::{
    AssetLocation, AssetType, CanonicalTrack, ContentHash, OriginalAsset, OriginalAssetRole,
    ReconciliationAction, ReconciliationDecision, ReconciliationRepository,
    ReconciliationResolution, ReconciliationResolutionFilter, ReconciliationResult,
    ReconciliationSuppression, ReplicationMode, RepositoryError, RepositoryPageRequest,
    RepositoryTransaction, StorageObject, StorageRole, StorageVerificationStatus, SyncJobPayload,
    SyncRunFilter, SyncRunRepository, SyncStatus, TransactionManager, SYNC_JOB_PAYLOAD_VERSION,
};
use rec_persistence::TursoCatalog;
use rec_sync::{JobPlanner, JobStoreError};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;
use ui::MusicManagementEvent;
use uuid::Uuid;

use super::integration::{IntegrationFuture, IntegrationSliceHandler};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

#[derive(Debug, Error)]
pub enum ReconciliationActionError {
    #[error("invalid {kind} identifier: {value}")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("reconciliation result was not found: {0}")]
    ResultNotFound(Uuid),
    #[error("match candidate was not found: {0}")]
    CandidateNotFound(Uuid),
    #[error("canonical track was not found: {0}")]
    TrackNotFound(Uuid),
    #[error("original asset was not found: {0}")]
    AssetNotFound(Uuid),
    #[error("storage object was not found: {storage_id}/{object_id}")]
    StorageObjectNotFound { storage_id: Uuid, object_id: String },
    #[error("action is not authorized by the persisted reconciliation result: {0}")]
    Unauthorized(String),
    #[error("invalid action state: {0}")]
    InvalidState(String),
    #[error("local file is not a readable regular file: {0}")]
    InvalidLocalFile(String),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog repository operation failed: {0}")]
    Repository(#[from] RepositoryError),
    #[error("job planning failed: {0}")]
    Job(String),
    #[error("event is not supported by reconciliation actions: {0}")]
    UnsupportedEvent(String),
}

impl ReconciliationActionError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Io(_))
            || matches!(self, Self::Repository(error) if error.is_retryable())
    }
}

/// Slice 5 service. The lock serializes authorization/check/apply sequences within
/// this desktop process; deterministic resolution IDs and durable job keys preserve
/// idempotency across retries and process restarts.
pub struct ReconciliationActionsService {
    catalog: Arc<TursoCatalog>,
    action_lock: Mutex<()>,
}

impl ReconciliationActionsService {
    pub fn new(catalog: Arc<TursoCatalog>) -> Self {
        Self {
            catalog,
            action_lock: Mutex::new(()),
        }
    }

    pub fn catalog(&self) -> &Arc<TursoCatalog> {
        &self.catalog
    }

    pub async fn confirm_match(
        &self,
        candidate_id: Uuid,
    ) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = self
            .confirm_match_in(transaction.as_ref(), candidate_id)
            .await;
        finish_transaction(transaction, outcome).await
    }

    async fn confirm_match_in(
        &self,
        transaction: &dyn RepositoryTransaction,
        candidate_id: Uuid,
    ) -> Result<bool, ReconciliationActionError> {
        let candidate = transaction
            .reconciliation()
            .get_candidate(candidate_id)
            .await?
            .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
        let result = transaction
            .reconciliation()
            .get_result_by_candidate(candidate_id)
            .await?
            .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
        authorize(&result, &ReconciliationAction::Confirm { candidate_id })?;
        if result
            .candidates
            .iter()
            .all(|stored| stored.id != candidate_id)
        {
            return Err(ReconciliationActionError::Unauthorized(
                "candidate does not belong to the result".into(),
            ));
        }
        let asset_id = candidate.asset_id.ok_or_else(|| {
            ReconciliationActionError::InvalidState(
                "candidate has no original asset; use locate local file".into(),
            )
        })?;
        let asset = transaction
            .assets()
            .get_original(asset_id)
            .await?
            .ok_or(ReconciliationActionError::AssetNotFound(asset_id))?;
        let object = required_object(
            transaction,
            candidate.storage_id,
            &candidate.storage_object_id,
        )
        .await?;

        if resolution_exists(
            transaction,
            result.id,
            Some(candidate_id),
            ReconciliationDecision::ConfirmCandidate,
        )
        .await?
        {
            return Ok(false);
        }

        let now = Utc::now();
        let existing = transaction
            .assets()
            .get_location(candidate.storage_id, candidate.storage_object_id.clone())
            .await?;
        transaction
            .assets()
            .upsert_location(verified_location(
                existing.as_ref(),
                asset_id,
                candidate.storage_id,
                candidate.storage_object_id.clone(),
                asset.content_hash.clone(),
                &object,
                now,
            ))
            .await?;
        record_resolution(
            transaction,
            result.id,
            Some(candidate_id),
            ReconciliationDecision::ConfirmCandidate,
            None,
        )
        .await?;
        Ok(true)
    }

    pub async fn reject_match(
        &self,
        candidate_id: Uuid,
    ) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = transaction
                .reconciliation()
                .get_result_by_candidate(candidate_id)
                .await?
                .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
            authorize(&result, &ReconciliationAction::Reject { candidate_id })?;
            if resolution_exists(
                transaction.as_ref(),
                result.id,
                Some(candidate_id),
                ReconciliationDecision::RejectCandidate,
            )
            .await?
            {
                return Ok(false);
            }
            record_resolution(
                transaction.as_ref(),
                result.id,
                Some(candidate_id),
                ReconciliationDecision::RejectCandidate,
                Some("candidate rejected by user"),
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn locate_local_file(
        &self,
        candidate_id: Uuid,
        track_id: Uuid,
        file_path: &Path,
    ) -> Result<bool, ReconciliationActionError> {
        let canonical_path = std::fs::canonicalize(file_path).map_err(|_| {
            ReconciliationActionError::InvalidLocalFile(file_path.display().to_string())
        })?;
        if !canonical_path.is_file() {
            return Err(ReconciliationActionError::InvalidLocalFile(
                canonical_path.display().to_string(),
            ));
        }
        let bytes = std::fs::read(&canonical_path)?;
        let hash = sha256(&bytes);
        let file_name = canonical_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                ReconciliationActionError::InvalidLocalFile(canonical_path.display().to_string())
            })?
            .to_owned();

        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let candidate = transaction
                .reconciliation()
                .get_candidate(candidate_id)
                .await?
                .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
            if candidate.track_id != track_id {
                return Err(ReconciliationActionError::Unauthorized(
                    "candidate and selected track differ".into(),
                ));
            }
            let result = transaction
                .reconciliation()
                .get_result_by_candidate(candidate_id)
                .await?
                .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
            authorize(&result, &ReconciliationAction::Locate { track_id })?;
            transaction
                .tracks()
                .get(track_id)
                .await?
                .ok_or(ReconciliationActionError::TrackNotFound(track_id))?;
            transaction
                .music_storages()
                .get(candidate.storage_id)
                .await?
                .ok_or_else(|| {
                    ReconciliationActionError::InvalidState(format!(
                        "candidate storage {} does not exist",
                        candidate.storage_id
                    ))
                })?;
            if resolution_exists(
                transaction.as_ref(),
                result.id,
                Some(candidate_id),
                ReconciliationDecision::ConfirmCandidate,
            )
            .await?
            {
                return Ok(false);
            }

            let mut asset = OriginalAsset::new(
                track_id,
                OriginalAssetRole::Master,
                file_name.clone(),
                bytes.len() as u64,
                hash.value(),
            )
            .map_err(|error| ReconciliationActionError::InvalidState(error.to_string()))?;
            asset.is_preferred = true;
            let object_id = canonical_path.to_string_lossy().into_owned();
            let object = StorageObject {
                storage_id: candidate.storage_id,
                object_id: object_id.clone(),
                name: file_name.clone(),
                display_path: Some(object_id.clone()),
                size_bytes: bytes.len() as u64,
                media_type: None,
                modified_at: None,
                provider_version: None,
                hashes: vec![hash.clone()],
                embedded_asset_id: Some(asset.id()),
                is_trashed: false,
            };
            transaction.assets().upsert_original(asset.clone()).await?;
            transaction
                .assets()
                .upsert_storage_object(object.clone())
                .await?;
            transaction
                .assets()
                .upsert_location(verified_location(
                    None,
                    asset.id(),
                    candidate.storage_id,
                    object_id,
                    Some(hash),
                    &object,
                    Utc::now(),
                ))
                .await?;
            record_resolution(
                transaction.as_ref(),
                result.id,
                Some(candidate_id),
                ReconciliationDecision::ConfirmCandidate,
                Some("local file selected by user"),
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn associate_unknown(
        &self,
        result_id: Uuid,
        track_id: Uuid,
    ) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = required_result(transaction.as_ref(), result_id).await?;
            let (storage_id, object_id) = result_storage_object(&result)?;
            authorize(
                &result,
                &ReconciliationAction::AssociateUnknownObject {
                    result_id,
                    track_id,
                    asset_id: result.asset_id,
                    storage_id,
                    storage_object_id: object_id.clone(),
                },
            )?;
            if resolution_exists(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::AssociateUnknownObject,
            )
            .await?
            {
                return Ok(false);
            }
            transaction
                .tracks()
                .get(track_id)
                .await?
                .ok_or(ReconciliationActionError::TrackNotFound(track_id))?;
            let object = required_object(transaction.as_ref(), storage_id, &object_id).await?;
            let asset = match result.asset_id {
                Some(asset_id) => transaction
                    .assets()
                    .get_original(asset_id)
                    .await?
                    .ok_or(ReconciliationActionError::AssetNotFound(asset_id))?,
                None => original_from_object(track_id, &object)?,
            };
            transaction.assets().upsert_original(asset.clone()).await?;
            let existing = transaction
                .assets()
                .get_location(storage_id, object_id.clone())
                .await?;
            transaction
                .assets()
                .upsert_location(verified_location(
                    existing.as_ref(),
                    asset.id(),
                    storage_id,
                    object_id,
                    asset.content_hash.clone(),
                    &object,
                    Utc::now(),
                ))
                .await?;
            record_resolution(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::AssociateUnknownObject,
                None,
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn import_unknown(&self, result_id: Uuid) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = required_result(transaction.as_ref(), result_id).await?;
            let (storage_id, object_id) = result_storage_object(&result)?;
            authorize(
                &result,
                &ReconciliationAction::ImportUnknownObject {
                    result_id,
                    storage_id,
                    storage_object_id: object_id.clone(),
                },
            )?;
            if resolution_exists(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::ImportUnknownObject,
            )
            .await?
            {
                return Ok(false);
            }
            let object = required_object(transaction.as_ref(), storage_id, &object_id).await?;
            let track = match result.track_id {
                Some(track_id) => transaction
                    .tracks()
                    .get(track_id)
                    .await?
                    .ok_or(ReconciliationActionError::TrackNotFound(track_id))?,
                None => CanonicalTrack::new(),
            };
            transaction.tracks().upsert(track.clone()).await?;
            let asset = original_from_object(track.id, &object)?;
            transaction.assets().upsert_original(asset.clone()).await?;
            transaction
                .assets()
                .upsert_location(verified_location(
                    None,
                    asset.id(),
                    storage_id,
                    object_id,
                    asset.content_hash.clone(),
                    &object,
                    Utc::now(),
                ))
                .await?;
            record_resolution(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::ImportUnknownObject,
                None,
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn ignore_unknown(&self, result_id: Uuid) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = required_result(transaction.as_ref(), result_id).await?;
            let authorized_reason = result
                .actions
                .iter()
                .find_map(|action| match action {
                    ReconciliationAction::Ignore {
                        result_id: authorized_id,
                        reason,
                    } if *authorized_id == result_id => Some(reason.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    ReconciliationActionError::Unauthorized(format!(
                        "result {result_id} does not allow Ignore"
                    ))
                })?;
            if resolution_exists(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::IgnoreFinding,
            )
            .await?
            {
                return Ok(false);
            }
            record_resolution(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::IgnoreFinding,
                authorized_reason
                    .as_deref()
                    .or(Some("unknown primary object ignored by user")),
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    /// Approve changed primary bytes as a new original metadata version and atomically
    /// queue automatic backup replication jobs from that immutable provider version.
    pub async fn approve_external_change(
        &self,
        result_id: Uuid,
    ) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = required_result(transaction.as_ref(), result_id).await?;
            let (storage_id, object_id) = result_storage_object(&result)?;
            authorize(
                &result,
                &ReconciliationAction::ApproveExternalChange {
                    result_id,
                    storage_id,
                    storage_object_id: object_id.clone(),
                },
            )?;
            if resolution_exists(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::ApproveExternalChange,
            )
            .await?
            {
                return Ok(false);
            }
            let old_asset_id = result.asset_id.ok_or_else(|| {
                ReconciliationActionError::InvalidState(
                    "external change result has no original asset".into(),
                )
            })?;
            let old_asset = transaction
                .assets()
                .get_original(old_asset_id)
                .await?
                .ok_or(ReconciliationActionError::AssetNotFound(old_asset_id))?;
            let object = required_object(transaction.as_ref(), storage_id, &object_id).await?;
            let observed_hash = object.canonical_hash().cloned().ok_or_else(|| {
                ReconciliationActionError::InvalidState(
                    "changed object has no canonical SHA-256 hash".into(),
                )
            })?;
            let existing = transaction
                .assets()
                .get_location(storage_id, object_id.clone())
                .await?
                .ok_or_else(|| {
                    ReconciliationActionError::InvalidState(
                        "changed object has no existing asset location".into(),
                    )
                })?;
            if existing.asset_id != old_asset_id
                || existing.verification_status != StorageVerificationStatus::ChangedPendingReview
            {
                return Err(ReconciliationActionError::InvalidState(
                    "asset location is not the pending changed version".into(),
                ));
            }

            let mut new_asset = old_asset.new_metadata_version();
            new_asset.file_name = object.name.clone();
            new_asset.media_type = object.media_type.clone();
            new_asset.size_bytes = Some(object.size_bytes);
            new_asset.content_hash = Some(observed_hash.clone());
            transaction
                .assets()
                .upsert_original(new_asset.clone())
                .await?;
            transaction
                .assets()
                .upsert_location(verified_location(
                    Some(&existing),
                    new_asset.id(),
                    storage_id,
                    object_id.clone(),
                    Some(observed_hash.clone()),
                    &object,
                    Utc::now(),
                ))
                .await?;

            let backups = transaction.music_storages().list().await?;
            let payloads = backups
                .into_iter()
                .filter(|storage| {
                    storage.enabled
                        && storage.role == StorageRole::Backup
                        && storage.replication_mode == ReplicationMode::Automatic
                        && storage.id != storage_id
                })
                .map(|storage| SyncJobPayload::ReplicateAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id: new_asset.id(),
                    source_storage_id: storage_id,
                    source_object_id: object_id.clone(),
                    source_object_key: object.display_path.clone(),
                    source_provider_version: object.provider_version.clone(),
                    expected_checksum: Some(observed_hash.clone()),
                    destination_storage_id: storage.id,
                    destination_object_key: new_asset.file_name.clone(),
                })
                .collect::<Vec<_>>();
            if !payloads.is_empty() {
                let plan = JobPlanner::plan(
                    rec_core::SyncRunKind::BackupReplication,
                    None,
                    None,
                    payloads,
                    Utc::now(),
                );
                transaction.sync_runs().upsert(plan.run).await?;
                for job in plan.jobs {
                    transaction.sync_runs().upsert_job(job).await?;
                }
            }
            record_resolution(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::ApproveExternalChange,
                None,
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn retry_result(&self, result_id: Uuid) -> Result<bool, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let result = required_result(transaction.as_ref(), result_id).await?;
            authorize(&result, &ReconciliationAction::Retry { result_id })?;
            if resolution_exists(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::RetryFinding,
            )
            .await?
            {
                return Ok(false);
            }
            let track_id = result.track_id.ok_or_else(|| {
                ReconciliationActionError::InvalidState(
                    "retryable reconciliation result has no track".into(),
                )
            })?;
            let payload = SyncJobPayload::Reconcile {
                version: SYNC_JOB_PAYLOAD_VERSION,
                track_id,
                source_id: None,
                source_item_id: None,
                storage_id: result.storage_id,
                storage_object_id: result.storage_object_id.clone(),
            };
            let plan = JobPlanner::plan(
                rec_core::SyncRunKind::Reconciliation,
                None,
                result.storage_id,
                [payload],
                Utc::now(),
            );
            transaction.sync_runs().upsert(plan.run).await?;
            for job in plan.jobs {
                transaction.sync_runs().upsert_job(job).await?;
            }
            record_resolution(
                transaction.as_ref(),
                result_id,
                None,
                ReconciliationDecision::RetryFinding,
                None,
            )
            .await?;
            Ok(true)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn retry_run(&self, run_id: Uuid) -> Result<Uuid, ReconciliationActionError> {
        let _guard = self.action_lock.lock().await;
        let transaction = self.catalog.begin().await?;
        let outcome = async {
            let previous = transaction.sync_runs().get(run_id).await?.ok_or_else(|| {
                ReconciliationActionError::InvalidState(format!("sync run {run_id} was not found"))
            })?;
            if !matches!(
                previous.status,
                SyncStatus::Failed | SyncStatus::Paused | SyncStatus::Cancelled
            ) {
                return Err(ReconciliationActionError::Unauthorized(format!(
                    "sync run {} is not retryable from {:?}",
                    previous.id, previous.status
                )));
            }
            let runs = transaction
                .sync_runs()
                .list(RepositoryPageRequest::default())
                .await?;
            if let Some(existing) = runs.items.into_iter().find(|run| {
                run.resumed_from_run_id == Some(run_id)
                    && !matches!(run.status, SyncStatus::Failed | SyncStatus::Cancelled)
            }) {
                return Ok(existing.id);
            }
            let jobs = transaction.sync_runs().list_jobs(run_id).await?;
            let restarted_at = Utc::now();
            let plan =
                JobPlanner::restart(&previous, &jobs, restarted_at).map_err(job_planning_error)?;

            // A terminal run can still contain pending/running jobs after a process
            // failure. Supersede those jobs in this transaction before inserting the
            // resumed jobs so the active-key constraint is released without changing
            // the provider idempotency key or losing its checkpoint.
            for mut predecessor in jobs {
                if idempotency_key_is_active(predecessor.status) {
                    predecessor.status = SyncStatus::Cancelled;
                    predecessor.next_attempt_at = None;
                    predecessor.lease_owner = None;
                    predecessor.lease_expires_at = None;
                    predecessor.updated_at = predecessor.updated_at.max(restarted_at);
                    predecessor.finished_at = Some(predecessor.updated_at);
                    transaction.sync_runs().upsert_job(predecessor).await?;
                }
            }

            let resumed_id = plan.run.id;
            transaction.sync_runs().upsert(plan.run).await?;
            for job in plan.jobs {
                transaction.sync_runs().upsert_job(job).await?;
            }
            Ok(resumed_id)
        }
        .await;
        finish_transaction(transaction, outcome).await
    }

    pub async fn retry_storage(&self, storage_id: Uuid) -> Result<Uuid, ReconciliationActionError> {
        let run = self
            .catalog
            .latest(SyncRunFilter {
                status: Some(SyncStatus::Failed),
                storage_id: Some(storage_id),
                ..Default::default()
            })
            .await?
            .ok_or_else(|| {
                ReconciliationActionError::InvalidState(format!(
                    "storage {storage_id} has no failed run"
                ))
            })?;
        self.retry_run(run.id).await
    }

    async fn review_match(&self, candidate_id: Uuid) -> Result<(), ReconciliationActionError> {
        let candidate = self
            .catalog
            .get_candidate(candidate_id)
            .await?
            .ok_or(ReconciliationActionError::CandidateNotFound(candidate_id))?;
        let result = self
            .catalog
            .get_result_by_candidate(candidate.id)
            .await?
            .ok_or(ReconciliationActionError::CandidateNotFound(candidate.id))?;
        let can_review = result.actions.iter().any(|action| {
            matches!(
                action,
                ReconciliationAction::Confirm { candidate_id: id }
                    | ReconciliationAction::Reject { candidate_id: id }
                    if *id == candidate.id
            )
        });
        if !can_review {
            return Err(ReconciliationActionError::Unauthorized(
                "candidate has no review action".into(),
            ));
        }
        Ok(())
    }
}

impl IntegrationSliceHandler for ReconciliationActionsService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::ReconciliationActions
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            if request.slice != IntegrationSlice::ReconciliationActions {
                return Err(IntegrationError::for_request(
                    &request,
                    "request was routed to the wrong integration slice",
                    false,
                ));
            }
            let result: Result<IntegrationOutcome, ReconciliationActionError> = async {
                let message = match &request.event {
                    MusicManagementEvent::ReviewMatch(id) => {
                        self.review_match(parse_uuid("candidate", id)?).await?;
                        "Match is ready for review".to_owned()
                    }
                    MusicManagementEvent::ConfirmMatch(id) => mutation_message(
                        self.confirm_match(parse_uuid("candidate", id)?).await?,
                        "Match confirmed",
                    ),
                    MusicManagementEvent::RejectMatch(id) => mutation_message(
                        self.reject_match(parse_uuid("candidate", id)?).await?,
                        "Match rejected",
                    ),
                    MusicManagementEvent::AssociateLocalFile {
                        candidate_id,
                        track_id,
                        file_path,
                    } => mutation_message(
                        self.locate_local_file(
                            parse_uuid("candidate", candidate_id)?,
                            parse_uuid("track", track_id)?,
                            Path::new(file_path),
                        )
                        .await?,
                        "Local file associated",
                    ),
                    MusicManagementEvent::AssociateUnknownFile { file_id, track_id } => {
                        mutation_message(
                            self.associate_unknown(
                                parse_uuid("result", file_id)?,
                                parse_uuid("track", track_id)?,
                            )
                            .await?,
                            "Unknown file associated",
                        )
                    }
                    MusicManagementEvent::ImportUnknownFile(id) => mutation_message(
                        self.import_unknown(parse_uuid("result", id)?).await?,
                        "Unknown file imported",
                    ),
                    MusicManagementEvent::IgnoreUnknownFile(id) => mutation_message(
                        self.ignore_unknown(parse_uuid("result", id)?).await?,
                        "Unknown file ignored",
                    ),
                    MusicManagementEvent::ApproveExternalChange { result_id } => mutation_message(
                        self.approve_external_change(parse_uuid("result", result_id)?)
                            .await?,
                        "External change approved",
                    ),
                    MusicManagementEvent::RetryReconciliationResult { result_id } => {
                        mutation_message(
                            self.retry_result(parse_uuid("result", result_id)?).await?,
                            "Reconciliation retry queued",
                        )
                    }
                    MusicManagementEvent::RetrySyncRun { sync_run_id } => {
                        let resumed = self.retry_run(parse_uuid("sync run", sync_run_id)?).await?;
                        format!("Sync retry queued as {resumed}")
                    }
                    MusicManagementEvent::RetryStorage(storage_id) => {
                        let resumed = self
                            .retry_storage(parse_uuid("storage", storage_id)?)
                            .await?;
                        format!("Storage retry queued as {resumed}")
                    }
                    MusicManagementEvent::ToggleOfflinePin(_, _)
                    | MusicManagementEvent::CopyToPrimary(_) => {
                        return Err(ReconciliationActionError::UnsupportedEvent(format!(
                            "{:?} belongs to its asset-owning slice",
                            request.event
                        )))
                    }
                    _ => {
                        return Err(ReconciliationActionError::UnsupportedEvent(format!(
                            "{:?}",
                            request.event
                        )))
                    }
                };
                Ok(IntegrationOutcome {
                    view: None,
                    message: Some(message),
                })
            }
            .await;
            result.map_err(|error| {
                IntegrationError::for_request(&request, error.to_string(), error.retryable())
            })
        })
    }
}

fn idempotency_key_is_active(status: SyncStatus) -> bool {
    matches!(
        status,
        SyncStatus::Pending | SyncStatus::Running | SyncStatus::Paused | SyncStatus::Cancelling
    )
}

fn mutation_message(changed: bool, changed_message: &str) -> String {
    if changed {
        changed_message.to_owned()
    } else {
        format!("{changed_message} (already applied)")
    }
}

fn parse_uuid(kind: &'static str, value: &str) -> Result<Uuid, ReconciliationActionError> {
    Uuid::parse_str(value).map_err(|_| ReconciliationActionError::InvalidIdentifier {
        kind,
        value: value.to_owned(),
    })
}

fn authorize(
    result: &ReconciliationResult,
    expected: &ReconciliationAction,
) -> Result<(), ReconciliationActionError> {
    if result.actions.iter().any(|action| action == expected) {
        Ok(())
    } else {
        Err(ReconciliationActionError::Unauthorized(format!(
            "result {} does not allow {expected:?}",
            result.id
        )))
    }
}

async fn required_result(
    transaction: &dyn RepositoryTransaction,
    result_id: Uuid,
) -> Result<ReconciliationResult, ReconciliationActionError> {
    transaction
        .reconciliation()
        .get_result(result_id)
        .await?
        .ok_or(ReconciliationActionError::ResultNotFound(result_id))
}

fn result_storage_object(
    result: &ReconciliationResult,
) -> Result<(Uuid, String), ReconciliationActionError> {
    let storage_id = result.storage_id.ok_or_else(|| {
        ReconciliationActionError::InvalidState("result has no storage identity".into())
    })?;
    let object_id = result.storage_object_id.clone().ok_or_else(|| {
        ReconciliationActionError::InvalidState("result has no storage object identity".into())
    })?;
    Ok((storage_id, object_id))
}

async fn required_object(
    transaction: &dyn RepositoryTransaction,
    storage_id: Uuid,
    object_id: &str,
) -> Result<StorageObject, ReconciliationActionError> {
    transaction
        .assets()
        .get_storage_object(storage_id, object_id.to_owned())
        .await?
        .ok_or_else(|| ReconciliationActionError::StorageObjectNotFound {
            storage_id,
            object_id: object_id.to_owned(),
        })
}

async fn resolution_exists(
    transaction: &dyn RepositoryTransaction,
    result_id: Uuid,
    candidate_id: Option<Uuid>,
    decision: ReconciliationDecision,
) -> Result<bool, ReconciliationActionError> {
    let page = transaction
        .reconciliation()
        .list_resolutions(
            ReconciliationResolutionFilter {
                result_id: Some(result_id),
                candidate_id,
                decision: Some(decision),
            },
            RepositoryPageRequest::default(),
        )
        .await?;
    Ok(!page.items.is_empty())
}

async fn record_resolution(
    transaction: &dyn RepositoryTransaction,
    result_id: Uuid,
    candidate_id: Option<Uuid>,
    decision: ReconciliationDecision,
    reason: Option<&str>,
) -> Result<(), ReconciliationActionError> {
    let mut resolution = ReconciliationResolution::new(result_id, decision);
    resolution.id = semantic_uuid("resolution", result_id, candidate_id, decision);
    resolution.candidate_id = candidate_id;
    resolution.reason = reason.map(str::to_owned);
    let mut suppression = ReconciliationSuppression::from_resolution(&resolution);
    if let Some(value) = &mut suppression {
        value.id = semantic_uuid("suppression", result_id, candidate_id, decision);
    }
    transaction
        .reconciliation()
        .record_resolution(resolution, suppression)
        .await?;
    Ok(())
}

fn semantic_uuid(
    namespace: &str,
    result_id: Uuid,
    candidate_id: Option<Uuid>,
    decision: ReconciliationDecision,
) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update(result_id.as_bytes());
    digest.update(candidate_id.unwrap_or(Uuid::nil()).as_bytes());
    digest.update(format!("{decision:?}").as_bytes());
    let digest = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn verified_location(
    existing: Option<&AssetLocation>,
    asset_id: Uuid,
    storage_id: Uuid,
    object_id: String,
    expected_hash: Option<ContentHash>,
    object: &StorageObject,
    now: chrono::DateTime<Utc>,
) -> AssetLocation {
    AssetLocation {
        id: existing.map_or_else(Uuid::new_v4, |location| location.id),
        asset_id,
        asset_type: AssetType::Original,
        storage_id,
        object_id,
        verification_status: StorageVerificationStatus::Verified,
        expected_hash,
        observed_hash: object.canonical_hash().cloned(),
        provider_version: object.provider_version.clone(),
        last_checked_at: Some(now),
        verified_at: Some(now),
        error_message: None,
    }
}

fn original_from_object(
    track_id: Uuid,
    object: &StorageObject,
) -> Result<OriginalAsset, ReconciliationActionError> {
    let mut asset = if let Some(hash) = object.canonical_hash() {
        OriginalAsset::new(
            track_id,
            OriginalAssetRole::Master,
            object.name.clone(),
            object.size_bytes,
            hash.value(),
        )
        .map_err(|error| ReconciliationActionError::InvalidState(error.to_string()))?
    } else {
        let mut asset =
            OriginalAsset::new_unverified(track_id, OriginalAssetRole::Master, object.name.clone());
        asset.size_bytes = Some(object.size_bytes);
        asset
    };
    asset.media_type = object.media_type.clone();
    asset.is_preferred = true;
    Ok(asset)
}

fn sha256(bytes: &[u8]) -> ContentHash {
    let digest = Sha256::digest(bytes);
    ContentHash::sha256(format!("{digest:x}"))
        .expect("SHA-256 formatting always creates a canonical digest")
}

fn job_planning_error(error: JobStoreError) -> ReconciliationActionError {
    ReconciliationActionError::Job(error.to_string())
}

async fn finish_transaction<T>(
    transaction: Box<dyn RepositoryTransaction + '_>,
    result: Result<T, ReconciliationActionError>,
) -> Result<T, ReconciliationActionError> {
    match result {
        Ok(value) => {
            transaction.commit().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{
        AssetRepository, MatchCandidate, MatchEvidence, MusicStorage, MusicStorageKind,
        MusicStorageRepository, ReconciliationOutcome, RepositoryPageRequest, StorageCapabilities,
        SyncError, SyncErrorKind, SyncJob, SyncRun, SyncRunKind, TrackRepository,
        TrackStorageStatus,
    };

    async fn catalog() -> Arc<TursoCatalog> {
        Arc::new(TursoCatalog::in_memory().await.unwrap())
    }

    fn storage(role: StorageRole) -> MusicStorage {
        MusicStorage::new(
            format!("{role:?}"),
            MusicStorageKind::LocalDirectory,
            role,
            StorageCapabilities::default(),
        )
    }

    fn object(storage_id: Uuid, object_id: &str, bytes: &[u8]) -> StorageObject {
        StorageObject {
            storage_id,
            object_id: object_id.into(),
            name: format!("{object_id}.flac"),
            display_path: Some(format!("Music/{object_id}.flac")),
            size_bytes: bytes.len() as u64,
            media_type: Some("audio/flac".into()),
            modified_at: None,
            provider_version: Some("v2".into()),
            hashes: vec![sha256(bytes)],
            embedded_asset_id: None,
            is_trashed: false,
        }
    }

    async fn seed_candidate(
        catalog: &TursoCatalog,
        include_confirm: bool,
    ) -> (
        CanonicalTrack,
        OriginalAsset,
        MusicStorage,
        MatchCandidate,
        ReconciliationResult,
    ) {
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog, track.clone())
            .await
            .unwrap();
        let asset = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "song.flac",
            3,
            sha256(b"old").value(),
        )
        .unwrap();
        catalog.upsert_original(asset.clone()).await.unwrap();
        let primary = storage(StorageRole::Primary);
        MusicStorageRepository::upsert(catalog, primary.clone())
            .await
            .unwrap();
        catalog
            .upsert_storage_object(object(primary.id, "candidate", b"old"))
            .await
            .unwrap();
        let candidate = MatchCandidate::new(
            track.id,
            Some(asset.id()),
            primary.id,
            "candidate",
            vec![MatchEvidence::metadata("title")],
        );
        let mut result = ReconciliationResult::new(ReconciliationOutcome::Track(
            TrackStorageStatus::PossibleMatch,
        ))
        .with_track(track.id)
        .with_asset(asset.id())
        .with_storage_object(primary.id, "candidate")
        .with_candidate(candidate.clone())
        .with_action(ReconciliationAction::Reject {
            candidate_id: candidate.id,
        });
        if include_confirm {
            result = result.with_action(ReconciliationAction::Confirm {
                candidate_id: candidate.id,
            });
        }
        catalog.upsert_result(result.clone()).await.unwrap();
        (track, asset, primary, candidate, result)
    }

    async fn resolutions(catalog: &TursoCatalog, result_id: Uuid) -> usize {
        catalog
            .list_resolutions(
                ReconciliationResolutionFilter {
                    result_id: Some(result_id),
                    ..Default::default()
                },
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap()
            .items
            .len()
    }

    #[tokio::test]
    async fn confirm_is_transactional_authorized_and_idempotent() {
        let catalog = catalog().await;
        let (_, asset, primary, candidate, result) = seed_candidate(&catalog, true).await;
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service.confirm_match(candidate.id).await.unwrap());
        assert!(!service.confirm_match(candidate.id).await.unwrap());
        let location = catalog
            .get_location(primary.id, "candidate".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(location.asset_id, asset.id());
        assert_eq!(
            location.verification_status,
            StorageVerificationStatus::Verified
        );
        assert_eq!(resolutions(&catalog, result.id).await, 1);
    }

    #[tokio::test]
    async fn unauthorized_confirm_rolls_back_all_side_effects() {
        let catalog = catalog().await;
        let (_, _, primary, candidate, result) = seed_candidate(&catalog, false).await;
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(matches!(
            service.confirm_match(candidate.id).await,
            Err(ReconciliationActionError::Unauthorized(_))
        ));
        assert!(catalog
            .get_location(primary.id, "candidate".into())
            .await
            .unwrap()
            .is_none());
        assert_eq!(resolutions(&catalog, result.id).await, 0);
    }

    #[tokio::test]
    async fn concurrent_reject_creates_one_resolution_and_one_suppression() {
        let catalog = catalog().await;
        let (_, _, _, candidate, result) = seed_candidate(&catalog, true).await;
        let service = Arc::new(ReconciliationActionsService::new(catalog.clone()));
        let left = {
            let service = service.clone();
            tokio::spawn(async move { service.reject_match(candidate.id).await })
        };
        let right = {
            let service = service.clone();
            tokio::spawn(async move { service.reject_match(candidate.id).await })
        };
        let changed = [left.await.unwrap().unwrap(), right.await.unwrap().unwrap()];
        assert_eq!(changed.into_iter().filter(|value| *value).count(), 1);
        assert_eq!(resolutions(&catalog, result.id).await, 1);
        assert_eq!(
            catalog
                .list_suppressions(
                    ReconciliationResolutionFilter {
                        result_id: Some(result.id),
                        ..Default::default()
                    },
                    RepositoryPageRequest::default(),
                )
                .await
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn locate_local_file_hashes_and_associates_a_real_file() {
        let catalog = catalog().await;
        let (track, _, primary, candidate, mut result) = seed_candidate(&catalog, false).await;
        result
            .actions
            .push(ReconciliationAction::Locate { track_id: track.id });
        catalog.upsert_result(result.clone()).await.unwrap();
        let path = std::env::temp_dir().join(format!("rec-bag-locate-{}.flac", Uuid::new_v4()));
        std::fs::write(&path, b"located audio").unwrap();
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service
            .locate_local_file(candidate.id, track.id, &path)
            .await
            .unwrap());
        let canonical = std::fs::canonicalize(&path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let location = catalog
            .get_location(primary.id, canonical)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(location.observed_hash, Some(sha256(b"located audio")));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn unknown_object_can_be_associated_imported_or_ignored() {
        let catalog = catalog().await;
        let primary = storage(StorageRole::Primary);
        MusicStorageRepository::upsert(catalog.as_ref(), primary.clone())
            .await
            .unwrap();
        catalog
            .upsert_storage_object(object(primary.id, "unknown-a", b"a"))
            .await
            .unwrap();
        catalog
            .upsert_storage_object(object(primary.id, "unknown-b", b"b"))
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();

        let associate = ReconciliationResult::new(ReconciliationOutcome::UntrackedStorageObject)
            .with_storage_object(primary.id, "unknown-a")
            .with_action(ReconciliationAction::AssociateUnknownObject {
                result_id: Uuid::nil(),
                track_id: track.id,
                asset_id: None,
                storage_id: primary.id,
                storage_object_id: "unknown-a".into(),
            });
        let mut associate = associate;
        if let ReconciliationAction::AssociateUnknownObject { result_id, .. } =
            &mut associate.actions[0]
        {
            *result_id = associate.id;
        }
        catalog.upsert_result(associate.clone()).await.unwrap();

        let mut import = ReconciliationResult::new(ReconciliationOutcome::UntrackedStorageObject)
            .with_storage_object(primary.id, "unknown-b");
        import
            .actions
            .push(ReconciliationAction::ImportUnknownObject {
                result_id: import.id,
                storage_id: primary.id,
                storage_object_id: "unknown-b".into(),
            });
        catalog.upsert_result(import.clone()).await.unwrap();

        let mut ignore = ReconciliationResult::new(ReconciliationOutcome::UntrackedStorageObject)
            .with_storage_object(primary.id, "ignored");
        ignore.actions.push(ReconciliationAction::Ignore {
            result_id: ignore.id,
            reason: None,
        });
        catalog.upsert_result(ignore.clone()).await.unwrap();
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service
            .associate_unknown(associate.id, track.id)
            .await
            .unwrap());
        assert!(service.import_unknown(import.id).await.unwrap());
        assert!(service.ignore_unknown(ignore.id).await.unwrap());
        assert!(catalog
            .get_location(primary.id, "unknown-a".into())
            .await
            .unwrap()
            .is_some());
        assert!(catalog
            .get_location(primary.id, "unknown-b".into())
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            catalog
                .list_suppressions(
                    ReconciliationResolutionFilter {
                        result_id: Some(ignore.id),
                        ..Default::default()
                    },
                    RepositoryPageRequest::default(),
                )
                .await
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn external_change_versions_asset_and_plans_backup_atomically() {
        let catalog = catalog().await;
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let old = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "song.flac",
            3,
            sha256(b"old").value(),
        )
        .unwrap();
        catalog.upsert_original(old.clone()).await.unwrap();
        let primary = storage(StorageRole::Primary);
        let backup = storage(StorageRole::Backup);
        MusicStorageRepository::upsert(catalog.as_ref(), primary.clone())
            .await
            .unwrap();
        MusicStorageRepository::upsert(catalog.as_ref(), backup.clone())
            .await
            .unwrap();
        let changed = object(primary.id, "song", b"new bytes");
        catalog
            .upsert_storage_object(changed.clone())
            .await
            .unwrap();
        catalog
            .upsert_location(AssetLocation {
                id: Uuid::new_v4(),
                asset_id: old.id(),
                asset_type: AssetType::Original,
                storage_id: primary.id,
                object_id: "song".into(),
                verification_status: StorageVerificationStatus::ChangedPendingReview,
                expected_hash: old.content_hash.clone(),
                observed_hash: changed.canonical_hash().cloned(),
                provider_version: changed.provider_version.clone(),
                last_checked_at: Some(Utc::now()),
                verified_at: None,
                error_message: None,
            })
            .await
            .unwrap();
        let mut result = ReconciliationResult::new(ReconciliationOutcome::ChangedStorageObject)
            .with_track(track.id)
            .with_asset(old.id())
            .with_storage_object(primary.id, "song");
        result
            .actions
            .push(ReconciliationAction::ApproveExternalChange {
                result_id: result.id,
                storage_id: primary.id,
                storage_object_id: "song".into(),
            });
        catalog.upsert_result(result.clone()).await.unwrap();
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service.approve_external_change(result.id).await.unwrap());
        assert!(!service.approve_external_change(result.id).await.unwrap());
        let versions = catalog.list_originals_for_track(track.id).await.unwrap();
        assert_eq!(versions.len(), 2);
        let next = versions.iter().find(|asset| asset.version == 2).unwrap();
        assert_eq!(next.supersedes_asset_id, Some(old.id()));
        assert_eq!(next.content_hash, changed.canonical_hash().cloned());
        let runs = catalog
            .list_filtered(
                SyncRunFilter {
                    kind: Some(SyncRunKind::BackupReplication),
                    ..Default::default()
                },
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(runs.items.len(), 1);
        assert_eq!(catalog.list_jobs(runs.items[0].id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_external_approval_rolls_back_version_resolution_and_jobs() {
        let catalog = catalog().await;
        let (track, old, primary, _, _) = seed_candidate(&catalog, true).await;
        let no_hash = StorageObject {
            hashes: Vec::new(),
            ..object(primary.id, "changed", b"changed")
        };
        catalog.upsert_storage_object(no_hash).await.unwrap();
        catalog
            .upsert_location(AssetLocation {
                id: Uuid::new_v4(),
                asset_id: old.id(),
                asset_type: AssetType::Original,
                storage_id: primary.id,
                object_id: "changed".into(),
                verification_status: StorageVerificationStatus::ChangedPendingReview,
                expected_hash: old.content_hash.clone(),
                observed_hash: None,
                provider_version: Some("v2".into()),
                last_checked_at: None,
                verified_at: None,
                error_message: None,
            })
            .await
            .unwrap();
        let mut result = ReconciliationResult::new(ReconciliationOutcome::ChangedStorageObject)
            .with_track(track.id)
            .with_asset(old.id())
            .with_storage_object(primary.id, "changed");
        result
            .actions
            .push(ReconciliationAction::ApproveExternalChange {
                result_id: result.id,
                storage_id: primary.id,
                storage_object_id: "changed".into(),
            });
        catalog.upsert_result(result.clone()).await.unwrap();
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service.approve_external_change(result.id).await.is_err());
        assert_eq!(
            catalog
                .list_originals_for_track(track.id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(resolutions(&catalog, result.id).await, 0);
    }

    #[tokio::test]
    async fn retry_result_and_failed_run_are_durable_and_idempotent() {
        let catalog = catalog().await;
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let mut result = ReconciliationResult::new(ReconciliationOutcome::Track(
            TrackStorageStatus::MissingObtainable,
        ))
        .with_track(track.id);
        result.actions.push(ReconciliationAction::Retry {
            result_id: result.id,
        });
        catalog.upsert_result(result.clone()).await.unwrap();
        let service = ReconciliationActionsService::new(catalog.clone());

        assert!(service.retry_result(result.id).await.unwrap());
        assert!(!service.retry_result(result.id).await.unwrap());
        let reconciliation_runs = catalog
            .list_filtered(
                SyncRunFilter {
                    kind: Some(SyncRunKind::Reconciliation),
                    ..Default::default()
                },
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(reconciliation_runs.items.len(), 1);

        let mut failed = SyncRun::new(SyncRunKind::Reconciliation, None, None);
        failed.start().unwrap();
        failed
            .fail(SyncError::new(
                SyncErrorKind::TemporarilyUnavailable,
                "test.retry",
                "retry me",
                true,
            ))
            .unwrap();
        let mut job = SyncJob::new(
            failed.id,
            SyncJobPayload::Reconcile {
                version: SYNC_JOB_PAYLOAD_VERSION,
                track_id: track.id,
                source_id: None,
                source_item_id: None,
                storage_id: None,
                storage_object_id: None,
            },
        );
        job.checkpoint = Some(rec_core::SyncCheckpoint::new(7));
        let predecessor_job_id = job.id;
        let idempotency_key = job.idempotency_key.clone();
        let checkpoint = job.checkpoint.clone();
        SyncRunRepository::upsert(catalog.as_ref(), failed.clone())
            .await
            .unwrap();
        catalog.upsert_job(job).await.unwrap();
        let resumed = service.retry_run(failed.id).await.unwrap();
        assert_eq!(service.retry_run(failed.id).await.unwrap(), resumed);
        let resumed_run = SyncRunRepository::get(catalog.as_ref(), resumed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed_run.resumed_from_run_id, Some(failed.id));

        let predecessor = catalog.get_job(predecessor_job_id).await.unwrap().unwrap();
        assert_eq!(predecessor.status, SyncStatus::Cancelled);
        let resumed_jobs = catalog.list_jobs(resumed).await.unwrap();
        assert_eq!(resumed_jobs.len(), 1);
        assert_eq!(resumed_jobs[0].idempotency_key, idempotency_key);
        assert_eq!(resumed_jobs[0].checkpoint, checkpoint);
    }

    #[tokio::test]
    async fn handler_maps_every_owned_action_and_rejects_wrong_slice() {
        let catalog = catalog().await;
        let service = ReconciliationActionsService::new(catalog);
        let request = IntegrationRequest {
            id: 7,
            slice: IntegrationSlice::LocalLibrary,
            event: MusicManagementEvent::ConfirmMatch(Uuid::new_v4().to_string()),
        };
        let error = service.handle(request).await.unwrap_err();
        assert_eq!(error.slice, Some(IntegrationSlice::LocalLibrary));
        assert!(!error.retryable);

        let unsupported = IntegrationRequest {
            id: 8,
            slice: IntegrationSlice::ReconciliationActions,
            event: MusicManagementEvent::CopyToPrimary(Uuid::new_v4().to_string()),
        };
        assert!(service.handle(unsupported).await.is_err());
    }
}
