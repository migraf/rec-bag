//! Minimal bridge from legacy playback state to the live Stage 3 asset resolver.
//!
//! Cache paths are execution details: this module returns them to playback and analysis,
//! but never writes them back to a [`rec_core::Track`] or the catalog.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, OnceLock, RwLock},
};

use rec_core::{AssetRepository, StorageVerificationStatus};
use uuid::Uuid;

use super::integration::{AssetReference, AssetResolver, DesktopIntegration};

pub type PlaybackPathFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedPlaybackPath, PlaybackPathError>> + Send + 'a>>;

/// A local file prepared for immediate playback or analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlaybackPath {
    pub path: PathBuf,
    /// True when the provider resolver materialized a disposable cache entry.
    pub cached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackPathError {
    message: String,
}

impl PlaybackPathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlaybackPathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PlaybackPathError {}

/// Callback boundary owned by playback. Integration supplies an implementation backed by
/// its live runtime without exposing provider-specific services to application state.
pub trait PlaybackPathResolver: Send + Sync {
    fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a>;
}

fn resolver_slot() -> &'static RwLock<Option<Arc<dyn PlaybackPathResolver>>> {
    static RESOLVER: OnceLock<RwLock<Option<Arc<dyn PlaybackPathResolver>>>> = OnceLock::new();
    RESOLVER.get_or_init(|| RwLock::new(None))
}

/// Install or clear the callback used by deck loading and analysis.
///
/// The integration composition root should install one callback from its live runtime.
pub fn set_playback_path_resolver(resolver: Option<Arc<dyn PlaybackPathResolver>>) {
    *resolver_slot()
        .write()
        .expect("playback resolver lock poisoned") = resolver;
}

/// Build and install a callback from the public, provider-neutral runtime APIs.
pub fn register_integration_runtime(runtime: Arc<DesktopIntegration>) {
    set_playback_path_resolver(Some(Arc::new(RuntimePlaybackResolver {
        catalog: runtime.catalog().clone(),
        resolver: runtime.asset_resolver(),
    })));
}

/// Resolve a canonical asset when registered, otherwise retain compatibility with an
/// existing legacy local path. A provider URI or a missing path is never sent to audio.
pub async fn resolve_track_path(
    track_id: Uuid,
    legacy_path: &Path,
) -> Result<ResolvedPlaybackPath, PlaybackPathError> {
    let resolver = resolver_slot()
        .read()
        .expect("playback resolver lock poisoned")
        .clone();
    resolve_track_path_with(resolver.as_deref(), track_id, legacy_path).await
}

async fn resolve_track_path_with(
    resolver: Option<&dyn PlaybackPathResolver>,
    track_id: Uuid,
    legacy_path: &Path,
) -> Result<ResolvedPlaybackPath, PlaybackPathError> {
    let canonical_error = if let Some(resolver) = resolver {
        match resolver.resolve(track_id).await {
            Ok(resolved) => return verified_local_file(resolved),
            Err(error) => Some(error),
        }
    } else {
        None
    };

    if legacy_path.is_file() {
        return Ok(ResolvedPlaybackPath {
            path: legacy_path.to_path_buf(),
            cached: false,
        });
    }

    Err(canonical_error.unwrap_or_else(|| {
        PlaybackPathError::new(format!(
            "track {track_id} has no canonical resolver and no available legacy local path"
        ))
    }))
}

fn verified_local_file(
    resolved: ResolvedPlaybackPath,
) -> Result<ResolvedPlaybackPath, PlaybackPathError> {
    if resolved.path.is_file() {
        Ok(resolved)
    } else {
        Err(PlaybackPathError::new(format!(
            "resolved playback path is not an available local file: {}",
            resolved.path.display()
        )))
    }
}

struct RuntimePlaybackResolver {
    catalog: Arc<rec_persistence::TursoCatalog>,
    resolver: Arc<dyn AssetResolver>,
}

impl PlaybackPathResolver for RuntimePlaybackResolver {
    fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a> {
        Box::pin(async move {
            let mut originals =
                AssetRepository::list_originals_for_track(self.catalog.as_ref(), track_id)
                    .await
                    .map_err(|error| {
                        PlaybackPathError::new(format!(
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
                            PlaybackPathError::new(format!(
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
                            let candidate = ResolvedPlaybackPath {
                                path: PathBuf::from(resolved.uri),
                                cached: resolved.cached,
                            };
                            match verified_local_file(candidate) {
                                Ok(path) => return Ok(path),
                                Err(error) => last_error = Some(error),
                            }
                        }
                        Err(error) => {
                            last_error = Some(PlaybackPathError::new(format!(
                                "could not resolve asset {} from storage {}: {error}",
                                asset.id(),
                                location.storage_id
                            )));
                        }
                    }
                }
            }

            Err(last_error.unwrap_or_else(|| {
                PlaybackPathError::new(format!(
                    "track {track_id} has no verified available asset location"
                ))
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubResolver {
        result: Mutex<Option<Result<ResolvedPlaybackPath, PlaybackPathError>>>,
    }

    impl StubResolver {
        fn returning(result: Result<ResolvedPlaybackPath, PlaybackPathError>) -> Self {
            Self {
                result: Mutex::new(Some(result)),
            }
        }
    }

    impl PlaybackPathResolver for StubResolver {
        fn resolve<'a>(&'a self, _track_id: Uuid) -> PlaybackPathFuture<'a> {
            let result = self
                .result
                .lock()
                .expect("stub resolver lock poisoned")
                .take()
                .expect("stub resolver called more than once");
            Box::pin(async move { result })
        }
    }

    fn temp_file(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rec-bag-playback-{label}-{}", Uuid::new_v4()));
        std::fs::write(&path, b"audio").unwrap();
        path
    }

    #[tokio::test]
    async fn resolves_verified_local_asset() {
        let local = temp_file("local");
        let resolver = StubResolver::returning(Ok(ResolvedPlaybackPath {
            path: local.clone(),
            cached: false,
        }));

        let resolved = resolve_track_path_with(
            Some(&resolver),
            Uuid::new_v4(),
            Path::new("provider-identity"),
        )
        .await
        .unwrap();

        assert_eq!(resolved.path, local);
        assert!(!resolved.cached);
        std::fs::remove_file(resolved.path).unwrap();
    }

    #[tokio::test]
    async fn resolves_cached_drive_asset_without_changing_identity() {
        let cache = temp_file("drive-cache");
        let identity = PathBuf::from("drive-provider-asset");
        let resolver = StubResolver::returning(Ok(ResolvedPlaybackPath {
            path: cache.clone(),
            cached: true,
        }));

        let resolved = resolve_track_path_with(Some(&resolver), Uuid::new_v4(), &identity)
            .await
            .unwrap();

        assert_eq!(resolved.path, cache);
        assert!(resolved.cached);
        assert_eq!(identity, PathBuf::from("drive-provider-asset"));
        std::fs::remove_file(resolved.path).unwrap();
    }

    #[tokio::test]
    async fn rejects_unavailable_provider_and_missing_fallback() {
        let resolver = StubResolver::returning(Err(PlaybackPathError::new("Drive unavailable")));
        let error = resolve_track_path_with(
            Some(&resolver),
            Uuid::new_v4(),
            Path::new("missing-provider-identity"),
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "Drive unavailable");
    }

    #[tokio::test]
    async fn falls_back_to_existing_legacy_local_path() {
        let legacy = temp_file("legacy");
        let resolver =
            StubResolver::returning(Err(PlaybackPathError::new("canonical track not found")));

        let resolved = resolve_track_path_with(Some(&resolver), Uuid::new_v4(), &legacy)
            .await
            .unwrap();

        assert_eq!(resolved.path, legacy);
        assert!(!resolved.cached);
        std::fs::remove_file(resolved.path).unwrap();
    }
}
