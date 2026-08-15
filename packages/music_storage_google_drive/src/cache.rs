use md5::Md5;
use rec_core::{ContentHash, HashAlgorithm, MusicStorageError, MusicStorageErrorKind};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub directory: PathBuf,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    path: String,
    size_bytes: u64,
    last_access: u128,
    pinned: bool,
    checksum: ContentHash,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheIndex {
    entries: BTreeMap<String, CacheEntry>,
}

pub(crate) struct PlaybackCache {
    config: CacheConfig,
    operation_lock: Mutex<()>,
}

impl PlaybackCache {
    pub(crate) fn new(config: CacheConfig) -> Result<Self, MusicStorageError> {
        fs::create_dir_all(&config.directory).map_err(io_error)?;
        Ok(Self {
            config,
            operation_lock: Mutex::new(()),
        })
    }

    pub(crate) fn path_for(&self, object_id: &str) -> PathBuf {
        self.config.directory.join(cache_key(object_id))
    }

    pub(crate) fn cached_path(
        &self,
        object_id: &str,
    ) -> Result<Option<PathBuf>, MusicStorageError> {
        let _guard = self.lock()?;
        let mut index = self.read_index()?;
        let Some(entry) = index.entries.get(object_id).cloned() else {
            return Ok(None);
        };
        let path = self.safe_entry_path(&entry)?;
        let valid = path.is_file()
            && fs::metadata(&path).map_err(io_error)?.len() == entry.size_bytes
            && hash_file(&path, entry.checksum.algorithm())? == entry.checksum;
        if !valid {
            let _ = fs::remove_file(&path);
            index.entries.remove(object_id);
            self.write_index(&index)?;
            return Ok(None);
        }
        if let Some(entry) = index.entries.get_mut(object_id) {
            entry.last_access = now_unix_nanos();
        }
        self.write_index(&index)?;
        Ok(Some(path))
    }

    pub(crate) fn record(
        &self,
        object_id: &str,
        path: &Path,
        size_bytes: u64,
        checksum: ContentHash,
    ) -> Result<(), MusicStorageError> {
        let _guard = self.lock()?;
        let expected_path = self.path_for(object_id);
        if path != expected_path || !path.is_file() {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "cache entry path does not match its object key",
            ));
        }
        if fs::metadata(path).map_err(io_error)?.len() != size_bytes
            || hash_file(path, checksum.algorithm())? != checksum
        {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::ChecksumMismatch,
                "cache file failed integrity verification",
            ));
        }
        let mut index = self.read_index()?;
        let pinned = index
            .entries
            .get(object_id)
            .is_some_and(|entry| entry.pinned);
        index.entries.insert(
            object_id.to_owned(),
            CacheEntry {
                path: cache_key(object_id),
                size_bytes,
                last_access: now_unix_nanos(),
                pinned,
                checksum,
            },
        );
        self.evict_index(&mut index)?;
        self.write_index(&index)
    }

    pub(crate) fn set_pinned(
        &self,
        object_id: &str,
        pinned: bool,
    ) -> Result<(), MusicStorageError> {
        let _guard = self.lock()?;
        let mut index = self.read_index()?;
        let entry = index.entries.get_mut(object_id).ok_or_else(|| {
            MusicStorageError::new(
                MusicStorageErrorKind::NotFound,
                "object is not present in the playback cache",
            )
        })?;
        entry.pinned = pinned;
        self.write_index(&index)
    }

    pub(crate) fn evict(&self) -> Result<(), MusicStorageError> {
        let _guard = self.lock()?;
        let mut index = self.read_index()?;
        self.evict_index(&mut index)?;
        self.write_index(&index)
    }

    fn evict_index(&self, index: &mut CacheIndex) -> Result<(), MusicStorageError> {
        let mut total: u64 = index.entries.values().map(|entry| entry.size_bytes).sum();
        let mut candidates: Vec<(String, CacheEntry)> = index
            .entries
            .iter()
            .filter(|(_, entry)| !entry.pinned)
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect();
        candidates.sort_by_key(|(_, entry)| entry.last_access);
        for (object_id, entry) in candidates {
            if total <= self.config.max_bytes {
                break;
            }
            let path = self.safe_entry_path(&entry)?;
            match fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(entry.size_bytes);
                    index.entries.remove(&object_id);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    total = total.saturating_sub(entry.size_bytes);
                    index.entries.remove(&object_id);
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    fn safe_entry_path(&self, entry: &CacheEntry) -> Result<PathBuf, MusicStorageError> {
        let path = Path::new(&entry.path);
        if path.components().count() != 1 || path.file_name().is_none() {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "cache index contains an unsafe path",
            ));
        }
        Ok(self.config.directory.join(path))
    }

    fn index_path(&self) -> PathBuf {
        self.config.directory.join("cache-index.json")
    }

    fn read_index(&self) -> Result<CacheIndex, MusicStorageError> {
        let path = self.index_path();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(index) => Ok(index),
                Err(_) => {
                    let corrupt = self
                        .config
                        .directory
                        .join(format!("cache-index.corrupt-{}.json", now_unix_nanos()));
                    let _ = fs::rename(path, corrupt);
                    Ok(CacheIndex::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CacheIndex::default()),
            Err(error) => Err(io_error(error)),
        }
    }

    fn write_index(&self, index: &CacheIndex) -> Result<(), MusicStorageError> {
        let path = self.index_path();
        let temporary = self
            .config
            .directory
            .join(format!(".cache-index-{}.tmp", now_unix_nanos()));
        let bytes = serde_json::to_vec(index).map_err(|error| {
            MusicStorageError::new(MusicStorageErrorKind::InvalidData, error.to_string())
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, &path).map_err(io_error)?;
        sync_directory(&self.config.directory)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, MusicStorageError> {
        self.operation_lock.lock().map_err(|_| {
            MusicStorageError::new(MusicStorageErrorKind::Io, "playback cache lock is poisoned")
        })
    }
}

fn cache_key(object_id: &str) -> String {
    let digest = Sha256::digest(object_id.as_bytes());
    format!("object-{digest:x}.cache")
}

fn hash_file(path: &Path, algorithm: &HashAlgorithm) -> Result<ContentHash, MusicStorageError> {
    let value = match algorithm {
        HashAlgorithm::Sha256 => digest_file::<Sha256>(path)?,
        HashAlgorithm::Sha1 => digest_file::<Sha1>(path)?,
        HashAlgorithm::Md5 => digest_file::<Md5>(path)?,
        HashAlgorithm::Other(name) => {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                format!("unsupported cache checksum algorithm: {name}"),
            ))
        }
    };
    ContentHash::new(algorithm.clone(), value).map_err(|error| {
        MusicStorageError::new(MusicStorageErrorKind::InvalidData, error.to_string())
    })
}

fn digest_file<D: Digest + Default>(path: &Path) -> Result<String, MusicStorageError> {
    let file = File::open(path).map_err(io_error)?;
    let mut reader = BufReader::new(file);
    let mut digest = D::default();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let output = digest.finalize();
    Ok(output.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sync_directory(path: &Path) -> Result<(), MusicStorageError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    Ok(())
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn io_error(error: std::io::Error) -> MusicStorageError {
    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
}
