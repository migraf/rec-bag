//! Transport and credential seams for the Google Drive adapter.
//!
//! Keeping these interfaces small means application code can choose its HTTP client
//! and secure OAuth implementation, while tests can use a deterministic fake.

use rec_core::BoxFuture;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// A bearer token ready for use with the Drive API.
///
/// Implementations should refresh expired credentials before returning a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleAccessToken(pub String);

/// Failure returned by the application's OAuth token service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleTokenError {
    pub message: String,
    /// Whether asking the provider again could obtain a refreshed token.
    pub retryable: bool,
}

impl GoogleTokenError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

impl fmt::Display for GoogleTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for GoogleTokenError {}

/// Supplies a valid OAuth access token for the selected Google account.
///
/// Token persistence and refresh-token exchange deliberately stay outside this
/// crate so credentials are not stored in a music-storage configuration.
pub trait GoogleTokenProvider: Send + Sync {
    fn access_token(&self) -> BoxFuture<'_, Result<GoogleAccessToken, GoogleTokenError>>;

    /// Invalidates a token rejected by Drive. Production providers should evict it
    /// and refresh on the next [`Self::access_token`] call.
    fn invalidate_token<'a>(
        &'a self,
        _rejected_token: &'a GoogleAccessToken,
    ) -> BoxFuture<'a, Result<(), GoogleTokenError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Client-neutral HTTP request used by [`DriveHttpTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl DriveHttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

/// Client-neutral HTTP response used by [`DriveHttpTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl DriveHttpResponse {
    pub fn header(&self, expected_name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(name, value)| {
            name.eq_ignore_ascii_case(expected_name)
                .then_some(value.as_str())
        })
    }
}

/// Transport-level failure before the Drive API returned a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveTransportError {
    pub message: String,
    pub retryable: bool,
}

impl DriveTransportError {
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }
}

impl fmt::Display for DriveTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for DriveTransportError {}

/// Executes HTTP requests. Production code can implement this with `reqwest` or
/// another HTTP client; fakes need no network server.
pub trait DriveHttpTransport: Send + Sync {
    fn execute(
        &self,
        request: DriveHttpRequest,
    ) -> BoxFuture<'_, Result<DriveHttpResponse, DriveTransportError>>;
}
