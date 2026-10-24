//! SoundCloud's read-only catalog adapter.
//!
//! The application supplies a token provider backed by its credential store. API responses are
//! decoded into typed DTOs and retained as non-secret provider metadata while being normalized
//! into `rec_core` contracts.

use chrono::{DateTime, Utc};
use rec_core::{
    BoxFuture, CatalogSourceConnector, ExternalPlaylist, ExternalPlaylistEntry, HealthStatus,
    MetadataValue, SourceAccount, SourceAvailability, SourceCapabilities, SourceCollection,
    SourceCollectionRelationship, SourceCollectionRelationshipIdentity, SourceEntityKind,
    SourceError, SourceErrorKind, SourceHealth, SourceRemoval, SourceSyncItem, SourceSyncMode,
    SourceSyncPage, SourceSyncRequest, SourceTrackIdentity, SourceTrackOccurrence, SyncCursor,
    SyncProgress,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;
use uuid::Uuid;

const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;
const DEFAULT_API_URL: &str = "https://api.soundcloud.com/";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Playlist track lists default to excluding blocked tracks; request every access level so stored
/// positions match the playlist the user sees on SoundCloud.
const ALL_TRACK_ACCESS: &str = "playable,preview,blocked";
pub const DEFAULT_MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;
const CURSOR_VERSION: u8 = 1;

/// A non-secret access token provider. Implementations must atomically replace persisted refresh
/// token state before returning because SoundCloud refresh tokens are single-use.
pub trait SoundCloudTokenProvider: Send + Sync {
    fn access_token(&self) -> BoxFuture<'_, Result<String, SourceError>>;
    fn refresh_access_token(&self) -> BoxFuture<'_, Result<String, SourceError>>;
}

/// A deliberately small HTTP boundary that makes exact API fixtures testable without a server.
pub trait SoundCloudHttpTransport: Send + Sync {
    fn execute(
        &self,
        request: SoundCloudHttpRequest,
    ) -> BoxFuture<'_, Result<SoundCloudHttpResponse, SoundCloudTransportError>>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct SoundCloudHttpRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for SoundCloudHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SoundCloudHttpRequest")
            .field("url", &sanitized_url(&self.url))
            .field("headers", &sanitized_headers(&self.headers))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SoundCloudHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl fmt::Debug for SoundCloudHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SoundCloudHttpResponse")
            .field("status", &self.status)
            .field("headers", &sanitized_headers(&self.headers))
            .field("body", &"[redacted]")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCloudTransportErrorKind {
    Network,
    ResponseTooLarge,
    InvalidResponse,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SoundCloudTransportError {
    pub kind: SoundCloudTransportErrorKind,
}

impl fmt::Debug for SoundCloudTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SoundCloudTransportError")
            .field("kind", &self.kind)
            .field("message", &"[redacted]")
            .finish()
    }
}

impl SoundCloudTransportError {
    pub fn new(_message: impl Into<String>) -> Self {
        Self {
            kind: SoundCloudTransportErrorKind::Network,
        }
    }

    fn response_too_large() -> Self {
        Self {
            kind: SoundCloudTransportErrorKind::ResponseTooLarge,
        }
    }

    fn invalid_response(_message: impl Into<String>) -> Self {
        Self {
            kind: SoundCloudTransportErrorKind::InvalidResponse,
        }
    }
}

#[derive(Clone)]
pub struct ReqwestSoundCloudTransport {
    client: reqwest::Client,
    max_response_body_bytes: usize,
}

impl Default for ReqwestSoundCloudTransport {
    /// A stalled connection must not hang a sync forever, so the default client is bounded.
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .user_agent("rec-bag-desktop/1")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::new(client)
    }
}

impl fmt::Debug for ReqwestSoundCloudTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestSoundCloudTransport")
            .field("max_response_body_bytes", &self.max_response_body_bytes)
            .finish_non_exhaustive()
    }
}

impl ReqwestSoundCloudTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_max_response_body_bytes(client, DEFAULT_MAX_RESPONSE_BODY_BYTES)
    }

    pub fn with_max_response_body_bytes(
        client: reqwest::Client,
        max_response_body_bytes: usize,
    ) -> Self {
        Self {
            client,
            max_response_body_bytes: max_response_body_bytes.max(1),
        }
    }
}

impl SoundCloudHttpTransport for ReqwestSoundCloudTransport {
    fn execute(
        &self,
        request: SoundCloudHttpRequest,
    ) -> BoxFuture<'_, Result<SoundCloudHttpResponse, SoundCloudTransportError>> {
        let client = self.client.clone();
        let max_response_body_bytes = self.max_response_body_bytes;
        Box::pin(async move {
            let mut builder = client.get(request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let mut response = builder
                .send()
                .await
                .map_err(|error| SoundCloudTransportError::new(error.to_string()))?;
            if response
                .content_length()
                .is_some_and(|length| length > max_response_body_bytes as u64)
            {
                return Err(SoundCloudTransportError::response_too_large());
            }
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
                })
                .collect();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| SoundCloudTransportError::new(error.to_string()))?
            {
                if body.len().saturating_add(chunk.len()) > max_response_body_bytes {
                    return Err(SoundCloudTransportError::response_too_large());
                }
                body.extend_from_slice(&chunk);
            }
            let body = String::from_utf8(body).map_err(|_| {
                SoundCloudTransportError::invalid_response(
                    "SoundCloud response body was not valid UTF-8",
                )
            })?;
            Ok(SoundCloudHttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

pub trait SoundCloudSleeper: Send + Sync {
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()>;
}

#[derive(Default)]
pub struct TokioSoundCloudSleeper;

impl SoundCloudSleeper for TokioSoundCloudSleeper {
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move { tokio::time::sleep(duration).await })
    }
}

/// Supplies full-jitter delays. Production uses a process-local pseudo-random source; tests can
/// inject a deterministic implementation without sleeping.
pub trait SoundCloudJitterSource: Send + Sync {
    /// Return a duration in the inclusive range `0..=upper_bound`. Callers clamp the result.
    fn jitter(&self, upper_bound: Duration) -> Duration;
}

pub struct SystemSoundCloudJitter {
    state: AtomicU64,
}

impl Default for SystemSoundCloudJitter {
    fn default() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            state: AtomicU64::new(seed.max(1)),
        }
    }
}

impl fmt::Debug for SystemSoundCloudJitter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemSoundCloudJitter")
            .finish_non_exhaustive()
    }
}

impl SoundCloudJitterSource for SystemSoundCloudJitter {
    fn jitter(&self, upper_bound: Duration) -> Duration {
        let mut current = self.state.load(Ordering::Relaxed);
        let random = loop {
            let mut next = current;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break next,
                Err(observed) => current = observed,
            }
        };
        let upper_nanos = upper_bound.as_nanos().min(u64::MAX as u128) as u64;
        let nanos = if upper_nanos == u64::MAX {
            random
        } else {
            random % (upper_nanos + 1)
        };
        Duration::from_nanos(nanos)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundCloudRetryPolicy {
    pub max_retries: u8,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub max_retry_after: Duration,
}

impl Default for SoundCloudRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(60),
        }
    }
}

#[derive(Clone)]
pub struct SoundCloudClient {
    transport: Arc<dyn SoundCloudHttpTransport>,
    tokens: Arc<dyn SoundCloudTokenProvider>,
    sleeper: Arc<dyn SoundCloudSleeper>,
    jitter: Arc<dyn SoundCloudJitterSource>,
    retry_policy: SoundCloudRetryPolicy,
    max_response_body_bytes: usize,
    api_url: Url,
}

impl SoundCloudClient {
    pub fn new(
        transport: Arc<dyn SoundCloudHttpTransport>,
        tokens: Arc<dyn SoundCloudTokenProvider>,
    ) -> Result<Self, SourceError> {
        Self::with_options(
            transport,
            tokens,
            Arc::new(TokioSoundCloudSleeper),
            SoundCloudRetryPolicy::default(),
            DEFAULT_API_URL,
        )
    }

    pub fn with_options(
        transport: Arc<dyn SoundCloudHttpTransport>,
        tokens: Arc<dyn SoundCloudTokenProvider>,
        sleeper: Arc<dyn SoundCloudSleeper>,
        retry_policy: SoundCloudRetryPolicy,
        api_url: &str,
    ) -> Result<Self, SourceError> {
        Self::with_runtime_options(
            transport,
            tokens,
            sleeper,
            Arc::new(SystemSoundCloudJitter::default()),
            retry_policy,
            api_url,
            DEFAULT_MAX_RESPONSE_BODY_BYTES,
        )
    }

    /// Configures injectable timing and response limits. This is primarily useful for deterministic
    /// tests and hosts that require a lower memory ceiling than the default transport limit.
    pub fn with_runtime_options(
        transport: Arc<dyn SoundCloudHttpTransport>,
        tokens: Arc<dyn SoundCloudTokenProvider>,
        sleeper: Arc<dyn SoundCloudSleeper>,
        jitter: Arc<dyn SoundCloudJitterSource>,
        retry_policy: SoundCloudRetryPolicy,
        api_url: &str,
        max_response_body_bytes: usize,
    ) -> Result<Self, SourceError> {
        let api_url =
            Url::parse(api_url).map_err(|_| invalid_data("invalid SoundCloud API URL"))?;
        if api_url.scheme().is_empty() || api_url.host_str().is_none() {
            return Err(invalid_data(
                "SoundCloud API URL must include a scheme and host",
            ));
        }
        if max_response_body_bytes == 0 {
            return Err(invalid_data(
                "SoundCloud response body limit must be greater than zero",
            ));
        }
        Ok(Self {
            transport,
            tokens,
            sleeper,
            jitter,
            retry_policy,
            max_response_body_bytes,
            api_url,
        })
    }

    /// Fetches and normalizes `GET /me`.
    pub async fn account(&self, source_id: Uuid) -> Result<SourceAccount, SourceError> {
        let dto: AccountDto = self.get(self.url_for_path("me")?).await?;
        map_account(source_id, &dto)
    }

    async fn collection_page(
        &self,
        collection: SourceCollection,
        href: Option<&str>,
        page_size: u32,
    ) -> Result<CollectionPage, SourceError> {
        let url = match href {
            Some(href) => self.validated_next_href(href)?,
            None => self.collection_url(collection, page_size)?,
        };
        match collection {
            SourceCollection::UploadedTracks | SourceCollection::LikedTracks => {
                let page: ApiCollection<TrackDto> = self.get(url).await?;
                Ok(CollectionPage::Tracks(page.into_page()))
            }
            SourceCollection::OwnedPlaylists | SourceCollection::LikedPlaylists => {
                let page: ApiCollection<PlaylistDto> = self.get(url).await?;
                Ok(CollectionPage::Playlists(page.into_page()))
            }
        }
    }

    /// Fetches every track of a playlist in order via `GET /playlists/{urn}/tracks`. Collection
    /// pages are requested without embedded tracks so a page of large playlists cannot exceed the
    /// response body limit.
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<TrackDto>, SourceError> {
        let mut url = self.url_for_path(&format!("playlists/{id}/tracks"))?;
        url.query_pairs_mut()
            .append_pair("linked_partitioning", "true")
            .append_pair("limit", &MAX_PAGE_SIZE.to_string())
            .append_pair("access", ALL_TRACK_ACCESS);
        let mut tracks = Vec::new();
        loop {
            let page: ApiCollection<TrackDto> = self.get(url).await?;
            let page = page.into_page();
            tracks.extend(page.collection);
            match page.next_href {
                Some(href) => url = self.validated_next_href(&href)?,
                None => return Ok(tracks),
            }
        }
    }

    fn collection_url(
        &self,
        collection: SourceCollection,
        page_size: u32,
    ) -> Result<Url, SourceError> {
        let path = match collection {
            SourceCollection::UploadedTracks => "me/tracks",
            SourceCollection::LikedTracks => "me/likes/tracks",
            SourceCollection::OwnedPlaylists => "me/playlists",
            SourceCollection::LikedPlaylists => "me/likes/playlists",
        };
        let mut url = self.url_for_path(path)?;
        let mut query = url.query_pairs_mut();
        query.append_pair("linked_partitioning", "true");
        query.append_pair("limit", &page_size.to_string());
        if matches!(
            collection,
            SourceCollection::OwnedPlaylists | SourceCollection::LikedPlaylists
        ) {
            query.append_pair("show_tracks", "false");
        }
        drop(query);
        Ok(url)
    }

    fn url_for_path(&self, path: &str) -> Result<Url, SourceError> {
        self.api_url
            .join(path)
            .map_err(|_| invalid_data("invalid SoundCloud API path"))
    }

    fn validated_next_href(&self, href: &str) -> Result<Url, SourceError> {
        let url = Url::parse(href)
            .map_err(|_| invalid_data("SoundCloud returned an invalid next_href"))?;
        if url.scheme() != self.api_url.scheme()
            || url.host_str() != self.api_url.host_str()
            || url.port_or_known_default() != self.api_url.port_or_known_default()
        {
            return Err(invalid_data(
                "SoundCloud next_href has an unexpected origin",
            ));
        }
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, SourceError> {
        let mut token = self
            .tokens
            .access_token()
            .await
            .map_err(sanitize_token_provider_error)?;
        let mut refreshed = false;
        let mut retries = 0_u8;
        loop {
            let response = self
                .transport
                .execute(SoundCloudHttpRequest {
                    url: url.to_string(),
                    headers: BTreeMap::from([
                        (
                            "accept".to_string(),
                            "application/json; charset=utf-8".to_string(),
                        ),
                        ("authorization".to_string(), format!("OAuth {token}")),
                    ]),
                })
                .await;
            match response {
                Ok(response) if response.body.len() > self.max_response_body_bytes => {
                    return Err(invalid_data(
                        "SoundCloud response exceeded the configured body limit",
                    ));
                }
                Ok(response) if (200..300).contains(&response.status) => {
                    return serde_json::from_str(&response.body).map_err(|error| {
                        invalid_data(format!(
                            "SoundCloud returned a response outside the validated schema: {error}"
                        ))
                    });
                }
                Ok(response) if response.status == 401 && !refreshed => {
                    token = self
                        .tokens
                        .refresh_access_token()
                        .await
                        .map_err(sanitize_token_provider_error)?;
                    refreshed = true;
                }
                Ok(response) => {
                    let error = response_error(&response, self.retry_policy.max_retry_after);
                    if error.is_retryable() && retries < self.retry_policy.max_retries {
                        let delay = retry_delay(
                            &response,
                            retries,
                            self.retry_policy,
                            self.jitter.as_ref(),
                        );
                        retries += 1;
                        self.sleeper.sleep(delay).await;
                    } else {
                        return Err(error);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind,
                        SoundCloudTransportErrorKind::ResponseTooLarge
                            | SoundCloudTransportErrorKind::InvalidResponse
                    ) =>
                {
                    return Err(invalid_data(
                        "SoundCloud transport returned an invalid bounded response",
                    ));
                }
                Err(_error) if retries < self.retry_policy.max_retries => {
                    let delay = jittered_exponential_delay(
                        retries,
                        self.retry_policy,
                        self.jitter.as_ref(),
                    );
                    retries += 1;
                    self.sleeper.sleep(delay).await;
                }
                Err(_error) => {
                    return Err(SourceError::new(
                        SourceErrorKind::TemporarilyUnavailable,
                        "SoundCloud transport request failed",
                    ));
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct SoundCloudConnector {
    source_id: Uuid,
    client: SoundCloudClient,
}

impl SoundCloudConnector {
    pub fn new(source_id: Uuid, client: SoundCloudClient) -> Self {
        Self { source_id, client }
    }

    pub fn source_id(&self) -> Uuid {
        self.source_id
    }

    async fn sync_page_inner(
        &self,
        request: SourceSyncRequest,
    ) -> Result<SourceSyncPage, SourceError> {
        let page_size = validated_page_size(request.page_size)?;
        let requested_collections = selected_collections(&request.collections);
        let expected_base_cursor = match &request.mode {
            SourceSyncMode::Full => None,
            SourceSyncMode::Incremental { cursor } => Some(cursor.value.clone()),
        };

        let (mut state, mut items) = if let Some(cursor) = &request.page_cursor {
            let state: SoundCloudPageCursor = decode_cursor(&cursor.value, "page")?;
            if state.version != CURSOR_VERSION
                || state.collections != requested_collections
                || state.base_sync_cursor != expected_base_cursor
            {
                return Err(invalid_data(
                    "SoundCloud page cursor does not match this sync request",
                ));
            }
            (state, Vec::new())
        } else {
            let previous_relationships = match &request.mode {
                SourceSyncMode::Full => Vec::new(),
                SourceSyncMode::Incremental { cursor } => {
                    let snapshot: SoundCloudSyncCursor = decode_cursor(&cursor.value, "sync")?;
                    if snapshot.version != CURSOR_VERSION
                        || snapshot.collections != requested_collections
                    {
                        return Err(invalid_data(
                            "SoundCloud sync cursor does not match the requested collections",
                        ));
                    }
                    snapshot.relationships
                }
            };
            let account = self.client.account(self.source_id).await?;
            let account_id = account.external_id.clone();
            (
                SoundCloudPageCursor {
                    version: CURSOR_VERSION,
                    collections: requested_collections,
                    collection_index: 0,
                    next_href: None,
                    observed_at: Utc::now(),
                    account_external_id: account_id,
                    base_sync_cursor: expected_base_cursor,
                    previous_relationships,
                    current_relationships: Vec::new(),
                    emitted_tracks: Vec::new(),
                    emitted_playlists: Vec::new(),
                    emitted_removals: Vec::new(),
                },
                vec![SourceSyncItem::Account(account)],
            )
        };

        let collection = state
            .collections
            .get(state.collection_index)
            .copied()
            .ok_or_else(|| invalid_data("SoundCloud page cursor has no collection"))?;
        let page = self
            .client
            .collection_page(collection, state.next_href.as_deref(), page_size)
            .await?;
        let mut current: HashSet<_> = state.current_relationships.drain(..).collect();
        let mut emitted_tracks: HashSet<_> = state.emitted_tracks.drain(..).collect();
        let mut emitted_playlists: HashSet<_> = state.emitted_playlists.drain(..).collect();
        let mut emitted_removals: HashSet<_> = state.emitted_removals.drain(..).collect();
        let mut removals = Vec::new();

        let provider_next = match page {
            CollectionPage::Tracks(page) => {
                for track in page.collection {
                    add_track_and_relationship(
                        &mut items,
                        &mut removals,
                        &mut current,
                        &mut emitted_tracks,
                        &mut emitted_removals,
                        self.source_id,
                        &state.account_external_id,
                        collection,
                        &track,
                        state.observed_at,
                    )?;
                }
                page.next_href
            }
            CollectionPage::Playlists(page) => {
                for mut playlist in page.collection {
                    if playlist.needs_track_fetch() {
                        playlist.tracks =
                            Some(self.client.playlist_tracks(&playlist.external_id()?).await?);
                    }
                    add_playlist_and_entries(
                        &mut items,
                        &mut removals,
                        &mut current,
                        &mut emitted_tracks,
                        &mut emitted_playlists,
                        &mut emitted_removals,
                        self.source_id,
                        &state.account_external_id,
                        collection,
                        &playlist,
                        state.observed_at,
                    )?;
                }
                page.next_href
            }
        };

        state.current_relationships = sorted_relationships(current);
        state.emitted_tracks = sorted_strings(emitted_tracks);
        state.emitted_playlists = sorted_strings(emitted_playlists);
        state.emitted_removals = sorted_entities(emitted_removals);
        if let Some(next_href) = provider_next {
            state.next_href = Some(next_href);
        } else {
            state.collection_index += 1;
            state.next_href = None;
        }

        let complete = state.collection_index >= state.collections.len();
        let (next_page_cursor, next_sync_cursor) = if complete {
            let previous: HashSet<_> = state.previous_relationships.iter().cloned().collect();
            let current: HashSet<_> = state.current_relationships.iter().cloned().collect();
            let mut missing: Vec<_> = previous.difference(&current).cloned().collect();
            sort_relationship_slice(&mut missing);
            items.extend(missing.into_iter().map(SourceSyncItem::RelationshipRemoval));
            let snapshot = SoundCloudSyncCursor {
                version: CURSOR_VERSION,
                collections: state.collections.clone(),
                relationships: state.current_relationships.clone(),
            };
            (
                None,
                Some(SyncCursor::at(
                    serde_json::to_string(&snapshot)
                        .map_err(|_| invalid_data("could not encode SoundCloud sync cursor"))?,
                    state.observed_at,
                )),
            )
        } else {
            (
                Some(SyncCursor::at(
                    serde_json::to_string(&state)
                        .map_err(|_| invalid_data("could not encode SoundCloud page cursor"))?,
                    state.observed_at,
                )),
                None,
            )
        };

        let record_count = items.len() as u64 + removals.len() as u64;
        Ok(SourceSyncPage {
            items,
            removals,
            next_page_cursor,
            next_sync_cursor,
            progress: Some(SyncProgress {
                discovered_items: record_count,
                processed_items: record_count,
                succeeded_items: record_count,
                ..SyncProgress::default()
            }),
        })
    }
}

impl CatalogSourceConnector for SoundCloudConnector {
    fn health(&self) -> BoxFuture<'_, Result<SourceHealth, SourceError>> {
        Box::pin(async move {
            match self.client.account(self.source_id).await {
                Ok(_) => Ok(SourceHealth {
                    status: HealthStatus::Healthy,
                    checked_at: Utc::now(),
                    message: None,
                    capabilities: soundcloud_capabilities(),
                }),
                Err(error) if error.kind == SourceErrorKind::AuthenticationRequired => {
                    Ok(SourceHealth {
                        status: HealthStatus::AuthenticationRequired,
                        checked_at: Utc::now(),
                        message: Some("SoundCloud authorization is required".to_string()),
                        capabilities: soundcloud_capabilities(),
                    })
                }
                Err(error) => Err(error),
            }
        })
    }

    fn sync_page(
        &self,
        request: SourceSyncRequest,
    ) -> BoxFuture<'_, Result<SourceSyncPage, SourceError>> {
        Box::pin(async move { self.sync_page_inner(request).await })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SoundCloudSyncCursor {
    version: u8,
    collections: Vec<SourceCollection>,
    relationships: Vec<SourceCollectionRelationshipIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SoundCloudPageCursor {
    version: u8,
    collections: Vec<SourceCollection>,
    collection_index: usize,
    next_href: Option<String>,
    observed_at: DateTime<Utc>,
    account_external_id: String,
    base_sync_cursor: Option<String>,
    previous_relationships: Vec<SourceCollectionRelationshipIdentity>,
    current_relationships: Vec<SourceCollectionRelationshipIdentity>,
    emitted_tracks: Vec<String>,
    emitted_playlists: Vec<String>,
    emitted_removals: Vec<EntityIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct EntityIdentity {
    entity_kind: SourceEntityKind,
    external_id: String,
}

#[derive(Debug)]
enum CollectionPage {
    Tracks(TypedPage<TrackDto>),
    Playlists(TypedPage<PlaylistDto>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApiCollection<T> {
    Paginated(TypedPage<T>),
    Legacy(Vec<T>),
}

impl<T> ApiCollection<T> {
    fn into_page(self) -> TypedPage<T> {
        match self {
            Self::Paginated(page) => page,
            Self::Legacy(collection) => TypedPage {
                collection,
                next_href: None,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct TypedPage<T> {
    collection: Vec<T>,
    #[serde(default)]
    next_href: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum LegacyId {
    String(String),
    Number(u64),
}

impl LegacyId {
    fn as_string(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    urn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<LegacyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    full_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permalink_url: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    urn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<LegacyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permalink_url: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl UserDto {
    fn external_id(&self) -> Option<String> {
        provider_id("users", self.urn.as_deref(), self.id.as_ref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    urn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<LegacyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    downloadable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permalink_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<UserDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sharing: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl TrackDto {
    fn external_id(&self) -> Result<String, SourceError> {
        provider_id("tracks", self.urn.as_deref(), self.id.as_ref())
            .ok_or_else(|| invalid_data("SoundCloud track is missing a valid urn or id"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    urn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<LegacyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permalink_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<UserDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sharing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracks: Option<Vec<TrackDto>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl PlaylistDto {
    fn external_id(&self) -> Result<String, SourceError> {
        provider_id("playlists", self.urn.as_deref(), self.id.as_ref())
            .ok_or_else(|| invalid_data("SoundCloud playlist is missing a valid urn or id"))
    }

    /// Embedded tracks are used only when present and complete; SoundCloud may omit or truncate
    /// them, in which case `track_count` reveals the gap.
    fn needs_track_fetch(&self) -> bool {
        match (&self.tracks, self.track_count) {
            (None, _) => true,
            (Some(tracks), Some(count)) => (tracks.len() as u64) < count,
            (Some(_), None) => false,
        }
    }
}

/// Identity is always a URN. A legacy numeric `id` (removed from the API in 2025) is converted to
/// the equivalent `soundcloud:{kind}:{id}` URN so one resource never gets two identities.
fn provider_id(kind: &str, urn: Option<&str>, id: Option<&LegacyId>) -> Option<String> {
    urn.filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            id.map(LegacyId::as_string)
                .filter(|value| !value.is_empty())
                .map(|value| format!("soundcloud:{kind}:{value}"))
        })
}

fn decode_cursor<T: DeserializeOwned>(value: &str, kind: &str) -> Result<T, SourceError> {
    serde_json::from_str(value)
        .map_err(|_| invalid_data(format!("invalid SoundCloud {kind} cursor")))
}

fn selected_collections(request: &[SourceCollection]) -> Vec<SourceCollection> {
    let requested = if request.is_empty() {
        vec![
            SourceCollection::UploadedTracks,
            SourceCollection::OwnedPlaylists,
            SourceCollection::LikedTracks,
            SourceCollection::LikedPlaylists,
        ]
    } else {
        request.to_vec()
    };
    let mut selected = Vec::new();
    for collection in requested {
        if !selected.contains(&collection) {
            selected.push(collection);
        }
    }
    selected
}

fn validated_page_size(value: Option<u32>) -> Result<u32, SourceError> {
    let value = value.unwrap_or(DEFAULT_PAGE_SIZE);
    if value == 0 || value > MAX_PAGE_SIZE {
        return Err(invalid_data(format!(
            "SoundCloud page size must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(value)
}

fn soundcloud_capabilities() -> SourceCapabilities {
    SourceCapabilities {
        library_sync: true,
        playlist_sync: true,
        search: false,
        streaming: false,
        download: false,
        metadata: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn add_track_and_relationship(
    output: &mut Vec<SourceSyncItem>,
    removals: &mut Vec<SourceRemoval>,
    current: &mut HashSet<SourceCollectionRelationshipIdentity>,
    emitted_tracks: &mut HashSet<String>,
    emitted_removals: &mut HashSet<EntityIdentity>,
    source_id: Uuid,
    account_id: &str,
    collection: SourceCollection,
    track: &TrackDto,
    observed_at: DateTime<Utc>,
) -> Result<(), SourceError> {
    let external_id = track.external_id()?;
    emit_track(
        output,
        removals,
        emitted_tracks,
        emitted_removals,
        source_id,
        track,
        observed_at,
    )?;
    emit_relationship(
        output,
        current,
        SourceCollectionRelationshipIdentity::new(
            source_id,
            account_id,
            collection,
            SourceEntityKind::Track,
            external_id,
        ),
        observed_at,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_playlist_and_entries(
    output: &mut Vec<SourceSyncItem>,
    removals: &mut Vec<SourceRemoval>,
    current: &mut HashSet<SourceCollectionRelationshipIdentity>,
    emitted_tracks: &mut HashSet<String>,
    emitted_playlists: &mut HashSet<String>,
    emitted_removals: &mut HashSet<EntityIdentity>,
    source_id: Uuid,
    account_id: &str,
    collection: SourceCollection,
    value: &PlaylistDto,
    observed_at: DateTime<Utc>,
) -> Result<(), SourceError> {
    let mut playlist = map_playlist(source_id, value, observed_at)?;
    let playlist_id = playlist.external_id.clone();
    let emit_playlist = emitted_playlists.insert(playlist_id.clone());
    emit_relationship(
        output,
        current,
        SourceCollectionRelationshipIdentity::new(
            source_id,
            account_id,
            collection,
            SourceEntityKind::Playlist,
            playlist_id.clone(),
        ),
        observed_at,
    );

    for (position, track) in value
        .tracks
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let track_id = track.external_id()?;
        emit_track(
            output,
            removals,
            emitted_tracks,
            emitted_removals,
            source_id,
            track,
            observed_at,
        )?;
        let entry = ExternalPlaylistEntry::new(
            position as u32,
            SourceTrackIdentity::new(source_id, track_id.clone()),
        );
        playlist.entries.push(entry.clone());
        if emit_playlist {
            output.push(SourceSyncItem::PlaylistEntry {
                playlist_external_id: playlist_id.clone(),
                snapshot: playlist.snapshot.clone(),
                entry,
            });
        }
        emit_relationship(
            output,
            current,
            SourceCollectionRelationshipIdentity::new(
                source_id,
                account_id,
                collection,
                SourceEntityKind::Track,
                track_id,
            )
            .with_parent(playlist_id.clone(), Some(position as u32)),
            observed_at,
        );
    }
    if emit_playlist {
        playlist.sort_entries();
        output.push(SourceSyncItem::Playlist(playlist));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_track(
    output: &mut Vec<SourceSyncItem>,
    removals: &mut Vec<SourceRemoval>,
    emitted_tracks: &mut HashSet<String>,
    emitted_removals: &mut HashSet<EntityIdentity>,
    source_id: Uuid,
    track: &TrackDto,
    observed_at: DateTime<Utc>,
) -> Result<(), SourceError> {
    let id = track.external_id()?;
    if emitted_tracks.insert(id.clone()) {
        output.push(SourceSyncItem::Track(map_track(
            source_id,
            track,
            observed_at,
        )?));
    }
    if availability(track) == SourceAvailability::Removed {
        let identity = EntityIdentity {
            entity_kind: SourceEntityKind::Track,
            external_id: id.clone(),
        };
        if emitted_removals.insert(identity) {
            removals.push(SourceRemoval {
                entity_kind: SourceEntityKind::Track,
                external_id: id,
            });
        }
    }
    Ok(())
}

fn emit_relationship(
    output: &mut Vec<SourceSyncItem>,
    current: &mut HashSet<SourceCollectionRelationshipIdentity>,
    identity: SourceCollectionRelationshipIdentity,
    observed_at: DateTime<Utc>,
) {
    if current.insert(identity.clone()) {
        output.push(SourceSyncItem::Relationship(
            SourceCollectionRelationship::new(identity, observed_at),
        ));
    }
}

fn map_account(source_id: Uuid, value: &AccountDto) -> Result<SourceAccount, SourceError> {
    let external_id = provider_id("users", value.urn.as_deref(), value.id.as_ref())
        .ok_or_else(|| invalid_data("SoundCloud account is missing a valid urn or id"))?;
    let mut account = SourceAccount::new(source_id, external_id);
    account.username = value.username.clone();
    account.display_name = value.full_name.clone();
    account.external_url = value.permalink_url.clone();
    account.provider_metadata = metadata(value)?;
    Ok(account)
}

fn map_track(
    source_id: Uuid,
    value: &TrackDto,
    observed_at: DateTime<Utc>,
) -> Result<SourceTrackOccurrence, SourceError> {
    let mut track = SourceTrackOccurrence::new(source_id, value.external_id()?);
    track.account_external_id = value.user.as_ref().and_then(UserDto::external_id);
    track.external_url = value.permalink_url.clone();
    track.provider_metadata = metadata(value)?;
    track.availability = availability(value);
    track.last_seen_at = observed_at;
    Ok(track)
}

fn map_playlist(
    source_id: Uuid,
    value: &PlaylistDto,
    observed_at: DateTime<Utc>,
) -> Result<ExternalPlaylist, SourceError> {
    let mut playlist = ExternalPlaylist::new(
        source_id,
        value.external_id()?,
        value
            .title
            .clone()
            .unwrap_or_else(|| "Untitled SoundCloud playlist".to_string()),
    );
    playlist.description = value.description.clone();
    playlist.account_external_id = value.user.as_ref().and_then(UserDto::external_id);
    playlist.external_url = value.permalink_url.clone();
    playlist.snapshot = value
        .last_modified
        .clone()
        .or_else(|| value.created_at.clone());
    playlist.provider_metadata = metadata(value)?;
    playlist.last_seen_at = observed_at;
    Ok(playlist)
}

fn availability(value: &TrackDto) -> SourceAvailability {
    match value.state.as_deref() {
        Some("deleted") | Some("removed") => return SourceAvailability::Removed,
        Some("failed") | Some("inaccessible") => return SourceAvailability::Inaccessible,
        _ => {}
    }
    // The API Terms forbid downloading or storing User Content even when the uploader enabled
    // downloads, so a SoundCloud track is never `Obtainable`. `downloadable` stays in metadata.
    match value.access.as_deref() {
        Some("preview") => SourceAvailability::PreviewOnly,
        Some("blocked") => SourceAvailability::Blocked,
        Some("playable") => SourceAvailability::StreamingOnly,
        _ => SourceAvailability::MetadataOnly,
    }
}

fn metadata<T: Serialize>(value: &T) -> Result<BTreeMap<String, MetadataValue>, SourceError> {
    let value = serde_json::to_value(value)
        .map_err(|_| invalid_data("could not preserve SoundCloud provider metadata"))?;
    Ok(value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !is_sensitive_key(key))
                .map(|(key, value)| (key.clone(), metadata_value(value)))
                .collect()
        })
        .unwrap_or_default())
}

fn metadata_value(value: &Value) -> MetadataValue {
    match value {
        Value::Null => MetadataValue::Null,
        Value::Bool(value) => MetadataValue::Boolean(*value),
        Value::String(value) => MetadataValue::Text(value.clone()),
        Value::Number(value) => value
            .as_i64()
            .map(MetadataValue::Integer)
            .unwrap_or_else(|| MetadataValue::Decimal(value.to_string())),
        Value::Array(values) => MetadataValue::List(values.iter().map(metadata_value).collect()),
        Value::Object(values) => MetadataValue::Object(
            values
                .iter()
                .filter(|(key, _)| !is_sensitive_key(key))
                .map(|(key, value)| (key.clone(), metadata_value(value)))
                .collect(),
        ),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("api-key")
        || matches!(key.as_str(), "authorization" | "cookie" | "set-cookie")
}

fn sanitized_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = if is_sensitive_key(name) {
                "[redacted]".to_string()
            } else {
                value.clone()
            };
            (name.clone(), value)
        })
        .collect()
}

fn sanitized_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "[redacted invalid URL]".to_string();
    };
    let query_names: Vec<String> = url
        .query_pairs()
        .map(|(name, _)| name.into_owned())
        .collect();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    if !query_names.is_empty() {
        let mut query = url.query_pairs_mut();
        for name in query_names {
            query.append_pair(&name, "[redacted]");
        }
    }
    url.to_string()
}

fn sorted_relationships(
    values: HashSet<SourceCollectionRelationshipIdentity>,
) -> Vec<SourceCollectionRelationshipIdentity> {
    let mut values: Vec<_> = values.into_iter().collect();
    sort_relationship_slice(&mut values);
    values
}

fn sort_relationship_slice(values: &mut [SourceCollectionRelationshipIdentity]) {
    values.sort_by_cached_key(|value| serde_json::to_string(value).unwrap_or_default());
}

fn sorted_strings(values: HashSet<String>) -> Vec<String> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort();
    values
}

fn sorted_entities(values: HashSet<EntityIdentity>) -> Vec<EntityIdentity> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_by_key(|value| {
        (
            format!("{:?}", value.entity_kind),
            value.external_id.clone(),
        )
    });
    values
}

fn invalid_data(message: impl Into<String>) -> SourceError {
    SourceError::new(SourceErrorKind::InvalidData, message)
}

fn sanitize_token_provider_error(error: SourceError) -> SourceError {
    SourceError::new(error.kind, "SoundCloud token provider failed")
}

fn response_error(response: &SoundCloudHttpResponse, max_retry_after: Duration) -> SourceError {
    let body: Option<ApiErrorDto> = serde_json::from_str(&response.body).ok();
    let mut error = SourceError::new(
        match response.status {
            401 => SourceErrorKind::AuthenticationRequired,
            403 => SourceErrorKind::PermissionDenied,
            404 => SourceErrorKind::NotFound,
            429 => SourceErrorKind::RateLimited,
            500..=599 => SourceErrorKind::TemporarilyUnavailable,
            _ => SourceErrorKind::Permanent,
        },
        format!(
            "SoundCloud request failed with HTTP status {}",
            response.status
        ),
    );
    error.provider_code = body
        .and_then(|error| error.code.map(|code| code.to_string()))
        .or_else(|| Some(response.status.to_string()));
    error.retry_after_seconds =
        retry_after_seconds(response).map(|seconds| seconds.min(max_retry_after.as_secs()));
    error
}

/// `Retry-After`, or for rate limits the documented `X-Ratelimit-Reset` (Unix seconds).
fn retry_after_seconds(response: &SoundCloudHttpResponse) -> Option<u64> {
    header(&response.headers, "retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            if response.status != 429 {
                return None;
            }
            let reset = header(&response.headers, "x-ratelimit-reset")?
                .parse::<u64>()
                .ok()?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some(reset.saturating_sub(now))
        })
}

#[derive(Debug, Deserialize)]
struct ApiErrorDto {
    #[serde(default)]
    code: Option<u16>,
}

fn retry_delay(
    response: &SoundCloudHttpResponse,
    retry: u8,
    policy: SoundCloudRetryPolicy,
    jitter: &dyn SoundCloudJitterSource,
) -> Duration {
    retry_after_seconds(response)
        .map(Duration::from_secs)
        .map(|delay| delay.min(policy.max_retry_after))
        .unwrap_or_else(|| jittered_exponential_delay(retry, policy, jitter))
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn exponential_delay(retry: u8, policy: SoundCloudRetryPolicy) -> Duration {
    let multiplier = 1_u32.checked_shl(retry as u32).unwrap_or(u32::MAX);
    policy
        .initial_backoff
        .saturating_mul(multiplier)
        .min(policy.max_backoff)
}

fn jittered_exponential_delay(
    retry: u8,
    policy: SoundCloudRetryPolicy,
    jitter: &dyn SoundCloudJitterSource,
) -> Duration {
    let upper_bound = exponential_delay(retry, policy);
    jitter.jitter(upper_bound).min(upper_bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_track_maps_all_access_states() {
        let parse = |json| serde_json::from_value::<TrackDto>(json).unwrap();
        assert_eq!(
            availability(&parse(serde_json::json!({"id": 1, "access": "playable"}))),
            SourceAvailability::StreamingOnly
        );
        assert_eq!(
            availability(&parse(
                serde_json::json!({"id": 1, "access": "playable", "downloadable": true, "download_url": "x"})
            )),
            SourceAvailability::StreamingOnly,
            "downloadable SoundCloud tracks are still not archivable under the API Terms"
        );
        assert_eq!(
            availability(&parse(serde_json::json!({"id": 1, "access": "preview"}))),
            SourceAvailability::PreviewOnly
        );
        assert_eq!(
            availability(&parse(serde_json::json!({"id": 1, "access": "blocked"}))),
            SourceAvailability::Blocked
        );
        assert_eq!(
            availability(&parse(serde_json::json!({"id": 1, "state": "deleted"}))),
            SourceAvailability::Removed
        );
        assert_eq!(
            availability(&parse(serde_json::json!({"id": 1, "state": "failed"}))),
            SourceAvailability::Inaccessible
        );
    }

    #[test]
    fn typed_dto_preserves_unknown_metadata_and_redacts_secrets() {
        let dto: TrackDto = serde_json::from_value(serde_json::json!({
            "urn": "soundcloud:tracks:1",
            "title": "Safe",
            "future_field": {"ok": 1, "access_token": "nope"},
            "secret_token": "nope"
        }))
        .unwrap();
        let actual = metadata(&dto).unwrap();
        assert_eq!(
            actual.get("future_field"),
            Some(&MetadataValue::Object(BTreeMap::from([(
                "ok".to_string(),
                MetadataValue::Integer(1)
            )])))
        );
        assert!(!actual.contains_key("secret_token"));
    }

    #[test]
    fn playlists_fetch_tracks_when_embedded_list_is_missing_or_truncated() {
        let parse = |json| serde_json::from_value::<PlaylistDto>(json).unwrap();
        let track = serde_json::json!({"urn": "soundcloud:tracks:1"});
        assert!(parse(serde_json::json!({"urn": "p"})).needs_track_fetch());
        assert!(parse(serde_json::json!({"urn": "p", "track_count": 2, "tracks": [track.clone()]}))
            .needs_track_fetch());
        assert!(!parse(serde_json::json!({"urn": "p", "track_count": 1, "tracks": [track.clone()]}))
            .needs_track_fetch());
        assert!(!parse(serde_json::json!({"urn": "p", "tracks": [track]})).needs_track_fetch());
    }

    #[test]
    fn current_urn_and_legacy_numeric_ids_are_supported() {
        let current: TrackDto = serde_json::from_value(serde_json::json!({
            "urn": "soundcloud:tracks:123"
        }))
        .unwrap();
        let legacy: TrackDto = serde_json::from_value(serde_json::json!({"id": 123})).unwrap();
        assert_eq!(current.external_id().unwrap(), "soundcloud:tracks:123");
        assert_eq!(legacy.external_id().unwrap(), "soundcloud:tracks:123");
    }
}
