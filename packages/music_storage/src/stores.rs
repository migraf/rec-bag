//! The opened adapter for every connected storage.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use rec_core::MusicAssetStore;
use uuid::Uuid;

/// Opened stores by storage ID, shared by every workflow that talks to a
/// storage (connect, replication, verification, the dashboard's health probe).
///
/// Every adapter is a [`MusicAssetStore`], so callers never need to know which
/// provider a store belongs to.
#[derive(Default)]
pub struct LiveStores {
    stores: RwLock<BTreeMap<Uuid, Arc<dyn MusicAssetStore>>>,
}

impl LiveStores {
    /// Registers `store` for `storage_id`, returning the store it replaced.
    pub fn register(
        &self,
        storage_id: Uuid,
        store: Arc<dyn MusicAssetStore>,
    ) -> Option<Arc<dyn MusicAssetStore>> {
        self.stores
            .write()
            .expect("live store registry lock poisoned")
            .insert(storage_id, store)
    }

    /// Forgets the store for `storage_id`, returning it if one was open.
    pub fn remove(&self, storage_id: Uuid) -> Option<Arc<dyn MusicAssetStore>> {
        self.stores
            .write()
            .expect("live store registry lock poisoned")
            .remove(&storage_id)
    }

    /// The open store for `storage_id`, if any.
    pub fn get(&self, storage_id: Uuid) -> Option<Arc<dyn MusicAssetStore>> {
        self.stores
            .read()
            .expect("live store registry lock poisoned")
            .get(&storage_id)
            .cloned()
    }

    /// Whether a store is open for `storage_id`.
    pub fn contains(&self, storage_id: Uuid) -> bool {
        self.stores
            .read()
            .expect("live store registry lock poisoned")
            .contains_key(&storage_id)
    }
}
