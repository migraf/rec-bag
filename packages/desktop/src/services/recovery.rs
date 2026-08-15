//! Catalog snapshot restore and recovery workflows.
//!
//! Snapshot files are immutable JSON envelopes containing `CatalogSnapshot`. Verification checks
//! the envelope hash, schema/content type, payload shape, and the no-secrets policy. Restore uses
//! `TursoCatalog::replace_from_snapshot`, whose single database transaction is the atomic
//! replacement/rollback boundary. A durable operation marker and pre-restore snapshot make crashes diagnosable and
//! recoverable without ever copying a credential store.

use crate::services::profile_manager::{OwnershipLease, ProfileManager, TakeoverReason};
use chrono::{DateTime, Utc};
use rec_core::{CanonicalTrack, CatalogSnapshot, Setting, SettingsRepository, TrackRepository};
use rec_persistence::{TursoCatalog, SNAPSHOT_CONTENT_TYPE, SNAPSHOT_FORMAT_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use uuid::Uuid;

const SNAPSHOT_EXTENSION: &str = "catalog-snapshot.json";
const PRE_RESTORE_DIRECTORY: &str = "pre-restore";
const RESTORE_MARKER: &str = ".restore-in-progress.json";
const RESTORE_MARKER_VERSION: u32 = 1;
pub const CATALOG_AUTHORITY_SETTING_KEY: &str = "profile.authority.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogAuthority {
    pub profile_id: String,
    pub ownership_generation: u64,
    pub manager_device: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStage {
    Discovering,
    Verifying,
    DryRun,
    CreatingPreRestoreBackup,
    Restoring,
    RollingBack,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub stage: RecoveryStage,
    pub message: String,
    pub snapshot_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotHealth {
    Valid,
    Corrupt,
    Incompatible,
    ContainsSecrets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListing {
    pub path: PathBuf,
    pub id: Option<Uuid>,
    pub created_at: Option<DateTime<Utc>>,
    pub format_version: Option<u32>,
    pub health: SnapshotHealth,
    pub detail: Option<String>,
    pub authority: Option<CatalogAuthority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSnapshot {
    pub path: PathBuf,
    pub snapshot: CatalogSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored_snapshot_id: Uuid,
    pub pre_restore_snapshot: PathBuf,
    pub dry_run: bool,
    pub progress: Vec<RecoveryProgress>,
    pub foreign_authority: Option<CatalogAuthority>,
    pub transfer_confirmation_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOptions {
    pub dry_run: bool,
    /// Existing local-lease takeover path.
    pub ownership_handoff: Option<OwnershipHandoff>,
    /// Clean-device transfer path, bound to one exact snapshot authority observation.
    pub snapshot_confirmation: Option<SnapshotTransferConfirmation>,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            ownership_handoff: None,
            snapshot_confirmation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipHandoff {
    pub previous_device_id: String,
    pub lease: OwnershipLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotTransferConfirmation {
    pub snapshot_id: Uuid,
    pub observed_authority: CatalogAuthority,
    pub requesting_device: String,
    nonce: Uuid,
    valid_until: DateTime<Utc>,
}

impl SnapshotTransferConfirmation {
    pub fn valid_until(&self) -> DateTime<Utc> {
        self.valid_until
    }
}

#[derive(Debug, Clone)]
struct PendingSnapshotTransfer {
    snapshot_id: Uuid,
    observed_authority: CatalogAuthority,
    requesting_device: String,
    valid_until: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryErrorCode {
    Io,
    CorruptSnapshot,
    IncompatibleSnapshot,
    SecretMaterial,
    Catalog,
    Ownership,
    InterruptedRestore,
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("recovery I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("snapshot is corrupt: {0}")]
    CorruptSnapshot(String),
    #[error("snapshot schema is incompatible: {0}")]
    IncompatibleSnapshot(String),
    #[error("snapshot contains prohibited secret material at {0}")]
    SecretMaterial(String),
    #[error("catalog recovery failed: {0}")]
    Catalog(String),
    #[error("another-device restore requires a current, explicit ownership handoff: {0}")]
    Ownership(String),
    #[error("interrupted restore could not be recovered: {0}")]
    InterruptedRestore(String),
}

impl RecoveryError {
    pub fn code(&self) -> RecoveryErrorCode {
        match self {
            Self::Io { .. } => RecoveryErrorCode::Io,
            Self::CorruptSnapshot(_) => RecoveryErrorCode::CorruptSnapshot,
            Self::IncompatibleSnapshot(_) => RecoveryErrorCode::IncompatibleSnapshot,
            Self::SecretMaterial(_) => RecoveryErrorCode::SecretMaterial,
            Self::Catalog(_) => RecoveryErrorCode::Catalog,
            Self::Ownership(_) => RecoveryErrorCode::Ownership,
            Self::InterruptedRestore(_) => RecoveryErrorCode::InterruptedRestore,
        }
    }
}

#[derive(Clone)]
pub struct RecoveryService {
    catalog: Arc<TursoCatalog>,
    snapshot_root: PathBuf,
    snapshot_transfers: Arc<Mutex<BTreeMap<Uuid, PendingSnapshotTransfer>>>,
}

impl RecoveryService {
    pub fn new(catalog: Arc<TursoCatalog>, snapshot_root: impl Into<PathBuf>) -> Self {
        Self {
            catalog,
            snapshot_root: snapshot_root.into(),
            snapshot_transfers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn snapshot_root(&self) -> &Path {
        &self.snapshot_root
    }

    pub async fn persist_catalog_authority(
        &self,
        manager: &ProfileManager,
        lease: &OwnershipLease,
    ) -> Result<CatalogAuthority, RecoveryError> {
        if lease.record.session_id != manager.session_id()
            || lease.record.profile_id != manager.identity().profile_id
            || lease.record.device_id != manager.identity().device_id
        {
            return Err(RecoveryError::Ownership(
                "authority can only be persisted by the current lease holder".into(),
            ));
        }
        let previous = self.catalog_authority().await?;
        let ownership_generation = match previous {
            Some(ref authority)
                if authority.profile_id == manager.identity().profile_id
                    && authority.manager_device == manager.identity().device_id =>
            {
                authority.ownership_generation.max(1)
            }
            Some(authority) => authority.ownership_generation.saturating_add(1),
            None => 1,
        };
        let authority = CatalogAuthority {
            profile_id: manager.identity().profile_id.clone(),
            ownership_generation,
            manager_device: manager.identity().device_id.clone(),
        };
        self.write_catalog_authority(&authority).await?;
        Ok(authority)
    }

    pub async fn catalog_authority(&self) -> Result<Option<CatalogAuthority>, RecoveryError> {
        let setting = SettingsRepository::get(
            self.catalog.as_ref(),
            CATALOG_AUTHORITY_SETTING_KEY.to_owned(),
        )
        .await
        .map_err(|error| RecoveryError::Catalog(error.to_string()))?;
        setting
            .map(|setting| {
                serde_json::from_value(setting.value).map_err(|error| {
                    RecoveryError::Catalog(format!("catalog authority setting is invalid: {error}"))
                })
            })
            .transpose()
    }

    async fn write_catalog_authority(
        &self,
        authority: &CatalogAuthority,
    ) -> Result<(), RecoveryError> {
        SettingsRepository::set(
            self.catalog.as_ref(),
            Setting {
                key: CATALOG_AUTHORITY_SETTING_KEY.into(),
                value: serde_json::to_value(authority)
                    .map_err(|error| RecoveryError::Catalog(error.to_string()))?,
                updated_at: Utc::now(),
            },
        )
        .await
        .map_err(|error| RecoveryError::Catalog(error.to_string()))
    }

    pub fn prepare_snapshot_transfer(
        &self,
        listing: &SnapshotListing,
        manager: &ProfileManager,
        now: DateTime<Utc>,
    ) -> Result<SnapshotTransferConfirmation, RecoveryError> {
        let expected_id = listing.id.ok_or_else(|| {
            RecoveryError::Ownership("snapshot listing has no verified snapshot ID".into())
        })?;
        let expected_authority = listing.authority.clone().ok_or_else(|| {
            RecoveryError::Ownership("snapshot has no portable authority metadata".into())
        })?;
        let verified = self.verify_snapshot(&listing.path)?;
        let observed_authority = authority_from_snapshot(&verified.snapshot)?.ok_or_else(|| {
            RecoveryError::Ownership("snapshot authority is no longer available".into())
        })?;
        if verified.snapshot.id != expected_id || observed_authority != expected_authority {
            return Err(RecoveryError::Ownership(
                "snapshot listing is stale; refresh recovery snapshots before confirming transfer"
                    .into(),
            ));
        }
        let nonce = Uuid::new_v4();
        let valid_until = now + chrono::Duration::minutes(5);
        let pending = PendingSnapshotTransfer {
            snapshot_id: expected_id,
            observed_authority: observed_authority.clone(),
            requesting_device: manager.identity().device_id.clone(),
            valid_until,
        };
        self.snapshot_transfers
            .lock()
            .map_err(|_| RecoveryError::Ownership("snapshot transfer state is unavailable".into()))?
            .insert(nonce, pending);
        Ok(SnapshotTransferConfirmation {
            snapshot_id: expected_id,
            observed_authority,
            requesting_device: manager.identity().device_id.clone(),
            nonce,
            valid_until,
        })
    }

    pub fn cancel_snapshot_transfer(
        &self,
        confirmation: &SnapshotTransferConfirmation,
    ) -> Result<bool, RecoveryError> {
        Ok(self
            .snapshot_transfers
            .lock()
            .map_err(|_| RecoveryError::Ownership("snapshot transfer state is unavailable".into()))?
            .remove(&confirmation.nonce)
            .is_some())
    }

    /// Persist an imported/downloaded snapshot without replacing an existing immutable copy.
    pub fn store_snapshot(&self, snapshot: &CatalogSnapshot) -> Result<PathBuf, RecoveryError> {
        self.validate_snapshot(snapshot)?;
        fs::create_dir_all(&self.snapshot_root)
            .map_err(|source| io_error(&self.snapshot_root, source))?;
        let path = self.snapshot_root.join(snapshot_file_name(snapshot));
        if path.exists() {
            let existing = self.read_snapshot(&path)?;
            if existing == *snapshot {
                return Ok(path);
            }
            return Err(RecoveryError::CorruptSnapshot(format!(
                "snapshot path {} already contains different data",
                path.display()
            )));
        }
        atomic_write_json(&path, snapshot)?;
        Ok(path)
    }

    /// List every snapshot envelope, retaining invalid entries so recovery UI can explain them.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotListing>, RecoveryError> {
        let mut listings = Vec::new();
        let entries = match fs::read_dir(&self.snapshot_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(listings),
            Err(source) => return Err(io_error(&self.snapshot_root, source)),
        };
        for entry in entries {
            let entry = entry.map_err(|source| io_error(&self.snapshot_root, source))?;
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|source| io_error(&path, source))?
                .is_file()
                || !is_snapshot_path(&path)
            {
                continue;
            }
            let listing = match self.read_snapshot(&path) {
                Ok(snapshot) => match self.validate_snapshot(&snapshot) {
                    Ok(()) => match authority_from_snapshot(&snapshot) {
                        Ok(authority) => {
                            listing_for(&path, &snapshot, SnapshotHealth::Valid, None, authority)
                        }
                        Err(error) => listing_for(
                            &path,
                            &snapshot,
                            health_for_error(&error),
                            Some(error.to_string()),
                            None,
                        ),
                    },
                    Err(error) => listing_for(
                        &path,
                        &snapshot,
                        health_for_error(&error),
                        Some(error.to_string()),
                        None,
                    ),
                },
                Err(error) => SnapshotListing {
                    path,
                    id: None,
                    created_at: None,
                    format_version: None,
                    health: health_for_error(&error),
                    detail: Some(error.to_string()),
                    authority: None,
                },
            };
            listings.push(listing);
        }
        listings.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(listings)
    }

    pub fn verify_snapshot(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<VerifiedSnapshot, RecoveryError> {
        let path = path.as_ref();
        let snapshot = self.read_snapshot(path)?;
        self.validate_snapshot(&snapshot)?;
        Ok(VerifiedSnapshot {
            path: path.to_path_buf(),
            snapshot,
        })
    }

    /// Validate payload compatibility and replacement semantics in a separately migrated catalog.
    pub async fn dry_run(&self, verified: &VerifiedSnapshot) -> Result<(), RecoveryError> {
        self.validate_snapshot(&verified.snapshot)?;
        let scratch = TursoCatalog::in_memory()
            .await
            .map_err(|error| RecoveryError::Catalog(error.to_string()))?;
        let sentinel = CanonicalTrack::new();
        TrackRepository::upsert(&scratch, sentinel.clone())
            .await
            .map_err(|error| RecoveryError::Catalog(error.to_string()))?;
        scratch
            .replace_from_snapshot(&verified.snapshot)
            .await
            .map_err(|error| RecoveryError::IncompatibleSnapshot(error.to_string()))?;
        if TrackRepository::get(&scratch, sentinel.id)
            .await
            .map_err(|error| RecoveryError::Catalog(error.to_string()))?
            .is_some()
        {
            return Err(RecoveryError::IncompatibleSnapshot(
                "snapshot validation did not replace records absent from the snapshot".into(),
            ));
        }
        Ok(())
    }

    /// Restore a snapshot. The actual catalog mutation is one Turso transaction; on any returned
    /// error Turso rolls it back before this method returns.
    pub async fn restore(
        &self,
        path: impl AsRef<Path>,
        options: RestoreOptions,
        manager: Option<&ProfileManager>,
    ) -> Result<RestoreReport, RecoveryError> {
        let mut events = vec![progress(
            RecoveryStage::Verifying,
            "Verifying snapshot",
            None,
        )];
        let verified = self.verify_snapshot(path)?;
        let snapshot_id = verified.snapshot.id;
        events[0].snapshot_id = Some(snapshot_id);
        let snapshot_authority = authority_from_snapshot(&verified.snapshot)?;
        let current_authority = self.catalog_authority().await?;
        let transfer_required = transfer_confirmation_required(
            snapshot_authority.as_ref(),
            current_authority.as_ref(),
            manager,
        );

        events.push(progress(
            RecoveryStage::DryRun,
            "Validating snapshot against the current catalog schema",
            Some(snapshot_id),
        ));
        self.dry_run(&verified).await?;
        if options.dry_run {
            events.push(progress(
                RecoveryStage::Completed,
                if transfer_required {
                    "Dry-run completed; catalog was not changed. Explicit snapshot ownership transfer confirmation is required before restore"
                } else {
                    "Dry-run completed; catalog was not changed"
                },
                Some(snapshot_id),
            ));
            return Ok(RestoreReport {
                restored_snapshot_id: snapshot_id,
                pre_restore_snapshot: PathBuf::new(),
                dry_run: true,
                progress: events,
                foreign_authority: transfer_required
                    .then(|| snapshot_authority.clone())
                    .flatten(),
                transfer_confirmation_required: transfer_required,
            });
        }

        self.validate_handoff(
            snapshot_id,
            snapshot_authority.as_ref(),
            current_authority.as_ref(),
            options.ownership_handoff.as_ref(),
            options.snapshot_confirmation.as_ref(),
            manager,
            Utc::now(),
        )?;
        self.recover_interrupted().await?;
        events.push(progress(
            RecoveryStage::CreatingPreRestoreBackup,
            "Creating pre-restore catalog backup",
            Some(snapshot_id),
        ));
        let backup = self.catalog.export_snapshot().await.map_err(|error| {
            RecoveryError::Catalog(format!("pre-restore export failed: {error}"))
        })?;
        let backup_path = self.store_pre_restore_snapshot(&backup)?;
        let mut marker = RestoreMarker {
            format_version: RESTORE_MARKER_VERSION,
            target_snapshot_id: snapshot_id,
            pre_restore_snapshot: backup_path.clone(),
            stage: MarkerStage::Prepared,
            started_at: Utc::now(),
        };
        self.write_marker(&marker)?;

        marker.stage = MarkerStage::Importing;
        self.write_marker(&marker)?;
        events.push(progress(
            RecoveryStage::Restoring,
            "Applying snapshot atomically",
            Some(snapshot_id),
        ));
        if let Err(error) = self.catalog.replace_from_snapshot(&verified.snapshot).await {
            // `replace_from_snapshot` has already rolled back its transaction. The pre-restore file is
            // retained as durable evidence and an additional manual recovery point.
            let _ = self.remove_marker();
            return Err(RecoveryError::Catalog(format!(
                "restore transaction rolled back: {error}"
            )));
        }

        if let Some(authority) = authority_after_restore(
            snapshot_authority.as_ref(),
            current_authority.as_ref(),
            options.ownership_handoff.as_ref(),
            options.snapshot_confirmation.as_ref(),
            manager,
        ) {
            self.write_catalog_authority(&authority).await?;
        }
        marker.stage = MarkerStage::Committed;
        self.write_marker(&marker)?;
        self.remove_marker()?;
        events.push(progress(
            RecoveryStage::Completed,
            "Snapshot restore completed",
            Some(snapshot_id),
        ));
        Ok(RestoreReport {
            restored_snapshot_id: snapshot_id,
            pre_restore_snapshot: backup_path,
            dry_run: false,
            progress: events,
            foreign_authority: transfer_required
                .then(|| snapshot_authority.clone())
                .flatten(),
            transfer_confirmation_required: false,
        })
    }

    /// Resolve a marker left by process termination. An uncommitted Turso transaction is rolled
    /// back by SQLite itself; the verified pre-restore snapshot replaces the catalog as an
    /// additional exact repair step. A committed marker only needs cleanup.
    pub async fn recover_interrupted(&self) -> Result<Option<RecoveryProgress>, RecoveryError> {
        let Some(marker) = self.read_marker()? else {
            return Ok(None);
        };
        if marker.format_version != RESTORE_MARKER_VERSION {
            return Err(RecoveryError::InterruptedRestore(
                "unsupported restore marker version".into(),
            ));
        }
        if marker.stage == MarkerStage::Committed {
            self.remove_marker()?;
            return Ok(Some(progress(
                RecoveryStage::Completed,
                "Cleaned up a committed restore marker",
                Some(marker.target_snapshot_id),
            )));
        }
        let backup = self
            .verify_snapshot(&marker.pre_restore_snapshot)
            .map_err(|error| {
                RecoveryError::InterruptedRestore(format!(
                    "pre-restore backup could not be verified: {error}"
                ))
            })?;
        self.catalog
            .replace_from_snapshot(&backup.snapshot)
            .await
            .map_err(|error| {
                RecoveryError::InterruptedRestore(format!(
                    "pre-restore backup could not be applied: {error}"
                ))
            })?;
        self.remove_marker()?;
        Ok(Some(progress(
            RecoveryStage::RollingBack,
            "Recovered an interrupted restore from its pre-restore backup",
            Some(marker.target_snapshot_id),
        )))
    }

    fn validate_snapshot(&self, snapshot: &CatalogSnapshot) -> Result<(), RecoveryError> {
        if !snapshot.verify_integrity() {
            return Err(RecoveryError::CorruptSnapshot(
                "payload hash does not match the envelope".into(),
            ));
        }
        if snapshot.format_version != SNAPSHOT_FORMAT_VERSION
            || snapshot.content_type != SNAPSHOT_CONTENT_TYPE
        {
            return Err(RecoveryError::IncompatibleSnapshot(format!(
                "expected format {} and content type {}, got {} and {}",
                SNAPSHOT_FORMAT_VERSION,
                SNAPSHOT_CONTENT_TYPE,
                snapshot.format_version,
                snapshot.content_type
            )));
        }
        let payload: Value = serde_json::from_slice(snapshot.payload()).map_err(|error| {
            RecoveryError::CorruptSnapshot(format!("payload is not valid JSON: {error}"))
        })?;
        let document_version = payload
            .get("format_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                RecoveryError::CorruptSnapshot("payload has no numeric format_version".into())
            })?;
        if document_version != u64::from(SNAPSHOT_FORMAT_VERSION) {
            return Err(RecoveryError::IncompatibleSnapshot(format!(
                "payload format version {document_version} is unsupported"
            )));
        }
        reject_secrets(&payload, "$", None)
    }

    fn validate_handoff(
        &self,
        snapshot_id: Uuid,
        snapshot_authority: Option<&CatalogAuthority>,
        current_authority: Option<&CatalogAuthority>,
        handoff: Option<&OwnershipHandoff>,
        snapshot_confirmation: Option<&SnapshotTransferConfirmation>,
        manager: Option<&ProfileManager>,
        now: DateTime<Utc>,
    ) -> Result<(), RecoveryError> {
        let Some(snapshot_authority) = snapshot_authority else {
            return Ok(());
        };
        let manager_matches = manager.is_some_and(|manager| {
            snapshot_authority.profile_id == manager.identity().profile_id
                && snapshot_authority.manager_device == manager.identity().device_id
        });
        let generation_is_current = current_authority.is_none_or(|current| {
            snapshot_authority.profile_id == current.profile_id
                && snapshot_authority.ownership_generation <= current.ownership_generation
        });
        if let Some(confirmation) = snapshot_confirmation {
            let manager = manager.ok_or_else(|| {
                RecoveryError::Ownership("no profile manager was supplied".into())
            })?;
            let pending = self
                .snapshot_transfers
                .lock()
                .map_err(|_| {
                    RecoveryError::Ownership("snapshot transfer state is unavailable".into())
                })?
                .remove(&confirmation.nonce)
                .ok_or_else(|| {
                    RecoveryError::Ownership(
                        "snapshot transfer confirmation was already used or cancelled".into(),
                    )
                })?;
            if confirmation.snapshot_id != snapshot_id
                || pending.snapshot_id != snapshot_id
                || confirmation.observed_authority != *snapshot_authority
                || pending.observed_authority != *snapshot_authority
                || confirmation.requesting_device != manager.identity().device_id
                || pending.requesting_device != manager.identity().device_id
                || confirmation.valid_until != pending.valid_until
                || pending.valid_until < now
            {
                return Err(RecoveryError::Ownership(
                    "snapshot transfer confirmation is expired or no longer matches the observed authority"
                        .into(),
                ));
            }
            if !manager
                .owns_profile(now)
                .map_err(|error| RecoveryError::Ownership(error.to_string()))?
            {
                return Err(RecoveryError::Ownership(
                    "snapshot transfer requires the current clean-device manager lease".into(),
                ));
            }
            return Ok(());
        }

        if manager_matches && generation_is_current {
            return Ok(());
        }

        let handoff = handoff.ok_or_else(|| {
            RecoveryError::Ownership(format!(
                "snapshot authority belongs to profile {} generation {} on device {}",
                snapshot_authority.profile_id,
                snapshot_authority.ownership_generation,
                snapshot_authority.manager_device
            ))
        })?;
        let manager = manager
            .ok_or_else(|| RecoveryError::Ownership("no profile manager was supplied".into()))?;
        if snapshot_authority.profile_id != manager.identity().profile_id {
            return Err(RecoveryError::Ownership(
                "snapshot profile does not match the active profile".into(),
            ));
        }
        if handoff.previous_device_id != snapshot_authority.manager_device
            || handoff.previous_device_id == manager.identity().device_id
        {
            return Err(RecoveryError::Ownership(
                "handoff does not identify the snapshot's previous manager device".into(),
            ));
        }
        if handoff.lease.record.session_id != manager.session_id()
            || handoff.lease.record.device_id != manager.identity().device_id
            || !manager
                .owns_profile(now)
                .map_err(|error| RecoveryError::Ownership(error.to_string()))?
        {
            return Err(RecoveryError::Ownership(
                "handoff lease is not the current local ownership lease".into(),
            ));
        }
        if !matches!(
            handoff.lease.takeover_reason,
            TakeoverReason::ConfirmedHandoff | TakeoverReason::ExpiredLease
        ) {
            return Err(RecoveryError::Ownership(
                "lease was not acquired through a stale or explicitly confirmed handoff".into(),
            ));
        }
        Ok(())
    }

    fn read_snapshot(&self, path: &Path) -> Result<CatalogSnapshot, RecoveryError> {
        let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| RecoveryError::CorruptSnapshot(format!("{}: {error}", path.display())))
    }

    fn store_pre_restore_snapshot(
        &self,
        snapshot: &CatalogSnapshot,
    ) -> Result<PathBuf, RecoveryError> {
        self.validate_snapshot(snapshot)?;
        let directory = self.snapshot_root.join(PRE_RESTORE_DIRECTORY);
        fs::create_dir_all(&directory).map_err(|source| io_error(&directory, source))?;
        let path = directory.join(snapshot_file_name(snapshot));
        if !path.exists() {
            atomic_write_json(&path, snapshot)?;
        }
        Ok(path)
    }

    fn marker_path(&self) -> PathBuf {
        self.snapshot_root.join(RESTORE_MARKER)
    }

    fn write_marker(&self, marker: &RestoreMarker) -> Result<(), RecoveryError> {
        fs::create_dir_all(&self.snapshot_root)
            .map_err(|source| io_error(&self.snapshot_root, source))?;
        atomic_write_json(&self.marker_path(), marker)
    }

    fn read_marker(&self) -> Result<Option<RestoreMarker>, RecoveryError> {
        let path = self.marker_path();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                RecoveryError::InterruptedRestore(format!(
                    "invalid marker {}: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_error(&path, source)),
        }
    }

    fn remove_marker(&self) -> Result<(), RecoveryError> {
        let path = self.marker_path();
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(&self.snapshot_root)
                .map_err(|source| io_error(&self.snapshot_root, source)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error(&path, source)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MarkerStage {
    Prepared,
    Importing,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestoreMarker {
    format_version: u32,
    target_snapshot_id: Uuid,
    pre_restore_snapshot: PathBuf,
    stage: MarkerStage,
    started_at: DateTime<Utc>,
}

fn progress(
    stage: RecoveryStage,
    message: impl Into<String>,
    snapshot_id: Option<Uuid>,
) -> RecoveryProgress {
    RecoveryProgress {
        stage,
        message: message.into(),
        snapshot_id,
    }
}

fn authority_from_snapshot(
    snapshot: &CatalogSnapshot,
) -> Result<Option<CatalogAuthority>, RecoveryError> {
    let payload: Value = serde_json::from_slice(snapshot.payload()).map_err(|error| {
        RecoveryError::CorruptSnapshot(format!("payload is not valid JSON: {error}"))
    })?;
    let Some(settings) = payload.get("settings").and_then(Value::as_array) else {
        return Ok(None);
    };
    for value in settings {
        let setting: Setting = serde_json::from_value(value.clone()).map_err(|error| {
            RecoveryError::CorruptSnapshot(format!("snapshot setting is invalid: {error}"))
        })?;
        if setting.key == CATALOG_AUTHORITY_SETTING_KEY {
            return serde_json::from_value(setting.value)
                .map(Some)
                .map_err(|error| {
                    RecoveryError::CorruptSnapshot(format!(
                        "snapshot authority setting is invalid: {error}"
                    ))
                });
        }
    }
    Ok(None)
}

fn transfer_confirmation_required(
    snapshot: Option<&CatalogAuthority>,
    current: Option<&CatalogAuthority>,
    manager: Option<&ProfileManager>,
) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    let manager_matches = manager.is_some_and(|manager| {
        snapshot.profile_id == manager.identity().profile_id
            && snapshot.manager_device == manager.identity().device_id
    });
    let generation_is_current = current.is_none_or(|current| {
        snapshot.profile_id == current.profile_id
            && snapshot.ownership_generation <= current.ownership_generation
    });
    !(manager_matches && generation_is_current)
}

fn authority_after_restore(
    snapshot: Option<&CatalogAuthority>,
    current: Option<&CatalogAuthority>,
    handoff: Option<&OwnershipHandoff>,
    snapshot_confirmation: Option<&SnapshotTransferConfirmation>,
    manager: Option<&ProfileManager>,
) -> Option<CatalogAuthority> {
    let manager = manager?;
    let base_generation = current
        .map(|authority| authority.ownership_generation)
        .into_iter()
        .chain(snapshot.map(|authority| authority.ownership_generation))
        .max()
        .unwrap_or(0);
    let transferred = handoff.is_some()
        || snapshot_confirmation.is_some()
        || snapshot
            .is_some_and(|authority| authority.manager_device != manager.identity().device_id);
    Some(CatalogAuthority {
        profile_id: manager.identity().profile_id.clone(),
        ownership_generation: if transferred {
            base_generation.saturating_add(1)
        } else {
            base_generation.max(1)
        },
        manager_device: manager.identity().device_id.clone(),
    })
}

fn listing_for(
    path: &Path,
    snapshot: &CatalogSnapshot,
    health: SnapshotHealth,
    detail: Option<String>,
    authority: Option<CatalogAuthority>,
) -> SnapshotListing {
    SnapshotListing {
        path: path.to_path_buf(),
        id: Some(snapshot.id),
        created_at: Some(snapshot.created_at),
        format_version: Some(snapshot.format_version),
        health,
        detail,
        authority,
    }
}

fn health_for_error(error: &RecoveryError) -> SnapshotHealth {
    match error {
        RecoveryError::IncompatibleSnapshot(_) => SnapshotHealth::Incompatible,
        RecoveryError::SecretMaterial(_) => SnapshotHealth::ContainsSecrets,
        _ => SnapshotHealth::Corrupt,
    }
}

fn snapshot_file_name(snapshot: &CatalogSnapshot) -> String {
    format!(
        "{}-{}.{}",
        snapshot.created_at.format("%Y%m%dT%H%M%S%.3fZ"),
        snapshot.id,
        SNAPSHOT_EXTENSION
    )
}

fn is_snapshot_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(SNAPSHOT_EXTENSION))
}

/// Reject token/password material even if a hand-crafted envelope has a valid hash. Opaque
/// `credential_reference` values and the `refresh_token_metadata` collection are explicitly
/// metadata and are still validated by Turso during dry-run/replacement.
fn reject_secrets(
    value: &Value,
    path: &str,
    parent_key: Option<&str>,
) -> Result<(), RecoveryError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                let prohibited = matches!(
                    lower.as_str(),
                    "access_token"
                        | "refresh_token"
                        | "id_token"
                        | "password"
                        | "passphrase"
                        | "client_secret"
                        | "api_key"
                        | "authorization"
                        | "credentials"
                ) || (lower.ends_with("_secret")
                    && lower != "credential_reference");
                if prohibited {
                    return Err(RecoveryError::SecretMaterial(format!("{path}.{key}")));
                }
                reject_secrets(child, &format!("{path}.{key}"), Some(key))?;
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                reject_secrets(child, &format!("{path}[{index}]"), parent_key)?;
            }
        }
        Value::String(text) if parent_key != Some("credential_reference") => {
            let lower = text.to_ascii_lowercase();
            if lower.starts_with("bearer ")
                || lower.contains("access_token=")
                || lower.contains("refresh_token=")
            {
                return Err(RecoveryError::SecretMaterial(path.into()));
            }
        }
        _ => {}
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), RecoveryError> {
    let parent = path.parent().ok_or_else(|| RecoveryError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let temporary = parent.join(format!(".recovery-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), RecoveryError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| io_error(&temporary, source))?;
        serde_json::to_writer(&mut file, value)
            .map_err(|error| RecoveryError::CorruptSnapshot(error.to_string()))?;
        file.flush()
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(&temporary, path).map_err(|source| io_error(path, source))?;
        sync_directory(parent).map_err(|source| io_error(parent, source))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> RecoveryError {
    RecoveryError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rec-bag-recovery-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn service(label: &str) -> (RecoveryService, TempDirectory) {
        let root = TempDirectory::new(label);
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        (RecoveryService::new(catalog, &root.0), root)
    }

    async fn clean_device_transfer_fixture(
        label: &str,
    ) -> (
        RecoveryService,
        TempDirectory,
        TempDirectory,
        ProfileManager,
        SnapshotListing,
    ) {
        use crate::services::profile_manager::ManagerIdentity;
        use std::time::Duration;

        let (source, source_root) = service(&format!("{label}-source")).await;
        let source_manager = ProfileManager::new(
            source_root.0.join("profile"),
            ManagerIdentity::new("portable-profile", "source-device"),
            Duration::from_secs(60),
        )
        .unwrap();
        let source_lease = source_manager.acquire(Utc::now()).unwrap();
        source
            .persist_catalog_authority(&source_manager, &source_lease)
            .await
            .unwrap();
        let snapshot = source.catalog.export_snapshot().await.unwrap();

        let (destination, destination_root) = service(&format!("{label}-destination")).await;
        destination.store_snapshot(&snapshot).unwrap();
        let manager = ProfileManager::new(
            destination_root.0.join("profile"),
            ManagerIdentity::new("portable-profile", "clean-device"),
            Duration::from_secs(60),
        )
        .unwrap();
        let lease = manager.acquire(Utc::now()).unwrap();
        destination
            .persist_catalog_authority(&manager, &lease)
            .await
            .unwrap();
        let listing = destination.list_snapshots().unwrap().remove(0);
        (destination, source_root, destination_root, manager, listing)
    }

    #[tokio::test]
    async fn foreign_dry_run_is_non_mutating_and_restore_still_requires_confirmation() {
        let (service, _source_root, _destination_root, manager, listing) =
            clean_device_transfer_fixture("foreign-dry-run").await;
        let sentinel = CanonicalTrack::new();
        TrackRepository::upsert(service.catalog.as_ref(), sentinel.clone())
            .await
            .unwrap();
        let authority_before = service.catalog_authority().await.unwrap();
        let lease_before = manager.status(Utc::now()).unwrap();

        let report = service
            .restore(
                &listing.path,
                RestoreOptions {
                    dry_run: true,
                    ownership_handoff: None,
                    snapshot_confirmation: None,
                },
                Some(&manager),
            )
            .await
            .unwrap();

        assert!(report.dry_run);
        assert!(report.transfer_confirmation_required);
        assert_eq!(report.foreign_authority, listing.authority);
        assert_eq!(
            TrackRepository::get(service.catalog.as_ref(), sentinel.id)
                .await
                .unwrap(),
            Some(sentinel)
        );
        assert_eq!(service.catalog_authority().await.unwrap(), authority_before);
        assert_eq!(manager.status(Utc::now()).unwrap(), lease_before);
        assert!(matches!(
            service
                .restore(
                    &listing.path,
                    RestoreOptions {
                        dry_run: false,
                        ownership_handoff: None,
                        snapshot_confirmation: None,
                    },
                    Some(&manager),
                )
                .await,
            Err(RecoveryError::Ownership(_))
        ));
    }

    #[tokio::test]
    async fn clean_device_snapshot_confirmation_transfers_and_advances_authority() {
        let (service, _source_root, _destination_root, manager, listing) =
            clean_device_transfer_fixture("clean-transfer").await;
        assert_eq!(
            listing.authority.as_ref().unwrap().manager_device,
            "source-device"
        );
        let observed_generation = listing.authority.as_ref().unwrap().ownership_generation;
        let confirmation = service
            .prepare_snapshot_transfer(&listing, &manager, Utc::now())
            .unwrap();

        let report = service
            .restore(
                &listing.path,
                RestoreOptions {
                    dry_run: false,
                    ownership_handoff: None,
                    snapshot_confirmation: Some(confirmation),
                },
                Some(&manager),
            )
            .await
            .unwrap();

        assert_eq!(report.restored_snapshot_id, listing.id.unwrap());
        let authority = service.catalog_authority().await.unwrap().unwrap();
        assert_eq!(authority.manager_device, "clean-device");
        assert!(authority.ownership_generation > observed_generation);
    }

    #[tokio::test]
    async fn cancelled_snapshot_confirmation_is_single_use() {
        let (service, _source_root, _destination_root, manager, listing) =
            clean_device_transfer_fixture("cancel-transfer").await;
        let confirmation = service
            .prepare_snapshot_transfer(&listing, &manager, Utc::now())
            .unwrap();
        assert!(service.cancel_snapshot_transfer(&confirmation).unwrap());
        assert!(!service.cancel_snapshot_transfer(&confirmation).unwrap());

        assert!(matches!(
            service
                .restore(
                    &listing.path,
                    RestoreOptions {
                        dry_run: false,
                        ownership_handoff: None,
                        snapshot_confirmation: Some(confirmation),
                    },
                    Some(&manager),
                )
                .await,
            Err(RecoveryError::Ownership(_))
        ));
    }

    #[tokio::test]
    async fn snapshot_confirmation_replay_is_rejected() {
        let (service, _source_root, _destination_root, manager, listing) =
            clean_device_transfer_fixture("replay-transfer").await;
        let confirmation = service
            .prepare_snapshot_transfer(&listing, &manager, Utc::now())
            .unwrap();
        let replay = confirmation.clone();
        service
            .restore(
                &listing.path,
                RestoreOptions {
                    dry_run: false,
                    ownership_handoff: None,
                    snapshot_confirmation: Some(confirmation),
                },
                Some(&manager),
            )
            .await
            .unwrap();

        assert!(matches!(
            service
                .restore(
                    &listing.path,
                    RestoreOptions {
                        dry_run: false,
                        ownership_handoff: None,
                        snapshot_confirmation: Some(replay),
                    },
                    Some(&manager),
                )
                .await,
            Err(RecoveryError::Ownership(_))
        ));
    }

    #[tokio::test]
    async fn changed_snapshot_authority_invalidates_observed_confirmation() {
        let (service, _source_root, _destination_root, manager, listing) =
            clean_device_transfer_fixture("changed-transfer").await;
        let confirmation = service
            .prepare_snapshot_transfer(&listing, &manager, Utc::now())
            .unwrap();
        let verified = service.verify_snapshot(&listing.path).unwrap();
        let mut payload: Value = serde_json::from_slice(verified.snapshot.payload()).unwrap();
        let settings = payload["settings"].as_array_mut().unwrap();
        let authority = settings
            .iter_mut()
            .find(|setting| setting["key"] == CATALOG_AUTHORITY_SETTING_KEY)
            .unwrap();
        authority["value"]["manager_device"] = Value::String("changed-device".into());
        let changed = CatalogSnapshot::new(
            SNAPSHOT_FORMAT_VERSION,
            SNAPSHOT_CONTENT_TYPE,
            serde_json::to_vec(&payload).unwrap(),
        );
        fs::write(&listing.path, serde_json::to_vec(&changed).unwrap()).unwrap();

        assert!(matches!(
            service
                .restore(
                    &listing.path,
                    RestoreOptions {
                        dry_run: false,
                        ownership_handoff: None,
                        snapshot_confirmation: Some(confirmation),
                    },
                    Some(&manager),
                )
                .await,
            Err(RecoveryError::Ownership(_))
        ));
        assert!(matches!(
            service.prepare_snapshot_transfer(&listing, &manager, Utc::now()),
            Err(RecoveryError::Ownership(_))
        ));
    }

    #[tokio::test]
    async fn lists_verifies_dry_runs_and_restores_with_pre_backup() {
        let (service, _root) = service("restore").await;
        let snapshot = service.catalog.export_snapshot().await.unwrap();
        let path = service.store_snapshot(&snapshot).unwrap();
        let listings = service.list_snapshots().unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].health, SnapshotHealth::Valid);
        service
            .dry_run(&service.verify_snapshot(&path).unwrap())
            .await
            .unwrap();

        let report = service
            .restore(&path, RestoreOptions::default(), None)
            .await
            .unwrap();
        assert_eq!(report.restored_snapshot_id, snapshot.id);
        assert!(report.pre_restore_snapshot.exists());
        assert_eq!(
            report.progress.last().unwrap().stage,
            RecoveryStage::Completed
        );
        assert!(!service.marker_path().exists());
    }

    #[tokio::test]
    async fn replacement_restore_removes_absent_records_restores_primary_and_rolls_back_exactly() {
        use rec_core::{
            MusicStorage, MusicStorageKind, MusicStorageRepository, StorageCapabilities,
            StorageRole,
        };

        let (service, _root) = service("replacement-regression").await;
        let target_track = CanonicalTrack::new();
        TrackRepository::upsert(service.catalog.as_ref(), target_track.clone())
            .await
            .unwrap();
        let target_primary = MusicStorage::new(
            "snapshot-primary",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        MusicStorageRepository::upsert(service.catalog.as_ref(), target_primary.clone())
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(
            service.catalog.as_ref(),
            None,
            Some(target_primary.id),
        )
        .await
        .unwrap();
        let target_snapshot = service.catalog.export_snapshot().await.unwrap();
        let target_path = service.store_snapshot(&target_snapshot).unwrap();

        let post_snapshot_track = CanonicalTrack::new();
        TrackRepository::upsert(service.catalog.as_ref(), post_snapshot_track.clone())
            .await
            .unwrap();
        let pre_restore_primary = MusicStorage::new(
            "pre-restore-primary",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        MusicStorageRepository::upsert(service.catalog.as_ref(), pre_restore_primary.clone())
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(
            service.catalog.as_ref(),
            Some(target_primary.id),
            Some(pre_restore_primary.id),
        )
        .await
        .unwrap();
        SettingsRepository::set(
            service.catalog.as_ref(),
            Setting {
                key: "post-snapshot-only".into(),
                value: Value::String("restore-on-rollback".into()),
                updated_at: Utc::now(),
            },
        )
        .await
        .unwrap();

        let report = service
            .restore(&target_path, RestoreOptions::default(), None)
            .await
            .unwrap();
        assert!(
            TrackRepository::get(service.catalog.as_ref(), post_snapshot_track.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            SettingsRepository::get(service.catalog.as_ref(), "post-snapshot-only".into())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            MusicStorageRepository::get_active_primary(service.catalog.as_ref())
                .await
                .unwrap()
                .unwrap()
                .id,
            target_primary.id
        );

        let interrupted_only_track = CanonicalTrack::new();
        TrackRepository::upsert(service.catalog.as_ref(), interrupted_only_track.clone())
            .await
            .unwrap();
        service
            .write_marker(&RestoreMarker {
                format_version: RESTORE_MARKER_VERSION,
                target_snapshot_id: target_snapshot.id,
                pre_restore_snapshot: report.pre_restore_snapshot,
                stage: MarkerStage::Importing,
                started_at: Utc::now(),
            })
            .unwrap();
        service.recover_interrupted().await.unwrap().unwrap();

        assert_eq!(
            TrackRepository::get(service.catalog.as_ref(), target_track.id)
                .await
                .unwrap(),
            Some(target_track)
        );
        assert_eq!(
            TrackRepository::get(service.catalog.as_ref(), post_snapshot_track.id)
                .await
                .unwrap(),
            Some(post_snapshot_track)
        );
        assert!(
            TrackRepository::get(service.catalog.as_ref(), interrupted_only_track.id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            MusicStorageRepository::get_active_primary(service.catalog.as_ref())
                .await
                .unwrap()
                .unwrap()
                .id,
            pre_restore_primary.id
        );
        assert_eq!(
            SettingsRepository::get(service.catalog.as_ref(), "post-snapshot-only".into())
                .await
                .unwrap()
                .unwrap()
                .value,
            Value::String("restore-on-rollback".into())
        );
    }

    #[tokio::test]
    async fn corrupt_snapshot_is_reported_and_never_imported() {
        let (service, root) = service("corrupt").await;
        let snapshot = service.catalog.export_snapshot().await.unwrap();
        let path = service.store_snapshot(&snapshot).unwrap();
        let mut envelope: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        envelope["payload"] = serde_json::json!([1, 2, 3]);
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();

        assert!(matches!(
            service.verify_snapshot(&path),
            Err(RecoveryError::CorruptSnapshot(_))
        ));
        let listings = service.list_snapshots().unwrap();
        assert_eq!(listings[0].health, SnapshotHealth::Corrupt);
        assert!(!root.0.join(RESTORE_MARKER).exists());
    }

    #[tokio::test]
    async fn interrupted_restore_uses_verified_pre_restore_backup() {
        let (service, _root) = service("interrupted").await;
        let backup = service.catalog.export_snapshot().await.unwrap();
        let backup_path = service.store_pre_restore_snapshot(&backup).unwrap();
        let marker = RestoreMarker {
            format_version: RESTORE_MARKER_VERSION,
            target_snapshot_id: Uuid::new_v4(),
            pre_restore_snapshot: backup_path,
            stage: MarkerStage::Importing,
            started_at: Utc::now(),
        };
        service.write_marker(&marker).unwrap();

        let recovered = service.recover_interrupted().await.unwrap().unwrap();
        assert_eq!(recovered.stage, RecoveryStage::RollingBack);
        assert!(!service.marker_path().exists());
    }

    #[test]
    fn valid_hash_does_not_allow_secret_bearing_payload() {
        let root = TempDirectory::new("secret");
        let catalog = Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(TursoCatalog::in_memory())
                .unwrap(),
        );
        let service = RecoveryService::new(catalog, &root.0);
        let payload = serde_json::to_vec(&serde_json::json!({
            "format_version": SNAPSHOT_FORMAT_VERSION,
            "access_token": "do-not-restore"
        }))
        .unwrap();
        let snapshot =
            CatalogSnapshot::new(SNAPSHOT_FORMAT_VERSION, SNAPSHOT_CONTENT_TYPE, payload);
        assert!(matches!(
            service.store_snapshot(&snapshot),
            Err(RecoveryError::SecretMaterial(_))
        ));
    }

    #[tokio::test]
    async fn another_device_restore_requires_handoff_lease() {
        use crate::services::profile_manager::ManagerIdentity;
        use std::time::Duration;

        let (source, source_root) = service("handoff-source").await;
        let source_manager = ProfileManager::new(
            source_root.0.join("profile"),
            ManagerIdentity::new("profile", "old-device"),
            Duration::from_secs(60),
        )
        .unwrap();
        let source_lease = source_manager.acquire(Utc::now()).unwrap();
        source
            .persist_catalog_authority(&source_manager, &source_lease)
            .await
            .unwrap();
        let snapshot = source.catalog.export_snapshot().await.unwrap();

        let (service, root) = service("handoff-destination").await;
        let path = service.store_snapshot(&snapshot).unwrap();
        assert!(matches!(
            service
                .restore(&path, RestoreOptions::default(), None)
                .await,
            Err(RecoveryError::Ownership(_))
        ));

        let old_local_manager = ProfileManager::new(
            root.0.join("profile"),
            ManagerIdentity::new("profile", "old-device"),
            Duration::from_millis(1),
        )
        .unwrap();
        let now = Utc::now();
        old_local_manager.acquire(now).unwrap();
        let manager = ProfileManager::new(
            root.0.join("profile"),
            ManagerIdentity::new("profile", "new-device"),
            Duration::from_secs(60),
        )
        .unwrap();
        let takeover = manager.acquire(now + chrono::Duration::seconds(1)).unwrap();
        assert_eq!(takeover.takeover_reason, TakeoverReason::ExpiredLease);
        service
            .persist_catalog_authority(&manager, &takeover)
            .await
            .unwrap();

        assert!(matches!(
            service
                .restore(&path, RestoreOptions::default(), Some(&manager))
                .await,
            Err(RecoveryError::Ownership(_))
        ));

        let report = service
            .restore(
                &path,
                RestoreOptions {
                    dry_run: false,
                    ownership_handoff: Some(OwnershipHandoff {
                        previous_device_id: "old-device".into(),
                        lease: takeover,
                    }),
                    snapshot_confirmation: None,
                },
                Some(&manager),
            )
            .await
            .unwrap();
        assert!(!report.dry_run);
        let authority = service.catalog_authority().await.unwrap().unwrap();
        assert_eq!(authority.profile_id, "profile");
        assert_eq!(authority.manager_device, "new-device");
        assert!(authority.ownership_generation >= 2);
    }
}
