//! Local-directory storage driver.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use music_storage::{Provisioned, RootLocator, StorageDriver, StorageError, StorageSetup};
use music_storage_local::LocalMusicStorage;
use rec_core::{BoxFuture, MusicAssetStore, MusicStorage, MusicStorageKind};

use crate::services::local_library::{
    canonical_directory, LocalLibraryError, LocalSelectionProvider,
};

/// Storages in a directory on the host. Without a typed location, the folder
/// is chosen with the injected native picker.
pub struct LocalDriver {
    selection: Arc<dyn LocalSelectionProvider>,
}

impl LocalDriver {
    pub fn new(selection: Arc<dyn LocalSelectionProvider>) -> Self {
        Self { selection }
    }
}

impl StorageDriver for LocalDriver {
    fn kind(&self) -> MusicStorageKind {
        MusicStorageKind::LocalDirectory
    }

    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> BoxFuture<'a, Result<Provisioned, StorageError>> {
        Box::pin(async move {
            let (root, suggested_name) = match &setup.location {
                Some(location) => (PathBuf::from(location), None),
                None => {
                    tracing::info!("requesting local storage folder selection");
                    let selected = self
                        .selection
                        .select_source()
                        .await
                        .map_err(local_error)?
                        .ok_or(StorageError::Cancelled)?;
                    tracing::info!(name = %selected.name, root = %selected.root.display(), "local storage folder selected");
                    (selected.root, Some(selected.name))
                }
            };
            let root = ensure_directory(&root).map_err(local_error)?;
            Ok(Provisioned {
                root_identifier: root.to_string_lossy().into_owned(),
                suggested_name,
                config: BTreeMap::new(),
            })
        })
    }

    fn open<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<Arc<dyn MusicAssetStore>, StorageError>> {
        Box::pin(async move {
            let store = adapter(storage).map_err(local_error)?;
            Ok(Arc::new(store) as Arc<dyn MusicAssetStore>)
        })
    }

    fn root(&self, storage: &MusicStorage) -> Option<RootLocator> {
        storage.root_identifier.as_ref().map(RootLocator::host_path)
    }

    fn direct_path(&self, storage: &MusicStorage, object_id: &str) -> Option<PathBuf> {
        adapter(storage).ok()?.playback_path(object_id).ok()
    }
}

fn adapter(storage: &MusicStorage) -> Result<LocalMusicStorage, LocalLibraryError> {
    let root = storage
        .root_identifier
        .as_ref()
        .ok_or_else(|| LocalLibraryError::InvalidRoot("local storage has no root".into()))?;
    Ok(LocalMusicStorage::new(storage.id, root)?)
}

fn ensure_directory(path: &Path) -> Result<PathBuf, LocalLibraryError> {
    fs::create_dir_all(path)?;
    canonical_directory(path)
}

/// Keeps the local slice's wording ("local root is unavailable", "folder
/// chooser") so the dashboard's failure guidance still recognises it.
fn local_error(error: LocalLibraryError) -> StorageError {
    match error {
        LocalLibraryError::SelectionCancelled => StorageError::Cancelled,
        LocalLibraryError::Storage(error) => error,
        LocalLibraryError::Io(error) => error.into(),
        other => StorageError::Driver(other.to_string()),
    }
}

/// A composition with only the local driver, for desktop tests.
#[cfg(test)]
pub(crate) fn local_only_storages(
    catalog: &Arc<rec_persistence::TursoCatalog>,
    selection: Arc<dyn LocalSelectionProvider>,
) -> music_storage::MusicStorages {
    let root = std::env::temp_dir().join(format!("rec-bag-test-storages-{}", uuid::Uuid::new_v4()));
    music_storage::MusicStorages::new(
        catalog.clone(),
        music_storage::StorageDrivers::new(vec![Arc::new(LocalDriver::new(selection))]),
        music_storage::MusicStoragesConfig {
            cache_root: root.join("cache"),
            staging_root: root.join("staging"),
            default_cache_max_bytes: 1024 * 1024,
        },
        Arc::new(music_storage::LiveStores::default()),
        Arc::new(music_storage::NoHandoff),
    )
}
