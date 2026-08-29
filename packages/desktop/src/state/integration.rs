//! Provider-neutral state and dispatch boundary for music-management integrations.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use crate::{
    services::{
        catalog::{Catalog, CatalogError},
        integration::{DesktopIntegration, DesktopStartup, OwnershipTakeoverOffer},
        playback_bridge::AssetPlaybackPaths,
        recovery::{RecoveryProgress, RestoreReport, SnapshotListing},
    },
    state::AppState,
};

use dioxus::prelude::*;
use ui::{
    CompletenessView, MusicManagementEvent, MusicManagementView, ProviderChoice, TrackFilter,
};

/// The independently owned integration slice responsible for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrationSlice {
    LocalLibrary,
    SoundcloudCatalog,
    GoogleDrive,
    Backup,
    ReconciliationActions,
}

impl IntegrationSlice {
    pub const ALL: [Self; 5] = [
        Self::LocalLibrary,
        Self::SoundcloudCatalog,
        Self::GoogleDrive,
        Self::Backup,
        Self::ReconciliationActions,
    ];
}

/// Work emitted by the UI and waiting for an integration slice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationRequest {
    pub id: u64,
    pub slice: IntegrationSlice,
    pub event: MusicManagementEvent,
}

/// Successful output from a slice handler.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntegrationOutcome {
    /// A complete replacement view, when the operation changed projected data.
    pub view: Option<MusicManagementView>,
    /// Optional user-facing success detail.
    pub message: Option<String>,
}

/// A provider-neutral failure reported at the desktop integration boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationError {
    pub request_id: Option<u64>,
    pub slice: Option<IntegrationSlice>,
    pub message: String,
    pub retryable: bool,
}

impl IntegrationError {
    pub fn for_request(
        request: &IntegrationRequest,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            request_id: Some(request.id),
            slice: Some(request.slice),
            message: message.into(),
            retryable,
        }
    }
}

impl std::fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IntegrationError {}

/// Lifecycle events retained for status presentation and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationEvent {
    FilterChanged(TrackFilter),
    RequestQueued {
        id: u64,
        slice: IntegrationSlice,
    },
    RequestStarted {
        id: u64,
        slice: IntegrationSlice,
    },
    RequestCompleted {
        id: u64,
        slice: IntegrationSlice,
        message: Option<String>,
    },
    RequestFailed(IntegrationError),
    OwnershipBlocked {
        owner_device: String,
    },
    TakeoverStarted,
    TakeoverCompleted,
    RuntimeStarted,
    SchedulerStarted,
    RecoveryStarted(RecoveryAction),
    RecoveryCompleted {
        action: RecoveryAction,
        progress: Vec<RecoveryProgress>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    RefreshSnapshots,
    DryRun,
    Restore,
    Rollback,
}

/// State projected into the provider-independent dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationState {
    pub view: MusicManagementView,
    pub selected_filter: TrackFilter,
    pub events: Vec<IntegrationEvent>,
    pub errors: Vec<IntegrationError>,
    pub recovery_snapshots: Vec<SnapshotListing>,
    pub last_pre_restore_snapshot: Option<PathBuf>,
    pub ownership_blocked_by: Option<String>,
    pending: VecDeque<IntegrationRequest>,
    in_flight: BTreeMap<u64, IntegrationRequest>,
    failed_events: BTreeMap<u64, MusicManagementEvent>,
    next_request_id: u64,
}

impl Default for IntegrationState {
    fn default() -> Self {
        Self {
            view: empty_music_management_view(),
            selected_filter: TrackFilter::All,
            events: Vec::new(),
            errors: Vec::new(),
            recovery_snapshots: Vec::new(),
            last_pre_restore_snapshot: None,
            ownership_blocked_by: None,
            pending: VecDeque::new(),
            in_flight: BTreeMap::new(),
            failed_events: BTreeMap::new(),
            next_request_id: 1,
        }
    }
}

impl IntegrationState {
    pub fn pending_requests(&self) -> impl Iterator<Item = &IntegrationRequest> {
        self.pending.iter()
    }

    pub fn in_flight_requests(&self) -> impl Iterator<Item = &IntegrationRequest> {
        self.in_flight.values()
    }

    pub fn dismiss_error(&mut self, index: usize) {
        if index < self.errors.len() {
            if let Some(request_id) = self.errors.remove(index).request_id {
                self.failed_events.remove(&request_id);
            }
        }
    }

    pub fn failed_event(&self, request_id: Option<u64>) -> Option<&MusicManagementEvent> {
        request_id.and_then(|id| self.failed_events.get(&id))
    }

    pub fn report_error(&mut self, error: IntegrationError) {
        self.errors.push(error.clone());
        self.events.push(IntegrationEvent::RequestFailed(error));
    }
}

/// Stateless reducer/controller used by the Dioxus adapter and unit tests.
pub struct IntegrationController;

impl IntegrationController {
    /// Dispatch a dashboard event. Local presentation events are reduced immediately;
    /// integration work is queued for exactly one slice.
    pub fn dispatch(state: &mut IntegrationState, event: MusicManagementEvent) -> Option<u64> {
        if let MusicManagementEvent::SetTrackFilter(filter) = event {
            state.selected_filter = filter;
            state.events.push(IntegrationEvent::FilterChanged(filter));
            return None;
        }

        let slice = match slice_for_event(&event, &state.view) {
            Ok(slice) => slice,
            Err(message) => {
                state.report_error(IntegrationError {
                    request_id: None,
                    slice: None,
                    message: message.to_owned(),
                    retryable: false,
                });
                return None;
            }
        };
        let id = state.next_request_id;
        state.next_request_id += 1;
        state
            .pending
            .push_back(IntegrationRequest { id, slice, event });
        state
            .events
            .push(IntegrationEvent::RequestQueued { id, slice });
        tracing::debug!(request_id = id, ?slice, action = ?state.pending.back().expect("request was just queued").event, "integration request queued");
        Some(id)
    }

    /// Claim the oldest request for a slice. Slice runners call this before invoking
    /// their provider-specific handler.
    pub fn begin_next(
        state: &mut IntegrationState,
        slice: IntegrationSlice,
    ) -> Option<IntegrationRequest> {
        let position = state
            .pending
            .iter()
            .position(|request| request.slice == slice)?;
        let request = state.pending.remove(position)?;
        state.in_flight.insert(request.id, request.clone());
        state.events.push(IntegrationEvent::RequestStarted {
            id: request.id,
            slice,
        });
        Some(request)
    }

    pub fn complete(
        state: &mut IntegrationState,
        request_id: u64,
        outcome: IntegrationOutcome,
    ) -> bool {
        let Some(request) = state.in_flight.remove(&request_id) else {
            return false;
        };
        if let Some(view) = outcome.view {
            state.view = view;
        }
        state.events.push(IntegrationEvent::RequestCompleted {
            id: request_id,
            slice: request.slice,
            message: outcome.message,
        });
        true
    }

    pub fn fail(
        state: &mut IntegrationState,
        request_id: u64,
        message: impl Into<String>,
        retryable: bool,
    ) -> bool {
        let Some(request) = state.in_flight.remove(&request_id) else {
            return false;
        };
        let error = IntegrationError::for_request(&request, message, retryable);
        state.failed_events.insert(request_id, request.event);
        state.errors.push(error.clone());
        state.events.push(IntegrationEvent::RequestFailed(error));
        true
    }
}

#[derive(Clone, Default)]
struct RuntimeSlot {
    runtime: Option<Arc<DesktopIntegration>>,
    takeover: Option<OwnershipTakeoverOffer>,
}

/// Context adapter used by Dioxus views and the asynchronous slice runners.
#[derive(Clone)]
pub struct IntegrationContext {
    pub state: Signal<IntegrationState>,
    runtime: Signal<RuntimeSlot>,
}

impl IntegrationContext {
    pub fn dispatch(&mut self, event: MusicManagementEvent) -> Option<u64> {
        let request_id = IntegrationController::dispatch(&mut self.state.write(), event);
        if let Some(request_id) = request_id {
            let slice = self
                .state
                .read()
                .pending_requests()
                .find(|request| request.id == request_id)
                .map(|request| request.slice);
            if let (Some(runtime), Some(slice)) = (&self.runtime.read().runtime, slice) {
                tracing::debug!(
                    request_id,
                    ?slice,
                    "waking integration slice for queued request"
                );
                runtime.wake(slice);
            } else {
                tracing::debug!(
                    request_id,
                    ?slice,
                    "request queued while integration runtime is still starting"
                );
            }
        }
        request_id
    }

    pub fn runtime(&self) -> Option<Arc<DesktopIntegration>> {
        self.runtime.read().runtime.clone()
    }

    pub async fn assume_ownership(&mut self) {
        // Retain the offer when an attempt fails so the user can retry or review the reported
        // reason instead of being left with an ownership-blocked profile and no action.
        let takeover = self.runtime.read().takeover.clone();
        let Some(takeover) = takeover else {
            return;
        };
        self.state
            .write()
            .events
            .push(IntegrationEvent::TakeoverStarted);
        match takeover.assume().await {
            Ok(runtime) => {
                self.state.write().ownership_blocked_by = None;
                self.state
                    .write()
                    .events
                    .push(IntegrationEvent::TakeoverCompleted);
                let mut slot = self.runtime.write();
                slot.takeover = None;
                slot.runtime = Some(runtime);
            }
            Err(error) => self.state.write().report_error(error),
        }
    }

    pub async fn refresh_recovery_snapshots(&mut self) {
        self.state
            .write()
            .events
            .push(IntegrationEvent::RecoveryStarted(
                RecoveryAction::RefreshSnapshots,
            ));
        let Some(runtime) = self.runtime() else {
            return;
        };
        match runtime.recovery_snapshots() {
            Ok(snapshots) => {
                self.state.write().recovery_snapshots = snapshots;
                self.state
                    .write()
                    .events
                    .push(IntegrationEvent::RecoveryCompleted {
                        action: RecoveryAction::RefreshSnapshots,
                        progress: Vec::new(),
                    });
            }
            Err(error) => self.state.write().report_error(error),
        }
    }

    pub async fn run_recovery(&mut self, action: RecoveryAction, path: PathBuf) {
        self.state
            .write()
            .events
            .push(IntegrationEvent::RecoveryStarted(action));
        let Some(runtime) = self.runtime() else {
            return;
        };
        let result = match action {
            RecoveryAction::DryRun => runtime.dry_run_restore(path).await,
            RecoveryAction::Restore => runtime.restore_snapshot(path, false).await,
            RecoveryAction::Rollback => runtime.rollback_restore(path).await,
            RecoveryAction::RefreshSnapshots => return,
        };
        match result {
            Ok(report) => self.complete_recovery(action, report),
            Err(error) => self.state.write().report_error(error),
        }
    }

    fn complete_recovery(&mut self, action: RecoveryAction, report: RestoreReport) {
        if !report.pre_restore_snapshot.as_os_str().is_empty() {
            self.state.write().last_pre_restore_snapshot = Some(report.pre_restore_snapshot);
        }
        self.state
            .write()
            .events
            .push(IntegrationEvent::RecoveryCompleted {
                action,
                progress: report.progress,
            });
    }
}

/// Installs desktop integration state. The fullstack server intentionally supplies `None`: it
/// serves assets and server functions but must not open or own a local desktop profile.
///
/// On desktop builds without a pre-opened runtime the integration is opened asynchronously
/// so the UI appears immediately.
pub fn use_integration_state_provider(startup: Option<Result<DesktopStartup, IntegrationError>>) {
    let app_state = super::use_app_state();
    let mut state = IntegrationState::default();
    let mut slot = RuntimeSlot::default();
    match startup {
        Some(Ok(DesktopStartup::Ready(ref runtime))) => {
            slot.runtime = Some(runtime.clone());
        }
        Some(Ok(DesktopStartup::OwnershipBlocked(ref offer))) => {
            state.ownership_blocked_by = Some(offer.owner_device.clone());
            state.events.push(IntegrationEvent::OwnershipBlocked {
                owner_device: offer.owner_device.clone(),
            });
            slot.takeover = Some(offer.clone());
        }
        Some(Err(ref error)) => state.report_error(error.clone()),
        None => {}
    }
    if let Some(runtime) = &slot.runtime {
        for error in runtime.startup_errors() {
            state.report_error(error.clone());
        }
    }
    let mut state = Signal::new(state);
    let mut runtime_slot = Signal::new(slot);
    use_context_provider(|| IntegrationContext {
        state,
        runtime: runtime_slot,
    });

    // On desktop, open the integration on a background tokio task when no pre-opened
    // runtime was supplied so the UI renders immediately. The heavy catalog, migration,
    // and provider setup runs off the Dioxus executor; only the final signal writes
    // happen back on the UI thread.
    #[cfg(feature = "desktop")]
    {
        let needs_open = startup.is_none();
        use_future(move || async move {
            if !needs_open {
                return;
            }
            tracing::info!("desktop integration startup task started");
            let mut startup = tokio::task::spawn(async move { DesktopIntegration::open().await });
            let startup_result = tokio::select! {
                result = &mut startup => result,
                _ = tokio::time::sleep(Duration::from_secs(45)) => {
                    tracing::warn!("desktop integration startup has exceeded 45 seconds; allowing catalog recovery to continue");
                    startup.await
                }
            };
            let result = startup_result.unwrap_or_else(|error| {
                Err(IntegrationError {
                    request_id: None,
                    slice: None,
                    message: format!("Integration startup task panicked: {error}"),
                    retryable: false,
                })
            });
            match result {
                Ok(DesktopStartup::Ready(runtime)) => {
                    tracing::info!("desktop integration startup completed");
                    let _ =
                        crate::INTEGRATION_STARTUP.set(Ok(DesktopStartup::Ready(runtime.clone())));
                    for error in runtime.startup_errors() {
                        state.write().report_error(error.clone());
                    }
                    runtime_slot.write().runtime = Some(runtime);
                }
                Ok(DesktopStartup::OwnershipBlocked(offer)) => {
                    tracing::warn!(owner_device = %offer.owner_device, "desktop integration startup is blocked by another profile manager");
                    let _ = crate::INTEGRATION_STARTUP
                        .set(Ok(DesktopStartup::OwnershipBlocked(offer.clone())));
                    state.write().ownership_blocked_by = Some(offer.owner_device.clone());
                    state
                        .write()
                        .events
                        .push(IntegrationEvent::OwnershipBlocked {
                            owner_device: offer.owner_device.clone(),
                        });
                    runtime_slot.write().takeover = Some(offer);
                }
                Err(error) => {
                    tracing::error!(error = %error, "desktop integration startup failed");
                    let _ = crate::INTEGRATION_STARTUP.set(Err(error.clone()));
                    state.write().report_error(error);
                }
            }
        });
    }

    // React to a runtime that is installed after a confirmed ownership takeover as well as the
    // runtime available at launch. `use_future` runs only once and would otherwise leave a
    // successfully acquired runtime without workers or a projected dashboard.
    use_effect(move || {
        let runtime = runtime_slot.read().runtime.clone();
        let Some(runtime) = runtime else {
            return;
        };
        spawn(async move {
            // The Catalog is composed here, from the runtime's own repositories and its
            // asset resolver, and handed to the application state as a value.
            if let Err(error) = app_state.adopt_catalog(catalog_for(&runtime)).await {
                state.write().report_error(catalog_failure(error));
            }
            if let Err(error) = runtime.start_background().await {
                state.write().report_error(error);
                return;
            }
            state.write().events.push(IntegrationEvent::RuntimeStarted);
            state
                .write()
                .events
                .push(IntegrationEvent::SchedulerStarted);
            // Start slice runners immediately so user-dispatched events are
            // handled without waiting for the initial view projection to finish.
            let mut view_state = state;
            let view_runtime = runtime.clone();
            tokio::join!(
                async move {
                    match view_runtime.initial_view().await {
                        Ok(view) => {
                            view_state.write().view = view;
                            if let Err(error) = app_state.refresh_catalog().await {
                                view_state.write().report_error(catalog_failure(error));
                            }
                        }
                        Err(error) => view_state.write().report_error(error),
                    }
                },
                run_slice(
                    state,
                    app_state,
                    runtime.clone(),
                    IntegrationSlice::LocalLibrary
                ),
                run_slice(
                    state,
                    app_state,
                    runtime.clone(),
                    IntegrationSlice::SoundcloudCatalog
                ),
                run_slice(
                    state,
                    app_state,
                    runtime.clone(),
                    IntegrationSlice::GoogleDrive
                ),
                run_slice(state, app_state, runtime.clone(), IntegrationSlice::Backup),
                run_slice(
                    state,
                    app_state,
                    runtime,
                    IntegrationSlice::ReconciliationActions
                ),
            );
        });
    });
}

async fn run_slice(
    mut state: Signal<IntegrationState>,
    app_state: AppState,
    runtime: Arc<DesktopIntegration>,
    slice: IntegrationSlice,
) {
    tracing::info!(?slice, "integration slice runner started");
    loop {
        // Drain first: a user can queue work while the integration runtime is opening, before
        // this runner has registered its Notify waiter.
        loop {
            let request = {
                let mut snapshot = state.write();
                IntegrationController::begin_next(&mut snapshot, slice)
            };
            let Some(request) = request else {
                break;
            };
            let request_id = request.id;
            tracing::info!(request_id, ?slice, action = ?request.event, "integration request started");
            match runtime.execute(request).await {
                Ok(outcome) => {
                    tracing::info!(request_id, ?slice, message = ?outcome.message, "integration request completed");
                    IntegrationController::complete(&mut state.write(), request_id, outcome);
                    if let Err(error) = app_state.refresh_catalog().await {
                        tracing::warn!(?slice, error = %error, "catalog refresh failed after integration request");
                        state.write().report_error(catalog_failure(error));
                    }
                }
                Err(error) => {
                    let retryable = error.retryable;
                    tracing::warn!(request_id, ?slice, retryable, error = %error, "integration request failed");
                    IntegrationController::fail(
                        &mut state.write(),
                        request_id,
                        error.message,
                        retryable,
                    );
                }
            }
            if runtime.is_shutdown() {
                tracing::info!(?slice, "integration slice runner stopped during shutdown");
                return;
            }
        }

        tracing::debug!(?slice, "integration slice waiting for work");
        if !runtime.wait_for_work(slice).await {
            tracing::info!(?slice, "integration slice runner stopped");
            return;
        }
    }
}

/// Report a Catalog failure through the same surface as integration failures, so a
/// stale dashboard is never the only sign that a read did not happen.
fn catalog_failure(error: CatalogError) -> IntegrationError {
    IntegrationError {
        request_id: None,
        slice: None,
        retryable: error.retryable,
        message: error.to_string(),
    }
}

/// Compose the one Catalog from a live integration runtime.
fn catalog_for(runtime: &Arc<DesktopIntegration>) -> Arc<Catalog> {
    Arc::new(Catalog::new(
        runtime.catalog().clone(),
        AssetPlaybackPaths::new(runtime),
    ))
}

pub fn use_integration_state() -> IntegrationContext {
    use_context::<IntegrationContext>()
}

fn slice_for_event(
    event: &MusicManagementEvent,
    view: &MusicManagementView,
) -> Result<IntegrationSlice, &'static str> {
    let source_slice = |source_id: &str| {
        let source = view
            .sources
            .iter()
            .find(|source| source.id == source_id)
            .ok_or("The selected music source no longer exists")?;
        match source.provider {
            ProviderChoice::LocalFolder => Ok(IntegrationSlice::LocalLibrary),
            ProviderChoice::SoundCloud => Ok(IntegrationSlice::SoundcloudCatalog),
            ProviderChoice::GoogleDrive => Ok(IntegrationSlice::GoogleDrive),
        }
    };
    let storage_slice = |storage_id: &str| {
        let storage = view
            .primary_storage
            .iter()
            .chain(view.backup_storages.iter())
            .chain(view.unassigned_storages.iter())
            .find(|storage| storage.id == storage_id)
            .ok_or("The selected music storage no longer exists")?;
        match storage.provider {
            ProviderChoice::LocalFolder => Ok(IntegrationSlice::LocalLibrary),
            ProviderChoice::GoogleDrive => Ok(IntegrationSlice::GoogleDrive),
            ProviderChoice::SoundCloud => Err("SoundCloud cannot be used as music storage"),
        }
    };
    let persisted_data_slice = |record_id: &str| {
        let source_provider = view
            .sources
            .iter()
            .find(|source| source.id == record_id)
            .map(|source| source.provider);
        let storage_provider = view
            .primary_storage
            .iter()
            .chain(view.backup_storages.iter())
            .chain(view.unassigned_storages.iter())
            .find(|storage| storage.id == record_id)
            .map(|storage| storage.provider);
        let provider = match (source_provider, storage_provider) {
            (Some(source), Some(storage)) if source != storage => {
                return Err("The selected provider record is ambiguous")
            }
            (Some(provider), _) | (_, Some(provider)) => provider,
            (None, None) => return Err("The selected provider record no longer exists"),
        };
        match provider {
            ProviderChoice::LocalFolder => Ok(IntegrationSlice::LocalLibrary),
            ProviderChoice::SoundCloud if source_provider.is_some() => {
                Ok(IntegrationSlice::SoundcloudCatalog)
            }
            ProviderChoice::GoogleDrive => Ok(IntegrationSlice::GoogleDrive),
            ProviderChoice::SoundCloud => Err("SoundCloud cannot be used as music storage"),
        }
    };

    match event {
        MusicManagementEvent::AddSource(ProviderChoice::LocalFolder)
        | MusicManagementEvent::AddLocalFolderSource { .. } => Ok(IntegrationSlice::LocalLibrary),
        MusicManagementEvent::AddSource(ProviderChoice::SoundCloud)
        | MusicManagementEvent::ConfigureSoundCloud(_) => Ok(IntegrationSlice::SoundcloudCatalog),
        MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive) => {
            Ok(IntegrationSlice::GoogleDrive)
        }
        MusicManagementEvent::SyncSource(source_id) => source_slice(source_id),
        MusicManagementEvent::DisconnectSource { source_id } => match source_slice(source_id)? {
            IntegrationSlice::GoogleDrive => {
                Err("Google Drive must be disconnected as music storage")
            }
            slice => Ok(slice),
        },
        MusicManagementEvent::ClearLocalCache { source_id }
        | MusicManagementEvent::ForgetProviderCatalogData { source_id } => {
            persisted_data_slice(source_id)
        }
        MusicManagementEvent::DisconnectStorage { storage_id } => {
            match storage_slice(storage_id)? {
                IntegrationSlice::GoogleDrive => Ok(IntegrationSlice::GoogleDrive),
                _ => Err("Only Google Drive uses the storage disconnect lifecycle"),
            }
        }
        MusicManagementEvent::RevokeProviderGrant {
            provider,
            connection_id,
        } => match provider {
            ProviderChoice::SoundCloud => match source_slice(connection_id)? {
                IntegrationSlice::SoundcloudCatalog => Ok(IntegrationSlice::SoundcloudCatalog),
                _ => Err("The provider grant does not match the selected music source"),
            },
            ProviderChoice::GoogleDrive => match storage_slice(connection_id)? {
                IntegrationSlice::GoogleDrive => Ok(IntegrationSlice::GoogleDrive),
                _ => Err("The provider grant does not match the selected music storage"),
            },
            ProviderChoice::LocalFolder => {
                Err("Local folders do not have provider authorization grants")
            }
        },
        MusicManagementEvent::CreatePrimary(ui::PrimaryConfigurationView {
            provider: ProviderChoice::GoogleDrive,
            ..
        })
        | MusicManagementEvent::PreparePrimaryMigration(ui::PrimaryMigrationPlanView {
            destination:
                ui::PrimaryConfigurationView {
                    provider: ProviderChoice::GoogleDrive,
                    ..
                },
            ..
        }) => Ok(IntegrationSlice::GoogleDrive),
        MusicManagementEvent::CreatePrimary(_)
        | MusicManagementEvent::UpdatePrimaryMetadata(_)
        | MusicManagementEvent::PreparePrimaryMigration(_) => Ok(IntegrationSlice::LocalLibrary),
        MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder) => {
            Ok(IntegrationSlice::LocalLibrary)
        }
        MusicManagementEvent::AddBackup(ProviderChoice::GoogleDrive) => {
            Ok(IntegrationSlice::GoogleDrive)
        }
        MusicManagementEvent::AddBackup(ProviderChoice::SoundCloud)
        | MusicManagementEvent::SaveBackupPolicy(_)
        | MusicManagementEvent::StartCleanupReview
        | MusicManagementEvent::ConfirmCleanup(_) => Ok(IntegrationSlice::Backup),
        MusicManagementEvent::RetryStorage(storage_id) => Ok(view
            .primary_storage
            .iter()
            .chain(view.backup_storages.iter())
            .chain(view.unassigned_storages.iter())
            .find(|storage| storage.id == *storage_id)
            .map(|storage| match storage.provider {
                ProviderChoice::LocalFolder => IntegrationSlice::LocalLibrary,
                ProviderChoice::GoogleDrive => IntegrationSlice::GoogleDrive,
                ProviderChoice::SoundCloud => IntegrationSlice::Backup,
            })
            .unwrap_or(IntegrationSlice::ReconciliationActions)),
        MusicManagementEvent::ToggleOfflinePin(_, _) => Ok(IntegrationSlice::GoogleDrive),
        MusicManagementEvent::CopyToPrimary(_) => Ok(view
            .primary_storage
            .as_ref()
            .map(|storage| match storage.provider {
                ProviderChoice::LocalFolder => IntegrationSlice::LocalLibrary,
                ProviderChoice::GoogleDrive => IntegrationSlice::GoogleDrive,
                ProviderChoice::SoundCloud => IntegrationSlice::ReconciliationActions,
            })
            .unwrap_or(IntegrationSlice::ReconciliationActions)),
        MusicManagementEvent::RetrySyncRun { .. }
        | MusicManagementEvent::ReviewMatch(_)
        | MusicManagementEvent::ConfirmMatch(_)
        | MusicManagementEvent::RejectMatch(_)
        | MusicManagementEvent::AssociateLocalFile { .. }
        | MusicManagementEvent::AssociateUnknownFile { .. }
        | MusicManagementEvent::ApproveExternalChange { .. }
        | MusicManagementEvent::RetryReconciliationResult { .. }
        | MusicManagementEvent::ImportUnknownFile(_)
        | MusicManagementEvent::IgnoreUnknownFile(_) => Ok(IntegrationSlice::ReconciliationActions),
        MusicManagementEvent::SetTrackFilter(_) => unreachable!("handled by dispatch"),
    }
}

fn empty_music_management_view() -> MusicManagementView {
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
    use super::*;
    use ui::mock_music_management_view;

    #[test]
    fn dispatch_routes_provider_events_to_owned_slices() {
        let mut state = IntegrationState::default();

        let local_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::AddSource(ProviderChoice::LocalFolder),
        );
        let soundcloud_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::AddSource(ProviderChoice::SoundCloud),
        );
        let drive_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::AddBackup(ProviderChoice::GoogleDrive),
        );

        assert_eq!(local_id, Some(1));
        assert_eq!(soundcloud_id, Some(2));
        assert_eq!(drive_id, Some(3));
        let slices: Vec<_> = state
            .pending_requests()
            .map(|request| request.slice)
            .collect();
        assert_eq!(
            slices,
            vec![
                IntegrationSlice::LocalLibrary,
                IntegrationSlice::SoundcloudCatalog,
                IntegrationSlice::GoogleDrive,
            ]
        );
    }

    #[test]
    fn dispatch_routes_lifecycle_events_by_persisted_provider() {
        let mut state = IntegrationState {
            view: mock_music_management_view(),
            ..IntegrationState::default()
        };
        let events = [
            MusicManagementEvent::DisconnectSource {
                source_id: "soundcloud-main".to_owned(),
            },
            MusicManagementEvent::ClearLocalCache {
                source_id: "local-crate".to_owned(),
            },
            MusicManagementEvent::ForgetProviderCatalogData {
                source_id: "soundcloud-main".to_owned(),
            },
            MusicManagementEvent::DisconnectStorage {
                storage_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::ClearLocalCache {
                source_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::ForgetProviderCatalogData {
                source_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::GoogleDrive,
                connection_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::SoundCloud,
                connection_id: "soundcloud-main".to_owned(),
            },
        ];

        for event in events {
            assert!(IntegrationController::dispatch(&mut state, event).is_some());
        }

        assert_eq!(
            state
                .pending_requests()
                .map(|request| request.slice)
                .collect::<Vec<_>>(),
            vec![
                IntegrationSlice::SoundcloudCatalog,
                IntegrationSlice::LocalLibrary,
                IntegrationSlice::SoundcloudCatalog,
                IntegrationSlice::GoogleDrive,
                IntegrationSlice::GoogleDrive,
                IntegrationSlice::GoogleDrive,
                IntegrationSlice::GoogleDrive,
                IntegrationSlice::SoundcloudCatalog,
            ]
        );
    }

    #[test]
    fn dispatch_rejects_missing_and_mismatched_lifecycle_providers() {
        let mut state = IntegrationState {
            view: mock_music_management_view(),
            ..IntegrationState::default()
        };
        let invalid_events = [
            MusicManagementEvent::DisconnectStorage {
                storage_id: "missing-storage".to_owned(),
            },
            MusicManagementEvent::DisconnectStorage {
                storage_id: "primary-local".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::SoundCloud,
                connection_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::LocalFolder,
                connection_id: "local-crate".to_owned(),
            },
        ];

        for event in invalid_events {
            assert_eq!(IntegrationController::dispatch(&mut state, event), None);
        }

        assert_eq!(state.pending_requests().count(), 0);
        assert_eq!(state.errors.len(), 4);
        assert!(state.errors.iter().all(|error| !error.retryable));
        assert!(state
            .events
            .iter()
            .all(|event| matches!(event, IntegrationEvent::RequestFailed(_))));
    }

    #[test]
    fn filter_dispatch_is_reduced_without_provider_work() {
        let mut state = IntegrationState::default();

        let request_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::SetTrackFilter(TrackFilter::Missing),
        );

        assert_eq!(request_id, None);
        assert_eq!(state.selected_filter, TrackFilter::Missing);
        assert_eq!(state.pending_requests().count(), 0);
        assert_eq!(
            state.events,
            vec![IntegrationEvent::FilterChanged(TrackFilter::Missing)]
        );
    }

    #[test]
    fn successful_request_replaces_the_dashboard_projection() {
        let mut state = IntegrationState {
            view: mock_music_management_view(),
            ..IntegrationState::default()
        };
        let request_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::SyncSource("soundcloud-main".to_string()),
        )
        .unwrap();
        IntegrationController::begin_next(&mut state, IntegrationSlice::SoundcloudCatalog).unwrap();

        let mut updated_view = empty_music_management_view();
        updated_view.completeness.canonical_tracks = 12;
        assert!(IntegrationController::complete(
            &mut state,
            request_id,
            IntegrationOutcome {
                view: Some(updated_view),
                message: Some("sync complete".to_string()),
            },
        ));

        assert_eq!(state.view.completeness.canonical_tracks, 12);
        assert_eq!(state.in_flight_requests().count(), 0);
        assert!(matches!(
            state.events.last(),
            Some(IntegrationEvent::RequestCompleted { message: Some(message), .. })
                if message == "sync complete"
        ));
    }

    #[test]
    fn stage4_lifecycle_events_are_retained_in_action_order() {
        let mut state = IntegrationState::default();
        state.events.extend([
            IntegrationEvent::OwnershipBlocked {
                owner_device: "device-a".into(),
            },
            IntegrationEvent::TakeoverStarted,
            IntegrationEvent::TakeoverCompleted,
            IntegrationEvent::RuntimeStarted,
            IntegrationEvent::SchedulerStarted,
            IntegrationEvent::RecoveryStarted(RecoveryAction::DryRun),
            IntegrationEvent::RecoveryCompleted {
                action: RecoveryAction::DryRun,
                progress: Vec::new(),
            },
        ]);

        assert!(matches!(
            state.events.as_slice(),
            [
                IntegrationEvent::OwnershipBlocked { .. },
                IntegrationEvent::TakeoverStarted,
                IntegrationEvent::TakeoverCompleted,
                IntegrationEvent::RuntimeStarted,
                IntegrationEvent::SchedulerStarted,
                IntegrationEvent::RecoveryStarted(RecoveryAction::DryRun),
                IntegrationEvent::RecoveryCompleted {
                    action: RecoveryAction::DryRun,
                    ..
                },
            ]
        ));
    }

    #[test]
    fn request_lifecycle_updates_activity_and_error_boundaries() {
        let mut state = IntegrationState {
            view: mock_music_management_view(),
            ..IntegrationState::default()
        };
        let request_id = IntegrationController::dispatch(
            &mut state,
            MusicManagementEvent::CopyToPrimary("track-1".to_string()),
        )
        .unwrap();

        let request =
            IntegrationController::begin_next(&mut state, IntegrationSlice::LocalLibrary).unwrap();
        assert_eq!(request.id, request_id);
        assert_eq!(state.pending_requests().count(), 0);
        assert_eq!(state.in_flight_requests().count(), 1);

        assert!(IntegrationController::fail(
            &mut state,
            request_id,
            "primary unavailable",
            true,
        ));
        assert_eq!(state.in_flight_requests().count(), 0);
        assert_eq!(state.errors.len(), 1);
        assert!(state.errors[0].retryable);
        assert_eq!(
            state.failed_event(Some(request_id)),
            Some(&MusicManagementEvent::CopyToPrimary("track-1".to_owned()))
        );
        assert!(matches!(
            state.events.last(),
            Some(IntegrationEvent::RequestFailed(_))
        ));
    }
}
