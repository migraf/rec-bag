//! Google Drive storage driver: OAuth authorisation, Drive folder selection,
//! the Drive store for each storage, and the account grant its storages share.
//!
//! OAuth credentials remain behind [`CredentialStore`]. Only folder identity,
//! an opaque credential reference, and non-secret preferences enter the catalog.
//! Every failure leaves this module as a [`StorageError`].

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use music_storage::{Provisioned, RootLocator, StorageDriver, StorageError, StorageSetup};
use music_storage_google_drive::{
    DriveHttpRequest, DriveHttpTransport, GoogleAccessToken, GoogleTokenError, GoogleTokenProvider,
};
#[cfg(any(feature = "desktop", test))]
use music_storage_google_drive::{DriveHttpResponse, DriveTransportError};
#[cfg(feature = "desktop")]
use music_storage_google_drive::{
    GoogleDriveFolder, GoogleDriveMusicStore, GoogleDriveStoreConfig,
};
use rand::RngCore;
use rec_core::{BoxFuture, MusicAssetStore, MusicStorage, MusicStorageError, MusicStorageKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use crate::services::integration::{CredentialKey, CredentialStore, Secret};
use crate::state::integration::IntegrationError;

const PROVIDER_KEY: &str = "google_drive";
const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const DRIVE_API_FILES: &str = "https://www.googleapis.com/drive/v3/files";
const GOOGLE_REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";
const DEFAULT_DRIVE_FOLDER_NAME: &str = "rec_bag Music";
const CONFIG_CREDENTIAL_ACCOUNT: &str = "credential_account";
const CONFIG_FOLDER_NAME: &str = "folder_display_name";

type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, GoogleDriveError>> + Send + 'a>>;

/// What can go wrong inside the Drive driver. It never leaves the driver:
/// [`drive_storage_error`] turns it into a [`StorageError`].
#[derive(Debug, Error)]
pub enum GoogleDriveError {
    // Only the desktop build reads a persisted Drive configuration.
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    #[error("invalid Google Drive configuration: {0}")]
    InvalidConfiguration(String),
    #[error("Google authentication failed: {0}")]
    Authentication(String),
    #[error("Google Drive folder selection failed: {0}")]
    FolderSelection(String),
    #[error("credential storage failed: {0}")]
    Credential(String),
    #[error("Google Drive storage operation failed: {0}")]
    Storage(#[from] MusicStorageError),
    // Only a build without the desktop feature lacks a runtime.
    #[cfg_attr(feature = "desktop", allow(dead_code))]
    #[error("configured runtime is unavailable: {0}")]
    Runtime(String),
    #[error("invalid persisted data: {0}")]
    InvalidData(String),
}

/// Reports a driver-internal failure to `music_storage`; the text is the
/// driver's own, so the dashboard's failure guidance still recognises it.
fn drive_storage_error(error: GoogleDriveError) -> StorageError {
    match error {
        GoogleDriveError::Storage(error) => StorageError::Provider(error),
        other => StorageError::Driver(other.to_string()),
    }
}

impl GoogleDriveError {
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    fn retryable(&self) -> bool {
        match self {
            Self::Storage(error) => error.is_retryable(),
            Self::Runtime(_) => true,
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PkceState {
    pub state: String,
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for PkceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PkceState")
            .field("state", &"[REDACTED]")
            .field("verifier", &"[REDACTED]")
            .field("challenge", &"[REDACTED]")
            .finish()
    }
}

pub fn generate_pkce_state() -> PkceState {
    let mut state = [0_u8; 32];
    let mut verifier = [0_u8; 64];
    rand::rng().fill_bytes(&mut state);
    rand::rng().fill_bytes(&mut verifier);
    let state = URL_SAFE_NO_PAD.encode(state);
    let verifier = URL_SAFE_NO_PAD.encode(verifier);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkceState {
        state,
        verifier,
        challenge,
    }
}

pub fn verify_oauth_state(expected: &str, received: &str) -> bool {
    expected.len() == received.len()
        && expected.as_bytes().ct_eq(received.as_bytes()).unwrap_u8() == 1
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for GoogleCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleCredential")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl GoogleCredential {
    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.expires_at
            .is_none_or(|expires| expires > now + chrono::Duration::seconds(30))
    }
}

#[derive(Deserialize)]
struct GoogleTokenEndpointResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    token_type: Option<String>,
}

#[derive(Deserialize)]
struct GoogleTokenErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

/// Formats only Google's short `error`/`error_description` fields; the raw body is never echoed.
fn google_token_error_detail(body: &[u8]) -> String {
    let short = |value: Option<String>| {
        value.filter(|value| {
            !value.trim().is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
        })
    };
    let Ok(value) = serde_json::from_slice::<GoogleTokenErrorResponse>(body) else {
        return String::new();
    };
    match (short(value.error), short(value.error_description)) {
        (Some(error), Some(description)) => format!(": {error} ({description})"),
        (Some(error), None) => format!(": {error}"),
        (None, Some(description)) => format!(": {description}"),
        (None, None) => String::new(),
    }
}

fn parse_google_token_response(
    status: u16,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<GoogleCredential, GoogleDriveError> {
    if !(200..300).contains(&status) {
        return Err(GoogleDriveError::Authentication(format!(
            "Google token endpoint returned {status}{}",
            google_token_error_detail(body)
        )));
    }
    let value: GoogleTokenEndpointResponse = serde_json::from_slice(body)
        .map_err(|error| GoogleDriveError::Authentication(error.to_string()))?;
    let scopes = value.scope.as_deref().unwrap_or_default();
    if !scopes
        .split_ascii_whitespace()
        .any(|scope| scope == DRIVE_FILE_SCOPE)
    {
        return Err(GoogleDriveError::Authentication(
            "Google token response did not grant the required drive.file scope".into(),
        ));
    }
    if !value
        .token_type
        .as_deref()
        .is_some_and(|token_type| token_type.eq_ignore_ascii_case("bearer"))
    {
        return Err(GoogleDriveError::Authentication(
            "Google token response did not declare Bearer token_type".into(),
        ));
    }
    if value.access_token.trim().is_empty() {
        return Err(GoogleDriveError::Authentication(
            "Google token response contained an empty access token".into(),
        ));
    }
    Ok(GoogleCredential {
        access_token: value.access_token,
        refresh_token: value.refresh_token,
        expires_at: value
            .expires_in
            .map(|seconds| now + chrono::Duration::seconds(seconds)),
        account_id: None,
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct GoogleOAuthRequest {
    pub authorization_url: Url,
    pub state: String,
    pub verifier: String,
    pub redirect_uri: Url,
}

impl std::fmt::Debug for GoogleOAuthRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleOAuthRequest")
            .field("authorization_url", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("verifier", &"[REDACTED]")
            .field("redirect_uri", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GoogleOAuthCallback {
    pub code: String,
    pub state: String,
}

impl std::fmt::Debug for GoogleOAuthCallback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleOAuthCallback")
            .field("code", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .finish()
    }
}

pub trait GoogleOAuth: Send + Sync {
    fn begin(&self, redirect_uri: Url) -> ServiceFuture<'_, GoogleOAuthRequest>;
    fn exchange<'a>(
        &'a self,
        request: &'a GoogleOAuthRequest,
        callback: GoogleOAuthCallback,
    ) -> ServiceFuture<'a, GoogleCredential>;
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> ServiceFuture<'a, GoogleCredential>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedDriveFolder {
    pub id: String,
    pub display_name: String,
}

pub trait GoogleDriveFolderSelector: Send + Sync {
    fn select_folder<'a>(&'a self, access_token: &'a str)
        -> ServiceFuture<'a, SelectedDriveFolder>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProvisionedDrive {
    pub credential_account: String,
    pub credential: GoogleCredential,
    pub folder: SelectedDriveFolder,
}

impl std::fmt::Debug for ProvisionedDrive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProvisionedDrive")
            .field("credential_account", &"[REDACTED]")
            .field("credential", &"[REDACTED]")
            .field("folder", &self.folder)
            .finish()
    }
}

/// Authorises an account and selects the Drive folder for a new storage. The
/// returned credential is stored once, by [`DriveDriver`].
pub trait GoogleDriveProvisioner: Send + Sync {
    fn provision<'a>(&'a self, setup: &'a StorageSetup) -> ServiceFuture<'a, ProvisionedDrive>;
}

/// Composes OAuth and folder-selection seams for callers that own callback capture.
pub struct OAuthFolderProvisioner<O, F, C>
where
    O: GoogleOAuth,
    F: GoogleDriveFolderSelector,
    C: Fn(&GoogleOAuthRequest) -> ServiceFuture<'_, GoogleOAuthCallback> + Send + Sync,
{
    oauth: O,
    folders: F,
    redirect_uri: Url,
    callback: C,
}

impl<O, F, C> OAuthFolderProvisioner<O, F, C>
where
    O: GoogleOAuth,
    F: GoogleDriveFolderSelector,
    C: Fn(&GoogleOAuthRequest) -> ServiceFuture<'_, GoogleOAuthCallback> + Send + Sync,
{
    pub fn new(oauth: O, folders: F, redirect_uri: Url, callback: C) -> Self {
        Self {
            oauth,
            folders,
            redirect_uri,
            callback,
        }
    }
}

impl<O, F, C> GoogleDriveProvisioner for OAuthFolderProvisioner<O, F, C>
where
    O: GoogleOAuth,
    F: GoogleDriveFolderSelector,
    C: Fn(&GoogleOAuthRequest) -> ServiceFuture<'_, GoogleOAuthCallback> + Send + Sync,
{
    fn provision<'a>(&'a self, _setup: &'a StorageSetup) -> ServiceFuture<'a, ProvisionedDrive> {
        Box::pin(async move {
            let request = self.oauth.begin(self.redirect_uri.clone()).await?;
            let callback = (self.callback)(&request).await?;
            if !verify_oauth_state(&request.state, &callback.state) {
                return Err(GoogleDriveError::Authentication(
                    "OAuth callback state did not match".into(),
                ));
            }
            let credential = self.oauth.exchange(&request, callback).await?;
            let folder = self.folders.select_folder(&credential.access_token).await?;
            validate_folder(&folder)?;
            let account = credential
                .account_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            Ok(ProvisionedDrive {
                credential_account: account,
                credential,
                folder,
            })
        })
    }
}

fn validate_folder(folder: &SelectedDriveFolder) -> Result<(), GoogleDriveError> {
    if folder.id.trim().is_empty() {
        return Err(GoogleDriveError::FolderSelection(
            "selected folder has no stable Drive ID".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveFolderSelection {
    /// Create a folder through `files.create`; `drive.file` then grants access to it.
    CreateDedicated { name: String },
    /// Validate an identity returned by Google Picker under the same OAuth grant.
    PickerGranted { folder_id: String },
}

impl Default for DriveFolderSelection {
    fn default() -> Self {
        Self::CreateDedicated {
            name: DEFAULT_DRIVE_FOLDER_NAME.into(),
        }
    }
}

pub struct TransportDriveFolderSelector<T> {
    transport: T,
    selection: DriveFolderSelection,
    api_base_url: String,
}

impl<T> TransportDriveFolderSelector<T> {
    pub fn new(transport: T, selection: DriveFolderSelection) -> Self {
        Self {
            transport,
            selection,
            api_base_url: DRIVE_API_FILES.into(),
        }
    }

    #[cfg(test)]
    fn with_api_base_url(mut self, value: impl Into<String>) -> Self {
        self.api_base_url = value.into();
        self
    }
}

impl<T: DriveHttpTransport> GoogleDriveFolderSelector for TransportDriveFolderSelector<T> {
    fn select_folder<'a>(
        &'a self,
        access_token: &'a str,
    ) -> ServiceFuture<'a, SelectedDriveFolder> {
        Box::pin(async move {
            if access_token.trim().is_empty() {
                return Err(GoogleDriveError::Authentication(
                    "Google access token is empty".into(),
                ));
            }
            let request = match &self.selection {
                DriveFolderSelection::CreateDedicated { name } => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(GoogleDriveError::FolderSelection(
                            "dedicated Drive folder name cannot be empty".into(),
                        ));
                    }
                    DriveHttpRequest::new(
                        "POST",
                        format!(
                            "{}?fields=id,name,mimeType,trashed&supportsAllDrives=false",
                            self.api_base_url
                        ),
                    )
                    .header("Authorization", format!("Bearer {access_token}"))
                    .header("Content-Type", "application/json")
                    .body(
                        serde_json::to_vec(&serde_json::json!({
                            "name": name,
                            "mimeType": "application/vnd.google-apps.folder",
                        }))
                        .expect("folder metadata serializes"),
                    )
                }
                DriveFolderSelection::PickerGranted { folder_id } => {
                    validate_picker_folder_id(folder_id)?;
                    DriveHttpRequest::new(
                        "GET",
                        format!(
                            "{}/{}?fields=id,name,mimeType,trashed,driveId&supportsAllDrives=false",
                            self.api_base_url, folder_id
                        ),
                    )
                    .header("Authorization", format!("Bearer {access_token}"))
                }
            };
            let response = self
                .transport
                .execute(request)
                .await
                .map_err(|error| GoogleDriveError::FolderSelection(error.message))?;
            if !(200..300).contains(&response.status) {
                return Err(GoogleDriveError::FolderSelection(format!(
                    "Drive folder operation returned {}",
                    response.status
                )));
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct FolderResponse {
                id: String,
                name: String,
                mime_type: String,
                #[serde(default)]
                trashed: bool,
                drive_id: Option<String>,
            }
            let folder: FolderResponse = serde_json::from_slice(&response.body)
                .map_err(|error| GoogleDriveError::FolderSelection(error.to_string()))?;
            if folder.trashed
                || folder.mime_type != "application/vnd.google-apps.folder"
                || folder.drive_id.is_some()
            {
                return Err(GoogleDriveError::FolderSelection(
                    "selected Drive item is not an available My Drive folder".into(),
                ));
            }
            if let DriveFolderSelection::PickerGranted { folder_id } = &self.selection {
                if &folder.id != folder_id {
                    return Err(GoogleDriveError::FolderSelection(
                        "Drive returned a different folder than Picker granted".into(),
                    ));
                }
            }
            let selected = SelectedDriveFolder {
                id: folder.id,
                display_name: folder.name,
            };
            validate_folder(&selected)?;
            Ok(selected)
        })
    }
}

fn validate_picker_folder_id(folder_id: &str) -> Result<(), GoogleDriveError> {
    if folder_id.is_empty()
        || !folder_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(GoogleDriveError::FolderSelection(
            "Picker folder identity is invalid".into(),
        ));
    }
    Ok(())
}

pub struct GoogleOAuthRevoker<T> {
    transport: T,
    endpoint: String,
}

impl<T> GoogleOAuthRevoker<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            endpoint: GOOGLE_REVOKE_ENDPOINT.into(),
        }
    }

    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

impl<T: DriveHttpTransport> GoogleOAuthRevoker<T> {
    pub async fn revoke(&self, token: &str) -> Result<(), GoogleDriveError> {
        if token.trim().is_empty() {
            return Err(GoogleDriveError::Authentication(
                "cannot revoke an empty Google token".into(),
            ));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .finish();
        let response = self
            .transport
            .execute(
                DriveHttpRequest::new("POST", self.endpoint.clone())
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.into_bytes()),
            )
            .await
            .map_err(|error| GoogleDriveError::Authentication(error.message))?;
        if (200..300).contains(&response.status) || response.status == 400 {
            // Google returns 400 when a token is already invalid. Treat that as a
            // successful terminal state so a retried revoke can finish local cleanup.
            Ok(())
        } else {
            Err(GoogleDriveError::Authentication(format!(
                "Google revoke endpoint returned {}",
                response.status
            )))
        }
    }
}

pub trait GoogleGrantRevoker: Send + Sync {
    fn revoke<'a>(&'a self, token: &'a str) -> ServiceFuture<'a, ()>;
}

impl<T: DriveHttpTransport + Send + Sync> GoogleGrantRevoker for GoogleOAuthRevoker<T> {
    fn revoke<'a>(&'a self, token: &'a str) -> ServiceFuture<'a, ()> {
        Box::pin(async move { GoogleOAuthRevoker::revoke(self, token).await })
    }
}

#[cfg(not(feature = "desktop"))]
struct UnavailableGoogleGrantRevoker;

#[cfg(not(feature = "desktop"))]
impl GoogleGrantRevoker for UnavailableGoogleGrantRevoker {
    fn revoke<'a>(&'a self, _token: &'a str) -> ServiceFuture<'a, ()> {
        Box::pin(async {
            Err(GoogleDriveError::Runtime(
                "Google grant revocation requires the desktop feature".into(),
            ))
        })
    }
}

pub struct DesktopGoogleTokenProvider {
    key: CredentialKey,
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn GoogleOAuth>,
    cached: std::sync::Mutex<Option<GoogleCredential>>,
}

impl DesktopGoogleTokenProvider {
    pub fn new(
        account: impl Into<String>,
        credentials: Arc<dyn CredentialStore>,
        oauth: Arc<dyn GoogleOAuth>,
    ) -> Self {
        Self {
            key: CredentialKey {
                provider: PROVIDER_KEY.into(),
                account: account.into(),
            },
            credentials,
            oauth,
            cached: std::sync::Mutex::new(None),
        }
    }

    async fn load(&self) -> Result<GoogleCredential, GoogleTokenError> {
        let secret = self
            .credentials
            .load(&self.key)
            .await
            .map_err(|error| token_error(error.to_string(), true))?
            .ok_or_else(|| token_error("Google credentials are not available", false))?;
        serde_json::from_slice(secret.expose())
            .map_err(|_| token_error("stored Google credentials are invalid", false))
    }

    async fn store(&self, credential: &GoogleCredential) -> Result<(), GoogleTokenError> {
        let bytes = serde_json::to_vec(credential)
            .map_err(|_| token_error("Google credentials could not be encoded", false))?;
        self.credentials
            .store(&self.key, Secret::new(bytes))
            .await
            .map_err(|error| token_error(error.to_string(), true))
    }
}

impl GoogleTokenProvider for DesktopGoogleTokenProvider {
    fn access_token(&self) -> BoxFuture<'_, Result<GoogleAccessToken, GoogleTokenError>> {
        Box::pin(async move {
            if let Some(credential) = self.cached.lock().unwrap().clone() {
                if credential.is_fresh(Utc::now()) {
                    return Ok(GoogleAccessToken(credential.access_token));
                }
            }
            let stored = self.load().await?;
            let credential = if stored.is_fresh(Utc::now()) {
                stored
            } else {
                let refresh_token = stored
                    .refresh_token
                    .as_deref()
                    .ok_or_else(|| token_error("Google authorization must be renewed", false))?;
                let mut refreshed = self
                    .oauth
                    .refresh(refresh_token)
                    .await
                    .map_err(|error| token_error(error.to_string(), error.retryable()))?;
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = stored.refresh_token;
                }
                if refreshed.account_id.is_none() {
                    refreshed.account_id = stored.account_id;
                }
                self.store(&refreshed).await?;
                refreshed
            };
            let token = GoogleAccessToken(credential.access_token.clone());
            *self.cached.lock().unwrap() = Some(credential);
            Ok(token)
        })
    }

    fn invalidate_token<'a>(
        &'a self,
        _rejected_token: &'a GoogleAccessToken,
    ) -> BoxFuture<'a, Result<(), GoogleTokenError>> {
        Box::pin(async move {
            *self.cached.lock().unwrap() = None;
            Ok(())
        })
    }
}

fn token_error(message: impl Into<String>, retryable: bool) -> GoogleTokenError {
    GoogleTokenError {
        message: message.into(),
        retryable,
    }
}

/// The grant revoker used outside tests: Google's revoke endpoint when the
/// desktop feature provides HTTP, otherwise one that reports it is missing.
fn production_grant_revoker() -> Arc<dyn GoogleGrantRevoker> {
    #[cfg(feature = "desktop")]
    {
        Arc::new(GoogleOAuthRevoker::new(ReqwestDriveTransport::default()))
    }
    #[cfg(not(feature = "desktop"))]
    {
        Arc::new(UnavailableGoogleGrantRevoker)
    }
}

async fn store_google_credential(
    credentials: &dyn CredentialStore,
    account: &str,
    credential: &GoogleCredential,
) -> Result<(), GoogleDriveError> {
    let bytes = serde_json::to_vec(credential)
        .map_err(|error| GoogleDriveError::InvalidData(error.to_string()))?;
    credentials
        .store(
            &CredentialKey {
                provider: PROVIDER_KEY.into(),
                account: account.to_owned(),
            },
            Secret::new(bytes),
        )
        .await
        .map_err(|error| GoogleDriveError::Credential(error.to_string()))
}

fn credential_account(storage: &MusicStorage) -> Option<&str> {
    storage
        .config
        .get(CONFIG_CREDENTIAL_ACCOUNT)
        .map(String::as_str)
        .filter(|account| !account.trim().is_empty())
}

/// Google Drive's storage driver: OAuth and folder provisioning, the Drive
/// store for each storage, and the account grant its storages share.
pub struct DriveDriver {
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn GoogleOAuth>,
    provisioner: Arc<dyn GoogleDriveProvisioner>,
    revoker: Arc<dyn GoogleGrantRevoker>,
    /// Opens stores in place of the Drive API, for tests.
    #[cfg(test)]
    test_opener: Option<Arc<TestStoreOpener>>,
}

#[cfg(test)]
type TestStoreOpener =
    dyn Fn(&MusicStorage) -> Result<Arc<dyn MusicAssetStore>, StorageError> + Send + Sync;

impl DriveDriver {
    pub fn new(
        credentials: Arc<dyn CredentialStore>,
        oauth: Arc<dyn GoogleOAuth>,
        provisioner: Arc<dyn GoogleDriveProvisioner>,
    ) -> Self {
        Self {
            credentials,
            oauth,
            provisioner,
            revoker: production_grant_revoker(),
            #[cfg(test)]
            test_opener: None,
        }
    }

    pub fn with_grant_revoker(mut self, revoker: Arc<dyn GoogleGrantRevoker>) -> Self {
        self.revoker = revoker;
        self
    }

    #[cfg(test)]
    fn with_test_opener(mut self, opener: Arc<TestStoreOpener>) -> Self {
        self.test_opener = Some(opener);
        self
    }
}

/// The Drive store for `storage`, authorised through the account's stored
/// credential. Tokens are only requested once the store is used.
#[cfg(feature = "desktop")]
fn open_drive_store(
    storage: &MusicStorage,
    credentials: &Arc<dyn CredentialStore>,
    oauth: &Arc<dyn GoogleOAuth>,
) -> Result<Arc<dyn MusicAssetStore>, StorageError> {
    let (account, folder) = parse_storage_config(storage).map_err(drive_storage_error)?;
    let tokens = DesktopGoogleTokenProvider::new(account, credentials.clone(), oauth.clone());
    let store = GoogleDriveMusicStore::new(
        GoogleDriveStoreConfig::new(storage.id, folder),
        ReqwestDriveTransport::default(),
        tokens,
    )?;
    Ok(Arc::new(store))
}

#[cfg(not(feature = "desktop"))]
fn open_drive_store(
    _storage: &MusicStorage,
    _credentials: &Arc<dyn CredentialStore>,
    _oauth: &Arc<dyn GoogleOAuth>,
) -> Result<Arc<dyn MusicAssetStore>, StorageError> {
    Err(StorageError::Driver(
        "Google Drive storage requires the desktop feature".into(),
    ))
}

impl StorageDriver for DriveDriver {
    fn kind(&self) -> MusicStorageKind {
        MusicStorageKind::GoogleDrive
    }

    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> BoxFuture<'a, Result<Provisioned, StorageError>> {
        Box::pin(async move {
            let provisioned = self
                .provisioner
                .provision(setup)
                .await
                .map_err(drive_storage_error)?;
            validate_folder(&provisioned.folder).map_err(drive_storage_error)?;
            store_google_credential(
                self.credentials.as_ref(),
                &provisioned.credential_account,
                &provisioned.credential,
            )
            .await
            .map_err(drive_storage_error)?;
            let mut config = BTreeMap::new();
            config.insert(
                CONFIG_CREDENTIAL_ACCOUNT.to_owned(),
                provisioned.credential_account,
            );
            config.insert(
                CONFIG_FOLDER_NAME.to_owned(),
                provisioned.folder.display_name.clone(),
            );
            Ok(Provisioned {
                root_identifier: provisioned.folder.id,
                suggested_name: Some(provisioned.folder.display_name),
                config,
            })
        })
    }

    fn open<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<Arc<dyn MusicAssetStore>, StorageError>> {
        #[cfg(test)]
        if let Some(open) = &self.test_opener {
            return Box::pin(std::future::ready(open(storage)));
        }
        Box::pin(std::future::ready(open_drive_store(
            storage,
            &self.credentials,
            &self.oauth,
        )))
    }

    fn root(&self, storage: &MusicStorage) -> Option<RootLocator> {
        Some(RootLocator::Remote {
            namespace: format!(
                "{PROVIDER_KEY}:{}",
                credential_account(storage).unwrap_or_default()
            ),
            path: vec![storage.root_identifier.clone()?],
        })
    }

    fn grant_id(&self, storage: &MusicStorage) -> Option<String> {
        credential_account(storage).map(str::to_owned)
    }

    /// Revokes the account's grant at Google (the refresh token when one is
    /// stored, else the access token), then deletes the stored credential.
    ///
    /// Google answers 400 for a token that is already invalid, which counts
    /// as revoked. Any other failure, such as being offline, keeps the
    /// credential so the revoke can be retried. Without a stored credential
    /// there is nothing to revoke, so repeating a revoke only deletes nothing.
    fn release_grant<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(async move {
            let Some(account) = credential_account(storage) else {
                return Ok(());
            };
            let key = CredentialKey {
                provider: PROVIDER_KEY.into(),
                account: account.to_owned(),
            };
            let stored = self.credentials.load(&key).await.map_err(|error| {
                drive_storage_error(GoogleDriveError::Credential(error.to_string()))
            })?;
            if let Some(secret) = stored {
                let credential: GoogleCredential = serde_json::from_slice(secret.expose())
                    .map_err(|error| {
                        drive_storage_error(GoogleDriveError::InvalidData(error.to_string()))
                    })?;
                let token = credential
                    .refresh_token
                    .as_deref()
                    .unwrap_or(&credential.access_token);
                self.revoker
                    .revoke(token)
                    .await
                    .map_err(drive_storage_error)?;
            }
            self.credentials.delete(&key).await.map_err(|error| {
                drive_storage_error(GoogleDriveError::Credential(error.to_string()))
            })
        })
    }
}

#[cfg(feature = "desktop")]
fn parse_storage_config(
    storage: &MusicStorage,
) -> Result<(String, GoogleDriveFolder), GoogleDriveError> {
    let account = storage
        .config
        .get(CONFIG_CREDENTIAL_ACCOUNT)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            GoogleDriveError::InvalidConfiguration("credential reference is missing".into())
        })?;
    let folder_id = storage
        .root_identifier
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            GoogleDriveError::InvalidConfiguration("Drive folder ID is missing".into())
        })?;
    let mut folder = GoogleDriveFolder::new(folder_id);
    if let Some(name) = storage.config.get(CONFIG_FOLDER_NAME) {
        folder = folder.with_display_name(name.clone());
    }
    Ok((account, folder))
}

#[cfg(feature = "desktop")]
pub struct ReqwestDriveTransport {
    client: reqwest::Client,
}

#[cfg(feature = "desktop")]
impl Default for ReqwestDriveTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[cfg(feature = "desktop")]
impl DriveHttpTransport for ReqwestDriveTransport {
    fn execute(
        &self,
        request: DriveHttpRequest,
    ) -> BoxFuture<'_, Result<DriveHttpResponse, DriveTransportError>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|error| DriveTransportError::new(error.to_string(), false))?;
            let mut builder = self.client.request(method, &request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let response = builder.body(request.body).send().await.map_err(|error| {
                DriveTransportError::new(
                    error.to_string(),
                    error.is_timeout() || error.is_connect(),
                )
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.to_string(), value.to_owned()))
                })
                .collect();
            let body = response
                .bytes()
                .await
                .map_err(|error| DriveTransportError::new(error.to_string(), true))?
                .to_vec();
            Ok(DriveHttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

#[cfg(feature = "desktop")]
#[derive(Clone)]
pub struct ReqwestGoogleOAuth {
    client: reqwest::Client,
    client_id: String,
    client_secret: Option<String>,
    authorization_endpoint: Url,
    token_endpoint: Url,
}

#[cfg(feature = "desktop")]
impl ReqwestGoogleOAuth {
    pub fn new(client_id: impl Into<String>, client_secret: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            client_id: client_id.into(),
            client_secret,
            authorization_endpoint: Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
                .unwrap(),
            token_endpoint: Url::parse("https://oauth2.googleapis.com/token").unwrap(),
        }
    }
}

#[cfg(feature = "desktop")]
impl GoogleOAuth for ReqwestGoogleOAuth {
    fn begin(&self, redirect_uri: Url) -> ServiceFuture<'_, GoogleOAuthRequest> {
        Box::pin(async move {
            let pkce = generate_pkce_state();
            let mut authorization_url = self.authorization_endpoint.clone();
            authorization_url
                .query_pairs_mut()
                .append_pair("client_id", &self.client_id)
                .append_pair("redirect_uri", redirect_uri.as_str())
                .append_pair("response_type", "code")
                .append_pair("scope", "https://www.googleapis.com/auth/drive.file")
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent")
                .append_pair("state", &pkce.state)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256");
            webbrowser::open(authorization_url.as_str())
                .map_err(|error| GoogleDriveError::Authentication(error.to_string()))?;
            Ok(GoogleOAuthRequest {
                authorization_url,
                state: pkce.state,
                verifier: pkce.verifier,
                redirect_uri,
            })
        })
    }

    fn exchange<'a>(
        &'a self,
        request: &'a GoogleOAuthRequest,
        callback: GoogleOAuthCallback,
    ) -> ServiceFuture<'a, GoogleCredential> {
        Box::pin(async move {
            if !verify_oauth_state(&request.state, &callback.state) {
                return Err(GoogleDriveError::Authentication(
                    "OAuth state mismatch".into(),
                ));
            }
            let mut form = vec![
                ("client_id", self.client_id.clone()),
                ("code", callback.code),
                ("code_verifier", request.verifier.clone()),
                ("redirect_uri", request.redirect_uri.to_string()),
                ("grant_type", "authorization_code".into()),
            ];
            if let Some(secret) = &self.client_secret {
                form.push(("client_secret", secret.clone()));
            }
            token_response(
                self.client
                    .post(self.token_endpoint.clone())
                    .form(&form)
                    .send()
                    .await,
            )
            .await
        })
    }

    fn refresh<'a>(&'a self, refresh_token: &'a str) -> ServiceFuture<'a, GoogleCredential> {
        Box::pin(async move {
            let mut form = vec![
                ("client_id", self.client_id.clone()),
                ("refresh_token", refresh_token.to_owned()),
                ("grant_type", "refresh_token".into()),
            ];
            if let Some(secret) = &self.client_secret {
                form.push(("client_secret", secret.clone()));
            }
            token_response(
                self.client
                    .post(self.token_endpoint.clone())
                    .form(&form)
                    .send()
                    .await,
            )
            .await
        })
    }
}

#[cfg(feature = "desktop")]
async fn token_response(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<GoogleCredential, GoogleDriveError> {
    let response = response.map_err(|error| GoogleDriveError::Authentication(error.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| GoogleDriveError::Authentication(error.to_string()))?;
    parse_google_token_response(status, &body, Utc::now())
}

/// Production selector. `default`/`create_dedicated` uses `files.create`;
/// `new` accepts only an identity returned by a Google Picker grant.
#[cfg(feature = "desktop")]
pub struct DriveApiFolderSelector {
    inner: TransportDriveFolderSelector<ReqwestDriveTransport>,
}

#[cfg(feature = "desktop")]
impl Default for DriveApiFolderSelector {
    fn default() -> Self {
        Self::create_dedicated(DEFAULT_DRIVE_FOLDER_NAME)
    }
}

#[cfg(feature = "desktop")]
impl DriveApiFolderSelector {
    /// Constructs a selector for a folder identity supplied by Google Picker.
    pub fn new(picker_granted_folder_id: impl Into<String>) -> Self {
        Self {
            inner: TransportDriveFolderSelector::new(
                ReqwestDriveTransport::default(),
                DriveFolderSelection::PickerGranted {
                    folder_id: picker_granted_folder_id.into(),
                },
            ),
        }
    }

    pub fn create_dedicated(name: impl Into<String>) -> Self {
        Self {
            inner: TransportDriveFolderSelector::new(
                ReqwestDriveTransport::default(),
                DriveFolderSelection::CreateDedicated { name: name.into() },
            ),
        }
    }
}

#[cfg(feature = "desktop")]
impl GoogleDriveFolderSelector for DriveApiFolderSelector {
    fn select_folder<'a>(
        &'a self,
        access_token: &'a str,
    ) -> ServiceFuture<'a, SelectedDriveFolder> {
        self.inner.select_folder(access_token)
    }
}

#[cfg(not(feature = "desktop"))]
struct EnvironmentGoogleProvisioner;

#[cfg(not(feature = "desktop"))]
impl GoogleDriveProvisioner for EnvironmentGoogleProvisioner {
    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async move {
            let access_token = std::env::var("REC_BAG_GOOGLE_ACCESS_TOKEN").map_err(|_| {
                GoogleDriveError::Authentication(
                    "Interactive Google authorization requires the desktop feature. For server or test injection, set REC_BAG_GOOGLE_ACCESS_TOKEN and REC_BAG_GOOGLE_CLIENT_ID.".into(),
                )
            })?;
            let location = setup.location.as_deref().unwrap_or_default();
            let identity = if location.trim().is_empty() {
                std::env::var("REC_BAG_GOOGLE_DRIVE_FOLDER").map_err(|_| {
                    GoogleDriveError::FolderSelection(
                        "Google Drive backup needs a folder ID or link. Set REC_BAG_GOOGLE_DRIVE_FOLDER for this headless build and retry.".into(),
                    )
                })?
            } else {
                location.to_owned()
            };
            let folder_id = normalize_drive_folder_identity(&identity)?;
            let account =
                std::env::var("REC_BAG_GOOGLE_ACCOUNT").unwrap_or_else(|_| "default".into());
            Ok(ProvisionedDrive {
                credential_account: account.clone(),
                credential: GoogleCredential {
                    access_token,
                    refresh_token: std::env::var("REC_BAG_GOOGLE_REFRESH_TOKEN").ok(),
                    expires_at: None,
                    account_id: Some(account),
                },
                folder: SelectedDriveFolder {
                    id: folder_id,
                    display_name: setup.name.clone().unwrap_or_default(),
                },
            })
        })
    }
}

trait GoogleCallbackSession: Send + Sync {
    fn redirect_uri(&self) -> url::Url;
    fn receive<'a>(
        &'a self,
        request: &'a GoogleOAuthRequest,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveError>> + Send + 'a>>;
}

trait GoogleCallbackSessionFactory: Send + Sync {
    fn prepare(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveError>>
                + Send
                + '_,
        >,
    >;
}

trait DriveFolderSelectorFactory: Send + Sync {
    fn selector(
        &self,
        identity: &str,
    ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveError>;
}

trait DriveFolderIdentityProvider: Send + Sync {
    fn request<'a>(
        &'a self,
        configured: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveError>> + Send + 'a>>;
}

#[derive(Default)]
struct CallbackStates(std::sync::Mutex<BTreeSet<String>>);

impl CallbackStates {
    fn register(&self, state: String) -> Result<(), GoogleDriveError> {
        let mut states = self.0.lock().map_err(|_| callback_state_unavailable())?;
        if !states.insert(state) {
            return Err(GoogleDriveError::Authentication(
                "Google OAuth state was already pending".into(),
            ));
        }
        Ok(())
    }

    fn consume(&self, expected: &str, actual: &str) -> Result<(), GoogleDriveError> {
        let mut states = self.0.lock().map_err(|_| callback_state_unavailable())?;
        if !states.remove(expected) {
            return Err(GoogleDriveError::Authentication(
                "Google OAuth callback was expired or replayed".into(),
            ));
        }
        if !bool::from(expected.as_bytes().ct_eq(actual.as_bytes())) {
            return Err(GoogleDriveError::Authentication(
                "Google OAuth callback state did not match".into(),
            ));
        }
        Ok(())
    }

    fn cancel(&self, state: &str) {
        if let Ok(mut states) = self.0.lock() {
            states.remove(state);
        }
    }
}

fn callback_state_unavailable() -> GoogleDriveError {
    GoogleDriveError::Authentication("Google OAuth state store is unavailable".into())
}

struct InteractiveGoogleProvisioner {
    oauth: Arc<dyn GoogleOAuth>,
    sessions: Arc<dyn GoogleCallbackSessionFactory>,
    folders: Arc<dyn DriveFolderSelectorFactory>,
    identities: Arc<dyn DriveFolderIdentityProvider>,
    states: CallbackStates,
}

#[cfg(feature = "desktop")]
struct ProductionGoogleProvisioner {
    oauth: Arc<dyn GoogleOAuth>,
    sessions: Arc<dyn GoogleCallbackSessionFactory>,
    states: CallbackStates,
}

#[cfg(feature = "desktop")]
impl GoogleDriveProvisioner for ProductionGoogleProvisioner {
    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async move {
            let session = self.sessions.prepare().await?;
            let request = self.oauth.begin(session.redirect_uri()).await?;
            self.states.register(request.state.clone())?;
            let callback = match session.receive(&request).await {
                Ok(callback) => callback,
                Err(error) => {
                    self.states.cancel(&request.state);
                    return Err(error);
                }
            };
            self.states.consume(&request.state, &callback.state)?;
            let credential = self.oauth.exchange(&request, callback).await?;
            let folder = match production_drive_folder_selection(setup) {
                DriveFolderSelection::CreateDedicated { name } => {
                    DriveApiFolderSelector::create_dedicated(name)
                        .select_folder(&credential.access_token)
                        .await?
                }
                DriveFolderSelection::PickerGranted { .. } => {
                    unreachable!("production folder selection never asserts Picker provenance")
                }
            };
            let account = credential
                .account_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            Ok(ProvisionedDrive {
                credential_account: account,
                credential,
                folder,
            })
        })
    }
}

fn production_drive_folder_selection(setup: &StorageSetup) -> DriveFolderSelection {
    let name = setup.name.as_deref().unwrap_or_default().trim();
    DriveFolderSelection::CreateDedicated {
        name: if name.is_empty() {
            "rec_bag Music".into()
        } else {
            name.to_owned()
        },
    }
}

impl GoogleDriveProvisioner for InteractiveGoogleProvisioner {
    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async move {
            let identity = self
                .identities
                .request(setup.location.as_deref().unwrap_or_default())
                .await?;
            let folder_id = normalize_drive_folder_identity(&identity)?;
            let session = self.sessions.prepare().await?;
            let request = self.oauth.begin(session.redirect_uri()).await?;
            self.states.register(request.state.clone())?;
            let callback = match session.receive(&request).await {
                Ok(callback) => callback,
                Err(error) => {
                    self.states.cancel(&request.state);
                    return Err(error);
                }
            };
            self.states.consume(&request.state, &callback.state)?;
            let credential = self.oauth.exchange(&request, callback).await?;
            let folder = self
                .folders
                .selector(&folder_id)?
                .select_folder(&credential.access_token)
                .await?;
            let account = credential
                .account_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            Ok(ProvisionedDrive {
                credential_account: account,
                credential,
                folder,
            })
        })
    }
}

fn normalize_drive_folder_identity(value: &str) -> Result<String, GoogleDriveError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GoogleDriveError::FolderSelection(
            "Paste a Google Drive folder link or stable folder ID in the storage location field"
                .into(),
        ));
    }
    if let Ok(url) = url::Url::parse(value) {
        if !url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("drive.google.com")
                || host.eq_ignore_ascii_case("docs.google.com")
        }) {
            return Err(GoogleDriveError::FolderSelection(
                "The selected folder link must be a Google Drive URL".into(),
            ));
        }
        let path_id = url.path_segments().and_then(|segments| {
            let segments = segments.collect::<Vec<_>>();
            segments
                .windows(2)
                .find(|parts| parts[0] == "folders")
                .map(|parts| parts[1].to_owned())
        });
        let query_id = url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, id)| id.into_owned());
        if let Some(id) = path_id.or(query_id).filter(|id| !id.is_empty()) {
            return Ok(id);
        }
        return Err(GoogleDriveError::FolderSelection(
            "Could not find a stable folder ID in the Google Drive link".into(),
        ));
    }
    if value.chars().any(char::is_whitespace) || value.contains('/') {
        return Err(GoogleDriveError::FolderSelection(
            "The Drive folder ID is malformed; paste the folder link or its stable ID".into(),
        ));
    }
    Ok(value.to_owned())
}

fn parse_google_callback(callback: &url::Url) -> Result<GoogleOAuthCallback, GoogleDriveError> {
    if let Some(error) = callback
        .query_pairs()
        .find(|(key, _)| key == "error")
        .map(|(_, value)| value.into_owned())
    {
        let detail = callback
            .query_pairs()
            .find(|(key, _)| key == "error_description")
            .map(|(_, value)| value.into_owned())
            .unwrap_or(error);
        return Err(GoogleDriveError::Authentication(format!(
            "Google authorization was cancelled or denied: {detail}"
        )));
    }
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GoogleDriveError::Authentication(
                "Google OAuth callback did not include an authorization code".into(),
            )
        })?;
    let state = callback
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GoogleDriveError::Authentication("Google OAuth callback did not include state".into())
        })?;
    Ok(GoogleOAuthCallback { code, state })
}

#[cfg(feature = "desktop")]
struct LoopbackGoogleSessionFactory {
    redirect_uri: url::Url,
}

#[cfg(feature = "desktop")]
impl GoogleCallbackSessionFactory for LoopbackGoogleSessionFactory {
    fn prepare(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let host_is_loopback = self.redirect_uri.scheme() == "http"
                && self.redirect_uri.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
            if !host_is_loopback {
                return Err(GoogleDriveError::Authentication(
                    "REC_BAG_GOOGLE_REDIRECT_URI must be an HTTP loopback URL".into(),
                ));
            }
            let port = self.redirect_uri.port().ok_or_else(|| {
                GoogleDriveError::Authentication(
                    "REC_BAG_GOOGLE_REDIRECT_URI must include a loopback port".into(),
                )
            })?;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .map_err(|error| {
                    GoogleDriveError::Authentication(format!(
                        "Could not listen for the Google OAuth callback on port {port}: {error}"
                    ))
                })?;
            Ok(Box::new(LoopbackGoogleSession {
                redirect_uri: self.redirect_uri.clone(),
                listener: tokio::sync::Mutex::new(Some(listener)),
            }) as Box<dyn GoogleCallbackSession>)
        })
    }
}

#[cfg(feature = "desktop")]
struct LoopbackGoogleSession {
    redirect_uri: url::Url,
    listener: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
}

#[cfg(feature = "desktop")]
impl GoogleCallbackSession for LoopbackGoogleSession {
    fn redirect_uri(&self) -> url::Url {
        self.redirect_uri.clone()
    }

    fn receive<'a>(
        &'a self,
        _request: &'a GoogleOAuthRequest,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveError>> + Send + 'a>>
    {
        Box::pin(async move {
            let listener = self.listener.lock().await.take().ok_or_else(|| {
                GoogleDriveError::Authentication(
                    "Google OAuth callback was already consumed".into(),
                )
            })?;
            // Browsers may open stray connections (favicon requests, idle preconnects); keep
            // accepting until the redirect path arrives or the overall timeout expires.
            let accept_callback = async {
                loop {
                    let (mut stream, _) = listener.accept().await.map_err(|error| {
                        GoogleDriveError::Authentication(format!(
                            "Could not receive the Google OAuth callback: {error}"
                        ))
                    })?;
                    let mut bytes = vec![0_u8; 16 * 1024];
                    let count = match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        tokio::io::AsyncReadExt::read(&mut stream, &mut bytes),
                    )
                    .await
                    {
                        Ok(Ok(count)) if count > 0 => count,
                        _ => continue,
                    };
                    let callback = std::str::from_utf8(&bytes[..count])
                        .ok()
                        .and_then(|request| request.lines().next())
                        .and_then(|line| line.strip_prefix("GET "))
                        .and_then(|line| line.split_once(' ').map(|(target, _)| target))
                        .and_then(|target| self.redirect_uri.join(target).ok());
                    let Some(callback) = callback else {
                        write_google_callback_response(
                            &mut stream,
                            400,
                            "Invalid callback request.",
                        )
                        .await;
                        continue;
                    };
                    if callback.path() != self.redirect_uri.path() {
                        write_google_callback_response(&mut stream, 404, "Not found.").await;
                        continue;
                    }
                    let parsed = parse_google_callback(&callback);
                    let (status, message) = if parsed.is_ok() {
                        (
                            200,
                            "Authorization received. You can close this window and return to rec_bag.",
                        )
                    } else {
                        (400, "Authorization was cancelled or could not be completed. Return to rec_bag for details.")
                    };
                    write_google_callback_response(&mut stream, status, message).await;
                    return parsed;
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(180), accept_callback)
                .await
                .map_err(|_| {
                    GoogleDriveError::Authentication(
                        "Google authorization timed out or was cancelled".into(),
                    )
                })?
        })
    }
}

#[cfg(feature = "desktop")]
async fn write_google_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    );
    let _ = tokio::io::AsyncWriteExt::write_all(stream, response.as_bytes()).await;
}

#[cfg(feature = "desktop")]
struct NativeDriveFolderIdentityProvider;

#[cfg(feature = "desktop")]
impl DriveFolderIdentityProvider for NativeDriveFolderIdentityProvider {
    fn request<'a>(
        &'a self,
        configured: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async move {
            if !configured.trim().is_empty() {
                return Ok(configured.to_owned());
            }
            if let Ok(identity) = std::env::var("REC_BAG_GOOGLE_DRIVE_FOLDER") {
                if !identity.trim().is_empty() {
                    return Ok(identity);
                }
            }
            let selected = tokio::task::spawn_blocking(native_drive_folder_identity_dialog)
                .await
                .map_err(|error| {
                    GoogleDriveError::FolderSelection(format!(
                        "The Google Drive folder prompt failed: {error}"
                    ))
                })??;
            selected.ok_or_else(|| {
                GoogleDriveError::FolderSelection(
                    "Google Drive folder selection was cancelled. Paste or select a Drive folder ID/link and retry Add backup.".into(),
                )
            })
        })
    }
}

#[cfg(all(feature = "desktop", target_os = "windows"))]
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveError> {
    run_drive_identity_prompt(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.Interaction]::InputBox('Paste the Google Drive folder ID or link','Google Drive backup','')",
        ],
    )
}

#[cfg(all(feature = "desktop", target_os = "macos"))]
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveError> {
    run_drive_identity_prompt(
        "osascript",
        &[
            "-e",
            "text returned of (display dialog \"Paste the Google Drive folder ID or link\" default answer \"\" with title \"Google Drive backup\")",
        ],
    )
}

#[cfg(all(
    feature = "desktop",
    not(any(target_os = "windows", target_os = "macos"))
))]
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveError> {
    match run_drive_identity_prompt(
        "zenity",
        &[
            "--entry",
            "--title=Google Drive backup",
            "--text=Paste the Google Drive folder ID or link",
        ],
    ) {
        Err(_) => run_drive_identity_prompt(
            "kdialog",
            &[
                "--inputbox",
                "Paste the Google Drive folder ID or link",
                "",
                "--title",
                "Google Drive backup",
            ],
        )
        .map_err(|_| {
            GoogleDriveError::FolderSelection(
                "No native text prompt is available. Install zenity or kdialog, or set REC_BAG_GOOGLE_DRIVE_FOLDER and retry.".into(),
            )
        }),
        result => result,
    }
}

#[cfg(feature = "desktop")]
fn run_drive_identity_prompt(
    program: &str,
    arguments: &[&str],
) -> Result<Option<String>, GoogleDriveError> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| {
            GoogleDriveError::FolderSelection(format!(
                "Native Google Drive folder prompt {program} could not start: {error}"
            ))
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        GoogleDriveError::FolderSelection(
            "The native Google Drive folder prompt returned invalid text".into(),
        )
    })?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

#[cfg(feature = "desktop")]
struct DriveApiSelectorFactory;

#[cfg(feature = "desktop")]
impl DriveFolderSelectorFactory for DriveApiSelectorFactory {
    fn selector(
        &self,
        identity: &str,
    ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveError> {
        Ok(Arc::new(DriveApiFolderSelector::new(identity)))
    }
}

fn production_google_provisioner(
    oauth: Arc<dyn GoogleOAuth>,
) -> Result<Arc<dyn GoogleDriveProvisioner>, IntegrationError> {
    #[cfg(feature = "desktop")]
    {
        let redirect_uri = std::env::var("REC_BAG_GOOGLE_REDIRECT_URI")
            .unwrap_or_else(|_| "http://127.0.0.1:17842/oauth/google".into());
        let redirect_uri = url::Url::parse(&redirect_uri).map_err(|error| IntegrationError {
            request_id: None,
            slice: Some(crate::state::integration::IntegrationSlice::MusicStorage),
            message: format!("REC_BAG_GOOGLE_REDIRECT_URI is invalid: {error}"),
            retryable: false,
        })?;
        Ok(Arc::new(ProductionGoogleProvisioner {
            oauth,
            sessions: Arc::new(LoopbackGoogleSessionFactory { redirect_uri }),
            states: CallbackStates::default(),
        }))
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = oauth;
        Ok(Arc::new(EnvironmentGoogleProvisioner))
    }
}

struct MissingGoogleOAuth;

impl GoogleOAuth for MissingGoogleOAuth {
    fn begin(
        &self,
        _redirect_uri: url::Url,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleOAuthRequest, GoogleDriveError>> + Send + '_>>
    {
        Box::pin(async { Err(missing_google_client()) })
    }

    fn exchange<'a>(
        &'a self,
        _request: &'a GoogleOAuthRequest,
        _callback: GoogleOAuthCallback,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async { Err(missing_google_client()) })
    }

    fn refresh<'a>(
        &'a self,
        _refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveError>> + Send + 'a>> {
        Box::pin(async { Err(missing_google_client()) })
    }
}

fn missing_google_client() -> GoogleDriveError {
    GoogleDriveError::Authentication(
        "Google OAuth is not configured. Set REC_BAG_GOOGLE_CLIENT_ID and REC_BAG_GOOGLE_CLIENT_SECRET to the Desktop app OAuth client's ID and secret, then restart rec_bag.".into(),
    )
}

fn production_google_oauth() -> Arc<dyn GoogleOAuth> {
    let client_id = std::env::var("REC_BAG_GOOGLE_CLIENT_ID").ok();
    #[cfg(feature = "desktop")]
    if let Some(client_id) = client_id {
        // Google's token endpoint requires the Desktop client's secret even with PKCE; it is not
        // treated as confidential for installed apps.
        let client_secret = std::env::var("REC_BAG_GOOGLE_CLIENT_SECRET")
            .ok()
            .filter(|secret| !secret.trim().is_empty());
        return Arc::new(ReqwestGoogleOAuth::new(client_id, client_secret));
    }
    let _ = client_id;
    Arc::new(MissingGoogleOAuth)
}

/// The Drive driver the desktop runs with: OAuth from `REC_BAG_GOOGLE_CLIENT_ID`
/// and `REC_BAG_GOOGLE_CLIENT_SECRET`, loopback callback capture, and a
/// dedicated My Drive folder per storage.
pub fn production_drive_driver(
    credentials: Arc<dyn CredentialStore>,
) -> Result<DriveDriver, IntegrationError> {
    let oauth = production_google_oauth();
    let provisioner = production_google_provisioner(oauth.clone())?;
    Ok(DriveDriver::new(credentials, oauth, provisioner))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use music_storage::{MusicStorages, Release};
    use rec_persistence::TursoCatalog;
    use ui::{MusicManagementEvent, PrimaryConfigurationView, ProviderChoice};

    use crate::services::integration::IntegrationSliceHandler;
    use crate::services::music_storage_slice::MusicStorageService;
    use crate::state::integration::{IntegrationRequest, IntegrationSlice};

    use rec_core::{
        AssetLocation, AssetRepository, CanonicalTrack, ContentHash, DownloadRequest,
        DownloadResult, HealthStatus, InventoryPage, InventoryRequest, MusicStorageErrorKind,
        MusicStorageRepository, OriginalAsset, OriginalAssetRole, StorageCapabilities,
        StorageHealth, StorageObject, StorageRole, StorageVerificationStatus, StorageVersionPage,
        StorageVersionRequest, TrackRepository, UploadRequest,
    };

    #[derive(Default)]
    struct MockHttpState {
        requests: Mutex<Vec<DriveHttpRequest>>,
        responses: Mutex<VecDeque<DriveHttpResponse>>,
    }

    #[derive(Clone, Default)]
    struct MockHttpTransport(Arc<MockHttpState>);

    impl MockHttpTransport {
        fn with_responses(responses: impl IntoIterator<Item = DriveHttpResponse>) -> Self {
            Self(Arc::new(MockHttpState {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
            }))
        }

        fn requests(&self) -> Vec<DriveHttpRequest> {
            self.0.requests.lock().unwrap().clone()
        }
    }

    impl DriveHttpTransport for MockHttpTransport {
        fn execute(
            &self,
            request: DriveHttpRequest,
        ) -> BoxFuture<'_, Result<DriveHttpResponse, DriveTransportError>> {
            self.0.requests.lock().unwrap().push(request);
            let response = self
                .0
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| DriveTransportError::new("unexpected HTTP request", false));
            Box::pin(async move { response })
        }
    }

    fn http_response(status: u16, body: &str) -> DriveHttpResponse {
        DriveHttpResponse {
            status,
            headers: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[derive(Default)]
    struct MemoryCredentials(Mutex<BTreeMap<CredentialKey, Secret>>);

    impl CredentialStore for MemoryCredentials {
        fn load<'a>(
            &'a self,
            key: &'a CredentialKey,
        ) -> crate::services::integration::CredentialFuture<'a, Option<Secret>> {
            Box::pin(async move { Ok(self.0.lock().unwrap().get(key).cloned()) })
        }
        fn store<'a>(
            &'a self,
            key: &'a CredentialKey,
            secret: Secret,
        ) -> crate::services::integration::CredentialFuture<'a, ()> {
            Box::pin(async move {
                self.0.lock().unwrap().insert(key.clone(), secret);
                Ok(())
            })
        }
        fn delete<'a>(
            &'a self,
            key: &'a CredentialKey,
        ) -> crate::services::integration::CredentialFuture<'a, ()> {
            Box::pin(async move {
                self.0.lock().unwrap().remove(key);
                Ok(())
            })
        }
    }

    struct FakeOAuth {
        refreshed: Mutex<u32>,
    }

    #[derive(Default)]
    struct RecordingRevoker(Mutex<Vec<String>>);

    impl GoogleGrantRevoker for RecordingRevoker {
        fn revoke<'a>(&'a self, token: &'a str) -> ServiceFuture<'a, ()> {
            Box::pin(async move {
                self.0.lock().unwrap().push(token.to_owned());
                Ok(())
            })
        }
    }

    struct OfflineRevoker;

    impl GoogleGrantRevoker for OfflineRevoker {
        fn revoke<'a>(&'a self, _token: &'a str) -> ServiceFuture<'a, ()> {
            Box::pin(async {
                Err(GoogleDriveError::Authentication(
                    "Google is unreachable".into(),
                ))
            })
        }
    }

    impl GoogleOAuth for FakeOAuth {
        fn begin(&self, redirect_uri: Url) -> ServiceFuture<'_, GoogleOAuthRequest> {
            Box::pin(async move {
                Ok(GoogleOAuthRequest {
                    authorization_url: Url::parse("https://example.test/auth").unwrap(),
                    state: "state".into(),
                    verifier: "verifier".into(),
                    redirect_uri,
                })
            })
        }
        fn exchange<'a>(
            &'a self,
            _request: &'a GoogleOAuthRequest,
            _callback: GoogleOAuthCallback,
        ) -> ServiceFuture<'a, GoogleCredential> {
            Box::pin(async { unreachable!() })
        }
        fn refresh<'a>(&'a self, _refresh_token: &'a str) -> ServiceFuture<'a, GoogleCredential> {
            Box::pin(async move {
                *self.refreshed.lock().unwrap() += 1;
                Ok(GoogleCredential {
                    access_token: "fresh".into(),
                    refresh_token: None,
                    expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                    account_id: Some("account".into()),
                })
            })
        }
    }

    struct FakeProvisioner;
    impl GoogleDriveProvisioner for FakeProvisioner {
        fn provision<'a>(&'a self, setup: &'a StorageSetup) -> ServiceFuture<'a, ProvisionedDrive> {
            Box::pin(async move {
                Ok(ProvisionedDrive {
                    credential_account: "account".into(),
                    credential: GoogleCredential {
                        access_token: "token".into(),
                        refresh_token: Some("refresh".into()),
                        expires_at: None,
                        account_id: Some("account".into()),
                    },
                    folder: SelectedDriveFolder {
                        id: setup
                            .location
                            .clone()
                            .unwrap_or_else(|| "selected-backup-folder".into()),
                        display_name: "Music".into(),
                    },
                })
            })
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        inventory: Mutex<Vec<InventoryPage>>,
        objects: Mutex<BTreeMap<String, StorageObject>>,
        bytes: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl FakeRuntime {
        fn object(storage_id: Uuid, object_id: &str, hash: ContentHash) -> StorageObject {
            StorageObject {
                storage_id,
                object_id: object_id.into(),
                name: format!("{object_id}.flac"),
                display_path: Some(format!("Music/{object_id}.flac")),
                size_bytes: 3,
                media_type: Some("audio/flac".into()),
                modified_at: Some(Utc::now()),
                provider_version: Some("1".into()),
                hashes: vec![hash],
                embedded_asset_id: None,
                is_trashed: false,
            }
        }
    }

    impl MusicAssetStore for FakeRuntime {
        fn health(&self) -> BoxFuture<'_, Result<StorageHealth, MusicStorageError>> {
            Box::pin(async {
                Ok(StorageHealth {
                    status: HealthStatus::Healthy,
                    checked_at: Utc::now(),
                    message: None,
                    capabilities: StorageCapabilities {
                        can_inventory: true,
                        can_stat: true,
                        can_upload: true,
                        can_download: true,
                        ..Default::default()
                    },
                })
            })
        }
        fn inventory_page(
            &self,
            _request: InventoryRequest,
        ) -> BoxFuture<'_, Result<InventoryPage, MusicStorageError>> {
            // Once the queued pages are used up, the folder lists as empty.
            Box::pin(async move {
                let mut pages = self.inventory.lock().unwrap();
                Ok(if pages.is_empty() {
                    InventoryPage {
                        objects: Vec::new(),
                        removals: Vec::new(),
                        next_page_cursor: None,
                        next_change_cursor: None,
                        progress: None,
                    }
                } else {
                    pages.remove(0)
                })
            })
        }
        fn stat<'a>(
            &'a self,
            object_id: &'a str,
        ) -> BoxFuture<'a, Result<StorageObject, MusicStorageError>> {
            Box::pin(async move {
                self.objects
                    .lock()
                    .unwrap()
                    .get(object_id)
                    .cloned()
                    .ok_or_else(|| {
                        MusicStorageError::new(MusicStorageErrorKind::NotFound, "missing")
                    })
            })
        }
        fn versions_page(
            &self,
            _request: StorageVersionRequest,
        ) -> BoxFuture<'_, Result<StorageVersionPage, MusicStorageError>> {
            Box::pin(async {
                Ok(StorageVersionPage {
                    versions: vec![],
                    next_page_cursor: None,
                })
            })
        }
        fn upload(
            &self,
            request: UploadRequest,
        ) -> BoxFuture<'_, Result<StorageObject, MusicStorageError>> {
            Box::pin(async move {
                let bytes = std::fs::read(&request.source_path).map_err(|error| {
                    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
                })?;
                let hash = ContentHash::sha256(format!("{:x}", Sha256::digest(&bytes))).unwrap();
                let storage_id = self
                    .objects
                    .lock()
                    .unwrap()
                    .values()
                    .next()
                    .map(|value| value.storage_id)
                    .unwrap_or(Uuid::nil());
                let object = Self::object(
                    storage_id,
                    &format!("uploaded-{}", self.objects.lock().unwrap().len()),
                    hash,
                );
                self.bytes
                    .lock()
                    .unwrap()
                    .insert(object.object_id.clone(), bytes);
                self.objects
                    .lock()
                    .unwrap()
                    .insert(object.object_id.clone(), object.clone());
                Ok(object)
            })
        }
        fn download(
            &self,
            request: DownloadRequest,
        ) -> BoxFuture<'_, Result<DownloadResult, MusicStorageError>> {
            Box::pin(async move {
                let object = self
                    .objects
                    .lock()
                    .unwrap()
                    .get(&request.object_id)
                    .cloned()
                    .unwrap();
                let bytes = self
                    .bytes
                    .lock()
                    .unwrap()
                    .get(&request.object_id)
                    .cloned()
                    .unwrap();
                if let Some(parent) = request.destination_path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&request.destination_path, &bytes).unwrap();
                let checksum =
                    ContentHash::sha256(format!("{:x}", Sha256::digest(&bytes))).unwrap();
                let verified = request.expected_checksum.as_ref() == Some(&checksum);
                Ok(DownloadResult {
                    object,
                    destination_path: request.destination_path,
                    bytes_written: bytes.len() as u64,
                    checksum: Some(checksum),
                    checksum_verified: verified,
                })
            })
        }
    }

    /// Opens a [`FakeRuntime`] seeded with one object and one inventory page
    /// for every storage, recording which storages were opened.
    fn creating_opener(opened: Arc<Mutex<Vec<Uuid>>>) -> Arc<TestStoreOpener> {
        Arc::new(move |storage: &MusicStorage| {
            opened.lock().unwrap().push(storage.id);
            let runtime = Arc::new(FakeRuntime::default());
            runtime.objects.lock().unwrap().insert(
                "factory-seed".into(),
                FakeRuntime::object(storage.id, "factory-seed", hash(b"seed")),
            );
            runtime.inventory.lock().unwrap().push(InventoryPage {
                objects: Vec::new(),
                removals: Vec::new(),
                next_page_cursor: None,
                next_change_cursor: Some(rec_core::SyncCursor::new("factory-cursor")),
                progress: None,
            });
            Ok(runtime as Arc<dyn MusicAssetStore>)
        })
    }

    /// Refuses to open anything: tests register the stores they use.
    fn never_opener() -> Arc<TestStoreOpener> {
        Arc::new(|_: &MusicStorage| Err(StorageError::Driver("not registered".into())))
    }

    /// Records `object` as a verified copy of `original`.
    fn verified_location(original: &OriginalAsset, object: &StorageObject) -> AssetLocation {
        let now = Utc::now();
        AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: rec_core::AssetType::Original,
            storage_id: object.storage_id,
            object_id: object.object_id.clone(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: original.content_hash.clone(),
            observed_hash: object.canonical_hash().cloned(),
            provider_version: object.provider_version.clone(),
            last_checked_at: Some(now),
            verified_at: Some(now),
            error_message: None,
        }
    }

    fn hash(bytes: &[u8]) -> ContentHash {
        ContentHash::sha256(format!("{:x}", Sha256::digest(bytes))).unwrap()
    }

    fn configuration(folder: &str) -> PrimaryConfigurationView {
        PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            name: "Drive primary".into(),
            location: folder.into(),
            organize_by_artist: false,
            verify_after_copy: true,
        }
    }

    /// A Drive slice whose storages are provisioned by [`FakeProvisioner`] and
    /// opened by `opener`; grants are never revoked over the network.
    fn drive_service(
        catalog: Arc<TursoCatalog>,
        credentials: Arc<dyn CredentialStore>,
        opener: Arc<TestStoreOpener>,
        root: &Path,
    ) -> MusicStorageService {
        revoking_drive_service(
            catalog,
            credentials,
            opener,
            root,
            Arc::new(RecordingRevoker::default()),
        )
    }

    /// [`drive_service`] whose driver revokes grants through `revoker`.
    fn revoking_drive_service(
        catalog: Arc<TursoCatalog>,
        credentials: Arc<dyn CredentialStore>,
        opener: Arc<TestStoreOpener>,
        root: &Path,
        revoker: Arc<dyn GoogleGrantRevoker>,
    ) -> MusicStorageService {
        let driver = DriveDriver::new(
            credentials,
            Arc::new(FakeOAuth {
                refreshed: Mutex::new(0),
            }),
            Arc::new(FakeProvisioner),
        )
        .with_grant_revoker(revoker)
        .with_test_opener(opener);
        let storages = MusicStorages::new(
            catalog.clone(),
            music_storage::StorageDrivers::new(vec![Arc::new(driver)]),
            music_storage::MusicStoragesConfig {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
                default_cache_max_bytes: 1024 * 1024,
            },
            Arc::new(music_storage::LiveStores::default()),
            Arc::new(music_storage::NoHandoff),
        );
        MusicStorageService::new(catalog, storages)
    }

    async fn fixture() -> (MusicStorageService, Arc<TursoCatalog>, PathBuf) {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-test-{}", Uuid::new_v4()));
        let service = drive_service(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            never_opener(),
            &root,
        );
        (service, catalog, root)
    }

    #[test]
    fn pkce_and_state_are_valid_and_constant_time_checked() {
        let first = generate_pkce_state();
        let second = generate_pkce_state();
        assert_ne!(first.state, second.state);
        assert!((43..=128).contains(&first.verifier.len()));
        assert!(verify_oauth_state(&first.state, &first.state));
        assert!(!verify_oauth_state(&first.state, "wrong"));
    }

    #[test]
    fn oauth_debug_output_redacts_every_secret_bearing_dto() {
        const ACCESS: &str = "ACCESS_SECRET_SENTINEL";
        const REFRESH: &str = "REFRESH_SECRET_SENTINEL";
        const CODE: &str = "CODE_SECRET_SENTINEL";
        const VERIFIER: &str = "VERIFIER_SECRET_SENTINEL";
        const STATE: &str = "STATE_SECRET_SENTINEL";
        const ACCOUNT: &str = "ACCOUNT_SECRET_SENTINEL";

        let pkce = PkceState {
            state: STATE.into(),
            verifier: VERIFIER.into(),
            challenge: "CHALLENGE_SECRET_SENTINEL".into(),
        };
        let credential = GoogleCredential {
            access_token: ACCESS.into(),
            refresh_token: Some(REFRESH.into()),
            expires_at: None,
            account_id: Some(ACCOUNT.into()),
        };
        let request = GoogleOAuthRequest {
            authorization_url: Url::parse(&format!(
                "https://accounts.example/authorize?state={STATE}&code_challenge={VERIFIER}"
            ))
            .unwrap(),
            state: STATE.into(),
            verifier: VERIFIER.into(),
            redirect_uri: Url::parse("http://127.0.0.1/callback").unwrap(),
        };
        let callback = GoogleOAuthCallback {
            code: CODE.into(),
            state: STATE.into(),
        };
        let provisioned = ProvisionedDrive {
            credential_account: ACCOUNT.into(),
            credential: credential.clone(),
            folder: SelectedDriveFolder {
                id: "folder-id".into(),
                display_name: "rec_bag Music".into(),
            },
        };
        let rendered = format!("{pkce:?} {credential:?} {request:?} {callback:?} {provisioned:?}");
        for secret in [
            ACCESS,
            REFRESH,
            CODE,
            VERIFIER,
            STATE,
            ACCOUNT,
            "CHALLENGE_SECRET_SENTINEL",
        ] {
            assert!(!rendered.contains(secret), "debug output leaked {secret}");
        }
        assert!(rendered.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn folder_selector_creates_dedicated_my_drive_folder_by_default() {
        let transport = MockHttpTransport::with_responses([http_response(
            200,
            r#"{"id":"created-folder","name":"rec_bag Music","mimeType":"application/vnd.google-apps.folder","trashed":false}"#,
        )]);
        let selector =
            TransportDriveFolderSelector::new(transport.clone(), DriveFolderSelection::default())
                .with_api_base_url("https://drive.test/files");
        let folder = selector.select_folder("access-token").await.unwrap();
        assert_eq!(folder.id, "created-folder");
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].headers["Authorization"], "Bearer access-token");
        assert!(requests[0].url.contains("supportsAllDrives=false"));
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["name"], DEFAULT_DRIVE_FOLDER_NAME);
        assert_eq!(body["mimeType"], "application/vnd.google-apps.folder");
    }

    #[tokio::test]
    async fn folder_selector_accepts_only_matching_picker_granted_my_drive_folder() {
        let transport = MockHttpTransport::with_responses([http_response(
            200,
            r#"{"id":"picker_granted-1","name":"DJ Archive","mimeType":"application/vnd.google-apps.folder","trashed":false}"#,
        )]);
        let selector = TransportDriveFolderSelector::new(
            transport.clone(),
            DriveFolderSelection::PickerGranted {
                folder_id: "picker_granted-1".into(),
            },
        )
        .with_api_base_url("https://drive.test/files");
        let folder = selector.select_folder("access-token").await.unwrap();
        assert_eq!(folder.display_name, "DJ Archive");
        let requests = transport.requests();
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].url.contains("/picker_granted-1?"));
        assert!(requests[0].body.is_empty());
    }

    #[test]
    fn token_response_fails_closed_without_scope_or_bearer_type() {
        let missing_scope = parse_google_token_response(
            200,
            br#"{"access_token":"token","token_type":"Bearer","scope":"openid email"}"#,
            Utc::now(),
        )
        .unwrap_err();
        assert!(missing_scope.to_string().contains("drive.file scope"));

        let missing_type = parse_google_token_response(
            200,
            format!(
                r#"{{"access_token":"token","scope":"{}"}}"#,
                DRIVE_FILE_SCOPE
            )
            .as_bytes(),
            Utc::now(),
        )
        .unwrap_err();
        assert!(missing_type.to_string().contains("Bearer token_type"));

        let valid = parse_google_token_response(
            200,
            format!(
                r#"{{"access_token":"token","token_type":"bearer","scope":"openid {}"}}"#,
                DRIVE_FILE_SCOPE
            )
            .as_bytes(),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(valid.access_token, "token");
    }

    #[test]
    fn token_error_includes_google_error_fields_only() {
        let error = parse_google_token_response(
            400,
            br#"{"error":"invalid_request","error_description":"client_secret is missing.","access_token":"leaked"}"#,
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().ends_with(
            "Google token endpoint returned 400: invalid_request (client_secret is missing.)"
        ));
        assert!(!error.to_string().contains("leaked"));

        let opaque = parse_google_token_response(500, b"<html>oops</html>", Utc::now())
            .unwrap_err()
            .to_string();
        assert!(opaque.ends_with("Google token endpoint returned 500"));
    }

    #[tokio::test]
    async fn revoke_posts_form_body_without_exposing_token_in_url() {
        let transport = MockHttpTransport::with_responses([http_response(200, "")]);
        let revoker =
            GoogleOAuthRevoker::new(transport.clone()).with_endpoint("https://oauth.test/revoke");
        revoker.revoke("refresh token/+secret").await.unwrap();
        let requests = transport.requests();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].url, "https://oauth.test/revoke");
        assert!(!requests[0].url.contains("secret"));
        assert_eq!(
            requests[0].headers["Content-Type"],
            "application/x-www-form-urlencoded"
        );
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert_eq!(body, "token=refresh+token%2F%2Bsecret");
    }

    #[tokio::test]
    async fn expired_token_refreshes_and_preserves_rotating_refresh_token() {
        let credentials = Arc::new(MemoryCredentials::default());
        let key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: "account".into(),
        };
        let stored = GoogleCredential {
            access_token: "expired".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
            account_id: Some("account".into()),
        };
        credentials
            .store(&key, Secret::new(serde_json::to_vec(&stored).unwrap()))
            .await
            .unwrap();
        let oauth = Arc::new(FakeOAuth {
            refreshed: Mutex::new(0),
        });
        let provider =
            DesktopGoogleTokenProvider::new("account", credentials.clone(), oauth.clone());
        assert_eq!(provider.access_token().await.unwrap().0, "fresh");
        assert_eq!(*oauth.refreshed.lock().unwrap(), 1);
        let saved: GoogleCredential =
            serde_json::from_slice(credentials.load(&key).await.unwrap().unwrap().expose())
                .unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("refresh"));
        assert!(!format!("{:?}", credentials.load(&key).await.unwrap()).contains("refresh"));
    }

    #[tokio::test]
    async fn revoke_is_idempotent_and_disables_every_storage_for_the_account() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let credentials = Arc::new(MemoryCredentials::default());
        let revoker = Arc::new(RecordingRevoker::default());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-revoke-{}", Uuid::new_v4()));
        let service = revoking_drive_service(
            catalog.clone(),
            credentials.clone(),
            never_opener(),
            &root,
            revoker.clone(),
        );
        let live = service.storages().live_stores().clone();
        let mut first = MusicStorage::new(
            "First",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        first
            .config
            .insert(CONFIG_CREDENTIAL_ACCOUNT.into(), "shared".into());
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.name = "Second".into();
        let mut other = first.clone();
        other.id = Uuid::new_v4();
        other.name = "Other".into();
        other
            .config
            .insert(CONFIG_CREDENTIAL_ACCOUNT.into(), "other".into());
        for storage in [&first, &second, &other] {
            MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
                .await
                .unwrap();
            live.register(storage.id, Arc::new(FakeRuntime::default()));
        }
        let key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: "shared".into(),
        };
        let credential = GoogleCredential {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: None,
            account_id: Some("shared".into()),
        };
        credentials
            .store(&key, Secret::new(serde_json::to_vec(&credential).unwrap()))
            .await
            .unwrap();

        // Offline: Google cannot be reached, so nothing is revoked, the
        // credential stays and no storage is disconnected.
        let offline = revoking_drive_service(
            catalog.clone(),
            credentials.clone(),
            never_opener(),
            &root,
            Arc::new(OfflineRevoker),
        );
        let error = offline
            .storages()
            .release(first.id, Release::RevokeGrant)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Google is unreachable"),
            "{error}"
        );
        assert!(credentials.load(&key).await.unwrap().is_some());
        for storage in [&first, &second] {
            assert!(
                MusicStorageRepository::get(catalog.as_ref(), storage.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .enabled
            );
        }

        let outcome = service
            .handle(IntegrationRequest {
                id: 4,
                slice: IntegrationSlice::MusicStorage,
                event: MusicManagementEvent::RevokeProviderGrant {
                    provider: ProviderChoice::GoogleDrive,
                    connection_id: first.id.to_string(),
                },
            })
            .await
            .unwrap();
        assert_eq!(
            outcome.message.as_deref(),
            Some("Google access revoked and matching storages disabled")
        );
        service
            .storages()
            .release(first.id, Release::RevokeGrant)
            .await
            .unwrap();

        assert_eq!(&*revoker.0.lock().unwrap(), &["refresh"]);
        assert!(credentials.load(&key).await.unwrap().is_none());
        assert!(
            !MusicStorageRepository::get(catalog.as_ref(), first.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            !MusicStorageRepository::get(catalog.as_ref(), second.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            MusicStorageRepository::get(catalog.as_ref(), other.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(!live.contains(first.id));
        assert!(!live.contains(second.id));
        assert!(live.contains(other.id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn configuration_persists_only_non_secret_identity() {
        let (_service, catalog, root) = fixture().await;
        let storage_id = Uuid::new_v4();
        // Persist manually to test the configuration representation.
        let provisioned = FakeProvisioner
            .provision(&StorageSetup {
                location: Some("folder-id".into()),
                ..StorageSetup::default()
            })
            .await
            .unwrap();
        assert_eq!(provisioned.credential.access_token, "token");
        let mut storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("folder-id");
        storage.id = storage_id;
        storage.config.insert(
            CONFIG_CREDENTIAL_ACCOUNT.into(),
            provisioned.credential_account,
        );
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let json = serde_json::to_string(&storage).unwrap();
        assert!(json.contains("folder-id"));
        assert!(!json.contains("refresh"));
        assert!(!json.contains("token"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_migration_handler_is_final_confirmation_and_snapshots_before_cas() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-confirm-{}", Uuid::new_v4()));
        let service = drive_service(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            creating_opener(Arc::new(Mutex::new(Vec::new()))),
            &root,
        );
        let source = MusicStorage::new(
            "Current Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("current-folder");
        MusicStorageRepository::upsert(catalog.as_ref(), source.clone())
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(catalog.as_ref(), None, Some(source.id))
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "song.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let source_object = FakeRuntime::object(source.id, "source-song", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), source_object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(&original, &source_object),
        )
        .await
        .unwrap();
        let source_runtime = Arc::new(FakeRuntime::default());
        source_runtime
            .objects
            .lock()
            .unwrap()
            .insert("source-song".into(), source_object);
        source_runtime
            .bytes
            .lock()
            .unwrap()
            .insert("source-song".into(), b"abc".to_vec());
        service
            .storages()
            .live_stores()
            .register(source.id, source_runtime);

        let outcome = service
            .handle(IntegrationRequest {
                id: 9,
                slice: IntegrationSlice::MusicStorage,
                event: MusicManagementEvent::PreparePrimaryMigration(
                    ui::PrimaryMigrationPlanView {
                        destination: configuration("destination-folder"),
                        verify_checksums: true,
                        keep_old_as_backup: true,
                    },
                ),
            })
            .await
            .unwrap();
        let active = MusicStorageRepository::get_active_primary(catalog.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(active.id, source.id);
        assert_eq!(
            outcome.view.unwrap().primary_storage.unwrap().id,
            active.id.to_string()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn add_backup_handler_provisions_inventories_and_projects_backup() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-backup-{}", Uuid::new_v4()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let service = drive_service(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            creating_opener(opened.clone()),
            &root,
        );
        let outcome = service
            .handle(IntegrationRequest {
                id: 10,
                slice: IntegrationSlice::MusicStorage,
                event: MusicManagementEvent::AddBackup(ProviderChoice::GoogleDrive),
            })
            .await
            .unwrap();
        let storages = MusicStorageRepository::list(catalog.as_ref())
            .await
            .unwrap();
        assert_eq!(storages.len(), 1);
        assert_eq!(storages[0].role, StorageRole::Backup);
        assert_eq!(opened.lock().unwrap().as_slice(), [storages[0].id]);
        assert_eq!(outcome.view.unwrap().backup_storages.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn copy_to_primary_handler_uploads_local_original_and_projects_result() {
        let (service, catalog, root) = fixture().await;
        let drive = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("drive-folder");
        let local_root = root.join("local");
        std::fs::create_dir_all(&local_root).unwrap();
        std::fs::write(local_root.join("song.flac"), b"abc").unwrap();
        let local = MusicStorage::new(
            "Local",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier(local_root.to_string_lossy());
        MusicStorageRepository::upsert(catalog.as_ref(), drive.clone())
            .await
            .unwrap();
        MusicStorageRepository::upsert(catalog.as_ref(), local.clone())
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(catalog.as_ref(), None, Some(drive.id))
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "song.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let local_object = FakeRuntime::object(local.id, "song.flac", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), local_object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(&original, &local_object),
        )
        .await
        .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.objects.lock().unwrap().insert(
            "seed".into(),
            FakeRuntime::object(drive.id, "seed", hash(b"seed")),
        );
        service
            .storages()
            .live_stores()
            .register(drive.id, runtime.clone());
        // The verified local copy is restored from its open store.
        service.storages().live_stores().register(
            local.id,
            Arc::new(music_storage_local::LocalMusicStorage::new(local.id, &local_root).unwrap()),
        );
        let outcome = service
            .handle(IntegrationRequest {
                id: 11,
                slice: IntegrationSlice::MusicStorage,
                event: MusicManagementEvent::CopyToPrimary(track.id.to_string()),
            })
            .await
            .unwrap();
        assert!(outcome.view.is_some());
        assert!(
            AssetRepository::list_locations_for_asset(catalog.as_ref(), original.id())
                .await
                .unwrap()
                .into_iter()
                .any(|location| location.storage_id == drive.id
                    && location.verification_status == StorageVerificationStatus::Verified)
        );
        assert!(outcome.message.unwrap().starts_with("Copied "));
        assert_eq!(runtime.bytes.lock().unwrap().len(), 1, "one upload");
        // The upload's restart state is gone once its verified location committed.
        assert!(rec_core::SettingsRepository::list(catalog.as_ref())
            .await
            .unwrap()
            .iter()
            .all(|setting| !setting.key.starts_with("storage.")
                && !setting.key.starts_with("google_drive.upload.")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn toggle_pin_handler_resolves_track_and_returns_projection() {
        let (service, catalog, root) = fixture().await;
        let drive = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("folder");
        MusicStorageRepository::upsert(catalog.as_ref(), drive.clone())
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(catalog.as_ref(), None, Some(drive.id))
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "song.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let object = FakeRuntime::object(drive.id, "song", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(catalog.as_ref(), verified_location(&original, &object))
            .await
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .objects
            .lock()
            .unwrap()
            .insert("song".into(), object);
        runtime
            .bytes
            .lock()
            .unwrap()
            .insert("song".into(), b"abc".to_vec());
        service.storages().live_stores().register(drive.id, runtime);
        let outcome = service
            .handle(IntegrationRequest {
                id: 12,
                slice: IntegrationSlice::MusicStorage,
                event: MusicManagementEvent::ToggleOfflinePin(track.id.to_string(), true),
            })
            .await
            .unwrap();
        assert!(outcome.view.is_some());
        assert_eq!(
            outcome.message.as_deref(),
            Some("Track pinned for offline playback")
        );
        // The module filled its offline cache for the primary with the verified bytes.
        let cached: Vec<_> = std::fs::read_dir(root.join("cache").join(drive.id.to_string()))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "cache")
            })
            .collect();
        assert_eq!(cached.len(), 1);
        assert_eq!(std::fs::read(&cached[0]).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn production_drive_selection_never_claims_picker_provenance_for_pasted_ids() {
        let setup = StorageSetup {
            name: Some("Portable rec_bag".into()),
            location: Some("pasted-folder-id-without-picker-provenance".into()),
            ..StorageSetup::default()
        };

        assert_eq!(
            production_drive_folder_selection(&setup),
            DriveFolderSelection::CreateDedicated {
                name: "Portable rec_bag".into(),
            }
        );
    }

    #[test]
    fn google_callback_rejects_mismatched_state() {
        let states = CallbackStates::default();
        states.register("expected".into()).unwrap();
        let error = states.consume("expected", "attacker").unwrap_err();
        assert!(error.to_string().contains("state did not match"));
    }

    #[test]
    fn google_callback_state_is_single_use() {
        let states = CallbackStates::default();
        states.register("one-time".into()).unwrap();
        states.consume("one-time", "one-time").unwrap();
        let error = states.consume("one-time", "one-time").unwrap_err();
        assert!(error.to_string().contains("expired or replayed"));
    }

    #[test]
    fn google_callback_reports_user_cancellation() {
        let states = CallbackStates::default();
        states.register("pending".into()).unwrap();
        let callback = url::Url::parse(
            "http://127.0.0.1:17842/oauth/google?error=access_denied&state=pending",
        )
        .unwrap();
        let error = parse_google_callback(&callback).unwrap_err();
        states.cancel("pending");

        assert!(error.to_string().contains("cancelled or denied"));
        assert!(states
            .consume("pending", "pending")
            .unwrap_err()
            .to_string()
            .contains("expired or replayed"));
    }

    #[cfg(feature = "desktop")]
    #[tokio::test]
    async fn loopback_google_session_ignores_stray_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let redirect_uri =
            url::Url::parse(&format!("http://127.0.0.1:{port}/oauth/google")).unwrap();
        let session = Arc::new(LoopbackGoogleSession {
            redirect_uri: redirect_uri.clone(),
            listener: tokio::sync::Mutex::new(Some(listener)),
        });
        let request = GoogleOAuthRequest {
            authorization_url: url::Url::parse("https://accounts.example/authorize").unwrap(),
            state: "state".into(),
            verifier: "verifier".into(),
            redirect_uri,
        };
        let receiver = tokio::spawn({
            let session = session.clone();
            async move { session.receive(&request).await }
        });

        async fn send(port: u16, raw: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream.write_all(raw.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        }

        // A preconnect that closes without sending anything.
        drop(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap(),
        );
        let favicon = send(port, "GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(favicon.starts_with("HTTP/1.1 404"));
        let callback = send(
            port,
            "GET /oauth/google?code=code-1&state=state HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(callback.starts_with("HTTP/1.1 200"));

        let received = receiver.await.unwrap().unwrap();
        assert_eq!(received.code, "code-1");
        assert_eq!(received.state, "state");
    }

    struct TestDriveIdentityProvider {
        selected: Option<String>,
    }

    impl DriveFolderIdentityProvider for TestDriveIdentityProvider {
        fn request<'a>(
            &'a self,
            configured: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveError>> + Send + 'a>> {
            Box::pin(async move {
                if !configured.trim().is_empty() {
                    return Ok(configured.to_owned());
                }
                self.selected.clone().ok_or_else(|| {
                    GoogleDriveError::FolderSelection(
                        "Google Drive folder selection was cancelled; retry Add backup and choose a folder".into(),
                    )
                })
            })
        }
    }

    struct TestGoogleOAuth;

    impl GoogleOAuth for TestGoogleOAuth {
        fn begin(
            &self,
            redirect_uri: url::Url,
        ) -> Pin<Box<dyn Future<Output = Result<GoogleOAuthRequest, GoogleDriveError>> + Send + '_>>
        {
            Box::pin(async move {
                Ok(GoogleOAuthRequest {
                    authorization_url: url::Url::parse("https://accounts.example/authorize")
                        .unwrap(),
                    state: "state".into(),
                    verifier: "verifier".into(),
                    redirect_uri,
                })
            })
        }

        fn exchange<'a>(
            &'a self,
            _request: &'a GoogleOAuthRequest,
            _callback: GoogleOAuthCallback,
        ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveError>> + Send + 'a>>
        {
            Box::pin(async {
                Ok(GoogleCredential {
                    access_token: "access-token".into(),
                    refresh_token: Some("refresh-token".into()),
                    expires_at: None,
                    account_id: Some("account".into()),
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveError>> + Send + 'a>>
        {
            Box::pin(async { unreachable!("refresh is not used while provisioning") })
        }
    }

    struct TestCallbackSession;

    impl GoogleCallbackSession for TestCallbackSession {
        fn redirect_uri(&self) -> url::Url {
            url::Url::parse("http://127.0.0.1:17842/oauth/google").unwrap()
        }

        fn receive<'a>(
            &'a self,
            request: &'a GoogleOAuthRequest,
        ) -> Pin<Box<dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveError>> + Send + 'a>>
        {
            Box::pin(async move {
                Ok(GoogleOAuthCallback {
                    code: "code".into(),
                    state: request.state.clone(),
                })
            })
        }
    }

    struct TestCallbackSessionFactory;

    impl GoogleCallbackSessionFactory for TestCallbackSessionFactory {
        fn prepare(
            &self,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Ok(Box::new(TestCallbackSession) as Box<dyn GoogleCallbackSession>) })
        }
    }

    struct RecordingFolderSelector {
        selected_id: String,
        call: Arc<Mutex<Option<(String, String)>>>,
    }

    impl GoogleDriveFolderSelector for RecordingFolderSelector {
        fn select_folder<'a>(
            &'a self,
            access_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<SelectedDriveFolder, GoogleDriveError>> + Send + 'a>>
        {
            Box::pin(async move {
                *self.call.lock().unwrap() =
                    Some((self.selected_id.clone(), access_token.to_owned()));
                Ok(SelectedDriveFolder {
                    id: self.selected_id.clone(),
                    display_name: "Validated Music".into(),
                })
            })
        }
    }

    struct RecordingFolderSelectorFactory {
        call: Arc<Mutex<Option<(String, String)>>>,
    }

    impl DriveFolderSelectorFactory for RecordingFolderSelectorFactory {
        fn selector(
            &self,
            identity: &str,
        ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveError> {
            Ok(Arc::new(RecordingFolderSelector {
                selected_id: identity.to_owned(),
                call: self.call.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn selected_drive_folder_is_normalized_and_validated_with_access_token() {
        let call = Arc::new(Mutex::new(None));
        let provisioner = InteractiveGoogleProvisioner {
            oauth: Arc::new(TestGoogleOAuth),
            sessions: Arc::new(TestCallbackSessionFactory),
            folders: Arc::new(RecordingFolderSelectorFactory { call: call.clone() }),
            identities: Arc::new(TestDriveIdentityProvider {
                selected: Some(
                    "https://drive.google.com/drive/u/0/folders/folder-123?usp=sharing".into(),
                ),
            }),
            states: CallbackStates::default(),
        };
        let setup = StorageSetup {
            name: Some("Primary".into()),
            organize_by_artist: true,
            ..StorageSetup::default()
        };

        let provisioned = provisioner.provision(&setup).await.unwrap();

        assert_eq!(
            *call.lock().unwrap(),
            Some(("folder-123".into(), "access-token".into()))
        );
        assert_eq!(provisioned.folder.id, "folder-123");
        assert_eq!(provisioned.folder.display_name, "Validated Music");
        assert_eq!(provisioned.credential_account, "account");
    }

    #[tokio::test]
    async fn empty_drive_backup_location_reports_prompt_cancellation_actionably() {
        let provisioner = InteractiveGoogleProvisioner {
            oauth: Arc::new(TestGoogleOAuth),
            sessions: Arc::new(TestCallbackSessionFactory),
            folders: Arc::new(RecordingFolderSelectorFactory {
                call: Arc::new(Mutex::new(None)),
            }),
            identities: Arc::new(TestDriveIdentityProvider { selected: None }),
            states: CallbackStates::default(),
        };
        let setup = StorageSetup {
            name: Some("Google Drive backup".into()),
            ..StorageSetup::default()
        };

        let error = provisioner.provision(&setup).await.unwrap_err();

        assert!(error.to_string().contains("selection was cancelled"));
        assert!(error.to_string().contains("retry Add backup"));
    }
}
