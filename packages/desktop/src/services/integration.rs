//! Provider-neutral runtime contracts and the desktop Stage 3 composition root.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::Utc;
use music_storage_local::LocalMusicStorage;
use rec_core::{
    AssetLocationFilter, AssetRepository, BoxFuture, CatalogSource, CatalogSourceKind,
    MusicAssetStore, MusicStorageKind, MusicStorageRepository, RepositoryPageRequest,
    SourceRepository, StorageRole, StorageVerificationStatus,
};
use rec_persistence::TursoCatalog;
use rec_sync::{SnapshotTrigger as SnapshotReason, VerificationCadence, VerificationRequest};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Notify};
use ui::{CleanupItemView, MusicManagementView};
use uuid::Uuid;

use super::{
    backup::{BackupProviderRegistry, BackupService, RetentionReview},
    google_drive::{
        DriveFolderSelection, DriveRuntimeFactory, DriveRuntimeRegistry, GoogleCredential,
        GoogleDriveFolderSelector, GoogleDrivePaths, GoogleDriveProvisioner, GoogleDriveService,
        GoogleDriveServiceError, GoogleOAuth, GoogleOAuthCallback, GoogleOAuthRequest,
        ProvisionedDrive, SelectedDriveFolder,
    },
    local_library::{
        LocalLibraryError, LocalLibraryService, LocalSelectionProvider, LocalSourceSelection,
        SelectionFuture,
    },
    profile_manager::{
        AssumeOwnershipConfirmation, HeartbeatHandle, ManagerIdentity, OwnershipLease,
        OwnershipStatus, ProfileManager, ProfileManagerError, TakeoverReason,
    },
    reconciliation_actions::ReconciliationActionsService,
    recovery::{
        OwnershipHandoff, RecoveryService, RestoreOptions, RestoreReport, SnapshotListing,
        SnapshotTransferConfirmation,
    },
    scheduler::{
        start_scheduler, ScheduledTask, SchedulerConfig, SchedulerError, SchedulerHandle,
        SchedulerTaskError, SchedulerTasks, TaskOutcome,
    },
    soundcloud_catalog::{
        ApiSoundCloudBrokerClient, OAuthUserAgent, ProductionSoundCloudConnectorFactory,
        SoundCloudCatalogService, SoundCloudOAuthClientConfig,
    },
};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

pub type IntegrationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<IntegrationOutcome, IntegrationError>> + Send + 'a>>;

/// A vertical slice implements this interface and registers exactly one owner.
pub trait IntegrationSliceHandler: Send + Sync {
    fn slice(&self) -> IntegrationSlice;
    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a>;
}

/// Registry used by the scheduler/composition root to locate slice handlers.
#[derive(Default)]
pub struct ProviderRegistry {
    handlers: BTreeMap<IntegrationSlice, Arc<dyn IntegrationSliceHandler>>,
}

impl ProviderRegistry {
    pub fn register(
        &mut self,
        handler: Arc<dyn IntegrationSliceHandler>,
    ) -> Option<Arc<dyn IntegrationSliceHandler>> {
        self.handlers.insert(handler.slice(), handler)
    }

    pub fn handler(&self, slice: IntegrationSlice) -> Option<Arc<dyn IntegrationSliceHandler>> {
        self.handlers.get(&slice).cloned()
    }

    pub fn contains(&self, slice: IntegrationSlice) -> bool {
        self.handlers.contains_key(&slice)
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// Opaque credential address. Provider secrets never enter UI state or events.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialKey {
    pub provider: String,
    pub account: String,
}

/// Secret bytes with redacted debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

pub type CredentialFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, IntegrationError>> + Send + 'a>>;

/// OS-backed implementations store opaque provider credentials behind this seam.
pub trait CredentialStore: Send + Sync {
    fn load<'a>(&'a self, key: &'a CredentialKey) -> CredentialFuture<'a, Option<Secret>>;
    fn store<'a>(&'a self, key: &'a CredentialKey, secret: Secret) -> CredentialFuture<'a, ()>;
    fn delete<'a>(&'a self, key: &'a CredentialKey) -> CredentialFuture<'a, ()>;
}

/// Stable reference to an asset location without exposing provider SDK types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetReference {
    pub storage_id: String,
    pub asset_id: String,
}

/// A playable local path or URL and whether it is a disposable cache entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAsset {
    pub uri: String,
    pub cached: bool,
}

pub type AssetFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedAsset, IntegrationError>> + Send + 'a>>;

/// Storage slices implement resolution; playback depends only on this interface.
pub trait AssetResolver: Send + Sync {
    fn resolve<'a>(&'a self, asset: &'a AssetReference) -> AssetFuture<'a>;
}

#[derive(Clone)]
pub enum DesktopStartup {
    Ready(Arc<DesktopIntegration>),
    OwnershipBlocked(OwnershipTakeoverOffer),
}

#[derive(Clone)]
pub struct OwnershipTakeoverOffer {
    manager: ProfileManager,
    confirmation: AssumeOwnershipConfirmation,
    paths: DesktopPaths,
    pub owner_device: String,
    pub owner_session: Uuid,
}

impl OwnershipTakeoverOffer {
    pub async fn assume(&self) -> Result<Arc<DesktopIntegration>, IntegrationError> {
        let previous_device_id = self.confirmation.current_owner().device_id.clone();
        let lease = self
            .manager
            .assume_ownership(&self.confirmation, true, Utc::now())
            .map_err(ownership_error)?;
        DesktopIntegration::open_owned(
            self.paths.clone(),
            self.manager.clone(),
            lease,
            Some(previous_device_id),
        )
        .await
    }
}

/// Live Stage 4 runtime shared by all desktop views.
pub struct DesktopIntegration {
    catalog: Arc<TursoCatalog>,
    registry: Arc<ProviderRegistry>,
    projector: Arc<LocalLibraryService>,
    soundcloud: Arc<SoundCloudCatalogService>,
    backup: Arc<BackupService>,
    recovery: Arc<RecoveryService>,
    profile_manager: ProfileManager,
    ownership_lease: Mutex<OwnershipLease>,
    previous_owner_device: Option<String>,
    drive_registry: Arc<DriveRuntimeRegistry>,
    backup_providers: Arc<BackupProviderRegistry>,
    projection_lock: Mutex<()>,
    asset_resolver: Arc<dyn AssetResolver>,
    wakes: BTreeMap<IntegrationSlice, Arc<Notify>>,
    shutdown: AtomicBool,
    shutdown_notify: Notify,
    active_operations: AtomicUsize,
    operations_drained: Notify,
    scheduler: Mutex<Option<SchedulerHandle>>,
    heartbeat: Mutex<Option<HeartbeatHandle>>,
    startup_errors: Vec<IntegrationError>,
}

impl DesktopIntegration {
    /// Acquires the one-active-manager lease before opening or migrating the catalog.
    pub async fn open() -> Result<DesktopStartup, IntegrationError> {
        let paths = DesktopPaths::load()?;
        let manager = ProfileManager::new(
            paths.root.clone(),
            ManagerIdentity::new("default", device_identity()),
            Duration::from_secs(30),
        )
        .map_err(ownership_error)?;
        let now = Utc::now();
        let previous_owner_device = match manager.status(now).map_err(ownership_error)? {
            OwnershipStatus::Stale(lease) => Some(lease.device_id),
            _ => None,
        };
        match manager.acquire(now) {
            Ok(lease) => Self::open_owned(paths, manager, lease, previous_owner_device)
                .await
                .map(DesktopStartup::Ready),
            Err(ProfileManagerError::AlreadyOwned { .. }) => {
                let confirmation = manager
                    .prepare_assume_ownership(Utc::now())
                    .map_err(ownership_error)?;
                let owner_device = confirmation.current_owner().device_id.clone();
                let owner_session = confirmation.current_owner().session_id;
                Ok(DesktopStartup::OwnershipBlocked(OwnershipTakeoverOffer {
                    manager,
                    confirmation,
                    paths,
                    owner_device,
                    owner_session,
                }))
            }
            Err(error) => Err(ownership_error(error)),
        }
    }

    async fn open_owned(
        paths: DesktopPaths,
        manager: ProfileManager,
        lease: OwnershipLease,
        previous_owner_device: Option<String>,
    ) -> Result<Arc<Self>, IntegrationError> {
        let result =
            Self::compose_owned(paths, manager.clone(), lease, previous_owner_device).await;
        if result.is_err() {
            let _ = manager.release();
        }
        result
    }

    async fn compose_owned(
        paths: DesktopPaths,
        manager: ProfileManager,
        lease: OwnershipLease,
        previous_owner_device: Option<String>,
    ) -> Result<Arc<Self>, IntegrationError> {
        let catalog = Arc::new(TursoCatalog::open(&paths.catalog).await.map_err(|error| {
            startup_error(format!(
                "Could not open the music catalog at {}: {error}",
                paths.catalog.display()
            ))
        })?);
        let recovery = Arc::new(RecoveryService::new(
            catalog.clone(),
            paths.snapshots.clone(),
        ));
        let mut startup_errors = Vec::new();
        if let Some(progress) = recovery.recover_interrupted().await.map_err(|error| {
            startup_error(format!("Could not reconcile interrupted recovery: {error}"))
        })? {
            startup_errors.push(startup_error(progress.message));
        }
        recovery
            .persist_catalog_authority(&manager, &lease)
            .await
            .map_err(|error| {
                startup_error(format!("Could not persist profile authority: {error}"))
            })?;

        let credentials = production_credentials();
        let local = Arc::new(LocalLibraryService::new(
            catalog.clone(),
            production_local_selection(),
        ));
        match adopt_migrated_local_library(catalog.as_ref(), local.as_ref()).await {
            Ok(StartupAdoption::NotNeeded | StartupAdoption::Adopted) => {}
            Ok(StartupAdoption::NeedsSelection(message)) => {
                startup_errors.push(provider_error(IntegrationSlice::LocalLibrary, message))
            }
            Err(error) => startup_errors.push(error),
        }

        let soundcloud = Arc::new(soundcloud_service(catalog.clone(), credentials.clone())?);
        let google_oauth = production_google_oauth();
        let drive_factory = production_drive_factory(
            credentials.clone(),
            google_oauth.clone(),
            paths.drive_cache.clone(),
        );
        let drive_registry = Arc::new(DriveRuntimeRegistry::new(drive_factory));
        let drive = Arc::new(GoogleDriveService::new(
            catalog.clone(),
            credentials.clone(),
            production_google_provisioner(google_oauth, credentials)?,
            drive_registry.clone(),
            GoogleDrivePaths {
                cache_root: paths.drive_cache,
                staging_root: paths.staging.join("drive"),
            },
        ));
        let backup_providers = Arc::new(BackupProviderRegistry::default());
        let backup = Arc::new(BackupService::new(
            catalog.clone(),
            backup_providers.clone(),
            paths.staging.join("backup"),
        ));
        reconcile_staging(&paths.staging).map_err(|error| {
            startup_error(format!("Could not reconcile staging directory: {error}"))
        })?;
        let reconciliation = Arc::new(ReconciliationActionsService::new(catalog.clone()));

        let mut registry = ProviderRegistry::default();
        registry.register(local.clone());
        registry.register(soundcloud.clone());
        registry.register(drive.clone());
        registry.register(backup.clone());
        registry.register(reconciliation);

        initialize_storage_registries(
            catalog.as_ref(),
            drive_registry.clone(),
            backup_providers.clone(),
            &mut startup_errors,
        )
        .await;

        let resolver = Arc::new(DesktopAssetResolver {
            catalog: catalog.clone(),
            drive,
        });
        Ok(Arc::new(Self::from_parts(
            catalog,
            Arc::new(registry),
            local,
            soundcloud,
            backup,
            recovery,
            manager,
            lease,
            previous_owner_device,
            drive_registry,
            backup_providers,
            resolver,
            startup_errors,
        )))
    }

    fn from_parts(
        catalog: Arc<TursoCatalog>,
        registry: Arc<ProviderRegistry>,
        projector: Arc<LocalLibraryService>,
        soundcloud: Arc<SoundCloudCatalogService>,
        backup: Arc<BackupService>,
        recovery: Arc<RecoveryService>,
        profile_manager: ProfileManager,
        ownership_lease: OwnershipLease,
        previous_owner_device: Option<String>,
        drive_registry: Arc<DriveRuntimeRegistry>,
        backup_providers: Arc<BackupProviderRegistry>,
        asset_resolver: Arc<dyn AssetResolver>,
        startup_errors: Vec<IntegrationError>,
    ) -> Self {
        let wakes = IntegrationSlice::ALL
            .into_iter()
            .map(|slice| (slice, Arc::new(Notify::new())))
            .collect();
        Self {
            catalog,
            registry,
            projector,
            soundcloud,
            backup,
            recovery,
            profile_manager,
            ownership_lease: Mutex::new(ownership_lease),
            previous_owner_device,
            drive_registry,
            backup_providers,
            projection_lock: Mutex::new(()),
            asset_resolver,
            wakes,
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            active_operations: AtomicUsize::new(0),
            operations_drained: Notify::new(),
            scheduler: Mutex::new(None),
            heartbeat: Mutex::new(None),
            startup_errors,
        }
    }

    pub fn catalog(&self) -> &Arc<TursoCatalog> {
        &self.catalog
    }

    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    pub fn asset_resolver(&self) -> Arc<dyn AssetResolver> {
        self.asset_resolver.clone()
    }

    pub fn startup_errors(&self) -> &[IntegrationError] {
        &self.startup_errors
    }

    pub fn wake(&self, slice: IntegrationSlice) {
        if let Some(wake) = self.wakes.get(&slice) {
            wake.notify_one();
        }
    }

    pub async fn wait_for_work(&self, slice: IntegrationSlice) -> bool {
        if self.is_shutdown() {
            return false;
        }
        if let Some(wake) = self.wakes.get(&slice) {
            wake.notified().await;
        }
        !self.is_shutdown()
    }

    pub async fn execute(
        &self,
        request: IntegrationRequest,
    ) -> Result<IntegrationOutcome, IntegrationError> {
        let _operation = self.begin_operation(&request)?;
        if self.is_shutdown() {
            return Err(IntegrationError::for_request(
                &request,
                "Integration runtime is shutting down",
                true,
            ));
        }
        self.require_ownership(Some(&request))?;
        if matches!(
            request.event,
            ui::MusicManagementEvent::PreparePrimaryMigration(_)
        ) {
            self.record_snapshot(SnapshotReason::BeforePrimaryMigration, None)
                .await
                .map_err(|error| {
                    IntegrationError::for_request(&request, error.message, error.retryable)
                })?;
        }
        let handler = self.registry.handler(request.slice).ok_or_else(|| {
            IntegrationError::for_request(
                &request,
                format!("No handler is registered for {:?}", request.slice),
                false,
            )
        })?;
        let mut outcome = handler.handle(request.clone()).await?;
        if let Some(reason) = snapshot_reason_for_event(&request.event) {
            self.record_snapshot(reason, None).await.map_err(|error| {
                IntegrationError::for_request(&request, error.message, error.retryable)
            })?;
        }
        self.complete_cross_slice_handoffs(&request, &mut outcome)
            .await;

        // Handler projections may overlap. Serialize the final catalog projection so a
        // slower slice cannot overwrite newer data with a stale full view.
        let _projection = self.projection_lock.lock().await;
        if self.is_shutdown() {
            return Err(IntegrationError::for_request(
                &request,
                "Integration runtime stopped before the dashboard refresh completed",
                true,
            ));
        }
        let mut view =
            self.projector.project_view().await.map_err(|error| {
                IntegrationError::for_request(&request, error.to_string(), true)
            })?;
        if matches!(
            request.event,
            ui::MusicManagementEvent::StartCleanupReview
                | ui::MusicManagementEvent::ConfirmCleanup(_)
        ) {
            let review = self
                .backup
                .review_retention(None, Utc::now())
                .await
                .map_err(|error| {
                    IntegrationError::for_request(&request, error.to_string(), true)
                })?;
            view.cleanup_candidates = retention_candidate_views(&review);
        }
        outcome.view = Some(view);
        Ok(outcome)
    }

    async fn complete_cross_slice_handoffs(
        &self,
        request: &IntegrationRequest,
        outcome: &mut IntegrationOutcome,
    ) {
        let storage_changed = matches!(
            request.event,
            ui::MusicManagementEvent::AddBackup(_)
                | ui::MusicManagementEvent::PreparePrimaryMigration(_)
        );
        if storage_changed {
            let mut registration_errors = Vec::new();
            initialize_storage_registries(
                self.catalog.as_ref(),
                self.drive_registry.clone(),
                self.backup_providers.clone(),
                &mut registration_errors,
            )
            .await;
            if !registration_errors.is_empty() {
                append_outcome_message(
                    outcome,
                    format!(
                        "Provider refresh deferred for {} storage(s): {}",
                        registration_errors.len(),
                        registration_errors
                            .into_iter()
                            .map(|error| error.message)
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                );
            }
        }

        let track_id = match &request.event {
            ui::MusicManagementEvent::CopyToPrimary(track_id) => Uuid::parse_str(track_id).ok(),
            ui::MusicManagementEvent::PreparePrimaryMigration(_)
            | ui::MusicManagementEvent::ApproveExternalChange { .. } => None,
            _ => return,
        };
        let status =
            plan_catalog_verified_writes(self.catalog.as_ref(), self.backup.as_ref(), track_id)
                .await;
        append_outcome_message(outcome, status.message());

        // One wake gives already-due durable work a chance to advance. Deferred jobs remain
        // persisted for the Stage 4 scheduler; this never changes primary-write success.
        self.wake(IntegrationSlice::Backup);
        if let Err(error) = self
            .backup
            .run_worker_once("desktop-stage3-handoff", Utc::now())
            .await
        {
            append_outcome_message(
                outcome,
                format!("Backup work remains queued/deferred: {error}"),
            );
        }
    }

    pub async fn start_background(self: &Arc<Self>) -> Result<(), IntegrationError> {
        self.require_ownership(None)?;
        {
            let mut heartbeat = self.heartbeat.lock().await;
            if heartbeat.is_none() {
                *heartbeat = Some(
                    self.profile_manager
                        .start_heartbeat(Duration::from_secs(10)),
                );
            }
        }
        let mut scheduler = self.scheduler.lock().await;
        if scheduler.is_none() {
            let tasks = Arc::new(RuntimeSchedulerTasks {
                catalog: self.catalog.clone(),
                backup: self.backup.clone(),
                soundcloud: self.soundcloud.clone(),
                recovery: self.recovery.clone(),
                profile_manager: self.profile_manager.clone(),
            });
            *scheduler = Some(start_scheduler(tasks, SchedulerConfig::default()));
        }
        Ok(())
    }

    pub fn recovery_snapshots(&self) -> Result<Vec<SnapshotListing>, IntegrationError> {
        let _operation = self.begin_unscoped_operation("Recovery listing")?;
        self.require_ownership(None)?;
        self.recovery
            .list_snapshots()
            .map_err(|error| recovery_error(error.to_string()))
    }

    pub async fn prepare_snapshot_transfer(
        &self,
        listing: &SnapshotListing,
    ) -> Result<Option<SnapshotTransferConfirmation>, IntegrationError> {
        let _operation = self.begin_unscoped_operation("Snapshot ownership transfer")?;
        self.require_ownership(None)?;
        let Some(observed) = listing.authority.as_ref() else {
            return Ok(None);
        };
        let current = self
            .recovery
            .catalog_authority()
            .await
            .map_err(|error| recovery_error(error.to_string()))?;
        let transfer_required = current.as_ref().is_none_or(|current| {
            observed.profile_id != current.profile_id
                || observed.manager_device != current.manager_device
                || observed.ownership_generation > current.ownership_generation
        });
        if !transfer_required {
            return Ok(None);
        }
        self.recovery
            .prepare_snapshot_transfer(listing, &self.profile_manager, Utc::now())
            .map(Some)
            .map_err(|error| recovery_error(error.to_string()))
    }

    pub fn cancel_snapshot_transfer(
        &self,
        confirmation: &SnapshotTransferConfirmation,
    ) -> Result<bool, IntegrationError> {
        self.recovery
            .cancel_snapshot_transfer(confirmation)
            .map_err(|error| recovery_error(error.to_string()))
    }

    pub async fn dry_run_restore(&self, path: PathBuf) -> Result<RestoreReport, IntegrationError> {
        self.restore_snapshot_internal(path, true, None).await
    }

    pub async fn restore_snapshot(
        &self,
        path: PathBuf,
        dry_run: bool,
    ) -> Result<RestoreReport, IntegrationError> {
        self.restore_snapshot_internal(path, dry_run, None).await
    }

    pub async fn restore_snapshot_with_confirmation(
        &self,
        path: PathBuf,
        confirmation: SnapshotTransferConfirmation,
    ) -> Result<RestoreReport, IntegrationError> {
        self.restore_snapshot_internal(path, false, Some(confirmation))
            .await
    }

    async fn restore_snapshot_internal(
        &self,
        path: PathBuf,
        dry_run: bool,
        snapshot_confirmation: Option<SnapshotTransferConfirmation>,
    ) -> Result<RestoreReport, IntegrationError> {
        let _operation = self.begin_unscoped_operation("Catalog recovery")?;
        self.require_ownership(None)?;
        let lease = self.ownership_lease.lock().await.clone();
        let ownership_handoff = self
            .previous_owner_device
            .clone()
            .and_then(|previous_device_id| {
                matches!(
                    lease.takeover_reason,
                    TakeoverReason::ConfirmedHandoff | TakeoverReason::ExpiredLease
                )
                .then_some(OwnershipHandoff {
                    previous_device_id,
                    lease,
                })
            });
        let restore = self.recovery.restore(
            path,
            RestoreOptions {
                dry_run,
                ownership_handoff,
                snapshot_confirmation,
            },
            Some(&self.profile_manager),
        );
        tokio::select! {
            biased;
            _ = self.shutdown_notify.notified() => {
                Err(recovery_error("Recovery was cancelled by application shutdown"))
            }
            result = restore => result.map_err(|error| recovery_error(error.to_string())),
        }
    }

    pub async fn rollback_restore(
        &self,
        pre_restore_snapshot: PathBuf,
    ) -> Result<RestoreReport, IntegrationError> {
        self.restore_snapshot(pre_restore_snapshot, false).await
    }

    pub async fn initial_view(&self) -> Result<MusicManagementView, IntegrationError> {
        let _projection = self.projection_lock.lock().await;
        self.projector
            .project_view()
            .await
            .map_err(|error| startup_error(format!("Could not project the catalog: {error}")))
    }

    pub fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
        for wake in self.wakes.values() {
            wake.notify_waiters();
        }
        if let Ok(scheduler) = self.scheduler.try_lock() {
            if let Some(scheduler) = scheduler.as_ref() {
                scheduler.cancel();
            }
        }
    }

    pub async fn shutdown(&self) -> Result<(), IntegrationError> {
        self.shutdown_with_timeouts(Duration::from_secs(35), Duration::from_secs(10))
            .await
    }

    async fn shutdown_with_timeouts(
        &self,
        scheduler_timeout: Duration,
        operation_timeout: Duration,
    ) -> Result<(), IntegrationError> {
        self.begin_shutdown();
        let mut errors = Vec::new();
        if let Some(scheduler) = self.scheduler.lock().await.take() {
            match tokio::time::timeout(scheduler_timeout, scheduler.shutdown()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(scheduler_error(error).message),
                Err(_) => errors.push(format!(
                    "Scheduler shutdown exceeded {} seconds",
                    scheduler_timeout.as_secs_f64()
                )),
            }
        }
        if let Some(heartbeat) = self.heartbeat.lock().await.take() {
            heartbeat.stop();
        }
        errors.extend(
            drain_operations_and_release(
                &self.active_operations,
                &self.operations_drained,
                &self.profile_manager,
                operation_timeout,
            )
            .await,
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(startup_error(format!(
                "Graceful shutdown completed with errors: {}",
                errors.join("; ")
            )))
        }
    }

    fn begin_operation(
        &self,
        request: &IntegrationRequest,
    ) -> Result<OperationPermit<'_>, IntegrationError> {
        if self.is_shutdown() {
            return Err(IntegrationError::for_request(
                request,
                "Integration runtime is shutting down",
                true,
            ));
        }
        self.active_operations.fetch_add(1, Ordering::AcqRel);
        if self.is_shutdown() {
            self.finish_operation();
            return Err(IntegrationError::for_request(
                request,
                "Integration runtime is shutting down",
                true,
            ));
        }
        Ok(OperationPermit { runtime: self })
    }

    fn begin_unscoped_operation(
        &self,
        label: &'static str,
    ) -> Result<OperationPermit<'_>, IntegrationError> {
        if self.is_shutdown() {
            return Err(startup_error(format!(
                "{label} cannot start while the integration runtime is shutting down"
            )));
        }
        self.active_operations.fetch_add(1, Ordering::AcqRel);
        if self.is_shutdown() {
            self.finish_operation();
            return Err(startup_error(format!(
                "{label} was cancelled because shutdown started"
            )));
        }
        Ok(OperationPermit { runtime: self })
    }

    fn finish_operation(&self) {
        if self.active_operations.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.operations_drained.notify_waiters();
        }
    }

    fn require_ownership(
        &self,
        request: Option<&IntegrationRequest>,
    ) -> Result<(), IntegrationError> {
        let owns = self
            .profile_manager
            .owns_profile(Utc::now())
            .map_err(ownership_error)?;
        if owns {
            return Ok(());
        }
        self.begin_shutdown();
        Err(request.map_or_else(
            || ownership_error(ProfileManagerError::OwnershipLost),
            |request| {
                IntegrationError::for_request(
                    request,
                    "Profile ownership was lost; catalog mutation has been stopped",
                    false,
                )
            },
        ))
    }

    async fn record_snapshot(
        &self,
        reason: SnapshotReason,
        source_run_id: Option<Uuid>,
    ) -> Result<(), IntegrationError> {
        let now = Utc::now();
        self.backup
            .record_snapshot_trigger(reason, source_run_id, now)
            .await
            .map_err(|error| provider_error(IntegrationSlice::Backup, error.to_string()))?;
        let snapshot = self.catalog.export_snapshot().await.map_err(|error| {
            recovery_error(format!("Could not export recovery snapshot: {error}"))
        })?;
        self.recovery
            .store_snapshot(&snapshot)
            .map_err(|error| recovery_error(error.to_string()))?;
        if let Some(scheduler) = self.scheduler.lock().await.as_ref() {
            scheduler.wake();
        }
        Ok(())
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

struct OperationPermit<'a> {
    runtime: &'a DesktopIntegration,
}

impl Drop for OperationPermit<'_> {
    fn drop(&mut self) {
        self.runtime.finish_operation();
    }
}

async fn drain_operations_and_release(
    active_operations: &AtomicUsize,
    operations_drained: &Notify,
    profile_manager: &ProfileManager,
    operation_timeout: Duration,
) -> Vec<String> {
    let mut errors = Vec::new();
    let wait_for_operations = async {
        while active_operations.load(Ordering::Acquire) != 0 {
            operations_drained.notified().await;
        }
    };
    if tokio::time::timeout(operation_timeout, wait_for_operations)
        .await
        .is_err()
    {
        errors.push(format!(
            "Integration operations did not stop within {} seconds",
            operation_timeout.as_secs_f64()
        ));
    }
    if let Err(error) = profile_manager.release() {
        errors.push(ownership_error(error).message);
    }
    errors
}

struct RuntimeSchedulerTasks {
    catalog: Arc<TursoCatalog>,
    backup: Arc<BackupService>,
    soundcloud: Arc<SoundCloudCatalogService>,
    recovery: Arc<RecoveryService>,
    profile_manager: ProfileManager,
}

impl SchedulerTasks for RuntimeSchedulerTasks {
    fn run(
        &self,
        task: ScheduledTask,
        now: chrono::DateTime<Utc>,
    ) -> BoxFuture<'_, Result<TaskOutcome, SchedulerTaskError>> {
        Box::pin(async move {
            if !self
                .profile_manager
                .owns_profile(now)
                .map_err(|error| SchedulerTaskError::Failed(error.to_string()))?
            {
                return Err(SchedulerTaskError::Failed(
                    "profile ownership was lost".into(),
                ));
            }
            match task {
                ScheduledTask::DurableJobs => self
                    .backup
                    .run_worker_once("desktop-stage4", now)
                    .await
                    .map(|event| event.map_or(TaskOutcome::IDLE, |_| TaskOutcome::progress()))
                    .map_err(scheduler_backup_error),
                ScheduledTask::SoundCloudSync => {
                    let sources = list_sources(self.catalog.as_ref())
                        .await
                        .map_err(SchedulerTaskError::Unavailable)?;
                    let mut synced = false;
                    for source in sources.into_iter().filter(|source| {
                        source.enabled && source.kind == CatalogSourceKind::SoundCloud
                    }) {
                        self.soundcloud
                            .sync(source.id)
                            .await
                            .map_err(|error| scheduler_integration_error(error))?;
                        synced = true;
                    }
                    if synced {
                        persist_recovery_snapshot(
                            self.catalog.as_ref(),
                            self.backup.as_ref(),
                            self.recovery.as_ref(),
                            SnapshotReason::SuccessfulSync,
                            now,
                        )
                        .await?;
                    }
                    Ok(TaskOutcome {
                        made_progress: synced,
                        next_due: None,
                    })
                }
                ScheduledTask::AutomaticBackup | ScheduledTask::ScheduledBackup => self
                    .backup
                    .handoff_verified_primary_assets(None, now)
                    .await
                    .map(|handoff| TaskOutcome {
                        made_progress: handoff.queued_jobs > 0,
                        next_due: None,
                    })
                    .map_err(scheduler_backup_error),
                ScheduledTask::DailyVerification => {
                    let requests = verification_requests(self.catalog.as_ref())
                        .await
                        .map_err(SchedulerTaskError::Unavailable)?;
                    let made_progress = !requests.is_empty();
                    self.backup
                        .schedule_verification(requests, now, 7)
                        .await
                        .map_err(scheduler_backup_error)?;
                    Ok(TaskOutcome {
                        made_progress,
                        next_due: None,
                    })
                }
                ScheduledTask::MonthlyInventory => self
                    .backup
                    .schedule_monthly_inventories(now)
                    .await
                    .map(|run| TaskOutcome {
                        made_progress: run.is_some(),
                        next_due: None,
                    })
                    .map_err(scheduler_backup_error),
                ScheduledTask::DailySnapshot => {
                    persist_recovery_snapshot(
                        self.catalog.as_ref(),
                        self.backup.as_ref(),
                        self.recovery.as_ref(),
                        SnapshotReason::Daily,
                        now,
                    )
                    .await?;
                    let run = self
                        .backup
                        .create_due_snapshot(now)
                        .await
                        .map_err(scheduler_backup_error)?;
                    Ok(TaskOutcome {
                        made_progress: run.is_some(),
                        next_due: None,
                    })
                }
            }
        })
    }

    fn checkpoint(
        &self,
        now: chrono::DateTime<Utc>,
    ) -> BoxFuture<'_, Result<(), SchedulerTaskError>> {
        Box::pin(async move {
            self.backup
                .create_due_snapshot(now)
                .await
                .map_err(scheduler_backup_error)?;
            Ok(())
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BackupHandoffStatus {
    planned_assets: usize,
    queued_jobs: usize,
    deferred_targets: usize,
    warnings: Vec<String>,
}

impl BackupHandoffStatus {
    fn message(&self) -> String {
        if self.planned_assets == 0 && self.warnings.is_empty() {
            return "Backup handoff complete: no verified primary assets required planning".into();
        }
        let mut message = format!(
            "Backup handoff: {} verified asset(s), {} durable job(s) queued, {} unavailable target(s) deferred",
            self.planned_assets, self.queued_jobs, self.deferred_targets
        );
        if !self.warnings.is_empty() {
            message.push_str("; non-blocking issues: ");
            message.push_str(&self.warnings.join("; "));
        }
        message
    }
}

async fn plan_catalog_verified_writes(
    catalog: &TursoCatalog,
    backup: &BackupService,
    track_id: Option<Uuid>,
) -> BackupHandoffStatus {
    let mut status = BackupHandoffStatus::default();
    let asset_ids = match track_id {
        Some(track_id) => {
            match AssetRepository::list_originals_for_track(catalog, track_id).await {
                Ok(assets) => assets
                    .into_iter()
                    .map(|asset| asset.id())
                    .collect::<Vec<_>>(),
                Err(error) => {
                    status
                        .warnings
                        .push(format!("could not resolve copied track assets: {error}"));
                    return status;
                }
            }
        }
        None => Vec::new(),
    };
    if track_id.is_some() && asset_ids.is_empty() {
        status
            .warnings
            .push("the copied track has no original asset to hand off".into());
        return status;
    }

    let targets: Vec<Option<Uuid>> = if track_id.is_some() {
        asset_ids.into_iter().map(Some).collect()
    } else {
        vec![None]
    };
    for asset_id in targets {
        match backup
            .handoff_verified_primary_assets(asset_id, Utc::now())
            .await
        {
            Ok(handoff) => {
                status.planned_assets += handoff.considered_assets;
                status.queued_jobs += handoff.queued_jobs;
                status.deferred_targets += handoff.deferred_targets;
            }
            Err(error) => status
                .warnings
                .push(format!("backup planning remains deferred: {error}")),
        }
    }
    status
}

fn retention_candidate_views(review: &RetentionReview) -> Vec<CleanupItemView> {
    let mut rows = review
        .items
        .iter()
        .flat_map(|item| {
            item.evaluation
                .cleanup_candidates
                .iter()
                .filter_map(|candidate| {
                    let key = format!(
                        "{}|{}:{}",
                        item.storage_id, candidate.object_id, candidate.version_id
                    );
                    if item.approved_candidate_keys.contains(&key) {
                        return None;
                    }
                    let age = candidate
                        .modified_at
                        .map(|modified| {
                            format!(
                                "{} days old",
                                review
                                    .generated_at
                                    .signed_duration_since(modified)
                                    .num_days()
                                    .max(0)
                            )
                        })
                        .unwrap_or_else(|| "Age unavailable".into());
                    Some(CleanupItemView {
                        id: key,
                        name: format!("{} · version {}", candidate.object_id, candidate.version_id),
                        age,
                        reason: candidate.reason.clone(),
                    })
                })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    rows
}

fn append_outcome_message(outcome: &mut IntegrationOutcome, message: impl Into<String>) {
    let detail = message.into();
    outcome.message = Some(match outcome.message.take() {
        Some(message) if !message.is_empty() => format!("{message}. {detail}"),
        _ => detail,
    });
}

fn snapshot_reason_for_event(event: &ui::MusicManagementEvent) -> Option<SnapshotReason> {
    use ui::MusicManagementEvent;
    match event {
        MusicManagementEvent::PreparePrimaryMigration(_) => None,
        MusicManagementEvent::AddSource(ui::ProviderChoice::SoundCloud)
        | MusicManagementEvent::SyncSource(_)
        | MusicManagementEvent::RetrySyncRun { .. } => Some(SnapshotReason::SuccessfulSync),
        MusicManagementEvent::ReviewMatch(_)
        | MusicManagementEvent::ConfirmMatch(_)
        | MusicManagementEvent::RejectMatch(_)
        | MusicManagementEvent::AssociateLocalFile { .. }
        | MusicManagementEvent::AssociateUnknownFile { .. }
        | MusicManagementEvent::ApproveExternalChange { .. }
        | MusicManagementEvent::RetryReconciliationResult { .. }
        | MusicManagementEvent::ImportUnknownFile(_)
        | MusicManagementEvent::IgnoreUnknownFile(_) => Some(SnapshotReason::ReconciliationChange),
        MusicManagementEvent::SetTrackFilter(_) => None,
        _ => Some(SnapshotReason::CatalogEdit),
    }
}

async fn list_sources(catalog: &TursoCatalog) -> Result<Vec<CatalogSource>, String> {
    let mut cursor = None;
    let mut sources = Vec::new();
    loop {
        let page = SourceRepository::list(
            catalog,
            RepositoryPageRequest {
                cursor,
                limit: Some(500),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        sources.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(sources),
        }
    }
}

async fn verification_requests(catalog: &TursoCatalog) -> Result<Vec<VerificationRequest>, String> {
    let backup_ids = MusicStorageRepository::list(catalog)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|storage| storage.enabled && storage.role == StorageRole::Backup)
        .map(|storage| storage.id)
        .collect::<BTreeSet<_>>();
    let mut request = RepositoryPageRequest {
        cursor: None,
        limit: Some(500),
    };
    let mut locations = Vec::new();
    loop {
        let page = AssetRepository::list_locations(
            catalog,
            AssetLocationFilter {
                asset_id: None,
                storage_id: None,
            },
            request.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;
        locations.extend(page.items.into_iter().filter(|location| {
            backup_ids.contains(&location.storage_id)
                && location.verification_status == StorageVerificationStatus::Verified
        }));
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request.cursor = Some(cursor);
    }
    locations.sort_by(|left, right| {
        (left.storage_id, &left.object_id).cmp(&(right.storage_id, &right.object_id))
    });
    Ok(locations
        .into_iter()
        .enumerate()
        .map(|(ordinal, location)| VerificationRequest {
            cadence: VerificationCadence::DailyRotatingSample,
            asset_id: location.asset_id,
            storage_id: location.storage_id,
            object_id: location.object_id.clone(),
            object_key: Some(location.object_id),
            expected_checksum: location
                .expected_hash
                .as_ref()
                .map(|hash| format!("sha256:{}", hash.value())),
            known_provider_version: location.provider_version.clone(),
            observed_provider_version: location.provider_version,
            inventory_ordinal: ordinal as u64,
        })
        .collect())
}

async fn persist_recovery_snapshot(
    catalog: &TursoCatalog,
    backup: &BackupService,
    recovery: &RecoveryService,
    reason: SnapshotReason,
    now: chrono::DateTime<Utc>,
) -> Result<(), SchedulerTaskError> {
    backup
        .record_snapshot_trigger(reason, None, now)
        .await
        .map_err(scheduler_backup_error)?;
    let snapshot = catalog
        .export_snapshot()
        .await
        .map_err(|error| SchedulerTaskError::Unavailable(error.to_string()))?;
    recovery
        .store_snapshot(&snapshot)
        .map_err(|error| SchedulerTaskError::Failed(error.to_string()))?;
    Ok(())
}

fn scheduler_backup_error(error: super::backup::BackupError) -> SchedulerTaskError {
    if error.retryable() {
        SchedulerTaskError::Unavailable(error.to_string())
    } else {
        SchedulerTaskError::Failed(error.to_string())
    }
}

fn scheduler_integration_error(error: IntegrationError) -> SchedulerTaskError {
    if error.retryable {
        SchedulerTaskError::Unavailable(error.message)
    } else {
        SchedulerTaskError::Failed(error.message)
    }
}

fn scheduler_error(error: SchedulerError) -> IntegrationError {
    startup_error(error.to_string())
}

fn ownership_error(error: ProfileManagerError) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: None,
        message: format!("Profile ownership is unavailable: {error}"),
        retryable: matches!(
            error,
            ProfileManagerError::Busy | ProfileManagerError::Io(_)
        ),
    }
}

fn recovery_error(message: impl Into<String>) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: None,
        message: format!("Recovery operation failed: {}", message.into()),
        retryable: false,
    }
}

fn device_identity() -> String {
    std::env::var("REC_BAG_DEVICE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "local-device".into())
}

fn reconcile_staging(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let candidate = entry.path();
        if candidate.is_file()
            && candidate
                .extension()
                .is_some_and(|extension| extension == "tmp")
            && entry
                .metadata()?
                .modified()?
                .elapsed()
                .is_ok_and(|age| age > Duration::from_secs(24 * 60 * 60))
        {
            let _ = std::fs::remove_file(candidate);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct DesktopPaths {
    root: PathBuf,
    catalog: PathBuf,
    drive_cache: PathBuf,
    staging: PathBuf,
    snapshots: PathBuf,
}

impl DesktopPaths {
    fn load() -> Result<Self, IntegrationError> {
        let root = match std::env::var_os("REC_BAG_DATA_DIR") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            Some(_) => {
                return Err(startup_error(
                    "REC_BAG_DATA_DIR is set but empty; set it to a writable directory",
                ))
            }
            None => default_data_directory().ok_or_else(|| {
                startup_error(
                    "Could not determine the application data directory; set REC_BAG_DATA_DIR",
                )
            })?,
        };
        Ok(Self {
            catalog: root.join("library.db"),
            drive_cache: root.join("cache").join("google-drive"),
            staging: root.join("staging"),
            snapshots: root.join("recovery").join("catalog-snapshots"),
            root,
        })
    }
}

fn default_data_directory() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("share"))
            })
    }
    .map(|path| path.join("rec_bag"))
}

fn startup_error(message: impl Into<String>) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: None,
        message: message.into(),
        retryable: false,
    }
}

fn provider_error(slice: IntegrationSlice, message: impl Into<String>) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: Some(slice),
        message: message.into(),
        retryable: false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartupAdoption {
    NotNeeded,
    Adopted,
    NeedsSelection(String),
}

async fn adopt_migrated_local_library(
    catalog: &TursoCatalog,
    local: &LocalLibraryService,
) -> Result<StartupAdoption, IntegrationError> {
    if MusicStorageRepository::get_active_primary(catalog)
        .await
        .map_err(|error| {
            migration_adoption_error(format!("could not inspect primary storage: {error}"))
        })?
        .is_some()
    {
        return Ok(StartupAdoption::NotNeeded);
    }

    let migrated = MusicStorageRepository::list(catalog)
        .await
        .map_err(|error| {
            migration_adoption_error(format!("could not inspect migrated storage: {error}"))
        })?
        .into_iter()
        .find(is_migrated_synthetic_storage);
    let Some(migrated) = migrated else {
        return Ok(StartupAdoption::NotNeeded);
    };

    let legacy_paths = migrated_location_paths(catalog, migrated.id).await?;
    if legacy_paths.is_empty() {
        return Ok(StartupAdoption::NotNeeded);
    }
    let mut usable_paths = Vec::with_capacity(legacy_paths.len());
    for path in legacy_paths {
        if !path.is_absolute() || !path.is_file() {
            return Ok(migration_selection_notice(
                "one or more migrated audio paths are no longer available",
            ));
        }
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            migration_adoption_error(format!(
                "could not inspect migrated path {}: {error}",
                path.display()
            ))
        })?;
        usable_paths.push(canonical);
    }

    let mut candidates = configured_local_sources(catalog).await?;
    candidates.sort_by_key(|(_, root)| std::cmp::Reverse(root.components().count()));
    let Some((source, root)) = candidates
        .into_iter()
        .find(|(_, root)| usable_paths.iter().all(|path| path.starts_with(root)))
    else {
        return Ok(migration_selection_notice(
            "the migrated files do not share an available configured local source root",
        ));
    };

    let configuration = ui::PrimaryConfigurationView {
        provider: ui::ProviderChoice::LocalFolder,
        name: source.name.clone(),
        location: root.to_string_lossy().into_owned(),
        organize_by_artist: false,
        verify_after_copy: true,
    };
    local
        .adopt_migrated_library(source.id, &configuration)
        .await
        .map_err(|error| {
            migration_adoption_error(format!(
                "automatic in-place adoption failed: {error}. No files were moved; choose the existing library folder in Music Management to retry"
            ))
        })?;
    Ok(StartupAdoption::Adopted)
}

async fn migrated_location_paths(
    catalog: &TursoCatalog,
    storage_id: Uuid,
) -> Result<Vec<PathBuf>, IntegrationError> {
    let mut request = RepositoryPageRequest {
        cursor: None,
        limit: Some(500),
    };
    let mut paths = Vec::new();
    loop {
        let page = AssetRepository::list_locations(
            catalog,
            AssetLocationFilter {
                asset_id: None,
                storage_id: Some(storage_id),
            },
            request.clone(),
        )
        .await
        .map_err(|error| {
            migration_adoption_error(format!("could not inspect migrated paths: {error}"))
        })?;
        paths.extend(
            page.items
                .into_iter()
                .map(|location| PathBuf::from(location.object_id)),
        );
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request.cursor = Some(cursor);
    }
    Ok(paths)
}

async fn configured_local_sources(
    catalog: &TursoCatalog,
) -> Result<Vec<(CatalogSource, PathBuf)>, IntegrationError> {
    let mut request = RepositoryPageRequest {
        cursor: None,
        limit: Some(500),
    };
    let mut sources = Vec::new();
    loop {
        let page = SourceRepository::list(catalog, request.clone())
            .await
            .map_err(|error| {
                migration_adoption_error(format!("could not inspect local sources: {error}"))
            })?;
        for source in page.items {
            if !source.enabled || source.kind != CatalogSourceKind::LocalFolder {
                continue;
            }
            let Some(root) = source.config.get("path").map(PathBuf::from) else {
                continue;
            };
            if let Ok(root) = std::fs::canonicalize(root) {
                if root.is_dir() {
                    sources.push((source, root));
                }
            }
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request.cursor = Some(cursor);
    }
    Ok(sources)
}

fn is_migrated_synthetic_storage(storage: &rec_core::MusicStorage) -> bool {
    storage.kind == MusicStorageKind::LocalDirectory
        && storage.role == rec_core::StorageRole::Unassigned
        && storage.name == "Migrated local files"
        && storage.root_identifier.is_none()
}

fn migration_selection_notice(reason: &str) -> StartupAdoption {
    StartupAdoption::NeedsSelection(format!(
        "Migrated local-library records were preserved, but {reason}. Open Music Management, choose the existing library folder, and retry adoption; rec_bag will not move files during this step."
    ))
}

fn migration_adoption_error(message: impl Into<String>) -> IntegrationError {
    provider_error(
        IntegrationSlice::LocalLibrary,
        format!(
            "Could not adopt the migrated local library: {}",
            message.into()
        ),
    )
}

fn production_credentials() -> Arc<dyn CredentialStore> {
    #[cfg(feature = "desktop")]
    {
        Arc::new(super::google_drive::KeyringCredentialStore::new("rec_bag"))
    }
    #[cfg(not(feature = "desktop"))]
    {
        Arc::new(super::soundcloud_catalog::MemoryCredentialStore::default())
    }
}

fn soundcloud_service(
    catalog: Arc<TursoCatalog>,
    credentials: Arc<dyn CredentialStore>,
) -> Result<SoundCloudCatalogService, IntegrationError> {
    let client_id = std::env::var("REC_BAG_SOUNDCLOUD_CLIENT_ID").ok();
    let redirect = std::env::var("REC_BAG_SOUNDCLOUD_REDIRECT_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:17841/oauth/soundcloud".to_string());
    let redirect = url::Url::parse(&redirect).map_err(|error| {
        provider_error(
            IntegrationSlice::SoundcloudCatalog,
            format!("REC_BAG_SOUNDCLOUD_REDIRECT_URI is invalid: {error}"),
        )
    })?;
    let config = SoundCloudOAuthClientConfig::new(
        client_id.clone().unwrap_or_else(|| "not-configured".into()),
        redirect,
    )?;
    let user_agent: Arc<dyn OAuthUserAgent> = if client_id.is_none() {
        Arc::new(MissingSoundCloudConfiguration)
    } else {
        production_soundcloud_user_agent()
    };
    Ok(SoundCloudCatalogService::new(
        config,
        user_agent,
        credentials,
        Arc::new(ApiSoundCloudBrokerClient),
        Arc::new(ProductionSoundCloudConnectorFactory),
        catalog,
    ))
}

struct MissingSoundCloudConfiguration;

impl OAuthUserAgent for MissingSoundCloudConfiguration {
    fn authorize(
        &self,
        _authorization_url: url::Url,
        _redirect_uri: url::Url,
        _timeout: std::time::Duration,
    ) -> rec_core::BoxFuture<'_, Result<url::Url, IntegrationError>> {
        Box::pin(async {
            Err(provider_error(
                IntegrationSlice::SoundcloudCatalog,
                "SoundCloud is not configured. Set REC_BAG_SOUNDCLOUD_CLIENT_ID and, if needed, REC_BAG_SOUNDCLOUD_REDIRECT_URI, then restart rec_bag.",
            ))
        })
    }
}

fn production_soundcloud_user_agent() -> Arc<dyn OAuthUserAgent> {
    #[cfg(feature = "desktop")]
    {
        Arc::new(super::soundcloud_catalog::LoopbackOAuthUserAgent)
    }
    #[cfg(not(feature = "desktop"))]
    {
        Arc::new(MissingSoundCloudConfiguration)
    }
}

fn local_selection(root: PathBuf) -> LocalSourceSelection {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Local music")
        .to_owned();
    LocalSourceSelection {
        name,
        root,
        copy_policy: rec_core::SourceCopyPolicy::AskBeforeCopying,
    }
}

struct EnvironmentLocalSelection;

impl LocalSelectionProvider for EnvironmentLocalSelection {
    fn select_source<'a>(&'a self) -> SelectionFuture<'a> {
        Box::pin(async {
            let root = std::env::var_os("REC_BAG_LOCAL_SOURCE").ok_or_else(|| {
                LocalLibraryError::InvalidRoot(
                    "No folder picker is available in this build. Set REC_BAG_LOCAL_SOURCE to an audio directory and retry.".into(),
                )
            })?;
            Ok(Some(local_selection(PathBuf::from(root))))
        })
    }
}

#[cfg(feature = "desktop")]
struct NativeLocalSelection;

#[cfg(feature = "desktop")]
impl LocalSelectionProvider for NativeLocalSelection {
    fn select_source<'a>(&'a self) -> SelectionFuture<'a> {
        Box::pin(async {
            // The override keeps headless integration tests and managed deployments injectable.
            if let Some(root) = std::env::var_os("REC_BAG_LOCAL_SOURCE") {
                return Ok(Some(local_selection(PathBuf::from(root))));
            }
            let selected = tokio::task::spawn_blocking(native_folder_dialog)
                .await
                .map_err(|error| {
                    LocalLibraryError::InvalidRoot(format!("The folder chooser failed: {error}"))
                })??;
            Ok(selected.map(local_selection))
        })
    }
}

#[cfg(all(feature = "desktop", target_os = "windows"))]
fn native_folder_dialog() -> Result<Option<PathBuf>, LocalLibraryError> {
    let script = "$folder=(New-Object -ComObject Shell.Application).BrowseForFolder(0,'Choose a music folder',0,0); if ($folder) { $folder.Self.Path }";
    run_folder_dialog("powershell", &["-NoProfile", "-Command", script])
}

#[cfg(all(feature = "desktop", target_os = "macos"))]
fn native_folder_dialog() -> Result<Option<PathBuf>, LocalLibraryError> {
    run_folder_dialog(
        "osascript",
        &[
            "-e",
            "POSIX path of (choose folder with prompt \"Choose a music folder\")",
        ],
    )
}

#[cfg(all(
    feature = "desktop",
    not(any(target_os = "windows", target_os = "macos"))
))]
fn native_folder_dialog() -> Result<Option<PathBuf>, LocalLibraryError> {
    match run_folder_dialog(
        "zenity",
        &["--file-selection", "--directory", "--title=Choose a music folder"],
    ) {
        Err(LocalLibraryError::InvalidRoot(message)) if message.contains("not installed") => {
            run_folder_dialog("kdialog", &["--getexistingdirectory", "."] ).map_err(|_| {
                LocalLibraryError::InvalidRoot(
                    "No native folder chooser is installed. Install zenity or kdialog, or set REC_BAG_LOCAL_SOURCE for a headless session.".into(),
                )
            })
        }
        result => result,
    }
}

#[cfg(feature = "desktop")]
fn run_folder_dialog(
    program: &str,
    arguments: &[&str],
) -> Result<Option<PathBuf>, LocalLibraryError> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| {
            LocalLibraryError::InvalidRoot(format!(
                "Native folder chooser {program} is not installed or could not start: {error}"
            ))
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8(output.stdout).map_err(|_| {
        LocalLibraryError::InvalidRoot("The native folder chooser returned an invalid path".into())
    })?;
    let path = path.trim();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

fn production_local_selection() -> Arc<dyn LocalSelectionProvider> {
    #[cfg(feature = "desktop")]
    {
        Arc::new(NativeLocalSelection)
    }
    #[cfg(not(feature = "desktop"))]
    {
        Arc::new(EnvironmentLocalSelection)
    }
}

#[cfg(not(feature = "desktop"))]
struct EnvironmentGoogleProvisioner;

#[cfg(not(feature = "desktop"))]
impl GoogleDriveProvisioner for EnvironmentGoogleProvisioner {
    fn provision<'a>(
        &'a self,
        configuration: &'a ui::PrimaryConfigurationView,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveServiceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let access_token = std::env::var("REC_BAG_GOOGLE_ACCESS_TOKEN").map_err(|_| {
                GoogleDriveServiceError::Authentication(
                    "Interactive Google authorization requires the desktop feature. For server or test injection, set REC_BAG_GOOGLE_ACCESS_TOKEN and REC_BAG_GOOGLE_CLIENT_ID.".into(),
                )
            })?;
            let identity = if configuration.location.trim().is_empty() {
                std::env::var("REC_BAG_GOOGLE_DRIVE_FOLDER").map_err(|_| {
                    GoogleDriveServiceError::FolderSelection(
                        "Google Drive backup needs a folder ID or link. Set REC_BAG_GOOGLE_DRIVE_FOLDER for this headless build and retry.".into(),
                    )
                })?
            } else {
                configuration.location.clone()
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
                    display_name: configuration.name.clone(),
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
    ) -> Pin<
        Box<dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveServiceError>> + Send + 'a>,
    >;
}

trait GoogleCallbackSessionFactory: Send + Sync {
    fn prepare(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveServiceError>>
                + Send
                + '_,
        >,
    >;
}

trait DriveFolderSelectorFactory: Send + Sync {
    fn selector(
        &self,
        identity: &str,
    ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveServiceError>;
}

trait DriveFolderIdentityProvider: Send + Sync {
    fn request<'a>(
        &'a self,
        configured: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveServiceError>> + Send + 'a>>;
}

#[derive(Default)]
struct CallbackStates(std::sync::Mutex<BTreeSet<String>>);

impl CallbackStates {
    fn register(&self, state: String) -> Result<(), GoogleDriveServiceError> {
        let mut states = self.0.lock().map_err(|_| callback_state_unavailable())?;
        if !states.insert(state) {
            return Err(GoogleDriveServiceError::Authentication(
                "Google OAuth state was already pending".into(),
            ));
        }
        Ok(())
    }

    fn consume(&self, expected: &str, actual: &str) -> Result<(), GoogleDriveServiceError> {
        let mut states = self.0.lock().map_err(|_| callback_state_unavailable())?;
        if !states.remove(expected) {
            return Err(GoogleDriveServiceError::Authentication(
                "Google OAuth callback was expired or replayed".into(),
            ));
        }
        if !bool::from(expected.as_bytes().ct_eq(actual.as_bytes())) {
            return Err(GoogleDriveServiceError::Authentication(
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

fn callback_state_unavailable() -> GoogleDriveServiceError {
    GoogleDriveServiceError::Authentication("Google OAuth state store is unavailable".into())
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
        configuration: &'a ui::PrimaryConfigurationView,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveServiceError>> + Send + 'a>>
    {
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
            let folder = match production_drive_folder_selection(configuration) {
                DriveFolderSelection::CreateDedicated { name } => {
                    super::google_drive::DriveApiFolderSelector::create_dedicated(name)
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

fn production_drive_folder_selection(
    configuration: &ui::PrimaryConfigurationView,
) -> DriveFolderSelection {
    let name = configuration.name.trim();
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
        configuration: &'a ui::PrimaryConfigurationView,
    ) -> Pin<Box<dyn Future<Output = Result<ProvisionedDrive, GoogleDriveServiceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let identity = self.identities.request(&configuration.location).await?;
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

fn normalize_drive_folder_identity(value: &str) -> Result<String, GoogleDriveServiceError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GoogleDriveServiceError::FolderSelection(
            "Paste a Google Drive folder link or stable folder ID in the storage location field"
                .into(),
        ));
    }
    if let Ok(url) = url::Url::parse(value) {
        if !url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("drive.google.com")
                || host.eq_ignore_ascii_case("docs.google.com")
        }) {
            return Err(GoogleDriveServiceError::FolderSelection(
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
        return Err(GoogleDriveServiceError::FolderSelection(
            "Could not find a stable folder ID in the Google Drive link".into(),
        ));
    }
    if value.chars().any(char::is_whitespace) || value.contains('/') {
        return Err(GoogleDriveServiceError::FolderSelection(
            "The Drive folder ID is malformed; paste the folder link or its stable ID".into(),
        ));
    }
    Ok(value.to_owned())
}

fn parse_google_callback(
    callback: &url::Url,
) -> Result<GoogleOAuthCallback, GoogleDriveServiceError> {
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
        return Err(GoogleDriveServiceError::Authentication(format!(
            "Google authorization was cancelled or denied: {detail}"
        )));
    }
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GoogleDriveServiceError::Authentication(
                "Google OAuth callback did not include an authorization code".into(),
            )
        })?;
    let state = callback
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GoogleDriveServiceError::Authentication(
                "Google OAuth callback did not include state".into(),
            )
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
            dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveServiceError>>
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
                return Err(GoogleDriveServiceError::Authentication(
                    "REC_BAG_GOOGLE_REDIRECT_URI must be an HTTP loopback URL".into(),
                ));
            }
            let port = self.redirect_uri.port().ok_or_else(|| {
                GoogleDriveServiceError::Authentication(
                    "REC_BAG_GOOGLE_REDIRECT_URI must include a loopback port".into(),
                )
            })?;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .map_err(|error| {
                    GoogleDriveServiceError::Authentication(format!(
                        "Could not listen for the Google OAuth callback on port {port}: {error}"
                    ))
                })?;
            Ok(Box::new(LoopbackGoogleSession {
                redirect_uri: self.redirect_uri.clone(),
                listener: Mutex::new(Some(listener)),
            }) as Box<dyn GoogleCallbackSession>)
        })
    }
}

#[cfg(feature = "desktop")]
struct LoopbackGoogleSession {
    redirect_uri: url::Url,
    listener: Mutex<Option<tokio::net::TcpListener>>,
}

#[cfg(feature = "desktop")]
impl GoogleCallbackSession for LoopbackGoogleSession {
    fn redirect_uri(&self) -> url::Url {
        self.redirect_uri.clone()
    }

    fn receive<'a>(
        &'a self,
        _request: &'a GoogleOAuthRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveServiceError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let listener = self.listener.lock().await.take().ok_or_else(|| {
                GoogleDriveServiceError::Authentication(
                    "Google OAuth callback was already consumed".into(),
                )
            })?;
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(180), listener.accept())
                    .await
                    .map_err(|_| {
                        GoogleDriveServiceError::Authentication(
                            "Google authorization timed out or was cancelled".into(),
                        )
                    })?
                    .map_err(|error| {
                        GoogleDriveServiceError::Authentication(format!(
                            "Could not receive the Google OAuth callback: {error}"
                        ))
                    })?;
            let mut bytes = vec![0_u8; 16 * 1024];
            let count = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::io::AsyncReadExt::read(&mut stream, &mut bytes),
            )
            .await
            .map_err(|_| {
                GoogleDriveServiceError::Authentication(
                    "Google OAuth callback read timed out or was cancelled".into(),
                )
            })?
            .map_err(|error| {
                GoogleDriveServiceError::Authentication(format!(
                    "Could not read the Google OAuth callback: {error}"
                ))
            })?;
            let request = std::str::from_utf8(&bytes[..count]).map_err(|_| {
                GoogleDriveServiceError::Authentication(
                    "Google OAuth callback was malformed".into(),
                )
            })?;
            let target = request
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|line| line.split_once(' ').map(|(target, _)| target))
                .ok_or_else(|| {
                    GoogleDriveServiceError::Authentication(
                        "Google OAuth callback was malformed".into(),
                    )
                })?;
            let callback = self.redirect_uri.join(target).map_err(|_| {
                GoogleDriveServiceError::Authentication(
                    "Google OAuth callback URL was malformed".into(),
                )
            })?;
            if callback.path() != self.redirect_uri.path() {
                write_google_callback_response(&mut stream, 400, "Invalid callback request.").await;
                return Err(GoogleDriveServiceError::Authentication(
                    "Google OAuth callback path did not match".into(),
                ));
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
            parsed
        })
    }
}

#[cfg(feature = "desktop")]
async fn write_google_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
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
    ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveServiceError>> + Send + 'a>> {
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
                    GoogleDriveServiceError::FolderSelection(format!(
                        "The Google Drive folder prompt failed: {error}"
                    ))
                })??;
            selected.ok_or_else(|| {
                GoogleDriveServiceError::FolderSelection(
                    "Google Drive folder selection was cancelled. Paste or select a Drive folder ID/link and retry Add backup.".into(),
                )
            })
        })
    }
}

#[cfg(all(feature = "desktop", target_os = "windows"))]
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveServiceError> {
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
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveServiceError> {
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
fn native_drive_folder_identity_dialog() -> Result<Option<String>, GoogleDriveServiceError> {
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
            GoogleDriveServiceError::FolderSelection(
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
) -> Result<Option<String>, GoogleDriveServiceError> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| {
            GoogleDriveServiceError::FolderSelection(format!(
                "Native Google Drive folder prompt {program} could not start: {error}"
            ))
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        GoogleDriveServiceError::FolderSelection(
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
    ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveServiceError> {
        Ok(Arc::new(super::google_drive::DriveApiFolderSelector::new(
            identity,
        )))
    }
}

fn production_google_provisioner(
    oauth: Arc<dyn GoogleOAuth>,
    _credentials: Arc<dyn CredentialStore>,
) -> Result<Arc<dyn GoogleDriveProvisioner>, IntegrationError> {
    #[cfg(feature = "desktop")]
    {
        let redirect_uri = std::env::var("REC_BAG_GOOGLE_REDIRECT_URI")
            .unwrap_or_else(|_| "http://127.0.0.1:17842/oauth/google".into());
        let redirect_uri = url::Url::parse(&redirect_uri).map_err(|error| {
            provider_error(
                IntegrationSlice::GoogleDrive,
                format!("REC_BAG_GOOGLE_REDIRECT_URI is invalid: {error}"),
            )
        })?;
        Ok(Arc::new(ProductionGoogleProvisioner {
            oauth,
            sessions: Arc::new(LoopbackGoogleSessionFactory { redirect_uri }),
            states: CallbackStates::default(),
        }))
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = (oauth, _credentials);
        Ok(Arc::new(EnvironmentGoogleProvisioner))
    }
}

struct MissingGoogleOAuth;

impl GoogleOAuth for MissingGoogleOAuth {
    fn begin(
        &self,
        _redirect_uri: url::Url,
    ) -> Pin<
        Box<dyn Future<Output = Result<GoogleOAuthRequest, GoogleDriveServiceError>> + Send + '_>,
    > {
        Box::pin(async { Err(missing_google_client()) })
    }

    fn exchange<'a>(
        &'a self,
        _request: &'a GoogleOAuthRequest,
        _callback: GoogleOAuthCallback,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveServiceError>> + Send + 'a>>
    {
        Box::pin(async { Err(missing_google_client()) })
    }

    fn refresh<'a>(
        &'a self,
        _refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveServiceError>> + Send + 'a>>
    {
        Box::pin(async { Err(missing_google_client()) })
    }
}

fn missing_google_client() -> GoogleDriveServiceError {
    GoogleDriveServiceError::Authentication(
        "Google OAuth is not configured. Set REC_BAG_GOOGLE_CLIENT_ID to the installed-app OAuth client ID, register the loopback redirect URI, then restart rec_bag.".into(),
    )
}

fn production_google_oauth() -> Arc<dyn GoogleOAuth> {
    let client_id = std::env::var("REC_BAG_GOOGLE_CLIENT_ID").ok();
    #[cfg(feature = "desktop")]
    if let Some(client_id) = client_id {
        return Arc::new(super::google_drive::ReqwestGoogleOAuth::new(
            client_id, None,
        ));
    }
    let _ = client_id;
    Arc::new(MissingGoogleOAuth)
}

fn production_drive_factory(
    credentials: Arc<dyn CredentialStore>,
    oauth: Arc<dyn GoogleOAuth>,
    cache_root: PathBuf,
) -> Arc<dyn DriveRuntimeFactory> {
    #[cfg(feature = "desktop")]
    {
        Arc::new(super::google_drive::ProductionDriveRuntimeFactory::new(
            credentials,
            oauth,
            cache_root,
        ))
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = (credentials, oauth, cache_root);
        Arc::new(UnavailableDriveRuntimeFactory)
    }
}

#[cfg(not(feature = "desktop"))]
struct UnavailableDriveRuntimeFactory;

#[cfg(not(feature = "desktop"))]
impl DriveRuntimeFactory for UnavailableDriveRuntimeFactory {
    fn open<'a>(
        &'a self,
        _storage: &'a rec_core::MusicStorage,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Arc<dyn super::google_drive::DriveRuntime>,
                        GoogleDriveServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(GoogleDriveServiceError::Runtime(
                "Google Drive runtime requires the desktop feature".into(),
            ))
        })
    }
}

async fn initialize_storage_registries(
    catalog: &TursoCatalog,
    drive_registry: Arc<DriveRuntimeRegistry>,
    backup_registry: Arc<BackupProviderRegistry>,
    errors: &mut Vec<IntegrationError>,
) {
    let storages = match MusicStorageRepository::list(catalog).await {
        Ok(storages) => storages,
        Err(error) => {
            errors.push(startup_error(format!(
                "Could not load persisted storage providers: {error}"
            )));
            return;
        }
    };
    for storage in storages {
        // This migration evidence remains persisted for recovery/reconciliation but is
        // deliberately not a usable provider and must never become authoritative.
        if !storage.enabled || is_migrated_synthetic_storage(&storage) {
            drive_registry.remove(storage.id);
            backup_registry.remove(storage.id);
            continue;
        }
        match storage.kind {
            MusicStorageKind::LocalDirectory => {
                drive_registry.remove(storage.id);
                let Some(root) = storage.root_identifier.as_deref() else {
                    errors.push(provider_error(
                        IntegrationSlice::LocalLibrary,
                        format!("Local storage {} has no persisted root path", storage.name),
                    ));
                    continue;
                };
                match LocalMusicStorage::new(storage.id, root) {
                    Ok(provider) => {
                        backup_registry.register(storage.id, Arc::new(provider));
                    }
                    Err(error) => {
                        backup_registry.remove(storage.id);
                        errors.push(provider_error(
                            IntegrationSlice::LocalLibrary,
                            format!(
                                "Could not initialize local storage {}: {error}",
                                storage.name
                            ),
                        ));
                    }
                }
            }
            MusicStorageKind::GoogleDrive => match drive_registry.get_or_open(&storage).await {
                Ok(provider) => {
                    let provider: Arc<dyn MusicAssetStore> = provider;
                    backup_registry.register(storage.id, provider);
                }
                Err(error) => {
                    backup_registry.remove(storage.id);
                    errors.push(provider_error(
                        IntegrationSlice::GoogleDrive,
                        format!(
                            "Could not initialize Google Drive storage {}: {error}",
                            storage.name
                        ),
                    ));
                }
            },
            MusicStorageKind::Other(ref kind) => {
                drive_registry.remove(storage.id);
                backup_registry.remove(storage.id);
                errors.push(startup_error(format!(
                    "Storage {} uses unsupported provider kind {kind}",
                    storage.name
                )));
            }
        }
    }
}

struct DesktopAssetResolver {
    catalog: Arc<TursoCatalog>,
    drive: Arc<GoogleDriveService>,
}

impl AssetResolver for DesktopAssetResolver {
    fn resolve<'a>(&'a self, asset: &'a AssetReference) -> AssetFuture<'a> {
        Box::pin(async move {
            let storage_id = parse_id("storage", &asset.storage_id)?;
            let asset_id = parse_id("asset", &asset.asset_id)?;
            let storage = MusicStorageRepository::get(self.catalog.as_ref(), storage_id)
                .await
                .map_err(|error| startup_error(format!("Could not load asset storage: {error}")))?
                .ok_or_else(|| startup_error(format!("Storage {storage_id} was not found")))?;
            match storage.kind {
                MusicStorageKind::GoogleDrive => {
                    let resolved = self
                        .drive
                        .resolve(storage_id, asset_id)
                        .await
                        .map_err(IntegrationError::from)?;
                    Ok(ResolvedAsset {
                        uri: resolved.local_path.to_string_lossy().into_owned(),
                        cached: true,
                    })
                }
                MusicStorageKind::LocalDirectory => {
                    let location =
                        AssetRepository::list_locations_for_asset(self.catalog.as_ref(), asset_id)
                            .await
                            .map_err(|error| {
                                startup_error(format!("Could not load asset location: {error}"))
                            })?
                            .into_iter()
                            .find(|location| location.storage_id == storage_id)
                            .ok_or_else(|| {
                                startup_error("The asset has no location in that local storage")
                            })?;
                    let root = storage.root_identifier.as_deref().ok_or_else(|| {
                        startup_error("The local storage has no configured root path")
                    })?;
                    let provider = LocalMusicStorage::new(storage_id, root).map_err(|error| {
                        startup_error(format!("Could not open local storage: {error}"))
                    })?;
                    let path = provider
                        .playback_path(&location.object_id)
                        .map_err(|error| {
                            startup_error(format!(
                                "Could not resolve local playback asset: {error}"
                            ))
                        })?;
                    Ok(ResolvedAsset {
                        uri: path.to_string_lossy().into_owned(),
                        cached: false,
                    })
                }
                MusicStorageKind::Other(kind) => Err(startup_error(format!(
                    "Storage provider {kind} cannot resolve playback assets"
                ))),
            }
        })
    }
}

fn parse_id(kind: &str, value: &str) -> Result<Uuid, IntegrationError> {
    Uuid::parse_str(value).map_err(|error| startup_error(format!("Invalid {kind} ID: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Mutex as StdMutex,
        },
    };

    use crate::state::integration::{IntegrationController, IntegrationState};
    use ui::{MusicManagementEvent, ProviderChoice};

    struct NeverSelect {
        calls: Arc<AtomicUsize>,
    }

    impl LocalSelectionProvider for NeverSelect {
        fn select_source<'a>(&'a self) -> SelectionFuture<'a> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Box::pin(async { panic!("startup adoption must never open the folder picker") })
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("rec-bag-integration-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestAssetResolver;

    impl AssetResolver for TestAssetResolver {
        fn resolve<'a>(&'a self, _asset: &'a AssetReference) -> AssetFuture<'a> {
            Box::pin(async { Err(startup_error("not used by lifecycle tests")) })
        }
    }

    async fn lifecycle_runtime(label: &str) -> (Arc<DesktopIntegration>, TestDirectory) {
        let root = TestDirectory::new(label);
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let local = Arc::new(LocalLibraryService::new(
            catalog.clone(),
            Arc::new(NeverSelect {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        ));
        let credentials =
            Arc::new(super::super::soundcloud_catalog::MemoryCredentialStore::default());
        let soundcloud = Arc::new(soundcloud_service(catalog.clone(), credentials).unwrap());
        let backup_providers = Arc::new(BackupProviderRegistry::default());
        let backup = Arc::new(BackupService::new(
            catalog.clone(),
            backup_providers.clone(),
            root.path().join("staging"),
        ));
        let recovery = Arc::new(RecoveryService::new(
            catalog.clone(),
            root.path().join("snapshots"),
        ));
        let manager = ProfileManager::new(
            root.path(),
            ManagerIdentity::new("profile", "device"),
            Duration::from_secs(60),
        )
        .unwrap();
        let lease = manager.acquire(Utc::now()).unwrap();
        recovery
            .persist_catalog_authority(&manager, &lease)
            .await
            .unwrap();
        let drive_registry = Arc::new(DriveRuntimeRegistry::new(Arc::new(NeverDriveFactory)));
        let runtime = Arc::new(DesktopIntegration::from_parts(
            catalog,
            Arc::new(ProviderRegistry::default()),
            local,
            soundcloud,
            backup,
            recovery,
            manager,
            lease,
            None,
            drive_registry,
            backup_providers,
            Arc::new(TestAssetResolver),
            Vec::new(),
        ));
        (runtime, root)
    }

    #[test]
    fn production_drive_selection_never_claims_picker_provenance_for_pasted_ids() {
        let configuration = ui::PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            name: "Portable rec_bag".into(),
            location: "pasted-folder-id-without-picker-provenance".into(),
            organize_by_artist: false,
            verify_after_copy: true,
        };

        assert_eq!(
            production_drive_folder_selection(&configuration),
            DriveFolderSelection::CreateDedicated {
                name: "Portable rec_bag".into(),
            }
        );
    }

    #[tokio::test]
    async fn restore_cannot_start_after_shutdown_begins() {
        let (runtime, _root) = lifecycle_runtime("restore-shutdown").await;
        runtime.begin_shutdown();

        let error = runtime
            .restore_snapshot(PathBuf::from("unused-snapshot"), false)
            .await
            .unwrap_err();

        assert!(error.message.contains("cannot start"));
        runtime
            .shutdown_with_timeouts(Duration::from_millis(1), Duration::from_millis(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn operation_timeout_still_releases_profile_lease() {
        let root = TestDirectory::new("shutdown-release");
        let manager = ProfileManager::new(
            root.path(),
            ManagerIdentity::new("profile", "device"),
            Duration::from_secs(60),
        )
        .unwrap();
        manager.acquire(Utc::now()).unwrap();
        let active = AtomicUsize::new(1);
        let errors = drain_operations_and_release(
            &active,
            &Notify::new(),
            &manager,
            Duration::from_millis(1),
        )
        .await;

        assert!(errors.iter().any(|error| error.contains("did not stop")));
        assert_eq!(
            manager.status(Utc::now()).unwrap(),
            OwnershipStatus::Available
        );
    }

    async fn migrated_catalog(
        root: &std::path::Path,
        source_path: Option<&std::path::Path>,
    ) -> (
        Arc<TursoCatalog>,
        Arc<LocalLibraryService>,
        Uuid,
        Arc<AtomicUsize>,
    ) {
        use rec_core::{
            AssetLocation, AssetType, CanonicalTrack, MusicStorage, OriginalAsset,
            OriginalAssetRole, StorageCapabilities, StorageObject, StorageRole,
            StorageVerificationStatus, TrackRepository,
        };

        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        if let Some(source_path) = source_path {
            let source = CatalogSource::local_folder(
                "Existing local library",
                source_path.to_string_lossy(),
            );
            SourceRepository::upsert(catalog.as_ref(), source)
                .await
                .unwrap();
        }
        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let asset = OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "track.mp3");
        AssetRepository::upsert_original(catalog.as_ref(), asset.clone())
            .await
            .unwrap();
        let storage = MusicStorage::new(
            "Migrated local files",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        let storage_id = storage.id;
        MusicStorageRepository::upsert(catalog.as_ref(), storage)
            .await
            .unwrap();
        let legacy_path = root.join("track.mp3");
        let legacy_object_id = legacy_path.to_string_lossy().into_owned();
        AssetRepository::upsert_storage_object(
            catalog.as_ref(),
            StorageObject {
                storage_id,
                object_id: legacy_object_id.clone(),
                name: "track.mp3".into(),
                display_path: Some(legacy_object_id.clone()),
                size_bytes: std::fs::metadata(&legacy_path).map_or(0, |metadata| metadata.len()),
                media_type: None,
                modified_at: None,
                provider_version: None,
                hashes: Vec::new(),
                embedded_asset_id: Some(asset.id()),
                is_trashed: false,
            },
        )
        .await
        .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            AssetLocation {
                id: Uuid::new_v4(),
                asset_id: asset.id(),
                asset_type: AssetType::Original,
                storage_id,
                object_id: legacy_object_id,
                verification_status: StorageVerificationStatus::PossibleMatch,
                expected_hash: None,
                observed_hash: None,
                provider_version: None,
                last_checked_at: None,
                verified_at: None,
                error_message: None,
            },
        )
        .await
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let local = Arc::new(LocalLibraryService::new(
            catalog.clone(),
            Arc::new(NeverSelect {
                calls: calls.clone(),
            }),
        ));
        (catalog, local, storage_id, calls)
    }

    struct RecordingHandler {
        slice: IntegrationSlice,
        calls: Arc<StdMutex<Vec<IntegrationSlice>>>,
    }

    impl IntegrationSliceHandler for RecordingHandler {
        fn slice(&self) -> IntegrationSlice {
            self.slice
        }

        fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(request.slice);
                Ok(IntegrationOutcome {
                    view: None,
                    message: Some(format!("{:?} handled", request.slice)),
                })
            })
        }
    }

    #[tokio::test]
    async fn composition_dispatches_every_slice_and_updates_state() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut registry = ProviderRegistry::default();
        for slice in IntegrationSlice::ALL {
            registry.register(Arc::new(RecordingHandler {
                slice,
                calls: calls.clone(),
            }));
        }
        assert_eq!(registry.len(), IntegrationSlice::ALL.len());

        let events = [
            MusicManagementEvent::AddSource(ProviderChoice::LocalFolder),
            MusicManagementEvent::AddSource(ProviderChoice::SoundCloud),
            MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive),
            MusicManagementEvent::StartCleanupReview,
            MusicManagementEvent::CopyToPrimary(Uuid::new_v4().to_string()),
        ];
        let mut state = IntegrationState::default();
        state.events.extend([
            crate::state::integration::IntegrationEvent::RuntimeStarted,
            crate::state::integration::IntegrationEvent::SchedulerStarted,
        ]);
        for event in events {
            IntegrationController::dispatch(&mut state, event);
        }
        for slice in IntegrationSlice::ALL {
            let request = IntegrationController::begin_next(&mut state, slice).unwrap();
            let outcome = registry
                .handler(slice)
                .unwrap()
                .handle(request.clone())
                .await
                .unwrap();
            assert!(IntegrationController::complete(
                &mut state, request.id, outcome
            ));
        }

        assert_eq!(*calls.lock().unwrap(), IntegrationSlice::ALL);
        assert!(matches!(
            state.events.as_slice(),
            [
                crate::state::integration::IntegrationEvent::RuntimeStarted,
                crate::state::integration::IntegrationEvent::SchedulerStarted,
                ..
            ]
        ));
        assert_eq!(state.pending_requests().count(), 0);
        assert_eq!(state.in_flight_requests().count(), 0);
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| matches!(
                    event,
                    crate::state::integration::IntegrationEvent::RequestCompleted { .. }
                ))
                .count(),
            IntegrationSlice::ALL.len()
        );
    }

    struct NeverDriveFactory;

    impl DriveRuntimeFactory for NeverDriveFactory {
        fn open<'a>(
            &'a self,
            _storage: &'a rec_core::MusicStorage,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Arc<dyn super::super::google_drive::DriveRuntime>,
                            GoogleDriveServiceError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(GoogleDriveServiceError::Runtime(
                    "Drive should not be opened in this test".into(),
                ))
            })
        }
    }

    async fn verified_primary_with_backup(
        primary_root: &std::path::Path,
        backup_root: &std::path::Path,
    ) -> (Arc<TursoCatalog>, Uuid, Uuid, Uuid) {
        use rec_core::{
            AssetLocation, AssetType, CanonicalTrack, MusicStorage, OriginalAsset,
            OriginalAssetRole, StorageCapabilities, StorageRole, StorageVerificationStatus,
            TrackRepository,
        };

        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let primary = MusicStorage::new(
            "Primary",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        )
        .with_root_identifier(primary_root.to_string_lossy());
        let primary_id = primary.id;
        MusicStorageRepository::upsert(catalog.as_ref(), primary)
            .await
            .unwrap();
        MusicStorageRepository::switch_active_primary(catalog.as_ref(), None, Some(primary_id))
            .await
            .unwrap();
        let path = primary_root.join("track.flac");
        if !path.exists() {
            fs::write(&path, b"data").unwrap();
        }
        let provider = LocalMusicStorage::new(primary_id, primary_root).unwrap();
        let mut object = provider.stat("track.flac").await.unwrap();
        let hash = object.canonical_hash().cloned().unwrap();

        let track = CanonicalTrack::new();
        TrackRepository::upsert(catalog.as_ref(), track.clone())
            .await
            .unwrap();
        let mut asset =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "track.flac");
        asset.size_bytes = Some(object.size_bytes);
        asset.content_hash = Some(hash.clone());
        AssetRepository::upsert_original(catalog.as_ref(), asset.clone())
            .await
            .unwrap();
        object.embedded_asset_id = Some(asset.id());
        AssetRepository::upsert_storage_object(catalog.as_ref(), object.clone())
            .await
            .unwrap();
        AssetRepository::upsert_location(
            catalog.as_ref(),
            AssetLocation {
                id: Uuid::new_v4(),
                asset_id: asset.id(),
                asset_type: AssetType::Original,
                storage_id: primary_id,
                object_id: object.object_id,
                verification_status: StorageVerificationStatus::Verified,
                expected_hash: Some(hash.clone()),
                observed_hash: Some(hash),
                provider_version: object.provider_version,
                last_checked_at: Some(Utc::now()),
                verified_at: Some(Utc::now()),
                error_message: None,
            },
        )
        .await
        .unwrap();

        let backup = MusicStorage::new(
            "Backup",
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            StorageCapabilities::default(),
        )
        .with_root_identifier(backup_root.to_string_lossy());
        let backup_id = backup.id;
        MusicStorageRepository::upsert(catalog.as_ref(), backup)
            .await
            .unwrap();
        (catalog, track.id, primary_id, backup_id)
    }

    #[tokio::test]
    async fn unavailable_backup_is_durably_deferred_without_failing_primary_handoff() {
        let primary_root = TestDirectory::new("handoff-primary");
        let backup_root = TestDirectory::new("handoff-backup");
        fs::write(primary_root.path().join("track.flac"), b"data").unwrap();
        let (catalog, track_id, primary_id, backup_id) =
            verified_primary_with_backup(primary_root.path(), backup_root.path()).await;
        let providers = Arc::new(BackupProviderRegistry::default());
        providers.register(
            primary_id,
            Arc::new(LocalMusicStorage::new(primary_id, primary_root.path()).unwrap()),
        );
        let backup = BackupService::new(catalog.clone(), providers, backup_root.path());

        let status = plan_catalog_verified_writes(catalog.as_ref(), &backup, Some(track_id)).await;

        assert_eq!(status.planned_assets, 1);
        assert_eq!(status.queued_jobs, 1);
        assert_eq!(status.deferred_targets, 1);
        assert!(status.warnings.is_empty());
        assert!(status.message().contains("queued"));
        assert!(status.message().contains("deferred"));
        assert!(MusicStorageRepository::get(catalog.as_ref(), backup_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn provider_refresh_registers_new_local_backup_and_primary() {
        let primary_root = TestDirectory::new("refresh-primary");
        let backup_root = TestDirectory::new("refresh-backup");
        let (catalog, _, primary_id, backup_id) =
            verified_primary_with_backup(primary_root.path(), backup_root.path()).await;
        let drive = Arc::new(DriveRuntimeRegistry::new(Arc::new(NeverDriveFactory)));
        let providers = Arc::new(BackupProviderRegistry::default());
        let mut errors = Vec::new();

        initialize_storage_registries(catalog.as_ref(), drive, providers.clone(), &mut errors)
            .await;

        assert!(errors.is_empty());
        assert!(providers.get(primary_id).is_ok());
        assert!(providers.get(backup_id).is_ok());
    }

    #[test]
    fn handoff_status_is_appended_without_replacing_primary_success() {
        let mut outcome = IntegrationOutcome {
            view: None,
            message: Some("Primary write completed".into()),
        };
        append_outcome_message(
            &mut outcome,
            "Backup handoff: 1 durable job queued, 1 unavailable target deferred",
        );
        let message = outcome.message.unwrap();
        assert!(message.starts_with("Primary write completed"));
        assert!(message.contains("queued"));
        assert!(message.contains("deferred"));
    }

    #[tokio::test]
    async fn startup_adopts_migrated_library_in_place_without_prompting_or_moving_files() {
        use rec_core::StorageRole;

        let root = TestDirectory::new("adopt");
        let path = root.path().join("track.mp3");
        fs::write(&path, b"original audio").unwrap();
        let (catalog, local, migrated_storage_id, picker_calls) =
            migrated_catalog(root.path(), Some(root.path())).await;

        let outcome = adopt_migrated_local_library(catalog.as_ref(), local.as_ref())
            .await
            .unwrap();

        assert_eq!(outcome, StartupAdoption::Adopted);
        assert_eq!(picker_calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(fs::read(&path).unwrap(), b"original audio");
        let active = MusicStorageRepository::get_active_primary(catalog.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(active.id, migrated_storage_id);
        assert_eq!(
            active.root_identifier.as_deref(),
            Some(root.path().to_string_lossy().as_ref())
        );
        let migrated = MusicStorageRepository::get(catalog.as_ref(), migrated_storage_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(migrated.role, StorageRole::Unassigned);
        assert!(migrated.root_identifier.is_none());
    }

    #[tokio::test]
    async fn startup_preserves_migrated_records_and_notices_when_selection_is_needed() {
        let root = TestDirectory::new("notice");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let unrelated = TestDirectory::new("unrelated");
        let (catalog, local, migrated_storage_id, picker_calls) =
            migrated_catalog(root.path(), Some(unrelated.path())).await;

        let outcome = adopt_migrated_local_library(catalog.as_ref(), local.as_ref())
            .await
            .unwrap();

        let StartupAdoption::NeedsSelection(message) = outcome else {
            panic!("expected an actionable startup notice")
        };
        assert!(message.contains("choose the existing library folder"));
        assert!(message.contains("will not move files"));
        assert_eq!(picker_calls.load(AtomicOrdering::SeqCst), 0);
        assert!(MusicStorageRepository::get_active_primary(catalog.as_ref())
            .await
            .unwrap()
            .is_none());
        assert!(
            MusicStorageRepository::get(catalog.as_ref(), migrated_storage_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn secret_debug_never_exposes_bytes() {
        let secret = Secret::new(b"refresh-token".to_vec());
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
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

    struct TestDriveIdentityProvider {
        selected: Option<String>,
    }

    impl DriveFolderIdentityProvider for TestDriveIdentityProvider {
        fn request<'a>(
            &'a self,
            configured: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, GoogleDriveServiceError>> + Send + 'a>>
        {
            Box::pin(async move {
                if !configured.trim().is_empty() {
                    return Ok(configured.to_owned());
                }
                self.selected.clone().ok_or_else(|| {
                    GoogleDriveServiceError::FolderSelection(
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
        ) -> Pin<
            Box<
                dyn Future<Output = Result<GoogleOAuthRequest, GoogleDriveServiceError>>
                    + Send
                    + '_,
            >,
        > {
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
        ) -> Pin<
            Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveServiceError>> + Send + 'a>,
        > {
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
        ) -> Pin<
            Box<dyn Future<Output = Result<GoogleCredential, GoogleDriveServiceError>> + Send + 'a>,
        > {
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
        ) -> Pin<
            Box<
                dyn Future<Output = Result<GoogleOAuthCallback, GoogleDriveServiceError>>
                    + Send
                    + 'a,
            >,
        > {
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
                dyn Future<Output = Result<Box<dyn GoogleCallbackSession>, GoogleDriveServiceError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Ok(Box::new(TestCallbackSession) as Box<dyn GoogleCallbackSession>) })
        }
    }

    struct RecordingFolderSelector {
        selected_id: String,
        call: Arc<StdMutex<Option<(String, String)>>>,
    }

    impl GoogleDriveFolderSelector for RecordingFolderSelector {
        fn select_folder<'a>(
            &'a self,
            access_token: &'a str,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<SelectedDriveFolder, GoogleDriveServiceError>>
                    + Send
                    + 'a,
            >,
        > {
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
        call: Arc<StdMutex<Option<(String, String)>>>,
    }

    impl DriveFolderSelectorFactory for RecordingFolderSelectorFactory {
        fn selector(
            &self,
            identity: &str,
        ) -> Result<Arc<dyn GoogleDriveFolderSelector>, GoogleDriveServiceError> {
            Ok(Arc::new(RecordingFolderSelector {
                selected_id: identity.to_owned(),
                call: self.call.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn selected_drive_folder_is_normalized_and_validated_with_access_token() {
        let call = Arc::new(StdMutex::new(None));
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
        let configuration = ui::PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            name: "Primary".into(),
            location: String::new(),
            organize_by_artist: true,
            verify_after_copy: true,
        };

        let provisioned = provisioner.provision(&configuration).await.unwrap();

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
                call: Arc::new(StdMutex::new(None)),
            }),
            identities: Arc::new(TestDriveIdentityProvider { selected: None }),
            states: CallbackStates::default(),
        };
        let configuration = ui::PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            name: "Google Drive backup".into(),
            location: String::new(),
            organize_by_artist: false,
            verify_after_copy: true,
        };

        let error = provisioner.provision(&configuration).await.unwrap_err();

        assert!(error.to_string().contains("selection was cancelled"));
        assert!(error.to_string().contains("retry Add backup"));
    }

    #[test]
    fn cleanup_composition_projects_exact_keys_and_clears_approved_rows() {
        use chrono::Duration;
        use rec_sync::{CleanupCandidate, RetentionEvaluation, RetentionStatus};

        let storage_id = Uuid::new_v4();
        let generated_at = Utc::now();
        let exact_key = format!("{storage_id}|artist/track.mp3:version:old");
        let mut review = RetentionReview {
            generated_at,
            items: vec![super::super::backup::RetentionReviewItem {
                storage_id,
                object_id: "artist/track.mp3".into(),
                evaluation: RetentionEvaluation {
                    status: RetentionStatus::ReviewRequired,
                    retained_version_ids: vec!["current".into()],
                    cleanup_candidates: vec![CleanupCandidate {
                        object_id: "artist/track.mp3".into(),
                        version_id: "version:old".into(),
                        modified_at: Some(generated_at - Duration::days(42)),
                        reason: "retention period elapsed".into(),
                    }],
                },
                approved_candidate_keys: BTreeSet::new(),
            }],
        };

        let rows = retention_candidate_views(&review);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, exact_key);
        assert_eq!(rows[0].age, "42 days old");

        review.items[0]
            .approved_candidate_keys
            .insert(exact_key.clone());
        assert!(retention_candidate_views(&review).is_empty());
    }
}
