//! Inventory: enumerating what a storage holds as a staged generation, then
//! reconciling the catalog against the active primary.

use std::collections::BTreeMap;

use chrono::Utc;
use rec_core::{
    AssetRepository, HealthStatus, InventoryGenerationAbort, InventoryGenerationAbortKind,
    InventoryGenerationKind, InventoryGenerationStart, InventoryMode, InventoryRequest,
    InventoryStagingPage, MusicAssetStore, MusicStorageErrorKind, MusicStorageRepository,
    RepositoryErrorKind, StorageObjectTombstone, SyncCheckpoint, SyncCursor, SyncError,
    SyncErrorKind, SyncRun, SyncRunFilter, SyncRunKind, SyncRunRepository, SyncStatus,
    TransactionManager,
};
use rec_persistence::TursoCatalog;
use rec_sync::ReconciliationReport;
use uuid::Uuid;

use crate::error::StorageError;
use crate::storages::{unhealthy, MusicStorages};

const INVENTORY_PAGE_SIZE: u32 = 250;

/// Which storage a refresh inventories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The active primary.
    Primary,
    /// A configured storage of any role.
    Storage(Uuid),
}

/// The outcome of [`MusicStorages::refresh`].
#[derive(Debug, Clone, PartialEq)]
pub struct Refreshed {
    pub storage_id: Uuid,
    /// The storage inventory sync run.
    pub run_id: Uuid,
    /// The finalized inventory generation.
    pub generation_id: Uuid,
    pub object_count: u64,
    pub removal_count: u64,
    /// Whether only changes since the last successful inventory were listed.
    pub incremental: bool,
    /// Reconciliation against the active primary after the inventory.
    pub report: ReconciliationReport,
}

impl MusicStorages {
    /// Inventories `target` and reconciles the catalog against the active
    /// primary.
    ///
    /// The storage is opened on demand. A storage that needs reauthorisation is
    /// reported as [`StorageError::Unhealthy`] before anything is listed. The
    /// inventory is incremental when the store tracks changes and the last
    /// successful inventory recorded a change cursor; otherwise it is full.
    pub async fn refresh(&self, target: Target) -> Result<Refreshed, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let storage = match target {
            Target::Primary => MusicStorageRepository::get_active_primary(catalog)
                .await?
                .ok_or(StorageError::NoPrimary)?,
            Target::Storage(id) => MusicStorageRepository::get(catalog, id)
                .await?
                .ok_or(StorageError::StorageNotFound(id))?,
        };
        if !storage.enabled {
            return Err(StorageError::InvalidTarget(format!(
                "storage {} is disconnected",
                storage.id
            )));
        }
        let store = self.open_store(&storage).await?;
        let health = store.health().await?;
        if health.status == HealthStatus::AuthenticationRequired {
            return Err(unhealthy(health));
        }
        let inventoried = inventory(
            catalog,
            storage.id,
            store.as_ref(),
            health.capabilities.supports_change_tracking,
            false,
        )
        .await?;
        self.clear_fault(storage.id);
        let report = self
            .reconcile_active(&BTreeMap::from([(storage.id, Some(health))]))
            .await?;
        Ok(Refreshed {
            storage_id: storage.id,
            run_id: inventoried.run_id,
            generation_id: inventoried.generation_id,
            object_count: inventoried.object_count,
            removal_count: inventoried.removal_count,
            incremental: inventoried.incremental,
            report,
        })
    }
}

/// One finalized inventory generation.
pub(crate) struct Inventoried {
    pub run_id: Uuid,
    pub generation_id: Uuid,
    pub object_count: u64,
    pub removal_count: u64,
    pub incremental: bool,
}

/// Lists every page of `store` into a staged generation and, once the whole
/// sequence has been traversed, finalizes it together with its sync run.
///
/// Incremental listing resumes from the change cursor recorded by the last
/// successful inventory run, and is used only when `change_tracking` holds and
/// `force_full` does not. When a listing ends without a new change cursor the
/// prior one is kept. Any failure aborts the generation, leaving the previous
/// finalized inventory visible, and fails the run; an unreachable store aborts
/// as `Unavailable` rather than `Failed`.
pub(crate) async fn inventory(
    catalog: &TursoCatalog,
    storage_id: Uuid,
    store: &dyn MusicAssetStore,
    change_tracking: bool,
    force_full: bool,
) -> Result<Inventoried, StorageError> {
    let prior_cursor = SyncRunRepository::latest(
        catalog,
        SyncRunFilter {
            kind: Some(SyncRunKind::StorageInventory),
            status: Some(SyncStatus::Succeeded),
            storage_id: Some(storage_id),
            ..Default::default()
        },
    )
    .await?
    .and_then(|run| run.sync_cursor);
    let mode = match &prior_cursor {
        Some(cursor) if change_tracking && !force_full => InventoryMode::Incremental {
            change_cursor: cursor.clone(),
        },
        _ => InventoryMode::Full,
    };
    let incremental = matches!(mode, InventoryMode::Incremental { .. });
    let mut run = SyncRun::new(SyncRunKind::StorageInventory, None, Some(storage_id));
    run.start().expect("a new sync run can start");
    SyncRunRepository::upsert(catalog, run.clone()).await?;
    let generation_id = Uuid::new_v4();
    AssetRepository::begin_inventory_generation(
        catalog,
        InventoryGenerationStart {
            id: generation_id,
            storage_id,
            run_id: run.id,
            kind: if incremental {
                InventoryGenerationKind::Incremental
            } else {
                InventoryGenerationKind::Full
            },
            started_at: Utc::now(),
        },
    )
    .await?;

    let listed = stage_pages(
        catalog,
        store,
        storage_id,
        generation_id,
        mode,
        prior_cursor,
        &mut run,
    )
    .await;
    let finalized = match listed {
        Ok(listed) => finalize(catalog, generation_id, &mut run, &listed)
            .await
            .map(|()| listed),
        Err(error) => Err(error),
    };
    match finalized {
        Ok(listed) => Ok(Inventoried {
            run_id: run.id,
            generation_id,
            object_count: listed.objects,
            removal_count: listed.removals,
            incremental,
        }),
        Err(error) => {
            let _ = AssetRepository::abort_inventory_generation(
                catalog,
                generation_id,
                InventoryGenerationAbort {
                    kind: abort_kind(&error),
                    finished_at: Utc::now(),
                    message: Some(error.to_string()),
                },
            )
            .await;
            let _ = run.fail(sync_error(&error));
            let _ = SyncRunRepository::upsert(catalog, run).await;
            Err(error)
        }
    }
}

struct Listed {
    objects: u64,
    removals: u64,
    change_cursor: Option<SyncCursor>,
}

async fn stage_pages(
    catalog: &TursoCatalog,
    store: &dyn MusicAssetStore,
    storage_id: Uuid,
    generation_id: Uuid,
    mode: InventoryMode,
    prior_cursor: Option<SyncCursor>,
    run: &mut SyncRun,
) -> Result<Listed, StorageError> {
    let mut page_cursor = None;
    let mut listed = Listed {
        objects: 0,
        removals: 0,
        change_cursor: prior_cursor.clone(),
    };
    loop {
        let page = store
            .inventory_page(InventoryRequest {
                mode: mode.clone(),
                page_cursor: page_cursor.clone(),
                page_size: Some(INVENTORY_PAGE_SIZE),
                prefix: None,
                include_checksums: true,
            })
            .await?;
        listed.objects += page.objects.len() as u64;
        listed.removals += page.removals.len() as u64;
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
        AssetRepository::stage_inventory_page(
            catalog,
            generation_id,
            InventoryStagingPage {
                objects: page.objects,
                tombstones,
            },
        )
        .await?;
        run.progress
            .discover(listed.objects + listed.removals - run.progress.discovered_items);
        if let Some(cursor) = page.next_change_cursor {
            listed.change_cursor = Some(cursor);
        }
        page_cursor = page.next_page_cursor;
        // The durable cursor stays at the prior high-water mark until the whole
        // sequence is finalized; only the page continuation advances.
        let mut checkpoint = SyncCheckpoint::new(run.progress.processed_items);
        checkpoint.sync_cursor = prior_cursor.clone();
        checkpoint.page_cursor = page_cursor.clone();
        run.update_checkpoint(checkpoint);
        SyncRunRepository::upsert(catalog, run.clone()).await?;
        if page_cursor.is_none() {
            return Ok(listed);
        }
    }
}

/// Activates the generation and records the run's new change cursor in one
/// transaction, so the cursor never runs ahead of the visible inventory.
async fn finalize(
    catalog: &TursoCatalog,
    generation_id: Uuid,
    run: &mut SyncRun,
    listed: &Listed,
) -> Result<(), StorageError> {
    // `run` stays running until the commit succeeds, so a failed finalize can
    // still be recorded as a failed run.
    let mut succeeded = run.clone();
    for _ in 0..listed.objects + listed.removals {
        succeeded.progress.record_success();
    }
    let mut checkpoint = SyncCheckpoint::new(succeeded.progress.processed_items);
    checkpoint.sync_cursor = listed.change_cursor.clone();
    succeeded.update_checkpoint(checkpoint);
    succeeded.succeed().expect("a running sync run can succeed");
    let transaction = TransactionManager::begin(catalog).await?;
    transaction
        .assets()
        .finalize_inventory_generation(generation_id, Utc::now())
        .await?;
    transaction.sync_runs().upsert(succeeded.clone()).await?;
    transaction.commit().await?;
    *run = succeeded;
    Ok(())
}

fn abort_kind(error: &StorageError) -> InventoryGenerationAbortKind {
    match error {
        StorageError::Provider(error)
            if matches!(
                error.kind,
                MusicStorageErrorKind::AuthenticationRequired
                    | MusicStorageErrorKind::RateLimited
                    | MusicStorageErrorKind::TemporarilyUnavailable
                    | MusicStorageErrorKind::NotFound
            ) =>
        {
            InventoryGenerationAbortKind::Unavailable
        }
        _ => InventoryGenerationAbortKind::Failed,
    }
}

fn sync_error(error: &StorageError) -> SyncError {
    let kind = match error {
        StorageError::Provider(error) => match error.kind {
            MusicStorageErrorKind::AuthenticationRequired => SyncErrorKind::Authentication,
            MusicStorageErrorKind::PermissionDenied => SyncErrorKind::Authorization,
            MusicStorageErrorKind::RateLimited => SyncErrorKind::RateLimited,
            MusicStorageErrorKind::TemporarilyUnavailable => SyncErrorKind::TemporarilyUnavailable,
            MusicStorageErrorKind::NotFound => SyncErrorKind::NotFound,
            MusicStorageErrorKind::ChecksumMismatch => SyncErrorKind::Integrity,
            MusicStorageErrorKind::Io => SyncErrorKind::Io,
            MusicStorageErrorKind::InvalidData => SyncErrorKind::InvalidData,
            _ => SyncErrorKind::Provider,
        },
        StorageError::Repository(error) if error.kind == RepositoryErrorKind::Conflict => {
            SyncErrorKind::Conflict
        }
        StorageError::Repository(error) if error.is_retryable() => {
            SyncErrorKind::TemporarilyUnavailable
        }
        StorageError::Io(_) => SyncErrorKind::Io,
        _ => SyncErrorKind::Provider,
    };
    SyncError::new(kind, "music_storage", error.to_string(), error.retryable())
}
