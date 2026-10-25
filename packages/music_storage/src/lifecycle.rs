//! Releasing what rec_bag holds for a storage, and changing its settings.

use chrono::Utc;
use rec_core::{MusicStorage, MusicStorageRepository, TransactionManager};
use uuid::Uuid;

use crate::error::StorageError;
use crate::storages::{MusicStorages, CONFIG_ORGANIZE_BY_ARTIST, CONFIG_VERIFY_AFTER_COPY};

/// What to give up for a storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Stop using the storage, keeping its catalog records and offline cache.
    Disconnect,
    /// Delete its offline cache. Catalog records and stored files are kept.
    ClearCache,
    /// Revoke the authorisation grant it uses, disconnecting every storage of
    /// the same kind that shares the grant.
    RevokeGrant,
    /// Disconnect and delete everything the catalog recorded about it, and its
    /// offline cache. Stored files, owned assets and the grant are kept.
    Forget,
}

/// The user-editable settings of a storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSettings {
    pub name: String,
    pub organize_by_artist: bool,
    pub verify_after_copy: bool,
}

impl MusicStorages {
    /// Releases `release` for `storage_id`.
    ///
    /// Every variant is idempotent: repeating it succeeds and changes nothing
    /// more. Forgetting a storage the catalog no longer has therefore succeeds
    /// (and still removes any offline cache left behind); the other variants
    /// report [`StorageError::StorageNotFound`] for an unknown storage.
    ///
    /// The primary can be replaced but never removed, so disconnecting or
    /// forgetting the active primary, or revoking a grant the active primary
    /// uses, fails with [`StorageError::PrimaryInUse`] and changes nothing.
    /// Clearing the cache only deletes derived files, so it is allowed for the
    /// active primary too.
    ///
    /// Revoking asks the driver to give up the grant first; when that fails
    /// (for example offline), no storage is disconnected, so the revoke can
    /// simply be retried.
    pub async fn release(&self, storage_id: Uuid, release: Release) -> Result<(), StorageError> {
        let catalog = self.0.catalog.as_ref();
        let Some(storage) = MusicStorageRepository::get(catalog, storage_id).await? else {
            return match release {
                Release::Forget => self.forget(storage_id).await,
                _ => Err(StorageError::StorageNotFound(storage_id)),
            };
        };
        match release {
            Release::ClearCache => self.0.cache.purge(storage_id),
            Release::Disconnect => {
                self.refuse_active_primary(&[storage_id]).await?;
                self.disconnect(vec![storage]).await
            }
            Release::RevokeGrant => self.revoke_grant(storage).await,
            Release::Forget => {
                self.refuse_active_primary(&[storage_id]).await?;
                self.disconnect(vec![storage]).await?;
                self.forget(storage_id).await
            }
        }
    }

    /// Replaces the user-editable settings of `storage_id`.
    pub async fn update_settings(
        &self,
        storage_id: Uuid,
        settings: StorageSettings,
    ) -> Result<MusicStorage, StorageError> {
        let name = settings.name.trim();
        if name.is_empty() {
            return Err(StorageError::InvalidTarget(
                "storage name cannot be empty".into(),
            ));
        }
        let catalog = self.0.catalog.as_ref();
        let mut storage = MusicStorageRepository::get(catalog, storage_id)
            .await?
            .ok_or(StorageError::StorageNotFound(storage_id))?;
        storage.name = name.to_owned();
        storage.config.insert(
            CONFIG_ORGANIZE_BY_ARTIST.into(),
            settings.organize_by_artist.to_string(),
        );
        storage.config.insert(
            CONFIG_VERIFY_AFTER_COPY.into(),
            settings.verify_after_copy.to_string(),
        );
        storage.updated_at = Utc::now();
        MusicStorageRepository::upsert(catalog, storage.clone()).await?;
        Ok(storage)
    }

    /// Gives up `storage`'s grant, then disconnects every storage of the same
    /// kind that shares it.
    async fn revoke_grant(&self, storage: MusicStorage) -> Result<(), StorageError> {
        let driver = self.driver(&storage.kind)?;
        let grant = driver.grant_id(&storage).ok_or_else(|| {
            StorageError::InvalidTarget(format!(
                "storage {} has no authorisation grant to revoke",
                storage.id
            ))
        })?;
        let sharing: Vec<MusicStorage> = MusicStorageRepository::list(self.0.catalog.as_ref())
            .await?
            .into_iter()
            .filter(|other| {
                other.kind == storage.kind
                    && driver.grant_id(other).as_deref() == Some(grant.as_str())
            })
            .collect();
        let ids: Vec<Uuid> = sharing.iter().map(|other| other.id).collect();
        self.refuse_active_primary(&ids).await?;
        driver.release_grant(&storage).await?;
        self.disconnect(sharing).await
    }

    /// Disables `storages` in one transaction and closes their stores. Their
    /// catalog records and offline caches are kept.
    async fn disconnect(&self, storages: Vec<MusicStorage>) -> Result<(), StorageError> {
        let enabled: Vec<MusicStorage> = storages
            .iter()
            .filter(|storage| storage.enabled)
            .cloned()
            .collect();
        if !enabled.is_empty() {
            let transaction = TransactionManager::begin(self.0.catalog.as_ref()).await?;
            for mut storage in enabled {
                storage.enabled = false;
                storage.updated_at = Utc::now();
                transaction.music_storages().upsert(storage).await?;
            }
            transaction.commit().await?;
        }
        for storage in &storages {
            self.0.stores.remove(storage.id);
            self.clear_fault(storage.id);
        }
        Ok(())
    }

    /// Deletes the offline cache and every catalog record kept for
    /// `storage_id` other than owned assets and canonical tracks.
    async fn forget(&self, storage_id: Uuid) -> Result<(), StorageError> {
        self.0.stores.remove(storage_id);
        self.clear_fault(storage_id);
        self.0.cache.purge(storage_id)?;
        self.0
            .catalog
            .delete_storage_provider_data(storage_id)
            .await?;
        Ok(())
    }

    /// Fails with [`StorageError::PrimaryInUse`] when the active primary is
    /// one of `storage_ids`.
    async fn refuse_active_primary(&self, storage_ids: &[Uuid]) -> Result<(), StorageError> {
        match MusicStorageRepository::get_active_primary(self.0.catalog.as_ref()).await? {
            Some(active) if storage_ids.contains(&active.id) => {
                Err(StorageError::PrimaryInUse { id: active.id })
            }
            _ => Ok(()),
        }
    }
}
