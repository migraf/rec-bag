//! Reusable provider contract fakes and assertions.
//!
//! The fakes record requests and return queued results in FIFO order. They mirror
//! the provider contracts exactly: destructive deletion is intentionally absent
//! from [`MusicAssetStore`] and therefore from this test-support API.

use crate::asset::StorageObject;
use crate::provider::{
    BoxFuture, CatalogAssetReadRequest, CatalogAssetReadResult, CatalogAssetReader,
    CatalogSourceConnector, DownloadRequest, DownloadResult, InventoryPage, InventoryRequest,
    MusicAssetStore, MusicStorageError, MusicStorageErrorKind, SourceError, SourceErrorKind,
    SourceHealth, SourceSyncPage, SourceSyncRequest, StorageHealth, StorageVersionPage,
    StorageVersionRequest, UploadRequest,
};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn missing_source_result<T>(operation: &str) -> Result<T, SourceError> {
    Err(SourceError::new(
        SourceErrorKind::Permanent,
        format!("no fake result queued for {operation}"),
    ))
}

fn missing_storage_result<T>(operation: &str) -> Result<T, MusicStorageError> {
    Err(MusicStorageError::new(
        MusicStorageErrorKind::Permanent,
        format!("no fake result queued for {operation}"),
    ))
}

#[derive(Debug, Default)]
struct CatalogState {
    health_results: VecDeque<Result<SourceHealth, SourceError>>,
    sync_page_results: VecDeque<Result<SourceSyncPage, SourceError>>,
    health_calls: usize,
    sync_requests: VecDeque<SourceSyncRequest>,
}

/// Thread-safe, FIFO fake for [`CatalogSourceConnector`].
#[derive(Debug, Default)]
pub struct FakeCatalogSourceConnector {
    state: Mutex<CatalogState>,
}

impl FakeCatalogSourceConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue_health_result(&self, result: Result<SourceHealth, SourceError>) {
        lock(&self.state).health_results.push_back(result);
    }

    pub fn queue_sync_page_result(&self, result: Result<SourceSyncPage, SourceError>) {
        lock(&self.state).sync_page_results.push_back(result);
    }

    pub fn health_call_count(&self) -> usize {
        lock(&self.state).health_calls
    }

    pub fn sync_requests(&self) -> Vec<SourceSyncRequest> {
        lock(&self.state).sync_requests.iter().cloned().collect()
    }
}

impl CatalogSourceConnector for FakeCatalogSourceConnector {
    fn health(&self) -> BoxFuture<'_, Result<SourceHealth, SourceError>> {
        let result = {
            let mut state = lock(&self.state);
            state.health_calls += 1;
            state
                .health_results
                .pop_front()
                .unwrap_or_else(|| missing_source_result("health"))
        };

        Box::pin(async move { result })
    }

    fn sync_page(
        &self,
        request: SourceSyncRequest,
    ) -> BoxFuture<'_, Result<SourceSyncPage, SourceError>> {
        let result = {
            let mut state = lock(&self.state);
            state.sync_requests.push_back(request);
            state
                .sync_page_results
                .pop_front()
                .unwrap_or_else(|| missing_source_result("sync_page"))
        };

        Box::pin(async move { result })
    }
}

#[derive(Debug, Default)]
struct CatalogAssetState {
    read_results: VecDeque<Result<CatalogAssetReadResult, SourceError>>,
    read_requests: VecDeque<CatalogAssetReadRequest>,
}

/// Thread-safe, FIFO fake for legitimately obtainable source files.
#[derive(Debug, Default)]
pub struct FakeCatalogAssetReader {
    state: Mutex<CatalogAssetState>,
}

impl FakeCatalogAssetReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue_read_result(&self, result: Result<CatalogAssetReadResult, SourceError>) {
        lock(&self.state).read_results.push_back(result);
    }

    pub fn read_requests(&self) -> Vec<CatalogAssetReadRequest> {
        lock(&self.state).read_requests.iter().cloned().collect()
    }
}

impl CatalogAssetReader for FakeCatalogAssetReader {
    fn read_asset(
        &self,
        request: CatalogAssetReadRequest,
    ) -> BoxFuture<'_, Result<CatalogAssetReadResult, SourceError>> {
        let result = {
            let mut state = lock(&self.state);
            state.read_requests.push_back(request);
            state
                .read_results
                .pop_front()
                .unwrap_or_else(|| missing_source_result("read_asset"))
        };

        Box::pin(async move { result })
    }
}

#[derive(Debug, Default)]
struct StorageState {
    health_results: VecDeque<Result<StorageHealth, MusicStorageError>>,
    inventory_page_results: VecDeque<Result<InventoryPage, MusicStorageError>>,
    stat_results: VecDeque<Result<StorageObject, MusicStorageError>>,
    version_page_results: VecDeque<Result<StorageVersionPage, MusicStorageError>>,
    upload_results: VecDeque<Result<StorageObject, MusicStorageError>>,
    download_results: VecDeque<Result<DownloadResult, MusicStorageError>>,
    health_calls: usize,
    inventory_requests: VecDeque<InventoryRequest>,
    stat_requests: VecDeque<String>,
    version_requests: VecDeque<StorageVersionRequest>,
    upload_requests: VecDeque<UploadRequest>,
    download_requests: VecDeque<DownloadRequest>,
}

/// Thread-safe, FIFO fake for [`MusicAssetStore`].
///
/// There is deliberately no delete operation: destructive cleanup is outside the
/// storage contract until a retention and confirmation contract is introduced.
#[derive(Debug, Default)]
pub struct FakeMusicAssetStore {
    state: Mutex<StorageState>,
}

impl FakeMusicAssetStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn queue_health_result(&self, result: Result<StorageHealth, MusicStorageError>) {
        lock(&self.state).health_results.push_back(result);
    }

    pub fn queue_inventory_page_result(&self, result: Result<InventoryPage, MusicStorageError>) {
        lock(&self.state).inventory_page_results.push_back(result);
    }

    pub fn queue_stat_result(&self, result: Result<StorageObject, MusicStorageError>) {
        lock(&self.state).stat_results.push_back(result);
    }

    pub fn queue_version_page_result(&self, result: Result<StorageVersionPage, MusicStorageError>) {
        lock(&self.state).version_page_results.push_back(result);
    }

    pub fn queue_upload_result(&self, result: Result<StorageObject, MusicStorageError>) {
        lock(&self.state).upload_results.push_back(result);
    }

    pub fn queue_download_result(&self, result: Result<DownloadResult, MusicStorageError>) {
        lock(&self.state).download_results.push_back(result);
    }

    pub fn health_call_count(&self) -> usize {
        lock(&self.state).health_calls
    }

    pub fn inventory_requests(&self) -> Vec<InventoryRequest> {
        lock(&self.state)
            .inventory_requests
            .iter()
            .cloned()
            .collect()
    }

    pub fn stat_requests(&self) -> Vec<String> {
        lock(&self.state).stat_requests.iter().cloned().collect()
    }

    pub fn version_requests(&self) -> Vec<StorageVersionRequest> {
        lock(&self.state).version_requests.iter().cloned().collect()
    }

    pub fn upload_requests(&self) -> Vec<UploadRequest> {
        lock(&self.state).upload_requests.iter().cloned().collect()
    }

    pub fn download_requests(&self) -> Vec<DownloadRequest> {
        lock(&self.state)
            .download_requests
            .iter()
            .cloned()
            .collect()
    }
}

impl MusicAssetStore for FakeMusicAssetStore {
    fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.health_calls += 1;
            state
                .health_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("health"))
        };

        Box::pin(async move { result })
    }

    fn inventory_page(
        &self,
        request: InventoryRequest,
    ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.inventory_requests.push_back(request);
            state
                .inventory_page_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("inventory_page"))
        };

        Box::pin(async move { result })
    }

    fn stat<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.stat_requests.push_back(object_id.to_owned());
            state
                .stat_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("stat"))
        };

        Box::pin(async move { result })
    }

    fn versions_page(
        &self,
        request: StorageVersionRequest,
    ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.version_requests.push_back(request);
            state
                .version_page_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("versions_page"))
        };

        Box::pin(async move { result })
    }

    fn upload(
        &self,
        request: UploadRequest,
    ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.upload_requests.push_back(request);
            state
                .upload_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("upload"))
        };

        Box::pin(async move { result })
    }

    fn download(
        &self,
        request: DownloadRequest,
    ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
        let result = {
            let mut state = lock(&self.state);
            state.download_requests.push_back(request);
            state
                .download_results
                .pop_front()
                .unwrap_or_else(|| missing_storage_result("download"))
        };

        Box::pin(async move { result })
    }
}

/// Assert that a catalog connector's health and one paged sync operation succeed.
///
/// The returned values remain available for contract-specific assertions.
pub async fn assert_catalog_source_connector_contract<C>(
    connector: &C,
    request: SourceSyncRequest,
) -> (SourceHealth, SourceSyncPage)
where
    C: CatalogSourceConnector + ?Sized,
{
    let health = connector
        .health()
        .await
        .expect("catalog connector health check should succeed");
    let page = connector
        .sync_page(request)
        .await
        .expect("catalog connector paged sync should succeed");
    (health, page)
}

/// Assert that a catalog asset reader can materialize one legitimately obtainable file.
pub async fn assert_catalog_asset_reader_contract<R>(
    reader: &R,
    request: CatalogAssetReadRequest,
) -> CatalogAssetReadResult
where
    R: CatalogAssetReader + ?Sized,
{
    reader
        .read_asset(request)
        .await
        .expect("catalog asset read should succeed")
}

/// Assert that an asset store's health and one paged inventory operation succeed.
///
/// Destructive deletion is intentionally not asserted because it is absent from
/// [`MusicAssetStore`] by design. The returned values remain available for
/// contract-specific assertions.
pub async fn assert_music_asset_store_contract<S>(
    store: &S,
    request: InventoryRequest,
) -> (StorageHealth, InventoryPage)
where
    S: MusicAssetStore + ?Sized,
{
    let health = store
        .health()
        .await
        .expect("music asset store health check should succeed");
    let page = store
        .inventory_page(request)
        .await
        .expect("music asset store paged inventory should succeed");
    (health, page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::StorageCapabilities;
    use crate::catalog::{SourceCapabilities, SourceTrackIdentity};
    use crate::provider::{HealthStatus, InventoryMode, SourceSyncMode, StorageObjectVersion};
    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use uuid::Uuid;

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn timestamp() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("Unix epoch should be valid")
    }

    fn source_health() -> SourceHealth {
        SourceHealth {
            status: HealthStatus::Healthy,
            checked_at: timestamp(),
            message: None,
            capabilities: SourceCapabilities::default(),
        }
    }

    fn storage_health() -> StorageHealth {
        StorageHealth {
            status: HealthStatus::Healthy,
            checked_at: timestamp(),
            message: None,
            capabilities: StorageCapabilities::default(),
        }
    }

    fn storage_object(object_id: &str) -> StorageObject {
        StorageObject {
            storage_id: Uuid::nil(),
            object_id: object_id.to_owned(),
            name: format!("{object_id}.wav"),
            display_path: None,
            size_bytes: 4,
            media_type: Some("audio/wav".to_owned()),
            modified_at: Some(timestamp()),
            provider_version: None,
            hashes: Vec::new(),
            embedded_asset_id: None,
            is_trashed: false,
        }
    }

    #[test]
    fn catalog_fake_returns_fifo_results_and_records_requests() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FakeCatalogSourceConnector>();

        let fake = FakeCatalogSourceConnector::new();
        let health = source_health();
        let page = SourceSyncPage {
            items: Vec::new(),
            removals: Vec::new(),
            next_page_cursor: None,
            next_sync_cursor: None,
            progress: None,
        };
        let request = SourceSyncRequest {
            mode: SourceSyncMode::Full,
            collections: Vec::new(),
            page_cursor: None,
            page_size: Some(25),
        };
        fake.queue_health_result(Ok(health.clone()));
        fake.queue_sync_page_result(Ok(page.clone()));

        let actual = block_on(assert_catalog_source_connector_contract(
            &fake,
            request.clone(),
        ));

        assert_eq!(actual, (health, page));
        assert_eq!(fake.health_call_count(), 1);
        assert_eq!(fake.sync_requests(), vec![request]);
    }

    #[test]
    fn catalog_asset_fake_records_obtainable_file_reads() {
        let fake = FakeCatalogAssetReader::new();
        let request = CatalogAssetReadRequest {
            track: SourceTrackIdentity::new(Uuid::new_v4(), "track-1"),
            destination_path: PathBuf::from("staging.wav"),
            expected_checksum: None,
        };
        let result = CatalogAssetReadResult {
            destination_path: request.destination_path.clone(),
            bytes_written: 4,
            checksum: None,
        };
        fake.queue_read_result(Ok(result.clone()));

        let actual = block_on(assert_catalog_asset_reader_contract(&fake, request.clone()));

        assert_eq!(actual, result);
        assert_eq!(fake.read_requests(), vec![request]);
    }

    #[test]
    fn storage_fake_supports_all_non_destructive_operations() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FakeMusicAssetStore>();

        let fake = FakeMusicAssetStore::new();
        let object = storage_object("object-1");
        let inventory = InventoryPage {
            objects: vec![object.clone()],
            removals: Vec::new(),
            next_page_cursor: None,
            next_change_cursor: None,
            progress: None,
        };
        let inventory_request = InventoryRequest {
            mode: InventoryMode::Full,
            page_cursor: None,
            page_size: Some(10),
            prefix: Some("music".to_owned()),
            include_checksums: true,
        };
        let version_request = StorageVersionRequest {
            object_id: object.object_id.clone(),
            page_cursor: None,
            page_size: Some(10),
        };
        let version_page = StorageVersionPage {
            versions: vec![StorageObjectVersion {
                object_id: object.object_id.clone(),
                version_id: "v1".to_owned(),
                modified_at: Some(timestamp()),
                size_bytes: Some(4),
                hashes: Vec::new(),
                is_current: true,
            }],
            next_page_cursor: None,
        };
        let upload_request = UploadRequest {
            source_path: PathBuf::from("staging.wav"),
            object_path: "music/object-1.wav".to_owned(),
            content_type: Some("audio/wav".to_owned()),
            content_length: Some(4),
            expected_checksum: None,
            metadata: BTreeMap::new(),
            idempotency_key: Some("upload-1".to_owned()),
        };
        let download_request = DownloadRequest {
            object_id: object.object_id.clone(),
            destination_path: PathBuf::from("download.wav"),
            range: None,
            expected_checksum: None,
            overwrite: false,
        };
        let download = DownloadResult {
            object: object.clone(),
            destination_path: download_request.destination_path.clone(),
            bytes_written: 4,
            checksum: None,
            checksum_verified: false,
        };

        fake.queue_health_result(Ok(storage_health()));
        fake.queue_inventory_page_result(Ok(inventory));
        fake.queue_stat_result(Ok(object.clone()));
        fake.queue_version_page_result(Ok(version_page.clone()));
        fake.queue_upload_result(Ok(object.clone()));
        fake.queue_download_result(Ok(download.clone()));

        block_on(assert_music_asset_store_contract(
            &fake,
            inventory_request.clone(),
        ));
        assert_eq!(block_on(fake.stat("object-1")), Ok(object.clone()));
        assert_eq!(
            block_on(fake.versions_page(version_request.clone())),
            Ok(version_page)
        );
        assert_eq!(block_on(fake.upload(upload_request.clone())), Ok(object));
        assert_eq!(
            block_on(fake.download(download_request.clone())),
            Ok(download)
        );

        assert_eq!(fake.health_call_count(), 1);
        assert_eq!(fake.inventory_requests(), vec![inventory_request]);
        assert_eq!(fake.stat_requests(), vec!["object-1".to_owned()]);
        assert_eq!(fake.version_requests(), vec![version_request]);
        assert_eq!(fake.upload_requests(), vec![upload_request]);
        assert_eq!(fake.download_requests(), vec![download_request]);
    }
}
