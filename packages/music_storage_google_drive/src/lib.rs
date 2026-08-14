//! Google Drive implementation of rec_bag's provider-neutral music-storage API.
//!
//! The adapter intentionally accepts a token provider and transport rather than
//! coupling to a particular OAuth broker or HTTP client. It targets **My Drive**
//! only: every listing uses `corpora=user`, `spaces=drive`, and never enables
//! shared-drive support.

mod cache;
mod http;

pub use cache::CacheConfig;
pub use http::{
    DriveHttpRequest, DriveHttpResponse, DriveHttpTransport, DriveTransportError,
    GoogleAccessToken, GoogleTokenError, GoogleTokenProvider,
};

use cache::PlaybackCache;
use chrono::{DateTime, Utc};
use md5::Md5;
use rec_core::{
    BoxFuture, ContentHash, DownloadRequest, DownloadResult, HashAlgorithm, HealthStatus,
    InventoryMode, InventoryPage, InventoryRequest, MusicAssetStore, MusicStorageError,
    MusicStorageErrorKind, StorageCapabilities, StorageHealth, StorageObject, StorageObjectRemoval,
    StorageObjectVersion, StorageVersionPage, StorageVersionRequest, SyncCursor, UploadRequest,
};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const DRIVE_V3: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_V3: &str = "https://www.googleapis.com/upload/drive/v3";
const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const DEFAULT_UPLOAD_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Selects the dedicated folder in a user's My Drive that rec_bag may manage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleDriveFolder {
    /// Stable Drive folder ID. A display path is never used as identity.
    pub id: String,
    pub display_name: Option<String>,
}

impl GoogleDriveFolder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
        }
    }
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }
}

/// Bounded exponential retry policy for Drive transient failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
        }
    }
}

/// Configuration for [`GoogleDriveMusicStore`]. No credentials are retained here.
#[derive(Debug, Clone)]
pub struct GoogleDriveStoreConfig {
    pub storage_id: Uuid,
    pub folder: GoogleDriveFolder,
    pub cache: Option<CacheConfig>,
    pub retry: RetryPolicy,
    pub api_base_url: String,
    pub upload_base_url: String,
    pub upload_chunk_size: usize,
}

impl GoogleDriveStoreConfig {
    pub fn new(storage_id: Uuid, folder: GoogleDriveFolder) -> Self {
        Self {
            storage_id,
            folder,
            cache: None,
            retry: RetryPolicy::default(),
            api_base_url: DRIVE_V3.to_owned(),
            upload_base_url: UPLOAD_V3.to_owned(),
            upload_chunk_size: DEFAULT_UPLOAD_CHUNK_SIZE,
        }
    }
}

/// A Drive-backed music store. `T` and `P` are usually application services.
pub struct GoogleDriveMusicStore<T, P> {
    config: GoogleDriveStoreConfig,
    transport: T,
    tokens: P,
    cache: Option<PlaybackCache>,
}

impl<T, P> GoogleDriveMusicStore<T, P> {
    pub fn new(
        config: GoogleDriveStoreConfig,
        transport: T,
        tokens: P,
    ) -> Result<Self, MusicStorageError> {
        if config.folder.id.trim().is_empty() {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "Google Drive folder ID cannot be empty",
            ));
        }
        if config.upload_chunk_size == 0 {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "upload chunk size cannot be zero",
            ));
        }
        let cache = config.cache.clone().map(PlaybackCache::new).transpose()?;
        Ok(Self {
            config,
            transport,
            tokens,
            cache,
        })
    }

    pub fn folder(&self) -> &GoogleDriveFolder {
        &self.config.folder
    }

    /// Returns an existing verified playback-cache entry, updating its LRU time.
    pub fn cached_playback_path(
        &self,
        object_id: &str,
    ) -> Result<Option<std::path::PathBuf>, MusicStorageError> {
        self.cache
            .as_ref()
            .map_or(Ok(None), |cache| cache.cached_path(object_id))
    }

    /// Pins an already cached object so LRU eviction will not remove it.
    pub fn set_offline_pinned(
        &self,
        object_id: &str,
        pinned: bool,
    ) -> Result<(), MusicStorageError> {
        self.cache
            .as_ref()
            .ok_or_else(|| {
                MusicStorageError::new(
                    MusicStorageErrorKind::NotFound,
                    "playback cache is disabled",
                )
            })?
            .set_pinned(object_id, pinned)
    }

    /// Applies the configured LRU size limit immediately.
    pub fn evict_playback_cache(&self) -> Result<(), MusicStorageError> {
        if let Some(cache) = &self.cache {
            cache.evict()?;
        }
        Ok(())
    }
}

impl<T: DriveHttpTransport, P: GoogleTokenProvider> GoogleDriveMusicStore<T, P> {
    async fn execute(
        &self,
        request: DriveHttpRequest,
    ) -> Result<DriveHttpResponse, MusicStorageError> {
        let mut delay = self.config.retry.initial_backoff;
        for attempt in 0..self.config.retry.max_attempts.max(1) {
            let token = self.tokens.access_token().await.map_err(|error| {
                MusicStorageError::new(
                    if error.retryable {
                        MusicStorageErrorKind::TemporarilyUnavailable
                    } else {
                        MusicStorageErrorKind::AuthenticationRequired
                    },
                    error.message,
                )
            })?;
            let mut request = request.clone();
            request
                .headers
                .insert("Authorization".to_owned(), format!("Bearer {}", token.0));
            match self.transport.execute(request).await {
                Ok(response)
                    if !(200..300).contains(&response.status) && response.status != 308 =>
                {
                    let error = drive_error(&response);
                    let can_retry = attempt + 1 < self.config.retry.max_attempts.max(1);
                    if response.status == 401 && can_retry {
                        self.tokens
                            .invalidate_token(&token)
                            .await
                            .map_err(token_error)?;
                        continue;
                    }
                    if error.is_retryable() && can_retry {
                        futures_timer::Delay::new(response_retry_delay(&response).unwrap_or(delay))
                            .await;
                        delay = delay.saturating_mul(2).min(self.config.retry.max_backoff);
                        continue;
                    }
                    return Err(error);
                }
                Ok(response) => return Ok(response),
                Err(error) => {
                    if error.retryable && attempt + 1 < self.config.retry.max_attempts.max(1) {
                        futures_timer::Delay::new(delay).await;
                        delay = delay.saturating_mul(2).min(self.config.retry.max_backoff);
                        continue;
                    }
                    return Err(MusicStorageError::new(
                        if error.retryable {
                            MusicStorageErrorKind::TemporarilyUnavailable
                        } else {
                            MusicStorageErrorKind::Provider
                        },
                        error.message,
                    ));
                }
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn file(&self, object_id: &str) -> Result<DriveFile, MusicStorageError> {
        let url = format!(
            "{}/files/{}?fields={}",
            self.config.api_base_url,
            url_encode(object_id),
            url_encode(FILE_FIELDS)
        );
        parse_json(self.execute(DriveHttpRequest::new("GET", url)).await?)
    }

    async fn list_parent(
        &self,
        parent_id: &str,
        page_token: Option<&str>,
        page_size: u32,
    ) -> Result<DriveFileList, MusicStorageError> {
        let mut url = format!("{}/files?corpora=user&spaces=drive&supportsAllDrives=false&includeItemsFromAllDrives=false&pageSize={}&q={}&fields={}",
            self.config.api_base_url, page_size, url_encode(&format!("'{}' in parents and trashed = false", parent_id)), url_encode("nextPageToken,files(id,name,mimeType,size,modifiedTime,version,etag,md5Checksum,sha1Checksum,sha256Checksum,trashed,appProperties,capabilities(canDownload),parents)"));
        if let Some(token) = page_token {
            url.push_str("&pageToken=");
            url.push_str(&url_encode(token));
        }
        parse_json(self.execute(DriveHttpRequest::new("GET", url)).await?)
    }

    async fn change_page(
        &self,
        cursor: &str,
        page_token: Option<&str>,
        page_size: u32,
    ) -> Result<DriveChangeList, MusicStorageError> {
        let effective_page_token = page_token.unwrap_or(cursor);
        let url = format!("{}/changes?spaces=drive&includeItemsFromAllDrives=false&supportsAllDrives=false&pageSize={}&pageToken={}&fields={}",
            self.config.api_base_url, page_size, url_encode(effective_page_token), url_encode("nextPageToken,newStartPageToken,changes(fileId,removed,time,file(id,name,mimeType,size,modifiedTime,version,etag,md5Checksum,sha1Checksum,sha256Checksum,trashed,appProperties,capabilities(canDownload),parents))"));
        parse_json(self.execute(DriveHttpRequest::new("GET", url)).await?)
    }

    async fn start_page_token(&self) -> Result<String, MusicStorageError> {
        #[derive(Deserialize)]
        struct Token {
            #[serde(rename = "startPageToken")]
            token: String,
        }
        let url = format!(
            "{}/changes/startPageToken?spaces=drive&supportsAllDrives=false&fields=startPageToken",
            self.config.api_base_url
        );
        Ok(parse_json::<Token>(self.execute(DriveHttpRequest::new("GET", url)).await?)?.token)
    }

    async fn inventory_full(
        &self,
        request: InventoryRequest,
    ) -> Result<InventoryPage, MusicStorageError> {
        let mut state = request
            .page_cursor
            .map(|cursor| {
                serde_json::from_str::<FullInventoryCursor>(&cursor.value).map_err(|_| {
                    MusicStorageError::new(
                        MusicStorageErrorKind::InvalidData,
                        "invalid Google Drive inventory cursor",
                    )
                })
            })
            .transpose()?
            .unwrap_or_else(|| FullInventoryCursor::new(&self.config.folder.id));
        let wanted = request.page_size.unwrap_or(100).clamp(1, 1000) as usize;
        let mut objects = Vec::new();
        while objects.len() < wanted {
            if let Some(file) = state.pending.pop_front() {
                let file: DriveFile = file.into();
                if state.seen_objects.insert(file.id.clone())
                    && matches_prefix(&file, request.prefix.as_deref())
                {
                    objects.push(self.storage_object(file));
                }
                continue;
            }
            if state.active_parent.is_none() {
                state.active_parent = state.parents.pop_front();
                if state.active_parent.is_none() {
                    break;
                }
            }
            let parent = state.active_parent.clone().expect("set above");
            let page = self
                .list_parent(&parent, state.api_page_token.as_deref(), wanted as u32)
                .await?;
            state.api_page_token = page.next_page_token;
            for file in page.files {
                if file.mime_type.as_deref() == Some(FOLDER_MIME_TYPE) {
                    if state.seen_folders.insert(file.id.clone()) {
                        state.parents.push_back(file.id);
                    }
                } else {
                    state.pending.push_back(file.into());
                }
            }
            if state.api_page_token.is_none() {
                state.active_parent = None;
            }
        }
        let complete =
            state.pending.is_empty() && state.parents.is_empty() && state.active_parent.is_none();
        let next_page_cursor = (!complete)
            .then(|| SyncCursor::new(serde_json::to_string(&state).expect("cursor serializes")));
        let next_change_cursor = if complete {
            Some(SyncCursor::new(self.start_page_token().await?))
        } else {
            None
        };
        Ok(InventoryPage {
            objects,
            removals: Vec::new(),
            next_page_cursor,
            next_change_cursor,
            progress: None,
        })
    }

    async fn inventory_incremental(
        &self,
        change_cursor: String,
        request: InventoryRequest,
    ) -> Result<InventoryPage, MusicStorageError> {
        let page = self
            .change_page(
                &change_cursor,
                request
                    .page_cursor
                    .as_ref()
                    .map(|cursor| cursor.value.as_str()),
                request.page_size.unwrap_or(100).clamp(1, 1000),
            )
            .await?;
        let mut objects = Vec::new();
        let mut removals = Vec::new();
        for change in page.changes {
            if change.removed || change.file.as_ref().is_some_and(|file| file.trashed) {
                removals.push(StorageObjectRemoval {
                    object_id: change.file_id,
                    provider_version: change.file.and_then(|file| file.version),
                    removed_at: change.time.unwrap_or_else(Utc::now),
                });
            } else if let Some(file) = change.file {
                if self.belongs_to_selected_folder(&file).await? {
                    if matches_prefix(&file, request.prefix.as_deref()) {
                        objects.push(self.storage_object(file));
                    }
                } else {
                    removals.push(StorageObjectRemoval {
                        object_id: change.file_id,
                        provider_version: file.version.clone(),
                        removed_at: change.time.unwrap_or_else(Utc::now),
                    });
                }
            }
        }
        Ok(InventoryPage {
            objects,
            removals,
            next_page_cursor: page.next_page_token.map(SyncCursor::new),
            next_change_cursor: page.new_start_page_token.map(SyncCursor::new),
            progress: None,
        })
    }

    /// Resolves parent links so incremental changes remain confined to the selected
    /// folder even when a file sits several levels below it.
    async fn belongs_to_selected_folder(
        &self,
        file: &DriveFile,
    ) -> Result<bool, MusicStorageError> {
        let mut unchecked: VecDeque<String> = file.parents.iter().cloned().collect();
        let mut visited = BTreeSet::new();
        while let Some(parent_id) = unchecked.pop_front() {
            if parent_id == self.config.folder.id {
                return Ok(true);
            }
            if !visited.insert(parent_id.clone()) {
                continue;
            }
            let parent = self.file(&parent_id).await?;
            if !parent.trashed {
                unchecked.extend(parent.parents);
            }
        }
        Ok(false)
    }

    fn storage_object(&self, file: DriveFile) -> StorageObject {
        let mut hashes = Vec::new();
        for (algorithm, value) in [
            (HashAlgorithm::Md5, file.md5_checksum.as_deref()),
            (HashAlgorithm::Sha1, file.sha1_checksum.as_deref()),
            (HashAlgorithm::Sha256, file.sha256_checksum.as_deref()),
        ] {
            if let Some(value) = value {
                if let Ok(hash) = ContentHash::new(algorithm, value) {
                    hashes.push(hash);
                }
            }
        }
        let embedded_asset_id = file
            .app_properties
            .get("rec_bag_asset_id")
            .or_else(|| file.app_properties.get("asset_id"))
            .and_then(|value| Uuid::parse_str(value).ok());
        let display_path = self
            .config
            .folder
            .display_name
            .as_ref()
            .map(|folder| format!("{folder}/{}", file.name));
        StorageObject {
            storage_id: self.config.storage_id,
            object_id: file.id,
            name: file.name,
            display_path,
            size_bytes: file.size.unwrap_or(0),
            media_type: file.mime_type,
            modified_at: file.modified_time,
            provider_version: file.version.or(file.etag),
            hashes,
            embedded_asset_id,
            is_trashed: file.trashed,
        }
    }

    async fn upload_inner(
        &self,
        request: UploadRequest,
    ) -> Result<StorageObject, MusicStorageError> {
        let source_size = fs::metadata(&request.source_path).map_err(io_error)?.len();
        let expected_source_hash = request
            .expected_checksum
            .as_ref()
            .map(|expected| hash_file_as(&request.source_path, expected.algorithm()))
            .transpose()?;
        if let (Some(expected), Some(actual)) = (
            request.expected_checksum.as_ref(),
            expected_source_hash.as_ref(),
        ) {
            if expected != actual {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::ChecksumMismatch,
                    "source file does not match expected upload checksum",
                ));
            }
        }
        if let Some(length) = request.content_length {
            if length != source_size {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::InvalidData,
                    "upload content length does not match source file",
                ));
            }
        }
        let name = file_name_from_path(&request.object_path)?;
        let metadata = serde_json::json!({
            "name": name,
            "parents": [self.config.folder.id],
            "mimeType": request.content_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
            "appProperties": app_properties(&request.metadata),
        });
        let init = DriveHttpRequest::new(
            "POST",
            format!(
                "{}/files?uploadType=resumable&supportsAllDrives=false",
                self.config.upload_base_url
            ),
        )
        .header("Content-Type", "application/json")
        .header("X-Upload-Content-Length", source_size.to_string())
        .header(
            "X-Upload-Content-Type",
            request
                .content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
        )
        .body(serde_json::to_vec(&metadata).expect("JSON metadata serializes"));
        let init = self.execute(init).await?;
        let session = init
            .header("Location")
            .ok_or_else(|| {
                MusicStorageError::new(
                    MusicStorageErrorKind::Provider,
                    "resumable upload response omitted Location",
                )
            })?
            .to_owned();
        let mut file = File::open(&request.source_path).map_err(io_error)?;
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; self.config.upload_chunk_size];
        loop {
            let read = file.read(&mut buffer).map_err(io_error)?;
            if read == 0 && source_size != 0 {
                break;
            }
            let end = offset + read as u64;
            let content_range = if read == 0 {
                format!("bytes */{source_size}")
            } else {
                format!("bytes {offset}-{}/{}", end - 1, source_size)
            };
            let response = self
                .execute(
                    DriveHttpRequest::new("PUT", session.clone())
                        .header("Content-Length", read.to_string())
                        .header("Content-Range", content_range)
                        .body(buffer[..read].to_vec()),
                )
                .await?;
            if response.status == 308 {
                let acknowledged = response
                    .header("Range")
                    .and_then(range_end)
                    .map(|end| end + 1)
                    .unwrap_or(0);
                if acknowledged < offset || acknowledged > source_size {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::Provider,
                        "Drive returned invalid resumable upload range",
                    ));
                }
                offset = acknowledged;
                file = File::open(&request.source_path).map_err(io_error)?;
                skip_file(&mut file, offset)?;
                continue;
            }
            let uploaded: DriveFile = parse_json(response)?;
            let object = self.storage_object(uploaded);
            for remote_hash in &object.hashes {
                let source_hash = hash_file_as(&request.source_path, remote_hash.algorithm())?;
                if &source_hash != remote_hash {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::ChecksumMismatch,
                        "Drive reported a hash that does not match the uploaded source",
                    ));
                }
            }
            return Ok(object);
        }
        Err(MusicStorageError::new(
            MusicStorageErrorKind::Provider,
            "resumable upload ended before Drive confirmed completion",
        ))
    }

    async fn download_inner(
        &self,
        request: DownloadRequest,
    ) -> Result<DownloadResult, MusicStorageError> {
        if request.destination_path.exists() && !request.overwrite {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::Io,
                "download destination already exists",
            ));
        }
        if let Some(range) = request.range {
            if range.end_exclusive.is_some_and(|end| end <= range.start) {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::InvalidData,
                    "download range end must be greater than its start",
                ));
            }
            if request.expected_checksum.is_some() {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::InvalidData,
                    "cannot validate a complete-file checksum for a ranged download",
                ));
            }
        }
        let object_file = self.file(&request.object_id).await?;
        if object_file.trashed {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::NotFound,
                "Drive file is trashed",
            ));
        }
        if !object_file.capabilities.can_download {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::DownloadProhibited,
                "Drive does not permit downloading this file",
            ));
        }
        let object = self.storage_object(object_file);
        let mut http = DriveHttpRequest::new(
            "GET",
            format!(
                "{}/files/{}?alt=media&supportsAllDrives=false",
                self.config.api_base_url,
                url_encode(&request.object_id)
            ),
        );
        if let Some(range) = request.range {
            http = http.header(
                "Range",
                format!(
                    "bytes={}-{}",
                    range.start,
                    range
                        .end_exclusive
                        .map(|end| end.saturating_sub(1).to_string())
                        .unwrap_or_default()
                ),
            );
        }
        let response = self.execute(http).await?;
        if request.range.is_some() && response.status != 206 {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::Provider,
                "Drive did not honor the requested byte range",
            ));
        }
        if let Some(expected_length) = request
            .range
            .and_then(|range| range.end_exclusive.map(|end| end - range.start))
        {
            if response.body.len() as u64 != expected_length {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::Provider,
                    "Drive returned an unexpected ranged response length",
                ));
            }
        }
        let parent = request.destination_path.parent().ok_or_else(|| {
            MusicStorageError::new(
                MusicStorageErrorKind::Io,
                "download destination has no parent directory",
            )
        })?;
        fs::create_dir_all(parent).map_err(io_error)?;
        let temporary = parent.join(format!(".rec-bag-download-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<(ContentHash, Option<ContentHash>), MusicStorageError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(io_error)?;
            file.write_all(&response.body).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            let expected = if request.range.is_none() {
                request
                    .expected_checksum
                    .clone()
                    .or_else(|| preferred_hash(&object).cloned())
            } else {
                None
            };
            let checksum = hash_file_as(
                &temporary,
                expected
                    .as_ref()
                    .map(ContentHash::algorithm)
                    .unwrap_or(&HashAlgorithm::Sha256),
            )?;
            if expected
                .as_ref()
                .is_some_and(|expected| expected != &checksum)
            {
                return Err(MusicStorageError::new(
                    MusicStorageErrorKind::ChecksumMismatch,
                    "downloaded bytes did not match expected checksum",
                ));
            }
            fs::rename(&temporary, &request.destination_path).map_err(io_error)?;
            sync_directory(parent)?;
            Ok((checksum, expected))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        let (checksum, expected) = result?;
        Ok(DownloadResult {
            object,
            destination_path: request.destination_path,
            bytes_written: response.body.len() as u64,
            checksum: Some(checksum),
            checksum_verified: expected.is_some(),
        })
    }

    /// Downloads an object to the verified local playback cache, returning its path.
    pub async fn cache_for_playback(
        &self,
        object_id: &str,
    ) -> Result<std::path::PathBuf, MusicStorageError> {
        let cache = self.cache.as_ref().ok_or_else(|| {
            MusicStorageError::new(
                MusicStorageErrorKind::NotFound,
                "playback cache is disabled",
            )
        })?;
        if let Some(path) = cache.cached_path(object_id)? {
            return Ok(path);
        }
        let path = cache.path_for(object_id);
        let result = self
            .download_inner(DownloadRequest {
                object_id: object_id.to_owned(),
                destination_path: path.clone(),
                range: None,
                expected_checksum: None,
                overwrite: true,
            })
            .await?;
        let checksum = result.checksum.ok_or_else(|| {
            MusicStorageError::new(
                MusicStorageErrorKind::ChecksumMismatch,
                "playback cache download did not produce a checksum",
            )
        })?;
        cache.record(object_id, &path, result.bytes_written, checksum)?;
        Ok(path)
    }
}

impl<T: DriveHttpTransport + Send + Sync, P: GoogleTokenProvider + Send + Sync> MusicAssetStore
    for GoogleDriveMusicStore<T, P>
{
    fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
        Box::pin(async move {
            let url = format!(
                "{}/about?fields=user(permissionId)&supportsAllDrives=false",
                self.config.api_base_url
            );
            match self.execute(DriveHttpRequest::new("GET", url)).await {
                Ok(_) => Ok(StorageHealth {
                    status: HealthStatus::Healthy,
                    checked_at: Utc::now(),
                    message: None,
                    capabilities: drive_capabilities(),
                }),
                Err(error) => Ok(StorageHealth {
                    status: match error.kind {
                        MusicStorageErrorKind::AuthenticationRequired => {
                            HealthStatus::AuthenticationRequired
                        }
                        MusicStorageErrorKind::TemporarilyUnavailable
                        | MusicStorageErrorKind::RateLimited => HealthStatus::Degraded,
                        _ => HealthStatus::Unavailable,
                    },
                    checked_at: Utc::now(),
                    message: Some(error.message),
                    capabilities: drive_capabilities(),
                }),
            }
        })
    }

    fn inventory_page(
        &self,
        request: InventoryRequest,
    ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
        Box::pin(async move {
            match request.mode.clone() {
                InventoryMode::Full => self.inventory_full(request).await,
                InventoryMode::Incremental { change_cursor } => {
                    self.inventory_incremental(change_cursor.value, request)
                        .await
                }
            }
        })
    }

    fn stat<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
        Box::pin(async move { Ok(self.storage_object(self.file(object_id).await?)) })
    }

    fn versions_page(
        &self,
        request: StorageVersionRequest,
    ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
        Box::pin(async move {
            let mut url = format!("{}/files/{}/revisions?pageSize={}&fields={}", self.config.api_base_url, url_encode(&request.object_id), request.page_size.unwrap_or(100).clamp(1, 1000), url_encode("nextPageToken,revisions(id,modifiedTime,size,md5Checksum,sha1Checksum,sha256Checksum,keepForever)"));
            if let Some(cursor) = request.page_cursor {
                url.push_str("&pageToken=");
                url.push_str(&url_encode(&cursor.value));
            }
            let page: DriveRevisionList =
                parse_json(self.execute(DriveHttpRequest::new("GET", url)).await?)?;
            let versions = page
                .revisions
                .into_iter()
                .map(|revision| {
                    let hashes = revision.hashes();
                    StorageObjectVersion {
                        object_id: request.object_id.clone(),
                        version_id: revision.id,
                        modified_at: revision.modified_time,
                        size_bytes: revision.size,
                        hashes,
                        is_current: false,
                    }
                })
                .collect();
            Ok(StorageVersionPage {
                versions,
                next_page_cursor: page.next_page_token.map(SyncCursor::new),
            })
        })
    }

    fn upload(
        &self,
        request: UploadRequest,
    ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
        Box::pin(async move { self.upload_inner(request).await })
    }
    fn download(
        &self,
        request: DownloadRequest,
    ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
        Box::pin(async move { self.download_inner(request).await })
    }
}

fn drive_capabilities() -> StorageCapabilities {
    StorageCapabilities {
        can_inventory: true,
        can_stat: true,
        can_upload: true,
        can_download: true,
        can_create_containers: false,
        supports_atomic_replace: false,
        supports_change_tracking: true,
        supports_version_history: true,
        provider_hash_algorithms: vec![
            HashAlgorithm::Md5,
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
        ],
    }
}

const FILE_FIELDS: &str = "id,name,mimeType,size,modifiedTime,version,etag,md5Checksum,sha1Checksum,sha256Checksum,trashed,appProperties,capabilities(canDownload),parents";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    next_page_token: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveChangeList {
    #[serde(default)]
    changes: Vec<DriveChange>,
    next_page_token: Option<String>,
    new_start_page_token: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveChange {
    file_id: String,
    #[serde(default)]
    removed: bool,
    time: Option<DateTime<Utc>>,
    file: Option<DriveFile>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: String,
    mime_type: Option<String>,
    #[serde(default, deserialize_with = "optional_u64")]
    size: Option<u64>,
    modified_time: Option<DateTime<Utc>>,
    version: Option<String>,
    etag: Option<String>,
    md5_checksum: Option<String>,
    sha1_checksum: Option<String>,
    sha256_checksum: Option<String>,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    app_properties: BTreeMap<String, String>,
    #[serde(default)]
    capabilities: DriveCapabilities,
    #[serde(default)]
    parents: Vec<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveCapabilities {
    #[serde(default)]
    can_download: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveRevisionList {
    #[serde(default)]
    revisions: Vec<DriveRevision>,
    next_page_token: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveRevision {
    id: String,
    modified_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "optional_u64")]
    size: Option<u64>,
    md5_checksum: Option<String>,
    sha1_checksum: Option<String>,
    sha256_checksum: Option<String>,
}
impl DriveRevision {
    fn hashes(&self) -> Vec<ContentHash> {
        [
            (HashAlgorithm::Md5, self.md5_checksum.as_deref()),
            (HashAlgorithm::Sha1, self.sha1_checksum.as_deref()),
            (HashAlgorithm::Sha256, self.sha256_checksum.as_deref()),
        ]
        .into_iter()
        .filter_map(|(algorithm, value)| {
            value.and_then(|value| ContentHash::new(algorithm, value).ok())
        })
        .collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FullInventoryCursor {
    parents: VecDeque<String>,
    active_parent: Option<String>,
    api_page_token: Option<String>,
    pending: VecDeque<DriveFileCursor>,
    #[serde(default)]
    seen_folders: BTreeSet<String>,
    #[serde(default)]
    seen_objects: BTreeSet<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct DriveFileCursor {
    id: String,
    name: String,
    mime_type: Option<String>,
    size: Option<u64>,
    modified_time: Option<DateTime<Utc>>,
    version: Option<String>,
    etag: Option<String>,
    md5_checksum: Option<String>,
    sha1_checksum: Option<String>,
    sha256_checksum: Option<String>,
    trashed: bool,
    app_properties: BTreeMap<String, String>,
    can_download: bool,
    parents: Vec<String>,
}
impl FullInventoryCursor {
    fn new(root: &str) -> Self {
        Self {
            parents: VecDeque::from([root.to_owned()]),
            active_parent: None,
            api_page_token: None,
            pending: VecDeque::new(),
            seen_folders: BTreeSet::from([root.to_owned()]),
            seen_objects: BTreeSet::new(),
        }
    }
}
impl From<DriveFile> for DriveFileCursor {
    fn from(file: DriveFile) -> Self {
        Self {
            id: file.id,
            name: file.name,
            mime_type: file.mime_type,
            size: file.size,
            modified_time: file.modified_time,
            version: file.version,
            etag: file.etag,
            md5_checksum: file.md5_checksum,
            sha1_checksum: file.sha1_checksum,
            sha256_checksum: file.sha256_checksum,
            trashed: file.trashed,
            app_properties: file.app_properties,
            can_download: file.capabilities.can_download,
            parents: file.parents,
        }
    }
}
impl From<DriveFileCursor> for DriveFile {
    fn from(file: DriveFileCursor) -> Self {
        Self {
            id: file.id,
            name: file.name,
            mime_type: file.mime_type,
            size: file.size,
            modified_time: file.modified_time,
            version: file.version,
            etag: file.etag,
            md5_checksum: file.md5_checksum,
            sha1_checksum: file.sha1_checksum,
            sha256_checksum: file.sha256_checksum,
            trashed: file.trashed,
            app_properties: file.app_properties,
            capabilities: DriveCapabilities {
                can_download: file.can_download,
            },
            parents: file.parents,
        }
    }
}

fn optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<serde_json::Value>::deserialize(deserializer)?
        .map(|value| match value {
            serde_json::Value::String(value) => value.parse().map_err(serde::de::Error::custom),
            serde_json::Value::Number(value) => value
                .as_u64()
                .ok_or_else(|| serde::de::Error::custom("invalid unsigned size")),
            _ => Err(serde::de::Error::custom("invalid size")),
        })
        .transpose()
}
fn parse_json<T: for<'de> Deserialize<'de>>(
    response: DriveHttpResponse,
) -> Result<T, MusicStorageError> {
    serde_json::from_slice(&response.body).map_err(|error| {
        MusicStorageError::new(
            MusicStorageErrorKind::InvalidData,
            format!("invalid Drive response: {error}"),
        )
    })
}
fn app_properties(metadata: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for (key, target) in [
        ("rec_bag_track_id", "rec_bag_track_id"),
        ("track_id", "rec_bag_track_id"),
        ("rec_bag_asset_id", "rec_bag_asset_id"),
        ("asset_id", "rec_bag_asset_id"),
    ] {
        if let Some(value) = metadata.get(key) {
            values
                .entry(target.to_owned())
                .or_insert_with(|| value.clone());
        }
    }
    values
}
fn file_name_from_path(path: &str) -> Result<String, MusicStorageError> {
    let path = path.replace('\\', "/");
    let name = path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| {
            MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "upload object path has no file name",
            )
        })?;
    Ok(name.to_owned())
}
fn matches_prefix(file: &DriveFile, prefix: Option<&str>) -> bool {
    prefix.is_none_or(|prefix| file.name.starts_with(prefix))
}
fn preferred_hash(object: &StorageObject) -> Option<&ContentHash> {
    object
        .hashes
        .iter()
        .find(|hash| hash.algorithm() == &HashAlgorithm::Sha256)
        .or_else(|| {
            object
                .hashes
                .iter()
                .find(|hash| hash.algorithm() == &HashAlgorithm::Md5)
        })
        .or_else(|| {
            object
                .hashes
                .iter()
                .find(|hash| hash.algorithm() == &HashAlgorithm::Sha1)
        })
}
fn hash_file_as(path: &Path, algorithm: &HashAlgorithm) -> Result<ContentHash, MusicStorageError> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    let value = match algorithm {
        HashAlgorithm::Sha256 => format!("{:x}", Sha256::digest(&bytes)),
        HashAlgorithm::Sha1 => format!("{:x}", Sha1::digest(&bytes)),
        HashAlgorithm::Md5 => format!("{:x}", Md5::digest(&bytes)),
        HashAlgorithm::Other(name) => {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                format!("unsupported checksum algorithm: {name}"),
            ))
        }
    };
    ContentHash::new(algorithm.clone(), value).map_err(|error| {
        MusicStorageError::new(MusicStorageErrorKind::InvalidData, error.to_string())
    })
}
fn token_error(error: GoogleTokenError) -> MusicStorageError {
    MusicStorageError::new(
        if error.retryable {
            MusicStorageErrorKind::TemporarilyUnavailable
        } else {
            MusicStorageErrorKind::AuthenticationRequired
        },
        error.message,
    )
}
fn sync_directory(path: &Path) -> Result<(), MusicStorageError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)?;
    Ok(())
}
fn skip_file(file: &mut File, mut bytes: u64) -> Result<(), MusicStorageError> {
    let mut buffer = [0_u8; 8192];
    while bytes > 0 {
        let buffer_length = buffer.len() as u64;
        let read = file
            .read(&mut buffer[..bytes.min(buffer_length) as usize])
            .map_err(io_error)?;
        if read == 0 {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::Io,
                "could not resume upload beyond source length",
            ));
        }
        bytes -= read as u64;
    }
    Ok(())
}
fn io_error(error: std::io::Error) -> MusicStorageError {
    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
}
fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                vec![(byte as char).to_string()]
            } else {
                vec![format!("%{byte:02X}")]
            }
        })
        .collect()
}
fn range_end(value: &str) -> Option<u64> {
    value.rsplit('-').next()?.parse().ok()
}
fn response_retry_delay(response: &DriveHttpResponse) -> Option<Duration> {
    response
        .header("Retry-After")?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
fn drive_error(response: &DriveHttpResponse) -> MusicStorageError {
    #[derive(Deserialize)]
    struct Envelope {
        error: Option<DriveError>,
    }
    #[derive(Deserialize)]
    struct DriveError {
        message: Option<String>,
        status: Option<String>,
    }
    let parsed: Option<Envelope> = serde_json::from_slice(&response.body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.error.as_ref())
        .and_then(|value| value.message.clone())
        .unwrap_or_else(|| format!("Google Drive returned HTTP {}", response.status));
    let kind = match response.status {
        401 => MusicStorageErrorKind::AuthenticationRequired,
        403 => MusicStorageErrorKind::PermissionDenied,
        404 => MusicStorageErrorKind::NotFound,
        429 => MusicStorageErrorKind::RateLimited,
        500..=599 => MusicStorageErrorKind::TemporarilyUnavailable,
        _ => MusicStorageErrorKind::Provider,
    };
    let mut error = MusicStorageError::new(kind, message);
    error.provider_code = parsed.and_then(|value| value.error.and_then(|value| value.status));
    error.retry_after_seconds = response_retry_delay(response).map(|delay| delay.as_secs());
    error
}

#[cfg(test)]
mod tests;
