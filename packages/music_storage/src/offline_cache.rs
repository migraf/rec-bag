//! The offline cache: verified local copies of primary objects, kept so remote
//! storages can play (and keep playing offline).
//!
//! Cached files are derived assets: losing one loses nothing, so every entry is
//! re-verified before use and discarded when it does not check out. Each
//! storage has its own directory under the cache root holding the cached files
//! and an index. Files are filled through the store's ordinary `download`,
//! written beside the index first and renamed into place only once their
//! SHA-256 has been computed locally, and the index itself is replaced
//! atomically. Unpinned entries are evicted least recently used first once a
//! storage's cache exceeds its budget.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rec_core::{
    AssetLocation, ContentHash, DerivedAssetRole, DownloadRequest, MusicAssetStore, MusicStorage,
    MusicStorageError, MusicStorageErrorKind,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::StorageError;

/// Storage configuration key for a storage's own offline cache budget.
pub(crate) const CONFIG_CACHE_MAX_BYTES: &str = "cache_max_bytes";

const INDEX_FILE: &str = "cache-index.json";

/// Identifies one revision of one storage object in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheKey(String);

impl CacheKey {
    /// The key for `location`'s object at its current revision: the provider
    /// version when the storage reports one, otherwise the expected content
    /// hash. A new revision therefore never reuses an older cached file.
    pub(crate) fn for_location(location: &AssetLocation) -> Self {
        let revision = location
            .provider_version
            .clone()
            .or_else(|| {
                location
                    .expected_hash
                    .as_ref()
                    .map(|hash| hash.value().to_owned())
            })
            .unwrap_or_default();
        Self::new(&location.object_id, &revision)
    }

    fn new(object_id: &str, revision: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(object_id.as_bytes());
        digest.update([0]);
        digest.update(revision.as_bytes());
        Self(format!("object-{:x}.cache", digest.finalize()))
    }

    /// The cached file's name, which is also its index key.
    fn file_name(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    path: String,
    object_id: String,
    source_asset_id: Uuid,
    role: DerivedAssetRole,
    size_bytes: u64,
    last_access: u128,
    pinned: bool,
    /// SHA-256 computed locally when the entry was filled.
    checksum: ContentHash,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheIndex {
    entries: BTreeMap<String, CacheEntry>,
}

/// Every storage's offline cache, under one root.
pub(crate) struct OfflineCache {
    root: PathBuf,
    default_max_bytes: u64,
    /// Serialises index reads and writes across storages.
    operation_lock: Mutex<()>,
}

impl OfflineCache {
    pub(crate) fn new(root: PathBuf, default_max_bytes: u64) -> Self {
        Self {
            root,
            default_max_bytes,
            operation_lock: Mutex::new(()),
        }
    }

    /// The verified cached file for `key`, refreshing its last use.
    ///
    /// An entry whose file is missing, has the wrong size, or does not hash to
    /// the recorded (and, when known, the expected) SHA-256 is discarded.
    pub(crate) fn lookup(
        &self,
        storage: &MusicStorage,
        key: &CacheKey,
        expected: Option<&ContentHash>,
    ) -> Result<Option<PathBuf>, StorageError> {
        let _guard = self.lock()?;
        let directory = self.directory(storage.id);
        let mut index = read_index(&directory)?;
        let Some(entry) = index.entries.get(key.file_name()).cloned() else {
            return Ok(None);
        };
        let path = safe_entry_path(&directory, &entry)?;
        let expected_matches = expected
            .filter(|hash| hash.is_canonical_sha256())
            .is_none_or(|hash| hash == &entry.checksum);
        let valid = expected_matches
            && path.is_file()
            && fs::metadata(&path)?.len() == entry.size_bytes
            && sha256_file(&path)? == entry.checksum;
        if !valid {
            let _ = fs::remove_file(&path);
            index.entries.remove(key.file_name());
            write_index(&directory, &index)?;
            return Ok(None);
        }
        if let Some(entry) = index.entries.get_mut(key.file_name()) {
            entry.last_access = now_unix_nanos();
        }
        write_index(&directory, &index)?;
        Ok(Some(path))
    }

    /// Downloads `location`'s object from `store` into the cache and returns
    /// the cached file.
    ///
    /// The download lands in a temporary file and is renamed into place only
    /// after its locally computed SHA-256 matches the location's expected
    /// canonical hash (when there is one). Recording it may evict other
    /// unpinned entries, never the one just filled.
    pub(crate) async fn fill(
        &self,
        storage: &MusicStorage,
        store: &dyn MusicAssetStore,
        key: &CacheKey,
        location: &AssetLocation,
    ) -> Result<PathBuf, StorageError> {
        let directory = self.directory(storage.id);
        fs::create_dir_all(&directory)?;
        let expected = location
            .expected_hash
            .clone()
            .filter(ContentHash::is_canonical_sha256);
        let temporary = directory.join(format!(".fill-{}.tmp", Uuid::new_v4()));
        let filled = async {
            store
                .download(DownloadRequest {
                    object_id: location.object_id.clone(),
                    destination_path: temporary.clone(),
                    range: None,
                    expected_checksum: expected.clone(),
                    overwrite: true,
                })
                .await?;
            let size_bytes = fs::metadata(&temporary)?.len();
            let checksum = sha256_file(&temporary)?;
            if expected.as_ref().is_some_and(|hash| hash != &checksum) {
                return Err(StorageError::Verification(
                    "cached bytes do not match the original's content hash".into(),
                ));
            }
            self.record(storage, key, location, &temporary, size_bytes, checksum)
        }
        .await;
        if filled.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        filled
    }

    fn record(
        &self,
        storage: &MusicStorage,
        key: &CacheKey,
        location: &AssetLocation,
        temporary: &Path,
        size_bytes: u64,
        checksum: ContentHash,
    ) -> Result<PathBuf, StorageError> {
        let _guard = self.lock()?;
        let directory = self.directory(storage.id);
        let path = directory.join(key.file_name());
        fs::rename(temporary, &path)?;
        sync_directory(&directory)?;
        let mut index = read_index(&directory)?;
        let pinned = index
            .entries
            .get(key.file_name())
            .is_some_and(|entry| entry.pinned);
        index.entries.insert(
            key.file_name().to_owned(),
            CacheEntry {
                path: key.file_name().to_owned(),
                object_id: location.object_id.clone(),
                source_asset_id: location.asset_id,
                role: DerivedAssetRole::OfflineCache,
                size_bytes,
                last_access: now_unix_nanos(),
                pinned,
                checksum,
            },
        );
        evict(&directory, &mut index, self.max_bytes(storage), Some(key))?;
        write_index(&directory, &index)?;
        Ok(path)
    }

    /// Pins or unpins a cached entry. Pinned entries are never evicted.
    /// Unpinning an entry that is not cached is a no-op.
    pub(crate) fn set_pinned(
        &self,
        storage: &MusicStorage,
        key: &CacheKey,
        pinned: bool,
    ) -> Result<(), StorageError> {
        let _guard = self.lock()?;
        let directory = self.directory(storage.id);
        let mut index = read_index(&directory)?;
        match index.entries.get_mut(key.file_name()) {
            Some(entry) => entry.pinned = pinned,
            None if !pinned => return Ok(()),
            None => {
                return Err(StorageError::Provider(MusicStorageError::new(
                    MusicStorageErrorKind::NotFound,
                    "object is not present in the offline cache",
                )))
            }
        }
        // Unpinning may put the cache back over its budget.
        evict(&directory, &mut index, self.max_bytes(storage), None)?;
        write_index(&directory, &index)
    }

    /// Removes every cached file of `storage_id`. Repeating it is harmless.
    pub(crate) fn purge(&self, storage_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.lock()?;
        let path = self.directory(storage_id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
                fs::remove_file(&path)?;
            }
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => {
                return Err(StorageError::Io(
                    "cache path is not a regular file, symlink, or directory".into(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// Whether `key` is cached and pinned, for tests.
    #[cfg(test)]
    pub(crate) fn is_pinned(&self, storage: &MusicStorage, key: &CacheKey) -> bool {
        read_index(&self.directory(storage.id))
            .ok()
            .and_then(|index| index.entries.get(key.file_name()).map(|entry| entry.pinned))
            .unwrap_or(false)
    }

    pub(crate) fn directory(&self, storage_id: Uuid) -> PathBuf {
        self.root.join(storage_id.to_string())
    }

    fn max_bytes(&self, storage: &MusicStorage) -> u64 {
        storage
            .config
            .get(CONFIG_CACHE_MAX_BYTES)
            .and_then(|value| value.parse().ok())
            .unwrap_or(self.default_max_bytes)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, StorageError> {
        self.operation_lock
            .lock()
            .map_err(|_| StorageError::Io("offline cache lock is poisoned".into()))
    }
}

/// Evicts unpinned entries, least recently used first, until the cache fits
/// `max_bytes`. `keep` is never evicted.
fn evict(
    directory: &Path,
    index: &mut CacheIndex,
    max_bytes: u64,
    keep: Option<&CacheKey>,
) -> Result<(), StorageError> {
    let mut total: u64 = index.entries.values().map(|entry| entry.size_bytes).sum();
    let mut candidates: Vec<(String, CacheEntry)> = index
        .entries
        .iter()
        .filter(|(key, entry)| !entry.pinned && keep.is_none_or(|keep| keep.file_name() != *key))
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect();
    candidates.sort_by_key(|(_, entry)| entry.last_access);
    for (key, entry) in candidates {
        if total <= max_bytes {
            break;
        }
        let path = safe_entry_path(directory, &entry)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        total = total.saturating_sub(entry.size_bytes);
        index.entries.remove(&key);
    }
    Ok(())
}

/// The entry's file, refusing index paths that could leave the directory.
fn safe_entry_path(directory: &Path, entry: &CacheEntry) -> Result<PathBuf, StorageError> {
    let path = Path::new(&entry.path);
    if path.components().count() != 1 || path.file_name().is_none() {
        return Err(StorageError::Io(
            "offline cache index contains an unsafe path".into(),
        ));
    }
    Ok(directory.join(path))
}

/// Reads the index; a corrupt one is set aside and the cache starts empty.
fn read_index(directory: &Path) -> Result<CacheIndex, StorageError> {
    let path = directory.join(INDEX_FILE);
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(index) => Ok(index),
            Err(_) => {
                let corrupt =
                    directory.join(format!("cache-index.corrupt-{}.json", now_unix_nanos()));
                let _ = fs::rename(path, corrupt);
                Ok(CacheIndex::default())
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CacheIndex::default()),
        Err(error) => Err(error.into()),
    }
}

/// Replaces the index atomically: a synced temporary file renamed over it.
fn write_index(directory: &Path, index: &CacheIndex) -> Result<(), StorageError> {
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".cache-index-{}.tmp", now_unix_nanos()));
    let bytes = serde_json::to_vec(index).map_err(|error| StorageError::Io(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, directory.join(INDEX_FILE))?;
    sync_directory(directory)
}

/// The SHA-256 content hash of the file at `path`, computed locally.
pub(crate) fn sha256_file(path: &Path) -> Result<ContentHash, StorageError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    ContentHash::sha256(format!("{:x}", digest.finalize()))
        .map_err(|error| StorageError::Io(error.to_string()))
}

fn sync_directory(path: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    File::open(path).and_then(|file| file.sync_all())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
