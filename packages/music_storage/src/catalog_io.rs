//! Reads whole catalog collections through the repositories' paged interfaces.
//!
//! Every workflow that needs a complete collection (reconciliation, the
//! dashboard projection, source scans) reads it through these helpers rather
//! than repeating the paging loop.

use std::future::Future;

use rec_core::{
    AssetLocation, AssetLocationFilter, AssetRepository, CanonicalPlaylist, CanonicalTrack,
    CatalogSource, OriginalAsset, OriginalAssetFilter, PlaylistRepository,
    ReconciliationRepository, ReconciliationResolution, ReconciliationResolutionFilter,
    ReconciliationSuppression, RepositoryError, RepositoryPage, RepositoryPageRequest,
    SourceOccurrenceFilter, SourceRepository, SourceTrackOccurrence, StorageObject,
    TrackRepository,
};
use rec_persistence::TursoCatalog;
use uuid::Uuid;

const PAGE_SIZE: u32 = 500;

/// Follows `next_cursor` until the collection is exhausted.
async fn collect<T, F, Fut>(mut fetch: F) -> Result<Vec<T>, RepositoryError>
where
    F: FnMut(RepositoryPageRequest) -> Fut,
    Fut: Future<Output = Result<RepositoryPage<T>, RepositoryError>>,
{
    let mut values = Vec::new();
    let mut cursor = None;
    loop {
        let page = fetch(RepositoryPageRequest {
            cursor,
            limit: Some(PAGE_SIZE),
        })
        .await?;
        values.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(values);
        }
    }
}

/// Every catalog source.
pub async fn sources(catalog: &TursoCatalog) -> Result<Vec<CatalogSource>, RepositoryError> {
    collect(|page| SourceRepository::list(catalog, page)).await
}

/// Every canonical track.
pub async fn tracks(catalog: &TursoCatalog) -> Result<Vec<CanonicalTrack>, RepositoryError> {
    collect(|page| TrackRepository::list(catalog, page)).await
}

/// Every canonical playlist.
pub async fn playlists(catalog: &TursoCatalog) -> Result<Vec<CanonicalPlaylist>, RepositoryError> {
    collect(|page| PlaylistRepository::list(catalog, page)).await
}

/// Source track occurrences, optionally narrowed to one source and/or track.
pub async fn occurrences(
    catalog: &TursoCatalog,
    source_id: Option<Uuid>,
    track_id: Option<Uuid>,
) -> Result<Vec<SourceTrackOccurrence>, RepositoryError> {
    collect(|page| {
        SourceRepository::list_occurrences(
            catalog,
            SourceOccurrenceFilter {
                source_id,
                canonical_track_id: track_id,
                account_external_id: None,
            },
            page,
        )
    })
    .await
}

/// Every original asset.
pub async fn originals(catalog: &TursoCatalog) -> Result<Vec<OriginalAsset>, RepositoryError> {
    collect(|page| AssetRepository::list_originals(catalog, OriginalAssetFilter::default(), page))
        .await
}

/// Every asset location on every storage.
pub async fn locations(catalog: &TursoCatalog) -> Result<Vec<AssetLocation>, RepositoryError> {
    collect(|page| AssetRepository::list_locations(catalog, AssetLocationFilter::default(), page))
        .await
}

/// The storage objects the catalog currently records for `storage_id`.
pub async fn storage_objects(
    catalog: &TursoCatalog,
    storage_id: Uuid,
) -> Result<Vec<StorageObject>, RepositoryError> {
    collect(|page| AssetRepository::list_storage_objects(catalog, storage_id, page)).await
}

/// Every reconciliation resolution.
pub async fn resolutions(
    catalog: &TursoCatalog,
) -> Result<Vec<ReconciliationResolution>, RepositoryError> {
    collect(|page| {
        ReconciliationRepository::list_resolutions(
            catalog,
            ReconciliationResolutionFilter::default(),
            page,
        )
    })
    .await
}

/// Every reconciliation suppression.
pub async fn suppressions(
    catalog: &TursoCatalog,
) -> Result<Vec<ReconciliationSuppression>, RepositoryError> {
    collect(|page| {
        ReconciliationRepository::list_suppressions(
            catalog,
            ReconciliationResolutionFilter::default(),
            page,
        )
    })
    .await
}
