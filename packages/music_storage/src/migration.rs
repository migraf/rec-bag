//! Primary migration: making another storage the active primary once it holds
//! every original the current primary holds.

use std::collections::{BTreeMap, HashMap};
use std::fs;

use chrono::Utc;
use rec_core::{
    AssetLocation, HealthStatus, MusicStorage, MusicStorageErrorKind, MusicStorageRepository,
    OriginalAsset, StorageRole, StorageVerificationStatus, TrackRepository, TransactionManager,
};
use uuid::Uuid;

use crate::catalog_io;
use crate::error::StorageError;
use crate::handoff::HandoffReport;
use crate::inventory::inventory;
use crate::reconcile::ProbedHealth;
use crate::roots::RootPurpose;
use crate::storages::{missing_capability, unhealthy, MusicStorages};

/// How a primary migration treats checksums and the storage it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationOptions {
    /// Keep the replaced primary as a backup instead of leaving it unassigned.
    pub keep_old_as_backup: bool,
    /// Download every copy the destination cannot vouch for and hash it.
    pub verify_checksums: bool,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        Self {
            keep_old_as_backup: false,
            verify_checksums: true,
        }
    }
}

/// The outcome of [`MusicStorages::migrate_primary`].
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationOutcome {
    pub previous_primary: Uuid,
    pub active_primary: Uuid,
    /// Originals copied from the previous primary.
    pub copied_assets: u64,
    /// Originals the destination already held verified.
    pub already_present: u64,
    /// What replication planned for the new primary.
    pub handoff: HandoffReport,
}

impl MusicStorages {
    /// Replaces the active primary with the unassigned storage `to`.
    ///
    /// The destination must be open, healthy and able to store and verify
    /// music. It is fully inventoried and every hash match in it is recorded
    /// as a verified location. Each original the current primary holds
    /// verified but the destination does not is then copied from the current
    /// primary (read in place when it keeps the original on the host,
    /// otherwise downloaded), uploaded idempotently and verified.
    ///
    /// The switch happens only when the destination holds verified every
    /// original the current primary holds; otherwise nothing switches and
    /// [`StorageError::MigrationIncomplete`] reports how many are missing.
    /// Rerunning a migration resumes it. The switch and, with
    /// [`MigrationOptions::keep_old_as_backup`], the replaced primary's new
    /// backup role commit together. Keeping the old primary as a backup is
    /// refused up front with [`StorageError::RootsOverlap`] when its root
    /// overlaps a catalog source or another storage, as connecting a backup
    /// there would be. Reconciliation then runs against the new
    /// primary and every verified asset is handed to replication.
    pub async fn migrate_primary(
        &self,
        to: Uuid,
        options: MigrationOptions,
    ) -> Result<MigrationOutcome, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let previous = MusicStorageRepository::get_active_primary(catalog)
            .await?
            .ok_or(StorageError::NoPrimary)?;
        let destination = MusicStorageRepository::get(catalog, to)
            .await?
            .ok_or(StorageError::StorageNotFound(to))?;
        if destination.id == previous.id || destination.role != StorageRole::Unassigned {
            return Err(StorageError::InvalidTarget(format!(
                "storage {to} is not unassigned; a primary migration needs an unassigned destination"
            )));
        }
        if !destination.enabled {
            return Err(StorageError::InvalidTarget(format!(
                "storage {to} is disconnected"
            )));
        }
        if options.keep_old_as_backup {
            // A backup never shares files with a catalog source, so an old
            // primary adopted in place cannot become one. Refuse before
            // copying anything rather than quietly leaving it unassigned.
            if let Some(root) = self
                .0
                .drivers
                .get(&previous.kind)
                .and_then(|driver| driver.root(&previous))
            {
                self.check_root_separation_except(
                    &root,
                    RootPurpose::Storage(StorageRole::Backup),
                    Some(previous.id),
                )
                .await?;
            }
        }

        let store = self.open_store(&destination).await?;
        let health = store.health().await?;
        if health.status != HealthStatus::Healthy {
            return Err(unhealthy(health));
        }
        if let Some(missing) = missing_capability(&health.capabilities) {
            return Err(StorageError::MissingCapability {
                role: StorageRole::Primary,
                missing,
            });
        }
        inventory(
            catalog,
            destination.id,
            store.as_ref(),
            health.capabilities.supports_change_tracking,
            true,
        )
        .await?;
        self.clear_fault(destination.id);
        self.record_matches_on(
            &destination,
            &ProbedHealth::from([(destination.id, Some(health))]),
        )
        .await?;

        let unmigrated = self.unmigrated(previous.id, destination.id).await?;
        let already_present = unmigrated.held - unmigrated.missing.len() as u64;
        let mut copied_assets = 0;
        if !unmigrated.missing.is_empty() {
            self.open_store(&previous).await?;
            for original in &unmigrated.missing {
                match self
                    .migrate_one(&previous, &destination, original, options.verify_checksums)
                    .await
                {
                    Ok(()) => copied_assets += 1,
                    // Counted by the completeness check below.
                    Err(error) if concerns_only_this_asset(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let remaining = self
            .unmigrated(previous.id, destination.id)
            .await?
            .missing
            .len() as u64;
        if remaining != 0 {
            return Err(StorageError::MigrationIncomplete(remaining));
        }

        let transaction = TransactionManager::begin(catalog).await?;
        transaction
            .music_storages()
            .switch_active_primary(Some(previous.id), Some(destination.id))
            .await?;
        if options.keep_old_as_backup {
            if let Some(mut replaced) = transaction.music_storages().get(previous.id).await? {
                replaced.role = StorageRole::Backup;
                replaced.updated_at = Utc::now();
                transaction.music_storages().upsert(replaced).await?;
            }
        }
        transaction.commit().await?;
        self.reconcile_active(&ProbedHealth::new()).await?;
        let handoff = self.0.handoff.verified_primary_write(&[]).await;
        Ok(MigrationOutcome {
            previous_primary: previous.id,
            active_primary: destination.id,
            copied_assets,
            already_present,
            handoff,
        })
    }

    /// Copies `original` from `previous` to `destination`.
    async fn migrate_one(
        &self,
        previous: &MusicStorage,
        destination: &MusicStorage,
        original: &OriginalAsset,
        download_check: bool,
    ) -> Result<(), StorageError> {
        let track = TrackRepository::get(self.0.catalog.as_ref(), original.track_id())
            .await?
            .ok_or(StorageError::TrackNotFound(original.track_id()))?;
        let from = BTreeMap::from([(previous.id, previous.clone())]);
        let mut restore_error = None;
        let obtained = match self
            .obtain_from_storage(&from, std::slice::from_ref(original), &mut restore_error)
            .await?
        {
            Some(obtained) => obtained,
            None => {
                return Err(
                    restore_error.unwrap_or(StorageError::NoEligibleSource(original.track_id()))
                )
            }
        };
        let idempotency_key = format!(
            "primary-migration:{}:{}:{}",
            previous.id,
            destination.id,
            original.id()
        );
        let stored = self
            .store(
                destination,
                &track,
                &obtained,
                &idempotency_key,
                download_check,
            )
            .await;
        if obtained.staged {
            let _ = fs::remove_file(&obtained.path);
        }
        stored.map(|_| ())
    }

    /// The originals `from` holds verified, and those of them `to` does not.
    async fn unmigrated(&self, from: Uuid, to: Uuid) -> Result<Unmigrated, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let mut locations: HashMap<Uuid, Vec<AssetLocation>> = HashMap::new();
        for location in catalog_io::locations(catalog).await? {
            locations
                .entry(location.asset_id)
                .or_default()
                .push(location);
        }
        let mut unmigrated = Unmigrated {
            held: 0,
            missing: Vec::new(),
        };
        for original in catalog_io::originals(catalog).await? {
            let locations = locations
                .get(&original.id())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let holds = |storage_id: Uuid| {
                locations
                    .iter()
                    .any(|location| holds_verified(location, &original, storage_id))
            };
            if !holds(from) {
                continue;
            }
            unmigrated.held += 1;
            if !holds(to) {
                unmigrated.missing.push(original);
            }
        }
        Ok(unmigrated)
    }
}

/// What a migration still has to copy.
struct Unmigrated {
    /// Originals the current primary holds verified.
    held: u64,
    /// Those of them the destination does not.
    missing: Vec<OriginalAsset>,
}

/// Whether `location` is a verified copy of `original`'s current content on
/// `storage_id`.
fn holds_verified(location: &AssetLocation, original: &OriginalAsset, storage_id: Uuid) -> bool {
    location.storage_id == storage_id
        && location.verification_status == StorageVerificationStatus::Verified
        && (original.content_hash.is_none() || location.expected_hash == original.content_hash)
}

/// Whether `error` stops only this original from being copied, so the others
/// can still be tried before completeness is judged.
fn concerns_only_this_asset(error: &StorageError) -> bool {
    match error {
        StorageError::NoEligibleSource(_)
        | StorageError::TrackNotFound(_)
        | StorageError::Verification(_) => true,
        StorageError::Provider(error) => matches!(
            error.kind,
            MusicStorageErrorKind::NotFound
                | MusicStorageErrorKind::ChecksumMismatch
                | MusicStorageErrorKind::InvalidData
        ),
        _ => false,
    }
}
