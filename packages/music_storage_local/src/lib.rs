//! Local filesystem implementation of the provider-neutral music storage contract.
//!
//! Object identifiers are normalized, root-relative paths.  The adapter never follows
//! symlinks while walking its root and excludes its `.rec_bag_history` directory from
//! normal inventory, preventing nested-root and history loops.

use chrono::{DateTime, Duration, Utc};
use rec_core::{
    BoxFuture, ByteRange, ContentHash, DownloadRequest, DownloadResult, HashAlgorithm,
    HealthStatus, InventoryMode, InventoryPage, InventoryRequest, MusicAssetStore,
    MusicStorageError, MusicStorageErrorKind, RetentionPolicy, StorageCapabilities, StorageHealth,
    StorageObject, StorageObjectRemoval, StorageObjectVersion, StorageVersionPage,
    StorageVersionRequest, SyncCursor, UploadRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const HISTORY_DIRECTORY: &str = ".rec_bag_history";
const OPERATIONS_DIRECTORY: &str = ".rec_bag_operations";
const TEMPORARY_PREFIX: &str = ".rec_bag-tmp-";
const COPY_BUFFER_SIZE: usize = 128 * 1024;
const LOCAL_CHANGE_CURSOR_VERSION: u8 = 1;
const LOCAL_PAGE_CURSOR_VERSION: u8 = 1;

/// Audio classification used by local inventory callers that need to distinguish
/// playable formats from files that are recognized but not currently supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioClassification {
    Supported,
    RecognizedUnsupported,
    NotAudio,
}

/// Classifies an extension without opening or decoding the file.
pub fn classify_audio_extension(extension: &str) -> AudioClassification {
    match extension.to_ascii_lowercase().as_str() {
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" => AudioClassification::Supported,
        "aif" | "aiff" | "alac" | "ape" | "opus" | "wma" => {
            AudioClassification::RecognizedUnsupported
        }
        _ => AudioClassification::NotAudio,
    }
}

/// Returns whether a path is a supported or recognized audio file.
pub fn is_recognized_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            classify_audio_extension(extension) != AudioClassification::NotAudio
        })
}

/// Failures specific to local storage configuration and safety checks.
#[derive(Debug, Error)]
pub enum LocalStorageError {
    #[error("storage root must not be empty")]
    EmptyRoot,
    #[error("storage root is a file, not a directory: {root}", root = .root.display())]
    RootIsFile { root: PathBuf },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// An external filesystem change observed by [`LocalChangeWatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalChangeKind {
    Created,
    Modified,
    Removed,
}

/// One root-relative file change emitted by the polling watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalChange {
    pub kind: LocalChangeKind,
    pub object_id: String,
    pub provider_version: Option<String>,
}

/// A portable, polling external-change watcher.
///
/// It is intentionally driven by the job engine with [`Self::poll_changes`] instead
/// of spawning a background thread. That gives removable volumes predictable failure
/// handling and avoids holding a filesystem watcher open after a drive is ejected.
pub struct LocalChangeWatcher {
    root: PathBuf,
    known: BTreeMap<String, String>,
}

impl LocalChangeWatcher {
    /// Takes an initial snapshot. No changes are reported until the next poll.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, LocalStorageError> {
        let root = root.as_ref().to_path_buf();
        let known = snapshot_versions(&root)?;
        Ok(Self { root, known })
    }

    /// Compares the current tree with the preceding snapshot.
    pub fn poll_changes(&mut self) -> Result<Vec<LocalChange>, LocalStorageError> {
        let current = snapshot_versions(&self.root)?;
        let mut changes = Vec::new();

        for (object_id, version) in &current {
            match self.known.get(object_id) {
                None => changes.push(LocalChange {
                    kind: LocalChangeKind::Created,
                    object_id: object_id.clone(),
                    provider_version: Some(version.clone()),
                }),
                Some(previous) if previous != version => changes.push(LocalChange {
                    kind: LocalChangeKind::Modified,
                    object_id: object_id.clone(),
                    provider_version: Some(version.clone()),
                }),
                Some(_) => {}
            }
        }
        for object_id in self.known.keys().filter(|key| !current.contains_key(*key)) {
            changes.push(LocalChange {
                kind: LocalChangeKind::Removed,
                object_id: object_id.clone(),
                provider_version: None,
            });
        }

        self.known = current;
        Ok(changes)
    }
}

/// A retained history version that may be removed after retention confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    pub object_id: String,
    pub version_id: String,
    pub path: PathBuf,
    pub modified_at: DateTime<Utc>,
    pub size_bytes: u64,
}

/// Local directory implementation of [`MusicAssetStore`].
///
/// The root is not created during construction so a configured removable drive can
/// remain unavailable without losing its configuration. The directory is created on
/// the first successful upload.
pub struct LocalMusicStorage {
    storage_id: Uuid,
    root: PathBuf,
    operation_lock: Mutex<()>,
}

impl LocalMusicStorage {
    /// Configures a local storage root. A missing root is valid and is reported as
    /// unavailable by [`MusicAssetStore::health`].
    pub fn new(storage_id: Uuid, root: impl AsRef<Path>) -> Result<Self, LocalStorageError> {
        let configured_root = root.as_ref();
        if configured_root.as_os_str().is_empty() {
            return Err(LocalStorageError::EmptyRoot);
        }
        let root = normalized_path(configured_root)?;
        if root.exists() && !root.is_dir() {
            return Err(LocalStorageError::RootIsFile { root });
        }
        let storage = Self {
            storage_id,
            root,
            operation_lock: Mutex::new(()),
        };
        if storage.root.is_dir() && is_effectively_writable(&storage.root) {
            storage.cleanup_orphaned_temporary_files()?;
        }
        Ok(storage)
    }

    /// Returns the configured, potentially unavailable storage root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the configured rec_bag storage identity.
    pub const fn storage_id(&self) -> Uuid {
        self.storage_id
    }

    /// Removes stale adapter-owned temporary files left by interrupted operations.
    pub fn cleanup_orphaned_temporary_files(&self) -> Result<usize, LocalStorageError> {
        if !self.root.is_dir() {
            return Ok(0);
        }
        let mut removed = 0;
        for path in walk_files(&self.root)? {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TEMPORARY_PREFIX))
            {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Creates a watcher snapshot for detecting external renames and modifications.
    pub fn watch_changes(&self) -> Result<LocalChangeWatcher, LocalStorageError> {
        LocalChangeWatcher::new(&self.root)
    }

    /// Resolves an object to a direct local playback path without copying it.
    pub fn playback_path(&self, object_id: &str) -> Result<PathBuf, MusicStorageError> {
        let path = self.object_path(object_id)?;
        if !path.is_file() {
            return Err(storage_error(
                MusicStorageErrorKind::NotFound,
                format!("object not found: {object_id}"),
            ));
        }
        Ok(path)
    }

    /// Lists history versions which are older than the policy window after retaining
    /// the configured number of most-recent versions per object. It does not delete.
    pub fn retention_candidates(
        &self,
        policy: RetentionPolicy,
    ) -> Result<Vec<RetentionCandidate>, MusicStorageError> {
        let history_root = self.history_root();
        if !history_root.exists() {
            return Ok(Vec::new());
        }

        let cutoff = Utc::now() - Duration::days(i64::from(policy.retention_days));
        let mut by_object: BTreeMap<String, Vec<RetentionCandidate>> = BTreeMap::new();
        for file in walk_files(&history_root).map_err(map_io_error)? {
            let relative = file.strip_prefix(&history_root).map_err(|_| {
                storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "history path escaped its root",
                )
            })?;
            let parent = relative.parent().unwrap_or(Path::new(""));
            let object_id = path_to_object_id(parent)?;
            let metadata = fs::metadata(&file).map_err(map_io_error)?;
            let modified_at = metadata
                .modified()
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now());
            let version_id = relative
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            by_object
                .entry(object_id)
                .or_default()
                .push(RetentionCandidate {
                    path: file,
                    version_id,
                    modified_at,
                    size_bytes: metadata.len(),
                    object_id: String::new(),
                });
        }

        let mut candidates = Vec::new();
        for (object_id, versions) in &mut by_object {
            versions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
            for (index, version) in versions.iter().enumerate() {
                if index >= policy.minimum_versions as usize && version.modified_at < cutoff {
                    let mut candidate = version.clone();
                    candidate.object_id = object_id.clone();
                    candidates.push(candidate);
                }
            }
        }
        Ok(candidates)
    }

    fn history_root(&self) -> PathBuf {
        self.root.join(HISTORY_DIRECTORY)
    }

    fn object_path(&self, object_id: &str) -> Result<PathBuf, MusicStorageError> {
        let relative = validate_object_id(object_id)?;
        let mut checked = self.root.clone();
        for component in relative.components() {
            checked.push(component.as_os_str());
            match fs::symlink_metadata(&checked) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(storage_error(
                        MusicStorageErrorKind::PermissionDenied,
                        "object path traverses a symbolic link",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(map_io_error(error)),
            }
        }
        Ok(self.root.join(relative))
    }

    fn ensure_root_for_write(&self) -> Result<(), MusicStorageError> {
        if self.root.exists() && !self.root.is_dir() {
            return Err(storage_error(
                MusicStorageErrorKind::InvalidData,
                format!("storage root is not a directory: {}", self.root.display()),
            ));
        }
        fs::create_dir_all(&self.root).map_err(map_io_error)?;
        if !is_effectively_writable(&self.root) {
            return Err(storage_error(
                MusicStorageErrorKind::PermissionDenied,
                format!("local storage is read-only: {}", self.root.display()),
            ));
        }
        Ok(())
    }

    fn storage_object(
        &self,
        object_id: &str,
        include_checksum: bool,
    ) -> Result<StorageObject, MusicStorageError> {
        let path = self.object_path(object_id)?;
        let metadata = fs::metadata(&path).map_err(map_io_error)?;
        if !metadata.is_file() {
            return Err(storage_error(
                MusicStorageErrorKind::NotFound,
                format!("object is not a file: {object_id}"),
            ));
        }
        let hashes = if include_checksum {
            vec![sha256_file(&path)?]
        } else {
            Vec::new()
        };
        Ok(StorageObject {
            storage_id: self.storage_id,
            object_id: object_id.to_owned(),
            name: path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_owned(),
            display_path: Some(object_id.to_owned()),
            size_bytes: metadata.len(),
            media_type: media_type(&path).map(str::to_owned),
            modified_at: metadata.modified().ok().map(DateTime::<Utc>::from),
            provider_version: provider_version(&metadata).ok(),
            hashes,
            embedded_asset_id: None,
            is_trashed: false,
        })
    }

    fn copy_to_destination(
        &self,
        source: &Path,
        destination: &Path,
        retain_previous: bool,
    ) -> Result<(), MusicStorageError> {
        let _guard = self
            .operation_lock
            .lock()
            .expect("local storage operation lock poisoned");
        self.copy_to_destination_locked(source, destination, retain_previous)
    }

    fn copy_to_destination_locked(
        &self,
        source: &Path,
        destination: &Path,
        retain_previous: bool,
    ) -> Result<(), MusicStorageError> {
        self.ensure_root_for_write()?;
        let parent = destination.parent().ok_or_else(|| {
            storage_error(
                MusicStorageErrorKind::InvalidData,
                "destination has no parent directory",
            )
        })?;
        fs::create_dir_all(parent).map_err(map_io_error)?;

        let temporary = parent.join(format!("{TEMPORARY_PREFIX}{}", Uuid::new_v4()));
        let result = (|| -> Result<(), MusicStorageError> {
            copy_file_with_sync(source, &temporary)?;
            if retain_previous && destination.is_file() {
                self.retain_previous(destination)?;
            }
            fs::rename(&temporary, destination).map_err(map_io_error)?;
            sync_directory(parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn upload_idempotently(
        &self,
        request: &UploadRequest,
        destination: &Path,
        idempotency_key: &str,
    ) -> Result<StorageObject, MusicStorageError> {
        if idempotency_key.is_empty() {
            return Err(storage_error(
                MusicStorageErrorKind::InvalidData,
                "upload idempotency key must not be empty",
            ));
        }
        let _guard = self
            .operation_lock
            .lock()
            .expect("local storage operation lock poisoned");
        self.ensure_root_for_write()?;
        let source_hash = sha256_file(&request.source_path)?;
        let source_size = fs::metadata(&request.source_path)
            .map_err(map_io_error)?
            .len();
        let operation_path = self.operation_path(idempotency_key)?;
        if let Some(mut operation) = read_local_operation(&operation_path)? {
            if operation.object_path != request.object_path
                || operation.source_size != source_size
                || operation.source_hash != source_hash
            {
                return Err(storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "upload idempotency key was reused for different content or destination",
                ));
            }
            if let Some(result) = operation.result {
                return Ok(result);
            }
            if destination.is_file() && sha256_file(destination)? == source_hash {
                let result = self.storage_object(&request.object_path, true)?;
                operation.result = Some(result.clone());
                write_local_operation(&operation_path, &operation)?;
                return Ok(result);
            }
        } else {
            write_local_operation(
                &operation_path,
                &LocalUploadOperation {
                    version: 1,
                    object_path: request.object_path.clone(),
                    source_size,
                    source_hash: source_hash.clone(),
                    result: None,
                },
            )?;
        }

        self.copy_to_destination_locked(&request.source_path, destination, true)?;
        let result = self.storage_object(&request.object_path, true)?;
        write_local_operation(
            &operation_path,
            &LocalUploadOperation {
                version: 1,
                object_path: request.object_path.clone(),
                source_size,
                source_hash,
                result: Some(result.clone()),
            },
        )?;
        Ok(result)
    }

    fn operation_path(&self, idempotency_key: &str) -> Result<PathBuf, MusicStorageError> {
        let directory = self.root.join(OPERATIONS_DIRECTORY);
        fs::create_dir_all(&directory).map_err(map_io_error)?;
        let digest = Sha256::digest(idempotency_key.as_bytes());
        Ok(directory.join(format!("{digest:x}.json")))
    }

    fn retain_previous(&self, destination: &Path) -> Result<(), MusicStorageError> {
        let relative = destination.strip_prefix(&self.root).map_err(|_| {
            storage_error(
                MusicStorageErrorKind::InvalidData,
                "destination escaped storage root",
            )
        })?;
        let history_directory = self.history_root().join(relative);
        fs::create_dir_all(&history_directory).map_err(map_io_error)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let version_path = history_directory.join(format!("{stamp}-{}.version", Uuid::new_v4()));
        copy_file_with_sync(destination, &version_path)?;
        sync_directory(&history_directory)
    }

    fn inventory_objects(
        &self,
        prefix: Option<&str>,
        include_checksums: bool,
    ) -> Result<Vec<StorageObject>, MusicStorageError> {
        if !self.root.is_dir() {
            return Err(unavailable_error(&self.root));
        }
        let prefix = prefix.map(validate_object_id).transpose()?;
        let mut objects = Vec::new();
        for path in walk_files(&self.root).map_err(map_io_error)? {
            let relative = path.strip_prefix(&self.root).map_err(|_| {
                storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "inventory path escaped storage root",
                )
            })?;
            if is_provider_internal_path(relative) || !is_recognized_audio_path(&path) {
                continue;
            }
            if let Some(prefix) = &prefix {
                if !relative.starts_with(prefix) {
                    continue;
                }
            }
            let object_id = path_to_object_id(relative)?;
            objects.push(self.storage_object(&object_id, include_checksums)?);
        }
        objects.sort_by(|left, right| left.object_id.cmp(&right.object_id));
        Ok(objects)
    }
}

impl MusicAssetStore for LocalMusicStorage {
    fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
        Box::pin(async move {
            let (status, message, capabilities) = if !self.root.is_dir() {
                (
                    HealthStatus::Unavailable,
                    Some(format!(
                        "local storage is unavailable: {}",
                        self.root.display()
                    )),
                    StorageCapabilities::default(),
                )
            } else {
                let read_only = !is_effectively_writable(&self.root);
                (
                    if read_only {
                        HealthStatus::Degraded
                    } else {
                        HealthStatus::Healthy
                    },
                    read_only.then(|| "local storage is read-only".to_owned()),
                    StorageCapabilities {
                        can_inventory: true,
                        can_stat: true,
                        can_upload: !read_only,
                        can_download: true,
                        can_create_containers: !read_only,
                        supports_atomic_replace: !read_only,
                        supports_change_tracking: true,
                        supports_version_history: true,
                        provider_hash_algorithms: vec![HashAlgorithm::Sha256],
                    },
                )
            };
            Ok(StorageHealth {
                status,
                checked_at: Utc::now(),
                message,
                capabilities,
            })
        })
    }

    fn inventory_page(
        &self,
        request: InventoryRequest,
    ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
        Box::pin(async move {
            if !self.root.is_dir() {
                return Err(unavailable_error(&self.root));
            }
            let mut state = if let Some(cursor) = request.page_cursor.as_ref() {
                let state = parse_inventory_page_cursor(cursor)?;
                state.validate_request(&request)?;
                state
            } else {
                let current = snapshot_versions(&self.root).map_err(map_local_error)?;
                let (objects, removals, mode) = match &request.mode {
                    InventoryMode::Full => (
                        self.inventory_objects(
                            request.prefix.as_deref(),
                            request.include_checksums,
                        )?,
                        Vec::new(),
                        LocalCursorMode::Full,
                    ),
                    InventoryMode::Incremental { change_cursor } => {
                        let previous = parse_change_cursor(change_cursor)?;
                        let mut changed = Vec::new();
                        let mut removals = Vec::new();
                        for (object_id, version) in &current {
                            if previous.snapshot.get(object_id) != Some(version)
                                && request
                                    .prefix
                                    .as_ref()
                                    .is_none_or(|prefix| object_id.starts_with(prefix))
                            {
                                changed.push(
                                    self.storage_object(object_id, request.include_checksums)?,
                                );
                            }
                        }
                        for (object_id, version) in &previous.snapshot {
                            if !current.contains_key(object_id)
                                && request
                                    .prefix
                                    .as_ref()
                                    .is_none_or(|prefix| object_id.starts_with(prefix))
                            {
                                removals.push(StorageObjectRemoval {
                                    object_id: object_id.clone(),
                                    provider_version: Some(version.clone()),
                                    removed_at: Utc::now(),
                                });
                            }
                        }
                        (changed, removals, LocalCursorMode::Incremental)
                    }
                };
                LocalInventoryPageCursor {
                    version: LOCAL_PAGE_CURSOR_VERSION,
                    mode,
                    prefix: request.prefix.clone(),
                    include_checksums: request.include_checksums,
                    objects,
                    removals,
                    offset: 0,
                    next_snapshot: current,
                }
            };
            let object_count = state.objects.len();
            let total = object_count.saturating_add(state.removals.len());
            let page_size = request.page_size.unwrap_or(500).clamp(1, 10_000) as usize;
            let start = state.offset;
            let end = start.saturating_add(page_size).min(total);
            let objects = state.objects[start.min(object_count)..end.min(object_count)].to_vec();
            let removal_start = start.saturating_sub(object_count).min(state.removals.len());
            let removal_end = end.saturating_sub(object_count).min(state.removals.len());
            let removals = state.removals[removal_start..removal_end].to_vec();
            state.offset = end;
            let complete = end >= total;
            Ok(InventoryPage {
                objects,
                removals,
                next_page_cursor: (!complete)
                    .then(|| inventory_page_cursor(&state))
                    .transpose()?,
                next_change_cursor: complete
                    .then(|| change_cursor(state.next_snapshot))
                    .transpose()?,
                progress: None,
            })
        })
    }

    fn stat<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
        Box::pin(async move { self.storage_object(object_id, true) })
    }

    fn versions_page(
        &self,
        request: StorageVersionRequest,
    ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
        Box::pin(async move {
            let current = self.storage_object(&request.object_id, true)?;
            let mut versions = vec![StorageObjectVersion {
                object_id: request.object_id.clone(),
                version_id: current
                    .provider_version
                    .clone()
                    .unwrap_or_else(|| "current".to_owned()),
                modified_at: current.modified_at,
                size_bytes: Some(current.size_bytes),
                hashes: current.hashes,
                is_current: true,
            }];
            let history_directory = self
                .history_root()
                .join(validate_object_id(&request.object_id)?);
            if history_directory.is_dir() {
                for path in walk_files(&history_directory).map_err(map_io_error)? {
                    let metadata = fs::metadata(&path).map_err(map_io_error)?;
                    versions.push(StorageObjectVersion {
                        object_id: request.object_id.clone(),
                        version_id: path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned(),
                        modified_at: metadata.modified().ok().map(DateTime::<Utc>::from),
                        size_bytes: Some(metadata.len()),
                        hashes: vec![sha256_file(&path)?],
                        is_current: false,
                    });
                }
            }
            versions[1..].sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
            let start = request
                .page_cursor
                .as_ref()
                .map(parse_page_cursor)
                .transpose()?
                .unwrap_or(0);
            if start > versions.len() {
                return Err(storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "version page cursor is outside the result set",
                ));
            }
            let end = (start + request.page_size.unwrap_or(500).clamp(1, 10_000) as usize)
                .min(versions.len());
            Ok(StorageVersionPage {
                versions: versions[start..end].to_vec(),
                next_page_cursor: (end < versions.len()).then(|| page_cursor(end)),
            })
        })
    }

    fn upload(
        &self,
        request: UploadRequest,
    ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
        Box::pin(async move {
            if !request.source_path.is_file() {
                return Err(storage_error(
                    MusicStorageErrorKind::NotFound,
                    format!(
                        "upload source is not a file: {}",
                        request.source_path.display()
                    ),
                ));
            }
            let normalized_source = normalized_path(&request.source_path)
                .map_err(|error| storage_error(MusicStorageErrorKind::Io, error.to_string()))?;
            if normalized_source.starts_with(&self.root) {
                return Err(storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "upload source must be outside the managed storage root",
                ));
            }
            if let Some(expected) = &request.expected_checksum {
                let actual = sha256_file(&request.source_path)?;
                if &actual != expected {
                    return Err(storage_error(
                        MusicStorageErrorKind::ChecksumMismatch,
                        "upload source checksum did not match the expected checksum",
                    ));
                }
            }
            if let Some(length) = request.content_length {
                if fs::metadata(&request.source_path)
                    .map_err(map_io_error)?
                    .len()
                    != length
                {
                    return Err(storage_error(
                        MusicStorageErrorKind::InvalidData,
                        "upload source length did not match content_length",
                    ));
                }
            }
            let destination = self.object_path(&request.object_path)?;
            if let Some(idempotency_key) = request.idempotency_key.as_deref() {
                self.upload_idempotently(&request, &destination, idempotency_key)
            } else {
                self.copy_to_destination(&request.source_path, &destination, true)?;
                self.storage_object(&request.object_path, true)
            }
        })
    }

    fn download(
        &self,
        request: DownloadRequest,
    ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
        Box::pin(async move {
            let source = self.object_path(&request.object_id)?;
            if !source.is_file() {
                return Err(storage_error(
                    MusicStorageErrorKind::NotFound,
                    format!("object not found: {}", request.object_id),
                ));
            }
            if request.destination_path.exists() && !request.overwrite {
                return Err(storage_error(
                    MusicStorageErrorKind::Permanent,
                    format!(
                        "download destination already exists: {}",
                        request.destination_path.display()
                    ),
                ));
            }
            if request.range.is_some() && request.expected_checksum.is_some() {
                return Err(storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "cannot validate a complete-file checksum for a ranged download",
                ));
            }
            let checksum = if request.range.is_none() {
                Some(sha256_file(&source)?)
            } else {
                None
            };
            if let (Some(expected), Some(actual)) = (&request.expected_checksum, &checksum) {
                if expected != actual {
                    return Err(storage_error(
                        MusicStorageErrorKind::ChecksumMismatch,
                        "stored object checksum did not match the expected checksum",
                    ));
                }
            }
            let bytes_written =
                copy_download_atomically(&source, &request.destination_path, request.range)?;
            let object = self.storage_object(&request.object_id, false)?;
            Ok(DownloadResult {
                object,
                destination_path: request.destination_path,
                bytes_written,
                checksum,
                checksum_verified: request.expected_checksum.is_some(),
            })
        })
    }
}

fn storage_error(kind: MusicStorageErrorKind, message: impl Into<String>) -> MusicStorageError {
    MusicStorageError::new(kind, message)
}

fn unavailable_error(root: &Path) -> MusicStorageError {
    storage_error(
        MusicStorageErrorKind::TemporarilyUnavailable,
        format!("local storage is unavailable: {}", root.display()),
    )
}

fn map_io_error(error: io::Error) -> MusicStorageError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => MusicStorageErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => MusicStorageErrorKind::PermissionDenied,
        _ => MusicStorageErrorKind::Io,
    };
    storage_error(kind, error.to_string())
}

fn validate_object_id(object_id: &str) -> Result<PathBuf, MusicStorageError> {
    if object_id.is_empty() {
        return Err(storage_error(
            MusicStorageErrorKind::InvalidData,
            "object path must not be empty",
        ));
    }
    let path = Path::new(object_id);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(storage_error(
            MusicStorageErrorKind::InvalidData,
            "object path must be a safe relative path",
        ));
    }
    if path.starts_with(HISTORY_DIRECTORY)
        || path.starts_with(OPERATIONS_DIRECTORY)
        || path.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with(TEMPORARY_PREFIX)
        })
    {
        return Err(storage_error(
            MusicStorageErrorKind::InvalidData,
            "object path uses a reserved local storage name",
        ));
    }
    Ok(path.to_path_buf())
}

fn path_to_object_id(path: &Path) -> Result<String, MusicStorageError> {
    let value = path
        .to_str()
        .ok_or_else(|| storage_error(MusicStorageErrorKind::InvalidData, "path is not UTF-8"))?;
    Ok(value.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn walk_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("directory unavailable: {}", root.display()),
        ));
    }
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn snapshot_versions(root: &Path) -> Result<BTreeMap<String, String>, LocalStorageError> {
    let mut versions = BTreeMap::new();
    for path in walk_files(root)? {
        let relative = path
            .strip_prefix(root)
            .expect("walk path must be under root");
        if is_provider_internal_path(relative) || !is_recognized_audio_path(&path) {
            continue;
        }
        let object_id = path_to_object_id(relative)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.message))?;
        versions.insert(object_id, provider_version(&fs::metadata(path)?)?);
    }
    Ok(versions)
}

fn is_provider_internal_path(path: &Path) -> bool {
    path.starts_with(HISTORY_DIRECTORY) || path.starts_with(OPERATIONS_DIRECTORY)
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalUploadOperation {
    version: u8,
    object_path: String,
    source_size: u64,
    source_hash: ContentHash,
    result: Option<StorageObject>,
}

fn read_local_operation(path: &Path) -> Result<Option<LocalUploadOperation>, MusicStorageError> {
    match fs::read(path) {
        Ok(bytes) => {
            let operation: LocalUploadOperation = serde_json::from_slice(&bytes).map_err(|_| {
                storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "local upload operation record is corrupt",
                )
            })?;
            if operation.version != 1 {
                return Err(storage_error(
                    MusicStorageErrorKind::InvalidData,
                    "unsupported local upload operation record version",
                ));
            }
            Ok(Some(operation))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(map_io_error(error)),
    }
}

fn write_local_operation(
    path: &Path,
    operation: &LocalUploadOperation,
) -> Result<(), MusicStorageError> {
    let parent = path.parent().expect("operation path has a parent");
    fs::create_dir_all(parent).map_err(map_io_error)?;
    let temporary = parent.join(format!("{TEMPORARY_PREFIX}{}", Uuid::new_v4()));
    let result = (|| {
        let bytes = serde_json::to_vec(operation).map_err(|error| {
            storage_error(MusicStorageErrorKind::InvalidData, error.to_string())
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(map_io_error)?;
        file.write_all(&bytes).map_err(map_io_error)?;
        file.sync_all().map_err(map_io_error)?;
        fs::rename(&temporary, path).map_err(map_io_error)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn provider_version(metadata: &fs::Metadata) -> io::Result<String> {
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(format!(
            "{}-{}-{}-{}-{}-{}-{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            modified.as_secs(),
            modified.subsec_nanos(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ));
    }
    #[cfg(not(unix))]
    Ok(format!(
        "{}-{}-{}",
        metadata.len(),
        modified.as_secs(),
        modified.subsec_nanos()
    ))
}

fn sha256_file(path: &Path) -> Result<ContentHash, MusicStorageError> {
    let file = File::open(path).map_err(map_io_error)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    loop {
        let read = reader.read(&mut buffer).map_err(map_io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    ContentHash::sha256(format!("{:x}", hasher.finalize()))
        .map_err(|error| storage_error(MusicStorageErrorKind::InvalidData, error.to_string()))
}

fn copy_file_with_sync(source: &Path, destination: &Path) -> Result<(), MusicStorageError> {
    let source = File::open(source).map_err(map_io_error)?;
    let mut reader = BufReader::new(source);
    let destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(map_io_error)?;
    let mut writer = BufWriter::new(destination);
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    loop {
        let read = reader.read(&mut buffer).map_err(map_io_error)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read]).map_err(map_io_error)?;
    }
    writer.flush().map_err(map_io_error)?;
    writer
        .into_inner()
        .map_err(|error| map_io_error(error.into_error()))?
        .sync_all()
        .map_err(map_io_error)
}

fn copy_download_atomically(
    source: &Path,
    destination: &Path,
    range: Option<ByteRange>,
) -> Result<u64, MusicStorageError> {
    let parent = destination.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(map_io_error)?;
    let temporary = parent.join(format!("{TEMPORARY_PREFIX}{}", Uuid::new_v4()));
    let result = (|| -> Result<u64, MusicStorageError> {
        let mut reader = BufReader::new(File::open(source).map_err(map_io_error)?);
        let source_length = reader.get_ref().metadata().map_err(map_io_error)?.len();
        let limit = match range {
            Some(range) => {
                if range.start > source_length
                    || range
                        .end_exclusive
                        .is_some_and(|end| end < range.start || end > source_length)
                {
                    return Err(storage_error(
                        MusicStorageErrorKind::InvalidData,
                        "download range is outside the object",
                    ));
                }
                reader
                    .seek(SeekFrom::Start(range.start))
                    .map_err(map_io_error)?;
                range.end_exclusive.map(|end| end - range.start)
            }
            None => None,
        };
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(map_io_error)?;
        let mut writer = BufWriter::new(file);
        let mut written = 0_u64;
        let mut buffer = [0_u8; COPY_BUFFER_SIZE];
        loop {
            let allowed = limit
                .map(|limit| limit.saturating_sub(written) as usize)
                .unwrap_or(buffer.len())
                .min(buffer.len());
            if allowed == 0 {
                break;
            }
            let read = reader.read(&mut buffer[..allowed]).map_err(map_io_error)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read]).map_err(map_io_error)?;
            written += read as u64;
        }
        writer.flush().map_err(map_io_error)?;
        writer
            .into_inner()
            .map_err(|error| map_io_error(error.into_error()))?
            .sync_all()
            .map_err(map_io_error)?;
        fs::rename(&temporary, destination).map_err(map_io_error)?;
        sync_directory(parent)?;
        Ok(written)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn media_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "flac" => Some("audio/flac"),
        "ogg" => Some("audio/ogg"),
        "m4a" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "aif" | "aiff" => Some("audio/aiff"),
        "opus" => Some("audio/opus"),
        "wma" => Some("audio/x-ms-wma"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LocalCursorMode {
    Full,
    Incremental,
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalChangeCursor {
    version: u8,
    snapshot: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalInventoryPageCursor {
    version: u8,
    mode: LocalCursorMode,
    prefix: Option<String>,
    include_checksums: bool,
    objects: Vec<StorageObject>,
    removals: Vec<StorageObjectRemoval>,
    offset: usize,
    next_snapshot: BTreeMap<String, String>,
}

impl LocalInventoryPageCursor {
    fn validate_request(&self, request: &InventoryRequest) -> Result<(), MusicStorageError> {
        let mode = match request.mode {
            InventoryMode::Full => LocalCursorMode::Full,
            InventoryMode::Incremental { .. } => LocalCursorMode::Incremental,
        };
        if self.version != LOCAL_PAGE_CURSOR_VERSION
            || self.mode != mode
            || self.prefix != request.prefix
            || self.include_checksums != request.include_checksums
        {
            return Err(storage_error(
                MusicStorageErrorKind::InvalidData,
                "local inventory page cursor does not match the request",
            ));
        }
        Ok(())
    }
}

fn change_cursor(snapshot: BTreeMap<String, String>) -> Result<SyncCursor, MusicStorageError> {
    serde_json::to_string(&LocalChangeCursor {
        version: LOCAL_CHANGE_CURSOR_VERSION,
        snapshot,
    })
    .map(SyncCursor::new)
    .map_err(|error| storage_error(MusicStorageErrorKind::InvalidData, error.to_string()))
}

fn parse_change_cursor(cursor: &SyncCursor) -> Result<LocalChangeCursor, MusicStorageError> {
    let parsed: LocalChangeCursor = serde_json::from_str(&cursor.value).map_err(|_| {
        storage_error(
            MusicStorageErrorKind::InvalidData,
            "invalid local change cursor",
        )
    })?;
    if parsed.version != LOCAL_CHANGE_CURSOR_VERSION {
        return Err(storage_error(
            MusicStorageErrorKind::InvalidData,
            "unsupported local change cursor version",
        ));
    }
    Ok(parsed)
}

fn inventory_page_cursor(
    state: &LocalInventoryPageCursor,
) -> Result<SyncCursor, MusicStorageError> {
    serde_json::to_string(state)
        .map(SyncCursor::new)
        .map_err(|error| storage_error(MusicStorageErrorKind::InvalidData, error.to_string()))
}

fn parse_inventory_page_cursor(
    cursor: &SyncCursor,
) -> Result<LocalInventoryPageCursor, MusicStorageError> {
    serde_json::from_str(&cursor.value).map_err(|_| {
        storage_error(
            MusicStorageErrorKind::InvalidData,
            "invalid local inventory page cursor",
        )
    })
}

fn page_cursor(offset: usize) -> SyncCursor {
    SyncCursor {
        value: format!("offset:{offset}"),
        updated_at: Utc::now(),
    }
}

fn parse_page_cursor(cursor: &SyncCursor) -> Result<usize, MusicStorageError> {
    cursor
        .value
        .strip_prefix("offset:")
        .ok_or_else(|| {
            storage_error(
                MusicStorageErrorKind::InvalidData,
                "invalid local inventory page cursor",
            )
        })?
        .parse()
        .map_err(|_| {
            storage_error(
                MusicStorageErrorKind::InvalidData,
                "invalid local inventory page cursor",
            )
        })
}

fn normalized_path(path: &Path) -> Result<PathBuf, LocalStorageError> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn is_effectively_writable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.permissions().readonly() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o222 == 0 {
            return false;
        }
    }
    true
}

fn sync_directory(path: &Path) -> Result<(), MusicStorageError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(map_io_error)?;
    Ok(())
}

fn map_local_error(error: LocalStorageError) -> MusicStorageError {
    match error {
        LocalStorageError::Io(error) => map_io_error(error),
        other => storage_error(MusicStorageErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{
        DownloadRequest, InventoryMode, InventoryRequest, MusicAssetStore, RetentionPolicy,
        StorageVersionRequest, UploadRequest,
    };
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn storage(directory: &TempDir) -> LocalMusicStorage {
        LocalMusicStorage::new(Uuid::new_v4(), directory.path()).unwrap()
    }

    fn inventory_request(checksums: bool) -> InventoryRequest {
        InventoryRequest {
            mode: InventoryMode::Full,
            page_cursor: None,
            page_size: None,
            prefix: None,
            include_checksums: checksums,
        }
    }

    #[test]
    fn classifies_supported_recognized_and_non_audio_extensions() {
        assert_eq!(
            classify_audio_extension("MP3"),
            AudioClassification::Supported
        );
        assert_eq!(
            classify_audio_extension("aiff"),
            AudioClassification::RecognizedUnsupported
        );
        assert_eq!(
            classify_audio_extension("pdf"),
            AudioClassification::NotAudio
        );
    }

    #[tokio::test]
    async fn recursively_inventories_audio_with_optional_sha256() {
        let directory = TempDir::new().unwrap();
        fs::create_dir_all(directory.path().join("artist")).unwrap();
        fs::write(directory.path().join("artist/track.mp3"), b"music").unwrap();
        fs::write(directory.path().join("artist/notes.txt"), b"ignore").unwrap();
        fs::write(directory.path().join("recognized.aiff"), b"music").unwrap();
        let store = storage(&directory);

        let page = store.inventory_page(inventory_request(true)).await.unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].object_id, "artist/track.mp3");
        assert!(page
            .objects
            .iter()
            .all(|object| object.canonical_hash().is_some()));
    }

    #[tokio::test]
    async fn upload_is_atomic_and_keeps_replaced_backup_history() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = staging.path().join("track.flac");
        fs::write(&source, b"first").unwrap();
        let store = storage(&root);
        let request = |source_path| UploadRequest {
            source_path,
            object_path: "Artist/Track [abc].flac".to_owned(),
            content_type: None,
            content_length: None,
            expected_checksum: None,
            metadata: BTreeMap::new(),
            idempotency_key: None,
        };
        store.upload(request(source.clone())).await.unwrap();
        fs::write(&source, b"second").unwrap();
        store.upload(request(source)).await.unwrap();

        assert_eq!(
            fs::read(root.path().join("Artist/Track [abc].flac")).unwrap(),
            b"second"
        );
        let versions = store
            .versions_page(StorageVersionRequest {
                object_id: "Artist/Track [abc].flac".to_owned(),
                page_cursor: None,
                page_size: None,
            })
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 2);
        assert!(versions.versions[0].is_current);
        assert!(!versions.versions[1].is_current);
    }

    #[tokio::test]
    async fn idempotency_key_deduplicates_retries_without_creating_history() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = staging.path().join("track.mp3");
        fs::write(&source, b"same upload").unwrap();
        let request = || UploadRequest {
            source_path: source.clone(),
            object_path: "track.mp3".to_owned(),
            content_type: Some("audio/mpeg".to_owned()),
            content_length: Some(11),
            expected_checksum: None,
            metadata: BTreeMap::new(),
            idempotency_key: Some("operation-1".to_owned()),
        };

        let first = storage(&root).upload(request()).await.unwrap();
        let second = storage(&root).upload(request()).await.unwrap();

        assert_eq!(first, second);
        let versions = storage(&root)
            .versions_page(StorageVersionRequest {
                object_id: "track.mp3".to_owned(),
                page_cursor: None,
                page_size: None,
            })
            .await
            .unwrap();
        assert_eq!(versions.versions.len(), 1);
        assert!(root.path().join(OPERATIONS_DIRECTORY).is_dir());
        assert_eq!(
            storage(&root)
                .inventory_page(inventory_request(false))
                .await
                .unwrap()
                .objects
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn download_verifies_checksum_and_honors_ranges() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("track.wav"), b"abcdef").unwrap();
        let destination = TempDir::new().unwrap();
        let store = storage(&root);
        let result = store
            .download(DownloadRequest {
                object_id: "track.wav".to_owned(),
                destination_path: destination.path().join("range.wav"),
                range: Some(ByteRange {
                    start: 1,
                    end_exclusive: Some(4),
                }),
                expected_checksum: None,
                overwrite: false,
            })
            .await
            .unwrap();
        assert_eq!(result.bytes_written, 3);
        assert_eq!(fs::read(result.destination_path).unwrap(), b"bcd");
    }

    #[test]
    fn watcher_reports_external_rename_as_remove_and_create() {
        let directory = TempDir::new().unwrap();
        fs::write(directory.path().join("old.mp3"), b"music").unwrap();
        let store = storage(&directory);
        let mut watcher = store.watch_changes().unwrap();
        fs::rename(
            directory.path().join("old.mp3"),
            directory.path().join("new.mp3"),
        )
        .unwrap();
        let changes = watcher.poll_changes().unwrap();
        assert!(
            changes
                .iter()
                .any(|change| change.kind == LocalChangeKind::Removed
                    && change.object_id == "old.mp3")
        );
        assert!(
            changes
                .iter()
                .any(|change| change.kind == LocalChangeKind::Created
                    && change.object_id == "new.mp3")
        );
    }

    #[tokio::test]
    async fn unavailable_root_is_reported_without_creating_it() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("ejected-drive");
        let store = LocalMusicStorage::new(Uuid::new_v4(), &root).unwrap();
        assert_eq!(
            store.health().await.unwrap().status,
            HealthStatus::Unavailable
        );
        assert!(
            matches!(store.inventory_page(inventory_request(false)).await, Err(error) if error.kind == MusicStorageErrorKind::TemporarilyUnavailable)
        );
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn retention_candidates_preserve_recent_versions() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = staging.path().join("track.mp3");
        let store = storage(&root);
        for contents in [b"one".as_slice(), b"two", b"three", b"four"] {
            fs::write(&source, contents).unwrap();
            store
                .upload(UploadRequest {
                    source_path: source.clone(),
                    object_path: "track.mp3".to_owned(),
                    content_type: None,
                    content_length: None,
                    expected_checksum: None,
                    metadata: BTreeMap::new(),
                    idempotency_key: None,
                })
                .await
                .unwrap();
        }
        let candidates = store
            .retention_candidates(RetentionPolicy {
                retention_days: 0,
                minimum_versions: 1,
                require_cleanup_confirmation: true,
            })
            .unwrap();
        assert_eq!(candidates.len(), 2);
    }

    #[tokio::test]
    async fn empty_and_large_inventories_are_stable_across_pages() {
        let root = TempDir::new().unwrap();
        let store = storage(&root);
        let empty = store
            .inventory_page(inventory_request(false))
            .await
            .unwrap();
        assert!(empty.objects.is_empty());
        let initial_cursor = empty.next_change_cursor.unwrap();

        for index in 0..1_025 {
            fs::write(root.path().join(format!("track-{index:04}.mp3")), b"x").unwrap();
        }
        let mut request = inventory_request(false);
        request.page_size = Some(97);
        let mut ids = Vec::new();
        loop {
            let page = store.inventory_page(request.clone()).await.unwrap();
            ids.extend(page.objects.into_iter().map(|object| object.object_id));
            let Some(cursor) = page.next_page_cursor else {
                assert!(page.next_change_cursor.is_some());
                break;
            };
            request.page_cursor = Some(cursor);
        }
        assert_eq!(ids.len(), 1_025);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

        let incremental = store
            .inventory_page(InventoryRequest {
                mode: InventoryMode::Incremental {
                    change_cursor: initial_cursor,
                },
                page_cursor: None,
                page_size: Some(2_000),
                prefix: None,
                include_checksums: false,
            })
            .await
            .unwrap();
        assert_eq!(incremental.objects.len(), 1_025);
    }

    #[tokio::test]
    async fn inventory_preserves_duplicate_names_and_same_content_at_distinct_paths() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("a")).unwrap();
        fs::create_dir_all(root.path().join("b")).unwrap();
        fs::write(root.path().join("a/same.mp3"), b"identical").unwrap();
        fs::write(root.path().join("b/same.mp3"), b"identical").unwrap();
        let page = storage(&root)
            .inventory_page(inventory_request(true))
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_ne!(page.objects[0].object_id, page.objects[1].object_id);
        assert_eq!(page.objects[0].hashes, page.objects[1].hashes);
    }

    #[tokio::test]
    async fn incremental_inventory_reports_modifications_and_removals_once() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("changed.mp3"), b"old").unwrap();
        fs::write(root.path().join("removed.mp3"), b"gone").unwrap();
        let store = storage(&root);
        let full = store
            .inventory_page(inventory_request(false))
            .await
            .unwrap();
        let cursor = full.next_change_cursor.unwrap();
        fs::write(root.path().join("changed.mp3"), b"new-and-longer").unwrap();
        fs::remove_file(root.path().join("removed.mp3")).unwrap();

        let page = store
            .inventory_page(InventoryRequest {
                mode: InventoryMode::Incremental {
                    change_cursor: cursor,
                },
                page_cursor: None,
                page_size: Some(1),
                prefix: None,
                include_checksums: true,
            })
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].object_id, "changed.mp3");
        assert!(page.next_change_cursor.is_none());
        let final_page = store
            .inventory_page(InventoryRequest {
                mode: InventoryMode::Incremental {
                    change_cursor: SyncCursor::new("ignored-because-page-state-is-frozen"),
                },
                page_cursor: page.next_page_cursor,
                page_size: Some(1),
                prefix: None,
                include_checksums: true,
            })
            .await
            .unwrap();
        assert_eq!(final_page.removals[0].object_id, "removed.mp3");
        assert!(final_page.next_change_cursor.is_some());
    }

    #[test]
    fn construction_cleans_interrupted_copy_temporary_files() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/.rec_bag-tmp-orphan"), b"partial").unwrap();
        fs::write(root.path().join("nested/track.mp3"), b"complete").unwrap();
        let store = storage(&root);
        assert!(!root.path().join("nested/.rec_bag-tmp-orphan").exists());
        assert!(store.playback_path("nested/track.mp3").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_root_degrades_health_and_rejects_upload() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = staging.path().join("source.mp3");
        fs::write(&source, b"music").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let store = storage(&root);
        let health = store.health().await.unwrap();
        assert_eq!(health.status, HealthStatus::Degraded);
        assert!(!health.capabilities.can_upload);
        let error = store
            .upload(UploadRequest {
                source_path: source,
                object_path: "track.mp3".to_owned(),
                content_type: None,
                content_length: None,
                expected_checksum: None,
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind, MusicStorageErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn concurrent_atomic_replacements_never_publish_partial_content() {
        use std::sync::Arc;
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let first = staging.path().join("first.mp3");
        let second = staging.path().join("second.mp3");
        fs::write(&first, vec![b'a'; 256 * 1024]).unwrap();
        fs::write(&second, vec![b'b'; 256 * 1024]).unwrap();
        let store = Arc::new(storage(&root));
        let handles = [first, second].map(|source_path| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(store.upload(UploadRequest {
                        source_path,
                        object_path: "same.mp3".to_owned(),
                        content_type: None,
                        content_length: None,
                        expected_checksum: None,
                        metadata: BTreeMap::new(),
                        idempotency_key: None,
                    }))
                    .unwrap();
            })
        });
        for handle in handles {
            handle.join().unwrap();
        }
        let bytes = fs::read(root.path().join("same.mp3")).unwrap();
        assert!(bytes.iter().all(|byte| *byte == b'a') || bytes.iter().all(|byte| *byte == b'b'));
        assert!(!walk_files(root.path()).unwrap().iter().any(|path| path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(TEMPORARY_PREFIX)));
    }

    #[tokio::test]
    async fn satisfies_shared_music_asset_store_contract() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = staging.path().join("contract.mp3");
        fs::write(&source, b"contract-bytes").unwrap();
        let store: Box<dyn MusicAssetStore> = Box::new(storage(&root));
        assert!(store.health().await.unwrap().capabilities.can_inventory);
        let uploaded = store
            .upload(UploadRequest {
                source_path: source,
                object_path: "Artist/contract.mp3".to_owned(),
                content_type: Some("audio/mpeg".to_owned()),
                content_length: Some(14),
                expected_checksum: None,
                metadata: BTreeMap::new(),
                idempotency_key: Some("contract-upload".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(store.stat(&uploaded.object_id).await.unwrap(), uploaded);
        let inventory = store.inventory_page(inventory_request(true)).await.unwrap();
        assert_eq!(inventory.objects, vec![uploaded.clone()]);
        let destination = staging.path().join("download.mp3");
        let downloaded = store
            .download(DownloadRequest {
                object_id: uploaded.object_id.clone(),
                destination_path: destination.clone(),
                range: None,
                expected_checksum: uploaded.canonical_hash().cloned(),
                overwrite: false,
            })
            .await
            .unwrap();
        assert!(downloaded.checksum_verified);
        assert_eq!(fs::read(destination).unwrap(), b"contract-bytes");
    }
}
