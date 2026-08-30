//! Persistent, provider-independent synchronization run and job contracts.

use crate::asset::ContentHash;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};
use uuid::Uuid;

/// The top-level operation represented by a sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRunKind {
    CatalogSync,
    StorageInventory,
    Reconciliation,
    AssetTransfer,
    BackupReplication,
    DatabaseBackup,
    FullSync,
}

/// A persistable unit of work within a sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncJobKind {
    FetchCatalogPage,
    InventoryStoragePage,
    Reconcile,
    AcquireSourceAsset,
    CopyAsset,
    UploadAsset,
    DownloadAsset,
    VerifyAsset,
    ReplicateAsset,
    BackUpDatabase,
}

/// The current durable schema version of every [`SyncJobPayload`] variant.
pub const SYNC_JOB_PAYLOAD_VERSION: u16 = 1;

/// Durable, provider-independent inputs needed to restart a sync job.
///
/// The explicit `kind` tag and per-variant `version` make payloads safe to store
/// independently and evolve without inferring their shape from [`SyncJobKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncJobPayload {
    FetchCatalogPage {
        version: u16,
        source_id: Uuid,
        /// Stable source collection name or provider-independent collection key.
        collection_key: String,
        /// High-water mark from the last fully committed collection sequence.
        sync_cursor: Option<SyncCursor>,
        /// Continuation within the collection sequence currently being fetched.
        page_cursor: Option<SyncCursor>,
    },
    InventoryStoragePage {
        version: u16,
        storage_id: Uuid,
        /// Optional root, prefix, or folder whose inventory is being enumerated.
        inventory_root: Option<String>,
        /// Durable provider change token from the last completed inventory sequence.
        change_cursor: Option<SyncCursor>,
        /// Continuation within the inventory sequence currently being fetched.
        page_cursor: Option<SyncCursor>,
    },
    Reconcile {
        version: u16,
        track_id: Uuid,
        source_id: Option<Uuid>,
        source_item_id: Option<String>,
        storage_id: Option<Uuid>,
        storage_object_id: Option<String>,
    },
    AcquireSourceAsset {
        version: u16,
        source_id: Uuid,
        source_asset_id: String,
        asset_id: Uuid,
        destination_storage_id: Option<Uuid>,
        destination_path: PathBuf,
    },
    CopyAsset {
        version: u16,
        asset_id: Uuid,
        source_storage_id: Uuid,
        source_object_id: String,
        source_object_key: Option<String>,
        destination_storage_id: Uuid,
        destination_object_key: String,
    },
    UploadAsset {
        version: u16,
        asset_id: Uuid,
        source_path: PathBuf,
        destination_storage_id: Uuid,
        destination_object_key: String,
    },
    DownloadAsset {
        version: u16,
        asset_id: Uuid,
        source_storage_id: Uuid,
        source_object_id: String,
        source_object_key: Option<String>,
        destination_path: PathBuf,
    },
    VerifyAsset {
        version: u16,
        asset_id: Uuid,
        storage_id: Uuid,
        object_id: String,
        object_key: Option<String>,
        expected_checksum: Option<String>,
    },
    ReplicateAsset {
        version: u16,
        asset_id: Uuid,
        source_storage_id: Uuid,
        source_object_id: String,
        source_object_key: Option<String>,
        /// Immutable source generation captured when the job is planned.
        #[serde(default)]
        source_provider_version: Option<String>,
        /// Immutable canonical content identity captured when the job is planned.
        #[serde(default)]
        expected_checksum: Option<ContentHash>,
        destination_storage_id: Uuid,
        destination_object_key: String,
    },
    /// Uploads a consistent copy of the catalog database file to a backup storage.
    BackUpDatabase {
        version: u16,
        destination_storage_id: Uuid,
        destination_object_key: String,
    },
}

impl SyncJobPayload {
    pub const fn kind(&self) -> SyncJobKind {
        match self {
            Self::FetchCatalogPage { .. } => SyncJobKind::FetchCatalogPage,
            Self::InventoryStoragePage { .. } => SyncJobKind::InventoryStoragePage,
            Self::Reconcile { .. } => SyncJobKind::Reconcile,
            Self::AcquireSourceAsset { .. } => SyncJobKind::AcquireSourceAsset,
            Self::CopyAsset { .. } => SyncJobKind::CopyAsset,
            Self::UploadAsset { .. } => SyncJobKind::UploadAsset,
            Self::DownloadAsset { .. } => SyncJobKind::DownloadAsset,
            Self::VerifyAsset { .. } => SyncJobKind::VerifyAsset,
            Self::ReplicateAsset { .. } => SyncJobKind::ReplicateAsset,
            Self::BackUpDatabase { .. } => SyncJobKind::BackUpDatabase,
        }
    }

    pub const fn version(&self) -> u16 {
        match self {
            Self::FetchCatalogPage { version, .. }
            | Self::InventoryStoragePage { version, .. }
            | Self::Reconcile { version, .. }
            | Self::AcquireSourceAsset { version, .. }
            | Self::CopyAsset { version, .. }
            | Self::UploadAsset { version, .. }
            | Self::DownloadAsset { version, .. }
            | Self::VerifyAsset { version, .. }
            | Self::ReplicateAsset { version, .. }
            | Self::BackUpDatabase { version, .. } => *version,
        }
    }

    pub const fn has_supported_version(&self) -> bool {
        self.version() == SYNC_JOB_PAYLOAD_VERSION
    }
}

/// Lifecycle shared by runs and jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Pending,
    Running,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl SyncStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub const fn can_start(self) -> bool {
        matches!(self, Self::Pending | Self::Paused)
    }
}

/// An opaque provider pagination cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

impl SyncCursor {
    pub fn new(value: impl Into<String>) -> Self {
        Self::at(value, Utc::now())
    }

    pub fn at(value: impl Into<String>, updated_at: DateTime<Utc>) -> Self {
        Self {
            value: value.into(),
            updated_at,
        }
    }
}

/// Durable stage of a portable asset transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStage {
    PendingDownload,
    DownloadVerified,
    Uploaded,
    DestinationVerified,
}

/// Immutable transfer identity plus the latest durable stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCheckpoint {
    pub stage: TransferStage,
    pub expected_checksum: ContentHash,
    #[serde(default)]
    pub source_provider_version: Option<String>,
    #[serde(default)]
    pub destination_object_id: Option<String>,
}

impl TransferCheckpoint {
    pub fn new(expected_checksum: ContentHash, source_provider_version: Option<String>) -> Self {
        Self {
            stage: TransferStage::PendingDownload,
            expected_checksum,
            source_provider_version,
            destination_object_id: None,
        }
    }

    /// Advance without permitting a retry to substitute a different source identity.
    pub fn advance(
        &self,
        stage: TransferStage,
        expected_checksum: &ContentHash,
        source_provider_version: Option<&str>,
        destination_object_id: Option<String>,
    ) -> Result<Self, TransferCheckpointError> {
        if &self.expected_checksum != expected_checksum
            || self.source_provider_version.as_deref() != source_provider_version
        {
            return Err(TransferCheckpointError::SourceIdentityChanged);
        }
        if stage < self.stage {
            return Err(TransferCheckpointError::StageRegression);
        }
        Ok(Self {
            stage,
            expected_checksum: self.expected_checksum.clone(),
            source_provider_version: self.source_provider_version.clone(),
            destination_object_id: destination_object_id
                .or_else(|| self.destination_object_id.clone()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCheckpointError {
    SourceIdentityChanged,
    StageRegression,
}

impl fmt::Display for TransferCheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceIdentityChanged => formatter.write_str("transfer source identity changed"),
            Self::StageRegression => formatter.write_str("transfer stage cannot move backwards"),
        }
    }
}

impl std::error::Error for TransferCheckpointError {}

/// Durable state from which interrupted work can continue.
///
/// `sync_cursor` is the authoritative high-water mark and must only advance
/// after the entire collection/page sequence has been committed. While that
/// sequence is in progress, `page_cursor` identifies its next page and the
/// previous `sync_cursor` remains authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCheckpoint {
    pub sync_cursor: Option<SyncCursor>,
    pub page_cursor: Option<SyncCursor>,
    pub last_item_id: Option<String>,
    pub processed_items: u64,
    pub created_at: DateTime<Utc>,
    /// Present for staged asset-copy jobs; absent in legacy and non-transfer checkpoints.
    #[serde(default)]
    pub transfer: Option<TransferCheckpoint>,
}

impl SyncCheckpoint {
    pub fn new(processed_items: u64) -> Self {
        Self::at(processed_items, Utc::now())
    }

    pub fn at(processed_items: u64, created_at: DateTime<Utc>) -> Self {
        Self {
            sync_cursor: None,
            page_cursor: None,
            last_item_id: None,
            processed_items,
            created_at,
            transfer: None,
        }
    }

    /// Preserve the high-water mark of the last fully committed sequence.
    pub fn with_sync_cursor(mut self, sync_cursor: SyncCursor) -> Self {
        self.sync_cursor = Some(sync_cursor);
        self
    }

    /// Record the next page in the collection sequence currently in progress.
    pub fn with_page_cursor(mut self, page_cursor: SyncCursor) -> Self {
        self.page_cursor = Some(page_cursor);
        self
    }

    /// Advance the authoritative high-water mark after committing a full
    /// collection/page sequence and clear its now-consumed page continuation.
    pub fn with_completed_sequence(mut self, sync_cursor: SyncCursor) -> Self {
        self.sync_cursor = Some(sync_cursor);
        self.page_cursor = None;
        self
    }

    pub fn with_last_item(mut self, item_id: impl Into<String>) -> Self {
        self.last_item_id = Some(item_id.into());
        self
    }

    pub fn with_transfer(mut self, transfer: TransferCheckpoint) -> Self {
        self.transfer = Some(transfer);
        self
    }
}

/// Monotonic progress counters for a run or job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProgress {
    pub discovered_items: u64,
    pub total_items: Option<u64>,
    pub processed_items: u64,
    pub succeeded_items: u64,
    pub skipped_items: u64,
    pub failed_items: u64,
    pub bytes_transferred: u64,
}

impl SyncProgress {
    pub fn new(total_items: Option<u64>) -> Self {
        Self {
            total_items,
            ..Self::default()
        }
    }

    /// Increase the discovered count and keep a known total from falling below it.
    pub fn discover(&mut self, count: u64) {
        self.discovered_items = self.discovered_items.saturating_add(count);
        if let Some(total) = &mut self.total_items {
            *total = (*total).max(self.discovered_items);
        }
    }

    /// Set an estimate without allowing it below already observed work.
    pub fn set_total(&mut self, total: u64) {
        self.total_items = Some(total.max(self.discovered_items).max(self.processed_items));
    }

    pub fn record_success(&mut self) {
        self.processed_items = self.processed_items.saturating_add(1);
        self.succeeded_items = self.succeeded_items.saturating_add(1);
        self.ensure_observed_bounds();
    }

    pub fn record_skip(&mut self) {
        self.processed_items = self.processed_items.saturating_add(1);
        self.skipped_items = self.skipped_items.saturating_add(1);
        self.ensure_observed_bounds();
    }

    pub fn record_failure(&mut self) {
        self.processed_items = self.processed_items.saturating_add(1);
        self.failed_items = self.failed_items.saturating_add(1);
        self.ensure_observed_bounds();
    }

    pub fn add_bytes(&mut self, bytes: u64) {
        self.bytes_transferred = self.bytes_transferred.saturating_add(bytes);
    }

    /// Completed work as a fraction of the known total, when a non-zero total exists.
    pub fn fraction(&self) -> Option<f64> {
        self.total_items
            .filter(|total| *total != 0)
            .map(|total| (self.processed_items.min(total) as f64) / total as f64)
    }

    pub const fn outcome_count(&self) -> u64 {
        self.succeeded_items
            .saturating_add(self.skipped_items)
            .saturating_add(self.failed_items)
    }

    pub const fn is_consistent(&self) -> bool {
        self.processed_items == self.outcome_count()
            && self.discovered_items >= self.processed_items
    }

    fn ensure_observed_bounds(&mut self) {
        self.discovered_items = self.discovered_items.max(self.processed_items);
        if let Some(total) = &mut self.total_items {
            *total = (*total).max(self.discovered_items);
        }
    }
}

/// Broad machine-readable classification for a sync failure or warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorKind {
    Authentication,
    Authorization,
    RateLimited,
    TemporarilyUnavailable,
    NotFound,
    Conflict,
    InvalidData,
    Integrity,
    Io,
    Provider,
    Cancelled,
    Internal,
}

/// Severity of a structured issue attached to a run or job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncIssueSeverity {
    Info,
    Warning,
    Error,
}

/// A structured, persistable issue record suitable for UI and retry decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncIssue {
    pub id: Uuid,
    pub severity: SyncIssueSeverity,
    pub kind: SyncErrorKind,
    pub code: String,
    pub message: String,
    pub item_id: Option<String>,
    pub retryable: bool,
    pub occurred_at: DateTime<Utc>,
}

impl SyncIssue {
    pub fn new(
        severity: SyncIssueSeverity,
        kind: SyncErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self::at(severity, kind, code, message, retryable, Utc::now())
    }

    pub fn at(
        severity: SyncIssueSeverity,
        kind: SyncErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            severity,
            kind,
            code: code.into(),
            message: message.into(),
            item_id: None,
            retryable,
            occurred_at,
        }
    }

    pub fn for_item(mut self, item_id: impl Into<String>) -> Self {
        self.item_id = Some(item_id.into());
        self
    }
}

/// The terminal error associated with a failed run or job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncError {
    pub kind: SyncErrorKind,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    /// Provider-supplied minimum retry delay, when available.
    #[serde(default)]
    pub retry_after_seconds: Option<u64>,
    pub occurred_at: DateTime<Utc>,
}

impl SyncError {
    pub fn new(
        kind: SyncErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self::at(kind, code, message, retryable, Utc::now())
    }

    pub fn at(
        kind: SyncErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
            retryable,
            retry_after_seconds: None,
            occurred_at,
        }
    }

    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }
}

/// Rejected lifecycle operation. Failed transitions never partially mutate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncTransitionError {
    pub from: SyncStatus,
    pub operation: &'static str,
}

impl fmt::Display for SyncTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot {} a sync in {:?} status",
            self.operation, self.from
        )
    }
}

impl std::error::Error for SyncTransitionError {}

/// A persisted synchronization run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRun {
    pub id: Uuid,
    pub kind: SyncRunKind,
    pub status: SyncStatus,
    pub source_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
    pub progress: SyncProgress,
    /// High-water mark of the last fully committed collection/page sequence.
    pub sync_cursor: Option<SyncCursor>,
    /// Continuation within the collection/page sequence currently in progress.
    pub page_cursor: Option<SyncCursor>,
    pub checkpoint: Option<SyncCheckpoint>,
    pub issues: Vec<SyncIssue>,
    pub error: Option<SyncError>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub cancellation_requested_at: Option<DateTime<Utc>>,
    /// The prior run from which this run resumed, if any.
    pub resumed_from_run_id: Option<Uuid>,
    /// The checkpoint copied from the prior run when this run was created.
    pub resume_from: Option<SyncCheckpoint>,
    pub resume_count: u32,
}

impl SyncRun {
    pub fn new(kind: SyncRunKind, source_id: Option<Uuid>, storage_id: Option<Uuid>) -> Self {
        Self::at(kind, source_id, storage_id, Utc::now())
    }

    pub fn at(
        kind: SyncRunKind,
        source_id: Option<Uuid>,
        storage_id: Option<Uuid>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            status: SyncStatus::Pending,
            source_id,
            storage_id,
            progress: SyncProgress::default(),
            sync_cursor: None,
            page_cursor: None,
            checkpoint: None,
            issues: Vec::new(),
            error: None,
            created_at,
            started_at: None,
            updated_at: created_at,
            finished_at: None,
            cancellation_requested_at: None,
            resumed_from_run_id: None,
            resume_from: None,
            resume_count: 0,
        }
    }

    /// Create a fresh pending run that resumes the latest durable checkpoint.
    pub fn resumed_from(previous: &Self) -> Result<Self, SyncTransitionError> {
        Self::resumed_from_at(previous, Utc::now())
    }

    pub fn resumed_from_at(
        previous: &Self,
        created_at: DateTime<Utc>,
    ) -> Result<Self, SyncTransitionError> {
        if !matches!(
            previous.status,
            SyncStatus::Paused | SyncStatus::Failed | SyncStatus::Cancelled
        ) {
            return Err(SyncTransitionError {
                from: previous.status,
                operation: "resume from",
            });
        }

        let created_at = created_at.max(previous.updated_at);
        let mut run = Self::at(
            previous.kind,
            previous.source_id,
            previous.storage_id,
            created_at,
        );
        run.resumed_from_run_id = Some(previous.id);
        run.resume_from = previous.checkpoint.clone();
        run.checkpoint = previous.checkpoint.clone();
        run.sync_cursor = previous.sync_cursor.clone();
        run.page_cursor = previous.page_cursor.clone();
        run.progress = previous.progress;
        run.resume_count = previous.resume_count.saturating_add(1);
        Ok(run)
    }

    pub fn start(&mut self) -> Result<(), SyncTransitionError> {
        self.start_at(Utc::now())
    }

    pub fn start_at(&mut self, at: DateTime<Utc>) -> Result<(), SyncTransitionError> {
        if !self.status.can_start() {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "start",
            });
        }
        let at = at.max(self.updated_at);
        self.status = SyncStatus::Running;
        self.started_at.get_or_insert(at);
        self.updated_at = at;
        self.finished_at = None;
        Ok(())
    }

    /// Replace the complete durable cursor snapshot for this run.
    ///
    /// Callers must retain the prior `sync_cursor` in in-progress checkpoints;
    /// it should only be replaced after committing the completed page sequence.
    pub fn update_checkpoint(&mut self, checkpoint: SyncCheckpoint) {
        self.sync_cursor = checkpoint.sync_cursor.clone();
        self.page_cursor = checkpoint.page_cursor.clone();
        self.updated_at = self.updated_at.max(checkpoint.created_at);
        self.checkpoint = Some(checkpoint);
    }

    pub fn add_issue(&mut self, issue: SyncIssue) {
        self.updated_at = self.updated_at.max(issue.occurred_at);
        self.issues.push(issue);
    }

    pub fn pause(&mut self, checkpoint: SyncCheckpoint) -> Result<(), SyncTransitionError> {
        if self.status != SyncStatus::Running {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "pause",
            });
        }
        self.update_checkpoint(checkpoint);
        self.status = SyncStatus::Paused;
        Ok(())
    }

    pub fn request_cancellation(&mut self) -> Result<(), SyncTransitionError> {
        self.request_cancellation_at(Utc::now())
    }

    pub fn request_cancellation_at(
        &mut self,
        at: DateTime<Utc>,
    ) -> Result<(), SyncTransitionError> {
        if self.status.is_terminal() {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "request cancellation for",
            });
        }
        let at = at.max(self.updated_at);
        self.cancellation_requested_at.get_or_insert(at);
        self.status = SyncStatus::Cancelling;
        self.updated_at = at;
        Ok(())
    }

    pub fn succeed(&mut self) -> Result<(), SyncTransitionError> {
        self.succeed_at(Utc::now())
    }

    pub fn succeed_at(&mut self, at: DateTime<Utc>) -> Result<(), SyncTransitionError> {
        if self.status != SyncStatus::Running {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "complete",
            });
        }
        self.finish(SyncStatus::Succeeded, at);
        Ok(())
    }

    pub fn fail(&mut self, error: SyncError) -> Result<(), SyncTransitionError> {
        if self.status.is_terminal() {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "fail",
            });
        }
        let at = error.occurred_at;
        self.error = Some(error);
        self.finish(SyncStatus::Failed, at);
        Ok(())
    }

    pub fn mark_cancelled(&mut self) -> Result<(), SyncTransitionError> {
        self.mark_cancelled_at(Utc::now())
    }

    pub fn mark_cancelled_at(&mut self, at: DateTime<Utc>) -> Result<(), SyncTransitionError> {
        if self.status != SyncStatus::Cancelling {
            return Err(SyncTransitionError {
                from: self.status,
                operation: "cancel",
            });
        }
        self.finish(SyncStatus::Cancelled, at);
        Ok(())
    }

    pub const fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    fn finish(&mut self, status: SyncStatus, at: DateTime<Utc>) {
        debug_assert!(status.is_terminal());
        let at = at.max(self.updated_at);
        self.status = status;
        self.updated_at = at;
        self.finished_at = Some(at);
    }
}

/// Default number of worker claims permitted for a sync job.
pub const DEFAULT_SYNC_JOB_MAX_ATTEMPTS: u32 = 3;

/// A persisted job whose indexed kind disagrees with its tagged payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncJobKindMismatch {
    pub job_kind: SyncJobKind,
    pub payload_kind: SyncJobKind,
}

impl fmt::Display for SyncJobKindMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "sync job kind {:?} does not match payload kind {:?}",
            self.job_kind, self.payload_kind
        )
    }
}

impl std::error::Error for SyncJobKindMismatch {}

/// A persistable job belonging to a synchronization run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncJob {
    pub id: Uuid,
    pub run_id: Uuid,
    /// Persisted separately for efficient indexing; constructors derive it from `payload`.
    pub kind: SyncJobKind,
    pub payload: SyncJobPayload,
    pub status: SyncStatus,
    pub progress: SyncProgress,
    pub checkpoint: Option<SyncCheckpoint>,
    pub issues: Vec<SyncIssue>,
    pub error: Option<SyncError>,
    /// Number of worker claims, including the currently running attempt.
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Stable across retries so providers and repositories can deduplicate effects.
    pub idempotency_key: String,
    /// Worker currently entitled to mutate this job, when claimed.
    pub lease_owner: Option<String>,
    /// A different worker may reclaim the job after this instant.
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl SyncJob {
    pub fn new(run_id: Uuid, payload: SyncJobPayload) -> Self {
        Self::with_retry_policy(
            run_id,
            payload,
            DEFAULT_SYNC_JOB_MAX_ATTEMPTS,
            None::<String>,
        )
    }

    /// Construct a job with an explicit retry budget and optional stable key.
    /// When omitted, the idempotency key is derived from the new job ID.
    pub fn with_retry_policy(
        run_id: Uuid,
        payload: SyncJobPayload,
        max_attempts: u32,
        idempotency_key: Option<impl Into<String>>,
    ) -> Self {
        Self::with_retry_policy_at(run_id, payload, max_attempts, idempotency_key, Utc::now())
    }

    pub fn with_retry_policy_at(
        run_id: Uuid,
        payload: SyncJobPayload,
        max_attempts: u32,
        idempotency_key: Option<impl Into<String>>,
        created_at: DateTime<Utc>,
    ) -> Self {
        let id = Uuid::new_v4();
        let idempotency_key = idempotency_key
            .map(Into::into)
            .unwrap_or_else(|| format!("sync-job:{id}"));

        Self {
            id,
            run_id,
            kind: payload.kind(),
            payload,
            status: SyncStatus::Pending,
            progress: SyncProgress::default(),
            checkpoint: None,
            issues: Vec::new(),
            error: None,
            attempt_count: 0,
            max_attempts: max_attempts.max(1),
            next_attempt_at: None,
            idempotency_key,
            lease_owner: None,
            lease_expires_at: None,
            created_at,
            started_at: None,
            updated_at: created_at,
            finished_at: None,
        }
    }

    /// Validate data loaded from persistence before dispatching it to a worker.
    pub fn validate_kind(&self) -> Result<(), SyncJobKindMismatch> {
        let payload_kind = self.payload.kind();
        if self.kind == payload_kind {
            Ok(())
        } else {
            Err(SyncJobKindMismatch {
                job_kind: self.kind,
                payload_kind,
            })
        }
    }

    pub const fn attempts_exhausted(&self) -> bool {
        self.attempt_count >= self.max_attempts
    }

    pub fn has_active_lease_at(&self, at: DateTime<Utc>) -> bool {
        self.lease_owner.is_some()
            && self
                .lease_expires_at
                .is_some_and(|lease_expires_at| lease_expires_at > at)
    }

    /// Whether a pending job or an abandoned running job may be claimed.
    pub fn is_claimable_at(&self, at: DateTime<Utc>) -> bool {
        if self.attempts_exhausted()
            || self
                .next_attempt_at
                .is_some_and(|next_attempt_at| next_attempt_at > at)
            || self.has_active_lease_at(at)
        {
            return false;
        }

        matches!(self.status, SyncStatus::Pending | SyncStatus::Paused)
            || (self.status == SyncStatus::Running
                && self
                    .lease_expires_at
                    .is_none_or(|lease_expires_at| lease_expires_at <= at))
    }

    /// Claim an attempt. Returns `false` if the job is not due, has an active
    /// lease, has exhausted retries, or the requested lease is invalid.
    pub fn claim_at(
        &mut self,
        worker: impl Into<String>,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> bool {
        let claimed_at = claimed_at.max(self.updated_at);
        let worker = worker.into();
        if worker.is_empty() || lease_expires_at <= claimed_at || !self.is_claimable_at(claimed_at)
        {
            return false;
        }

        self.attempt_count = self.attempt_count.saturating_add(1);
        self.next_attempt_at = None;
        self.lease_owner = Some(worker);
        self.lease_expires_at = Some(lease_expires_at);
        self.status = SyncStatus::Running;
        self.started_at.get_or_insert(claimed_at);
        self.updated_at = claimed_at;
        self.finished_at = None;
        true
    }

    /// Return a claimed job to the pending queue without changing its stable
    /// idempotency key. Returns `false` once its retry budget is exhausted.
    pub fn schedule_retry_at(
        &mut self,
        next_attempt_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> bool {
        if self.attempts_exhausted()
            || matches!(self.status, SyncStatus::Succeeded | SyncStatus::Cancelled)
        {
            return false;
        }

        self.status = SyncStatus::Pending;
        self.next_attempt_at = Some(next_attempt_at);
        self.lease_owner = None;
        self.lease_expires_at = None;
        self.updated_at = self.updated_at.max(updated_at);
        self.finished_at = None;
        true
    }

    /// Release a lease only when it is still owned by `worker`.
    pub fn release_lease_at(&mut self, worker: &str, released_at: DateTime<Utc>) -> bool {
        if self.lease_owner.as_deref() != Some(worker) {
            return false;
        }

        self.lease_owner = None;
        self.lease_expires_at = None;
        self.updated_at = self.updated_at.max(released_at);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn time(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, second)
            .single()
            .unwrap()
    }

    #[test]
    fn progress_helpers_keep_counters_consistent() {
        let mut progress = SyncProgress::new(Some(3));
        progress.discover(3);
        progress.record_success();
        progress.record_skip();
        progress.record_failure();
        progress.add_bytes(512);

        assert_eq!(progress.processed_items, 3);
        assert_eq!(progress.outcome_count(), 3);
        assert_eq!(progress.fraction(), Some(1.0));
        assert_eq!(progress.bytes_transferred, 512);
        assert!(progress.is_consistent());
    }

    #[test]
    fn terminal_statuses_cannot_be_restarted_or_cancelled() {
        for status in [
            SyncStatus::Succeeded,
            SyncStatus::Failed,
            SyncStatus::Cancelled,
        ] {
            assert!(status.is_terminal());
        }

        let mut run = SyncRun::at(SyncRunKind::CatalogSync, None, None, time(0));
        run.start_at(time(1)).unwrap();
        run.succeed_at(time(2)).unwrap();

        assert!(run.is_terminal());
        assert_eq!(run.finished_at, Some(time(2)));
        assert!(run.start_at(time(3)).is_err());
        assert!(run.request_cancellation_at(time(3)).is_err());
        assert_eq!(run.status, SyncStatus::Succeeded);
    }

    #[test]
    fn pause_and_resume_preserve_both_cursors() {
        let source_id = Uuid::new_v4();
        let mut run = SyncRun::at(SyncRunKind::CatalogSync, Some(source_id), None, time(0));
        run.start_at(time(1)).unwrap();
        let sync_cursor = SyncCursor::at("last-complete-sequence", time(1));
        let page_cursor = SyncCursor::at("next-page", time(2));
        let checkpoint = SyncCheckpoint::at(4, time(2))
            .with_sync_cursor(sync_cursor.clone())
            .with_page_cursor(page_cursor.clone());
        run.pause(checkpoint.clone()).unwrap();

        let resumed = SyncRun::resumed_from_at(&run, time(3)).unwrap();

        assert_eq!(resumed.status, SyncStatus::Pending);
        assert_eq!(resumed.source_id, Some(source_id));
        assert_eq!(resumed.resumed_from_run_id, Some(run.id));
        assert_eq!(resumed.resume_from, Some(checkpoint));
        assert_eq!(resumed.sync_cursor, Some(sync_cursor));
        assert_eq!(resumed.page_cursor, Some(page_cursor));
        assert_eq!(resumed.progress, run.progress);
        assert!(resumed.progress.is_consistent());
        assert_eq!(resumed.cancellation_requested_at, None);
    }

    #[test]
    fn completed_sequence_advances_sync_cursor_and_clears_page_cursor() {
        let completed_cursor = SyncCursor::at("completed", time(2));
        let checkpoint = SyncCheckpoint::at(10, time(2))
            .with_page_cursor(SyncCursor::at("consumed-page", time(1)))
            .with_completed_sequence(completed_cursor.clone());
        let mut run = SyncRun::at(SyncRunKind::CatalogSync, None, None, time(0));

        run.update_checkpoint(checkpoint);

        assert_eq!(run.sync_cursor, Some(completed_cursor));
        assert_eq!(run.page_cursor, None);
    }

    #[test]
    fn payload_variants_are_versioned_tagged_and_derive_job_kind() {
        let source_id = Uuid::new_v4();
        let source_storage_id = Uuid::new_v4();
        let destination_storage_id = Uuid::new_v4();
        let asset_id = Uuid::new_v4();
        let track_id = Uuid::new_v4();
        let payloads = vec![
            (
                SyncJobPayload::FetchCatalogPage {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    source_id,
                    collection_key: "library".to_owned(),
                    sync_cursor: Some(SyncCursor::at("sync", time(1))),
                    page_cursor: Some(SyncCursor::at("page", time(1))),
                },
                SyncJobKind::FetchCatalogPage,
                "fetch_catalog_page",
            ),
            (
                SyncJobPayload::InventoryStoragePage {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    storage_id: source_storage_id,
                    inventory_root: Some("music/".to_owned()),
                    change_cursor: Some(SyncCursor::at("change", time(1))),
                    page_cursor: Some(SyncCursor::at("page", time(1))),
                },
                SyncJobKind::InventoryStoragePage,
                "inventory_storage_page",
            ),
            (
                SyncJobPayload::Reconcile {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    track_id,
                    source_id: Some(source_id),
                    source_item_id: Some("source-track".to_owned()),
                    storage_id: Some(source_storage_id),
                    storage_object_id: Some("stored-track".to_owned()),
                },
                SyncJobKind::Reconcile,
                "reconcile",
            ),
            (
                SyncJobPayload::AcquireSourceAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    source_id,
                    source_asset_id: "source-asset".to_owned(),
                    asset_id,
                    destination_storage_id: Some(destination_storage_id),
                    destination_path: PathBuf::from("staging/source.flac"),
                },
                SyncJobKind::AcquireSourceAsset,
                "acquire_source_asset",
            ),
            (
                SyncJobPayload::CopyAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id,
                    source_storage_id,
                    source_object_id: "source-object".to_owned(),
                    source_object_key: Some("source/key".to_owned()),
                    destination_storage_id,
                    destination_object_key: "destination/key".to_owned(),
                },
                SyncJobKind::CopyAsset,
                "copy_asset",
            ),
            (
                SyncJobPayload::UploadAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id,
                    source_path: PathBuf::from("staging/upload.flac"),
                    destination_storage_id,
                    destination_object_key: "uploads/asset".to_owned(),
                },
                SyncJobKind::UploadAsset,
                "upload_asset",
            ),
            (
                SyncJobPayload::DownloadAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id,
                    source_storage_id,
                    source_object_id: "download-object".to_owned(),
                    source_object_key: Some("downloads/source".to_owned()),
                    destination_path: PathBuf::from("staging/download.flac"),
                },
                SyncJobKind::DownloadAsset,
                "download_asset",
            ),
            (
                SyncJobPayload::VerifyAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id,
                    storage_id: destination_storage_id,
                    object_id: "verify-object".to_owned(),
                    object_key: Some("verify/key".to_owned()),
                    expected_checksum: Some("sha256:abc".to_owned()),
                },
                SyncJobKind::VerifyAsset,
                "verify_asset",
            ),
            (
                SyncJobPayload::ReplicateAsset {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    asset_id,
                    source_storage_id,
                    source_object_id: "replication-source".to_owned(),
                    source_object_key: Some("primary/asset".to_owned()),
                    source_provider_version: Some("v1".to_owned()),
                    expected_checksum: None,
                    destination_storage_id,
                    destination_object_key: "backup/asset".to_owned(),
                },
                SyncJobKind::ReplicateAsset,
                "replicate_asset",
            ),
            (
                SyncJobPayload::BackUpDatabase {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    destination_storage_id,
                    destination_object_key: "catalog-backups/library.db".to_owned(),
                },
                SyncJobKind::BackUpDatabase,
                "back_up_database",
            ),
        ];

        for (payload, expected_kind, expected_tag) in payloads {
            assert_eq!(payload.kind(), expected_kind);
            assert!(payload.has_supported_version());
            let serialized = serde_json::to_value(&payload).unwrap();
            assert_eq!(serialized["kind"], expected_tag);
            assert_eq!(serialized["version"], SYNC_JOB_PAYLOAD_VERSION);

            let job = SyncJob::new(Uuid::new_v4(), payload);
            assert_eq!(job.kind, expected_kind);
            assert!(job.validate_kind().is_ok());
        }
    }

    #[test]
    fn job_claims_enforce_retry_schedule_and_preserve_idempotency() {
        let payload = SyncJobPayload::UploadAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: Uuid::new_v4(),
            source_path: PathBuf::from("staging/upload.flac"),
            destination_storage_id: Uuid::new_v4(),
            destination_object_key: "music/upload.flac".to_owned(),
        };
        let mut job = SyncJob::with_retry_policy_at(
            Uuid::new_v4(),
            payload,
            2,
            Some("stable-operation-key"),
            time(0),
        );

        assert_eq!(job.kind, SyncJobKind::UploadAsset);
        assert_eq!(job.attempt_count, 0);
        assert_eq!(job.max_attempts, 2);
        assert_eq!(job.idempotency_key, "stable-operation-key");
        assert!(job.claim_at("worker-a", time(1), time(2)));
        assert_eq!(job.attempt_count, 1);
        assert!(job.has_active_lease_at(time(1)));
        assert!(!job.claim_at("worker-b", time(1), time(3)));

        assert!(job.schedule_retry_at(time(4), time(2)));
        assert_eq!(job.idempotency_key, "stable-operation-key");
        assert_eq!(job.lease_owner, None);
        assert!(!job.claim_at("worker-b", time(3), time(5)));
        assert!(job.claim_at("worker-b", time(4), time(5)));
        assert_eq!(job.attempt_count, 2);
        assert!(job.attempts_exhausted());
        assert!(!job.schedule_retry_at(time(6), time(5)));
    }

    #[test]
    fn legacy_payload_checkpoint_and_error_json_remain_readable() {
        let asset_id = Uuid::new_v4();
        let source_storage_id = Uuid::new_v4();
        let destination_storage_id = Uuid::new_v4();
        let payload_json = format!(
            r#"{{"kind":"replicate_asset","version":1,"asset_id":"{asset_id}","source_storage_id":"{source_storage_id}","source_object_id":"source","source_object_key":null,"destination_storage_id":"{destination_storage_id}","destination_object_key":"target"}}"#
        );
        let payload: SyncJobPayload = serde_json::from_str(&payload_json).unwrap();
        assert!(matches!(
            payload,
            SyncJobPayload::ReplicateAsset {
                source_provider_version: None,
                expected_checksum: None,
                ..
            }
        ));

        let checkpoint_json = format!(
            r#"{{"sync_cursor":null,"page_cursor":null,"last_item_id":null,"processed_items":0,"created_at":"{}"}}"#,
            time(0).to_rfc3339()
        );
        let checkpoint: SyncCheckpoint = serde_json::from_str(&checkpoint_json).unwrap();
        assert_eq!(checkpoint.transfer, None);

        let error_json = format!(
            r#"{{"kind":"provider","code":"old","message":"legacy","retryable":true,"occurred_at":"{}"}}"#,
            time(0).to_rfc3339()
        );
        let error: SyncError = serde_json::from_str(&error_json).unwrap();
        assert_eq!(error.retry_after_seconds, None);
    }

    #[test]
    fn transfer_checkpoint_rejects_source_changes_and_stage_regression() {
        let hash = ContentHash::sha256("a".repeat(64)).unwrap();
        let pending = TransferCheckpoint::new(hash.clone(), Some("v1".into()));
        let downloaded = pending
            .advance(TransferStage::DownloadVerified, &hash, Some("v1"), None)
            .unwrap();
        assert_eq!(downloaded.stage, TransferStage::DownloadVerified);
        assert_eq!(
            downloaded.advance(TransferStage::PendingDownload, &hash, Some("v1"), None),
            Err(TransferCheckpointError::StageRegression)
        );
        assert_eq!(
            downloaded.advance(
                TransferStage::Uploaded,
                &ContentHash::sha256("b".repeat(64)).unwrap(),
                Some("v1"),
                Some("object".into()),
            ),
            Err(TransferCheckpointError::SourceIdentityChanged)
        );
    }

    #[test]
    fn persisted_job_kind_mismatch_is_detected() {
        let payload = SyncJobPayload::BackUpDatabase {
            version: SYNC_JOB_PAYLOAD_VERSION,
            destination_storage_id: Uuid::new_v4(),
            destination_object_key: "catalog-backups/library.db".to_owned(),
        };
        let mut job = SyncJob::new(Uuid::new_v4(), payload);
        job.kind = SyncJobKind::Reconcile;

        assert_eq!(
            job.validate_kind(),
            Err(SyncJobKindMismatch {
                job_kind: SyncJobKind::Reconcile,
                payload_kind: SyncJobKind::BackUpDatabase,
            })
        );
    }
}
