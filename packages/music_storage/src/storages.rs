//! The music storage module's caller-facing entry point.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use rec_core::{
    HealthStatus, MusicAssetStore, MusicStorage, MusicStorageKind, MusicStorageRepository,
    StorageCapabilities, StorageHealth, StorageRole,
};
use rec_persistence::TursoCatalog;
use uuid::Uuid;

use crate::catalog_io;
use crate::driver::{StorageDriver, StorageDrivers, StorageSetup};
use crate::error::StorageError;
use crate::handoff::ReplicationHandoff;
use crate::inventory::inventory;
use crate::offline_cache::OfflineCache;
use crate::reconcile::ProbedHealth;
use crate::roots::{source_root, storage_and_source_may_coexist, RootLocator, RootPurpose};
use crate::stores::LiveStores;

/// Module-owned storage configuration keys.
pub(crate) const CONFIG_ORGANIZE_BY_ARTIST: &str = "organize_by_artist";
pub(crate) const CONFIG_VERIFY_AFTER_COPY: &str = "verify_after_copy";

/// Name of the placeholder storage the catalog migration created for legacy
/// local track paths. It records evidence only and is never opened.
const MIGRATED_STORAGE_NAME: &str = "Migrated local files";

/// Host locations the module keeps derived files in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicStoragesConfig {
    /// Root of the offline cache; one directory per storage.
    pub cache_root: PathBuf,
    /// Where downloads are staged before verification.
    pub staging_root: PathBuf,
    /// Offline cache budget for storages that do not set their own.
    pub default_cache_max_bytes: u64,
}

/// A storage that could not be opened or is not healthy right now.
#[derive(Debug, Clone)]
pub struct StorageFault {
    pub storage_id: Uuid,
    pub name: String,
    pub kind: MusicStorageKind,
    pub error: StorageError,
}

/// Every music storage workflow, for every provider.
///
/// Cheap to clone; clones share the same live stores and faults.
#[derive(Clone)]
pub struct MusicStorages(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) catalog: Arc<TursoCatalog>,
    pub(crate) drivers: StorageDrivers,
    pub(crate) config: MusicStoragesConfig,
    pub(crate) stores: Arc<LiveStores>,
    pub(crate) handoff: Arc<dyn ReplicationHandoff>,
    pub(crate) cache: OfflineCache,
    pub(crate) faults: RwLock<BTreeMap<Uuid, StorageFault>>,
}

impl MusicStorages {
    pub fn new(
        catalog: Arc<TursoCatalog>,
        drivers: StorageDrivers,
        config: MusicStoragesConfig,
        stores: Arc<LiveStores>,
        handoff: Arc<dyn ReplicationHandoff>,
    ) -> Self {
        let cache = OfflineCache::new(config.cache_root.clone(), config.default_cache_max_bytes);
        Self(Arc::new(Inner {
            catalog,
            drivers,
            config,
            stores,
            handoff,
            cache,
            faults: RwLock::new(BTreeMap::new()),
        }))
    }

    /// The opened stores, shared with replication.
    pub fn live_stores(&self) -> &Arc<LiveStores> {
        &self.0.stores
    }

    /// Faults recorded by the last [`MusicStorages::open_all`].
    pub fn faults(&self) -> Vec<StorageFault> {
        self.0
            .faults
            .read()
            .expect("storage fault lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Opens every enabled persisted storage and probes its health.
    ///
    /// Disabled storages and the migration placeholder are closed. A storage
    /// whose driver is missing or whose adapter cannot be opened is reported
    /// as a fault; an opened one is registered even when unhealthy, so work
    /// for it can be deferred rather than lost. Healthy probes refresh the
    /// persisted capabilities; unhealthy ones keep them and add a fault.
    pub async fn open_all(&self) -> Result<Vec<StorageFault>, StorageError> {
        let storages = MusicStorageRepository::list(self.0.catalog.as_ref()).await?;
        let mut faults = BTreeMap::new();
        for storage in storages {
            if !storage.enabled || is_migrated_synthetic_storage(&storage) {
                self.0.stores.remove(storage.id);
                continue;
            }
            if let Err(error) = self.open_one(storage.clone()).await {
                faults.insert(
                    storage.id,
                    StorageFault {
                        storage_id: storage.id,
                        name: storage.name,
                        kind: storage.kind,
                        error,
                    },
                );
            }
        }
        let list = faults.values().cloned().collect();
        *self.0.faults.write().expect("storage fault lock poisoned") = faults;
        Ok(list)
    }

    async fn open_one(&self, mut storage: MusicStorage) -> Result<(), StorageError> {
        let driver = self.driver(&storage.kind).inspect_err(|_| {
            self.0.stores.remove(storage.id);
        })?;
        let store = match driver.open(&storage).await {
            Ok(store) => store,
            Err(error) => {
                self.0.stores.remove(storage.id);
                return Err(error);
            }
        };
        self.0.stores.register(storage.id, store.clone());
        let health = store.health().await?;
        if health.status != HealthStatus::Healthy {
            return Err(unhealthy(health));
        }
        if health.capabilities != storage.capabilities {
            storage.capabilities = health.capabilities;
            storage.updated_at = Utc::now();
            MusicStorageRepository::upsert(self.0.catalog.as_ref(), storage).await?;
        }
        Ok(())
    }

    /// Connects a new storage of `kind` for `role`.
    ///
    /// The driver provisions a root, which must not already be configured and
    /// must be separate from other storages and sources. The opened store must
    /// report `Healthy` with enough capabilities to store and verify music;
    /// only then is the storage persisted, with the probed capabilities, and
    /// registered in [`LiveStores`]. It is then fully inventoried and the
    /// catalog reconciled. A `Primary` request is rejected before provisioning
    /// while an active primary exists; it is persisted as `Unassigned` and
    /// becomes the active primary once its inventory has completed.
    ///
    /// An `Unassigned` request whose root is already held by an enabled
    /// `Unassigned` storage reconnects that storage (with the new name and
    /// settings) instead of failing with [`StorageError::AlreadyConfigured`],
    /// so a failed primary migration can be retried with the same location.
    ///
    /// When anything after provisioning fails before the storage is persisted,
    /// the driver's grant is released unless another persisted storage still
    /// uses it.
    pub async fn connect(
        &self,
        kind: MusicStorageKind,
        role: StorageRole,
        setup: &StorageSetup,
    ) -> Result<MusicStorage, StorageError> {
        let driver = self.driver(&kind)?;
        if role == StorageRole::Primary {
            if let Some(active) =
                MusicStorageRepository::get_active_primary(self.0.catalog.as_ref()).await?
            {
                return Err(StorageError::PrimaryExists { id: active.id });
            }
        }
        let provisioned = driver.provision(setup).await?;
        let name = setup
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or(provisioned.suggested_name)
            .unwrap_or_else(|| "Music storage".into());
        let persisted_role = match role {
            StorageRole::Primary => StorageRole::Unassigned,
            other => other,
        };
        let mut candidate =
            MusicStorage::new(name, kind, persisted_role, StorageCapabilities::default())
                .with_root_identifier(provisioned.root_identifier);
        candidate.config = provisioned.config;
        candidate.config.insert(
            CONFIG_ORGANIZE_BY_ARTIST.into(),
            setup.organize_by_artist.to_string(),
        );
        candidate.config.insert(
            CONFIG_VERIFY_AFTER_COPY.into(),
            setup.verify_after_copy.to_string(),
        );

        let (storage, store, health) =
            match self.admit(driver.as_ref(), role, candidate.clone()).await {
                Ok(admitted) => admitted,
                Err(error) => {
                    self.release_unshared_grant(driver.as_ref(), &candidate)
                        .await;
                    return Err(error);
                }
            };
        self.settle(role, storage, store.as_ref(), health).await
    }

    /// The tail of [`Self::connect`] for a persisted, registered storage: a
    /// full inventory, then (for `Primary`) the switch from no active primary,
    /// then reconciliation. A failure here keeps the storage configured so the
    /// refresh can be retried.
    async fn settle(
        &self,
        role: StorageRole,
        mut storage: MusicStorage,
        store: &dyn MusicAssetStore,
        health: StorageHealth,
    ) -> Result<MusicStorage, StorageError> {
        let catalog = self.0.catalog.as_ref();
        inventory(
            catalog,
            storage.id,
            store,
            health.capabilities.supports_change_tracking,
            true,
        )
        .await?;
        if role == StorageRole::Primary {
            storage =
                MusicStorageRepository::switch_active_primary(catalog, None, Some(storage.id))
                    .await?
                    .ok_or(StorageError::StorageNotFound(storage.id))?;
        }
        self.reconcile_active(&ProbedHealth::from([(storage.id, Some(health))]))
            .await?;
        // Re-read so the caller sees what the inventory recorded.
        Ok(MusicStorageRepository::get(catalog, storage.id)
            .await?
            .unwrap_or(storage))
    }

    async fn admit(
        &self,
        driver: &dyn StorageDriver,
        role: StorageRole,
        mut candidate: MusicStorage,
    ) -> Result<(MusicStorage, Arc<dyn MusicAssetStore>, StorageHealth), StorageError> {
        let existing = MusicStorageRepository::list(self.0.catalog.as_ref())
            .await?
            .into_iter()
            .find(|storage| {
                storage.kind == candidate.kind
                    && storage.root_identifier == candidate.root_identifier
            });
        match existing {
            // Connecting a candidate again (say, to retry a failed migration)
            // reconnects the one already there instead of adding another.
            Some(existing) if reconnects_candidate(role, &existing) => {
                let mut config = existing.config.clone();
                config.extend(candidate.config);
                candidate = MusicStorage {
                    name: candidate.name,
                    config,
                    ..existing
                };
            }
            Some(existing) => return Err(StorageError::AlreadyConfigured { id: existing.id }),
            None => {
                if let Some(root) = driver.root(&candidate) {
                    self.check_root_separation(&root, RootPurpose::Storage(role))
                        .await?;
                }
            }
        }
        let store = driver.open(&candidate).await?;
        let health = store.health().await?;
        if health.status != HealthStatus::Healthy {
            return Err(unhealthy(health));
        }
        if let Some(missing) = missing_capability(&health.capabilities) {
            return Err(StorageError::MissingCapability { role, missing });
        }
        candidate.capabilities = health.capabilities.clone();
        candidate.updated_at = Utc::now();
        MusicStorageRepository::upsert(self.0.catalog.as_ref(), candidate.clone()).await?;
        self.0.stores.register(candidate.id, store.clone());
        Ok((candidate, store, health))
    }

    /// The open store for `storage`, opening and registering it when needed.
    pub async fn open_store(
        &self,
        storage: &MusicStorage,
    ) -> Result<Arc<dyn MusicAssetStore>, StorageError> {
        if let Some(store) = self.0.stores.get(storage.id) {
            return Ok(store);
        }
        let store = self.driver(&storage.kind)?.open(storage).await?;
        self.0.stores.register(storage.id, store.clone());
        Ok(store)
    }

    /// Where `storage` is rooted, as its driver describes it; `None` when no
    /// driver handles its kind or it has no root yet.
    pub fn root_of(&self, storage: &MusicStorage) -> Option<RootLocator> {
        self.0.drivers.get(&storage.kind)?.root(storage)
    }

    /// Forgets a recorded fault once the storage has worked again.
    pub(crate) fn clear_fault(&self, storage_id: Uuid) {
        self.0
            .faults
            .write()
            .expect("storage fault lock poisoned")
            .remove(&storage_id);
    }

    async fn release_unshared_grant(&self, driver: &dyn StorageDriver, candidate: &MusicStorage) {
        let Some(grant) = driver.grant_id(candidate) else {
            return;
        };
        // When the catalog cannot be read, keep the grant: revoking one that
        // another storage relies on is worse than leaving an unused one.
        let Ok(storages) = MusicStorageRepository::list(self.0.catalog.as_ref()).await else {
            return;
        };
        let shared = storages.iter().any(|storage| {
            storage.id != candidate.id
                && storage.kind == candidate.kind
                && driver.grant_id(storage).as_deref() == Some(grant.as_str())
        });
        if !shared {
            // The connect already failed; a failed release must not mask why.
            let _ = driver.release_grant(candidate).await;
        }
    }

    /// Rejects `candidate` when it collides with a persisted storage or
    /// catalog source root.
    ///
    /// Storages never overlap each other. Between a storage and a source, a
    /// backup must be disjoint, while a primary (or primary candidate) may sit
    /// exactly at a source root but not inside or around it. Sources are not
    /// checked against each other.
    pub async fn check_root_separation(
        &self,
        candidate: &RootLocator,
        purpose: RootPurpose,
    ) -> Result<(), StorageError> {
        self.check_root_separation_except(candidate, purpose, None)
            .await
    }

    /// [`Self::check_root_separation`] for a persisted storage taking on a new
    /// role: its own root is not compared with itself.
    pub(crate) async fn check_root_separation_except(
        &self,
        candidate: &RootLocator,
        purpose: RootPurpose,
        own_storage: Option<Uuid>,
    ) -> Result<(), StorageError> {
        for storage in MusicStorageRepository::list(self.0.catalog.as_ref()).await? {
            if Some(storage.id) == own_storage {
                continue;
            }
            let Some(existing) = self
                .0
                .drivers
                .get(&storage.kind)
                .and_then(|driver| driver.root(&storage))
            else {
                continue;
            };
            let relation = candidate.relation(&existing);
            let allowed = match purpose {
                RootPurpose::Storage(_) => relation == crate::roots::RootRelation::Disjoint,
                RootPurpose::CatalogSource => {
                    storage_and_source_may_coexist(storage.role, relation)
                }
            };
            if !allowed {
                return Err(StorageError::RootsOverlap {
                    candidate: candidate.clone(),
                    existing,
                });
            }
        }
        let RootPurpose::Storage(role) = purpose else {
            return Ok(());
        };
        let sources = catalog_io::sources(self.0.catalog.as_ref()).await?;
        for existing in sources.iter().filter_map(source_root) {
            if !storage_and_source_may_coexist(role, candidate.relation(&existing)) {
                return Err(StorageError::RootsOverlap {
                    candidate: candidate.clone(),
                    existing,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn driver(
        &self,
        kind: &MusicStorageKind,
    ) -> Result<Arc<dyn StorageDriver>, StorageError> {
        self.0
            .drivers
            .get(kind)
            .cloned()
            .ok_or_else(|| StorageError::NoDriver(kind.clone()))
    }
}

/// Whether connecting a storage for `role` at `existing`'s root reconnects
/// `existing`: only an enabled primary candidate is picked up again.
fn reconnects_candidate(role: StorageRole, existing: &MusicStorage) -> bool {
    role == StorageRole::Unassigned
        && existing.role == StorageRole::Unassigned
        && existing.enabled
        && !is_migrated_synthetic_storage(existing)
}

/// Whether `storage` is the catalog migration's placeholder for legacy local
/// track paths, which must never be opened or become authoritative.
pub fn is_migrated_synthetic_storage(storage: &MusicStorage) -> bool {
    storage.kind == MusicStorageKind::LocalDirectory
        && storage.role == StorageRole::Unassigned
        && storage.name == MIGRATED_STORAGE_NAME
        && storage.root_identifier.is_none()
}

pub(crate) fn unhealthy(health: StorageHealth) -> StorageError {
    StorageError::Unhealthy {
        status: health.status,
        message: health
            .message
            .unwrap_or_else(|| format!("storage health is {:?}", health.status)),
    }
}

/// The first operation missing from [`StorageCapabilities::can_store_and_verify`].
pub(crate) fn missing_capability(capabilities: &StorageCapabilities) -> Option<&'static str> {
    if capabilities.can_store_and_verify() {
        None
    } else if !capabilities.can_inventory {
        Some("list its objects")
    } else if !capabilities.can_stat {
        Some("inspect an object")
    } else if !capabilities.can_upload {
        Some("store new objects")
    } else {
        Some("read objects back")
    }
}
