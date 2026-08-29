//! Resolving a canonical track's audio to a file the audio engine can decode.
//!
//! Cache paths are execution details: this module hands them to the Catalog for one
//! load, and never writes one back to a track or the catalog.

use std::{path::PathBuf, sync::Arc};

use rec_core::{AssetRepository, StorageVerificationStatus};
use uuid::Uuid;

use super::catalog::{CatalogError, PlaybackPathFuture, PlaybackPaths, ResolvedPlaybackPath};
use super::integration::{AssetReference, AssetResolver, DesktopIntegration};

/// Resolve a track's audio through the live storage runtime.
pub struct AssetPlaybackPaths {
    catalog: Arc<rec_persistence::TursoCatalog>,
    resolver: Arc<dyn AssetResolver>,
}

impl AssetPlaybackPaths {
    /// Build a resolver from the runtime's public, provider-neutral APIs.
    pub fn new(runtime: &Arc<DesktopIntegration>) -> Arc<Self> {
        Arc::new(Self {
            catalog: runtime.catalog().clone(),
            resolver: runtime.asset_resolver(),
        })
    }
}

impl PlaybackPaths for AssetPlaybackPaths {
    fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a> {
        Box::pin(async move {
            let mut originals =
                AssetRepository::list_originals_for_track(self.catalog.as_ref(), track_id)
                    .await
                    .map_err(|error| {
                        CatalogError::new(format!(
                            "could not load canonical assets for track {track_id}: {error}"
                        ))
                    })?;

            // Prefer the user's selected original, then the newest metadata version.
            originals.sort_by_key(|asset| (!asset.is_preferred, std::cmp::Reverse(asset.version)));

            let mut last_error = None;
            for asset in originals {
                let mut locations =
                    AssetRepository::list_locations_for_asset(self.catalog.as_ref(), asset.id())
                        .await
                        .map_err(|error| {
                            CatalogError::new(format!(
                                "could not load locations for asset {}: {error}",
                                asset.id()
                            ))
                        })?;
                locations.retain(|location| {
                    location.verification_status == StorageVerificationStatus::Verified
                });

                for location in locations {
                    let reference = AssetReference {
                        storage_id: location.storage_id.to_string(),
                        asset_id: asset.id().to_string(),
                    };
                    match self.resolver.resolve(&reference).await {
                        Ok(resolved) => {
                            let candidate = PathBuf::from(resolved.uri);
                            if candidate.is_file() {
                                return Ok(ResolvedPlaybackPath {
                                    path: candidate,
                                    cached: resolved.cached,
                                });
                            }
                            last_error = Some(CatalogError::new(format!(
                                "resolved playback path is not an available local file: {}",
                                candidate.display()
                            )));
                        }
                        Err(error) => {
                            last_error = Some(CatalogError::new(format!(
                                "could not resolve asset {} from storage {}: {error}",
                                asset.id(),
                                location.storage_id
                            )));
                        }
                    }
                }
            }

            // Fall back to source occurrences for tracks that only exist in local
            // sources and have not yet been copied to a managed storage. Uses an
            // indexed query on canonical_track_id to avoid a full table scan.
            let source_paths = self
                .catalog
                .local_source_paths_for_track(track_id)
                .await
                .unwrap_or_default();
            for path in source_paths {
                if path.is_file() {
                    return Ok(ResolvedPlaybackPath {
                        path,
                        cached: false,
                    });
                }
            }

            Err(last_error.unwrap_or_else(|| {
                CatalogError::new(format!(
                    "track {track_id} has no verified available asset location"
                ))
            }))
        })
    }
}
