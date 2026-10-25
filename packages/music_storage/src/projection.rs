//! What the dashboard shows about storages: each one's health and fault, and
//! the reconciliation report against the active primary.

use rec_core::{MusicStorage, MusicStorageRepository, StorageHealth};
use rec_sync::ReconciliationReport;

use crate::error::StorageError;
use crate::reconcile::ProbedHealth;
use crate::storages::{MusicStorages, StorageFault};

/// One configured storage as it stands right now.
#[derive(Debug, Clone)]
pub struct StorageView {
    pub storage: MusicStorage,
    pub is_active_primary: bool,
    /// Whether an adapter is open for it.
    pub open: bool,
    /// The health probe of the open adapter, when it answered.
    pub health: Option<StorageHealth>,
    /// Why it could not be opened or is unhealthy, as last recorded.
    pub fault: Option<StorageFault>,
}

/// Every storage plus reconciliation against the active primary.
#[derive(Debug, Clone)]
pub struct StorageProjection {
    pub storages: Vec<StorageView>,
    pub report: ReconciliationReport,
}

impl MusicStorages {
    /// Reconciles against the active primary without inventorying, and probes
    /// the health of every open store.
    ///
    /// Stores that are not open are not opened here, so projecting never
    /// triggers provider authorisation.
    pub async fn project(&self) -> Result<StorageProjection, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let active_primary = MusicStorageRepository::get_active_primary(catalog)
            .await?
            .map(|storage| storage.id);
        let faults = self
            .0
            .faults
            .read()
            .expect("storage fault lock poisoned")
            .clone();
        let mut probed = ProbedHealth::new();
        let mut storages = Vec::new();
        for storage in MusicStorageRepository::list(catalog).await? {
            let store = self.0.stores.get(storage.id);
            let health = match &store {
                Some(store) => {
                    let health = store.health().await.ok();
                    probed.insert(storage.id, health.clone());
                    health
                }
                None => None,
            };
            storages.push(StorageView {
                is_active_primary: active_primary == Some(storage.id),
                open: store.is_some(),
                health,
                fault: faults.get(&storage.id).cloned(),
                storage,
            });
        }
        let report = self.reconcile_active(&probed).await?;
        Ok(StorageProjection { storages, report })
    }
}
