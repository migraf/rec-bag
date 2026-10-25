//! Copying a track's original to the active primary: finding bytes for it,
//! storing them under a managed name, verifying what the primary holds, and
//! handing the verified write to replication.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use rec_core::{
    AssetLocation, AssetRepository, AssetType, CanonicalTrack, ContentHash, DownloadRequest,
    MetadataField, MetadataValue, MusicAssetStore, MusicStorage, MusicStorageErrorKind,
    MusicStorageRepository, OriginalAsset, Setting, SettingsRepository, SourceAvailability,
    SourceCopyPolicy, SourceRepository, StorageObject, StorageVerificationStatus, SyncRun,
    SyncRunKind, TrackRepository, TransactionManager, TransferCheckpoint, TransferStage,
    UploadRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::catalog_io;
use crate::error::StorageError;
use crate::handoff::HandoffReport;
use crate::offline_cache::sha256_file;
use crate::reconcile::ProbedHealth;
use crate::roots::{source_root, RootLocator};
use crate::settings::storage_key;
use crate::storages::{MusicStorages, CONFIG_VERIFY_AFTER_COPY};

/// Where a local catalog source occurrence records its file, relative to the
/// source's root.
const OCCURRENCE_RELATIVE_PATH: &str = "relative_path";

/// Who asked for a copy, which decides what a source's copy policy allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAuthorization {
    /// The user asked for this copy.
    UserConfirmed,
    /// rec_bag is copying on its own.
    Automatic,
}

impl CopyAuthorization {
    /// Whether a source with `policy` may be copied from under this
    /// authorization.
    fn permits(self, policy: SourceCopyPolicy) -> Result<(), StorageError> {
        match (policy, self) {
            (SourceCopyPolicy::CheckOnly, _) => Err(StorageError::CopyForbidden),
            (SourceCopyPolicy::AskBeforeCopying, Self::Automatic) => {
                Err(StorageError::CopyConfirmationRequired)
            }
            _ => Ok(()),
        }
    }
}

/// Where the primary's copy of a track came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOrigin {
    /// The primary already held a verified copy; nothing was copied.
    AlreadyPresent,
    /// The catalog source's file already sits inside the primary; it was
    /// verified where it is.
    InPlace,
    /// Copied from a file of this catalog source.
    CatalogSource(Uuid),
    /// Restored from a verified copy on this storage.
    Storage(Uuid),
}

/// The outcome of [`MusicStorages::copy_to_primary`].
#[derive(Debug, Clone, PartialEq)]
pub struct Copied {
    pub track_id: Uuid,
    pub asset_id: Uuid,
    /// The active primary.
    pub storage_id: Uuid,
    pub object_id: String,
    pub origin: CopyOrigin,
    /// What replication planned for the verified write.
    pub handoff: HandoffReport,
}

/// Host bytes that can become a storage's copy of `asset`.
pub(crate) struct Obtained {
    /// The original, with the hash and size of these bytes.
    pub(crate) asset: OriginalAsset,
    pub(crate) path: PathBuf,
    hash: ContentHash,
    size_bytes: u64,
    origin: CopyOrigin,
    /// The catalog source the bytes came from, for the transfer run.
    source_id: Option<Uuid>,
    /// The destination's object when the file already sits inside it.
    in_place: Option<String>,
    /// A staged download to delete once the copy has finished.
    pub(crate) staged: bool,
}

/// Restart state for one upload, kept until its verified location commits.
#[derive(Debug, Serialize, Deserialize)]
struct TransferRecord {
    asset_id: Uuid,
    storage_id: Uuid,
    idempotency_key: String,
    source_path: PathBuf,
    transfer: TransferCheckpoint,
    updated_at: DateTime<Utc>,
}

impl MusicStorages {
    /// Copies `track_id`'s original to the active primary and verifies it.
    ///
    /// A track the primary already holds verified is left alone. Otherwise the
    /// bytes come from an obtainable file of a local catalog source, when its
    /// copy policy allows `authorization`; a file already inside the primary is
    /// verified in place instead of copied. Failing that, a verified copy on
    /// another enabled storage is restored, read directly from the host when
    /// the storage keeps it there and otherwise downloaded into staging.
    ///
    /// The copy is stored under a managed name and uploaded idempotently, then
    /// checked with `stat`; when the primary cannot confirm the content hash
    /// and the primary's `verify_after_copy` setting is on, it is downloaded
    /// back and hashed. Only verified bytes are recorded: the original, the
    /// storage object, a verified location and the transfer run commit
    /// together, so a mismatch records nothing. Reconciliation then runs and
    /// the write is handed to replication.
    pub async fn copy_to_primary(
        &self,
        track_id: Uuid,
        authorization: CopyAuthorization,
    ) -> Result<Copied, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let primary = MusicStorageRepository::get_active_primary(catalog)
            .await?
            .ok_or(StorageError::NoPrimary)?;
        let track = TrackRepository::get(catalog, track_id)
            .await?
            .ok_or(StorageError::TrackNotFound(track_id))?;
        let mut originals = AssetRepository::list_originals_for_track(catalog, track_id).await?;
        originals.sort_by_key(|asset| (!asset.is_preferred, Reverse(asset.version)));
        for original in &originals {
            if let Some(location) = verified_location(catalog, original.id(), primary.id).await? {
                let handoff = self
                    .0
                    .handoff
                    .verified_primary_write(&[original.id()])
                    .await;
                return Ok(Copied {
                    track_id,
                    asset_id: original.id(),
                    storage_id: primary.id,
                    object_id: location.object_id,
                    origin: CopyOrigin::AlreadyPresent,
                    handoff,
                });
            }
        }

        let obtained = self
            .obtain(&primary, track_id, &originals, authorization)
            .await?;
        let idempotency_key = format!(
            "copy-to-primary:{}:{}:{}",
            primary.id,
            obtained.asset.id(),
            obtained.hash.value()
        );
        let download_check = primary
            .config
            .get(CONFIG_VERIFY_AFTER_COPY)
            .is_none_or(|value| value != "false");
        let stored = self
            .store(
                &primary,
                &track,
                &obtained,
                &idempotency_key,
                download_check,
            )
            .await;
        if obtained.staged {
            let _ = fs::remove_file(&obtained.path);
        }
        let object_id = stored?;
        self.reconcile_active(&ProbedHealth::new()).await?;
        let asset_id = obtained.asset.id();
        let handoff = self.0.handoff.verified_primary_write(&[asset_id]).await;
        Ok(Copied {
            track_id,
            asset_id,
            storage_id: primary.id,
            object_id,
            origin: obtained.origin,
            handoff,
        })
    }

    /// Finds bytes for one of `originals`: a catalog source file first, then a
    /// verified copy on another storage. When neither exists, a copy policy
    /// refusal is reported ahead of a failed restore.
    async fn obtain(
        &self,
        primary: &MusicStorage,
        track_id: Uuid,
        originals: &[OriginalAsset],
        authorization: CopyAuthorization,
    ) -> Result<Obtained, StorageError> {
        if originals.is_empty() {
            return Err(StorageError::NoEligibleSource(track_id));
        }
        let mut refused = None;
        if let Some(obtained) = self
            .obtain_from_catalog_source(primary, track_id, originals, authorization, &mut refused)
            .await?
        {
            return Ok(obtained);
        }
        let others: BTreeMap<Uuid, MusicStorage> =
            MusicStorageRepository::list(self.0.catalog.as_ref())
                .await?
                .into_iter()
                .filter(|storage| storage.enabled && storage.id != primary.id)
                .map(|storage| (storage.id, storage))
                .collect();
        let mut restore_error = None;
        if let Some(obtained) = self
            .obtain_from_storage(&others, originals, &mut restore_error)
            .await?
        {
            return Ok(obtained);
        }
        Err(refused
            .or(restore_error)
            .unwrap_or(StorageError::NoEligibleSource(track_id)))
    }

    /// The first obtainable file of a local catalog source whose copy policy
    /// allows `authorization`. The original whose hash matches the file is
    /// used; otherwise the preferred one, which takes the file's hash and size.
    async fn obtain_from_catalog_source(
        &self,
        primary: &MusicStorage,
        track_id: Uuid,
        originals: &[OriginalAsset],
        authorization: CopyAuthorization,
        refused: &mut Option<StorageError>,
    ) -> Result<Option<Obtained>, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let occurrences = catalog_io::occurrences(catalog, None, Some(track_id)).await?;
        for occurrence in occurrences
            .iter()
            .filter(|occurrence| occurrence.availability == SourceAvailability::Obtainable)
        {
            let Some(source) =
                SourceRepository::get(catalog, occurrence.identity.source_id).await?
            else {
                continue;
            };
            let Some(RootLocator::HostPath(root)) = source_root(&source) else {
                continue;
            };
            let Some(MetadataValue::Text(relative)) =
                occurrence.provider_metadata.get(OCCURRENCE_RELATIVE_PATH)
            else {
                continue;
            };
            if let Err(error) = authorization.permits(source.copy_policy) {
                refused.get_or_insert(error);
                continue;
            }
            let Ok(path) = fs::canonicalize(root.join(relative)) else {
                continue;
            };
            if !path.is_file() {
                continue;
            }
            let hash = sha256_file(&path)?;
            let size_bytes = fs::metadata(&path)?.len();
            let mut asset = originals
                .iter()
                .find(|asset| asset.content_hash.as_ref() == Some(&hash))
                .unwrap_or(&originals[0])
                .clone();
            asset.content_hash = Some(hash.clone());
            asset.size_bytes = Some(size_bytes);
            let in_place = self.in_place_object_id(primary, &path);
            return Ok(Some(Obtained {
                asset,
                path,
                hash,
                size_bytes,
                origin: if in_place.is_some() {
                    CopyOrigin::InPlace
                } else {
                    CopyOrigin::CatalogSource(source.id)
                },
                source_id: Some(source.id),
                in_place,
                staged: false,
            }));
        }
        Ok(None)
    }

    /// A verified copy of one of `originals` on one of `storages`, read in
    /// place when the storage keeps it on the host, otherwise downloaded from
    /// its open store into staging. Bytes are hashed locally and must match
    /// the original's content hash. The last failed download is left in
    /// `restore_error`.
    pub(crate) async fn obtain_from_storage(
        &self,
        storages: &BTreeMap<Uuid, MusicStorage>,
        originals: &[OriginalAsset],
        restore_error: &mut Option<StorageError>,
    ) -> Result<Option<Obtained>, StorageError> {
        let catalog = self.0.catalog.as_ref();
        for original in originals {
            let Some(expected) = original
                .content_hash
                .clone()
                .filter(ContentHash::is_canonical_sha256)
            else {
                continue;
            };
            for location in
                AssetRepository::list_locations_for_asset(catalog, original.id()).await?
            {
                if location.verification_status != StorageVerificationStatus::Verified {
                    continue;
                }
                let Some(storage) = storages.get(&location.storage_id) else {
                    continue;
                };
                let obtained = |path: PathBuf, staged: bool| -> Result<Obtained, StorageError> {
                    let mut asset = original.clone();
                    let size_bytes = fs::metadata(&path)?.len();
                    asset.size_bytes = Some(size_bytes);
                    Ok(Obtained {
                        asset,
                        path,
                        hash: expected.clone(),
                        size_bytes,
                        origin: CopyOrigin::Storage(storage.id),
                        source_id: None,
                        in_place: None,
                        staged,
                    })
                };
                if let Some(path) = self
                    .0
                    .drivers
                    .get(&storage.kind)
                    .and_then(|driver| driver.direct_path(storage, &location.object_id))
                    .filter(|path| path.is_file())
                {
                    if sha256_file(&path).ok().as_ref() == Some(&expected) {
                        return obtained(path, false).map(Some);
                    }
                    continue;
                }
                let Some(store) = self.0.stores.get(storage.id) else {
                    continue;
                };
                match self
                    .stage_download(store.as_ref(), &location.object_id, &expected)
                    .await
                {
                    Ok(path) => return obtained(path, true).map(Some),
                    Err(error) => *restore_error = Some(error),
                }
            }
        }
        Ok(None)
    }

    /// Downloads `object_id` into staging and checks its locally computed hash.
    async fn stage_download(
        &self,
        store: &dyn MusicAssetStore,
        object_id: &str,
        expected: &ContentHash,
    ) -> Result<PathBuf, StorageError> {
        let staging = &self.0.config.staging_root;
        fs::create_dir_all(staging)?;
        let path = staging.join(format!("copy-{}.tmp", Uuid::new_v4()));
        let staged = async {
            store
                .download(DownloadRequest {
                    object_id: object_id.to_owned(),
                    destination_path: path.clone(),
                    range: None,
                    expected_checksum: Some(expected.clone()),
                    overwrite: true,
                })
                .await?;
            if &sha256_file(&path)? != expected {
                return Err(StorageError::Verification(
                    "restored bytes do not match the original's content hash".into(),
                ));
            }
            Ok(())
        }
        .await;
        match staged {
            Ok(()) => Ok(path),
            Err(error) => {
                let _ = fs::remove_file(&path);
                Err(error)
            }
        }
    }

    /// The primary's object ID for `path` when the primary is rooted on the
    /// host and `path` lies inside it.
    fn in_place_object_id(&self, primary: &MusicStorage, path: &Path) -> Option<String> {
        let RootLocator::HostPath(root) = self.0.drivers.get(&primary.kind)?.root(primary)? else {
            return None;
        };
        object_id_for(path.strip_prefix(&root).ok()?)
    }

    /// Puts `obtained` on `destination`, verifies it and records it; returns
    /// the destination's object ID.
    ///
    /// The upload carries `idempotency_key`, so a retried copy reuses what an
    /// interrupted one stored. `download_check` decides whether bytes the
    /// destination cannot vouch for are downloaded back and hashed (see
    /// [`Self::verify`]).
    pub(crate) async fn store(
        &self,
        destination: &MusicStorage,
        track: &CanonicalTrack,
        obtained: &Obtained,
        idempotency_key: &str,
        download_check: bool,
    ) -> Result<String, StorageError> {
        let store = self.open_store(destination).await?;
        let (object, checkpoint) = match &obtained.in_place {
            Some(object_id) => (store.stat(object_id).await?, None),
            None => {
                let (object, key) = self
                    .upload(
                        store.as_ref(),
                        destination,
                        track,
                        obtained,
                        idempotency_key,
                    )
                    .await?;
                (object, Some(key))
            }
        };
        let verified = self
            .verify(store.as_ref(), &object, &obtained.hash, download_check)
            .await;
        let observed = match verified {
            Ok(observed) => observed,
            Err(error) => {
                if let Some(key) = &checkpoint {
                    let _ = self.0.catalog.delete_setting(key).await;
                }
                return Err(error);
            }
        };
        self.record(destination, obtained, &object, observed)
            .await?;
        if let Some(key) = &checkpoint {
            // The verified location is the durable completion record; the
            // checkpoint was only restart state.
            self.0.catalog.delete_setting(key).await?;
        }
        Ok(object.object_id)
    }

    /// Uploads `obtained` under a free managed name, or reuses an object that
    /// already holds the same bytes there. Returns the object and the key of
    /// the checkpoint kept for the upload.
    async fn upload(
        &self,
        store: &dyn MusicAssetStore,
        destination: &MusicStorage,
        track: &CanonicalTrack,
        obtained: &Obtained,
        idempotency_key: &str,
    ) -> Result<(StorageObject, String), StorageError> {
        let asset = &obtained.asset;
        let idempotency_key = idempotency_key.to_owned();
        let key = transfer_key(destination.id, asset.id(), &idempotency_key);
        let file_name = match obtained.source_id {
            Some(_) => obtained.path.clone(),
            None => PathBuf::from(&asset.file_name),
        };
        let base = managed_object_path(
            preferred_text(track, MetadataField::Artist).as_deref(),
            preferred_text(track, MetadataField::Title).as_deref(),
            track.id,
            &file_name,
        );
        let (object_path, existing) = free_object_path(store, base, &obtained.hash).await?;
        let checkpoint = TransferCheckpoint::new(obtained.hash.clone(), None);
        let record = |source_path: PathBuf, transfer: TransferCheckpoint| TransferRecord {
            asset_id: asset.id(),
            storage_id: destination.id,
            idempotency_key: idempotency_key.clone(),
            source_path,
            transfer,
            updated_at: Utc::now(),
        };
        self.save_checkpoint(&key, record(obtained.path.clone(), checkpoint.clone()))
            .await?;
        let uploaded = match existing {
            Some(object) => object,
            None => {
                let uploaded = store
                    .upload(UploadRequest {
                        source_path: obtained.path.clone(),
                        object_path,
                        content_type: asset.media_type.clone(),
                        content_length: Some(obtained.size_bytes),
                        expected_checksum: Some(obtained.hash.clone()),
                        metadata: BTreeMap::from([
                            ("asset_id".into(), asset.id().to_string()),
                            ("track_id".into(), track.id.to_string()),
                        ]),
                        idempotency_key: Some(idempotency_key.clone()),
                    })
                    .await;
                match uploaded {
                    Ok(object) => object,
                    Err(error) => {
                        if !error.is_retryable() {
                            let _ = self.0.catalog.delete_setting(&key).await;
                        }
                        return Err(error.into());
                    }
                }
            }
        };
        let advanced = checkpoint
            .advance(
                TransferStage::Uploaded,
                &obtained.hash,
                None,
                Some(uploaded.object_id.clone()),
            )
            .map_err(|error| StorageError::Verification(error.to_string()))?;
        self.save_checkpoint(&key, record(PathBuf::new(), advanced))
            .await?;
        let object = store.stat(&uploaded.object_id).await?;
        Ok((object, key))
    }

    async fn save_checkpoint(&self, key: &str, record: TransferRecord) -> Result<(), StorageError> {
        let value =
            serde_json::to_value(record).map_err(|error| StorageError::Io(error.to_string()))?;
        SettingsRepository::set(
            self.0.catalog.as_ref(),
            Setting {
                key: key.to_owned(),
                value,
                updated_at: Utc::now(),
            },
        )
        .await?;
        Ok(())
    }

    /// Confirms that `object` holds `expected` and returns the hash observed.
    ///
    /// A matching `stat` hash suffices. A different one is checked by
    /// downloading the object back and hashing it when `download_check` is
    /// on, and rejected otherwise. When the destination reports no content hash,
    /// the object is downloaded and hashed when `download_check` is on; with it
    /// off, the store's own check of the upload against the expected checksum
    /// stands and no hash is observed.
    async fn verify(
        &self,
        store: &dyn MusicAssetStore,
        object: &StorageObject,
        expected: &ContentHash,
        download_check: bool,
    ) -> Result<Option<ContentHash>, StorageError> {
        match object.canonical_hash() {
            Some(observed) if observed == expected => return Ok(Some(observed.clone())),
            Some(_) if !download_check => {
                return Err(StorageError::Verification(
                    "the destination reports a different content hash than the original's".into(),
                ))
            }
            None if !download_check => return Ok(None),
            _ => {}
        }
        let staging = &self.0.config.staging_root;
        fs::create_dir_all(staging)?;
        let path = staging.join(format!("verify-{}.tmp", Uuid::new_v4()));
        let downloaded = store
            .download(DownloadRequest {
                object_id: object.object_id.clone(),
                destination_path: path.clone(),
                range: None,
                expected_checksum: Some(expected.clone()),
                overwrite: true,
            })
            .await;
        let matches = match downloaded {
            Ok(_) => sha256_file(&path).ok().as_ref() == Some(expected),
            Err(error) if error.kind == MusicStorageErrorKind::ChecksumMismatch => false,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error.into());
            }
        };
        let _ = fs::remove_file(&path);
        if matches {
            Ok(Some(expected.clone()))
        } else {
            Err(StorageError::Verification(
                "the destination's bytes do not match the original's content hash".into(),
            ))
        }
    }

    /// Records the verified copy: original, storage object, verified location
    /// and transfer run, in one transaction.
    async fn record(
        &self,
        destination: &MusicStorage,
        obtained: &Obtained,
        object: &StorageObject,
        observed: Option<ContentHash>,
    ) -> Result<(), StorageError> {
        let mut run = SyncRun::new(
            SyncRunKind::AssetTransfer,
            obtained.source_id,
            Some(destination.id),
        );
        run.start().expect("a new sync run can start");
        run.progress.discover(1);
        run.progress.add_bytes(obtained.size_bytes);
        run.progress.record_success();
        run.succeed().expect("a running sync run can succeed");
        let transaction = TransactionManager::begin(self.0.catalog.as_ref()).await?;
        transaction
            .assets()
            .upsert_original(obtained.asset.clone())
            .await?;
        transaction
            .assets()
            .upsert_storage_object(object.clone())
            .await?;
        let existing = transaction
            .assets()
            .get_location(destination.id, object.object_id.clone())
            .await?;
        let now = Utc::now();
        transaction
            .assets()
            .upsert_location(AssetLocation {
                id: existing.map_or_else(Uuid::new_v4, |location| location.id),
                asset_id: obtained.asset.id(),
                asset_type: AssetType::Original,
                storage_id: destination.id,
                object_id: object.object_id.clone(),
                verification_status: StorageVerificationStatus::Verified,
                expected_hash: Some(obtained.hash.clone()),
                observed_hash: observed,
                provider_version: object.provider_version.clone(),
                last_checked_at: Some(now),
                verified_at: Some(now),
                error_message: None,
            })
            .await?;
        transaction.sync_runs().upsert(run).await?;
        transaction.commit().await?;
        Ok(())
    }
}

/// The verified location of `asset_id` on `storage_id`, if any.
pub(crate) async fn verified_location(
    catalog: &rec_persistence::TursoCatalog,
    asset_id: Uuid,
    storage_id: Uuid,
) -> Result<Option<AssetLocation>, StorageError> {
    Ok(AssetRepository::list_locations_for_asset(catalog, asset_id)
        .await?
        .into_iter()
        .find(|location| {
            location.storage_id == storage_id
                && location.verification_status == StorageVerificationStatus::Verified
        }))
}

/// The settings key of the checkpoint for one upload of `asset_id`.
pub(crate) fn transfer_key(storage_id: Uuid, asset_id: Uuid, idempotency_key: &str) -> String {
    let digest = Sha256::digest(idempotency_key.as_bytes());
    storage_key(storage_id, &format!("transfer.{asset_id}.{digest:x}"))
}

/// The first of `base`, `base (2)`, `base (3)`, … that holds nothing, or that
/// already holds exactly `expected` (returned with its object, so it is reused
/// rather than uploaded again). A store that addresses objects by provider ID
/// rather than by path reports the path as not found or invalid; that counts
/// as free too.
async fn free_object_path(
    store: &dyn MusicAssetStore,
    base: String,
    expected: &ContentHash,
) -> Result<(String, Option<StorageObject>), StorageError> {
    for attempt in 1_u32.. {
        let candidate = if attempt == 1 {
            base.clone()
        } else {
            with_suffix(&base, attempt)
        };
        match store.stat(&candidate).await {
            Ok(object) if object.canonical_hash() == Some(expected) => {
                return Ok((candidate, Some(object)))
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind,
                    MusicStorageErrorKind::NotFound | MusicStorageErrorKind::InvalidData
                ) =>
            {
                return Ok((candidate, None))
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("unbounded suffix iterator exhausted")
}

/// `Artist/Title [short id].ext`, with each part sanitized as one path
/// component. A missing artist is `Unknown Artist`; a missing title falls back
/// to the source file's stem.
fn managed_object_path(
    artist: Option<&str>,
    title: Option<&str>,
    track_id: Uuid,
    source_file_name: &Path,
) -> String {
    let fallback_title = source_file_name
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Untitled");
    let artist = artist
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Unknown Artist");
    let title = title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_title);
    let extension = source_file_name
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!(".{}", value.to_ascii_lowercase()))
        .unwrap_or_default();
    let short_id = &track_id.to_string()[..8];
    format!(
        "{}/{} [{}]{}",
        sanitize_component(artist),
        sanitize_component(title),
        short_id,
        extension
    )
}

/// `Artist/Title [id] (n).ext` for `Artist/Title [id].ext`.
fn with_suffix(object_path: &str, suffix: u32) -> String {
    let (parent, name) = match object_path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, object_path),
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    let name = format!("{stem} ({suffix}){extension}");
    match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name,
    }
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .trim()
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    let sanitized = sanitized.trim_matches('.').trim();
    if sanitized.is_empty() {
        "Untitled".to_owned()
    } else {
        sanitized.to_owned()
    }
}

/// A relative host path as a `/`-separated object ID.
fn object_id_for(relative: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => components.push(value.to_str()?.to_owned()),
            _ => return None,
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn preferred_text(track: &CanonicalTrack, field: MetadataField) -> Option<String> {
    match &track.metadata.preferred(&field)?.value {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}
