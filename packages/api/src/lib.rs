//! Stateless fullstack server functions and OAuth broker DTOs for rec_bag.
//!
//! The SoundCloud broker loads deployment settings from environment variables, optionally using
//! the repository-root `.env` during local development. It never persists or logs user tokens.

#[cfg(feature = "server")]
use dioxus::fullstack::HeaderMap;
use dioxus::prelude::*;
use std::{future::Future, pin::Pin};

pub mod soundcloud_oauth;

pub use soundcloud_oauth::{
    OAuthClientIp, SoundCloudAuthorizationCodeExchange, SoundCloudOAuthError,
    SoundCloudOAuthErrorKind, SoundCloudPublicConfig, SoundCloudRefreshTokenExchange,
    SoundCloudTokenResponse,
};

/// A boxed sendable future used by the OAuth HTTP transport boundary.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Public SoundCloud desktop OAuth settings (client ID and loopback redirect URI), so the
/// desktop app does not need them entered by hand.
#[get("/api/soundcloud/oauth/config")]
pub async fn soundcloud_public_config() -> Result<SoundCloudPublicConfig, ServerFnError> {
    soundcloud_oauth::environment_broker()
        .and_then(|broker| broker.public_config())
        .map_err(server_error)
}

/// Exchange a SoundCloud authorization code using the server-only application credentials.
///
/// Before calling this endpoint desktop must compare callback `state` with its locally stored
/// nonce. The broker validates only input shape and redirect allow-list membership because it is
/// intentionally stateless and has no desktop session to inspect.
#[post("/api/soundcloud/oauth/exchange", headers: HeaderMap)]
pub async fn exchange_soundcloud_authorization_code(
    request: SoundCloudAuthorizationCodeExchange,
) -> Result<SoundCloudTokenResponse, ServerFnError> {
    let client = trusted_client_ip(&headers).map_err(server_error)?;
    soundcloud_oauth::environment_broker()
        .map_err(server_error)?
        .exchange_authorization_code(client, request)
        .await
        .map_err(server_error)
}

/// Rotate a SoundCloud refresh token. The returned refresh token replaces the previous one and
/// must be saved atomically by the desktop credential-store implementation.
#[post("/api/soundcloud/oauth/refresh", headers: HeaderMap)]
pub async fn refresh_soundcloud_token(
    request: SoundCloudRefreshTokenExchange,
) -> Result<SoundCloudTokenResponse, ServerFnError> {
    let client = trusted_client_ip(&headers).map_err(server_error)?;
    soundcloud_oauth::environment_broker()
        .map_err(server_error)?
        .refresh_token(client, request)
        .await
        .map_err(server_error)
}

/// Release builds require the trusted proxy's `X-Real-IP`. Debug builds (local `dx serve`, no
/// proxy) treat a missing header as the loopback client; a present but malformed header is still
/// rejected.
#[cfg(feature = "server")]
fn trusted_client_ip(headers: &HeaderMap) -> Result<OAuthClientIp, SoundCloudOAuthError> {
    match headers.get("x-real-ip") {
        Some(value) => OAuthClientIp::parse(value.to_str().unwrap_or_default()),
        None if cfg!(debug_assertions) => OAuthClientIp::parse("127.0.0.1"),
        None => OAuthClientIp::parse(""),
    }
}

#[cfg(feature = "server")]
fn server_error(error: SoundCloudOAuthError) -> ServerFnError {
    ServerFnError::ServerError {
        message: error.message.to_string(),
        code: error.status_code,
        details: None,
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn trusted_proxy_identity_is_required_and_must_be_one_ip() {
        let mut headers = HeaderMap::new();
        assert_eq!(trusted_client_ip(&headers).is_ok(), cfg!(debug_assertions));
        headers.insert("x-real-ip", "not-an-ip".parse().unwrap());
        assert!(trusted_client_ip(&headers).is_err());
        headers.insert("x-real-ip", "192.0.2.10".parse().unwrap());
        assert!(trusted_client_ip(&headers).is_ok());
        headers.insert("x-real-ip", "192.0.2.10, 198.51.100.1".parse().unwrap());
        assert!(trusted_client_ip(&headers).is_err());
    }

    #[test]
    fn server_function_errors_expose_only_static_safe_fields() {
        let secret = "code-token-secret";
        let source = SoundCloudOAuthError {
            kind: SoundCloudOAuthErrorKind::ProviderRejected,
            status_code: 400,
            message: "SoundCloud rejected the OAuth request",
        };
        let error = server_error(source);
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("SoundCloud rejected the OAuth request"));
        assert!(!rendered.contains(secret));
        match error {
            ServerFnError::ServerError { code, details, .. } => {
                assert_eq!(code, 400);
                assert!(details.is_none());
            }
            other => panic!("unexpected server error: {other:?}"),
        }
    }
}

/// Echo the user input on the server.
#[post("/api/echo")]
pub async fn echo(input: String) -> Result<String, ServerFnError> {
    Ok(input)
}
