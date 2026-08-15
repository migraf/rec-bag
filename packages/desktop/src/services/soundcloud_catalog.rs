//! SoundCloud catalog and desktop OAuth integration slice.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use api::{
    SoundCloudAuthorizationCodeExchange, SoundCloudRefreshTokenExchange, SoundCloudTokenResponse,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::TryRngCore;
use rec_core::{
    AssetRepository, BoxFuture, CanonicalTrack, CatalogSource, CatalogSourceConnector,
    CatalogSourceKind, ExternalPlaylist, HealthStatus, MetadataField, MetadataFieldValue,
    MetadataProvenance, MetadataValue, PlaylistRepository, RepositoryPageRequest,
    RepositoryTransaction, SourceAvailability, SourceCapabilities, SourceCollection,
    SourceCollectionRelationshipFilter, SourceEntityKind, SourceError, SourceErrorKind,
    SourceHealth, SourceOccurrenceFilter, SourceRepository, SourceSyncItem, SourceSyncMode,
    SourceSyncPage, SourceSyncRequest, SourceTrackOccurrence, StorageVerificationStatus,
    SyncCheckpoint, SyncError, SyncErrorKind, SyncProgress, SyncRun, SyncRunFilter, SyncRunKind,
    SyncRunRepository, SyncStatus, TrackRepository, TransactionManager,
};
use rec_persistence::{RefreshTokenMetadata, TursoCatalog};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soundcloud::{
    ReqwestSoundCloudTransport, SoundCloudClient, SoundCloudConnector, SoundCloudTokenProvider,
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex as AsyncMutex;
use ui::{
    CompletenessView, HealthTone, JobProgressView, MusicManagementEvent, MusicManagementView,
    PlaylistCompletenessView, ProviderChoice, SourceCollectionView, SourceView,
    TrackStorageStatusView, TrackStorageView,
};
use url::Url;
use uuid::Uuid;

use super::integration::{
    CredentialKey, CredentialStore, IntegrationFuture, IntegrationSliceHandler, Secret,
};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

const PROVIDER: &str = "soundcloud";
const AUTHORIZE_URL: &str = "https://secure.soundcloud.com/authorize";
const DEFAULT_PAGE_SIZE: u32 = 50;
const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
#[cfg(feature = "desktop")]
const MAX_CALLBACK_URL_BYTES: usize = 16 * 1024;
#[cfg(feature = "desktop")]
const SIGN_OUT_URL: &str = "https://secure.soundcloud.com/sign-out";
const FORGOTTEN_CONFIG_KEY: &str = "soundcloud_catalog_forgotten";

fn integration_error(message: impl Into<String>, retryable: bool) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: Some(IntegrationSlice::SoundcloudCatalog),
        message: message.into(),
        retryable,
    }
}

fn request_error(
    request: &IntegrationRequest,
    message: impl Into<String>,
    retryable: bool,
) -> IntegrationError {
    IntegrationError::for_request(request, message, retryable)
}

fn repository_error(error: impl std::fmt::Display) -> IntegrationError {
    integration_error(
        format!("SoundCloud catalog persistence failed: {error}"),
        true,
    )
}

/// Public, non-secret OAuth configuration supplied by desktop composition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundCloudOAuthClientConfig {
    pub client_id: String,
    pub redirect_uri: Url,
    pub authorization_endpoint: Url,
    pub callback_timeout: std::time::Duration,
}

impl SoundCloudOAuthClientConfig {
    pub fn new(client_id: impl Into<String>, redirect_uri: Url) -> Result<Self, IntegrationError> {
        let client_id = client_id.into();
        if client_id.trim().is_empty() || client_id.chars().any(char::is_control) {
            return Err(integration_error(
                "SoundCloud OAuth client ID is not configured",
                false,
            ));
        }
        if redirect_uri.fragment().is_some()
            || redirect_uri.host_str().is_none()
            || (redirect_uri.scheme() == "http"
                && !redirect_uri.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                }))
        {
            return Err(integration_error(
                "SoundCloud redirect URI is not a valid registered desktop callback",
                false,
            ));
        }
        Ok(Self {
            client_id,
            redirect_uri,
            authorization_endpoint: Url::parse(AUTHORIZE_URL)
                .expect("the fixed SoundCloud authorization URL is valid"),
            callback_timeout: CALLBACK_TIMEOUT,
        })
    }
}

/// Opens authorization and returns the callback URL. Implementations must arm the callback
/// receiver before opening the browser.
pub trait OAuthUserAgent: Send + Sync {
    fn authorize(
        &self,
        authorization_url: Url,
        redirect_uri: Url,
        timeout: std::time::Duration,
    ) -> BoxFuture<'_, Result<Url, IntegrationError>>;
}

/// Broker boundary used by authorization and single-use refresh rotation.
/// Result of asking SoundCloud to invalidate the current authorization grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderGrantRevocation {
    Revoked,
    AlreadyRevoked,
    Unsupported,
}

/// Secret-safe remote sign-out boundary. Implementations must never include token material in
/// returned errors; the service additionally replaces all client messages with fixed text.
pub trait SoundCloudSignOutClient: Send + Sync {
    fn revoke_grant(
        &self,
        access_token: Secret,
    ) -> BoxFuture<'_, Result<ProviderGrantRevocation, IntegrationError>>;
}

#[derive(Default)]
pub struct UnsupportedSoundCloudSignOutClient;

impl SoundCloudSignOutClient for UnsupportedSoundCloudSignOutClient {
    fn revoke_grant(
        &self,
        _access_token: Secret,
    ) -> BoxFuture<'_, Result<ProviderGrantRevocation, IntegrationError>> {
        Box::pin(async { Ok(ProviderGrantRevocation::Unsupported) })
    }
}

#[cfg(feature = "desktop")]
#[derive(Clone)]
pub struct ReqwestSoundCloudSignOutClient {
    client: reqwest::Client,
}

#[cfg(feature = "desktop")]
impl Default for ReqwestSoundCloudSignOutClient {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("rec-bag-desktop/1")
            .build()
            .expect("fixed SoundCloud sign-out client configuration is valid");
        Self { client }
    }
}

#[cfg(feature = "desktop")]
impl SoundCloudSignOutClient for ReqwestSoundCloudSignOutClient {
    fn revoke_grant(
        &self,
        access_token: Secret,
    ) -> BoxFuture<'_, Result<ProviderGrantRevocation, IntegrationError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let token = String::from_utf8(access_token.expose().to_vec()).map_err(|_| {
                integration_error("Stored SoundCloud credentials are invalid", false)
            })?;
            let authorization_value = format!("OAuth {token}");
            let mut authorization = reqwest::header::HeaderValue::from_bytes(
                authorization_value.as_bytes(),
            )
            .map_err(|_| integration_error("Stored SoundCloud credentials are invalid", false))?;
            authorization.set_sensitive(true);
            let response = client
                .post(SIGN_OUT_URL)
                .header(reqwest::header::AUTHORIZATION, authorization)
                .send()
                .await
                .map_err(|_| integration_error("SoundCloud remote sign-out failed", true))?;
            match response.status().as_u16() {
                200..=299 => Ok(ProviderGrantRevocation::Revoked),
                401 | 404 => Ok(ProviderGrantRevocation::AlreadyRevoked),
                429 | 500..=599 => Err(integration_error(
                    "SoundCloud remote sign-out is temporarily unavailable",
                    true,
                )),
                _ => Err(integration_error(
                    "SoundCloud rejected remote sign-out",
                    false,
                )),
            }
        })
    }
}

#[cfg(feature = "desktop")]
fn default_sign_out_client() -> Arc<dyn SoundCloudSignOutClient> {
    Arc::new(ReqwestSoundCloudSignOutClient::default())
}

#[cfg(not(feature = "desktop"))]
fn default_sign_out_client() -> Arc<dyn SoundCloudSignOutClient> {
    Arc::new(UnsupportedSoundCloudSignOutClient)
}

pub trait SoundCloudBrokerClient: Send + Sync {
    fn exchange_authorization_code(
        &self,
        request: SoundCloudAuthorizationCodeExchange,
    ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>>;

    fn refresh_token(
        &self,
        request: SoundCloudRefreshTokenExchange,
    ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>>;
}

#[derive(Default)]
pub struct ApiSoundCloudBrokerClient;

impl SoundCloudBrokerClient for ApiSoundCloudBrokerClient {
    fn exchange_authorization_code(
        &self,
        request: SoundCloudAuthorizationCodeExchange,
    ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>> {
        Box::pin(async move {
            api::exchange_soundcloud_authorization_code(request)
                .await
                .map_err(|error| broker_server_error(error.to_string()))
        })
    }

    fn refresh_token(
        &self,
        request: SoundCloudRefreshTokenExchange,
    ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>> {
        Box::pin(async move {
            api::refresh_soundcloud_token(request)
                .await
                .map_err(|error| broker_server_error(error.to_string()))
        })
    }
}

fn broker_server_error(rendered: String) -> IntegrationError {
    let lower = rendered.to_ascii_lowercase();
    let rate_limited = lower.contains("rate limit") || lower.contains("429");
    let unavailable = lower.contains("503") || lower.contains("temporarily");
    let unreachable = [
        "connection",
        "connect",
        "network",
        "transport",
        "timed out",
        "timeout",
        "request failed",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let (message, retryable) = if rate_limited {
        (
            "SoundCloud authorization is rate limited; try again shortly",
            true,
        )
    } else if unavailable {
        (
            "SoundCloud authorization service is temporarily unavailable",
            true,
        )
    } else if unreachable {
        (
            "SoundCloud authorization broker could not be reached. Start or deploy the rec_bag fullstack web service, then retry.",
            true,
        )
    } else {
        ("SoundCloud authorization was rejected", false)
    };
    integration_error(message, retryable)
}

/// Factory boundary allowing connector fixtures without HTTP.
pub trait SoundCloudConnectorFactory: Send + Sync {
    fn create(
        &self,
        source_id: Uuid,
        tokens: Arc<dyn SoundCloudTokenProvider>,
    ) -> Result<Arc<dyn CatalogSourceConnector>, IntegrationError>;
}

#[derive(Default)]
pub struct ProductionSoundCloudConnectorFactory;

impl SoundCloudConnectorFactory for ProductionSoundCloudConnectorFactory {
    fn create(
        &self,
        source_id: Uuid,
        tokens: Arc<dyn SoundCloudTokenProvider>,
    ) -> Result<Arc<dyn CatalogSourceConnector>, IntegrationError> {
        let client = SoundCloudClient::new(Arc::new(ReqwestSoundCloudTransport::default()), tokens)
            .map_err(source_error)?;
        Ok(Arc::new(SoundCloudConnector::new(source_id, client)))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredSoundCloudCredentials {
    access_token: String,
    refresh_token: String,
    expires_at: Option<DateTime<Utc>>,
    scope: Option<String>,
    token_type: Option<String>,
    generation: u64,
}

impl std::fmt::Debug for StoredSoundCloudCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredSoundCloudCredentials")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .field("generation", &self.generation)
            .finish()
    }
}

impl StoredSoundCloudCredentials {
    fn from_response(response: SoundCloudTokenResponse, generation: u64) -> Self {
        let expires_at = response
            .expires_in
            .and_then(|seconds| i64::try_from(seconds).ok())
            .and_then(|seconds| Utc::now().checked_add_signed(ChronoDuration::seconds(seconds)));
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at,
            scope: response.scope,
            token_type: response.token_type,
            generation,
        }
    }

    fn encode(&self) -> Result<Secret, IntegrationError> {
        serde_json::to_vec(self)
            .map(Secret::new)
            .map_err(|_| integration_error("Could not encode SoundCloud credentials", false))
    }

    fn decode(secret: Secret) -> Result<Self, IntegrationError> {
        serde_json::from_slice(secret.expose())
            .map_err(|_| integration_error("Stored SoundCloud credentials are invalid", false))
    }
}

fn credential_key(source_id: Uuid) -> CredentialKey {
    CredentialKey {
        provider: PROVIDER.to_string(),
        account: source_id.to_string(),
    }
}

fn credential_reference(source_id: Uuid) -> String {
    format!("keyring:{PROVIDER}/{source_id}")
}

/// In-memory credential implementation for injected tests and ephemeral previews.
#[derive(Default)]
pub struct MemoryCredentialStore {
    values: Mutex<BTreeMap<CredentialKey, Secret>>,
}

impl CredentialStore for MemoryCredentialStore {
    fn load<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, Option<Secret>> {
        Box::pin(async move {
            self.values
                .lock()
                .map_err(|_| integration_error("Credential store is unavailable", true))
                .map(|values| values.get(key).cloned())
        })
    }

    fn store<'a>(
        &'a self,
        key: &'a CredentialKey,
        secret: Secret,
    ) -> super::integration::CredentialFuture<'a, ()> {
        Box::pin(async move {
            self.values
                .lock()
                .map_err(|_| integration_error("Credential store is unavailable", true))?
                .insert(key.clone(), secret);
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, ()> {
        Box::pin(async move {
            self.values
                .lock()
                .map_err(|_| integration_error("Credential store is unavailable", true))?
                .remove(key);
            Ok(())
        })
    }
}

/// Native OS credential-store adapter. The complete token generation is stored as one value so
/// access and single-use refresh tokens cannot be observed from different generations.
#[cfg(feature = "desktop")]
#[derive(Clone)]
pub struct KeyringCredentialStore {
    service: String,
}

#[cfg(feature = "desktop")]
impl Default for KeyringCredentialStore {
    fn default() -> Self {
        Self {
            service: "rec_bag".to_string(),
        }
    }
}

#[cfg(feature = "desktop")]
impl KeyringCredentialStore {
    fn entry(&self, key: &CredentialKey) -> Result<keyring::Entry, IntegrationError> {
        keyring::Entry::new(&format!("{}:{}", self.service, key.provider), &key.account)
            .map_err(|_| integration_error("OS credential store is unavailable", true))
    }
}

#[cfg(feature = "desktop")]
impl CredentialStore for KeyringCredentialStore {
    fn load<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, Option<Secret>> {
        let entry = self.entry(key);
        Box::pin(async move {
            let entry = entry?;
            tokio::task::spawn_blocking(move || match entry.get_password() {
                Ok(value) => Ok(Some(Secret::new(value.into_bytes()))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(integration_error("Could not read OS credentials", true)),
            })
            .await
            .map_err(|_| integration_error("Credential-store task failed", true))?
        })
    }

    fn store<'a>(
        &'a self,
        key: &'a CredentialKey,
        secret: Secret,
    ) -> super::integration::CredentialFuture<'a, ()> {
        let entry = self.entry(key);
        let value = String::from_utf8(secret.expose().to_vec())
            .map_err(|_| integration_error("Credential payload is invalid", false));
        Box::pin(async move {
            let entry = entry?;
            let value = value?;
            tokio::task::spawn_blocking(move || {
                entry
                    .set_password(&value)
                    .map_err(|_| integration_error("Could not update OS credentials", true))
            })
            .await
            .map_err(|_| integration_error("Credential-store task failed", true))?
        })
    }

    fn delete<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, ()> {
        let entry = self.entry(key);
        Box::pin(async move {
            let entry = entry?;
            tokio::task::spawn_blocking(move || match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(integration_error("Could not delete OS credentials", true)),
            })
            .await
            .map_err(|_| integration_error("Credential-store task failed", true))?
        })
    }
}

/// Loopback browser callback implementation for native desktop builds.
#[cfg(feature = "desktop")]
#[derive(Default)]
pub struct LoopbackOAuthUserAgent;

#[cfg(feature = "desktop")]
impl OAuthUserAgent for LoopbackOAuthUserAgent {
    fn authorize(
        &self,
        authorization_url: Url,
        redirect_uri: Url,
        timeout: std::time::Duration,
    ) -> BoxFuture<'_, Result<Url, IntegrationError>> {
        Box::pin(async move {
            if redirect_uri.scheme() != "http"
                || !redirect_uri.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })
            {
                return Err(integration_error(
                    "The built-in callback receiver requires a registered loopback HTTP redirect",
                    false,
                ));
            }
            let port = redirect_uri
                .port_or_known_default()
                .ok_or_else(|| integration_error("Callback redirect has no port", false))?;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .map_err(|_| integration_error("Could not bind the OAuth callback port", true))?;
            webbrowser::open(authorization_url.as_str())
                .map_err(|_| integration_error("Could not open the system browser", true))?;
            let (mut stream, _) = tokio::time::timeout(timeout, listener.accept())
                .await
                .map_err(|_| integration_error("SoundCloud authorization timed out", true))?
                .map_err(|_| integration_error("OAuth callback could not be received", true))?;
            let mut bytes = vec![0_u8; MAX_CALLBACK_URL_BYTES];
            let count = tokio::io::AsyncReadExt::read(&mut stream, &mut bytes)
                .await
                .map_err(|_| integration_error("OAuth callback could not be read", true))?;
            let request = std::str::from_utf8(&bytes[..count])
                .map_err(|_| integration_error("OAuth callback was malformed", false))?;
            let target = request
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|line| line.split_once(' ').map(|(target, _)| target))
                .ok_or_else(|| integration_error("OAuth callback was malformed", false))?;
            let callback = redirect_uri
                .join(target)
                .map_err(|_| integration_error("OAuth callback URL was malformed", false))?;
            let valid_path = callback.path() == redirect_uri.path();
            let response = if valid_path {
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: 63\r\n\r\nAuthorization received. You can close this window and return to rec_bag."
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: 25\r\n\r\nInvalid callback request."
            };
            let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            if !valid_path {
                return Err(integration_error(
                    "OAuth callback path did not match",
                    false,
                ));
            }
            Ok(callback)
        })
    }
}

struct PendingAuthorization {
    state: String,
    verifier: String,
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PendingAuthorization([REDACTED])")
    }
}

struct CredentialTokenProvider {
    source_id: Uuid,
    credentials: Arc<dyn CredentialStore>,
    broker: Arc<dyn SoundCloudBrokerClient>,
    catalog: Arc<TursoCatalog>,
    refresh_lock: AsyncMutex<()>,
}

impl CredentialTokenProvider {
    async fn load(&self) -> Result<StoredSoundCloudCredentials, IntegrationError> {
        let secret = self
            .credentials
            .load(&credential_key(self.source_id))
            .await?
            .ok_or_else(|| integration_error("SoundCloud authorization is required", false))?;
        StoredSoundCloudCredentials::decode(secret)
    }

    async fn persist_rotation(
        &self,
        value: StoredSoundCloudCredentials,
    ) -> Result<(), IntegrationError> {
        self.credentials
            .store(&credential_key(self.source_id), value.encode()?)
            .await?;
        self.catalog
            .set_refresh_token_metadata(refresh_metadata(self.source_id, &value, Utc::now()))
            .await
            .map_err(repository_error)
    }
}

impl SoundCloudTokenProvider for CredentialTokenProvider {
    fn access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        Box::pin(async move {
            self.load()
                .await
                .map(|credentials| credentials.access_token)
                .map_err(integration_to_source_error)
        })
    }

    fn refresh_access_token(&self) -> BoxFuture<'_, Result<String, SourceError>> {
        Box::pin(async move {
            let _guard = self.refresh_lock.lock().await;
            let current = self.load().await.map_err(integration_to_source_error)?;
            let response = self
                .broker
                .refresh_token(SoundCloudRefreshTokenExchange {
                    refresh_token: current.refresh_token,
                })
                .await
                .map_err(integration_to_source_error)?;
            let rotated = StoredSoundCloudCredentials::from_response(
                response,
                current.generation.saturating_add(1),
            );
            let access_token = rotated.access_token.clone();
            self.persist_rotation(rotated)
                .await
                .map_err(integration_to_source_error)?;
            Ok(access_token)
        })
    }
}

fn integration_to_source_error(error: IntegrationError) -> SourceError {
    SourceError::new(
        if error.retryable {
            SourceErrorKind::TemporarilyUnavailable
        } else {
            SourceErrorKind::AuthenticationRequired
        },
        error.message,
    )
}

fn source_error(error: SourceError) -> IntegrationError {
    let message = match error.kind {
        SourceErrorKind::AuthenticationRequired => {
            "SoundCloud authorization is required".to_string()
        }
        SourceErrorKind::RateLimited => {
            "SoundCloud is rate limiting requests; try again shortly".to_string()
        }
        SourceErrorKind::TemporarilyUnavailable => {
            "SoundCloud is temporarily unavailable".to_string()
        }
        _ => error.message.clone(),
    };
    integration_error(message, error.is_retryable())
}

fn refresh_metadata(
    source_id: Uuid,
    credentials: &StoredSoundCloudCredentials,
    now: DateTime<Utc>,
) -> RefreshTokenMetadata {
    RefreshTokenMetadata {
        source_id,
        credential_reference: credential_reference(source_id),
        refresh_generation: credentials.generation,
        last_refreshed_at: (credentials.generation > 0).then_some(now),
        expires_at: credentials.expires_at,
        updated_at: now,
    }
}

/// Complete SoundCloud vertical-slice service.
pub struct SoundCloudCatalogService {
    config: SoundCloudOAuthClientConfig,
    user_agent: Arc<dyn OAuthUserAgent>,
    credentials: Arc<dyn CredentialStore>,
    broker: Arc<dyn SoundCloudBrokerClient>,
    connectors: Arc<dyn SoundCloudConnectorFactory>,
    sign_out: Arc<dyn SoundCloudSignOutClient>,
    catalog: Arc<TursoCatalog>,
    pending: Mutex<HashMap<Uuid, PendingAuthorization>>,
    operation_lock: AsyncMutex<()>,
    view: AsyncMutex<MusicManagementView>,
}

impl SoundCloudCatalogService {
    pub fn new(
        config: SoundCloudOAuthClientConfig,
        user_agent: Arc<dyn OAuthUserAgent>,
        credentials: Arc<dyn CredentialStore>,
        broker: Arc<dyn SoundCloudBrokerClient>,
        connectors: Arc<dyn SoundCloudConnectorFactory>,
        catalog: Arc<TursoCatalog>,
    ) -> Self {
        Self {
            config,
            user_agent,
            credentials,
            broker,
            connectors,
            sign_out: default_sign_out_client(),
            catalog,
            pending: Mutex::new(HashMap::new()),
            operation_lock: AsyncMutex::new(()),
            view: AsyncMutex::new(empty_view()),
        }
    }

    pub fn with_sign_out_client(mut self, sign_out: Arc<dyn SoundCloudSignOutClient>) -> Self {
        self.sign_out = sign_out;
        self
    }

    pub async fn connect(&self) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        let mut source = self
            .existing_soundcloud_source()
            .await?
            .unwrap_or_else(|| CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud));
        let source_id = source.id;
        let (state, verifier, challenge) = generate_pkce()?;
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| integration_error("OAuth state store is unavailable", true))?;
            if pending.contains_key(&source_id) {
                return Err(integration_error(
                    "SoundCloud authorization is already in progress",
                    false,
                ));
            }
            pending.insert(
                source_id,
                PendingAuthorization {
                    state: state.clone(),
                    verifier,
                },
            );
        }
        let authorization_url = self.authorization_url(&state, &challenge);
        let callback = self
            .user_agent
            .authorize(
                authorization_url,
                self.config.redirect_uri.clone(),
                self.config.callback_timeout,
            )
            .await;
        let pending = self
            .pending
            .lock()
            .map_err(|_| integration_error("OAuth state store is unavailable", true))?
            .remove(&source_id)
            .ok_or_else(|| integration_error("OAuth authorization state expired", false))?;
        let callback = callback?;
        if !same_callback_endpoint(&callback, &self.config.redirect_uri) {
            return Err(integration_error(
                "OAuth callback endpoint did not match the registered redirect",
                false,
            ));
        }
        let parsed = parse_callback(&callback)?;
        if pending
            .state
            .as_bytes()
            .ct_eq(parsed.state.as_bytes())
            .unwrap_u8()
            != 1
        {
            return Err(integration_error(
                "OAuth callback state did not match",
                false,
            ));
        }
        if parsed.rejected {
            return Err(integration_error(
                "SoundCloud authorization was cancelled or rejected",
                false,
            ));
        }
        let code = parsed
            .code
            .ok_or_else(|| integration_error("OAuth callback did not contain a code", false))?;
        let response = self
            .broker
            .exchange_authorization_code(SoundCloudAuthorizationCodeExchange {
                code,
                code_verifier: pending.verifier,
                redirect_uri: self.config.redirect_uri.to_string(),
                state: parsed.state,
            })
            .await?;
        let stored = StoredSoundCloudCredentials::from_response(response, 0);
        self.credentials
            .store(&credential_key(source_id), stored.encode()?)
            .await?;
        source.enabled = true;
        source.config.remove(FORGOTTEN_CONFIG_KEY);
        source.capabilities = soundcloud_capabilities();
        if let Err(error) = SourceRepository::upsert(self.catalog.as_ref(), source).await {
            let _ = self.credentials.delete(&credential_key(source_id)).await;
            return Err(repository_error(error));
        }
        if let Err(error) = self
            .catalog
            .set_refresh_token_metadata(refresh_metadata(source_id, &stored, Utc::now()))
            .await
        {
            let _ = self.credentials.delete(&credential_key(source_id)).await;
            return Err(repository_error(error));
        }
        self.sync_locked(source_id, true).await
    }

    pub async fn sync(&self, source_id: Uuid) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        self.sync_locked(source_id, false).await
    }

    pub async fn disconnect(
        &self,
        source_id: Uuid,
    ) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        self.sign_out_and_disable(source_id, false).await?;
        self.project(None).await
    }

    pub async fn revoke_provider_grant(
        &self,
        source_id: Uuid,
    ) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        self.sign_out_and_disable(source_id, true).await?;
        self.project(None).await
    }

    pub async fn clear_local_cache(
        &self,
        source_id: Uuid,
    ) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        self.require_soundcloud_source(source_id).await?;
        let transaction = self.catalog.begin().await.map_err(repository_error)?;
        let result = clear_source_checkpoints(transaction.as_ref(), source_id).await;
        match result {
            Ok(()) => transaction.commit().await.map_err(repository_error)?,
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(error);
            }
        }
        self.pending
            .lock()
            .map_err(|_| integration_error("OAuth state store is unavailable", true))?
            .remove(&source_id);
        self.project(None).await
    }

    pub async fn forget_provider_catalog_data(
        &self,
        source_id: Uuid,
    ) -> Result<MusicManagementView, IntegrationError> {
        let _operation = self.operation_lock.lock().await;
        if let Some(source) = SourceRepository::get(self.catalog.as_ref(), source_id)
            .await
            .map_err(repository_error)?
        {
            if source.kind != CatalogSourceKind::SoundCloud {
                return Err(integration_error("Source is not SoundCloud", false));
            }
        }
        self.pending
            .lock()
            .map_err(|_| integration_error("OAuth state store is unavailable", true))?
            .remove(&source_id);
        self.catalog
            .delete_source_provider_data(source_id)
            .await
            .map_err(repository_error)?;
        self.project(None).await
    }

    async fn sign_out_and_disable(
        &self,
        source_id: Uuid,
        require_remote_revocation: bool,
    ) -> Result<(), IntegrationError> {
        let mut source = self.require_soundcloud_source(source_id).await?;
        if let Some(secret) = self.credentials.load(&credential_key(source_id)).await? {
            let credentials = StoredSoundCloudCredentials::decode(secret)?;
            let result = self
                .sign_out
                .revoke_grant(Secret::new(credentials.access_token.into_bytes()))
                .await
                .map_err(|error| {
                    integration_error("SoundCloud remote sign-out failed", error.retryable)
                })?;
            if require_remote_revocation && result == ProviderGrantRevocation::Unsupported {
                return Err(integration_error(
                    "SoundCloud remote sign-out is unavailable; local credentials were retained",
                    true,
                ));
            }
        }
        self.credentials.delete(&credential_key(source_id)).await?;
        source.enabled = false;
        SourceRepository::upsert(self.catalog.as_ref(), source)
            .await
            .map_err(repository_error)
    }

    async fn require_soundcloud_source(
        &self,
        source_id: Uuid,
    ) -> Result<CatalogSource, IntegrationError> {
        let source = SourceRepository::get(self.catalog.as_ref(), source_id)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| integration_error("SoundCloud source was not found", false))?;
        if source.kind != CatalogSourceKind::SoundCloud {
            return Err(integration_error(
                "Source is not a SoundCloud catalog",
                false,
            ));
        }
        Ok(source)
    }

    pub async fn refresh_view(&self) -> Result<MusicManagementView, IntegrationError> {
        self.project(None).await
    }

    async fn existing_soundcloud_source(&self) -> Result<Option<CatalogSource>, IntegrationError> {
        let mut cursor = None;
        loop {
            let page = SourceRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(100),
                },
            )
            .await
            .map_err(repository_error)?;
            if let Some(source) = page
                .items
                .into_iter()
                .find(|source| source.kind == CatalogSourceKind::SoundCloud)
            {
                return Ok(Some(source));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(None);
            }
        }
    }

    fn token_provider(&self, source_id: Uuid) -> Arc<dyn SoundCloudTokenProvider> {
        Arc::new(CredentialTokenProvider {
            source_id,
            credentials: self.credentials.clone(),
            broker: self.broker.clone(),
            catalog: self.catalog.clone(),
            refresh_lock: AsyncMutex::new(()),
        })
    }

    async fn sync_locked(
        &self,
        source_id: Uuid,
        force_full: bool,
    ) -> Result<MusicManagementView, IntegrationError> {
        let mut source = SourceRepository::get(self.catalog.as_ref(), source_id)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| integration_error("SoundCloud source was not found", false))?;
        if source.kind != CatalogSourceKind::SoundCloud || !source.enabled {
            return Err(integration_error(
                "SoundCloud source is disconnected",
                false,
            ));
        }
        if self
            .credentials
            .load(&credential_key(source_id))
            .await?
            .is_none()
        {
            return Err(integration_error(
                "SoundCloud authorization is required",
                false,
            ));
        }
        let connector = self
            .connectors
            .create(source_id, self.token_provider(source_id))?;
        let health = connector.health().await.map_err(source_error)?;
        if health.status == HealthStatus::AuthenticationRequired {
            return Err(integration_error(
                "SoundCloud authorization is required",
                false,
            ));
        }
        source.capabilities = health.capabilities.clone();
        SourceRepository::upsert(self.catalog.as_ref(), source.clone())
            .await
            .map_err(repository_error)?;

        let previous = if force_full {
            None
        } else {
            SyncRunRepository::latest(
                self.catalog.as_ref(),
                SyncRunFilter {
                    kind: Some(SyncRunKind::CatalogSync),
                    status: Some(SyncStatus::Succeeded),
                    source_id: Some(source_id),
                    storage_id: None,
                },
            )
            .await
            .map_err(repository_error)?
        };
        let base_cursor = previous.as_ref().and_then(|run| run.sync_cursor.clone());
        let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source_id), None);
        run.start()
            .map_err(|error| integration_error(error.to_string(), false))?;
        SyncRunRepository::upsert(self.catalog.as_ref(), run.clone())
            .await
            .map_err(repository_error)?;

        let mode = base_cursor
            .clone()
            .map(|cursor| SourceSyncMode::Incremental { cursor })
            .unwrap_or(SourceSyncMode::Full);
        let mut page_cursor = None;
        loop {
            let page = match connector
                .sync_page(SourceSyncRequest {
                    mode: mode.clone(),
                    collections: Vec::new(),
                    page_cursor: page_cursor.clone(),
                    page_size: Some(DEFAULT_PAGE_SIZE),
                })
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    let integration = source_error(error.clone());
                    let _ = run.fail(sync_error(&error));
                    let _ = SyncRunRepository::upsert(self.catalog.as_ref(), run).await;
                    return Err(integration);
                }
            };
            let next_page = page.next_page_cursor.clone();
            let complete_cursor = page.next_sync_cursor.clone();
            if let Err(error) = self
                .apply_page(source_id, page, &mut run, base_cursor.clone())
                .await
            {
                let _ = run.fail(SyncError::new(
                    SyncErrorKind::Internal,
                    "soundcloud_persistence",
                    error.message.clone(),
                    error.retryable,
                ));
                let _ = SyncRunRepository::upsert(self.catalog.as_ref(), run).await;
                return Err(error);
            }
            if next_page.is_none() {
                if complete_cursor.is_none() {
                    return Err(integration_error(
                        "SoundCloud completed without a durable cursor",
                        false,
                    ));
                }
                break;
            }
            page_cursor = next_page;
        }
        run.succeed()
            .map_err(|error| integration_error(error.to_string(), false))?;
        source.last_synced_at = run.finished_at;
        let transaction = self.catalog.begin().await.map_err(repository_error)?;
        transaction
            .sources()
            .upsert(source)
            .await
            .map_err(repository_error)?;
        transaction
            .sync_runs()
            .upsert(run)
            .await
            .map_err(repository_error)?;
        transaction.commit().await.map_err(repository_error)?;
        self.project(Some(health)).await
    }

    async fn apply_page(
        &self,
        source_id: Uuid,
        page: SourceSyncPage,
        run: &mut SyncRun,
        base_cursor: Option<rec_core::SyncCursor>,
    ) -> Result<(), IntegrationError> {
        let SourceSyncPage {
            items,
            removals,
            next_page_cursor,
            next_sync_cursor,
            progress,
        } = page;
        let transaction = self.catalog.begin().await.map_err(repository_error)?;
        let result = self
            .apply_items(transaction.as_ref(), source_id, items, removals)
            .await;
        if let Err(error) = result {
            let _ = transaction.rollback().await;
            return Err(error);
        }
        if let Some(page_progress) = progress {
            add_progress(&mut run.progress, page_progress);
        }
        let mut checkpoint = SyncCheckpoint::new(run.progress.processed_items);
        if let Some(cursor) = base_cursor.or_else(|| run.sync_cursor.clone()) {
            checkpoint = checkpoint.with_sync_cursor(cursor);
        }
        if let Some(cursor) = next_sync_cursor {
            checkpoint = checkpoint.with_completed_sequence(cursor);
        } else if let Some(cursor) = next_page_cursor {
            checkpoint = checkpoint.with_page_cursor(cursor);
        }
        run.update_checkpoint(checkpoint);
        transaction
            .sync_runs()
            .upsert(run.clone())
            .await
            .map_err(repository_error)?;
        transaction.commit().await.map_err(repository_error)
    }

    async fn apply_items(
        &self,
        transaction: &dyn RepositoryTransaction,
        source_id: Uuid,
        items: Vec<SourceSyncItem>,
        removals: Vec<rec_core::SourceRemoval>,
    ) -> Result<(), IntegrationError> {
        let mut accounts = Vec::new();
        let mut occurrences = Vec::new();
        let mut playlists: BTreeMap<String, ExternalPlaylist> = BTreeMap::new();
        let mut entries: BTreeMap<String, Vec<rec_core::ExternalPlaylistEntry>> = BTreeMap::new();
        let mut relationships = Vec::new();
        let mut relationship_removals = Vec::new();
        for item in items {
            match item {
                SourceSyncItem::Account(value) => accounts.push(value),
                SourceSyncItem::Track(value) => occurrences.push(value),
                SourceSyncItem::Playlist(value) => {
                    playlists.insert(value.external_id.clone(), value);
                }
                SourceSyncItem::PlaylistEntry {
                    playlist_external_id,
                    entry,
                    ..
                } => entries.entry(playlist_external_id).or_default().push(entry),
                SourceSyncItem::Relationship(value) => relationships.push(value),
                SourceSyncItem::RelationshipRemoval(value) => relationship_removals.push(value),
            }
        }
        for account in accounts {
            if account.source_id != source_id {
                return Err(integration_error(
                    "SoundCloud account scope mismatch",
                    false,
                ));
            }
            transaction
                .sources()
                .upsert_account(account)
                .await
                .map_err(repository_error)?;
        }
        for occurrence in &mut occurrences {
            if occurrence.identity.source_id != source_id {
                return Err(integration_error("SoundCloud track scope mismatch", false));
            }
            canonicalize_occurrence(transaction, occurrence).await?;
        }
        transaction
            .sources()
            .upsert_occurrences(occurrences)
            .await
            .map_err(repository_error)?;
        for (external_id, mut playlist) in playlists {
            if playlist.source_id != source_id {
                return Err(integration_error(
                    "SoundCloud playlist scope mismatch",
                    false,
                ));
            }
            if let Some(mut emitted) = entries.remove(&external_id) {
                emitted.sort_by_key(|entry| entry.position);
                playlist.entries = emitted;
            }
            transaction
                .playlists()
                .upsert_external(playlist)
                .await
                .map_err(repository_error)?;
        }
        if !entries.is_empty() {
            return Err(integration_error(
                "SoundCloud page contained entries for an unknown playlist",
                false,
            ));
        }
        for relationship in relationships {
            if relationship.identity.source_id != source_id {
                return Err(integration_error(
                    "SoundCloud relationship scope mismatch",
                    false,
                ));
            }
            transaction
                .sources()
                .upsert_collection_relationship(relationship)
                .await
                .map_err(repository_error)?;
        }
        for identity in relationship_removals {
            if identity.source_id != source_id {
                return Err(integration_error(
                    "SoundCloud relationship scope mismatch",
                    false,
                ));
            }
            transaction
                .sources()
                .remove_collection_relationship(identity)
                .await
                .map_err(repository_error)?;
        }
        for removal in removals {
            if removal.entity_kind == SourceEntityKind::Track {
                if let Some(mut occurrence) = transaction
                    .sources()
                    .get_occurrence(source_id, removal.external_id)
                    .await
                    .map_err(repository_error)?
                {
                    occurrence.availability = SourceAvailability::Removed;
                    transaction
                        .sources()
                        .upsert_occurrences(vec![occurrence])
                        .await
                        .map_err(repository_error)?;
                }
            }
        }
        Ok(())
    }

    fn authorization_url(&self, state: &str, challenge: &str) -> Url {
        let mut url = self.config.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", self.config.redirect_uri.as_str())
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        url
    }

    async fn project(
        &self,
        known_health: Option<SourceHealth>,
    ) -> Result<MusicManagementView, IntegrationError> {
        let mut projected_sources = Vec::new();
        let mut projected_tracks = Vec::new();
        let mut playlist_completeness = Vec::new();
        let mut canonical_ids = BTreeSet::new();
        let mut counts = CompletenessView {
            canonical_tracks: 0,
            catalog_covered: 0,
            verified_assets: 0,
            missing_obtainable: 0,
            streaming_only: 0,
            restricted: 0,
            possible_matches: 0,
            ambiguous_matches: 0,
            unknown_primary_files: 0,
            backup_gaps: 0,
            provider_errors: 0,
            playlists: Vec::new(),
        };
        let mut source_cursor = None;
        loop {
            let page = SourceRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor: source_cursor,
                    limit: Some(100),
                },
            )
            .await
            .map_err(repository_error)?;
            for source in page
                .items
                .into_iter()
                .filter(|source| source.kind == CatalogSourceKind::SoundCloud)
            {
                let connected = self
                    .credentials
                    .load(&credential_key(source.id))
                    .await?
                    .is_some();
                let accounts = collect_accounts(self.catalog.as_ref(), source.id).await?;
                let account_identity = accounts.first().and_then(|account| {
                    account
                        .username
                        .as_ref()
                        .map(|username| format!("@{username}"))
                        .or_else(|| account.display_name.clone())
                });
                let latest_run = SyncRunRepository::latest(
                    self.catalog.as_ref(),
                    SyncRunFilter {
                        kind: Some(SyncRunKind::CatalogSync),
                        status: None,
                        source_id: Some(source.id),
                        storage_id: None,
                    },
                )
                .await
                .map_err(repository_error)?;
                let health = if known_health.is_some() {
                    known_health.clone()
                } else {
                    None
                };
                let forgotten = source.config.contains_key(FORGOTTEN_CONFIG_KEY);
                let collections = collection_views(self.catalog.as_ref(), source.id).await?;
                let occurrences = if forgotten {
                    Vec::new()
                } else {
                    collect_occurrences(self.catalog.as_ref(), source.id).await?
                };
                let mut status_by_track = HashMap::new();
                for occurrence in occurrences {
                    let Some(track_id) = occurrence.canonical_track_id else {
                        continue;
                    };
                    canonical_ids.insert(track_id);
                    let verified = has_verified_original(self.catalog.as_ref(), track_id).await?;
                    let status = availability_status(occurrence.availability, verified);
                    status_by_track.entry(track_id).or_insert(status);
                    let track = TrackRepository::get(self.catalog.as_ref(), track_id)
                        .await
                        .map_err(repository_error)?;
                    let (title, artist) =
                        track.as_ref().map(track_title_artist).unwrap_or_else(|| {
                            (
                                "Untitled SoundCloud track".to_string(),
                                "Unknown artist".to_string(),
                            )
                        });
                    projected_tracks.push(TrackStorageView {
                        id: track_id.to_string(),
                        title,
                        artist,
                        status,
                        badges: vec![status_badge(status).to_string(), "SoundCloud".to_string()],
                        pinned_offline: false,
                    });
                }
                for status in status_by_track.values().copied() {
                    counts.catalog_covered = counts.catalog_covered.saturating_add(1);
                    match status {
                        TrackStorageStatusView::StoredAndVerified => {
                            counts.verified_assets = counts.verified_assets.saturating_add(1)
                        }
                        TrackStorageStatusView::MissingObtainable => {
                            counts.missing_obtainable = counts.missing_obtainable.saturating_add(1)
                        }
                        TrackStorageStatusView::NotAvailableForArchival => {
                            counts.streaming_only = counts.streaming_only.saturating_add(1)
                        }
                        TrackStorageStatusView::PreviewOnly
                        | TrackStorageStatusView::Blocked
                        | TrackStorageStatusView::Removed
                        | TrackStorageStatusView::Inaccessible => {
                            counts.restricted = counts.restricted.saturating_add(1)
                        }
                        _ => {}
                    }
                }
                let playlists = if forgotten {
                    Vec::new()
                } else {
                    collect_playlists(self.catalog.as_ref(), source.id).await?
                };
                for playlist in playlists {
                    let mut archived = 0_u32;
                    let mut total = 0_u32;
                    for entry in &playlist.entries {
                        total = total.saturating_add(1);
                        if let Some(occurrence) = SourceRepository::get_occurrence(
                            self.catalog.as_ref(),
                            source.id,
                            entry.track.external_id.clone(),
                        )
                        .await
                        .map_err(repository_error)?
                        {
                            if occurrence.canonical_track_id.is_some_and(|id| {
                                status_by_track.get(&id)
                                    == Some(&TrackStorageStatusView::StoredAndVerified)
                            }) {
                                archived = archived.saturating_add(1);
                            }
                        }
                    }
                    playlist_completeness.push(PlaylistCompletenessView {
                        id: format!("{}:{}", source.id, playlist.external_id),
                        name: playlist.name,
                        archived_tracks: archived,
                        total_tracks: total,
                        needs_review: 0,
                    });
                }
                projected_sources.push(source_view(
                    &source,
                    account_identity,
                    connected,
                    health.as_ref(),
                    latest_run.as_ref(),
                    collections,
                ));
            }
            source_cursor = page.next_cursor;
            if source_cursor.is_none() {
                break;
            }
        }
        projected_tracks.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        projected_tracks.dedup_by(|a, b| a.id == b.id);
        counts.canonical_tracks = saturating_u32(canonical_ids.len() as u64);
        counts.playlists = playlist_completeness;
        let mut view = self.view.lock().await;
        view.sources
            .retain(|source| source.provider != ProviderChoice::SoundCloud);
        view.sources.extend(projected_sources);
        view.tracks
            .retain(|track| !track.badges.iter().any(|badge| badge == "SoundCloud"));
        view.tracks.extend(projected_tracks);
        view.completeness = counts;
        Ok(view.clone())
    }
}

async fn clear_source_checkpoints(
    transaction: &dyn RepositoryTransaction,
    source_id: Uuid,
) -> Result<(), IntegrationError> {
    let mut cursor = None;
    let mut runs = Vec::new();
    loop {
        let page = transaction
            .sync_runs()
            .list_filtered(
                SyncRunFilter {
                    kind: Some(SyncRunKind::CatalogSync),
                    status: None,
                    source_id: Some(source_id),
                    storage_id: None,
                },
                RepositoryPageRequest {
                    cursor,
                    limit: Some(200),
                },
            )
            .await
            .map_err(repository_error)?;
        runs.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    for mut run in runs {
        run.sync_cursor = None;
        run.page_cursor = None;
        run.checkpoint = None;
        run.resume_from = None;
        let jobs = transaction
            .sync_runs()
            .list_jobs(run.id)
            .await
            .map_err(repository_error)?;
        for mut job in jobs {
            job.checkpoint = None;
            transaction
                .sync_runs()
                .upsert_job(job)
                .await
                .map_err(repository_error)?;
        }
        transaction
            .sync_runs()
            .upsert(run)
            .await
            .map_err(repository_error)?;
    }
    Ok(())
}

impl IntegrationSliceHandler for SoundCloudCatalogService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::SoundcloudCatalog
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            if request.slice != IntegrationSlice::SoundcloudCatalog {
                return Err(request_error(
                    &request,
                    "Request belongs to another integration slice",
                    false,
                ));
            }
            let result = match &request.event {
                MusicManagementEvent::AddSource(ProviderChoice::SoundCloud) => self.connect().await,
                MusicManagementEvent::SyncSource(id) => {
                    parse_source_id(id).and_then_async(|id| self.sync(id)).await
                }
                MusicManagementEvent::DisconnectSource { source_id } => {
                    parse_source_id(source_id)
                        .and_then_async(|id| self.disconnect(id))
                        .await
                }
                MusicManagementEvent::RevokeProviderGrant {
                    provider: ProviderChoice::SoundCloud,
                    connection_id,
                } => {
                    parse_source_id(connection_id)
                        .and_then_async(|id| self.revoke_provider_grant(id))
                        .await
                }
                MusicManagementEvent::ClearLocalCache { source_id } => {
                    parse_source_id(source_id)
                        .and_then_async(|id| self.clear_local_cache(id))
                        .await
                }
                MusicManagementEvent::ForgetProviderCatalogData { source_id } => {
                    parse_source_id(source_id)
                        .and_then_async(|id| self.forget_provider_catalog_data(id))
                        .await
                }
                _ => Err(integration_error(
                    "Unsupported SoundCloud catalog operation",
                    false,
                )),
            };
            result
                .map(|view| IntegrationOutcome {
                    view: Some(view),
                    message: Some(match request.event {
                        MusicManagementEvent::DisconnectSource { .. } => {
                            "SoundCloud disconnected; imported catalog data was preserved"
                                .to_string()
                        }
                        MusicManagementEvent::RevokeProviderGrant { .. } => {
                            "SoundCloud authorization was revoked and local credentials removed"
                                .to_string()
                        }
                        MusicManagementEvent::ClearLocalCache { .. } => {
                            "SoundCloud local cache and checkpoints were cleared".to_string()
                        }
                        MusicManagementEvent::ForgetProviderCatalogData { .. } => {
                            "SoundCloud local provider data deleted; canonical tracks, owned assets, and authorization were retained".to_string()
                        }
                        MusicManagementEvent::AddSource(_) => {
                            "SoundCloud connected and synchronized".to_string()
                        }
                        _ => "SoundCloud catalog synchronized".to_string(),
                    }),
                })
                .map_err(|mut error| {
                    error.request_id = Some(request.id);
                    error.slice = Some(request.slice);
                    error
                })
        })
    }
}

trait ResultAsyncExt<T> {
    fn and_then_async<'a, F, Fut>(
        self,
        function: F,
    ) -> BoxFuture<'a, Result<MusicManagementView, IntegrationError>>
    where
        Self: Sized + 'a,
        T: 'a,
        F: FnOnce(T) -> Fut + Send + 'a,
        Fut:
            std::future::Future<Output = Result<MusicManagementView, IntegrationError>> + Send + 'a;
}

impl<T: Send> ResultAsyncExt<T> for Result<T, IntegrationError> {
    fn and_then_async<'a, F, Fut>(
        self,
        function: F,
    ) -> BoxFuture<'a, Result<MusicManagementView, IntegrationError>>
    where
        Self: 'a,
        T: 'a,
        F: FnOnce(T) -> Fut + Send + 'a,
        Fut:
            std::future::Future<Output = Result<MusicManagementView, IntegrationError>> + Send + 'a,
    {
        Box::pin(async move {
            match self {
                Ok(value) => function(value).await,
                Err(error) => Err(error),
            }
        })
    }
}

fn parse_source_id(value: &str) -> Result<Uuid, IntegrationError> {
    Uuid::parse_str(value).map_err(|_| integration_error("Invalid SoundCloud source ID", false))
}

fn generate_pkce() -> Result<(String, String, String), IntegrationError> {
    let mut state_bytes = [0_u8; 32];
    let mut verifier_bytes = [0_u8; 32];
    let mut rng = rand::rngs::OsRng;
    rng.try_fill_bytes(&mut state_bytes)
        .and_then(|_| rng.try_fill_bytes(&mut verifier_bytes))
        .map_err(|_| integration_error("Secure random generation failed", false))?;
    let state = URL_SAFE_NO_PAD.encode(state_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((state, verifier, challenge))
}

struct ParsedCallback {
    code: Option<String>,
    state: String,
    rejected: bool,
}

fn same_callback_endpoint(callback: &Url, redirect: &Url) -> bool {
    callback.scheme() == redirect.scheme()
        && callback.host_str() == redirect.host_str()
        && callback.port_or_known_default() == redirect.port_or_known_default()
        && callback.path() == redirect.path()
}

fn parse_callback(callback: &Url) -> Result<ParsedCallback, IntegrationError> {
    let mut code = None;
    let mut state = None;
    let mut provider_error = None;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
            "code" => {
                if code.replace(value.into_owned()).is_some() {
                    return Err(integration_error(
                        "OAuth callback contained duplicate code",
                        false,
                    ));
                }
            }
            "state" => {
                if state.replace(value.into_owned()).is_some() {
                    return Err(integration_error(
                        "OAuth callback contained duplicate state",
                        false,
                    ));
                }
            }
            "error" => provider_error = Some(value.into_owned()),
            _ => {}
        }
    }
    let state = state
        .filter(|value| !value.is_empty())
        .ok_or_else(|| integration_error("OAuth callback did not contain state", false))?;
    let rejected = provider_error.is_some();
    if rejected && code.is_some() {
        return Err(integration_error(
            "OAuth callback contained conflicting results",
            false,
        ));
    }
    if !rejected && code.as_ref().is_none_or(String::is_empty) {
        return Err(integration_error(
            "OAuth callback did not contain a code",
            false,
        ));
    }
    Ok(ParsedCallback {
        code,
        state,
        rejected,
    })
}

async fn canonicalize_occurrence(
    transaction: &dyn RepositoryTransaction,
    occurrence: &mut SourceTrackOccurrence,
) -> Result<(), IntegrationError> {
    let existing = transaction
        .sources()
        .get_occurrence(
            occurrence.identity.source_id,
            occurrence.identity.external_id.clone(),
        )
        .await
        .map_err(repository_error)?;
    let mut canonical = if let Some(id) = existing.and_then(|value| value.canonical_track_id) {
        transaction
            .tracks()
            .get(id)
            .await
            .map_err(repository_error)?
            .unwrap_or_else(CanonicalTrack::new)
    } else {
        CanonicalTrack::new()
    };
    canonical.link_occurrence(occurrence.identity.clone());
    add_provider_metadata(&mut canonical, occurrence);
    occurrence.canonical_track_id = Some(canonical.id);
    transaction
        .tracks()
        .upsert(canonical)
        .await
        .map_err(repository_error)
}

fn add_provider_metadata(canonical: &mut CanonicalTrack, occurrence: &SourceTrackOccurrence) {
    let provenance = MetadataProvenance::provider(
        occurrence.identity.source_id,
        occurrence.identity.external_id.clone(),
        occurrence.last_seen_at,
    );
    let title = occurrence
        .provider_metadata
        .get("title")
        .and_then(text_value);
    let artist = occurrence
        .provider_metadata
        .get("user")
        .and_then(object_value)
        .and_then(|user| user.get("username"))
        .and_then(text_value);
    for (field, value) in [
        (MetadataField::Title, title),
        (MetadataField::Artist, artist),
    ] {
        let Some(value) = value else { continue };
        let candidate = MetadataFieldValue::new(
            field,
            MetadataValue::Text(value.to_string()),
            provenance.clone(),
        );
        if !canonical.metadata.values.iter().any(|old| {
            old.field == candidate.field
                && old.value == candidate.value
                && old.provenance.source_id == candidate.provenance.source_id
                && old.provenance.external_id == candidate.provenance.external_id
        }) {
            canonical.metadata.add(candidate);
        }
    }
}

fn text_value(value: &MetadataValue) -> Option<&str> {
    match value {
        MetadataValue::Text(value) => Some(value),
        _ => None,
    }
}

fn object_value(value: &MetadataValue) -> Option<&BTreeMap<String, MetadataValue>> {
    match value {
        MetadataValue::Object(value) => Some(value),
        _ => None,
    }
}

fn add_progress(total: &mut SyncProgress, page: SyncProgress) {
    total.discovered_items = total.discovered_items.saturating_add(page.discovered_items);
    total.processed_items = total.processed_items.saturating_add(page.processed_items);
    total.succeeded_items = total.succeeded_items.saturating_add(page.succeeded_items);
    total.skipped_items = total.skipped_items.saturating_add(page.skipped_items);
    total.failed_items = total.failed_items.saturating_add(page.failed_items);
    total.bytes_transferred = total
        .bytes_transferred
        .saturating_add(page.bytes_transferred);
    total.total_items = match (total.total_items, page.total_items) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        _ => None,
    };
}

fn sync_error(error: &SourceError) -> SyncError {
    let kind = match error.kind {
        SourceErrorKind::AuthenticationRequired => SyncErrorKind::Authentication,
        SourceErrorKind::PermissionDenied => SyncErrorKind::Authorization,
        SourceErrorKind::RateLimited => SyncErrorKind::RateLimited,
        SourceErrorKind::TemporarilyUnavailable => SyncErrorKind::TemporarilyUnavailable,
        SourceErrorKind::NotFound => SyncErrorKind::NotFound,
        SourceErrorKind::InvalidData => SyncErrorKind::InvalidData,
        SourceErrorKind::Io => SyncErrorKind::Io,
        _ => SyncErrorKind::Provider,
    };
    SyncError::new(
        kind,
        error
            .provider_code
            .clone()
            .unwrap_or_else(|| "soundcloud".to_string()),
        error.message.clone(),
        error.is_retryable(),
    )
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

async fn collect_accounts(
    catalog: &TursoCatalog,
    source_id: Uuid,
) -> Result<Vec<rec_core::SourceAccount>, IntegrationError> {
    let mut values = Vec::new();
    let mut cursor = None;
    loop {
        let page = SourceRepository::list_accounts(
            catalog,
            source_id,
            RepositoryPageRequest {
                cursor,
                limit: Some(100),
            },
        )
        .await
        .map_err(repository_error)?;
        values.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(values);
        }
    }
}

async fn collect_occurrences(
    catalog: &TursoCatalog,
    source_id: Uuid,
) -> Result<Vec<SourceTrackOccurrence>, IntegrationError> {
    let mut values = Vec::new();
    let mut cursor = None;
    loop {
        let page = SourceRepository::list_occurrences(
            catalog,
            SourceOccurrenceFilter {
                source_id: Some(source_id),
                ..Default::default()
            },
            RepositoryPageRequest {
                cursor,
                limit: Some(200),
            },
        )
        .await
        .map_err(repository_error)?;
        values.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(values);
        }
    }
}

async fn collect_playlists(
    catalog: &TursoCatalog,
    source_id: Uuid,
) -> Result<Vec<ExternalPlaylist>, IntegrationError> {
    let mut values = Vec::new();
    let mut cursor = None;
    loop {
        let page = PlaylistRepository::list_external(
            catalog,
            rec_core::ExternalPlaylistFilter {
                source_id: Some(source_id),
                owner_external_id: None,
            },
            RepositoryPageRequest {
                cursor,
                limit: Some(100),
            },
        )
        .await
        .map_err(repository_error)?;
        values.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(values);
        }
    }
}

async fn collection_views(
    catalog: &TursoCatalog,
    source_id: Uuid,
) -> Result<Vec<SourceCollectionView>, IntegrationError> {
    let definitions = [
        (
            SourceCollection::UploadedTracks,
            SourceEntityKind::Track,
            "Uploads",
        ),
        (
            SourceCollection::OwnedPlaylists,
            SourceEntityKind::Playlist,
            "Owned playlists",
        ),
        (
            SourceCollection::LikedTracks,
            SourceEntityKind::Track,
            "Liked tracks",
        ),
        (
            SourceCollection::LikedPlaylists,
            SourceEntityKind::Playlist,
            "Liked playlists",
        ),
    ];
    let mut views = Vec::new();
    for (collection, entity_kind, name) in definitions {
        let mut count = 0_u32;
        let mut cursor = None;
        loop {
            let page = SourceRepository::list_collection_relationships(
                catalog,
                SourceCollectionRelationshipFilter {
                    source_id,
                    account_external_id: None,
                    collection: Some(collection),
                    entity_kind: Some(entity_kind),
                },
                RepositoryPageRequest {
                    cursor,
                    limit: Some(200),
                },
            )
            .await
            .map_err(repository_error)?;
            count = count.saturating_add(saturating_u32(
                page.items
                    .iter()
                    .filter(|value| value.identity.parent_external_id.is_none())
                    .count() as u64,
            ));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        views.push(SourceCollectionView {
            name: name.to_string(),
            item_count: count,
            read_only: true,
        });
    }
    Ok(views)
}

async fn has_verified_original(
    catalog: &TursoCatalog,
    track_id: Uuid,
) -> Result<bool, IntegrationError> {
    let originals = AssetRepository::list_originals_for_track(catalog, track_id)
        .await
        .map_err(repository_error)?;
    for original in originals {
        let locations = AssetRepository::list_locations_for_asset(catalog, original.record.id)
            .await
            .map_err(repository_error)?;
        if locations
            .iter()
            .any(|location| location.verification_status == StorageVerificationStatus::Verified)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn availability_status(availability: SourceAvailability, verified: bool) -> TrackStorageStatusView {
    if verified {
        return TrackStorageStatusView::StoredAndVerified;
    }
    match availability {
        SourceAvailability::Obtainable => TrackStorageStatusView::MissingObtainable,
        SourceAvailability::MetadataOnly | SourceAvailability::StreamingOnly => {
            TrackStorageStatusView::NotAvailableForArchival
        }
        SourceAvailability::PreviewOnly => TrackStorageStatusView::PreviewOnly,
        SourceAvailability::Blocked => TrackStorageStatusView::Blocked,
        SourceAvailability::Removed => TrackStorageStatusView::Removed,
        SourceAvailability::Inaccessible => TrackStorageStatusView::Inaccessible,
    }
}

fn status_badge(status: TrackStorageStatusView) -> &'static str {
    match status {
        TrackStorageStatusView::StoredAndVerified => "Archived",
        TrackStorageStatusView::MissingObtainable => "Download permitted",
        TrackStorageStatusView::PossibleMatch => "Possible match",
        TrackStorageStatusView::AmbiguousMatch => "Ambiguous match",
        TrackStorageStatusView::NotAvailableForArchival => "Not available for archival",
        TrackStorageStatusView::PreviewOnly => "Preview only",
        TrackStorageStatusView::Blocked => "Blocked",
        TrackStorageStatusView::Removed => "Removed from SoundCloud",
        TrackStorageStatusView::Inaccessible => "Inaccessible",
        TrackStorageStatusView::StorageUnavailable => "Storage unavailable",
        TrackStorageStatusView::Error => "Error",
        TrackStorageStatusView::Ignored => "Ignored",
    }
}

fn track_title_artist(track: &CanonicalTrack) -> (String, String) {
    let value = |field| {
        track
            .metadata
            .preferred(field)
            .and_then(|candidate| text_value(&candidate.value))
            .map(ToOwned::to_owned)
    };
    (
        value(&MetadataField::Title).unwrap_or_else(|| "Untitled SoundCloud track".to_string()),
        value(&MetadataField::Artist).unwrap_or_else(|| "Unknown artist".to_string()),
    )
}

fn source_view(
    source: &CatalogSource,
    account_identity: Option<String>,
    connected: bool,
    health: Option<&SourceHealth>,
    run: Option<&SyncRun>,
    collections: Vec<SourceCollectionView>,
) -> SourceView {
    let active = run.is_some_and(|run| {
        matches!(
            run.status,
            SyncStatus::Pending | SyncStatus::Running | SyncStatus::Paused | SyncStatus::Cancelling
        )
    });
    let (tone, warning, error) = if !connected {
        (
            HealthTone::Warning,
            Some("Reconnect SoundCloud to synchronize".to_string()),
            None,
        )
    } else if active {
        (HealthTone::Working, None, None)
    } else if let Some(health) = health {
        match health.status {
            HealthStatus::Healthy => (HealthTone::Healthy, None, None),
            HealthStatus::Degraded => (HealthTone::Warning, health.message.clone(), None),
            HealthStatus::Unavailable => (HealthTone::Offline, health.message.clone(), None),
            HealthStatus::AuthenticationRequired => (
                HealthTone::Warning,
                Some("Reconnect SoundCloud to synchronize".to_string()),
                None,
            ),
        }
    } else if run.is_some_and(|run| run.status == SyncStatus::Failed) {
        (
            HealthTone::Error,
            None,
            run.and_then(|run| run.error.as_ref().map(|error| error.message.clone())),
        )
    } else {
        (HealthTone::Healthy, None, None)
    };
    SourceView {
        id: source.id.to_string(),
        provider: ProviderChoice::SoundCloud,
        name: source.name.clone(),
        account_identity,
        detail: "Connected account catalog".to_string(),
        health: tone,
        last_sync: source.last_synced_at.map(|value| value.to_rfc3339()),
        next_sync: source
            .last_synced_at
            .map(|value| (value + ChronoDuration::hours(6)).to_rfc3339()),
        progress: run.filter(|_| active).map(|run| JobProgressView {
            label: "Synchronizing SoundCloud".to_string(),
            processed: saturating_u32(run.progress.processed_items),
            total: run.progress.total_items.map(saturating_u32),
            current_item: run
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.last_item_id.clone()),
        }),
        warning,
        error,
        failed_sync_run_id: run
            .filter(|run| run.status == SyncStatus::Failed)
            .map(|run| run.id.to_string()),
        collections,
        connected,
    }
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn empty_view() -> MusicManagementView {
    MusicManagementView {
        sources: Vec::new(),
        primary_storage: None,
        backup_storages: Vec::new(),
        unassigned_storages: Vec::new(),
        completeness: CompletenessView {
            canonical_tracks: 0,
            catalog_covered: 0,
            verified_assets: 0,
            missing_obtainable: 0,
            streaming_only: 0,
            restricted: 0,
            possible_matches: 0,
            ambiguous_matches: 0,
            unknown_primary_files: 0,
            backup_gaps: 0,
            provider_errors: 0,
            playlists: Vec::new(),
        },
        tracks: Vec::new(),
        possible_matches: Vec::new(),
        unknown_primary_files: Vec::new(),
        external_changes: Vec::new(),
        failed_reconciliations: Vec::new(),
        cleanup_candidates: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[test]
    fn broker_connection_failure_gives_a_safe_deployment_action() {
        let error = broker_server_error("transport connection refused".to_owned());
        assert!(error.retryable);
        assert_eq!(
            error.message,
            "SoundCloud authorization broker could not be reached. Start or deploy the rec_bag fullstack web service, then retry."
        );
    }
    use rec_core::{
        CatalogSnapshotRepository, MetadataOrigin, SourceAccount, SourceCollectionRelationship,
        SourceCollectionRelationshipIdentity, SourceSyncPage, SyncCursor,
    };

    struct MockUserAgent {
        callback_state_from_request: bool,
        callback: Mutex<Option<Url>>,
        opened: Mutex<Vec<Url>>,
    }

    impl MockUserAgent {
        fn successful() -> Self {
            Self {
                callback_state_from_request: true,
                callback: Mutex::new(None),
                opened: Mutex::new(Vec::new()),
            }
        }
    }

    impl OAuthUserAgent for MockUserAgent {
        fn authorize(
            &self,
            authorization_url: Url,
            redirect_uri: Url,
            _: std::time::Duration,
        ) -> BoxFuture<'_, Result<Url, IntegrationError>> {
            self.opened.lock().unwrap().push(authorization_url.clone());
            let explicit = self.callback.lock().unwrap().clone();
            let derive = self.callback_state_from_request;
            Box::pin(async move {
                if let Some(callback) = explicit {
                    return Ok(callback);
                }
                let state = authorization_url
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.into_owned())
                    .unwrap();
                let mut callback = redirect_uri;
                callback
                    .query_pairs_mut()
                    .append_pair("code", "authorization-code")
                    .append_pair("state", if derive { &state } else { "wrong-state-value" });
                Ok(callback)
            })
        }
    }

    struct MockSignOut {
        results: Mutex<VecDeque<Result<ProviderGrantRevocation, IntegrationError>>>,
        calls: Mutex<u32>,
    }

    impl MockSignOut {
        fn with_results(
            results: impl IntoIterator<Item = Result<ProviderGrantRevocation, IntegrationError>>,
        ) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: Mutex::new(0),
            }
        }
    }

    impl SoundCloudSignOutClient for MockSignOut {
        fn revoke_grant(
            &self,
            _access_token: Secret,
        ) -> BoxFuture<'_, Result<ProviderGrantRevocation, IntegrationError>> {
            *self.calls.lock().unwrap() += 1;
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(ProviderGrantRevocation::AlreadyRevoked));
            Box::pin(async move { result })
        }
    }

    #[derive(Default)]
    struct MockBroker {
        exchanges: Mutex<u32>,
        refreshes: Mutex<u32>,
    }

    impl SoundCloudBrokerClient for MockBroker {
        fn exchange_authorization_code(
            &self,
            _: SoundCloudAuthorizationCodeExchange,
        ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>> {
            *self.exchanges.lock().unwrap() += 1;
            Box::pin(async { Ok(token_response("access-1", "refresh-1")) })
        }
        fn refresh_token(
            &self,
            _: SoundCloudRefreshTokenExchange,
        ) -> BoxFuture<'_, Result<SoundCloudTokenResponse, IntegrationError>> {
            *self.refreshes.lock().unwrap() += 1;
            Box::pin(async { Ok(token_response("access-2", "refresh-2")) })
        }
    }

    struct QueueConnector {
        pages: Mutex<VecDeque<Result<SourceSyncPage, SourceError>>>,
        requests: Mutex<Vec<SourceSyncRequest>>,
    }

    impl CatalogSourceConnector for QueueConnector {
        fn health(&self) -> BoxFuture<'_, Result<SourceHealth, SourceError>> {
            Box::pin(async { Ok(healthy()) })
        }
        fn sync_page(
            &self,
            request: SourceSyncRequest,
        ) -> BoxFuture<'_, Result<SourceSyncPage, SourceError>> {
            self.requests.lock().unwrap().push(request);
            let result = self.pages.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { result })
        }
    }

    struct FixedFactory(Arc<QueueConnector>);
    impl SoundCloudConnectorFactory for FixedFactory {
        fn create(
            &self,
            _: Uuid,
            _: Arc<dyn SoundCloudTokenProvider>,
        ) -> Result<Arc<dyn CatalogSourceConnector>, IntegrationError> {
            Ok(self.0.clone())
        }
    }

    fn token_response(access: &str, refresh: &str) -> SoundCloudTokenResponse {
        SoundCloudTokenResponse {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_in: Some(3600),
            scope: None,
            token_type: Some("Bearer".to_string()),
        }
    }

    fn healthy() -> SourceHealth {
        SourceHealth {
            status: HealthStatus::Healthy,
            checked_at: Utc::now(),
            message: None,
            capabilities: soundcloud_capabilities(),
        }
    }

    fn terminal_page(source_id: Uuid) -> SourceSyncPage {
        let observed = Utc::now();
        let mut account = SourceAccount::new(source_id, "soundcloud:users:1");
        account.username = Some("tester".to_string());
        let mut occurrence = SourceTrackOccurrence::new(source_id, "soundcloud:tracks:1");
        occurrence.availability = SourceAvailability::PreviewOnly;
        occurrence.provider_metadata.insert(
            "title".to_string(),
            MetadataValue::Text("Track One".to_string()),
        );
        occurrence.provider_metadata.insert(
            "user".to_string(),
            MetadataValue::Object(BTreeMap::from([(
                "username".to_string(),
                MetadataValue::Text("Artist".to_string()),
            )])),
        );
        let relationship = SourceCollectionRelationship::new(
            SourceCollectionRelationshipIdentity::new(
                source_id,
                "soundcloud:users:1",
                SourceCollection::LikedTracks,
                SourceEntityKind::Track,
                "soundcloud:tracks:1",
            ),
            observed,
        );
        SourceSyncPage {
            items: vec![
                SourceSyncItem::Account(account),
                SourceSyncItem::Track(occurrence),
                SourceSyncItem::Relationship(relationship),
            ],
            removals: Vec::new(),
            next_page_cursor: None,
            next_sync_cursor: Some(SyncCursor::new("complete")),
            progress: Some(SyncProgress {
                discovered_items: 3,
                processed_items: 3,
                succeeded_items: 3,
                ..Default::default()
            }),
        }
    }

    async fn fixture() -> (
        SoundCloudCatalogService,
        Arc<MockUserAgent>,
        Arc<MockBroker>,
        Arc<MemoryCredentialStore>,
        Arc<QueueConnector>,
    ) {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let user_agent = Arc::new(MockUserAgent::successful());
        let broker = Arc::new(MockBroker::default());
        let credentials = Arc::new(MemoryCredentialStore::default());
        let connector = Arc::new(QueueConnector {
            pages: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });
        let service = SoundCloudCatalogService::new(
            SoundCloudOAuthClientConfig::new(
                "client-id",
                Url::parse("http://127.0.0.1:32123/callback").unwrap(),
            )
            .unwrap(),
            user_agent.clone(),
            credentials.clone(),
            broker.clone(),
            Arc::new(FixedFactory(connector.clone())),
            catalog,
        )
        .with_sign_out_client(Arc::new(UnsupportedSoundCloudSignOutClient));
        (service, user_agent, broker, credentials, connector)
    }

    #[test]
    fn pkce_is_url_safe_s256_and_has_required_entropy() {
        let (state, verifier, challenge) = generate_pkce().unwrap();
        assert!(state.len() >= 22);
        assert!((43..=128).contains(&verifier.len()));
        assert!(state
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        assert_ne!(generate_pkce().unwrap().0, state);
    }

    #[tokio::test]
    async fn connect_runs_secure_oauth_persists_full_page_and_projects_read_only_status() {
        let (service, user_agent, broker, credentials, connector) = fixture().await;
        let prospective = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        connector
            .pages
            .lock()
            .unwrap()
            .push_back(Ok(terminal_page(prospective.id)));
        // Pre-create fixes the source identity used by the fixture page; connect reuses it.
        SourceRepository::upsert(service.catalog.as_ref(), prospective.clone())
            .await
            .unwrap();
        let view = service.connect().await.unwrap();
        assert_eq!(*broker.exchanges.lock().unwrap(), 1);
        let opened = user_agent.opened.lock().unwrap();
        let query: BTreeMap<_, _> = opened[0]
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["redirect_uri"], "http://127.0.0.1:32123/callback");
        assert!(!query["state"].is_empty());
        assert!(credentials
            .load(&credential_key(prospective.id))
            .await
            .unwrap()
            .is_some());
        assert_eq!(view.sources[0].account_identity.as_deref(), Some("@tester"));
        assert!(view.sources[0]
            .collections
            .iter()
            .all(|collection| collection.read_only));
        assert_eq!(view.sources[0].collections[2].item_count, 1);
        assert_eq!(view.tracks[0].title, "Track One");
        assert_eq!(view.tracks[0].artist, "Artist");
        assert_eq!(view.tracks[0].status, TrackStorageStatusView::PreviewOnly);
        let occurrence = SourceRepository::get_occurrence(
            service.catalog.as_ref(),
            prospective.id,
            "soundcloud:tracks:1".to_string(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(occurrence.canonical_track_id.is_some());
    }

    #[tokio::test]
    async fn state_mismatch_consumes_pending_state_and_never_calls_broker() {
        let (service, _, broker, _, _) = fixture().await;
        let bad_agent = Arc::new(MockUserAgent {
            callback_state_from_request: false,
            callback: Mutex::new(None),
            opened: Mutex::new(Vec::new()),
        });
        let service = SoundCloudCatalogService::new(
            service.config.clone(),
            bad_agent,
            service.credentials.clone(),
            broker.clone(),
            service.connectors.clone(),
            service.catalog.clone(),
        );
        assert!(service
            .connect()
            .await
            .unwrap_err()
            .message
            .contains("state"));
        assert_eq!(*broker.exchanges.lock().unwrap(), 0);
        assert!(service.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_rotates_complete_generation_and_metadata_before_returning() {
        let (service, _, broker, credentials, _) = fixture().await;
        let source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        SourceRepository::upsert(service.catalog.as_ref(), source.clone())
            .await
            .unwrap();
        let initial = StoredSoundCloudCredentials::from_response(
            token_response("old-access", "old-refresh"),
            0,
        );
        credentials
            .store(&credential_key(source.id), initial.encode().unwrap())
            .await
            .unwrap();
        let provider = CredentialTokenProvider {
            source_id: source.id,
            credentials: credentials.clone(),
            broker: broker.clone(),
            catalog: service.catalog.clone(),
            refresh_lock: AsyncMutex::new(()),
        };
        assert_eq!(provider.refresh_access_token().await.unwrap(), "access-2");
        let rotated = StoredSoundCloudCredentials::decode(
            credentials
                .load(&credential_key(source.id))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(rotated.refresh_token, "refresh-2");
        assert_eq!(rotated.generation, 1);
        assert_eq!(
            service
                .catalog
                .get_refresh_token_metadata(source.id)
                .await
                .unwrap()
                .unwrap()
                .refresh_generation,
            1
        );
    }

    #[tokio::test]
    async fn revoke_is_remote_first_retryable_and_never_exposes_secrets() {
        let (service, _, _, credentials, _) = fixture().await;
        let source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        SourceRepository::upsert(service.catalog.as_ref(), source.clone())
            .await
            .unwrap();
        let stored = StoredSoundCloudCredentials::from_response(
            token_response("access-secret-never-log", "refresh-secret-never-log"),
            0,
        );
        credentials
            .store(&credential_key(source.id), stored.encode().unwrap())
            .await
            .unwrap();
        let sign_out = Arc::new(MockSignOut::with_results([
            Err(integration_error(
                "provider leaked access-secret-never-log",
                true,
            )),
            Ok(ProviderGrantRevocation::Revoked),
        ]));
        let service = service.with_sign_out_client(sign_out.clone());

        let error = service.revoke_provider_grant(source.id).await.unwrap_err();
        assert!(error.retryable);
        assert!(!error.message.contains("access-secret-never-log"));
        assert!(credentials
            .load(&credential_key(source.id))
            .await
            .unwrap()
            .is_some());
        assert!(
            SourceRepository::get(service.catalog.as_ref(), source.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );

        let view = service.revoke_provider_grant(source.id).await.unwrap();
        assert!(!view.sources[0].connected);
        assert!(credentials
            .load(&credential_key(source.id))
            .await
            .unwrap()
            .is_none());
        assert_eq!(*sign_out.calls.lock().unwrap(), 2);
        service.revoke_provider_grant(source.id).await.unwrap();
        assert_eq!(*sign_out.calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn clear_cache_is_idempotent_and_only_clears_soundcloud_checkpoints() {
        let (service, _, _, credentials, _) = fixture().await;
        let source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        SourceRepository::upsert(service.catalog.as_ref(), source.clone())
            .await
            .unwrap();
        let stored =
            StoredSoundCloudCredentials::from_response(token_response("access", "refresh"), 0);
        credentials
            .store(&credential_key(source.id), stored.encode().unwrap())
            .await
            .unwrap();
        let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
        run.sync_cursor = Some(SyncCursor::new("durable"));
        run.page_cursor = Some(SyncCursor::new("page"));
        run.checkpoint = Some(
            SyncCheckpoint::new(4)
                .with_sync_cursor(SyncCursor::new("durable"))
                .with_page_cursor(SyncCursor::new("page")),
        );
        SyncRunRepository::upsert(service.catalog.as_ref(), run.clone())
            .await
            .unwrap();

        service.clear_local_cache(source.id).await.unwrap();
        service.clear_local_cache(source.id).await.unwrap();
        let cleared = SyncRunRepository::get(service.catalog.as_ref(), run.id)
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.sync_cursor.is_none());
        assert!(cleared.page_cursor.is_none());
        assert!(cleared.checkpoint.is_none());
        assert!(credentials
            .load(&credential_key(source.id))
            .await
            .unwrap()
            .is_some());
        assert!(SourceRepository::get(service.catalog.as_ref(), source.id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn forget_catalog_is_idempotent_and_preserves_canonical_track_owned_asset_and_credentials(
    ) {
        use rec_core::{OriginalAsset, OriginalAssetRole};

        let (service, _, _, credentials, connector) = fixture().await;
        let mut source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        source.capabilities = soundcloud_capabilities();
        SourceRepository::upsert(service.catalog.as_ref(), source.clone())
            .await
            .unwrap();
        let stored =
            StoredSoundCloudCredentials::from_response(token_response("access", "refresh"), 0);
        credentials
            .store(&credential_key(source.id), stored.encode().unwrap())
            .await
            .unwrap();
        connector
            .pages
            .lock()
            .unwrap()
            .push_back(Ok(terminal_page(source.id)));
        service.sync(source.id).await.unwrap();
        let occurrence = SourceRepository::get_occurrence(
            service.catalog.as_ref(),
            source.id,
            "soundcloud:tracks:1".to_string(),
        )
        .await
        .unwrap()
        .unwrap();
        let track_id = occurrence.canonical_track_id.unwrap();
        let asset =
            OriginalAsset::new_unverified(track_id, OriginalAssetRole::Master, "owned-local.flac");
        AssetRepository::upsert_original(service.catalog.as_ref(), asset.clone())
            .await
            .unwrap();
        service.catalog.export_snapshot().await.unwrap();

        let view = service
            .forget_provider_catalog_data(source.id)
            .await
            .unwrap();
        service
            .forget_provider_catalog_data(source.id)
            .await
            .unwrap();

        assert!(SourceRepository::get(service.catalog.as_ref(), source.id)
            .await
            .unwrap()
            .is_none());
        assert!(SourceRepository::get_occurrence(
            service.catalog.as_ref(),
            source.id,
            "soundcloud:tracks:1".to_string(),
        )
        .await
        .unwrap()
        .is_none());
        let canonical = TrackRepository::get(service.catalog.as_ref(), track_id)
            .await
            .unwrap()
            .unwrap();
        assert!(canonical.occurrences.is_empty());
        assert!(canonical.metadata.values.iter().all(|value| {
            value.provenance.source_id != Some(source.id)
                || value.provenance.origin != MetadataOrigin::Provider
        }));
        assert_eq!(
            AssetRepository::get_original(service.catalog.as_ref(), asset.id())
                .await
                .unwrap(),
            Some(asset)
        );
        assert!(credentials
            .load(&credential_key(source.id))
            .await
            .unwrap()
            .is_some());
        assert!(view
            .sources
            .iter()
            .all(|source| source.provider != ProviderChoice::SoundCloud));
        assert!(view
            .tracks
            .iter()
            .all(|track| !track.badges.iter().any(|badge| badge == "SoundCloud")));
        assert!(CatalogSnapshotRepository::latest(service.catalog.as_ref())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn incremental_sync_uses_last_successful_cursor_and_disconnect_preserves_catalog() {
        let (service, _, _, credentials, connector) = fixture().await;
        let mut source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        source.capabilities = soundcloud_capabilities();
        SourceRepository::upsert(service.catalog.as_ref(), source.clone())
            .await
            .unwrap();
        let stored =
            StoredSoundCloudCredentials::from_response(token_response("access", "refresh"), 0);
        credentials
            .store(&credential_key(source.id), stored.encode().unwrap())
            .await
            .unwrap();
        connector
            .pages
            .lock()
            .unwrap()
            .push_back(Ok(terminal_page(source.id)));
        service.sync(source.id).await.unwrap();
        connector
            .pages
            .lock()
            .unwrap()
            .push_back(Ok(terminal_page(source.id)));
        service.sync(source.id).await.unwrap();
        let requests = connector.requests.lock().unwrap();
        assert!(matches!(requests[0].mode, SourceSyncMode::Full));
        assert!(matches!(
            requests[1].mode,
            SourceSyncMode::Incremental { .. }
        ));
        drop(requests);
        let disconnected = service.disconnect(source.id).await.unwrap();
        assert!(!disconnected.sources[0].connected);
        assert!(SourceRepository::get_occurrence(
            service.catalog.as_ref(),
            source.id,
            "soundcloud:tracks:1".to_string()
        )
        .await
        .unwrap()
        .is_some());
        assert!(
            TrackRepository::list(service.catalog.as_ref(), RepositoryPageRequest::default())
                .await
                .unwrap()
                .items
                .len()
                == 1
        );
    }

    #[test]
    fn every_availability_has_a_stable_ui_status_and_verified_wins() {
        assert_eq!(
            availability_status(SourceAvailability::Obtainable, false),
            TrackStorageStatusView::MissingObtainable
        );
        assert_eq!(
            availability_status(SourceAvailability::MetadataOnly, false),
            TrackStorageStatusView::NotAvailableForArchival
        );
        assert_eq!(
            availability_status(SourceAvailability::StreamingOnly, false),
            TrackStorageStatusView::NotAvailableForArchival
        );
        assert_eq!(
            availability_status(SourceAvailability::PreviewOnly, false),
            TrackStorageStatusView::PreviewOnly
        );
        assert_eq!(
            availability_status(SourceAvailability::Blocked, false),
            TrackStorageStatusView::Blocked
        );
        assert_eq!(
            availability_status(SourceAvailability::Removed, false),
            TrackStorageStatusView::Removed
        );
        assert_eq!(
            availability_status(SourceAvailability::Inaccessible, false),
            TrackStorageStatusView::Inaccessible
        );
        assert_eq!(
            availability_status(SourceAvailability::Removed, true),
            TrackStorageStatusView::StoredAndVerified
        );
    }

    #[test]
    fn secrets_are_redacted_from_debug_and_callback_rejects_duplicates() {
        let credentials = StoredSoundCloudCredentials::from_response(
            token_response("access-secret", "refresh-secret"),
            0,
        );
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("refresh-secret"));
        let callback =
            Url::parse("http://127.0.0.1/callback?code=one&code=two&state=state").unwrap();
        assert!(parse_callback(&callback).is_err());
    }
}
