//! Google Drive primary-storage integration for the desktop application.
//!
//! OAuth credentials remain behind [`CredentialStore`]. Only folder identity,
//! an opaque credential reference, and non-secret preferences enter the catalog.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, RwLock},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
#[cfg(feature = "desktop")]
use music_storage_google_drive::{CacheConfig, GoogleDriveStoreConfig};
use music_storage_google_drive::{
    DriveHttpRequest, DriveHttpTransport, GoogleAccessToken, GoogleDriveFolder,
    GoogleDriveMusicStore, GoogleTokenError, GoogleTokenProvider,
};
#[cfg(any(feature = "desktop", test))]
use music_storage_google_drive::{DriveHttpResponse, DriveTransportError};
use rand::RngCore;
use rec_core::{
    AssetLocation, AssetLocationFilter, AssetRepository, BoxFuture, CatalogSourceKind, ContentHash,
    DownloadRequest, HealthStatus, InventoryGenerationAbort, InventoryGenerationAbortKind,
    InventoryGenerationKind, InventoryGenerationStart, InventoryMode, InventoryRequest,
    InventoryStagingPage, MetadataField, MetadataValue, MusicAssetStore, MusicStorage,
    MusicStorageError, MusicStorageErrorKind, MusicStorageKind, MusicStorageRepository,
    OriginalAsset, OriginalAssetFilter, PlaylistRepository, ReconciliationOutcome,
    ReconciliationRepository, ReconciliationResolutionFilter, RepositoryError, RepositoryErrorKind,
    RepositoryPageRequest, Setting, SettingsRepository, SourceOccurrenceFilter, SourceRepository,
    StorageCapabilities, StorageObject, StorageObjectTombstone, StorageRole,
    StorageVerificationStatus, SyncCheckpoint, SyncError, SyncErrorKind, SyncRun, SyncRunFilter,
    SyncRunKind, SyncRunRepository, SyncStatus, TrackRepository, TrackStorageStatus,
    TransactionManager, TransferCheckpoint, TransferStage, UploadRequest,
};
use rec_persistence::TursoCatalog;
use rec_sync::{
    reconcile, InventoryAvailability, ReconciliationInput, ReconciliationReport, StorageInventory,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use ui::{
    CompletenessView, ExternalChangeView, FailedReconciliationView, HealthTone, MatchCandidateView,
    MusicManagementEvent, MusicManagementView, PlaylistCompletenessView, PrimaryConfigurationView,
    ProviderChoice, SourceView, StorageRoleView, StorageView, TrackStorageStatusView,
    TrackStorageView, UnknownPrimaryFileView,
};
use url::Url;
use uuid::Uuid;

use super::integration::{
    AssetFuture, AssetReference, AssetResolver, CredentialKey, CredentialStore, IntegrationFuture,
    IntegrationSliceHandler, ResolvedAsset, Secret,
};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

const PROVIDER_KEY: &str = "google_drive";
const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const DRIVE_API_FILES: &str = "https://www.googleapis.com/drive/v3/files";
const GOOGLE_REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";
const DEFAULT_DRIVE_FOLDER_NAME: &str = "rec_bag Music";
const CONFIG_CREDENTIAL_ACCOUNT: &str = "credential_account";
const CONFIG_FOLDER_NAME: &str = "folder_display_name";
const CONFIG_CACHE_MAX_BYTES: &str = "cache_max_bytes";
const CONFIG_ORGANIZE_BY_ARTIST: &str = "organize_by_artist";
const CONFIG_VERIFY_AFTER_COPY: &str = "verify_after_copy";
const CONFIG_CATALOG_FORGOTTEN: &str = "catalog_forgotten";
const DEFAULT_CACHE_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const INVENTORY_PAGE_SIZE: u32 = 250;

type ServiceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, GoogleDriveServiceError>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum GoogleDriveServiceError {
    #[error("invalid Google Drive configuration: {0}")]
    InvalidConfiguration(String),
    #[error("Google authentication failed: {0}")]
    Authentication(String),
    #[error("Google Drive folder selection failed: {0}")]
    FolderSelection(String),
    #[error("credential storage failed: {0}")]
    Credential(String),
    #[error("catalog operation failed: {0}")]
    Repository(#[from] RepositoryError),
    #[error("Google Drive storage operation failed: {0}")]
    Storage(#[from] MusicStorageError),
    #[error("sync state transition failed: {0}")]
    SyncTransition(String),
    #[error("configured runtime is unavailable: {0}")]
    Runtime(String),
    #[error("asset verification failed: {0}")]
    Verification(String),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid persisted data: {0}")]
    InvalidData(String),
}

impl GoogleDriveServiceError {
    fn retryable(&self) -> bool {
        match self {
            Self::Storage(error) => error.is_retryable(),
            Self::Repository(error) => error.is_retryable(),
            Self::Io(_) | Self::Runtime(_) => true,
            _ => false,
        }
    }
}

impl From<GoogleDriveServiceError> for IntegrationError {
    fn from(error: GoogleDriveServiceError) -> Self {
        IntegrationError {
            request_id: None,
            slice: Some(IntegrationSlice::GoogleDrive),
            retryable: error.retryable(),
            message: error.to_string(),
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

fn parse_google_token_response(
    status: u16,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<GoogleCredential, GoogleDriveServiceError> {
    if !(200..300).contains(&status) {
        return Err(GoogleDriveServiceError::Authentication(format!(
            "Google token endpoint returned {status}"
        )));
    }
    let value: GoogleTokenEndpointResponse = serde_json::from_slice(body)
        .map_err(|error| GoogleDriveServiceError::Authentication(error.to_string()))?;
    let scopes = value.scope.as_deref().unwrap_or_default();
    if !scopes
        .split_ascii_whitespace()
        .any(|scope| scope == DRIVE_FILE_SCOPE)
    {
        return Err(GoogleDriveServiceError::Authentication(
            "Google token response did not grant the required drive.file scope".into(),
        ));
    }
    if !value
        .token_type
        .as_deref()
        .is_some_and(|token_type| token_type.eq_ignore_ascii_case("bearer"))
    {
        return Err(GoogleDriveServiceError::Authentication(
            "Google token response did not declare Bearer token_type".into(),
        ));
    }
    if value.access_token.trim().is_empty() {
        return Err(GoogleDriveServiceError::Authentication(
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

pub trait GoogleDriveProvisioner: Send + Sync {
    fn provision<'a>(
        &'a self,
        configuration: &'a PrimaryConfigurationView,
    ) -> ServiceFuture<'a, ProvisionedDrive>;
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
    credentials: Arc<dyn CredentialStore>,
    redirect_uri: Url,
    callback: C,
}

impl<O, F, C> OAuthFolderProvisioner<O, F, C>
where
    O: GoogleOAuth,
    F: GoogleDriveFolderSelector,
    C: Fn(&GoogleOAuthRequest) -> ServiceFuture<'_, GoogleOAuthCallback> + Send + Sync,
{
    pub fn new(
        oauth: O,
        folders: F,
        credentials: Arc<dyn CredentialStore>,
        redirect_uri: Url,
        callback: C,
    ) -> Self {
        Self {
            oauth,
            folders,
            credentials,
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
    fn provision<'a>(
        &'a self,
        _configuration: &'a PrimaryConfigurationView,
    ) -> ServiceFuture<'a, ProvisionedDrive> {
        Box::pin(async move {
            let request = self.oauth.begin(self.redirect_uri.clone()).await?;
            let callback = (self.callback)(&request).await?;
            if !verify_oauth_state(&request.state, &callback.state) {
                return Err(GoogleDriveServiceError::Authentication(
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
            let encoded = serde_json::to_vec(&credential)
                .map_err(|error| GoogleDriveServiceError::InvalidData(error.to_string()))?;
            self.credentials
                .store(
                    &CredentialKey {
                        provider: PROVIDER_KEY.into(),
                        account: account.clone(),
                    },
                    Secret::new(encoded),
                )
                .await
                .map_err(|error| GoogleDriveServiceError::Credential(error.to_string()))?;
            Ok(ProvisionedDrive {
                credential_account: account,
                credential,
                folder,
            })
        })
    }
}

fn validate_folder(folder: &SelectedDriveFolder) -> Result<(), GoogleDriveServiceError> {
    if folder.id.trim().is_empty() {
        return Err(GoogleDriveServiceError::FolderSelection(
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
                return Err(GoogleDriveServiceError::Authentication(
                    "Google access token is empty".into(),
                ));
            }
            let request = match &self.selection {
                DriveFolderSelection::CreateDedicated { name } => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(GoogleDriveServiceError::FolderSelection(
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
                .map_err(|error| GoogleDriveServiceError::FolderSelection(error.message))?;
            if !(200..300).contains(&response.status) {
                return Err(GoogleDriveServiceError::FolderSelection(format!(
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
                .map_err(|error| GoogleDriveServiceError::FolderSelection(error.to_string()))?;
            if folder.trashed
                || folder.mime_type != "application/vnd.google-apps.folder"
                || folder.drive_id.is_some()
            {
                return Err(GoogleDriveServiceError::FolderSelection(
                    "selected Drive item is not an available My Drive folder".into(),
                ));
            }
            if let DriveFolderSelection::PickerGranted { folder_id } = &self.selection {
                if &folder.id != folder_id {
                    return Err(GoogleDriveServiceError::FolderSelection(
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

fn validate_picker_folder_id(folder_id: &str) -> Result<(), GoogleDriveServiceError> {
    if folder_id.is_empty()
        || !folder_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(GoogleDriveServiceError::FolderSelection(
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
    pub async fn revoke(&self, token: &str) -> Result<(), GoogleDriveServiceError> {
        if token.trim().is_empty() {
            return Err(GoogleDriveServiceError::Authentication(
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
            .map_err(|error| GoogleDriveServiceError::Authentication(error.message))?;
        if (200..300).contains(&response.status) || response.status == 400 {
            // Google returns 400 when a token is already invalid. Treat that as a
            // successful terminal state so a retried revoke can finish local cleanup.
            Ok(())
        } else {
            Err(GoogleDriveServiceError::Authentication(format!(
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
            Err(GoogleDriveServiceError::Runtime(
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

pub trait DriveRuntime: MusicAssetStore {
    fn cached_path(&self, object_id: &str) -> Result<Option<PathBuf>, MusicStorageError>;
    fn cache<'a>(&'a self, object_id: &'a str)
        -> BoxFuture<'a, Result<PathBuf, MusicStorageError>>;
    fn set_pinned(&self, object_id: &str, pinned: bool) -> Result<(), MusicStorageError>;
    fn evict_cache(&self) -> Result<(), MusicStorageError>;
}

impl<T, P> DriveRuntime for GoogleDriveMusicStore<T, P>
where
    T: DriveHttpTransport + Send + Sync,
    P: GoogleTokenProvider + Send + Sync,
{
    fn cached_path(&self, object_id: &str) -> Result<Option<PathBuf>, MusicStorageError> {
        self.cached_playback_path(object_id)
    }

    fn cache<'a>(
        &'a self,
        object_id: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, MusicStorageError>> {
        Box::pin(async move { self.cache_for_playback(object_id).await })
    }

    fn set_pinned(&self, object_id: &str, pinned: bool) -> Result<(), MusicStorageError> {
        self.set_offline_pinned(object_id, pinned)
    }

    fn evict_cache(&self) -> Result<(), MusicStorageError> {
        self.evict_playback_cache()
    }
}

pub trait DriveRuntimeFactory: Send + Sync {
    fn open<'a>(&'a self, storage: &'a MusicStorage) -> ServiceFuture<'a, Arc<dyn DriveRuntime>>;
}

pub struct DriveRuntimeRegistry {
    factory: Arc<dyn DriveRuntimeFactory>,
    runtimes: RwLock<BTreeMap<Uuid, Arc<dyn DriveRuntime>>>,
}

impl DriveRuntimeRegistry {
    pub fn new(factory: Arc<dyn DriveRuntimeFactory>) -> Self {
        Self {
            factory,
            runtimes: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn register(&self, storage_id: Uuid, runtime: Arc<dyn DriveRuntime>) {
        self.runtimes.write().unwrap().insert(storage_id, runtime);
    }

    pub fn remove(&self, storage_id: Uuid) {
        self.runtimes.write().unwrap().remove(&storage_id);
    }

    pub async fn get_or_open(
        &self,
        storage: &MusicStorage,
    ) -> Result<Arc<dyn DriveRuntime>, GoogleDriveServiceError> {
        if let Some(runtime) = self.runtimes.read().unwrap().get(&storage.id).cloned() {
            return Ok(runtime);
        }
        let runtime = self.factory.open(storage).await?;
        self.runtimes
            .write()
            .unwrap()
            .insert(storage.id, runtime.clone());
        Ok(runtime)
    }
}

#[derive(Clone, Debug)]
pub struct GoogleDrivePaths {
    pub cache_root: PathBuf,
    pub staging_root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryCompletion {
    pub run_id: Uuid,
    pub generation_id: Uuid,
    pub object_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDriveAsset {
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub object_id: String,
    pub local_path: PathBuf,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadCheckpoint {
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub idempotency_key: String,
    pub source_path: PathBuf,
    pub transfer: TransferCheckpoint,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationResult {
    pub source_storage_id: Uuid,
    pub destination_storage_id: Uuid,
    pub copied_assets: u64,
}

pub struct GoogleDriveService {
    catalog: Arc<TursoCatalog>,
    credentials: Arc<dyn CredentialStore>,
    provisioner: Arc<dyn GoogleDriveProvisioner>,
    revoker: Arc<dyn GoogleGrantRevoker>,
    registry: Arc<DriveRuntimeRegistry>,
    paths: GoogleDrivePaths,
}

impl GoogleDriveService {
    pub fn new(
        catalog: Arc<TursoCatalog>,
        credentials: Arc<dyn CredentialStore>,
        provisioner: Arc<dyn GoogleDriveProvisioner>,
        registry: Arc<DriveRuntimeRegistry>,
        paths: GoogleDrivePaths,
    ) -> Self {
        #[cfg(feature = "desktop")]
        let revoker: Arc<dyn GoogleGrantRevoker> =
            Arc::new(GoogleOAuthRevoker::new(ReqwestDriveTransport::default()));
        #[cfg(not(feature = "desktop"))]
        let revoker: Arc<dyn GoogleGrantRevoker> = Arc::new(UnavailableGoogleGrantRevoker);

        Self {
            catalog,
            credentials,
            provisioner,
            revoker,
            registry,
            paths,
        }
    }

    pub fn with_grant_revoker(mut self, revoker: Arc<dyn GoogleGrantRevoker>) -> Self {
        self.revoker = revoker;
        self
    }

    pub async fn configure_primary(
        &self,
        configuration: &PrimaryConfigurationView,
    ) -> Result<MusicStorage, GoogleDriveServiceError> {
        if configuration.provider != ProviderChoice::GoogleDrive {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "configuration is not for Google Drive".into(),
            ));
        }
        if configuration.name.trim().is_empty() {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "storage name cannot be empty".into(),
            ));
        }
        let provisioned = self.provisioner.provision(configuration).await?;
        validate_folder(&provisioned.folder)?;
        let credential_key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: provisioned.credential_account.clone(),
        };
        let credential_bytes = serde_json::to_vec(&provisioned.credential)
            .map_err(|error| GoogleDriveServiceError::InvalidData(error.to_string()))?;
        self.credentials
            .store(&credential_key, Secret::new(credential_bytes))
            .await
            .map_err(|error| GoogleDriveServiceError::Credential(error.to_string()))?;
        let capabilities = StorageCapabilities {
            can_inventory: true,
            can_stat: true,
            can_upload: true,
            can_download: true,
            can_create_containers: false,
            supports_atomic_replace: false,
            supports_change_tracking: true,
            supports_version_history: true,
            provider_hash_algorithms: vec![
                rec_core::HashAlgorithm::Md5,
                rec_core::HashAlgorithm::Sha1,
                rec_core::HashAlgorithm::Sha256,
            ],
        };
        let mut storage = MusicStorage::new(
            configuration.name.trim(),
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            capabilities,
        )
        .with_root_identifier(provisioned.folder.id);
        storage.config.insert(
            CONFIG_CREDENTIAL_ACCOUNT.into(),
            provisioned.credential_account,
        );
        storage
            .config
            .insert(CONFIG_FOLDER_NAME.into(), provisioned.folder.display_name);
        storage.config.insert(
            CONFIG_CACHE_MAX_BYTES.into(),
            DEFAULT_CACHE_MAX_BYTES.to_string(),
        );
        storage.config.insert(
            CONFIG_ORGANIZE_BY_ARTIST.into(),
            configuration.organize_by_artist.to_string(),
        );
        storage.config.insert(
            CONFIG_VERIFY_AFTER_COPY.into(),
            configuration.verify_after_copy.to_string(),
        );
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage.clone()).await?;
        let runtime = self.registry.get_or_open(&storage).await?;
        let health = runtime.health().await?;
        if health.status != HealthStatus::Healthy {
            return Err(GoogleDriveServiceError::Runtime(
                health
                    .message
                    .unwrap_or_else(|| "Google Drive is not healthy".into()),
            ));
        }
        storage.capabilities = health.capabilities;
        storage.updated_at = Utc::now();
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage.clone()).await?;
        Ok(storage)
    }

    pub async fn inventory(
        &self,
        storage_id: Uuid,
        force_full: bool,
    ) -> Result<InventoryCompletion, GoogleDriveServiceError> {
        let storage = self.storage(storage_id).await?;
        let runtime = self.registry.get_or_open(&storage).await?;
        let previous = SyncRunRepository::latest(
            self.catalog.as_ref(),
            SyncRunFilter {
                kind: Some(SyncRunKind::StorageInventory),
                status: Some(SyncStatus::Succeeded),
                storage_id: Some(storage_id),
                ..Default::default()
            },
        )
        .await?;
        let prior_cursor = previous.and_then(|run| run.sync_cursor);
        let mode = if force_full || prior_cursor.is_none() {
            InventoryMode::Full
        } else {
            InventoryMode::Incremental {
                change_cursor: prior_cursor.clone().unwrap(),
            }
        };
        let kind = match mode {
            InventoryMode::Full => InventoryGenerationKind::Full,
            InventoryMode::Incremental { .. } => InventoryGenerationKind::Incremental,
        };
        let mut run = SyncRun::new(SyncRunKind::StorageInventory, None, Some(storage_id));
        run.start()
            .map_err(|error| GoogleDriveServiceError::SyncTransition(error.to_string()))?;
        SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;
        let generation_id = Uuid::new_v4();
        AssetRepository::begin_inventory_generation(
            self.catalog.as_ref(),
            InventoryGenerationStart {
                id: generation_id,
                storage_id,
                run_id: run.id,
                kind,
                started_at: Utc::now(),
            },
        )
        .await?;

        let result = self
            .inventory_pages(
                runtime.as_ref(),
                storage_id,
                generation_id,
                mode,
                prior_cursor,
                &mut run,
            )
            .await;
        match result {
            Ok((count, change_cursor)) => {
                AssetRepository::finalize_inventory_generation(
                    self.catalog.as_ref(),
                    generation_id,
                    Utc::now(),
                )
                .await?;
                let checkpoint = SyncCheckpoint {
                    sync_cursor: change_cursor,
                    page_cursor: None,
                    last_item_id: None,
                    processed_items: count,
                    created_at: Utc::now(),
                    transfer: None,
                };
                run.update_checkpoint(checkpoint);
                run.succeed()
                    .map_err(|error| GoogleDriveServiceError::SyncTransition(error.to_string()))?;
                SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;
                Ok(InventoryCompletion {
                    run_id: run.id,
                    generation_id,
                    object_count: count,
                })
            }
            Err(error) => {
                let abort_kind = match &error {
                    GoogleDriveServiceError::Storage(storage_error)
                        if matches!(
                            storage_error.kind,
                            MusicStorageErrorKind::AuthenticationRequired
                                | MusicStorageErrorKind::RateLimited
                                | MusicStorageErrorKind::TemporarilyUnavailable
                        ) =>
                    {
                        InventoryGenerationAbortKind::Unavailable
                    }
                    _ => InventoryGenerationAbortKind::Failed,
                };
                let _ = AssetRepository::abort_inventory_generation(
                    self.catalog.as_ref(),
                    generation_id,
                    InventoryGenerationAbort {
                        kind: abort_kind,
                        finished_at: Utc::now(),
                        message: Some(error.to_string()),
                    },
                )
                .await;
                let _ = run.fail(sync_error_for(&error));
                let _ = SyncRunRepository::upsert(self.catalog.as_ref(), run).await;
                Err(error)
            }
        }
    }

    async fn inventory_pages(
        &self,
        runtime: &dyn DriveRuntime,
        storage_id: Uuid,
        generation_id: Uuid,
        mode: InventoryMode,
        prior_cursor: Option<rec_core::SyncCursor>,
        run: &mut SyncRun,
    ) -> Result<(u64, Option<rec_core::SyncCursor>), GoogleDriveServiceError> {
        let mut page_cursor = None;
        let mut change_cursor = prior_cursor.clone();
        let mut count = 0_u64;
        loop {
            let page = runtime
                .inventory_page(InventoryRequest {
                    mode: mode.clone(),
                    page_cursor: page_cursor.clone(),
                    page_size: Some(INVENTORY_PAGE_SIZE),
                    prefix: None,
                    include_checksums: true,
                })
                .await?;
            count = count.saturating_add(page.objects.len() as u64);
            let tombstones = page
                .removals
                .into_iter()
                .map(|removed| StorageObjectTombstone {
                    storage_id,
                    object_id: removed.object_id,
                    generation_id,
                    removed_at: removed.removed_at,
                    provider_version: removed.provider_version,
                })
                .collect();
            AssetRepository::stage_inventory_page(
                self.catalog.as_ref(),
                generation_id,
                InventoryStagingPage {
                    objects: page.objects,
                    tombstones,
                },
            )
            .await?;
            if let Some(next) = page.next_change_cursor {
                change_cursor = Some(next);
            }
            page_cursor = page.next_page_cursor;
            let checkpoint = SyncCheckpoint {
                sync_cursor: prior_cursor.clone(),
                page_cursor: page_cursor.clone(),
                last_item_id: None,
                processed_items: count,
                created_at: Utc::now(),
                transfer: None,
            };
            run.update_checkpoint(checkpoint);
            SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;
            if page_cursor.is_none() {
                break;
            }
        }
        Ok((count, change_cursor))
    }

    pub async fn reconcile(&self) -> Result<ReconciliationReport, GoogleDriveServiceError> {
        let active = MusicStorageRepository::get_active_primary(self.catalog.as_ref()).await?;
        let storages = MusicStorageRepository::list(self.catalog.as_ref()).await?;
        let forgotten_storage_ids: std::collections::BTreeSet<_> = storages
            .iter()
            .filter(|storage| {
                storage
                    .config
                    .get(CONFIG_CATALOG_FORGOTTEN)
                    .is_some_and(|value| value == "true")
            })
            .map(|storage| storage.id)
            .collect();
        let tracks = self.list_tracks().await?;
        let sources = self.list_sources().await?;
        let occurrences = self.list_occurrences().await?;
        let originals = self.list_originals().await?;
        let mut locations = self.list_locations().await?;
        locations.retain(|location| !forgotten_storage_ids.contains(&location.storage_id));
        let playlists = self.list_playlists().await?;
        let mut inventories = Vec::new();
        for storage in &storages {
            let objects = if forgotten_storage_ids.contains(&storage.id) {
                Vec::new()
            } else {
                self.list_storage_objects(storage.id).await?
            };
            inventories.push(StorageInventory {
                storage_id: storage.id,
                availability: InventoryAvailability::Available,
                objects,
            });
        }
        let input = ReconciliationInput {
            primary_storage_id: active.as_ref().map(|storage| storage.id),
            sources,
            tracks,
            occurrences,
            originals,
            locations,
            storages,
            inventories,
            playlists,
            external_evidence: Vec::new(),
            resolutions: self.list_resolutions().await?,
            suppressions: self.list_suppressions().await?,
        };
        let report = reconcile(&input);
        let transaction = TransactionManager::begin(self.catalog.as_ref()).await?;
        for candidate in report.candidates.iter().cloned() {
            transaction
                .reconciliation()
                .upsert_candidate(candidate)
                .await?;
        }
        for result in report.results.iter().cloned() {
            transaction.reconciliation().upsert_result(result).await?;
        }
        for matched in &report.verified_matches {
            let object = input
                .inventories
                .iter()
                .find(|inventory| inventory.storage_id == matched.storage_id)
                .and_then(|inventory| {
                    inventory
                        .objects
                        .iter()
                        .find(|object| object.object_id == matched.storage_object_id)
                });
            let original = input
                .originals
                .iter()
                .find(|asset| asset.id() == matched.asset_id);
            if let (Some(object), Some(original)) = (object, original) {
                let existing = transaction
                    .assets()
                    .get_location(matched.storage_id, matched.storage_object_id.clone())
                    .await?;
                transaction
                    .assets()
                    .upsert_location(verified_location(existing, original, object))
                    .await?;
            }
        }
        transaction.commit().await?;
        Ok(report)
    }

    pub async fn upload_original(
        &self,
        storage_id: Uuid,
        asset_id: Uuid,
        source_path: PathBuf,
        destination_key: String,
        idempotency_key: String,
    ) -> Result<AssetLocation, GoogleDriveServiceError> {
        let storage = self.storage(storage_id).await?;
        let active = MusicStorageRepository::get_active_primary(self.catalog.as_ref()).await?;
        if active.as_ref().map(|value| value.id) != Some(storage_id) {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "uploads require the active primary storage".into(),
            ));
        }
        let original = AssetRepository::get_original(self.catalog.as_ref(), asset_id)
            .await?
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidData("original asset not found".into())
            })?;
        let expected = original.content_hash.clone().ok_or_else(|| {
            GoogleDriveServiceError::Verification("original has no canonical checksum".into())
        })?;
        if !expected.is_canonical_sha256() {
            return Err(GoogleDriveServiceError::Verification(
                "original checksum is not canonical SHA-256".into(),
            ));
        }
        let runtime = self.registry.get_or_open(&storage).await?;
        let setting_key = upload_setting_key(storage_id, asset_id, &idempotency_key);
        self.persist_upload_checkpoint(
            &setting_key,
            UploadCheckpoint {
                asset_id,
                storage_id,
                idempotency_key: idempotency_key.clone(),
                source_path: source_path.clone(),
                transfer: TransferCheckpoint::new(expected.clone(), None),
                updated_at: Utc::now(),
            },
        )
        .await?;
        let mut metadata = BTreeMap::new();
        metadata.insert("asset_id".into(), asset_id.to_string());
        metadata.insert("track_id".into(), original.track_id().to_string());
        let uploaded = runtime
            .upload(UploadRequest {
                content_length: Some(std::fs::metadata(&source_path)?.len()),
                source_path,
                object_path: destination_key,
                content_type: original.media_type.clone(),
                expected_checksum: Some(expected.clone()),
                metadata,
                idempotency_key: Some(idempotency_key.clone()),
            })
            .await?;
        self.persist_upload_checkpoint(
            &setting_key,
            UploadCheckpoint {
                asset_id,
                storage_id,
                idempotency_key,
                source_path: PathBuf::new(),
                transfer: TransferCheckpoint::new(expected.clone(), None)
                    .advance(
                        TransferStage::Uploaded,
                        &expected,
                        None,
                        Some(uploaded.object_id.clone()),
                    )
                    .map_err(|error| GoogleDriveServiceError::InvalidData(error.to_string()))?,
                updated_at: Utc::now(),
            },
        )
        .await?;
        let stated = runtime.stat(&uploaded.object_id).await?;
        self.verify_object(runtime.as_ref(), &stated, &expected)
            .await?;
        AssetRepository::upsert_storage_object(self.catalog.as_ref(), stated.clone()).await?;
        let existing = AssetRepository::get_location(
            self.catalog.as_ref(),
            storage_id,
            stated.object_id.clone(),
        )
        .await?;
        let location = verified_location(existing, &original, &stated);
        AssetRepository::upsert_location(self.catalog.as_ref(), location.clone()).await?;
        // A verified location is the durable completion record. The settings row is
        // only restart state and must not accumulate after successful completion.
        self.catalog.delete_setting(&setting_key).await?;
        Ok(location)
    }

    async fn verify_object(
        &self,
        runtime: &dyn DriveRuntime,
        object: &StorageObject,
        expected: &ContentHash,
    ) -> Result<(), GoogleDriveServiceError> {
        if object.canonical_hash() == Some(expected) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.paths.staging_root)?;
        let path = self
            .paths
            .staging_root
            .join(format!("verify-{}", Uuid::new_v4()));
        let download = runtime
            .download(DownloadRequest {
                object_id: object.object_id.clone(),
                destination_path: path.clone(),
                range: None,
                expected_checksum: Some(expected.clone()),
                overwrite: true,
            })
            .await?;
        let _ = std::fs::remove_file(path);
        if download.checksum_verified && download.checksum.as_ref() == Some(expected) {
            Ok(())
        } else {
            Err(GoogleDriveServiceError::Verification(
                "destination bytes did not match the original checksum".into(),
            ))
        }
    }

    async fn persist_upload_checkpoint(
        &self,
        key: &str,
        checkpoint: UploadCheckpoint,
    ) -> Result<(), GoogleDriveServiceError> {
        SettingsRepository::set(
            self.catalog.as_ref(),
            Setting {
                key: key.into(),
                value: serde_json::to_value(checkpoint)
                    .map_err(|error| GoogleDriveServiceError::InvalidData(error.to_string()))?,
                updated_at: Utc::now(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn resolve(
        &self,
        storage_id: Uuid,
        asset_id: Uuid,
    ) -> Result<ResolvedDriveAsset, GoogleDriveServiceError> {
        let storage = self.storage(storage_id).await?;
        let location = AssetRepository::list_locations_for_asset(self.catalog.as_ref(), asset_id)
            .await?
            .into_iter()
            .find(|location| {
                location.storage_id == storage_id
                    && location.verification_status == StorageVerificationStatus::Verified
            })
            .ok_or_else(|| {
                GoogleDriveServiceError::Verification(
                    "asset has no verified location in this storage".into(),
                )
            })?;
        let runtime = self.registry.get_or_open(&storage).await?;
        if let Some(path) = runtime.cached_path(&location.object_id)? {
            return Ok(ResolvedDriveAsset {
                asset_id,
                storage_id,
                object_id: location.object_id,
                local_path: path,
                cache_hit: true,
            });
        }
        let path = runtime.cache(&location.object_id).await?;
        Ok(ResolvedDriveAsset {
            asset_id,
            storage_id,
            object_id: location.object_id,
            local_path: path,
            cache_hit: false,
        })
    }

    pub async fn set_offline_pinned(
        &self,
        storage_id: Uuid,
        asset_id: Uuid,
        pinned: bool,
    ) -> Result<(), GoogleDriveServiceError> {
        let resolved = self.resolve(storage_id, asset_id).await?;
        let storage = self.storage(storage_id).await?;
        self.registry
            .get_or_open(&storage)
            .await?
            .set_pinned(&resolved.object_id, pinned)?;
        Ok(())
    }

    pub async fn migrate_primary(
        &self,
        source_storage_id: Uuid,
        destination_storage_id: Uuid,
        verify_checksums: bool,
        keep_old_as_backup: bool,
    ) -> Result<MigrationResult, GoogleDriveServiceError> {
        let active = MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidConfiguration("no active primary exists".into())
            })?;
        if active.id != source_storage_id {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "active primary changed before migration".into(),
            ));
        }
        let destination = self.storage(destination_storage_id).await?;
        if destination.role != StorageRole::Unassigned {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "migration destination must be unassigned".into(),
            ));
        }
        let source_runtime = self.registry.get_or_open(&active).await?;
        let destination_runtime = self.registry.get_or_open(&destination).await?;
        let originals = self.list_originals().await?;
        std::fs::create_dir_all(&self.paths.staging_root)?;
        let mut copied = 0_u64;
        for original in originals {
            let Some(expected) = original.content_hash.clone() else {
                return Err(GoogleDriveServiceError::Verification(format!(
                    "asset {} has no checksum",
                    original.id()
                )));
            };
            let locations =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), original.id())
                    .await?;
            if locations.iter().any(|location| {
                location.storage_id == destination_storage_id
                    && location.verification_status == StorageVerificationStatus::Verified
                    && location.expected_hash.as_ref() == Some(&expected)
                    && location.observed_hash.as_ref() == Some(&expected)
            }) {
                continue;
            }
            let source = locations
                .iter()
                .find(|location| {
                    location.storage_id == source_storage_id
                        && location.verification_status == StorageVerificationStatus::Verified
                })
                .ok_or_else(|| {
                    GoogleDriveServiceError::Verification(format!(
                        "asset {} is not verified in the current primary",
                        original.id()
                    ))
                })?;
            let staging = self
                .paths
                .staging_root
                .join(format!("migration-{}", original.id()));
            let download = source_runtime
                .download(DownloadRequest {
                    object_id: source.object_id.clone(),
                    destination_path: staging.clone(),
                    range: None,
                    expected_checksum: Some(expected.clone()),
                    overwrite: true,
                })
                .await?;
            if verify_checksums
                && (!download.checksum_verified || download.checksum.as_ref() != Some(&expected))
            {
                return Err(GoogleDriveServiceError::Verification(format!(
                    "migration download for asset {} was not verified",
                    original.id()
                )));
            }
            let mut metadata = BTreeMap::new();
            metadata.insert("asset_id".into(), original.id().to_string());
            metadata.insert("track_id".into(), original.track_id().to_string());
            let uploaded = destination_runtime
                .upload(UploadRequest {
                    source_path: staging.clone(),
                    object_path: original.file_name.clone(),
                    content_type: original.media_type.clone(),
                    content_length: Some(download.bytes_written),
                    expected_checksum: Some(expected.clone()),
                    metadata,
                    idempotency_key: Some(format!(
                        "primary-migration:{source_storage_id}:{destination_storage_id}:{}",
                        original.id()
                    )),
                })
                .await?;
            let stated = destination_runtime.stat(&uploaded.object_id).await?;
            self.verify_object(destination_runtime.as_ref(), &stated, &expected)
                .await?;
            AssetRepository::upsert_storage_object(self.catalog.as_ref(), stated.clone()).await?;
            let existing = AssetRepository::get_location(
                self.catalog.as_ref(),
                destination_storage_id,
                stated.object_id.clone(),
            )
            .await?;
            AssetRepository::upsert_location(
                self.catalog.as_ref(),
                verified_location(existing, &original, &stated),
            )
            .await?;
            let _ = std::fs::remove_file(staging);
            copied = copied.saturating_add(1);
        }
        self.assert_destination_complete(destination_storage_id, &self.list_originals().await?)
            .await?;
        // The exported snapshot is persisted before the compare-and-swap so the
        // catalog can be restored to the exact pre-migration authority assignment.
        self.catalog.export_snapshot().await?;
        MusicStorageRepository::switch_active_primary(
            self.catalog.as_ref(),
            Some(source_storage_id),
            Some(destination_storage_id),
        )
        .await?;
        if keep_old_as_backup {
            let mut old = self.storage(source_storage_id).await?;
            old.role = StorageRole::Backup;
            old.updated_at = Utc::now();
            MusicStorageRepository::upsert(self.catalog.as_ref(), old).await?;
        }
        Ok(MigrationResult {
            source_storage_id,
            destination_storage_id,
            copied_assets: copied,
        })
    }

    async fn assert_destination_complete(
        &self,
        destination: Uuid,
        originals: &[OriginalAsset],
    ) -> Result<(), GoogleDriveServiceError> {
        for original in originals {
            let expected = original.content_hash.as_ref().ok_or_else(|| {
                GoogleDriveServiceError::Verification(format!(
                    "asset {} has no checksum",
                    original.id()
                ))
            })?;
            let verified =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), original.id())
                    .await?
                    .into_iter()
                    .any(|location| {
                        location.storage_id == destination
                            && location.verification_status == StorageVerificationStatus::Verified
                            && location.expected_hash.as_ref() == Some(expected)
                            && location.observed_hash.as_ref() == Some(expected)
                    });
            if !verified {
                return Err(GoogleDriveServiceError::Verification(format!(
                    "destination is missing verified asset {}",
                    original.id()
                )));
            }
        }
        Ok(())
    }

    pub async fn configure_backup(&self) -> Result<MusicStorage, GoogleDriveServiceError> {
        let configuration = PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            name: "Google Drive backup".into(),
            location: String::new(),
            organize_by_artist: false,
            verify_after_copy: true,
        };
        let mut storage = self.configure_primary(&configuration).await?;
        storage.role = StorageRole::Backup;
        storage.updated_at = Utc::now();
        MusicStorageRepository::upsert(self.catalog.as_ref(), storage.clone()).await?;
        self.inventory(storage.id, true).await?;
        Ok(storage)
    }

    pub async fn copy_track_to_active_primary(
        &self,
        track_id: Uuid,
    ) -> Result<AssetLocation, GoogleDriveServiceError> {
        let primary = MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidConfiguration("no active primary exists".into())
            })?;
        if primary.kind != MusicStorageKind::GoogleDrive {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "active primary is not Google Drive".into(),
            ));
        }
        let mut originals =
            AssetRepository::list_originals_for_track(self.catalog.as_ref(), track_id).await?;
        originals.sort_by_key(|asset| (!asset.is_preferred, asset.record.created_at));
        for original in originals {
            if let Some(existing) =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), original.id())
                    .await?
                    .into_iter()
                    .find(|location| {
                        location.storage_id == primary.id
                            && location.verification_status == StorageVerificationStatus::Verified
                    })
            {
                return Ok(existing);
            }
            if original.content_hash.is_none() {
                continue;
            }
            let locations =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), original.id())
                    .await?;
            for location in locations {
                if location.verification_status != StorageVerificationStatus::Verified {
                    continue;
                }
                let Some(source_storage) =
                    MusicStorageRepository::get(self.catalog.as_ref(), location.storage_id).await?
                else {
                    continue;
                };
                if source_storage.kind != MusicStorageKind::LocalDirectory
                    || !source_storage.enabled
                {
                    continue;
                }
                let object_path = PathBuf::from(&location.object_id);
                let source_path = if object_path.is_absolute() {
                    object_path
                } else if let Some(root) = source_storage.root_identifier.as_ref() {
                    PathBuf::from(root).join(object_path)
                } else {
                    continue;
                };
                if !source_path.is_file() {
                    continue;
                }
                let key = format!(
                    "copy-to-primary:{}:{}:{}",
                    primary.id,
                    original.id(),
                    original
                        .content_hash
                        .as_ref()
                        .map(ContentHash::value)
                        .unwrap_or("")
                );
                return self
                    .upload_original(
                        primary.id,
                        original.id(),
                        source_path,
                        original.file_name.clone(),
                        key,
                    )
                    .await;
            }
        }
        Err(GoogleDriveServiceError::Verification(
            "track has no eligible verified local original".into(),
        ))
    }

    pub async fn set_track_offline_pinned(
        &self,
        track_id: Uuid,
        pinned: bool,
    ) -> Result<(), GoogleDriveServiceError> {
        let primary = MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidConfiguration("no active primary exists".into())
            })?;
        if primary.kind != MusicStorageKind::GoogleDrive {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "active primary is not Google Drive".into(),
            ));
        }
        let mut originals =
            AssetRepository::list_originals_for_track(self.catalog.as_ref(), track_id).await?;
        originals.sort_by_key(|asset| (!asset.is_preferred, asset.record.created_at));
        for original in originals {
            let verified =
                AssetRepository::list_locations_for_asset(self.catalog.as_ref(), original.id())
                    .await?
                    .into_iter()
                    .any(|location| {
                        location.storage_id == primary.id
                            && location.verification_status == StorageVerificationStatus::Verified
                    });
            if verified {
                return self
                    .set_offline_pinned(primary.id, original.id(), pinned)
                    .await;
            }
        }
        Err(GoogleDriveServiceError::Verification(
            "track has no verified original in the active Drive primary".into(),
        ))
    }

    /// Removes cached bytes for one storage and drops its live runtime.
    pub fn purge_storage_cache(&self, storage_id: Uuid) -> Result<(), GoogleDriveServiceError> {
        self.registry.remove(storage_id);
        remove_cache_tree(&self.paths.cache_root.join(storage_id.to_string()))
    }

    /// Removes every Drive playback cache while preserving the configured root.
    pub fn purge_all_caches(&self) -> Result<(), GoogleDriveServiceError> {
        let storage_ids: Vec<_> = self
            .registry
            .runtimes
            .read()
            .map_err(|_| {
                GoogleDriveServiceError::Runtime("runtime registry lock is poisoned".into())
            })?
            .keys()
            .copied()
            .collect();
        for storage_id in storage_ids {
            self.registry.remove(storage_id);
        }
        remove_cache_tree(&self.paths.cache_root)?;
        std::fs::create_dir_all(&self.paths.cache_root)?;
        Ok(())
    }

    /// Disables one storage and its runtime while retaining credentials, cache,
    /// inventory, asset locations, and every other catalog record.
    pub async fn disconnect_storage(
        &self,
        storage_id: Uuid,
    ) -> Result<(), GoogleDriveServiceError> {
        let mut storage = self.storage(storage_id).await?;
        self.registry.remove(storage_id);
        if storage.enabled {
            storage.enabled = false;
            storage.updated_at = Utc::now();
            MusicStorageRepository::upsert(self.catalog.as_ref(), storage).await?;
        }
        if MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .is_some_and(|active| active.id == storage_id)
        {
            MusicStorageRepository::switch_active_primary(
                self.catalog.as_ref(),
                Some(storage_id),
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Revokes the account grant, removes its credential, invalidates all runtimes
    /// that could retain a token, and disables every Drive storage for that account.
    pub async fn revoke_provider_grant(
        &self,
        connection_id: Uuid,
    ) -> Result<(), GoogleDriveServiceError> {
        let connection = self.storage(connection_id).await?;
        let account = connection
            .config
            .get(CONFIG_CREDENTIAL_ACCOUNT)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidConfiguration(
                    "credential reference is missing".into(),
                )
            })?;
        let key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: account.clone(),
        };
        if let Some(secret) = self
            .credentials
            .load(&key)
            .await
            .map_err(|error| GoogleDriveServiceError::Credential(error.to_string()))?
        {
            let credential: GoogleCredential = serde_json::from_slice(secret.expose())
                .map_err(|error| GoogleDriveServiceError::InvalidData(error.to_string()))?;
            let token = credential
                .refresh_token
                .as_deref()
                .unwrap_or(&credential.access_token);
            self.revoker.revoke(token).await?;
        }
        self.credentials
            .delete(&key)
            .await
            .map_err(|error| GoogleDriveServiceError::Credential(error.to_string()))?;

        let matching: Vec<_> = MusicStorageRepository::list(self.catalog.as_ref())
            .await?
            .into_iter()
            .filter(|storage| {
                storage.kind == MusicStorageKind::GoogleDrive
                    && storage.config.get(CONFIG_CREDENTIAL_ACCOUNT) == Some(&account)
            })
            .collect();
        let matching_ids: std::collections::BTreeSet<_> =
            matching.iter().map(|storage| storage.id).collect();
        let active_id = MusicStorageRepository::get_active_primary(self.catalog.as_ref())
            .await?
            .map(|storage| storage.id);
        for mut storage in matching {
            self.registry.remove(storage.id);
            if storage.enabled {
                storage.enabled = false;
                storage.updated_at = Utc::now();
                MusicStorageRepository::upsert(self.catalog.as_ref(), storage).await?;
            }
        }
        if active_id.is_some_and(|id| matching_ids.contains(&id)) {
            MusicStorageRepository::switch_active_primary(self.catalog.as_ref(), active_id, None)
                .await?;
        }
        Ok(())
    }

    /// Clears only local Drive playback bytes. Catalog records and remote files are
    /// not modified, and repeating the operation is harmless.
    pub async fn clear_local_cache(&self, storage_id: Uuid) -> Result<(), GoogleDriveServiceError> {
        self.storage(storage_id).await?;
        self.purge_storage_cache(storage_id)
    }

    /// Disconnects one storage, purges its local cache/runtime, and atomically deletes
    /// its provider-owned persistence records. Credentials, canonical tracks, owned
    /// assets, and remote Drive files are retained.
    pub async fn forget_provider_catalog_data(
        &self,
        storage_id: Uuid,
    ) -> Result<(), GoogleDriveServiceError> {
        if MusicStorageRepository::get(self.catalog.as_ref(), storage_id)
            .await?
            .is_some()
        {
            self.disconnect_storage(storage_id).await?;
        }
        self.purge_storage_cache(storage_id)?;
        self.catalog
            .delete_storage_provider_data(storage_id)
            .await?;
        Ok(())
    }

    pub async fn retry_storage(&self, storage_id: Uuid) -> Result<(), GoogleDriveServiceError> {
        let storage = self.storage(storage_id).await?;
        let runtime = self.registry.get_or_open(&storage).await?;
        let health = runtime.health().await?;
        if health.status == HealthStatus::AuthenticationRequired {
            return Err(GoogleDriveServiceError::Authentication(
                health
                    .message
                    .unwrap_or_else(|| "Google authorization must be renewed".into()),
            ));
        }
        self.inventory(storage_id, false).await?;
        self.reconcile().await?;
        Ok(())
    }

    pub async fn project_view(&self) -> Result<MusicManagementView, GoogleDriveServiceError> {
        let report = self.reconcile().await?;
        let sources = self.list_sources().await?;
        let storages = MusicStorageRepository::list(self.catalog.as_ref()).await?;
        let active = MusicStorageRepository::get_active_primary(self.catalog.as_ref()).await?;
        let tracks = self.list_tracks().await?;
        let playlists = self.list_playlists().await?;
        let track_by_id: BTreeMap<_, _> = tracks.iter().map(|track| (track.id, track)).collect();
        let mut object_by_key = BTreeMap::new();
        for storage in &storages {
            for object in self.list_storage_objects(storage.id).await? {
                object_by_key.insert((storage.id, object.object_id.clone()), object);
            }
        }
        let source_views = sources
            .into_iter()
            .map(|source| SourceView {
                id: source.id.to_string(),
                provider: match source.kind {
                    CatalogSourceKind::SoundCloud => ProviderChoice::SoundCloud,
                    _ => ProviderChoice::LocalFolder,
                },
                name: source.name,
                account_identity: None,
                detail: source.config.values().next().cloned().unwrap_or_default(),
                health: if source.enabled {
                    HealthTone::Healthy
                } else {
                    HealthTone::Offline
                },
                last_sync: source.last_synced_at.map(format_time),
                next_sync: None,
                progress: None,
                warning: None,
                error: None,
                failed_sync_run_id: None,
                collections: Vec::new(),
                connected: source.enabled,
            })
            .collect();
        let mut primary_storage = None;
        let mut backup_storages = Vec::new();
        let mut unassigned_storages = Vec::new();
        for storage in &storages {
            let view = self.storage_view(storage).await;
            match storage.role {
                StorageRole::Primary
                    if active.as_ref().is_some_and(|value| value.id == storage.id) =>
                {
                    primary_storage = Some(view)
                }
                StorageRole::Backup => backup_storages.push(view),
                _ => unassigned_storages.push(view),
            }
        }
        let status_by_track: BTreeMap<_, _> = report
            .track_outcomes
            .iter()
            .map(|outcome| (outcome.track_id, outcome.status))
            .collect();
        let track_views = report
            .track_outcomes
            .iter()
            .map(|outcome| {
                let track = track_by_id.get(&outcome.track_id);
                let status = status_view(outcome.status);
                TrackStorageView {
                    id: outcome.track_id.to_string(),
                    title: track
                        .and_then(|track| preferred_text(track, MetadataField::Title))
                        .unwrap_or_else(|| "Unknown track".into()),
                    artist: track
                        .and_then(|track| preferred_text(track, MetadataField::Artist))
                        .unwrap_or_else(|| "Unknown artist".into()),
                    status,
                    badges: vec![format!("{status:?}")],
                    pinned_offline: false,
                }
            })
            .collect();
        let possible_matches = report
            .candidates
            .iter()
            .map(|candidate| {
                let object =
                    object_by_key.get(&(candidate.storage_id, candidate.storage_object_id.clone()));
                MatchCandidateView {
                    candidate_id: candidate.id.to_string(),
                    track_id: candidate.track_id.to_string(),
                    track_name: track_by_id
                        .get(&candidate.track_id)
                        .and_then(|track| preferred_text(track, MetadataField::Title))
                        .unwrap_or_else(|| "Unknown track".into()),
                    candidate_name: object
                        .map(|object| object.name.clone())
                        .unwrap_or_else(|| candidate.storage_object_id.clone()),
                    evidence: format!("{:?}", candidate.evidence),
                }
            })
            .collect();
        let unknown_primary_files = report
            .results
            .iter()
            .filter(|result| result.outcome == ReconciliationOutcome::UntrackedStorageObject)
            .map(|result| {
                let object_id = result.storage_object_id.clone().unwrap_or_default();
                let object = result
                    .storage_id
                    .and_then(|storage_id| object_by_key.get(&(storage_id, object_id.clone())));
                UnknownPrimaryFileView {
                    id: result.id.to_string(),
                    filename: object
                        .map(|object| object.name.clone())
                        .unwrap_or_else(|| object_id.clone()),
                    location: object
                        .and_then(|object| object.display_path.clone())
                        .unwrap_or(object_id),
                    discovered_at: "Current inventory".into(),
                }
            })
            .collect();
        let external_changes = report
            .results
            .iter()
            .filter(|result| result.outcome == ReconciliationOutcome::ChangedStorageObject)
            .map(|result| ExternalChangeView {
                result_id: result.id.to_string(),
                name: result
                    .storage_object_id
                    .clone()
                    .unwrap_or_else(|| "Changed object".into()),
                detail: "The primary object changed outside rec_bag.".into(),
            })
            .collect();
        let failed_reconciliations = report
            .results
            .iter()
            .filter(|result| {
                matches!(
                    result.outcome,
                    ReconciliationOutcome::Track(TrackStorageStatus::Error)
                )
            })
            .map(|result| FailedReconciliationView {
                result_id: result.id.to_string(),
                name: result
                    .track_id
                    .and_then(|id| track_by_id.get(&id))
                    .and_then(|track| preferred_text(track, MetadataField::Title))
                    .unwrap_or_else(|| "Reconciliation failure".into()),
                error: "Storage reconciliation failed".into(),
            })
            .collect();
        let playlist_views = report
            .playlist_outcomes
            .iter()
            .map(|outcome| PlaylistCompletenessView {
                id: outcome.playlist_id.to_string(),
                name: playlists
                    .iter()
                    .find(|playlist| playlist.id == outcome.playlist_id)
                    .map(|playlist| playlist.name.clone())
                    .unwrap_or_else(|| "Playlist".into()),
                archived_tracks: outcome.summary.archived_tracks as u32,
                total_tracks: outcome.summary.archivable_tracks as u32,
                needs_review: outcome
                    .unique_track_ids
                    .iter()
                    .filter(|id| {
                        status_by_track.get(id).is_some_and(|status| {
                            matches!(
                                status,
                                TrackStorageStatus::PossibleMatch
                                    | TrackStorageStatus::AmbiguousMatch
                            )
                        })
                    })
                    .count() as u32,
            })
            .collect();
        let count = |status| {
            report
                .track_outcomes
                .iter()
                .filter(|outcome| outcome.status == status)
                .count() as u32
        };
        Ok(MusicManagementView {
            sources: source_views,
            primary_storage,
            backup_storages,
            unassigned_storages,
            completeness: CompletenessView {
                canonical_tracks: report.summary.total_tracks as u32,
                catalog_covered: report.summary.covered_tracks as u32,
                verified_assets: report.summary.verified_assets as u32,
                missing_obtainable: count(TrackStorageStatus::MissingObtainable),
                streaming_only: count(TrackStorageStatus::NotAvailableForArchival),
                restricted: count(TrackStorageStatus::Blocked)
                    + count(TrackStorageStatus::Inaccessible),
                possible_matches: count(TrackStorageStatus::PossibleMatch),
                ambiguous_matches: count(TrackStorageStatus::AmbiguousMatch),
                unknown_primary_files: report
                    .results
                    .iter()
                    .filter(|result| {
                        result.outcome == ReconciliationOutcome::UntrackedStorageObject
                    })
                    .count() as u32,
                backup_gaps: 0,
                provider_errors: count(TrackStorageStatus::Error)
                    + count(TrackStorageStatus::StorageUnavailable),
                playlists: playlist_views,
            },
            tracks: track_views,
            possible_matches,
            unknown_primary_files,
            external_changes,
            failed_reconciliations,
            cleanup_candidates: Vec::new(),
        })
    }

    async fn storage_view(&self, storage: &MusicStorage) -> StorageView {
        let health = if storage.kind == MusicStorageKind::GoogleDrive {
            match self.registry.get_or_open(storage).await {
                Ok(runtime) => runtime
                    .health()
                    .await
                    .ok()
                    .map(|value| health_tone(value.status)),
                Err(_) => None,
            }
        } else {
            storage.enabled.then_some(HealthTone::Healthy)
        };
        StorageView {
            id: storage.id.to_string(),
            provider: if storage.kind == MusicStorageKind::GoogleDrive {
                ProviderChoice::GoogleDrive
            } else {
                ProviderChoice::LocalFolder
            },
            name: storage.name.clone(),
            location: storage
                .config
                .get(CONFIG_FOLDER_NAME)
                .cloned()
                .or_else(|| storage.root_identifier.clone())
                .unwrap_or_default(),
            health: health.unwrap_or(HealthTone::Offline),
            role: match storage.role {
                StorageRole::Primary => StorageRoleView::Primary,
                StorageRole::Backup => StorageRoleView::Backup,
                StorageRole::Unassigned => StorageRoleView::Unassigned,
            },
            last_inventory: storage.last_inventory_at.map(format_time),
            last_verified: None,
            pending_jobs: 0,
            failed_jobs: 0,
            replication_mode: (storage.role == StorageRole::Backup)
                .then(|| format!("{:?}", storage.replication_mode)),
            retention_summary: (storage.role == StorageRole::Backup).then(|| {
                format!(
                    "{} days / {} versions",
                    storage.retention_policy.retention_days,
                    storage.retention_policy.minimum_versions
                )
            }),
        }
    }

    async fn storage(&self, id: Uuid) -> Result<MusicStorage, GoogleDriveServiceError> {
        let storage = MusicStorageRepository::get(self.catalog.as_ref(), id)
            .await?
            .ok_or_else(|| {
                GoogleDriveServiceError::InvalidConfiguration("storage not found".into())
            })?;
        if storage.kind != MusicStorageKind::GoogleDrive {
            return Err(GoogleDriveServiceError::InvalidConfiguration(
                "storage is not Google Drive".into(),
            ));
        }
        Ok(storage)
    }

    async fn list_tracks(&self) -> Result<Vec<rec_core::CanonicalTrack>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = TrackRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_sources(&self) -> Result<Vec<rec_core::CatalogSource>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = SourceRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_occurrences(
        &self,
    ) -> Result<Vec<rec_core::SourceTrackOccurrence>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = SourceRepository::list_occurrences(
                self.catalog.as_ref(),
                SourceOccurrenceFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_originals(&self) -> Result<Vec<OriginalAsset>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = AssetRepository::list_originals(
                self.catalog.as_ref(),
                OriginalAssetFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_locations(&self) -> Result<Vec<AssetLocation>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = AssetRepository::list_locations(
                self.catalog.as_ref(),
                AssetLocationFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_resolutions(
        &self,
    ) -> Result<Vec<rec_core::ReconciliationResolution>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = ReconciliationRepository::list_resolutions(
                self.catalog.as_ref(),
                ReconciliationResolutionFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_suppressions(
        &self,
    ) -> Result<Vec<rec_core::ReconciliationSuppression>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = ReconciliationRepository::list_suppressions(
                self.catalog.as_ref(),
                ReconciliationResolutionFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_playlists(&self) -> Result<Vec<rec_core::CanonicalPlaylist>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = PlaylistRepository::list(
                self.catalog.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }

    async fn list_storage_objects(
        &self,
        storage_id: Uuid,
    ) -> Result<Vec<StorageObject>, RepositoryError> {
        let mut values = Vec::new();
        let mut cursor = None;
        loop {
            let page = AssetRepository::list_storage_objects(
                self.catalog.as_ref(),
                storage_id,
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await?;
            values.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(values);
            }
        }
    }
}

impl IntegrationSliceHandler for GoogleDriveService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::GoogleDrive
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            let result: Result<IntegrationOutcome, GoogleDriveServiceError> = async {
                let message = match &request.event {
                    MusicManagementEvent::CreatePrimary(configuration)
                        if configuration.provider == ProviderChoice::GoogleDrive =>
                    {
                        let storage = self.configure_primary(configuration).await?;
                        self.inventory(storage.id, true).await?;
                        if MusicStorageRepository::get_active_primary(self.catalog.as_ref())
                            .await?
                            .is_none()
                        {
                            MusicStorageRepository::switch_active_primary(
                                self.catalog.as_ref(),
                                None,
                                Some(storage.id),
                            )
                            .await?;
                        }
                        "Google Drive primary configured and inventoried".to_owned()
                    }
                    MusicManagementEvent::AddBackup(ProviderChoice::GoogleDrive) => {
                        self.configure_backup().await?;
                        "Google Drive backup configured and inventoried".to_owned()
                    }
                    MusicManagementEvent::CopyToPrimary(track_id) => {
                        let track_id = Uuid::parse_str(track_id).map_err(|error| {
                            GoogleDriveServiceError::InvalidConfiguration(format!(
                                "invalid track ID: {error}"
                            ))
                        })?;
                        self.copy_track_to_active_primary(track_id).await?;
                        "Track copied to the active Google Drive primary".to_owned()
                    }
                    MusicManagementEvent::ToggleOfflinePin(track_id, pinned) => {
                        let track_id = Uuid::parse_str(track_id).map_err(|error| {
                            GoogleDriveServiceError::InvalidConfiguration(format!(
                                "invalid track ID: {error}"
                            ))
                        })?;
                        self.set_track_offline_pinned(track_id, *pinned).await?;
                        if *pinned {
                            "Track pinned for offline playback".to_owned()
                        } else {
                            "Offline pin removed".to_owned()
                        }
                    }
                    MusicManagementEvent::RetryStorage(storage_id) => {
                        let storage_id = parse_lifecycle_storage_id(storage_id)?;
                        self.retry_storage(storage_id).await?;
                        "Google Drive storage inventory retried".to_owned()
                    }
                    MusicManagementEvent::DisconnectStorage { storage_id } => {
                        let storage_id = parse_lifecycle_storage_id(storage_id)?;
                        self.disconnect_storage(storage_id).await?;
                        "Google Drive storage disconnected; catalog data was retained".to_owned()
                    }
                    MusicManagementEvent::RevokeProviderGrant {
                        provider: ProviderChoice::GoogleDrive,
                        connection_id,
                    } => {
                        let storage_id = parse_lifecycle_storage_id(connection_id)?;
                        self.revoke_provider_grant(storage_id).await?;
                        "Google access revoked and matching storages disabled".to_owned()
                    }
                    MusicManagementEvent::ClearLocalCache { source_id } => {
                        let storage_id = parse_lifecycle_storage_id(source_id)?;
                        self.clear_local_cache(storage_id).await?;
                        "Google Drive local cache cleared; catalog data was retained".to_owned()
                    }
                    MusicManagementEvent::ForgetProviderCatalogData { source_id } => {
                        let storage_id = parse_lifecycle_storage_id(source_id)?;
                        self.forget_provider_catalog_data(storage_id).await?;
                        "Google Drive local provider data and cache deleted; remote files and authorization were retained"
                            .to_owned()
                    }
                    // The migration plan dialog is the final confirmation boundary. No
                    // authority switch occurs before this event reaches the handler.
                    MusicManagementEvent::PreparePrimaryMigration(plan)
                        if plan.destination.provider == ProviderChoice::GoogleDrive =>
                    {
                        let source =
                            MusicStorageRepository::get_active_primary(self.catalog.as_ref())
                                .await?
                                .ok_or_else(|| {
                                    GoogleDriveServiceError::InvalidConfiguration(
                                        "no active primary exists".into(),
                                    )
                                })?;
                        let destination = self.configure_primary(&plan.destination).await?;
                        self.inventory(destination.id, true).await?;
                        self.migrate_primary(
                            source.id,
                            destination.id,
                            plan.verify_checksums,
                            plan.keep_old_as_backup,
                        )
                        .await?;
                        "Primary migration completed and verified".to_owned()
                    }
                    MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive) => {
                        return Err(GoogleDriveServiceError::InvalidConfiguration(
                            "Google Drive is music storage, not a catalog source".into(),
                        ));
                    }
                    _ => {
                        return Err(GoogleDriveServiceError::InvalidConfiguration(
                            "event is not owned by the Google Drive slice".into(),
                        ));
                    }
                };
                Ok(IntegrationOutcome {
                    view: Some(self.project_view().await?),
                    message: Some(message),
                })
            }
            .await;
            result.map_err(|error| {
                let mut integration: IntegrationError = error.into();
                integration.request_id = Some(request.id);
                integration
            })
        })
    }
}

impl AssetResolver for GoogleDriveService {
    fn resolve<'a>(&'a self, asset: &'a AssetReference) -> AssetFuture<'a> {
        Box::pin(async move {
            let storage_id =
                Uuid::parse_str(&asset.storage_id).map_err(|error| IntegrationError {
                    request_id: None,
                    slice: Some(IntegrationSlice::GoogleDrive),
                    message: format!("invalid storage ID: {error}"),
                    retryable: false,
                })?;
            let asset_id = Uuid::parse_str(&asset.asset_id).map_err(|error| IntegrationError {
                request_id: None,
                slice: Some(IntegrationSlice::GoogleDrive),
                message: format!("invalid asset ID: {error}"),
                retryable: false,
            })?;
            let resolved = GoogleDriveService::resolve(self, storage_id, asset_id)
                .await
                .map_err(IntegrationError::from)?;
            Ok(ResolvedAsset {
                uri: resolved.local_path.to_string_lossy().into_owned(),
                cached: true,
            })
        })
    }
}

fn remove_cache_tree(path: &Path) -> Result<(), GoogleDriveServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(path)?;
        }
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => {
            return Err(GoogleDriveServiceError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache path is not a regular file, symlink, or directory",
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn parse_lifecycle_storage_id(value: &str) -> Result<Uuid, GoogleDriveServiceError> {
    Uuid::parse_str(value).map_err(|error| {
        GoogleDriveServiceError::InvalidConfiguration(format!("invalid storage ID: {error}"))
    })
}

fn format_time(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn preferred_text(track: &rec_core::CanonicalTrack, field: MetadataField) -> Option<String> {
    match &track.metadata.preferred(&field)?.value {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

fn health_tone(status: HealthStatus) -> HealthTone {
    match status {
        HealthStatus::Healthy => HealthTone::Healthy,
        HealthStatus::Degraded => HealthTone::Warning,
        HealthStatus::AuthenticationRequired => HealthTone::Warning,
        HealthStatus::Unavailable => HealthTone::Offline,
    }
}

fn status_view(status: TrackStorageStatus) -> TrackStorageStatusView {
    match status {
        TrackStorageStatus::StoredAndVerified => TrackStorageStatusView::StoredAndVerified,
        TrackStorageStatus::MissingObtainable => TrackStorageStatusView::MissingObtainable,
        TrackStorageStatus::PossibleMatch => TrackStorageStatusView::PossibleMatch,
        TrackStorageStatus::AmbiguousMatch => TrackStorageStatusView::AmbiguousMatch,
        TrackStorageStatus::NotAvailableForArchival => {
            TrackStorageStatusView::NotAvailableForArchival
        }
        TrackStorageStatus::PreviewOnly => TrackStorageStatusView::PreviewOnly,
        TrackStorageStatus::Blocked => TrackStorageStatusView::Blocked,
        TrackStorageStatus::Removed => TrackStorageStatusView::Removed,
        TrackStorageStatus::Inaccessible => TrackStorageStatusView::Inaccessible,
        TrackStorageStatus::StorageUnavailable => TrackStorageStatusView::StorageUnavailable,
        TrackStorageStatus::Error => TrackStorageStatusView::Error,
        TrackStorageStatus::Ignored => TrackStorageStatusView::Ignored,
    }
}

fn verified_location(
    existing: Option<AssetLocation>,
    original: &OriginalAsset,
    object: &StorageObject,
) -> AssetLocation {
    let now = Utc::now();
    let observed = object
        .canonical_hash()
        .cloned()
        .or_else(|| original.content_hash.clone());
    AssetLocation {
        id: existing
            .as_ref()
            .map_or_else(Uuid::new_v4, |value| value.id),
        asset_id: original.id(),
        asset_type: rec_core::AssetType::Original,
        storage_id: object.storage_id,
        object_id: object.object_id.clone(),
        verification_status: StorageVerificationStatus::Verified,
        expected_hash: original.content_hash.clone(),
        observed_hash: observed,
        provider_version: object.provider_version.clone(),
        last_checked_at: Some(now),
        verified_at: Some(now),
        error_message: None,
    }
}

fn upload_setting_key(storage_id: Uuid, asset_id: Uuid, idempotency_key: &str) -> String {
    let digest = Sha256::digest(idempotency_key.as_bytes());
    format!("google_drive.upload.{storage_id}.{asset_id}.{digest:x}")
}

fn sync_error_for(error: &GoogleDriveServiceError) -> SyncError {
    let kind = match error {
        GoogleDriveServiceError::Authentication(_) => SyncErrorKind::Authentication,
        GoogleDriveServiceError::Repository(source)
            if source.kind == RepositoryErrorKind::Conflict =>
        {
            SyncErrorKind::Conflict
        }
        GoogleDriveServiceError::Repository(source) if source.is_retryable() => {
            SyncErrorKind::TemporarilyUnavailable
        }
        GoogleDriveServiceError::Storage(source) => match source.kind {
            MusicStorageErrorKind::AuthenticationRequired => SyncErrorKind::Authentication,
            MusicStorageErrorKind::PermissionDenied => SyncErrorKind::Authorization,
            MusicStorageErrorKind::RateLimited => SyncErrorKind::RateLimited,
            MusicStorageErrorKind::TemporarilyUnavailable => SyncErrorKind::TemporarilyUnavailable,
            MusicStorageErrorKind::NotFound => SyncErrorKind::NotFound,
            MusicStorageErrorKind::ChecksumMismatch => SyncErrorKind::Integrity,
            MusicStorageErrorKind::Io => SyncErrorKind::Io,
            MusicStorageErrorKind::InvalidData => SyncErrorKind::InvalidData,
            _ => SyncErrorKind::Provider,
        },
        GoogleDriveServiceError::Verification(_) => SyncErrorKind::Integrity,
        GoogleDriveServiceError::Io(_) => SyncErrorKind::Io,
        GoogleDriveServiceError::InvalidConfiguration(_)
        | GoogleDriveServiceError::FolderSelection(_)
        | GoogleDriveServiceError::InvalidData(_) => SyncErrorKind::InvalidData,
        _ => SyncErrorKind::Provider,
    };
    SyncError::new(kind, "google_drive", error.to_string(), error.retryable())
}

fn parse_storage_config(
    storage: &MusicStorage,
) -> Result<(String, GoogleDriveFolder, u64), GoogleDriveServiceError> {
    let account = storage
        .config
        .get(CONFIG_CREDENTIAL_ACCOUNT)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            GoogleDriveServiceError::InvalidConfiguration("credential reference is missing".into())
        })?;
    let folder_id = storage
        .root_identifier
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            GoogleDriveServiceError::InvalidConfiguration("Drive folder ID is missing".into())
        })?;
    let mut folder = GoogleDriveFolder::new(folder_id);
    if let Some(name) = storage.config.get(CONFIG_FOLDER_NAME) {
        folder = folder.with_display_name(name.clone());
    }
    let max_bytes = storage
        .config
        .get(CONFIG_CACHE_MAX_BYTES)
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| GoogleDriveServiceError::InvalidConfiguration("cache size is invalid".into()))?
        .unwrap_or(DEFAULT_CACHE_MAX_BYTES);
    Ok((account, folder, max_bytes))
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
pub struct KeyringCredentialStore {
    service: String,
}

#[cfg(feature = "desktop")]
impl KeyringCredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &CredentialKey) -> Result<keyring::Entry, IntegrationError> {
        keyring::Entry::new(&self.service, &format!("{}:{}", key.provider, key.account)).map_err(
            |error| IntegrationError {
                request_id: None,
                slice: Some(IntegrationSlice::GoogleDrive),
                message: format!("credential store is unavailable: {error}"),
                retryable: true,
            },
        )
    }
}

#[cfg(feature = "desktop")]
impl CredentialStore for KeyringCredentialStore {
    fn load<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, Option<Secret>> {
        Box::pin(async move {
            let entry = self.entry(key)?;
            match entry.get_secret() {
                Ok(secret) => Ok(Some(Secret::new(secret))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(IntegrationError {
                    request_id: None,
                    slice: Some(IntegrationSlice::GoogleDrive),
                    message: format!("credential read failed: {error}"),
                    retryable: true,
                }),
            }
        })
    }

    fn store<'a>(
        &'a self,
        key: &'a CredentialKey,
        secret: Secret,
    ) -> super::integration::CredentialFuture<'a, ()> {
        Box::pin(async move {
            self.entry(key)?
                .set_secret(secret.expose())
                .map_err(|error| IntegrationError {
                    request_id: None,
                    slice: Some(IntegrationSlice::GoogleDrive),
                    message: format!("credential write failed: {error}"),
                    retryable: true,
                })
        })
    }

    fn delete<'a>(
        &'a self,
        key: &'a CredentialKey,
    ) -> super::integration::CredentialFuture<'a, ()> {
        Box::pin(async move {
            match self.entry(key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(IntegrationError {
                    request_id: None,
                    slice: Some(IntegrationSlice::GoogleDrive),
                    message: format!("credential deletion failed: {error}"),
                    retryable: true,
                }),
            }
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
                .map_err(|error| GoogleDriveServiceError::Authentication(error.to_string()))?;
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
                return Err(GoogleDriveServiceError::Authentication(
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
) -> Result<GoogleCredential, GoogleDriveServiceError> {
    let response =
        response.map_err(|error| GoogleDriveServiceError::Authentication(error.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| GoogleDriveServiceError::Authentication(error.to_string()))?;
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

#[cfg(feature = "desktop")]
pub struct ProductionDriveRuntimeFactory {
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn GoogleOAuth>,
    cache_root: PathBuf,
}

#[cfg(feature = "desktop")]
impl ProductionDriveRuntimeFactory {
    pub fn new(
        credentials: Arc<dyn CredentialStore>,
        oauth: Arc<dyn GoogleOAuth>,
        cache_root: PathBuf,
    ) -> Self {
        Self {
            credentials,
            oauth,
            cache_root,
        }
    }
}

#[cfg(feature = "desktop")]
impl DriveRuntimeFactory for ProductionDriveRuntimeFactory {
    fn open<'a>(&'a self, storage: &'a MusicStorage) -> ServiceFuture<'a, Arc<dyn DriveRuntime>> {
        Box::pin(async move {
            let (account, folder, max_bytes) = parse_storage_config(storage)?;
            let mut config = GoogleDriveStoreConfig::new(storage.id, folder);
            config.cache = Some(CacheConfig {
                directory: self.cache_root.join(storage.id.to_string()),
                max_bytes,
            });
            let tokens = DesktopGoogleTokenProvider::new(
                account,
                self.credentials.clone(),
                self.oauth.clone(),
            );
            let store =
                GoogleDriveMusicStore::new(config, ReqwestDriveTransport::default(), tokens)?;
            Ok(Arc::new(store) as Arc<dyn DriveRuntime>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{BTreeSet, VecDeque},
        sync::Mutex,
    };

    use rec_core::{
        CanonicalTrack, CatalogSnapshotRepository, DownloadResult, InventoryPage,
        OriginalAssetRole, StorageHealth, StorageVersionPage, StorageVersionRequest,
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
        ) -> super::super::integration::CredentialFuture<'a, Option<Secret>> {
            Box::pin(async move { Ok(self.0.lock().unwrap().get(key).cloned()) })
        }
        fn store<'a>(
            &'a self,
            key: &'a CredentialKey,
            secret: Secret,
        ) -> super::super::integration::CredentialFuture<'a, ()> {
            Box::pin(async move {
                self.0.lock().unwrap().insert(key.clone(), secret);
                Ok(())
            })
        }
        fn delete<'a>(
            &'a self,
            key: &'a CredentialKey,
        ) -> super::super::integration::CredentialFuture<'a, ()> {
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
        fn provision<'a>(
            &'a self,
            configuration: &'a PrimaryConfigurationView,
        ) -> ServiceFuture<'a, ProvisionedDrive> {
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
                        id: if configuration.location.is_empty() {
                            "selected-backup-folder".into()
                        } else {
                            configuration.location.clone()
                        },
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
        cache: Mutex<BTreeMap<String, PathBuf>>,
        pinned: Mutex<BTreeSet<String>>,
        root: Mutex<Option<PathBuf>>,
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
            Box::pin(async move { Ok(self.inventory.lock().unwrap().remove(0)) })
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

    impl DriveRuntime for FakeRuntime {
        fn cached_path(&self, object_id: &str) -> Result<Option<PathBuf>, MusicStorageError> {
            Ok(self.cache.lock().unwrap().get(object_id).cloned())
        }
        fn cache<'a>(
            &'a self,
            object_id: &'a str,
        ) -> BoxFuture<'a, Result<PathBuf, MusicStorageError>> {
            Box::pin(async move {
                let root = self.root.lock().unwrap().clone().unwrap();
                std::fs::create_dir_all(&root).unwrap();
                let path = root.join(object_id);
                let bytes = self.bytes.lock().unwrap().get(object_id).cloned().unwrap();
                std::fs::write(&path, bytes).unwrap();
                self.cache
                    .lock()
                    .unwrap()
                    .insert(object_id.into(), path.clone());
                Ok(path)
            })
        }
        fn set_pinned(&self, object_id: &str, pinned: bool) -> Result<(), MusicStorageError> {
            if pinned {
                self.pinned.lock().unwrap().insert(object_id.into());
            } else {
                self.pinned.lock().unwrap().remove(object_id);
            }
            Ok(())
        }
        fn evict_cache(&self) -> Result<(), MusicStorageError> {
            Ok(())
        }
    }

    struct CreatingFactory {
        root: PathBuf,
        opened: Arc<Mutex<Vec<Uuid>>>,
    }

    impl DriveRuntimeFactory for CreatingFactory {
        fn open<'a>(
            &'a self,
            storage: &'a MusicStorage,
        ) -> ServiceFuture<'a, Arc<dyn DriveRuntime>> {
            Box::pin(async move {
                self.opened.lock().unwrap().push(storage.id);
                let runtime = Arc::new(FakeRuntime::default());
                *runtime.root.lock().unwrap() = Some(self.root.join(storage.id.to_string()));
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
                Ok(runtime as Arc<dyn DriveRuntime>)
            })
        }
    }

    struct NeverFactory;
    impl DriveRuntimeFactory for NeverFactory {
        fn open<'a>(
            &'a self,
            _storage: &'a MusicStorage,
        ) -> ServiceFuture<'a, Arc<dyn DriveRuntime>> {
            Box::pin(async { Err(GoogleDriveServiceError::Runtime("not registered".into())) })
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

    async fn fixture() -> (GoogleDriveService, Arc<TursoCatalog>, PathBuf) {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-test-{}", Uuid::new_v4()));
        let registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(NeverFactory)));
        let service = GoogleDriveService::new(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            Arc::new(FakeProvisioner),
            registry,
            GoogleDrivePaths {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
            },
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
    async fn cache_purge_removes_one_or_all_storage_roots_and_runtime_entries() {
        let (service, _catalog, root) = fixture().await;
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let first_path = service.paths.cache_root.join(first.to_string());
        let second_path = service.paths.cache_root.join(second.to_string());
        std::fs::create_dir_all(&first_path).unwrap();
        std::fs::create_dir_all(&second_path).unwrap();
        std::fs::write(first_path.join("cached"), b"one").unwrap();
        std::fs::write(second_path.join("cached"), b"two").unwrap();
        service
            .registry
            .register(first, Arc::new(FakeRuntime::default()));
        service
            .registry
            .register(second, Arc::new(FakeRuntime::default()));

        service.purge_storage_cache(first).unwrap();
        assert!(!first_path.exists());
        assert!(second_path.exists());
        assert!(!service
            .registry
            .runtimes
            .read()
            .unwrap()
            .contains_key(&first));

        service.purge_all_caches().unwrap();
        assert!(service.paths.cache_root.is_dir());
        assert!(!second_path.exists());
        assert!(service.registry.runtimes.read().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
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
    async fn disconnect_is_idempotent_and_preserves_credentials_cache_and_catalog() {
        let (service, catalog, root) = fixture().await;
        let credentials = service.credentials.clone();
        let mut storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("folder");
        storage
            .config
            .insert(CONFIG_CREDENTIAL_ACCOUNT.into(), "account".into());
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: "account".into(),
        };
        credentials
            .store(&key, Secret::new(b"credential".to_vec()))
            .await
            .unwrap();
        service
            .registry
            .register(storage.id, Arc::new(FakeRuntime::default()));
        let cache_file = service
            .paths
            .cache_root
            .join(storage.id.to_string())
            .join("audio");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        std::fs::write(&cache_file, b"cached").unwrap();

        service.disconnect_storage(storage.id).await.unwrap();
        service.disconnect_storage(storage.id).await.unwrap();

        let saved = MusicStorageRepository::get(catalog.as_ref(), storage.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.root_identifier.as_deref(), Some("folder"));
        assert!(credentials.load(&key).await.unwrap().is_some());
        assert!(cache_file.exists());
        assert!(!service
            .registry
            .runtimes
            .read()
            .unwrap()
            .contains_key(&storage.id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn revoke_is_idempotent_and_disables_every_storage_for_the_account() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let credentials = Arc::new(MemoryCredentials::default());
        let revoker = Arc::new(RecordingRevoker::default());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-revoke-{}", Uuid::new_v4()));
        let registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(NeverFactory)));
        let service = GoogleDriveService::new(
            catalog.clone(),
            credentials.clone(),
            Arc::new(FakeProvisioner),
            registry.clone(),
            GoogleDrivePaths {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
            },
        )
        .with_grant_revoker(revoker.clone());
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
            registry.register(storage.id, Arc::new(FakeRuntime::default()));
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

        service.revoke_provider_grant(first.id).await.unwrap();
        service.revoke_provider_grant(first.id).await.unwrap();

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
        assert!(!registry.runtimes.read().unwrap().contains_key(&first.id));
        assert!(!registry.runtimes.read().unwrap().contains_key(&second.id));
        assert!(registry.runtimes.read().unwrap().contains_key(&other.id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn clear_local_cache_is_idempotent_and_does_not_change_catalog() {
        let (service, catalog, root) = fixture().await;
        let storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let cache_file = service
            .paths
            .cache_root
            .join(storage.id.to_string())
            .join("audio");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        std::fs::write(&cache_file, b"cached").unwrap();

        service.clear_local_cache(storage.id).await.unwrap();
        service.clear_local_cache(storage.id).await.unwrap();

        assert!(!cache_file.exists());
        assert_eq!(
            MusicStorageRepository::get(catalog.as_ref(), storage.id)
                .await
                .unwrap(),
            Some(storage)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn forget_catalog_data_is_idempotent_and_preserves_owned_and_remote_files() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let credentials = Arc::new(MemoryCredentials::default());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-forget-{}", Uuid::new_v4()));
        let registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(NeverFactory)));
        let service = GoogleDriveService::new(
            catalog.clone(),
            credentials.clone(),
            Arc::new(FakeProvisioner),
            registry.clone(),
            GoogleDrivePaths {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
            },
        );
        let mut storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        storage
            .config
            .insert(CONFIG_CREDENTIAL_ACCOUNT.into(), "account".into());
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let key = CredentialKey {
            provider: PROVIDER_KEY.into(),
            account: "account".into(),
        };
        credentials
            .store(&key, Secret::new(b"credential".to_vec()))
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "one.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let object = FakeRuntime::object(storage.id, "one", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(None, &original, &object),
        )
        .await
        .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .objects
            .lock()
            .unwrap()
            .insert(object.object_id.clone(), object.clone());
        registry.register(storage.id, runtime.clone());
        let cache_file = service
            .paths
            .cache_root
            .join(storage.id.to_string())
            .join("audio");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        std::fs::write(&cache_file, b"cached").unwrap();
        catalog.export_snapshot().await.unwrap();

        service
            .forget_provider_catalog_data(storage.id)
            .await
            .unwrap();
        service
            .forget_provider_catalog_data(storage.id)
            .await
            .unwrap();

        assert!(MusicStorageRepository::get(catalog.as_ref(), storage.id)
            .await
            .unwrap()
            .is_none());
        assert!(
            AssetRepository::get_location(catalog.as_ref(), storage.id, "one".into())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            AssetRepository::get_storage_object(catalog.as_ref(), storage.id, "one".into())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            TrackRepository::get(catalog.as_ref(), track.id)
                .await
                .unwrap(),
            Some(track)
        );
        assert_eq!(
            AssetRepository::get_original(catalog.as_ref(), original.id())
                .await
                .unwrap(),
            Some(original)
        );
        assert!(credentials.load(&key).await.unwrap().is_some());
        assert!(!cache_file.exists());
        assert!(!registry.runtimes.read().unwrap().contains_key(&storage.id));
        assert_eq!(
            runtime.objects.lock().unwrap().get("one").cloned(),
            Some(object)
        );
        assert!(CatalogSnapshotRepository::latest(catalog.as_ref())
            .await
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn configuration_persists_only_non_secret_identity() {
        let (service, catalog, root) = fixture().await;
        let storage_id = Uuid::new_v4();
        let runtime = Arc::new(FakeRuntime::default());
        service.registry.register(storage_id, runtime);
        // Register after the generated ID by using factory is intentionally impossible;
        // persist manually to test the configuration representation.
        let provisioned = FakeProvisioner
            .provision(&configuration("folder-id"))
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
    async fn staged_inventory_finalizes_objects_and_cursor() {
        let (service, catalog, root) = fixture().await;
        let storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("folder");
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.inventory.lock().unwrap().push(InventoryPage {
            objects: vec![FakeRuntime::object(storage.id, "one", hash(b"abc"))],
            removals: vec![],
            next_page_cursor: None,
            next_change_cursor: Some(rec_core::SyncCursor::new("cursor-1")),
            progress: None,
        });
        service.registry.register(storage.id, runtime);
        let completion = service.inventory(storage.id, true).await.unwrap();
        assert_eq!(completion.object_count, 1);
        assert!(
            AssetRepository::get_storage_object(catalog.as_ref(), storage.id, "one".into())
                .await
                .unwrap()
                .is_some()
        );
        let run = SyncRunRepository::get(catalog.as_ref(), completion.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, SyncStatus::Succeeded);
        assert_eq!(run.sync_cursor.unwrap().value, "cursor-1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resolver_downloads_once_and_pins_verified_cache() {
        let (service, catalog, root) = fixture().await;
        let mut storage = MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("folder");
        storage
            .config
            .insert(CONFIG_CREDENTIAL_ACCOUNT.into(), "account".into());
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let original = OriginalAsset::new(
            track.id,
            OriginalAssetRole::Master,
            "one.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let object = FakeRuntime::object(storage.id, "one", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(None, &original, &object),
        )
        .await
        .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        *runtime.root.lock().unwrap() = Some(root.join("cache"));
        runtime.objects.lock().unwrap().insert("one".into(), object);
        runtime
            .bytes
            .lock()
            .unwrap()
            .insert("one".into(), b"abc".to_vec());
        service.registry.register(storage.id, runtime.clone());
        assert!(
            !service
                .resolve(storage.id, original.id())
                .await
                .unwrap()
                .cache_hit
        );
        assert!(
            service
                .resolve(storage.id, original.id())
                .await
                .unwrap()
                .cache_hit
        );
        service
            .set_offline_pinned(storage.id, original.id(), true)
            .await
            .unwrap();
        assert!(runtime.pinned.lock().unwrap().contains("one"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn migration_switches_only_after_destination_verification() {
        let (service, catalog, root) = fixture().await;
        let source = MusicStorage::new(
            "Old",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("old");
        let destination = MusicStorage::new(
            "New",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier("new");
        MusicStorageRepository::upsert(catalog.as_ref(), source.clone())
            .await
            .unwrap();
        MusicStorageRepository::upsert(catalog.as_ref(), destination.clone())
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
            "one.flac",
            3,
            hash(b"abc").value(),
        )
        .unwrap();
        AssetRepository::upsert_original(catalog.as_ref(), original.clone())
            .await
            .unwrap();
        let source_object = FakeRuntime::object(source.id, "source", hash(b"abc"));
        AssetRepository::upsert_storage_object(catalog.as_ref(), source_object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(None, &original, &source_object),
        )
        .await
        .unwrap();
        let source_runtime = Arc::new(FakeRuntime::default());
        source_runtime
            .objects
            .lock()
            .unwrap()
            .insert("source".into(), source_object);
        source_runtime
            .bytes
            .lock()
            .unwrap()
            .insert("source".into(), b"abc".to_vec());
        let destination_runtime = Arc::new(FakeRuntime::default());
        destination_runtime.objects.lock().unwrap().insert(
            "seed".into(),
            FakeRuntime::object(destination.id, "seed", hash(b"seed")),
        );
        service.registry.register(source.id, source_runtime);
        service
            .registry
            .register(destination.id, destination_runtime);
        let result = service
            .migrate_primary(source.id, destination.id, true, true)
            .await
            .unwrap();
        assert_eq!(result.copied_assets, 1);
        assert_eq!(
            MusicStorageRepository::get_active_primary(catalog.as_ref())
                .await
                .unwrap()
                .unwrap()
                .id,
            destination.id
        );
        assert_eq!(
            MusicStorageRepository::get(catalog.as_ref(), source.id)
                .await
                .unwrap()
                .unwrap()
                .role,
            StorageRole::Backup
        );
        assert!(CatalogSnapshotRepository::latest(catalog.as_ref())
            .await
            .unwrap()
            .is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_migration_handler_is_final_confirmation_and_snapshots_before_cas() {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let root = std::env::temp_dir().join(format!("rec-bag-drive-confirm-{}", Uuid::new_v4()));
        let registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(CreatingFactory {
            root: root.clone(),
            opened: Arc::new(Mutex::new(Vec::new())),
        })));
        let service = GoogleDriveService::new(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            Arc::new(FakeProvisioner),
            registry.clone(),
            GoogleDrivePaths {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
            },
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
            verified_location(None, &original, &source_object),
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
        registry.register(source.id, source_runtime);

        assert!(CatalogSnapshotRepository::latest(catalog.as_ref())
            .await
            .unwrap()
            .is_none());
        let outcome = service
            .handle(IntegrationRequest {
                id: 9,
                slice: IntegrationSlice::GoogleDrive,
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
        assert!(CatalogSnapshotRepository::latest(catalog.as_ref())
            .await
            .unwrap()
            .is_some());
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
        let registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(CreatingFactory {
            root: root.clone(),
            opened: opened.clone(),
        })));
        let service = GoogleDriveService::new(
            catalog.clone(),
            Arc::new(MemoryCredentials::default()),
            Arc::new(FakeProvisioner),
            registry,
            GoogleDrivePaths {
                cache_root: root.join("cache"),
                staging_root: root.join("staging"),
            },
        );
        let outcome = service
            .handle(IntegrationRequest {
                id: 10,
                slice: IntegrationSlice::GoogleDrive,
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
            verified_location(None, &original, &local_object),
        )
        .await
        .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.objects.lock().unwrap().insert(
            "seed".into(),
            FakeRuntime::object(drive.id, "seed", hash(b"seed")),
        );
        service.registry.register(drive.id, runtime);
        let outcome = service
            .handle(IntegrationRequest {
                id: 11,
                slice: IntegrationSlice::GoogleDrive,
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
        let operation_key = format!(
            "copy-to-primary:{}:{}:{}",
            drive.id,
            original.id(),
            original.content_hash.as_ref().unwrap().value()
        );
        let checkpoint_key = upload_setting_key(drive.id, original.id(), &operation_key);
        assert!(SettingsRepository::get(catalog.as_ref(), checkpoint_key)
            .await
            .unwrap()
            .is_none());
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
        AssetRepository::upsert_location(
            catalog.as_ref(),
            verified_location(None, &original, &object),
        )
        .await
        .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        *runtime.root.lock().unwrap() = Some(root.join("cache"));
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
        service.registry.register(drive.id, runtime.clone());
        let outcome = service
            .handle(IntegrationRequest {
                id: 12,
                slice: IntegrationSlice::GoogleDrive,
                event: MusicManagementEvent::ToggleOfflinePin(track.id.to_string(), true),
            })
            .await
            .unwrap();
        assert!(outcome.view.is_some());
        assert!(runtime.pinned.lock().unwrap().contains("song"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn retry_storage_handler_commits_inventory_and_returns_projection() {
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
        let runtime = Arc::new(FakeRuntime::default());
        runtime.inventory.lock().unwrap().push(InventoryPage {
            objects: Vec::new(),
            removals: Vec::new(),
            next_page_cursor: None,
            next_change_cursor: Some(rec_core::SyncCursor::new("retry-cursor")),
            progress: None,
        });
        service.registry.register(drive.id, runtime);
        let outcome = service
            .handle(IntegrationRequest {
                id: 13,
                slice: IntegrationSlice::GoogleDrive,
                event: MusicManagementEvent::RetryStorage(drive.id.to_string()),
            })
            .await
            .unwrap();
        assert!(outcome.view.is_some());
        assert!(
            AssetRepository::latest_finalized_inventory_generation(catalog.as_ref(), drive.id)
                .await
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn handler_claims_only_google_drive_slice() {
        let registry = DriveRuntimeRegistry::new(Arc::new(NeverFactory));
        assert!(registry.runtimes.read().unwrap().is_empty());
        let error = sync_error_for(&GoogleDriveServiceError::Verification("bad hash".into()));
        assert_eq!(error.kind, SyncErrorKind::Integrity);
    }
}
