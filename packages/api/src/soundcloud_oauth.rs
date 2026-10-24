//! Stateless SoundCloud OAuth token broker.
//!
//! Desktop owns OAuth state generation, storage, and constant-time comparison before invoking this
//! broker. The broker validates callback input shape, exact redirect allow-list membership, and
//! forwards PKCE without retaining any user credential.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use url::Url;

#[cfg(feature = "server")]
use std::sync::OnceLock;

#[cfg(any(feature = "server", test))]
const SOUNDCLOUD_TOKEN_URL: &str = "https://secure.soundcloud.com/oauth/token";
const MAX_CODE_LENGTH: usize = 4096;
const MIN_PKCE_VERIFIER_LENGTH: usize = 43;
const MAX_PKCE_VERIFIER_LENGTH: usize = 128;
const MIN_STATE_LENGTH: usize = 16;
const MAX_STATE_LENGTH: usize = 512;
const MAX_REFRESH_TOKEN_LENGTH: usize = 4096;
const MAX_TOKEN_LENGTH: usize = 16 * 1024;
const MAX_SCOPE_LENGTH: usize = 4096;
const MAX_TOKEN_TYPE_LENGTH: usize = 64;
const MAX_PROVIDER_RESPONSE_BODY: usize = 64 * 1024;
#[cfg(any(feature = "server", test))]
const MAX_ENV_SECRET_LENGTH: usize = 16 * 1024;
#[cfg(any(feature = "server", test))]
const DEFAULT_RATE_LIMIT: usize = 20;
#[cfg(any(feature = "server", test))]
const DEFAULT_RATE_WINDOW_SECONDS: u64 = 60;
#[cfg(any(feature = "server", test))]
const DEFAULT_MAX_RATE_LIMIT_CLIENTS: usize = 10_000;
#[cfg(any(feature = "server", test))]
const DEFAULT_HTTP_TIMEOUT_SECONDS: u64 = 15;

#[cfg(feature = "server")]
static DOTENV_LOADED: OnceLock<()> = OnceLock::new();
#[cfg(feature = "server")]
static ENVIRONMENT_BROKER: OnceLock<Result<SoundCloudOAuthBroker, SoundCloudOAuthError>> =
    OnceLock::new();

/// Input for the authorization-code exchange endpoint.
///
/// `state` must already have been compared with desktop's locally stored state. It is accepted to
/// reject missing or malformed callback data, and is never forwarded or logged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundCloudAuthorizationCodeExchange {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub state: String,
}

/// Input for a refresh-token rotation request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundCloudRefreshTokenExchange {
    pub refresh_token: String,
}

/// Token material returned directly to desktop. This type intentionally does not implement
/// `Debug` or `Display`, reducing the chance of accidental log disclosure.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundCloudTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
    pub token_type: Option<String>,
}

/// Non-secret settings the desktop needs to start an authorization request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundCloudPublicConfig {
    pub client_id: String,
    pub redirect_uri: String,
}

/// Safe classifications exposed by the broker. Provider response text and credentials are never
/// included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCloudOAuthErrorKind {
    InvalidInput,
    RedirectNotAllowed,
    Misconfigured,
    RateLimited,
    ProviderRejected,
    TemporarilyUnavailable,
    InvalidProviderResponse,
}

/// An error safe to convert to a client-facing server-function error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundCloudOAuthError {
    pub kind: SoundCloudOAuthErrorKind,
    pub status_code: u16,
    pub message: &'static str,
}

impl SoundCloudOAuthError {
    fn new(kind: SoundCloudOAuthErrorKind, status_code: u16, message: &'static str) -> Self {
        Self {
            kind,
            status_code,
            message,
        }
    }
}

impl std::fmt::Display for SoundCloudOAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for SoundCloudOAuthError {}

/// Validated deployment configuration. Application credentials remain private.
#[derive(Clone)]
pub struct SoundCloudOAuthConfig {
    client_id: String,
    client_secret: String,
    allowed_redirect_uris: BTreeSet<String>,
    token_url: String,
    #[cfg(any(feature = "server", test))]
    rate_limit: usize,
    #[cfg(any(feature = "server", test))]
    rate_window: Duration,
    #[cfg(any(feature = "server", test))]
    max_rate_limit_clients: usize,
    #[cfg(any(feature = "server", test))]
    http_timeout: Duration,
}

impl SoundCloudOAuthConfig {
    /// Load and strictly validate deployment configuration.
    ///
    /// `.env` loading defaults to on for debug builds (local `dx serve`) and off for release
    /// builds; `SOUNDCLOUD_LOAD_DOTENV` overrides either default. Process environment always
    /// takes precedence over `.env` values.
    #[cfg(feature = "server")]
    pub fn from_environment() -> Result<Self, SoundCloudOAuthError> {
        if optional_environment("SOUNDCLOUD_LOAD_DOTENV")
            .as_deref()
            .map(parse_bool)
            .transpose()?
            .unwrap_or(cfg!(debug_assertions))
        {
            DOTENV_LOADED.get_or_init(|| {
                let _ = dotenvy::dotenv();
            });
        }
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    #[cfg(any(feature = "server", test))]
    fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, SoundCloudOAuthError> {
        let client_id = required_value(lookup("SOUNDCLOUD_CLIENT_ID"))?;
        let client_secret = required_value(lookup("SOUNDCLOUD_KEY"))?;
        let redirects = required_value(lookup("SOUNDCLOUD_ALLOWED_REDIRECT_URIS"))?;
        let allowed_redirect_uris = parse_redirect_allow_list(&redirects)?;
        let rate_limit = parse_usize(
            lookup("SOUNDCLOUD_OAUTH_RATE_LIMIT"),
            DEFAULT_RATE_LIMIT,
            1,
            10_000,
        )?;
        let rate_window_seconds = parse_u64(
            lookup("SOUNDCLOUD_OAUTH_RATE_WINDOW_SECONDS"),
            DEFAULT_RATE_WINDOW_SECONDS,
            1,
            86_400,
        )?;
        let max_rate_limit_clients = parse_usize(
            lookup("SOUNDCLOUD_OAUTH_MAX_TRACKED_CLIENTS"),
            DEFAULT_MAX_RATE_LIMIT_CLIENTS,
            1,
            1_000_000,
        )?;
        let http_timeout_seconds = parse_u64(
            lookup("SOUNDCLOUD_OAUTH_HTTP_TIMEOUT_SECONDS"),
            DEFAULT_HTTP_TIMEOUT_SECONDS,
            1,
            120,
        )?;

        Ok(Self {
            client_id,
            client_secret,
            allowed_redirect_uris,
            token_url: SOUNDCLOUD_TOKEN_URL.to_string(),
            rate_limit,
            rate_window: Duration::from_secs(rate_window_seconds),
            max_rate_limit_clients,
            http_timeout: Duration::from_secs(http_timeout_seconds),
        })
    }

    #[cfg(test)]
    fn for_test<'a>(
        client_id: &str,
        client_secret: &str,
        allowed_redirect_uris: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            allowed_redirect_uris: allowed_redirect_uris
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
            token_url: SOUNDCLOUD_TOKEN_URL.to_string(),
            rate_limit: DEFAULT_RATE_LIMIT,
            rate_window: Duration::from_secs(DEFAULT_RATE_WINDOW_SECONDS),
            max_rate_limit_clients: DEFAULT_MAX_RATE_LIMIT_CLIENTS,
            http_timeout: Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECONDS),
        }
    }
}

/// A validated limiter identity derived from trusted request metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OAuthClientIp(IpAddr);

impl OAuthClientIp {
    pub fn parse(value: &str) -> Result<Self, SoundCloudOAuthError> {
        let value = value.trim();
        if value.is_empty() || value.contains(',') {
            return Err(unavailable());
        }
        value.parse().map(Self).map_err(|_| unavailable())
    }
}

/// A transport boundary for testing the exact form sent to SoundCloud.
pub trait SoundCloudOAuthTransport: Send + Sync {
    fn post_form(
        &self,
        request: SoundCloudOAuthHttpRequest,
    ) -> crate::BoxFuture<'_, Result<SoundCloudOAuthHttpResponse, SoundCloudOAuthTransportError>>;
}

/// Form request sent only to the fixed SoundCloud token endpoint.
pub struct SoundCloudOAuthHttpRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub form: BTreeMap<String, String>,
}

pub struct SoundCloudOAuthHttpResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundCloudOAuthTransportError;

/// Production HTTP implementation. Request and response bodies are never logged.
#[cfg(feature = "server")]
#[derive(Clone)]
pub struct ReqwestSoundCloudOAuthTransport {
    client: reqwest::Client,
}

#[cfg(feature = "server")]
impl ReqwestSoundCloudOAuthTransport {
    fn new(timeout: Duration) -> Result<Self, SoundCloudOAuthError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("rec-bag-oauth-broker/1")
            .build()
            .map_err(|_| misconfigured())?;
        Ok(Self { client })
    }
}

#[cfg(feature = "server")]
impl SoundCloudOAuthTransport for ReqwestSoundCloudOAuthTransport {
    fn post_form(
        &self,
        request: SoundCloudOAuthHttpRequest,
    ) -> crate::BoxFuture<'_, Result<SoundCloudOAuthHttpResponse, SoundCloudOAuthTransportError>>
    {
        let client = self.client.clone();
        Box::pin(async move {
            let mut builder = client.post(request.url).form(&request.form);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let mut response = builder
                .send()
                .await
                .map_err(|_| SoundCloudOAuthTransportError)?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| SoundCloudOAuthTransportError)?
            {
                if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BODY {
                    return Err(SoundCloudOAuthTransportError);
                }
                body.extend_from_slice(&chunk);
            }
            let body = String::from_utf8(body).map_err(|_| SoundCloudOAuthTransportError)?;
            Ok(SoundCloudOAuthHttpResponse { status, body })
        })
    }
}

/// Concurrent per-client sliding-window limiter with bounded identity storage.
pub struct OAuthRateLimiter {
    maximum_requests: usize,
    window: Duration,
    maximum_clients: usize,
    requests: Mutex<HashMap<OAuthClientIp, VecDeque<Instant>>>,
}

impl OAuthRateLimiter {
    pub fn new(maximum_requests: usize, window: Duration, maximum_clients: usize) -> Self {
        Self {
            maximum_requests,
            window,
            maximum_clients,
            requests: Mutex::new(HashMap::new()),
        }
    }

    fn acquire(&self, client: OAuthClientIp) -> Result<(), SoundCloudOAuthError> {
        self.acquire_at(client, Instant::now())
    }

    fn acquire_at(&self, client: OAuthClientIp, now: Instant) -> Result<(), SoundCloudOAuthError> {
        let mut all_requests = self.requests.lock().map_err(|_| unavailable())?;
        all_requests.retain(|_, requests| {
            discard_expired(requests, now, self.window);
            !requests.is_empty()
        });

        if !all_requests.contains_key(&client) && all_requests.len() >= self.maximum_clients {
            return Err(rate_limited());
        }
        let requests = all_requests.entry(client).or_default();
        if requests.len() >= self.maximum_requests {
            return Err(rate_limited());
        }
        requests.push_back(now);
        Ok(())
    }
}

fn discard_expired(requests: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while requests
        .front()
        .is_some_and(|first| now.saturating_duration_since(*first) >= window)
    {
        requests.pop_front();
    }
}

/// Stateless OAuth broker. It retains only application configuration and limiter counters.
pub struct SoundCloudOAuthBroker {
    config: SoundCloudOAuthConfig,
    transport: Arc<dyn SoundCloudOAuthTransport>,
    rate_limiter: Arc<OAuthRateLimiter>,
}

impl SoundCloudOAuthBroker {
    pub fn new(
        config: SoundCloudOAuthConfig,
        transport: Arc<dyn SoundCloudOAuthTransport>,
        rate_limiter: Arc<OAuthRateLimiter>,
    ) -> Self {
        Self {
            config,
            transport,
            rate_limiter,
        }
    }

    /// Public desktop OAuth settings. The client ID is not secret, and the redirect is the
    /// first loopback HTTP entry of the allow-list, which is what the desktop callback
    /// receiver can serve.
    pub fn public_config(&self) -> Result<SoundCloudPublicConfig, SoundCloudOAuthError> {
        let redirect_uri = self
            .config
            .allowed_redirect_uris
            .iter()
            .find(|uri| {
                Url::parse(uri).is_ok_and(|url| {
                    url.scheme() == "http" && is_loopback_host(url.host_str().unwrap_or_default())
                })
            })
            .cloned()
            .ok_or_else(|| {
                SoundCloudOAuthError::new(
                    SoundCloudOAuthErrorKind::Misconfigured,
                    500,
                    "SoundCloud OAuth broker has no loopback desktop redirect URI",
                )
            })?;
        Ok(SoundCloudPublicConfig {
            client_id: self.config.client_id.clone(),
            redirect_uri,
        })
    }

    pub async fn exchange_authorization_code(
        &self,
        client: OAuthClientIp,
        request: SoundCloudAuthorizationCodeExchange,
    ) -> Result<SoundCloudTokenResponse, SoundCloudOAuthError> {
        self.rate_limiter.acquire(client)?;
        validate_code_exchange(&self.config, &request)?;
        self.exchange(BTreeMap::from([
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("client_id".to_string(), self.config.client_id.clone()),
            (
                "client_secret".to_string(),
                self.config.client_secret.clone(),
            ),
            ("redirect_uri".to_string(), request.redirect_uri),
            ("code_verifier".to_string(), request.code_verifier),
            ("code".to_string(), request.code),
        ]))
        .await
    }

    pub async fn refresh_token(
        &self,
        client: OAuthClientIp,
        request: SoundCloudRefreshTokenExchange,
    ) -> Result<SoundCloudTokenResponse, SoundCloudOAuthError> {
        self.rate_limiter.acquire(client)?;
        validate_secret_input(&request.refresh_token, MAX_REFRESH_TOKEN_LENGTH)?;
        self.exchange(BTreeMap::from([
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".to_string(), self.config.client_id.clone()),
            (
                "client_secret".to_string(),
                self.config.client_secret.clone(),
            ),
            ("refresh_token".to_string(), request.refresh_token),
        ]))
        .await
    }

    async fn exchange(
        &self,
        form: BTreeMap<String, String>,
    ) -> Result<SoundCloudTokenResponse, SoundCloudOAuthError> {
        let response = self
            .transport
            .post_form(SoundCloudOAuthHttpRequest {
                url: self.config.token_url.clone(),
                headers: BTreeMap::from([(
                    "accept".to_string(),
                    "application/json; charset=utf-8".to_string(),
                )]),
                form,
            })
            .await
            .map_err(|_| unavailable())?;

        if !(200..300).contains(&response.status) {
            return Err(provider_error(response.status));
        }
        if response.body.len() > MAX_PROVIDER_RESPONSE_BODY {
            return Err(invalid_provider_response());
        }
        let mut token: SoundCloudTokenWire =
            serde_json::from_str(&response.body).map_err(|error| {
                // Category and position only: serde messages can echo field values.
                dioxus::logger::tracing::warn!(
                    category = ?error.classify(),
                    line = error.line(),
                    column = error.column(),
                    "SoundCloud token response could not be parsed"
                );
                invalid_provider_response()
            })?;
        // SoundCloud sends `"scope": ""`; an empty optional value carries no information.
        token.scope = token.scope.filter(|scope| !scope.is_empty());
        token.token_type = token.token_type.filter(|kind| !kind.is_empty());
        validate_provider_token(&token).inspect_err(|_| {
            dioxus::logger::tracing::warn!(
                field = invalid_token_field(&token),
                "SoundCloud token response failed validation"
            );
        })?;
        Ok(SoundCloudTokenResponse {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_in: token.expires_in,
            scope: token.scope,
            token_type: token.token_type,
        })
    }
}

/// Process-wide broker. User codes and tokens are never placed in this static state.
#[cfg(feature = "server")]
pub fn environment_broker() -> Result<&'static SoundCloudOAuthBroker, SoundCloudOAuthError> {
    match ENVIRONMENT_BROKER.get_or_init(|| {
        let config = SoundCloudOAuthConfig::from_environment()?;
        let transport = ReqwestSoundCloudOAuthTransport::new(config.http_timeout)?;
        let limiter = OAuthRateLimiter::new(
            config.rate_limit,
            config.rate_window,
            config.max_rate_limit_clients,
        );
        Ok(SoundCloudOAuthBroker::new(
            config,
            Arc::new(transport),
            Arc::new(limiter),
        ))
    }) {
        Ok(broker) => Ok(broker),
        Err(error) => Err(error.clone()),
    }
}

#[derive(Deserialize)]
struct SoundCloudTokenWire {
    access_token: String,
    refresh_token: String,
    expires_in: Option<u64>,
    scope: Option<String>,
    token_type: Option<String>,
}

#[cfg(feature = "server")]
fn optional_environment(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(any(feature = "server", test))]
fn required_value(value: Option<String>) -> Result<String, SoundCloudOAuthError> {
    value
        .filter(|value| {
            !value.trim().is_empty()
                && value.len() <= MAX_ENV_SECRET_LENGTH
                && !value.chars().any(char::is_control)
        })
        .ok_or_else(misconfigured)
}

#[cfg(feature = "server")]
fn parse_bool(value: &str) -> Result<bool, SoundCloudOAuthError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(misconfigured()),
    }
}

#[cfg(any(feature = "server", test))]
fn parse_usize(
    value: Option<String>,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, SoundCloudOAuthError> {
    let parsed = value
        .map(|value| value.parse::<usize>().map_err(|_| misconfigured()))
        .transpose()?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&parsed) {
        return Err(misconfigured());
    }
    Ok(parsed)
}

#[cfg(any(feature = "server", test))]
fn parse_u64(
    value: Option<String>,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> Result<u64, SoundCloudOAuthError> {
    let parsed = value
        .map(|value| value.parse::<u64>().map_err(|_| misconfigured()))
        .transpose()?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&parsed) {
        return Err(misconfigured());
    }
    Ok(parsed)
}

#[cfg(any(feature = "server", test))]
fn parse_redirect_allow_list(value: &str) -> Result<BTreeSet<String>, SoundCloudOAuthError> {
    let mut values = BTreeSet::new();
    for candidate in value.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() || !is_valid_redirect_uri(candidate) {
            return Err(misconfigured());
        }
        values.insert(candidate.to_string());
    }
    if values.is_empty() {
        return Err(misconfigured());
    }
    Ok(values)
}

fn validate_code_exchange(
    config: &SoundCloudOAuthConfig,
    request: &SoundCloudAuthorizationCodeExchange,
) -> Result<(), SoundCloudOAuthError> {
    validate_secret_input(&request.code, MAX_CODE_LENGTH)?;
    if !(MIN_PKCE_VERIFIER_LENGTH..=MAX_PKCE_VERIFIER_LENGTH).contains(&request.code_verifier.len())
        || !request.code_verifier.bytes().all(is_pkce_character)
    {
        return Err(invalid_input());
    }
    if !(MIN_STATE_LENGTH..=MAX_STATE_LENGTH).contains(&request.state.len())
        || !request.state.bytes().all(is_state_character)
    {
        return Err(invalid_input());
    }
    if !is_valid_redirect_uri(&request.redirect_uri) {
        return Err(invalid_input());
    }
    if !config.allowed_redirect_uris.contains(&request.redirect_uri) {
        return Err(SoundCloudOAuthError::new(
            SoundCloudOAuthErrorKind::RedirectNotAllowed,
            400,
            "The SoundCloud redirect URI is not allowed",
        ));
    }
    Ok(())
}

fn is_pkce_character(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_state_character(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

fn validate_secret_input(value: &str, maximum_length: usize) -> Result<(), SoundCloudOAuthError> {
    if value.is_empty()
        || value.len() > maximum_length
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        Err(invalid_input())
    } else {
        Ok(())
    }
}

fn is_valid_redirect_uri(value: &str) -> bool {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return false;
    }
    Url::parse(value)
        .map(|url| {
            !url.scheme().is_empty()
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && (url.scheme() != "http" || is_loopback_host(url.host_str().unwrap_or_default()))
        })
        .unwrap_or(false)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn validate_provider_token(token: &SoundCloudTokenWire) -> Result<(), SoundCloudOAuthError> {
    validate_provider_field(&token.access_token, MAX_TOKEN_LENGTH)?;
    validate_provider_field(&token.refresh_token, MAX_TOKEN_LENGTH)?;
    if token.expires_in == Some(0)
        || token
            .scope
            .as_ref()
            .is_some_and(|scope| !is_safe_provider_metadata(scope, MAX_SCOPE_LENGTH))
        || token
            .token_type
            .as_ref()
            .is_some_and(|kind| !is_safe_provider_metadata(kind, MAX_TOKEN_TYPE_LENGTH))
    {
        return Err(invalid_provider_response());
    }
    Ok(())
}

/// Names the first field `validate_provider_token` rejects, for logs. Never returns values.
fn invalid_token_field(token: &SoundCloudTokenWire) -> &'static str {
    if validate_provider_field(&token.access_token, MAX_TOKEN_LENGTH).is_err() {
        "access_token"
    } else if validate_provider_field(&token.refresh_token, MAX_TOKEN_LENGTH).is_err() {
        "refresh_token"
    } else if token.expires_in == Some(0) {
        "expires_in"
    } else if token.scope.is_some() {
        "scope"
    } else {
        "token_type"
    }
}

fn validate_provider_field(value: &str, maximum_length: usize) -> Result<(), SoundCloudOAuthError> {
    if value.is_empty()
        || value.len() > maximum_length
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(invalid_provider_response());
    }
    Ok(())
}

fn is_safe_provider_metadata(value: &str, maximum_length: usize) -> bool {
    !value.is_empty() && value.len() <= maximum_length && !value.chars().any(char::is_control)
}

fn invalid_input() -> SoundCloudOAuthError {
    SoundCloudOAuthError::new(
        SoundCloudOAuthErrorKind::InvalidInput,
        400,
        "Invalid SoundCloud OAuth request",
    )
}

#[cfg(any(feature = "server", test))]
fn misconfigured() -> SoundCloudOAuthError {
    SoundCloudOAuthError::new(
        SoundCloudOAuthErrorKind::Misconfigured,
        500,
        "SoundCloud OAuth broker is not configured",
    )
}

fn rate_limited() -> SoundCloudOAuthError {
    SoundCloudOAuthError::new(
        SoundCloudOAuthErrorKind::RateLimited,
        429,
        "Too many SoundCloud OAuth requests; try again shortly",
    )
}

fn unavailable() -> SoundCloudOAuthError {
    SoundCloudOAuthError::new(
        SoundCloudOAuthErrorKind::TemporarilyUnavailable,
        503,
        "SoundCloud OAuth service is temporarily unavailable",
    )
}

fn invalid_provider_response() -> SoundCloudOAuthError {
    SoundCloudOAuthError::new(
        SoundCloudOAuthErrorKind::InvalidProviderResponse,
        502,
        "SoundCloud OAuth service returned an invalid response",
    )
}

fn provider_error(status: u16) -> SoundCloudOAuthError {
    match status {
        400 | 401 | 403 => SoundCloudOAuthError::new(
            SoundCloudOAuthErrorKind::ProviderRejected,
            400,
            "SoundCloud rejected the OAuth request",
        ),
        429 => SoundCloudOAuthError::new(
            SoundCloudOAuthErrorKind::RateLimited,
            429,
            "SoundCloud is rate limiting OAuth requests; try again shortly",
        ),
        _ => unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[derive(Default)]
    struct MockTransport {
        requests: Mutex<Vec<SoundCloudOAuthHttpRequest>>,
        responses:
            Mutex<VecDeque<Result<SoundCloudOAuthHttpResponse, SoundCloudOAuthTransportError>>>,
    }

    impl MockTransport {
        fn with_response(response: SoundCloudOAuthHttpResponse) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([Ok(response)])),
            }
        }

        fn with_transport_error() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([Err(SoundCloudOAuthTransportError)])),
            }
        }
    }

    impl SoundCloudOAuthTransport for MockTransport {
        fn post_form(
            &self,
            request: SoundCloudOAuthHttpRequest,
        ) -> crate::BoxFuture<'_, Result<SoundCloudOAuthHttpResponse, SoundCloudOAuthTransportError>>
        {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { response })
        }
    }

    fn client(last_octet: u8) -> OAuthClientIp {
        OAuthClientIp::parse(&format!("192.0.2.{last_octet}")).unwrap()
    }

    fn oauth_error<T>(result: Result<T, SoundCloudOAuthError>) -> SoundCloudOAuthError {
        match result {
            Ok(_) => panic!("expected OAuth operation to fail"),
            Err(error) => error,
        }
    }

    fn broker(transport: Arc<MockTransport>) -> SoundCloudOAuthBroker {
        SoundCloudOAuthBroker::new(
            SoundCloudOAuthConfig::for_test(
                "client-id",
                "client-secret",
                ["rec-bag://soundcloud/callback"],
            ),
            transport,
            Arc::new(OAuthRateLimiter::new(20, Duration::from_secs(60), 100)),
        )
    }

    fn exchange_request() -> SoundCloudAuthorizationCodeExchange {
        SoundCloudAuthorizationCodeExchange {
            code: "authorization-code".to_string(),
            code_verifier: "a".repeat(43),
            redirect_uri: "rec-bag://soundcloud/callback".to_string(),
            state: "s".repeat(32),
        }
    }

    fn token_response(access_token: &str, refresh_token: &str) -> SoundCloudOAuthHttpResponse {
        SoundCloudOAuthHttpResponse {
            status: 200,
            body: format!(
                "{{\"access_token\":\"{access_token}\",\"refresh_token\":\"{refresh_token}\",\"expires_in\":3600,\"scope\":\"non-expiring\",\"token_type\":\"OAuth\"}}"
            ),
        }
    }

    #[tokio::test]
    async fn exchanges_code_with_exact_pkce_and_allow_listed_redirect() {
        let transport = Arc::new(MockTransport::with_response(token_response(
            "access", "refresh",
        )));
        let broker = broker(transport.clone());
        let token = broker
            .exchange_authorization_code(client(1), exchange_request())
            .await
            .unwrap();

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token, "refresh");
        let request = &transport.requests.lock().unwrap()[0];
        assert_eq!(request.url, SOUNDCLOUD_TOKEN_URL);
        assert_eq!(request.form["grant_type"], "authorization_code");
        assert_eq!(request.form["client_secret"], "client-secret");
        assert_eq!(request.form["code_verifier"], "a".repeat(43));
        assert_eq!(request.form["code"], "authorization-code");
        assert!(!request.form.contains_key("state"));
    }

    #[tokio::test]
    async fn rejects_malformed_input_and_non_exact_redirect_before_transport() {
        let transport = Arc::new(MockTransport::default());
        let broker = broker(transport.clone());
        for mutate in [
            |request: &mut SoundCloudAuthorizationCodeExchange| request.state.clear(),
            |request: &mut SoundCloudAuthorizationCodeExchange| request.code_verifier.push('!'),
            |request: &mut SoundCloudAuthorizationCodeExchange| request.code.push('\n'),
        ] {
            let mut request = exchange_request();
            mutate(&mut request);
            assert_eq!(
                oauth_error(broker.exchange_authorization_code(client(1), request).await,).kind,
                SoundCloudOAuthErrorKind::InvalidInput
            );
        }
        let mut request = exchange_request();
        request.redirect_uri = "rec-bag://soundcloud/callback/".to_string();
        assert_eq!(
            oauth_error(broker.exchange_authorization_code(client(1), request).await,).kind,
            SoundCloudOAuthErrorKind::RedirectNotAllowed
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn public_config_exposes_client_id_and_loopback_redirect_only() {
        let broker = broker_with_redirects([
            "rec-bag://soundcloud/callback",
            "http://127.0.0.1:17841/oauth/soundcloud",
        ]);
        let config = broker.public_config().unwrap();
        assert_eq!(config.client_id, "client-id");
        assert_eq!(config.redirect_uri, "http://127.0.0.1:17841/oauth/soundcloud");

        let custom_scheme_only = broker_with_redirects(["rec-bag://soundcloud/callback"]);
        assert_eq!(
            oauth_error(custom_scheme_only.public_config()).kind,
            SoundCloudOAuthErrorKind::Misconfigured
        );
    }

    fn broker_with_redirects<'a>(
        redirects: impl IntoIterator<Item = &'a str>,
    ) -> SoundCloudOAuthBroker {
        SoundCloudOAuthBroker::new(
            SoundCloudOAuthConfig::for_test("client-id", "client-secret", redirects),
            Arc::new(MockTransport::default()),
            Arc::new(OAuthRateLimiter::new(20, Duration::from_secs(60), 100)),
        )
    }

    #[tokio::test]
    async fn empty_scope_from_soundcloud_is_accepted_as_absent() {
        let transport = Arc::new(MockTransport::with_response(SoundCloudOAuthHttpResponse {
            status: 200,
            body: r#"{"access_token":"eyJ.a.b","expires_in":3599,"refresh_token":"refresh","scope":"","token_type":"bearer"}"#
                .to_string(),
        }));
        let token = broker(transport)
            .exchange_authorization_code(client(1), exchange_request())
            .await
            .unwrap();
        assert_eq!(token.scope, None);
        assert_eq!(token.token_type.as_deref(), Some("bearer"));
    }

    #[tokio::test]
    async fn refresh_forwards_old_token_and_returns_rotation() {
        let transport = Arc::new(MockTransport::with_response(token_response(
            "new-access",
            "new-refresh",
        )));
        let broker = broker(transport.clone());
        let token = broker
            .refresh_token(
                client(1),
                SoundCloudRefreshTokenExchange {
                    refresh_token: "old-refresh".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(token.refresh_token, "new-refresh");
        assert_eq!(
            transport.requests.lock().unwrap()[0].form["refresh_token"],
            "old-refresh"
        );
    }

    #[tokio::test]
    async fn provider_failures_and_malformed_responses_are_safely_mapped() {
        for (response, expected) in [
            (
                SoundCloudOAuthHttpResponse {
                    status: 400,
                    body: "access_token=private&client_secret=private".to_string(),
                },
                SoundCloudOAuthErrorKind::ProviderRejected,
            ),
            (
                SoundCloudOAuthHttpResponse {
                    status: 200,
                    body: "not-json private-token".to_string(),
                },
                SoundCloudOAuthErrorKind::InvalidProviderResponse,
            ),
            (
                SoundCloudOAuthHttpResponse {
                    status: 200,
                    body: "{\"access_token\":\"a\",\"refresh_token\":\"\"}".to_string(),
                },
                SoundCloudOAuthErrorKind::InvalidProviderResponse,
            ),
            (
                SoundCloudOAuthHttpResponse {
                    status: 429,
                    body: "provider details must be discarded".to_string(),
                },
                SoundCloudOAuthErrorKind::RateLimited,
            ),
            (
                SoundCloudOAuthHttpResponse {
                    status: 500,
                    body: "provider details must be discarded".to_string(),
                },
                SoundCloudOAuthErrorKind::TemporarilyUnavailable,
            ),
        ] {
            let broker = broker(Arc::new(MockTransport::with_response(response)));
            let error = oauth_error(
                broker
                    .exchange_authorization_code(client(1), exchange_request())
                    .await,
            );
            assert_eq!(error.kind, expected);
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("private"));
            assert!(!rendered.contains("access_token"));
            assert!(!rendered.contains("client_secret"));
            assert!(!rendered.contains("provider details"));
        }

        let broker = broker(Arc::new(MockTransport::with_transport_error()));
        assert_eq!(
            oauth_error(
                broker
                    .exchange_authorization_code(client(1), exchange_request())
                    .await,
            )
            .kind,
            SoundCloudOAuthErrorKind::TemporarilyUnavailable
        );
    }

    #[test]
    fn parses_environment_strictly_without_reading_process_globals() {
        let valid = |name: &str| match name {
            "SOUNDCLOUD_CLIENT_ID" => Some("id".to_string()),
            "SOUNDCLOUD_KEY" => Some("secret".to_string()),
            "SOUNDCLOUD_ALLOWED_REDIRECT_URIS" => {
                Some("rec-bag://soundcloud/callback,https://example.com/oauth".to_string())
            }
            "SOUNDCLOUD_OAUTH_RATE_LIMIT" => Some("7".to_string()),
            _ => None,
        };
        let config = SoundCloudOAuthConfig::from_lookup(valid).unwrap();
        assert_eq!(config.rate_limit, 7);
        assert_eq!(config.rate_window, Duration::from_secs(60));
        assert_eq!(config.max_rate_limit_clients, 10_000);
        assert_eq!(config.http_timeout, Duration::from_secs(15));
        assert_eq!(config.allowed_redirect_uris.len(), 2);

        for (name, value) in [
            (
                "SOUNDCLOUD_ALLOWED_REDIRECT_URIS",
                "http://example.com/callback",
            ),
            (
                "SOUNDCLOUD_ALLOWED_REDIRECT_URIS",
                "https://user@example.com/callback",
            ),
            ("SOUNDCLOUD_OAUTH_RATE_LIMIT", "0"),
            ("SOUNDCLOUD_OAUTH_HTTP_TIMEOUT_SECONDS", "invalid"),
        ] {
            let result = SoundCloudOAuthConfig::from_lookup(|key| {
                if key == name {
                    Some(value.to_string())
                } else {
                    valid(key)
                }
            });
            assert_eq!(
                oauth_error(result).kind,
                SoundCloudOAuthErrorKind::Misconfigured
            );
        }
    }

    #[test]
    fn validates_single_ip_identity_and_rejects_forwarding_chains() {
        assert_eq!(
            OAuthClientIp::parse(" 2001:db8::1 ").unwrap().0,
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
        assert!(OAuthClientIp::parse("192.0.2.1, 10.0.0.1").is_err());
        assert!(OAuthClientIp::parse("attacker").is_err());
    }

    #[test]
    fn limiter_is_per_client_and_uses_a_sliding_window() {
        let limiter = OAuthRateLimiter::new(2, Duration::from_secs(10), 10);
        let start = Instant::now();
        assert!(limiter.acquire_at(client(1), start).is_ok());
        assert!(limiter
            .acquire_at(client(1), start + Duration::from_secs(1))
            .is_ok());
        assert_eq!(
            limiter
                .acquire_at(client(1), start + Duration::from_secs(9))
                .unwrap_err()
                .kind,
            SoundCloudOAuthErrorKind::RateLimited
        );
        assert!(limiter
            .acquire_at(client(2), start + Duration::from_secs(9))
            .is_ok());
        assert!(limiter
            .acquire_at(client(1), start + Duration::from_secs(10))
            .is_ok());
    }

    #[test]
    fn limiter_is_atomic_under_concurrent_requests_and_bounds_clients() {
        let limiter = Arc::new(OAuthRateLimiter::new(5, Duration::from_secs(60), 2));
        let barrier = Arc::new(Barrier::new(21));
        let mut workers = Vec::new();
        for _ in 0..20 {
            let limiter = limiter.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                limiter.acquire(client(1)).is_ok()
            }));
        }
        barrier.wait();
        let accepted: usize = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum();
        assert_eq!(accepted, 5);
        assert!(limiter.acquire(client(2)).is_ok());
        assert_eq!(
            limiter.acquire(client(3)).unwrap_err().kind,
            SoundCloudOAuthErrorKind::RateLimited
        );
    }
}
