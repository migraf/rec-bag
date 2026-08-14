use super::*;
use futures::executor::block_on;
use rec_core::{
    ByteRange, ContentHash, DownloadRequest, InventoryMode, InventoryRequest, MusicAssetStore,
    UploadRequest,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::tempdir;

struct Tokens;
impl GoogleTokenProvider for Tokens {
    fn access_token(&self) -> BoxFuture<'_, Result<GoogleAccessToken, GoogleTokenError>> {
        Box::pin(async { Ok(GoogleAccessToken("fresh-token".to_owned())) })
    }
}

struct MockTransport {
    responses: Mutex<VecDeque<DriveHttpResponse>>,
    requests: Mutex<Vec<DriveHttpRequest>>,
}

impl MockTransport {
    fn new(responses: impl IntoIterator<Item = DriveHttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<DriveHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl DriveHttpTransport for MockTransport {
    fn execute(
        &self,
        request: DriveHttpRequest,
    ) -> BoxFuture<'_, Result<DriveHttpResponse, DriveTransportError>> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected Drive request");
        Box::pin(async move { Ok(response) })
    }
}

fn response(status: u16, json: &str) -> DriveHttpResponse {
    DriveHttpResponse {
        status,
        headers: BTreeMap::new(),
        body: json.as_bytes().to_vec(),
    }
}

fn response_with_header(status: u16, name: &str, value: &str, body: &[u8]) -> DriveHttpResponse {
    DriveHttpResponse {
        status,
        headers: BTreeMap::from([(name.to_owned(), value.to_owned())]),
        body: body.to_vec(),
    }
}

fn store(transport: MockTransport) -> GoogleDriveMusicStore<MockTransport, Tokens> {
    let mut config =
        GoogleDriveStoreConfig::new(Uuid::nil(), GoogleDriveFolder::new("restricted-root"));
    config.retry = RetryPolicy {
        max_attempts: 2,
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    };
    GoogleDriveMusicStore::new(config, transport, Tokens).unwrap()
}

#[test]
fn inventory_is_paginated_and_never_lists_outside_the_selected_folder() {
    let transport = MockTransport::new([
        response(
            200,
            r#"{"files":[{"id":"nested","name":"nested","mimeType":"application/vnd.google-apps.folder"},{"id":"first","name":"first.mp3","size":"1","parents":["restricted-root"]}]}"#,
        ),
        response(
            200,
            r#"{"files":[{"id":"second","name":"second.mp3","size":"2","parents":["nested"]}]}"#,
        ),
        response(200, r#"{"startPageToken":"changes-1"}"#),
    ]);
    let store = store(transport);
    let request = InventoryRequest {
        mode: InventoryMode::Full,
        page_cursor: None,
        page_size: Some(1),
        prefix: None,
        include_checksums: true,
    };
    let first = block_on(store.inventory_page(request)).unwrap();
    assert_eq!(
        first
            .objects
            .iter()
            .map(|object| object.object_id.as_str())
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert!(first.next_change_cursor.is_none());
    let second = block_on(store.inventory_page(InventoryRequest {
        mode: InventoryMode::Full,
        page_cursor: first.next_page_cursor,
        page_size: Some(1),
        prefix: None,
        include_checksums: true,
    }))
    .unwrap();
    assert_eq!(
        second
            .objects
            .iter()
            .map(|object| object.object_id.as_str())
            .collect::<Vec<_>>(),
        ["second"]
    );
    assert_eq!(second.next_change_cursor.unwrap().value, "changes-1");
    let requests = store.transport.requests();
    assert!(requests[0]
        .url
        .contains("%27restricted-root%27%20in%20parents"));
    assert!(requests
        .iter()
        .all(|request| !request.url.contains("supportsAllDrives=true")));
    assert!(requests
        .iter()
        .all(|request| request.headers.get("Authorization")
            == Some(&"Bearer fresh-token".to_owned())));
}

#[test]
fn download_is_blocked_before_content_request_when_drive_disallows_it() {
    let transport = MockTransport::new([response(
        200,
        r#"{"id":"private","name":"private.mp3","capabilities":{"canDownload":false}}"#,
    )]);
    let store = store(transport);
    let directory = tempdir().unwrap();
    let error = block_on(store.download(DownloadRequest {
        object_id: "private".to_owned(),
        destination_path: directory.path().join("private.mp3"),
        range: None,
        expected_checksum: None,
        overwrite: false,
    }))
    .unwrap_err();
    assert_eq!(error.kind, MusicStorageErrorKind::DownloadProhibited);
    assert_eq!(store.transport.requests().len(), 1);
}

#[test]
fn rate_limit_is_retried_with_a_fresh_token_request() {
    let transport = MockTransport::new([
        response(
            429,
            r#"{"error":{"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#,
        ),
        response(
            200,
            r#"{"id":"file","name":"file.mp3","size":"3","capabilities":{"canDownload":true}}"#,
        ),
    ]);
    let store = store(transport);
    let object = block_on(store.stat("file")).unwrap();
    assert_eq!(object.object_id, "file");
    assert_eq!(store.transport.requests().len(), 2);
}

#[test]
fn playback_cache_uses_verified_download_and_then_hits_locally() {
    let directory = tempdir().unwrap();
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let transport = MockTransport::new([
        response(
            200,
            &format!(
                r#"{{"id":"file","name":"file.mp3","sha256Checksum":"{expected}","capabilities":{{"canDownload":true}}}}"#
            ),
        ),
        DriveHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"abc".to_vec(),
        },
    ]);
    let mut config =
        GoogleDriveStoreConfig::new(Uuid::nil(), GoogleDriveFolder::new("restricted-root"));
    config.cache = Some(CacheConfig {
        directory: directory.path().join("cache"),
        max_bytes: 10,
    });
    config.retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    };
    let store = GoogleDriveMusicStore::new(config, transport, Tokens).unwrap();
    let path = block_on(store.cache_for_playback("file")).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    assert_eq!(store.cached_playback_path("file").unwrap(), Some(path));
    assert_eq!(store.transport.requests().len(), 2);
}

#[test]
fn api_pagination_resume_uses_one_page_token_and_deduplicates_ids() {
    let transport = MockTransport::new([
        response(
            200,
            r#"{"files":[{"id":"one","name":"same.mp3"}],"nextPageToken":"page-2"}"#,
        ),
        response(
            200,
            r#"{"files":[{"id":"one","name":"same.mp3"},{"id":"two","name":"same.mp3"}]}"#,
        ),
        response(200, r#"{"startPageToken":"changes"}"#),
    ]);
    let store = store(transport);
    let first = block_on(store.inventory_page(InventoryRequest {
        mode: InventoryMode::Full,
        page_cursor: None,
        page_size: Some(1),
        prefix: None,
        include_checksums: false,
    }))
    .unwrap();
    let second = block_on(store.inventory_page(InventoryRequest {
        mode: InventoryMode::Full,
        page_cursor: first.next_page_cursor,
        page_size: Some(10),
        prefix: None,
        include_checksums: false,
    }))
    .unwrap();
    assert_eq!(
        second
            .objects
            .iter()
            .map(|object| object.object_id.as_str())
            .collect::<Vec<_>>(),
        ["two"]
    );
    assert_eq!(second.objects[0].name, "same.mp3");
    let requests = store.transport.requests();
    assert_eq!(requests[1].url.matches("pageToken=").count(), 1);
    assert!(requests[1].url.contains("pageToken=page-2"));
}

#[test]
fn incremental_reports_moved_out_trashed_and_modified_files() {
    let transport = MockTransport::new([
        response(
            200,
            r#"{"changes":[
            {"fileId":"moved","time":"2025-01-01T00:00:00Z","file":{"id":"moved","name":"moved.mp3","version":"2","parents":["outside"]}},
            {"fileId":"trashed","file":{"id":"trashed","name":"trash.mp3","trashed":true,"version":"3"}},
            {"fileId":"modified","file":{"id":"modified","name":"modified.mp3","version":"4","parents":["restricted-root"]}}
        ],"newStartPageToken":"next"}"#,
        ),
        response(200, r#"{"id":"outside","name":"outside","parents":[]}"#),
    ]);
    let store = store(transport);
    let page = block_on(store.inventory_page(InventoryRequest {
        mode: InventoryMode::Incremental {
            change_cursor: SyncCursor::new("changes-1"),
        },
        page_cursor: None,
        page_size: Some(100),
        prefix: None,
        include_checksums: false,
    }))
    .unwrap();
    assert_eq!(page.objects[0].object_id, "modified");
    assert_eq!(
        page.removals
            .iter()
            .map(|item| item.object_id.as_str())
            .collect::<Vec<_>>(),
        ["moved", "trashed"]
    );
    assert_eq!(page.next_change_cursor.unwrap().value, "next");
    assert_eq!(
        store.transport.requests()[0]
            .url
            .matches("pageToken=")
            .count(),
        1
    );
}

#[test]
fn resumable_upload_recovers_partial_acknowledgement_and_verifies_hash() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.mp3");
    std::fs::write(&source, b"abcdef").unwrap();
    let transport = MockTransport::new([
        response_with_header(200, "Location", "https://upload/session", b""),
        response_with_header(308, "Range", "bytes=0-1", b""),
        response_with_header(308, "Range", "bytes=0-4", b""),
        response(
            200,
            r#"{"id":"uploaded","name":"source.mp3","size":"6","md5Checksum":"e80b5017098950fc58aad83c8c14978e"}"#,
        ),
    ]);
    let mut config =
        GoogleDriveStoreConfig::new(Uuid::nil(), GoogleDriveFolder::new("restricted-root"));
    config.upload_chunk_size = 3;
    config.retry.max_attempts = 1;
    let store = GoogleDriveMusicStore::new(config, transport, Tokens).unwrap();
    let object = block_on(store.upload(UploadRequest {
        source_path: source,
        object_path: "folder/source.mp3".to_owned(),
        content_type: Some("audio/mpeg".to_owned()),
        content_length: Some(6),
        expected_checksum: None,
        metadata: BTreeMap::from([
            ("track_id".to_owned(), Uuid::nil().to_string()),
            ("asset_id".to_owned(), Uuid::nil().to_string()),
        ]),
        idempotency_key: Some("upload-1".to_owned()),
    }))
    .unwrap();
    assert_eq!(object.object_id, "uploaded");
    let requests = store.transport.requests();
    assert_eq!(requests[1].body, b"abc");
    assert_eq!(requests[2].body, b"cde");
    assert_eq!(requests[3].body, b"f");
    assert_eq!(requests[3].headers["Content-Range"], "bytes 5-5/6");
    let metadata: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        metadata["appProperties"]["rec_bag_track_id"],
        Uuid::nil().to_string()
    );
}

#[test]
fn bad_source_hash_is_rejected_before_upload_session() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.mp3");
    std::fs::write(&source, b"abc").unwrap();
    let store = store(MockTransport::new([]));
    let error = block_on(store.upload(UploadRequest {
        source_path: source,
        object_path: "source.mp3".to_owned(),
        content_type: None,
        content_length: None,
        expected_checksum: Some(ContentHash::sha256("00".repeat(32)).unwrap()),
        metadata: BTreeMap::new(),
        idempotency_key: None,
    }))
    .unwrap_err();
    assert_eq!(error.kind, MusicStorageErrorKind::ChecksumMismatch);
    assert!(store.transport.requests().is_empty());
}

struct RefreshingTokens {
    tokens: Mutex<VecDeque<String>>,
    invalidated: Mutex<Vec<String>>,
}

impl GoogleTokenProvider for RefreshingTokens {
    fn access_token(&self) -> BoxFuture<'_, Result<GoogleAccessToken, GoogleTokenError>> {
        let token = self.tokens.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { Ok(GoogleAccessToken(token)) })
    }

    fn invalidate_token<'a>(
        &'a self,
        rejected: &'a GoogleAccessToken,
    ) -> BoxFuture<'a, Result<(), GoogleTokenError>> {
        self.invalidated.lock().unwrap().push(rejected.0.clone());
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn unauthorized_invalidates_token_and_retries_with_refresh() {
    let transport = MockTransport::new([
        response(401, r#"{"error":{"message":"expired"}}"#),
        response(200, r#"{"id":"file","name":"file.mp3"}"#),
    ]);
    let tokens = RefreshingTokens {
        tokens: Mutex::new(VecDeque::from(["expired".to_owned(), "fresh".to_owned()])),
        invalidated: Mutex::new(Vec::new()),
    };
    let mut config =
        GoogleDriveStoreConfig::new(Uuid::nil(), GoogleDriveFolder::new("restricted-root"));
    config.retry.max_attempts = 2;
    config.retry.initial_backoff = Duration::ZERO;
    let store = GoogleDriveMusicStore::new(config, transport, tokens).unwrap();
    assert_eq!(block_on(store.stat("file")).unwrap().object_id, "file");
    assert_eq!(
        store.tokens.invalidated.lock().unwrap().as_slice(),
        ["expired"]
    );
    let requests = store.transport.requests();
    assert_eq!(requests[0].headers["Authorization"], "Bearer expired");
    assert_eq!(requests[1].headers["Authorization"], "Bearer fresh");
}

#[test]
fn temporary_server_failure_is_retried() {
    let store = store(MockTransport::new([
        response(503, r#"{"error":{"message":"temporary"}}"#),
        response(200, r#"{"id":"file","name":"file.mp3"}"#),
    ]));
    assert!(block_on(store.stat("file")).is_ok());
    assert_eq!(store.transport.requests().len(), 2);
}

#[test]
fn ranged_download_uses_exclusive_end_and_requires_partial_content() {
    let directory = tempdir().unwrap();
    let ranged_store = store(MockTransport::new([
        response(
            200,
            r#"{"id":"file","name":"file.mp3","capabilities":{"canDownload":true}}"#,
        ),
        DriveHttpResponse {
            status: 206,
            headers: BTreeMap::new(),
            body: b"bcd".to_vec(),
        },
    ]));
    let result = block_on(ranged_store.download(DownloadRequest {
        object_id: "file".to_owned(),
        destination_path: directory.path().join("range.mp3"),
        range: Some(ByteRange {
            start: 1,
            end_exclusive: Some(4),
        }),
        expected_checksum: None,
        overwrite: false,
    }))
    .unwrap();
    assert_eq!(std::fs::read(result.destination_path).unwrap(), b"bcd");
    assert!(!result.checksum_verified);
    assert_eq!(
        ranged_store.transport.requests()[1].headers["Range"],
        "bytes=1-3"
    );

    let ignored = store(MockTransport::new([
        response(
            200,
            r#"{"id":"file","name":"file.mp3","capabilities":{"canDownload":true}}"#,
        ),
        DriveHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"abcdef".to_vec(),
        },
    ]));
    let error = block_on(ignored.download(DownloadRequest {
        object_id: "file".to_owned(),
        destination_path: directory.path().join("ignored.mp3"),
        range: Some(ByteRange {
            start: 1,
            end_exclusive: None,
        }),
        expected_checksum: None,
        overwrite: false,
    }))
    .unwrap_err();
    assert_eq!(error.kind, MusicStorageErrorKind::Provider);
}

#[test]
fn cache_keys_eviction_pinning_and_corruption_recovery_are_safe() {
    let directory = tempdir().unwrap();
    let cache = crate::cache::PlaybackCache::new(CacheConfig {
        directory: directory.path().join("cache"),
        max_bytes: 6,
    })
    .unwrap();
    let hash = |bytes: &[u8]| {
        use sha2::Digest;
        ContentHash::sha256(format!("{:x}", sha2::Sha256::digest(bytes))).unwrap()
    };
    let first = cache.path_for("a/b");
    assert_ne!(first, cache.path_for("a?b"));
    std::fs::write(&first, b"aaa").unwrap();
    cache.record("a/b", &first, 3, hash(b"aaa")).unwrap();
    cache.set_pinned("a/b", true).unwrap();
    let second = cache.path_for("second");
    std::fs::write(&second, b"bbb").unwrap();
    cache.record("second", &second, 3, hash(b"bbb")).unwrap();
    let third = cache.path_for("third");
    std::fs::write(&third, b"ccc").unwrap();
    cache.record("third", &third, 3, hash(b"ccc")).unwrap();
    assert!(cache.cached_path("a/b").unwrap().is_some());
    assert!(cache.cached_path("second").unwrap().is_none());
    std::fs::write(&first, b"bad").unwrap();
    assert!(cache.cached_path("a/b").unwrap().is_none());
    std::fs::write(directory.path().join("cache/cache-index.json"), b"not-json").unwrap();
    assert!(cache.cached_path("third").unwrap().is_none());
}

#[test]
fn download_verifies_provider_md5_and_removes_corrupt_temporary_file() {
    let directory = tempdir().unwrap();
    let store = store(MockTransport::new([
        response(
            200,
            r#"{"id":"file","name":"file.mp3","md5Checksum":"900150983cd24fb0d6963f7d28e17f72","capabilities":{"canDownload":true}}"#,
        ),
        DriveHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"abd".to_vec(),
        },
    ]));
    let destination = directory.path().join("file.mp3");
    let error = block_on(store.download(DownloadRequest {
        object_id: "file".to_owned(),
        destination_path: destination.clone(),
        range: None,
        expected_checksum: None,
        overwrite: false,
    }))
    .unwrap_err();
    assert_eq!(error.kind, MusicStorageErrorKind::ChecksumMismatch);
    assert!(!destination.exists());
}

#[test]
fn satisfies_shared_music_asset_store_contract() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source.mp3");
    std::fs::write(&source, b"abc").unwrap();
    let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let transport = MockTransport::new([
        response(200, r#"{"user":{"permissionId":"user"}}"#),
        response_with_header(200, "Location", "https://upload/session", b""),
        response(
            200,
            &format!(r#"{{"id":"file","name":"source.mp3","size":"3","sha256Checksum":"{sha}"}}"#),
        ),
        response(
            200,
            &format!(
                r#"{{"id":"file","name":"source.mp3","size":"3","sha256Checksum":"{sha}","capabilities":{{"canDownload":true}}}}"#
            ),
        ),
        response(
            200,
            &format!(
                r#"{{"files":[{{"id":"file","name":"source.mp3","size":"3","sha256Checksum":"{sha}"}}]}}"#
            ),
        ),
        response(200, r#"{"startPageToken":"changes"}"#),
        response(
            200,
            &format!(
                r#"{{"id":"file","name":"source.mp3","size":"3","sha256Checksum":"{sha}","capabilities":{{"canDownload":true}}}}"#
            ),
        ),
        DriveHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"abc".to_vec(),
        },
    ]);
    let store: Box<dyn MusicAssetStore> = Box::new(store(transport));
    assert_eq!(
        block_on(store.health()).unwrap().status,
        HealthStatus::Healthy
    );
    let uploaded = block_on(store.upload(UploadRequest {
        source_path: source,
        object_path: "source.mp3".to_owned(),
        content_type: Some("audio/mpeg".to_owned()),
        content_length: Some(3),
        expected_checksum: Some(ContentHash::sha256(sha).unwrap()),
        metadata: BTreeMap::new(),
        idempotency_key: Some("contract".to_owned()),
    }))
    .unwrap();
    assert_eq!(
        block_on(store.stat("file")).unwrap().object_id,
        uploaded.object_id
    );
    let inventory = block_on(store.inventory_page(InventoryRequest {
        mode: InventoryMode::Full,
        page_cursor: None,
        page_size: Some(10),
        prefix: None,
        include_checksums: true,
    }))
    .unwrap();
    assert_eq!(inventory.objects[0].object_id, "file");
    let destination = directory.path().join("download.mp3");
    let downloaded = block_on(store.download(DownloadRequest {
        object_id: "file".to_owned(),
        destination_path: destination.clone(),
        range: None,
        expected_checksum: Some(ContentHash::sha256(sha).unwrap()),
        overwrite: false,
    }))
    .unwrap();
    assert!(downloaded.checksum_verified);
    assert_eq!(std::fs::read(destination).unwrap(), b"abc");
}
