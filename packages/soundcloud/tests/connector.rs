use rec_core::{
    BoxFuture, CatalogSourceConnector, MetadataValue, SourceAvailability, SourceCollection,
    SourceError, SourceErrorKind, SourceSyncItem, SourceSyncMode, SourceSyncPage,
    SourceSyncRequest,
};
use soundcloud::{
    ReqwestSoundCloudTransport, SoundCloudClient, SoundCloudConnector, SoundCloudHttpRequest,
    SoundCloudHttpResponse, SoundCloudHttpTransport, SoundCloudJitterSource, SoundCloudRetryPolicy,
    SoundCloudSleeper, SoundCloudTokenProvider, SoundCloudTransportError,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const API_URL: &str = "https://api.soundcloud.com/";
const SOURCE_ID: Uuid = Uuid::from_u128(7);

#[derive(Default)]
struct MockTransport {
    responses: Mutex<VecDeque<Result<SoundCloudHttpResponse, SoundCloudTransportError>>>,
    requests: Mutex<Vec<SoundCloudHttpRequest>>,
}

impl MockTransport {
    fn with_responses(responses: impl IntoIterator<Item = SoundCloudHttpResponse>) -> Self {
        Self::with_results(responses.into_iter().map(Ok))
    }

    fn with_results(
        responses: impl IntoIterator<Item = Result<SoundCloudHttpResponse, SoundCloudTransportError>>,
    ) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<SoundCloudHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl SoundCloudHttpTransport for MockTransport {
    fn execute(
        &self,
        request: SoundCloudHttpRequest,
    ) -> BoxFuture<'_, Result<SoundCloudHttpResponse, SoundCloudTransportError>> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected SoundCloud HTTP request");
        Box::pin(async move { response })
    }
}

struct MockTokens {
    access: String,
    refreshed: String,
    refresh_calls: Mutex<u32>,
}

impl MockTokens {
    fn new(access: &str, refreshed: &str) -> Self {
        Self {
            access: access.to_string(),
            refreshed: refreshed.to_string(),
            refresh_calls: Mutex::new(0),
        }
    }

    fn refresh_calls(&self) -> u32 {
        *self.refresh_calls.lock().unwrap()
    }
}

impl SoundCloudTokenProvider for MockTokens {
    fn access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        let token = self.access.clone();
        Box::pin(async move { Ok(token) })
    }

    fn refresh_access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        *self.refresh_calls.lock().unwrap() += 1;
        let token = self.refreshed.clone();
        Box::pin(async move { Ok(token) })
    }
}

#[derive(Default)]
struct RecordingSleeper(Mutex<Vec<Duration>>);

impl RecordingSleeper {
    fn delays(&self) -> Vec<Duration> {
        self.0.lock().unwrap().clone()
    }
}

impl SoundCloudSleeper for RecordingSleeper {
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
        self.0.lock().unwrap().push(duration);
        Box::pin(async {})
    }
}

struct SequenceJitter(Mutex<VecDeque<Duration>>);

impl SequenceJitter {
    fn new(values: impl IntoIterator<Item = Duration>) -> Self {
        Self(Mutex::new(values.into_iter().collect()))
    }
}

impl SoundCloudJitterSource for SequenceJitter {
    fn jitter(&self, upper_bound: Duration) -> Duration {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected jitter request")
            .min(upper_bound)
    }
}

fn response(status: u16, body: &str) -> SoundCloudHttpResponse {
    SoundCloudHttpResponse {
        status,
        headers: BTreeMap::new(),
        body: body.to_string(),
    }
}

fn connector(
    transport: Arc<MockTransport>,
    tokens: Arc<MockTokens>,
    sleeper: Arc<RecordingSleeper>,
    retry_policy: SoundCloudRetryPolicy,
) -> SoundCloudConnector {
    let client =
        SoundCloudClient::with_options(transport, tokens, sleeper, retry_policy, API_URL).unwrap();
    SoundCloudConnector::new(SOURCE_ID, client)
}

fn full_request(collections: Vec<SourceCollection>) -> SourceSyncRequest {
    SourceSyncRequest {
        mode: SourceSyncMode::Full,
        collections,
        page_cursor: None,
        page_size: Some(25),
    }
}

async fn complete_sync(
    connector: &SoundCloudConnector,
    mut request: SourceSyncRequest,
) -> Vec<SourceSyncPage> {
    let mut pages = Vec::new();
    loop {
        let page = connector.sync_page(request.clone()).await.unwrap();
        request.page_cursor = page.next_page_cursor.clone();
        let complete = request.page_cursor.is_none();
        pages.push(page);
        if complete {
            return pages;
        }
    }
}

#[tokio::test]
async fn satisfies_the_shared_catalog_connector_contract() {
    let transport = Arc::new(MockTransport::with_responses([
        response(200, include_str!("fixtures/me_current.json")),
        response(200, include_str!("fixtures/me_current.json")),
        response(200, "{\"collection\": [], \"next_href\": null}"),
    ]));
    let connector = connector(
        transport,
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
    );

    let (health, page) = rec_core::test_support::assert_catalog_source_connector_contract(
        &connector,
        full_request(vec![SourceCollection::UploadedTracks]),
    )
    .await;
    assert_eq!(health.status, rec_core::HealthStatus::Healthy);
    assert!(page.next_sync_cursor.is_some());
}

#[tokio::test]
async fn exhaustive_fixture_sync_covers_collections_access_order_and_duplicates() {
    let transport = Arc::new(MockTransport::with_responses([
        response(200, include_str!("fixtures/me_current.json")),
        response(200, include_str!("fixtures/uploads_page_one_current.json")),
        response(200, include_str!("fixtures/uploads_page_two_current.json")),
        response(200, include_str!("fixtures/owned_playlists_current.json")),
        response(200, include_str!("fixtures/liked_tracks_current.json")),
        response(200, include_str!("fixtures/liked_playlists_current.json")),
    ]));
    let connector = connector(
        transport.clone(),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
    );

    let pages = complete_sync(&connector, full_request(Vec::new())).await;
    assert_eq!(pages.len(), 5);
    assert!(pages.last().unwrap().next_sync_cursor.is_some());

    let items: Vec<_> = pages.iter().flat_map(|page| page.items.iter()).collect();
    let tracks: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            SourceSyncItem::Track(track) => Some(track),
            _ => None,
        })
        .collect();
    let ids: HashSet<_> = tracks
        .iter()
        .map(|track| track.identity.external_id.as_str())
        .collect();
    assert_eq!(tracks.len(), 9, "track DTOs are emitted once per full run");
    assert_eq!(ids.len(), 9);
    assert_eq!(
        tracks
            .iter()
            .filter(|track| track.identity.external_id == "soundcloud:tracks:10")
            .count(),
        1,
        "the same track across uploads, likes, and playlists is one occurrence"
    );

    let availability = |id: &str| {
        tracks
            .iter()
            .find(|track| track.identity.external_id == id)
            .unwrap()
            .availability
    };
    assert_eq!(
        availability("soundcloud:tracks:10"),
        SourceAvailability::Obtainable
    );
    assert_eq!(
        availability("soundcloud:tracks:20"),
        SourceAvailability::PreviewOnly
    );
    assert_eq!(
        availability("soundcloud:tracks:30"),
        SourceAvailability::Inaccessible
    );
    assert_eq!(
        availability("soundcloud:tracks:40"),
        SourceAvailability::Removed
    );
    assert_eq!(
        availability("soundcloud:tracks:50"),
        SourceAvailability::Blocked
    );
    assert_eq!(
        availability("soundcloud:tracks:60"),
        SourceAvailability::StreamingOnly
    );

    let private_track = tracks
        .iter()
        .find(|track| track.identity.external_id == "soundcloud:tracks:10")
        .unwrap();
    assert_eq!(
        private_track.provider_metadata.get("sharing"),
        Some(&MetadataValue::Text("private".to_string()))
    );
    assert!(private_track
        .provider_metadata
        .contains_key("future_provider_field"));
    assert!(!private_track.provider_metadata.contains_key("secret_token"));

    let owned = items
        .iter()
        .find_map(|item| match item {
            SourceSyncItem::Playlist(playlist)
                if playlist.external_id == "soundcloud:playlists:100" =>
            {
                Some(playlist)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        owned
            .entries
            .iter()
            .map(|entry| (entry.position, entry.track.external_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (0, "soundcloud:tracks:50"),
            (1, "soundcloud:tracks:10"),
            (2, "soundcloud:tracks:50")
        ]
    );
    assert_eq!(
        owned.provider_metadata.get("sharing"),
        Some(&MetadataValue::Text("private".to_string()))
    );

    let liked = items
        .iter()
        .find_map(|item| match item {
            SourceSyncItem::Playlist(playlist)
                if playlist.external_id == "soundcloud:playlists:200" =>
            {
                Some(playlist)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(liked.entries.len(), 4);
    assert!(items.iter().any(|item| matches!(
        item,
        SourceSyncItem::Relationship(relationship)
            if relationship.identity.collection == SourceCollection::LikedTracks
                && relationship.identity.entity_external_id == "soundcloud:tracks:60"
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        SourceSyncItem::Relationship(relationship)
            if relationship.identity.collection == SourceCollection::LikedPlaylists
                && relationship.identity.parent_external_id.as_deref() == Some("soundcloud:playlists:200")
                && relationship.identity.position == Some(3)
    )));

    let removed: HashSet<_> = pages
        .iter()
        .flat_map(|page| page.removals.iter())
        .map(|removal| removal.external_id.as_str())
        .collect();
    assert_eq!(
        removed,
        HashSet::from(["soundcloud:tracks:40", "soundcloud:tracks:90"])
    );

    let urls: Vec<_> = transport
        .requests()
        .into_iter()
        .map(|request| request.url)
        .collect();
    assert!(urls[1].contains("/me/tracks?"));
    assert_eq!(
        urls[2],
        "https://api.soundcloud.com/me/tracks?linked_partitioning=true&limit=2&cursor=uploads-2"
    );
    assert!(urls[3].contains("/me/playlists?"));
    assert!(urls[4].contains("/me/likes/tracks?"));
    assert!(urls[5].contains("/me/likes/playlists?"));
    assert!(urls[3].contains("show_tracks=true"));
    assert!(urls[5].contains("show_tracks=true"));
}

#[tokio::test]
async fn partial_resume_is_stateless_and_retrying_a_checkpoint_is_exactly_idempotent() {
    let first_transport = Arc::new(MockTransport::with_responses([
        response(200, include_str!("fixtures/me_current.json")),
        response(200, include_str!("fixtures/uploads_page_one_current.json")),
    ]));
    let first_connector = connector(
        first_transport,
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
    );
    let first = first_connector
        .sync_page(full_request(vec![SourceCollection::UploadedTracks]))
        .await
        .unwrap();
    let checkpoint = first.next_page_cursor.unwrap();

    let resumed_transport = Arc::new(MockTransport::with_responses([
        response(200, include_str!("fixtures/uploads_page_two_current.json")),
        response(200, include_str!("fixtures/uploads_page_two_current.json")),
    ]));
    let resumed_connector = connector(
        resumed_transport.clone(),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
    );
    let resumed_request = SourceSyncRequest {
        page_cursor: Some(checkpoint),
        ..full_request(vec![SourceCollection::UploadedTracks])
    };
    let completed = resumed_connector
        .sync_page(resumed_request.clone())
        .await
        .unwrap();
    let retried = resumed_connector.sync_page(resumed_request).await.unwrap();

    assert_eq!(completed, retried);
    assert!(completed.next_sync_cursor.is_some());
    assert_eq!(resumed_transport.requests().len(), 2);
    assert!(resumed_transport
        .requests()
        .iter()
        .all(|request| !request.url.ends_with("/me")));
}

#[tokio::test]
async fn incremental_snapshot_diff_reports_removals_and_repeated_sync_is_stable() {
    let first_body = r#"{
        "collection": [
            {"urn":"soundcloud:tracks:1","access":"playable"},
            {"urn":"soundcloud:tracks:2","access":"playable"}
        ], "next_href": null
    }"#;
    let current_body = r#"{
        "collection": [
            {"urn":"soundcloud:tracks:2","access":"playable"},
            {"urn":"soundcloud:tracks:3","access":"preview"}
        ], "next_href": null
    }"#;
    let make_connector = |body: &'static str| {
        connector(
            Arc::new(MockTransport::with_responses([
                response(200, include_str!("fixtures/me_current.json")),
                response(200, body),
            ])),
            Arc::new(MockTokens::new("access", "rotated")),
            Arc::new(RecordingSleeper::default()),
            SoundCloudRetryPolicy::default(),
        )
    };

    let initial = make_connector(first_body)
        .sync_page(full_request(vec![SourceCollection::LikedTracks]))
        .await
        .unwrap();
    let first_cursor = initial.next_sync_cursor.unwrap();
    let incremental_request = SourceSyncRequest {
        mode: SourceSyncMode::Incremental {
            cursor: first_cursor,
        },
        ..full_request(vec![SourceCollection::LikedTracks])
    };
    let changed = make_connector(current_body)
        .sync_page(incremental_request)
        .await
        .unwrap();
    assert!(changed.items.iter().any(|item| matches!(
        item,
        SourceSyncItem::RelationshipRemoval(identity)
            if identity.collection == SourceCollection::LikedTracks
                && identity.entity_external_id == "soundcloud:tracks:1"
    )));

    let changed_ids: HashSet<_> = changed
        .items
        .iter()
        .filter_map(|item| match item {
            SourceSyncItem::Track(track) => Some(track.identity.external_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        changed_ids,
        HashSet::from(["soundcloud:tracks:2", "soundcloud:tracks:3"])
    );

    let repeated = make_connector(current_body)
        .sync_page(SourceSyncRequest {
            mode: SourceSyncMode::Incremental {
                cursor: changed.next_sync_cursor.unwrap(),
            },
            ..full_request(vec![SourceCollection::LikedTracks])
        })
        .await
        .unwrap();
    assert!(!repeated
        .items
        .iter()
        .any(|item| matches!(item, SourceSyncItem::RelationshipRemoval(_))));
    assert!(repeated.removals.is_empty());
}

#[tokio::test]
async fn playlist_snapshot_diff_authoritatively_replaces_ordered_entry_relationships() {
    let old = r#"{"collection":[{"urn":"soundcloud:playlists:9","title":"Set","last_modified":"one","tracks":[{"urn":"soundcloud:tracks:1"},{"urn":"soundcloud:tracks:2"}]}],"next_href":null}"#;
    let new = r#"{"collection":[{"urn":"soundcloud:playlists:9","title":"Set","last_modified":"two","tracks":[{"urn":"soundcloud:tracks:2"}]}],"next_href":null}"#;
    let run = |body: &'static str| {
        connector(
            Arc::new(MockTransport::with_responses([
                response(200, include_str!("fixtures/me_current.json")),
                response(200, body),
            ])),
            Arc::new(MockTokens::new("access", "rotated")),
            Arc::new(RecordingSleeper::default()),
            SoundCloudRetryPolicy::default(),
        )
    };
    let initial = run(old)
        .sync_page(full_request(vec![SourceCollection::OwnedPlaylists]))
        .await
        .unwrap();
    let changed = run(new)
        .sync_page(SourceSyncRequest {
            mode: SourceSyncMode::Incremental {
                cursor: initial.next_sync_cursor.unwrap(),
            },
            ..full_request(vec![SourceCollection::OwnedPlaylists])
        })
        .await
        .unwrap();

    let removed_positions: HashSet<_> = changed
        .items
        .iter()
        .filter_map(|item| match item {
            SourceSyncItem::RelationshipRemoval(identity)
                if identity.parent_external_id.as_deref() == Some("soundcloud:playlists:9") =>
            {
                identity.position
            }
            _ => None,
        })
        .collect();
    assert_eq!(removed_positions, HashSet::from([0, 1]));
    assert!(changed.items.iter().any(|item| matches!(
        item,
        SourceSyncItem::Relationship(relationship)
            if relationship.identity.parent_external_id.as_deref() == Some("soundcloud:playlists:9")
                && relationship.identity.entity_external_id == "soundcloud:tracks:2"
                && relationship.identity.position == Some(0)
    )));
}

#[tokio::test]
async fn playlist_without_embedded_tracks_is_fetched_from_the_typed_detail_endpoint() {
    let list =
        r#"{"collection":[{"urn":"soundcloud:playlists:9","title":"Set"}],"next_href":null}"#;
    let detail = r#"{"urn":"soundcloud:playlists:9","title":"Set","tracks":[{"urn":"soundcloud:tracks:1","access":"playable"}]}"#;
    let transport = Arc::new(MockTransport::with_responses([
        response(200, include_str!("fixtures/me_current.json")),
        response(200, list),
        response(200, detail),
    ]));
    let connector = connector(
        transport.clone(),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
    );
    let page = connector
        .sync_page(full_request(vec![SourceCollection::LikedPlaylists]))
        .await
        .unwrap();
    assert!(page.items.iter().any(|item| matches!(
        item,
        SourceSyncItem::PlaylistEntry { playlist_external_id, .. }
            if playlist_external_id == "soundcloud:playlists:9"
    )));
    assert_eq!(
        transport.requests()[2].url,
        "https://api.soundcloud.com/playlists/soundcloud:playlists:9?show_tracks=true"
    );
}

#[tokio::test]
async fn cursor_validation_rejects_collection_mismatch_and_untrusted_next_href() {
    let make = || {
        connector(
            Arc::new(MockTransport::with_responses([
                response(200, include_str!("fixtures/me_current.json")),
                response(
                    200,
                    r#"{"collection":[],"next_href":"https://evil.example/steal"}"#,
                ),
            ])),
            Arc::new(MockTokens::new("access", "rotated")),
            Arc::new(RecordingSleeper::default()),
            SoundCloudRetryPolicy::default(),
        )
    };

    let connector = make();
    let first = connector
        .sync_page(full_request(vec![SourceCollection::UploadedTracks]))
        .await
        .unwrap();
    let untrusted = connector
        .sync_page(SourceSyncRequest {
            page_cursor: first.next_page_cursor,
            ..full_request(vec![SourceCollection::UploadedTracks])
        })
        .await
        .unwrap_err();
    assert_eq!(untrusted.kind, SourceErrorKind::InvalidData);

    let connector = make();
    let first = connector
        .sync_page(full_request(vec![SourceCollection::UploadedTracks]))
        .await
        .unwrap();
    let mismatch = connector
        .sync_page(SourceSyncRequest {
            page_cursor: first.next_page_cursor,
            ..full_request(vec![SourceCollection::LikedTracks])
        })
        .await
        .unwrap_err();
    assert_eq!(mismatch.kind, SourceErrorKind::InvalidData);
}

#[tokio::test]
async fn a_401_refreshes_once_and_retries_with_the_rotated_token() {
    let transport = Arc::new(MockTransport::with_responses([
        response(401, "{\"code\":401,\"message\":\"expired\"}"),
        response(200, include_str!("fixtures/me_current.json")),
    ]));
    let tokens = Arc::new(MockTokens::new("old-token", "new-token"));
    let client = SoundCloudClient::with_options(
        transport.clone(),
        tokens.clone(),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
        API_URL,
    )
    .unwrap();

    assert_eq!(
        client.account(SOURCE_ID).await.unwrap().external_id,
        "soundcloud:users:42"
    );
    assert_eq!(tokens.refresh_calls(), 1);
    let requests = transport.requests();
    assert_eq!(requests[0].headers["authorization"], "OAuth old-token");
    assert_eq!(requests[1].headers["authorization"], "OAuth new-token");
}

#[tokio::test]
async fn repeated_401_never_reuses_the_single_use_refresh_callback() {
    let transport = Arc::new(MockTransport::with_responses([
        response(401, "{}"),
        response(401, "{}"),
    ]));
    let tokens = Arc::new(MockTokens::new("old", "new"));
    let client = SoundCloudClient::with_options(
        transport,
        tokens.clone(),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
        API_URL,
    )
    .unwrap();

    let error = client.account(SOURCE_ID).await.unwrap_err();
    assert_eq!(error.kind, SourceErrorKind::AuthenticationRequired);
    assert_eq!(tokens.refresh_calls(), 1);
}

#[tokio::test]
async fn retry_after_is_capped_and_5xx_full_jitter_is_bounded_and_deterministic() {
    let mut limited = response(429, "{\"code\":429,\"message\":\"slow down\"}");
    limited
        .headers
        .insert("retry-after".to_string(), "700".to_string());
    let transport = Arc::new(MockTransport::with_responses([
        limited,
        response(200, include_str!("fixtures/me_current.json")),
    ]));
    let sleeper = Arc::new(RecordingSleeper::default());
    let policy = SoundCloudRetryPolicy {
        max_retries: 2,
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(10),
        max_retry_after: Duration::from_secs(5),
    };
    let client = SoundCloudClient::with_runtime_options(
        transport,
        Arc::new(MockTokens::new("access", "rotated")),
        sleeper.clone(),
        Arc::new(SequenceJitter::new([])),
        policy,
        API_URL,
        1024 * 1024,
    )
    .unwrap();
    assert!(client.account(SOURCE_ID).await.is_ok());
    assert_eq!(sleeper.delays(), vec![Duration::from_secs(5)]);

    let transport = Arc::new(MockTransport::with_responses([
        response(503, "{}"),
        response(500, "{}"),
        response(503, "{}"),
    ]));
    let sleeper = Arc::new(RecordingSleeper::default());
    let client = SoundCloudClient::with_runtime_options(
        transport.clone(),
        Arc::new(MockTokens::new("access", "rotated")),
        sleeper.clone(),
        Arc::new(SequenceJitter::new([
            Duration::from_millis(250),
            Duration::from_millis(1_500),
        ])),
        policy,
        API_URL,
        1024 * 1024,
    )
    .unwrap();
    let error = client.account(SOURCE_ID).await.unwrap_err();
    assert_eq!(error.kind, SourceErrorKind::TemporarilyUnavailable);
    assert_eq!(transport.requests().len(), 3);
    assert_eq!(
        sleeper.delays(),
        vec![Duration::from_millis(250), Duration::from_millis(1_500)]
    );
}

#[test]
fn debug_output_redacts_request_response_and_transport_secrets() {
    let request = SoundCloudHttpRequest {
        url: "https://api.soundcloud.com/me?secret_token=url-secret&cursor=opaque-secret"
            .to_string(),
        headers: BTreeMap::from([
            (
                "authorization".to_string(),
                "OAuth header-secret".to_string(),
            ),
            ("accept".to_string(), "application/json".to_string()),
        ]),
    };
    let response = SoundCloudHttpResponse {
        status: 401,
        headers: BTreeMap::from([(
            "set-cookie".to_string(),
            "session=response-header-secret".to_string(),
        )]),
        body: "response-body-secret".to_string(),
    };
    let transport_error = SoundCloudTransportError::new("transport-secret");

    let debug = format!("{request:?} {response:?} {transport_error:?}");
    for secret in [
        "url-secret",
        "opaque-secret",
        "header-secret",
        "response-header-secret",
        "response-body-secret",
        "transport-secret",
    ] {
        assert!(!debug.contains(secret), "Debug leaked {secret}");
    }
    assert!(debug.contains("[redacted]"));
}

#[tokio::test]
async fn response_body_limit_is_enforced_before_parsing_without_retry() {
    let transport = Arc::new(MockTransport::with_responses([response(
        200,
        &format!("{{\"padding\":\"{}\"}}", "x".repeat(128)),
    )]));
    let client = SoundCloudClient::with_runtime_options(
        transport.clone(),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        Arc::new(SequenceJitter::new([])),
        SoundCloudRetryPolicy::default(),
        API_URL,
        32,
    )
    .unwrap();

    let error = client.account(SOURCE_ID).await.unwrap_err();
    assert_eq!(error.kind, SourceErrorKind::InvalidData);
    assert!(error.message.contains("body limit"));
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn provider_and_transport_errors_never_surface_raw_secrets() {
    let provider_secret = "provider-error-secret";
    let client = SoundCloudClient::with_options(
        Arc::new(MockTransport::with_responses([response(
            400,
            &format!("{{\"code\":400,\"message\":\"bad token {provider_secret}\"}}"),
        )])),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        SoundCloudRetryPolicy::default(),
        API_URL,
    )
    .unwrap();
    let provider_error = client.account(SOURCE_ID).await.unwrap_err();
    assert!(!format!("{provider_error:?} {provider_error}").contains(provider_secret));

    let transport_secret = "transport-error-secret";
    let client = SoundCloudClient::with_runtime_options(
        Arc::new(MockTransport::with_results([Err(
            SoundCloudTransportError::new(transport_secret),
        )])),
        Arc::new(MockTokens::new("access", "rotated")),
        Arc::new(RecordingSleeper::default()),
        Arc::new(SequenceJitter::new([])),
        SoundCloudRetryPolicy {
            max_retries: 0,
            ..SoundCloudRetryPolicy::default()
        },
        API_URL,
        1024,
    )
    .unwrap();
    let transport_error = client.account(SOURCE_ID).await.unwrap_err();
    assert!(!format!("{transport_error:?} {transport_error}").contains(transport_secret));
    assert_eq!(
        transport_error.message,
        "SoundCloud transport request failed"
    );
}

struct EnvironmentToken(String);

impl SoundCloudTokenProvider for EnvironmentToken {
    fn access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }

    fn refresh_access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        Box::pin(async {
            Err(SourceError::new(
                SourceErrorKind::AuthenticationRequired,
                "live smoke token expired; provide a fresh SOUNDCLOUD_ACCESS_TOKEN",
            ))
        })
    }
}

/// Credential-gated schema smoke test. See `ENDPOINT_VALIDATION.md` for the exact command.
#[tokio::test]
#[ignore = "requires a real SOUNDCLOUD_ACCESS_TOKEN and makes one GET /me request"]
async fn live_account_smoke() {
    let token = std::env::var("SOUNDCLOUD_ACCESS_TOKEN")
        .expect("set SOUNDCLOUD_ACCESS_TOKEN to run the ignored live smoke test");
    let client = SoundCloudClient::new(
        Arc::new(ReqwestSoundCloudTransport::default()),
        Arc::new(EnvironmentToken(token)),
    )
    .unwrap();
    let account = client.account(SOURCE_ID).await.unwrap();
    assert!(!account.external_id.is_empty());
}
