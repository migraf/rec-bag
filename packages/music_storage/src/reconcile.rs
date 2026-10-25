//! Reconciliation of the catalog against the active primary's inventory.

use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use rec_core::{
    AssetLocation, AssetType, HealthStatus, MusicStorage, MusicStorageRepository,
    RepositoryTransaction, StorageHealth, StorageVerificationStatus, SyncRun, SyncRunKind,
    TransactionManager,
};
use rec_persistence::TursoCatalog;
use rec_sync::{
    reconcile, InventoryAvailability, ReconciliationInput, ReconciliationReport, StorageInventory,
};
use uuid::Uuid;

use crate::catalog_io;
use crate::error::StorageError;
use crate::storages::MusicStorages;

/// Health already probed during the current operation, by storage ID. `None`
/// records a probe that failed.
pub(crate) type ProbedHealth = BTreeMap<Uuid, Option<StorageHealth>>;

impl MusicStorages {
    /// Reconciles the catalog against the active primary and persists the
    /// findings: candidates, results, and a verified location for every match.
    ///
    /// The primary's inventory counts as available only while its store is
    /// open and its health probe (taken from `probed` when present) does not
    /// report it unavailable; otherwise reconciliation runs without it.
    pub(crate) async fn reconcile_active(
        &self,
        probed: &ProbedHealth,
    ) -> Result<ReconciliationReport, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let primary = MusicStorageRepository::get_active_primary(catalog).await?;
        let availability = match &primary {
            Some(storage) => Some(self.availability(storage, probed).await),
            None => None,
        };
        let input = reconciliation_input(
            catalog,
            primary.as_ref().map(|storage| storage.id).zip(availability),
        )
        .await?;
        let report = reconcile(&input);
        record(catalog, &input, &report).await?;
        Ok(report)
    }

    /// Reconciles the catalog as if `storage` were the primary and records
    /// only what that proves: a verified location for every hash match in its
    /// inventory. Candidates, results and the reconciliation run stay those of
    /// the actual active primary.
    pub(crate) async fn record_matches_on(
        &self,
        storage: &MusicStorage,
        probed: &ProbedHealth,
    ) -> Result<(), StorageError> {
        let catalog = self.0.catalog.as_ref();
        let availability = self.availability(storage, probed).await;
        let input = reconciliation_input(catalog, Some((storage.id, availability))).await?;
        let report = reconcile(&input);
        let transaction = TransactionManager::begin(catalog).await?;
        record_matches(transaction.as_ref(), &input, &report).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn availability(
        &self,
        storage: &MusicStorage,
        probed: &ProbedHealth,
    ) -> InventoryAvailability {
        if !storage.enabled {
            return InventoryAvailability::Unavailable;
        }
        let health = match probed.get(&storage.id) {
            Some(health) => health.clone(),
            None => match self.0.stores.get(storage.id) {
                Some(store) => store.health().await.ok(),
                None => return InventoryAvailability::Unavailable,
            },
        };
        match health {
            Some(health) if health.status != HealthStatus::Unavailable => {
                InventoryAvailability::Available
            }
            Some(_) => InventoryAvailability::Unavailable,
            None => InventoryAvailability::Error,
        }
    }
}

/// Everything reconciliation reads. Only the primary's inventory is included,
/// because reconciliation judges custody by the primary alone.
async fn reconciliation_input(
    catalog: &TursoCatalog,
    primary: Option<(Uuid, InventoryAvailability)>,
) -> Result<ReconciliationInput, StorageError> {
    let inventories = match primary {
        Some((storage_id, availability)) => vec![StorageInventory {
            storage_id,
            availability,
            objects: if availability == InventoryAvailability::Available {
                catalog_io::storage_objects(catalog, storage_id).await?
            } else {
                Vec::new()
            },
        }],
        None => Vec::new(),
    };
    Ok(ReconciliationInput {
        primary_storage_id: primary.map(|(storage_id, _)| storage_id),
        sources: catalog_io::sources(catalog).await?,
        tracks: catalog_io::tracks(catalog).await?,
        occurrences: catalog_io::occurrences(catalog, None, None).await?,
        originals: catalog_io::originals(catalog).await?,
        locations: catalog_io::locations(catalog).await?,
        storages: MusicStorageRepository::list(catalog).await?,
        inventories,
        playlists: catalog_io::playlists(catalog).await?,
        external_evidence: Vec::new(),
        resolutions: catalog_io::resolutions(catalog).await?,
        suppressions: catalog_io::suppressions(catalog).await?,
    })
}

/// Persists a report and its reconciliation run in one transaction.
async fn record(
    catalog: &TursoCatalog,
    input: &ReconciliationInput,
    report: &ReconciliationReport,
) -> Result<(), StorageError> {
    let mut run = SyncRun::new(SyncRunKind::Reconciliation, None, input.primary_storage_id);
    run.start().expect("a new sync run can start");
    run.progress
        .discover((report.results.len() + report.candidates.len()) as u64);
    let transaction = TransactionManager::begin(catalog).await?;
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
    record_matches(transaction.as_ref(), input, report).await?;
    run.succeed().expect("a running sync run can succeed");
    transaction.sync_runs().upsert(run).await?;
    transaction.commit().await?;
    Ok(())
}

/// Records a verified location for every hash match in the report.
async fn record_matches(
    transaction: &dyn RepositoryTransaction,
    input: &ReconciliationInput,
    report: &ReconciliationReport,
) -> Result<(), StorageError> {
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
        let (Some(asset), Some(object)) = (
            originals.get(&matched.asset_id),
            objects.get(&(matched.storage_id, matched.storage_object_id.clone())),
        ) else {
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
                id: existing.map_or_else(Uuid::new_v4, |location| location.id),
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
    Ok(())
}
