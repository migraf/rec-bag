//! Provider-neutral state and dispatch boundary for music-management integrations.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use crate::{
    services::{
        backup::DatabaseBackup,
        catalog::{Catalog, CatalogError},
        integration::{DesktopIntegration, DesktopStartup, OwnershipTakeoverOffer},
        playback_bridge::AssetPlaybackPaths,
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
    /// Every music storage, whatever its provider.
    MusicStorage,
    Backup,
    ReconciliationActions,
}

impl IntegrationSlice {
    pub const ALL: [Self; 5] = [
        Self::LocalLibrary,
        Self::SoundcloudCatalog,
        Self::MusicStorage,
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
    DatabaseBackupStarted(DatabaseBackupAction),
    DatabaseBackupCompleted {
        action: DatabaseBackupAction,
        /// Where a restored database waits for the next start, when one was staged.
        staged: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseBackupAction {
    Refresh,
    Back,
    Restore,
}

/// State projected into the provider-independent dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationState {
    pub view: MusicManagementView,
    pub selected_filter: TrackFilter,
    pub events: Vec<IntegrationEvent>,
    pub errors: Vec<IntegrationError>,
    pub database_backups: Vec<DatabaseBackup>,
    /// Set once a restore has been staged; it takes effect on the next start.
    pub staged_restore: Option<PathBuf>,
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
            database_backups: Vec::new(),
            staged_restore: None,
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

    /// Reloads the list of database copies held by the configured backup storages.
    pub async fn refresh_database_backups(&mut self) {
        self.push_backup_started(DatabaseBackupAction::Refresh);
        let Some(runtime) = self.runtime() else {
            return;
        };
        match runtime.database_backups().await {
            Ok(backups) => {
                self.state.write().database_backups = backups;
                self.push_backup_completed(DatabaseBackupAction::Refresh, None);
            }
            Err(error) => self.state.write().report_error(error),
        }
    }

    /// Copies the catalog database to every configured backup storage.
    pub async fn back_up_database(&mut self) {
        self.push_backup_started(DatabaseBackupAction::Back);
        let Some(runtime) = self.runtime() else {
            return;
        };
        match runtime.back_up_database().await {
            Ok(_) => self.push_backup_completed(DatabaseBackupAction::Back, None),
            Err(error) => self.state.write().report_error(error),
        }
    }

    /// Stages one backup to replace the catalog on the next start.
    pub async fn restore_database_backup(&mut self, backup: DatabaseBackup) {
        self.push_backup_started(DatabaseBackupAction::Restore);
        let Some(runtime) = self.runtime() else {
            return;
        };
        match runtime.restore_database_backup(&backup).await {
            Ok(staged) => {
                self.state.write().staged_restore = Some(staged.clone());
                self.push_backup_completed(DatabaseBackupAction::Restore, Some(staged));
            }
            Err(error) => self.state.write().report_error(error),
        }
    }

    fn push_backup_started(&mut self, action: DatabaseBackupAction) {
        self.state
            .write()
            .events
            .push(IntegrationEvent::DatabaseBackupStarted(action));
    }

    fn push_backup_completed(&mut self, action: DatabaseBackupAction, staged: Option<PathBuf>) {
        self.state
            .write()
            .events
            .push(IntegrationEvent::DatabaseBackupCompleted { action, staged });
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
        use std::time::Duration;

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
                    tracing::warn!("desktop integration startup has exceeded 45 seconds; still waiting");
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
                    IntegrationSlice::MusicStorage
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
    Arc::new(
        Catalog::new(runtime.catalog().clone(), AssetPlaybackPaths::new(runtime))
            .with_streams(runtime.track_streams()),
    )
}

pub fn use_integration_state() -> IntegrationContext {
    use_context::<IntegrationContext>()
}

fn slice_for_event(
    event: &MusicManagementEvent,
    view: &MusicManagementView,
) -> Result<IntegrationSlice, &'static str> {
    let source = |source_id: &str| view.sources.iter().find(|source| source.id == source_id);
    let storage = |storage_id: &str| {
        view.primary_storage
            .iter()
            .chain(view.backup_storages.iter())
            .chain(view.unassigned_storages.iter())
            .find(|storage| storage.id == storage_id)
    };
    let source_slice = |source_id: &str| {
        let source = source(source_id).ok_or("The selected music source no longer exists")?;
        match source.provider {
            ProviderChoice::LocalFolder => Ok(IntegrationSlice::LocalLibrary),
            ProviderChoice::SoundCloud => Ok(IntegrationSlice::SoundcloudCatalog),
            ProviderChoice::GoogleDrive => Ok(IntegrationSlice::MusicStorage),
        }
    };
    let existing_storage = |storage_id: &str| {
        storage(storage_id)
            .map(|_| IntegrationSlice::MusicStorage)
            .ok_or("The selected music storage no longer exists")
    };
    // Local data held for a record: a storage's belongs to the storage slice,
    // a source's to the slice owning that source.
    let persisted_data_slice = |record_id: &str| match (source(record_id), storage(record_id)) {
        (Some(_), Some(_)) => Err("The selected provider record is ambiguous"),
        (_, Some(_)) => Ok(IntegrationSlice::MusicStorage),
        (Some(_), None) => source_slice(record_id),
        (None, None) => Err("The selected provider record no longer exists"),
    };

    match event {
        MusicManagementEvent::AddSource(ProviderChoice::LocalFolder)
        | MusicManagementEvent::AddLocalFolderSource { .. } => Ok(IntegrationSlice::LocalLibrary),
        MusicManagementEvent::AddSource(ProviderChoice::SoundCloud)
        | MusicManagementEvent::ConfigureSoundCloud(_) => Ok(IntegrationSlice::SoundcloudCatalog),
        // Refused by the storage slice, which explains why.
        MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive) => {
            Ok(IntegrationSlice::MusicStorage)
        }
        MusicManagementEvent::SyncSource(source_id) => source_slice(source_id),
        MusicManagementEvent::DisconnectSource { source_id } => match source_slice(source_id)? {
            IntegrationSlice::MusicStorage => {
                Err("Google Drive must be disconnected as music storage")
            }
            slice => Ok(slice),
        },
        MusicManagementEvent::ClearLocalCache { source_id }
        | MusicManagementEvent::ForgetProviderCatalogData { source_id } => {
            persisted_data_slice(source_id)
        }
        MusicManagementEvent::DisconnectStorage { storage_id } => existing_storage(storage_id),
        MusicManagementEvent::RevokeProviderGrant {
            provider,
            connection_id,
        } => match provider {
            ProviderChoice::SoundCloud => match source_slice(connection_id)? {
                IntegrationSlice::SoundcloudCatalog => Ok(IntegrationSlice::SoundcloudCatalog),
                _ => Err("The provider grant does not match the selected music source"),
            },
            ProviderChoice::GoogleDrive => {
                match storage(connection_id).map(|storage| storage.provider) {
                    Some(ProviderChoice::GoogleDrive) => Ok(IntegrationSlice::MusicStorage),
                    Some(_) => Err("The provider grant does not match the selected music storage"),
                    None => Err("The selected music storage no longer exists"),
                }
            }
            ProviderChoice::LocalFolder => {
                Err("Local folders do not have provider authorization grants")
            }
        },
        MusicManagementEvent::CreatePrimary(_)
        | MusicManagementEvent::UpdatePrimaryMetadata(_)
        | MusicManagementEvent::PreparePrimaryMigration(_)
        | MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder)
        | MusicManagementEvent::AddBackup(ProviderChoice::GoogleDrive)
        | MusicManagementEvent::ToggleOfflinePin(_, _)
        | MusicManagementEvent::CopyToPrimary(_) => Ok(IntegrationSlice::MusicStorage),
        MusicManagementEvent::AddBackup(ProviderChoice::SoundCloud)
        | MusicManagementEvent::SaveBackupPolicy(_)
        | MusicManagementEvent::StartCleanupReview
        | MusicManagementEvent::ConfirmCleanup(_) => Ok(IntegrationSlice::Backup),
        MusicManagementEvent::RetryStorage(storage_id) => Ok(storage(storage_id)
            .map(|_| IntegrationSlice::MusicStorage)
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
                IntegrationSlice::MusicStorage,
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
            // Local storages share the one storage lifecycle.
            MusicManagementEvent::DisconnectStorage {
                storage_id: "primary-local".to_owned(),
            },
            MusicManagementEvent::ForgetProviderCatalogData {
                source_id: "primary-candidate".to_owned(),
            },
            MusicManagementEvent::RetryStorage("primary-local".to_owned()),
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
                IntegrationSlice::MusicStorage,
                IntegrationSlice::MusicStorage,
                IntegrationSlice::MusicStorage,
                IntegrationSlice::MusicStorage,
                IntegrationSlice::SoundcloudCatalog,
                IntegrationSlice::MusicStorage,
                IntegrationSlice::MusicStorage,
                IntegrationSlice::MusicStorage,
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
            // A source is not a storage.
            MusicManagementEvent::DisconnectStorage {
                storage_id: "local-crate".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::SoundCloud,
                connection_id: "backup-drive".to_owned(),
            },
            MusicManagementEvent::RevokeProviderGrant {
                provider: ProviderChoice::GoogleDrive,
                connection_id: "primary-local".to_owned(),
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
        assert_eq!(state.errors.len(), 5);
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
            IntegrationEvent::DatabaseBackupStarted(DatabaseBackupAction::Back),
            IntegrationEvent::DatabaseBackupCompleted {
                action: DatabaseBackupAction::Back,
                staged: None,
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
                IntegrationEvent::DatabaseBackupStarted(DatabaseBackupAction::Back),
                IntegrationEvent::DatabaseBackupCompleted {
                    action: DatabaseBackupAction::Back,
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
            IntegrationController::begin_next(&mut state, IntegrationSlice::MusicStorage).unwrap();
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
