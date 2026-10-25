//! Test doubles for the music storage module: an in-memory store with real
//! SHA-256 hashing, a scriptable driver, and a throwaway catalog.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rec_core::{
    BoxFuture, ContentHash, DownloadRequest, DownloadResult, HashAlgorithm, HealthStatus,
    InventoryMode, InventoryPage, InventoryRequest, MusicAssetStore, MusicStorage,
    MusicStorageError, MusicStorageErrorKind, MusicStorageKind, StorageCapabilities, StorageHealth,
    StorageObject, StorageObjectRemoval, StorageObjectVersion, StorageVersionPage,
    StorageVersionRequest, SyncCursor, UploadRequest,
};
use rec_persistence::TursoCatalog;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::driver::{Provisioned, StorageDriver, StorageSetup};
use crate::error::StorageError;
use crate::roots::RootLocator;

/// An empty in-memory catalog with every migration applied.
pub async fn temp_catalog() -> Arc<TursoCatalog> {
    Arc::new(
        TursoCatalog::in_memory()
            .await
            .expect("in-memory catalog should open"),
    )
}

/// Capabilities of a writable store that can store and verify music.
pub fn writable_capabilities() -> StorageCapabilities {
    StorageCapabilities {
        can_inventory: true,
        can_stat: true,
        can_upload: true,
        can_download: true,
        can_create_containers: true,
        supports_atomic_replace: false,
        supports_change_tracking: true,
        supports_version_history: false,
        provider_hash_algorithms: vec![HashAlgorithm::Sha256],
    }
}

/// The SHA-256 content hash of `bytes`.
pub fn sha256(bytes: &[u8]) -> ContentHash {
    ContentHash::sha256(format!("{:x}", Sha256::digest(bytes))).expect("valid sha256 digest")
}

/// What a [`MemoryStore`]'s health probe reports.
#[derive(Debug, Clone)]
pub struct HealthScript {
    pub status: HealthStatus,
    pub message: Option<String>,
    pub capabilities: StorageCapabilities,
    /// When set, the probe fails with this error instead.
    pub error: Option<MusicStorageError>,
}

impl Default for HealthScript {
    fn default() -> Self {
        Self {
            status: HealthStatus::Healthy,
            message: None,
            capabilities: writable_capabilities(),
            error: None,
        }
    }
}

#[derive(Default)]
struct MemoryState {
    health: HealthScript,
    health_calls: usize,
    objects: BTreeMap<String, (StorageObject, Vec<u8>)>,
    /// Removed object IDs with the revision that removed them.
    removed: BTreeMap<String, u64>,
    revision: u64,
    inventory_failures: Vec<MusicStorageError>,
    inventory_requests: Vec<InventoryRequest>,
    upload_failures: Vec<MusicStorageError>,
    corrupt_uploads: bool,
    uploads: Vec<UploadRequest>,
    downloads: Vec<DownloadRequest>,
}

/// An in-memory [`MusicAssetStore`] holding real bytes.
pub struct MemoryStore {
    storage_id: Uuid,
    state: Mutex<MemoryState>,
}

impl MemoryStore {
    /// A healthy, writable, empty store.
    pub fn new(storage_id: Uuid) -> Self {
        Self::with_health(storage_id, HealthScript::default())
    }

    pub fn with_health(storage_id: Uuid, health: HealthScript) -> Self {
        Self {
            storage_id,
            state: Mutex::new(MemoryState {
                health,
                ..MemoryState::default()
            }),
        }
    }

    pub fn storage_id(&self) -> Uuid {
        self.storage_id
    }

    /// Replaces what later health probes report.
    pub fn set_health(&self, health: HealthScript) {
        self.lock().health = health;
    }

    pub fn health_calls(&self) -> usize {
        self.lock().health_calls
    }

    /// Stores `bytes` as `object_id` as if it already existed in the storage.
    pub fn insert(&self, object_id: &str, bytes: &[u8]) -> StorageObject {
        let mut state = self.lock();
        state.revision += 1;
        let object = memory_object(self.storage_id, object_id, bytes, state.revision);
        state
            .objects
            .insert(object_id.into(), (object.clone(), bytes.to_vec()));
        state.removed.remove(object_id);
        object
    }

    /// Stores `bytes` as `object_id` with no provider hash, as a storage that
    /// cannot report checksums would list it.
    pub fn insert_unhashed(&self, object_id: &str, bytes: &[u8]) -> StorageObject {
        let mut state = self.lock();
        state.revision += 1;
        let mut object = memory_object(self.storage_id, object_id, bytes, state.revision);
        object.hashes.clear();
        state
            .objects
            .insert(object_id.into(), (object.clone(), bytes.to_vec()));
        state.removed.remove(object_id);
        object
    }

    /// Deletes `object_id` as if it disappeared from the storage.
    pub fn remove(&self, object_id: &str) {
        let mut state = self.lock();
        if state.objects.remove(object_id).is_some() {
            state.revision += 1;
            let revision = state.revision;
            state.removed.insert(object_id.into(), revision);
        }
    }

    /// Makes the next inventory page request fail with `error`.
    pub fn fail_next_inventory(&self, error: MusicStorageError) {
        self.lock().inventory_failures.push(error);
    }

    /// Every inventory page request received, oldest first.
    pub fn inventory_requests(&self) -> Vec<InventoryRequest> {
        self.lock().inventory_requests.clone()
    }

    /// Makes the next upload fail with `error`.
    pub fn fail_next_upload(&self, error: MusicStorageError) {
        self.lock().upload_failures.push(error);
    }

    /// Makes later uploads store damaged bytes without noticing, as a
    /// provider that corrupts data in transit or at rest would.
    pub fn corrupt_uploads(&self, corrupt: bool) {
        self.lock().corrupt_uploads = corrupt;
    }

    pub fn bytes(&self, object_id: &str) -> Option<Vec<u8>> {
        self.lock()
            .objects
            .get(object_id)
            .map(|(_, bytes)| bytes.clone())
    }

    pub fn object_ids(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    pub fn uploads(&self) -> Vec<UploadRequest> {
        self.lock().uploads.clone()
    }

    pub fn downloads(&self) -> Vec<DownloadRequest> {
        self.lock().downloads.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().expect("memory store lock poisoned")
    }
}

impl MusicAssetStore for MemoryStore {
    fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
        let result = {
            let mut state = self.lock();
            state.health_calls += 1;
            match state.health.error.clone() {
                Some(error) => Err(error),
                None => Ok(StorageHealth {
                    status: state.health.status,
                    checked_at: Utc::now(),
                    message: state.health.message.clone(),
                    capabilities: state.health.capabilities.clone(),
                }),
            }
        };
        Box::pin(std::future::ready(result))
    }

    /// One page holding everything (full) or everything changed after the
    /// cursor's revision (incremental); the change cursor is the revision.
    fn inventory_page(
        &self,
        request: InventoryRequest,
    ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
        let result = (|| {
            let mut state = self.lock();
            state.inventory_requests.push(request.clone());
            if !state.inventory_failures.is_empty() {
                return Err(state.inventory_failures.remove(0));
            }
            let since = match &request.mode {
                InventoryMode::Full => None,
                InventoryMode::Incremental { change_cursor } => {
                    Some(change_cursor.value.parse::<u64>().map_err(|_| {
                        MusicStorageError::new(
                            MusicStorageErrorKind::InvalidData,
                            "unknown change cursor",
                        )
                    })?)
                }
            };
            let changed = |revision: u64| since.is_none_or(|since| revision > since);
            Ok(InventoryPage {
                objects: state
                    .objects
                    .values()
                    .map(|(object, _)| object)
                    .filter(|object| changed(object_revision(object)))
                    .cloned()
                    .collect(),
                removals: state
                    .removed
                    .iter()
                    .filter(|(_, revision)| since.is_some() && changed(**revision))
                    .map(|(object_id, revision)| StorageObjectRemoval {
                        object_id: object_id.clone(),
                        provider_version: Some(revision.to_string()),
                        removed_at: Utc::now(),
                    })
                    .collect(),
                next_page_cursor: None,
                next_change_cursor: Some(SyncCursor::new(state.revision.to_string())),
                progress: None,
            })
        })();
        Box::pin(std::future::ready(result))
    }

    fn stat<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
        let result = self
            .lock()
            .objects
            .get(object_id)
            .map(|(object, _)| object.clone())
            .ok_or_else(|| not_found(object_id));
        Box::pin(std::future::ready(result))
    }

    fn versions_page(
        &self,
        request: StorageVersionRequest,
    ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
        let result = self
            .lock()
            .objects
            .get(&request.object_id)
            .map(|(object, _)| StorageVersionPage {
                versions: vec![StorageObjectVersion {
                    object_id: object.object_id.clone(),
                    version_id: object.provider_version.clone().unwrap_or_default(),
                    modified_at: object.modified_at,
                    size_bytes: Some(object.size_bytes),
                    hashes: object.hashes.clone(),
                    is_current: true,
                }],
                next_page_cursor: None,
            })
            .ok_or_else(|| not_found(&request.object_id));
        Box::pin(std::future::ready(result))
    }

    fn upload(
        &self,
        request: UploadRequest,
    ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
        let result = (|| {
            let mut state = self.lock();
            state.uploads.push(request.clone());
            if !state.upload_failures.is_empty() {
                return Err(state.upload_failures.remove(0));
            }
            let mut bytes = std::fs::read(&request.source_path).map_err(|error| {
                MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
            })?;
            if state.corrupt_uploads {
                bytes.push(0xFF);
            } else if let Some(expected) = &request.expected_checksum {
                if expected != &sha256(&bytes) {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::ChecksumMismatch,
                        "uploaded bytes do not match the expected checksum",
                    ));
                }
            }
            state.revision += 1;
            let object = memory_object(
                self.storage_id,
                &request.object_path,
                &bytes,
                state.revision,
            );
            state
                .objects
                .insert(request.object_path.clone(), (object.clone(), bytes));
            Ok(object)
        })();
        Box::pin(std::future::ready(result))
    }

    fn download(
        &self,
        request: DownloadRequest,
    ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
        let result = (|| {
            let mut state = self.lock();
            state.downloads.push(request.clone());
            let (object, bytes) = state
                .objects
                .get(&request.object_id)
                .cloned()
                .ok_or_else(|| not_found(&request.object_id))?;
            let checksum = sha256(&bytes);
            if request
                .expected_checksum
                .as_ref()
                .is_some_and(|expected| expected != &checksum)
            {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::ChecksumMismatch,
                    "downloaded bytes do not match the expected checksum",
                ));
            }
            if let Some(parent) = request.destination_path.parent() {
                std::fs::create_dir_all(parent).map_err(io_error)?;
            }
            std::fs::write(&request.destination_path, &bytes).map_err(io_error)?;
            Ok(DownloadResult {
                object,
                destination_path: request.destination_path.clone(),
                bytes_written: bytes.len() as u64,
                checksum_verified: request.expected_checksum.is_some(),
                checksum: Some(checksum),
            })
        })();
        Box::pin(std::future::ready(result))
    }
}

fn memory_object(storage_id: Uuid, object_id: &str, bytes: &[u8], revision: u64) -> StorageObject {
    StorageObject {
        storage_id,
        object_id: object_id.into(),
        name: object_id.rsplit('/').next().unwrap_or(object_id).into(),
        display_path: Some(object_id.into()),
        size_bytes: bytes.len() as u64,
        media_type: None,
        modified_at: Some(Utc::now()),
        provider_version: Some(revision.to_string()),
        hashes: vec![sha256(bytes)],
        embedded_asset_id: None,
        is_trashed: false,
    }
}

fn object_revision(object: &StorageObject) -> u64 {
    object
        .provider_version
        .as_deref()
        .and_then(|version| version.parse().ok())
        .unwrap_or_default()
}

fn not_found(object_id: &str) -> MusicStorageError {
    MusicStorageError::new(
        MusicStorageErrorKind::NotFound,
        format!("object {object_id} does not exist"),
    )
}

fn io_error(error: std::io::Error) -> MusicStorageError {
    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
}

/// What a [`FakeDriver`] was asked to do.
#[derive(Debug, Clone, Default)]
pub struct DriverCalls {
    pub provisioned: Vec<StorageSetup>,
    pub opened: Vec<Uuid>,
    pub released_grants: Vec<String>,
}

#[derive(Default)]
struct FakeDriverState {
    calls: DriverCalls,
    health: HealthScript,
    open_error: Option<StorageError>,
    release_error: Option<StorageError>,
    stores: BTreeMap<Uuid, Arc<MemoryStore>>,
}

/// A scriptable [`StorageDriver`] backed by [`MemoryStore`]s.
///
/// Provisioning uses [`StorageSetup::location`] as the root identifier (or a
/// fresh one). Roots are host paths unless a remote namespace is configured.
pub struct FakeDriver {
    kind: MusicStorageKind,
    remote_namespace: Option<String>,
    grant: Option<String>,
    direct_root: Option<PathBuf>,
    state: Mutex<FakeDriverState>,
}

const GRANT_CONFIG_KEY: &str = "grant";

impl FakeDriver {
    /// A driver whose roots are host paths.
    pub fn host(kind: MusicStorageKind) -> Self {
        Self {
            kind,
            remote_namespace: None,
            grant: None,
            direct_root: None,
            state: Mutex::new(FakeDriverState::default()),
        }
    }

    /// A driver whose roots are folders in `namespace`.
    pub fn remote(kind: MusicStorageKind, namespace: impl Into<String>) -> Self {
        Self {
            remote_namespace: Some(namespace.into()),
            ..Self::host(kind)
        }
    }

    /// Storages this driver provisions carry `grant`.
    pub fn with_grant(mut self, grant: impl Into<String>) -> Self {
        self.grant = Some(grant.into());
        self
    }

    /// Objects play directly from `root`.
    pub fn with_direct_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.direct_root = Some(root.into());
        self
    }

    /// What stores opened from now on report from their health probe.
    pub fn set_health(&self, health: HealthScript) {
        self.lock().health = health;
    }

    /// Makes every later `open` fail with `error`.
    pub fn fail_open(&self, error: StorageError) {
        self.lock().open_error = Some(error);
    }

    /// Makes later `release_grant` calls fail with `error` (`None` lets them
    /// succeed again). Failed releases are not recorded.
    pub fn fail_release_grant(&self, error: Option<StorageError>) {
        self.lock().release_error = error;
    }

    pub fn calls(&self) -> DriverCalls {
        self.lock().calls.clone()
    }

    /// The store opened for `storage_id`, if any.
    pub fn store(&self, storage_id: Uuid) -> Option<Arc<MemoryStore>> {
        self.lock().stores.get(&storage_id).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeDriverState> {
        self.state.lock().expect("fake driver lock poisoned")
    }
}

impl StorageDriver for FakeDriver {
    fn kind(&self) -> MusicStorageKind {
        self.kind.clone()
    }

    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> BoxFuture<'a, Result<Provisioned, StorageError>> {
        self.lock().calls.provisioned.push(setup.clone());
        let mut config = BTreeMap::new();
        if let Some(grant) = &self.grant {
            config.insert(GRANT_CONFIG_KEY.to_owned(), grant.clone());
        }
        let provisioned = Provisioned {
            root_identifier: setup
                .location
                .clone()
                .unwrap_or_else(|| format!("fake-root-{}", Uuid::new_v4())),
            suggested_name: Some("Fake storage".into()),
            config,
        };
        Box::pin(std::future::ready(Ok(provisioned)))
    }

    fn open<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<Arc<dyn MusicAssetStore>, StorageError>> {
        let result = {
            let mut state = self.lock();
            state.calls.opened.push(storage.id);
            match state.open_error.clone() {
                Some(error) => Err(error),
                None => {
                    let health = state.health.clone();
                    let store = state
                        .stores
                        .entry(storage.id)
                        .or_insert_with(|| Arc::new(MemoryStore::with_health(storage.id, health)))
                        .clone();
                    Ok(store as Arc<dyn MusicAssetStore>)
                }
            }
        };
        Box::pin(std::future::ready(result))
    }

    fn root(&self, storage: &MusicStorage) -> Option<RootLocator> {
        let root = storage.root_identifier.as_ref()?;
        Some(match &self.remote_namespace {
            Some(namespace) => RootLocator::Remote {
                namespace: namespace.clone(),
                path: vec![root.clone()],
            },
            None => RootLocator::host_path(root),
        })
    }

    fn direct_path(&self, _storage: &MusicStorage, object_id: &str) -> Option<PathBuf> {
        self.direct_root.as_ref().map(|root| root.join(object_id))
    }

    fn grant_id(&self, storage: &MusicStorage) -> Option<String> {
        storage.config.get(GRANT_CONFIG_KEY).cloned()
    }

    fn release_grant<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        let mut state = self.lock();
        if let Some(error) = state.release_error.clone() {
            return Box::pin(std::future::ready(Err(error)));
        }
        if let Some(grant) = self.grant_id(storage) {
            state.calls.released_grants.push(grant);
        }
        Box::pin(std::future::ready(Ok(())))
    }
}
