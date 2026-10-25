//! Resolving a canonical track's audio to a file the audio engine can decode.
//!
//! Cache paths are execution details: this module hands them to the Catalog for one
//! load, and never writes one back to a track or the catalog.

use std::sync::Arc;

use music_storage::{MusicStorages, PlayableOrigin, StorageError};
use uuid::Uuid;

use super::catalog::{CatalogError, PlaybackPathFuture, PlaybackPaths, ResolvedPlaybackPath};
use super::integration::DesktopIntegration;

/// Resolve a track's audio from the active primary, or from a catalog source
/// for tracks the primary cannot serve.
pub struct AssetPlaybackPaths {
    catalog: Arc<rec_persistence::TursoCatalog>,
    storages: MusicStorages,
}

impl AssetPlaybackPaths {
    /// Build a resolver from the runtime's public, provider-neutral APIs.
    pub fn new(runtime: &Arc<DesktopIntegration>) -> Arc<Self> {
        Arc::new(Self {
            catalog: runtime.catalog().clone(),
            storages: runtime.storages().clone(),
        })
    }
}

impl PlaybackPaths for AssetPlaybackPaths {
    fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a> {
        Box::pin(async move {
            // Only the primary serves owned files; a backup never does.
            let error = match self.storages.playable(track_id).await {
                Ok(playable) => {
                    return Ok(ResolvedPlaybackPath {
                        path: playable.path,
                        cached: matches!(playable.origin, PlayableOrigin::OfflineCache { .. }),
                    })
                }
                Err(error) => error,
            };

            // Tracks the primary cannot serve may still play from where a local
            // catalog source discovered them (this is discovery, not custody).
            // Uses an indexed query on canonical_track_id to avoid a full table scan.
            let source_paths = self
                .catalog
                .local_source_paths_for_track(track_id)
                .await
                .unwrap_or_default();
            if let Some(path) = source_paths.into_iter().find(|path| path.is_file()) {
                return Ok(ResolvedPlaybackPath {
                    path,
                    cached: false,
                });
            }

            Err(CatalogError::new(match error {
                StorageError::NoPrimary => {
                    format!("track {track_id} has no local file and no primary storage is set up")
                }
                StorageError::PrimaryUnavailable {
                    backup_has_copy: true,
                } => format!(
                    "track {track_id} is missing from the primary storage; restore it from backup"
                ),
                StorageError::PrimaryUnavailable {
                    backup_has_copy: false,
                } => format!("track {track_id} has no verified available asset location"),
                other => format!("could not resolve track {track_id} for playback: {other}"),
            }))
        })
    }
}
