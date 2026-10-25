//! Playback resolution: the host file a track plays from, which is always the
//! active primary's copy, either directly or through the offline cache.

use std::path::PathBuf;

use rec_core::{
    AssetLocation, AssetRepository, MusicStorage, MusicStorageErrorKind, MusicStorageRepository,
    OriginalAsset, StorageRole, StorageVerificationStatus, TrackRepository,
};
use uuid::Uuid;

use crate::error::StorageError;
use crate::offline_cache::CacheKey;
use crate::storages::MusicStorages;

/// A host file that plays a track's original from the active primary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playable {
    pub track_id: Uuid,
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub object_id: String,
    pub path: PathBuf,
    pub origin: PlayableOrigin,
}

/// How a [`Playable`] path was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayableOrigin {
    /// The primary keeps the object on the host; it plays in place.
    Direct,
    /// A verified offline cache copy; `hit` is false when it was just filled.
    OfflineCache { hit: bool },
}

/// The primary's verified copy of a track's original that playback uses.
struct Located {
    primary: MusicStorage,
    original: OriginalAsset,
    location: AssetLocation,
}

impl Located {
    fn cache_key(&self) -> CacheKey {
        CacheKey::for_location(&self.location)
    }
}

impl MusicStorages {
    /// The host file to play `track_id` from.
    ///
    /// Only the active primary serves playback. Originals are tried preferred
    /// first, then newest version; the first with a verified location on the
    /// primary plays in place when the primary keeps it on the host, otherwise
    /// from the offline cache, downloading it first when needed. A backup never
    /// serves playback: when the primary cannot serve the track the error says
    /// whether a backup holds a copy that could be restored.
    pub async fn playable(&self, track_id: Uuid) -> Result<Playable, StorageError> {
        let candidates = self.locate(track_id).await?;
        self.serve_first(&candidates)
            .await
            .map(|(playable, _)| playable)
    }

    /// Keeps (or stops keeping) `track_id` in the offline cache regardless of
    /// its budget.
    ///
    /// Pinning fills the cache first when needed. A track the primary plays in
    /// place needs no cache, so pinning it does nothing.
    pub async fn set_offline_pinned(
        &self,
        track_id: Uuid,
        pinned: bool,
    ) -> Result<(), StorageError> {
        let candidates = self.locate(track_id).await?;
        if !pinned {
            for located in &candidates.located {
                self.0
                    .cache
                    .set_pinned(&located.primary, &located.cache_key(), false)?;
            }
            return Ok(());
        }
        let (playable, located) = self.serve_first(&candidates).await?;
        if playable.origin == PlayableOrigin::Direct {
            return Ok(());
        }
        self.0
            .cache
            .set_pinned(&located.primary, &located.cache_key(), true)
    }

    /// Serves the first candidate that can be served.
    async fn serve_first<'a>(
        &self,
        candidates: &'a Candidates,
    ) -> Result<(Playable, &'a Located), StorageError> {
        let mut last_error = None;
        for located in &candidates.located {
            match self.serve(located).await {
                Ok(playable) => return Ok((playable, located)),
                Err(error) => last_error = Some(error),
            }
        }
        Err(match last_error {
            Some(error) if !primary_lacks_object(&error) => error,
            _ => StorageError::PrimaryUnavailable {
                backup_has_copy: candidates.backup_has_copy,
            },
        })
    }

    async fn serve(&self, located: &Located) -> Result<Playable, StorageError> {
        let Located {
            primary,
            original,
            location,
        } = located;
        let playable = |path, origin| Playable {
            track_id: original.track_id(),
            asset_id: original.id(),
            storage_id: primary.id,
            object_id: location.object_id.clone(),
            path,
            origin,
        };
        if let Some(path) = self
            .driver(&primary.kind)?
            .direct_path(primary, &location.object_id)
            .filter(|path| path.is_file())
        {
            return Ok(playable(path, PlayableOrigin::Direct));
        }
        let key = located.cache_key();
        if let Some(path) = self
            .0
            .cache
            .lookup(primary, &key, location.expected_hash.as_ref())?
        {
            return Ok(playable(path, PlayableOrigin::OfflineCache { hit: true }));
        }
        let store = self
            .0
            .stores
            .get(primary.id)
            .ok_or(StorageError::PrimaryUnavailable {
                backup_has_copy: false,
            })?;
        let path = self
            .0
            .cache
            .fill(primary, store.as_ref(), &key, location)
            .await?;
        Ok(playable(path, PlayableOrigin::OfflineCache { hit: false }))
    }

    async fn locate(&self, track_id: Uuid) -> Result<Candidates, StorageError> {
        let catalog = self.0.catalog.as_ref();
        let primary = MusicStorageRepository::get_active_primary(catalog)
            .await?
            .ok_or(StorageError::NoPrimary)?;
        if TrackRepository::get(catalog, track_id).await?.is_none() {
            return Err(StorageError::TrackNotFound(track_id));
        }
        let backups: Vec<Uuid> = MusicStorageRepository::list(catalog)
            .await?
            .into_iter()
            .filter(|storage| storage.role == StorageRole::Backup)
            .map(|storage| storage.id)
            .collect();
        let mut originals = AssetRepository::list_originals_for_track(catalog, track_id).await?;
        originals.sort_by_key(|asset| (!asset.is_preferred, std::cmp::Reverse(asset.version)));
        let mut candidates = Candidates {
            located: Vec::new(),
            backup_has_copy: false,
        };
        for original in originals {
            let verified: Vec<AssetLocation> =
                AssetRepository::list_locations_for_asset(catalog, original.id())
                    .await?
                    .into_iter()
                    .filter(|location| {
                        location.verification_status == StorageVerificationStatus::Verified
                    })
                    .collect();
            candidates.backup_has_copy |= verified
                .iter()
                .any(|location| backups.contains(&location.storage_id));
            if let Some(location) = verified
                .into_iter()
                .find(|location| location.storage_id == primary.id)
            {
                candidates.located.push(Located {
                    primary: primary.clone(),
                    original,
                    location,
                });
            }
        }
        Ok(candidates)
    }
}

struct Candidates {
    /// Originals with a verified location on the primary, in playback order.
    located: Vec<Located>,
    /// Whether any original is verified on a backup.
    backup_has_copy: bool,
}

/// Whether `error` means the primary does not (or cannot right now) hold the
/// object, as opposed to a failure worth reporting as is.
fn primary_lacks_object(error: &StorageError) -> bool {
    match error {
        StorageError::PrimaryUnavailable { .. } => true,
        StorageError::Provider(error) => error.kind == MusicStorageErrorKind::NotFound,
        _ => false,
    }
}
