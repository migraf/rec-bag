//! Desktop adapter for the provider-independent music-management dashboard.

use dioxus::prelude::*;
use std::path::PathBuf;
use ui::{MusicManagementDashboard, MusicManagementEvent, MusicManagementView, ProviderChoice};

use crate::{
    services::recovery::{CatalogAuthority, SnapshotListing, SnapshotTransferConfirmation},
    state::integration::{
        use_integration_state, IntegrationError, IntegrationEvent, IntegrationSlice, RecoveryAction,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SafeErrorKind {
    Authentication,
    RateLimited,
    Connectivity,
    Permission,
    Configuration,
    Unexpected,
}

impl SafeErrorKind {
    fn classify(untrusted: &str) -> Self {
        let value = untrusted.to_ascii_lowercase();
        if contains_any(
            &value,
            &[
                "oauth",
                "auth",
                "credential",
                "token",
                "unauthorized",
                "401",
                "403",
            ],
        ) {
            Self::Authentication
        } else if contains_any(&value, &["rate limit", "too many requests", "429"]) {
            Self::RateLimited
        } else if contains_any(
            &value,
            &[
                "offline",
                "network",
                "timeout",
                "timed out",
                "unavailable",
                "connection",
            ],
        ) {
            Self::Connectivity
        } else if contains_any(&value, &["permission", "access denied", "read-only"]) {
            Self::Permission
        } else if contains_any(
            &value,
            &["configuration", "configured", "not registered", "not found"],
        ) {
            Self::Configuration
        } else {
            Self::Unexpected
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Authentication => "Connection authorization expired",
            Self::RateLimited => "Provider is temporarily busy",
            Self::Connectivity => "Provider could not be reached",
            Self::Permission => "Provider access needs attention",
            Self::Configuration => "Provider setup needs attention",
            Self::Unexpected => "Music operation did not complete",
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::Authentication => "Reconnect the provider, then retry the operation.",
            Self::RateLimited => {
                "Wait a moment, then retry. Your existing catalog data is unchanged."
            }
            Self::Connectivity => "Check your connection and provider availability, then retry.",
            Self::Permission => {
                "Review the provider permissions or selected location before retrying."
            }
            Self::Configuration => "Review the provider configuration before retrying.",
            Self::Unexpected => {
                "Retry the operation. If it fails again, copy the safe diagnostics for support."
            }
        }
    }

    fn diagnostic_code(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::Connectivity => "connectivity",
            Self::Permission => "permission",
            Self::Configuration => "configuration",
            Self::Unexpected => "unexpected",
        }
    }
}

fn snapshot_date(value: Option<chrono::DateTime<chrono::Utc>>) -> String {
    value.map_or_else(|| "Unknown date".into(), |date| date.to_rfc3339())
}

fn contains_any(value: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| value.contains(marker))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DryRunReview {
    listing: SnapshotListing,
    foreign_authority: Option<CatalogAuthority>,
    transfer_confirmation_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRecoveryRestore {
    path: PathBuf,
    snapshot_id: Option<uuid::Uuid>,
    authority: Option<CatalogAuthority>,
    confirmation: Option<SnapshotTransferConfirmation>,
    rollback: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ErrorAccessibilityState {
    expanded: Option<usize>,
    copied: Option<usize>,
}

impl ErrorAccessibilityState {
    fn open(&mut self, index: usize) {
        self.expanded = Some(index);
        self.copied = None;
    }

    fn close(&mut self) {
        self.expanded = None;
        self.copied = None;
    }

    fn after_dismiss(&mut self, index: usize) {
        self.expanded = self.expanded.and_then(|open| {
            if open == index {
                None
            } else if open > index {
                Some(open - 1)
            } else {
                Some(open)
            }
        });
        self.copied = self.copied.and_then(|copied| {
            if copied == index {
                None
            } else if copied > index {
                Some(copied - 1)
            } else {
                Some(copied)
            }
        });
    }
}

#[component]
pub fn MusicManagement() -> Element {
    let integration = use_integration_state();
    let snapshot = integration.state.read();
    let view = snapshot.view.clone();
    let pending_count = snapshot.pending_requests().count();
    let in_flight_count = snapshot.in_flight_requests().count();
    let errors = snapshot.errors.clone();
    let retry_events: Vec<_> = errors
        .iter()
        .map(|error| snapshot.failed_event(error.request_id).cloned())
        .collect();
    let events = snapshot.events.clone();
    let ownership_blocked_by = snapshot.ownership_blocked_by.clone();
    let recovery_snapshots = snapshot.recovery_snapshots.clone();
    let rollback_snapshot = snapshot.last_pre_restore_snapshot.clone();
    drop(snapshot);

    let mut presentation = use_signal(ErrorAccessibilityState::default);
    let mut confirm_takeover = use_signal(|| false);
    let mut confirm_restore = use_signal(|| None::<PendingRecoveryRestore>);
    let mut dry_run_review = use_signal(|| None::<DryRunReview>);
    let mut integration_state = integration.state;
    let mut dashboard_integration = integration.clone();
    let takeover_integration = integration.clone();
    let recovery_refresh_integration = integration.clone();
    let recovery_action_integration = integration.clone();
    let transfer_prepare_integration = integration.clone();
    let transfer_confirm_integration = integration.clone();
    let transfer_cancel_integration = integration.clone();
    let runtime_ready = integration.runtime().is_some();

    rsx! {
        if let Some(owner) = ownership_blocked_by {
            section { class: "mm-error-banner", role: "alert",
                div { class: "mm-error-content",
                    h2 { "This profile is active on another manager" }
                    p { "Current owner: {owner}. Catalog writes are disabled until ownership is explicitly transferred." }
                    if !confirm_takeover() {
                        button {
                            class: "mm-button secondary",
                            onclick: move |_| confirm_takeover.set(true),
                            "Review ownership takeover"
                        }
                    } else {
                        p { "Taking ownership can interrupt work on the other manager. Continue only if that session should stop." }
                        button {
                            class: "mm-button primary",
                            onclick: move |_| {
                                let mut takeover = takeover_integration.clone();
                                confirm_takeover.set(false);
                                spawn(async move { takeover.assume_ownership().await; });
                            },
                            "Confirm and assume ownership"
                        }
                        button { class: "mm-button secondary", onclick: move |_| confirm_takeover.set(false), "Cancel" }
                    }
                }
            }
        }
        if runtime_ready {
            section { class: "mm-section", "aria-label": "Catalog recovery",
                div { class: "mm-section-header",
                    div {
                        h2 { "Catalog recovery" }
                        p { "Verify snapshots before restoring. Every restore creates a rollback snapshot." }
                    }
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| {
                            let mut recovery = recovery_refresh_integration.clone();
                            spawn(async move { recovery.refresh_recovery_snapshots().await; });
                        },
                        "Refresh snapshots"
                    }
                }
                if recovery_snapshots.is_empty() {
                    p { "No local recovery snapshots are listed yet." }
                } else {
                    ul {
                        for listing in recovery_snapshots.iter() {
                            li { key: "{listing.path.display()}",
                                strong { "{snapshot_date(listing.created_at)}" }
                                span { " · {listing.health:?}" }
                                if let Some(authority) = listing.authority.as_ref() {
                                    span {
                                        " · profile {authority.profile_id}, device {authority.manager_device}, generation {authority.ownership_generation}"
                                    }
                                }
                                button {
                                    class: "mm-link-button",
                                    disabled: listing.health != crate::services::recovery::SnapshotHealth::Valid,
                                    onclick: {
                                        let listing = listing.clone();
                                        let runtime = recovery_action_integration.runtime();
                                        move |_| {
                                            let listing = listing.clone();
                                            let runtime = runtime.clone();
                                            spawn(async move {
                                                let Some(runtime) = runtime else { return; };
                                                match runtime.dry_run_restore(listing.path.clone()).await {
                                                    Ok(report) => dry_run_review.set(Some(DryRunReview {
                                                        listing,
                                                        foreign_authority: report.foreign_authority,
                                                        transfer_confirmation_required: report.transfer_confirmation_required,
                                                    })),
                                                    Err(error) => integration_state.write().report_error(error),
                                                }
                                            });
                                        }
                                    },
                                    "Dry run"
                                }

                            }
                        }
                    }
                }
                if let Some(review) = dry_run_review() {
                    aside { class: "mm-activity-banner", role: "status", "aria-live": "polite",
                        h3 { "Snapshot dry-run passed" }
                        p { "Integrity, schema, secret filtering, and exact replacement checks completed without changing the catalog." }
                        if let Some(authority) = review.foreign_authority.as_ref() {
                            p {
                                "Foreign authority: profile {authority.profile_id}, device {authority.manager_device}, generation {authority.ownership_generation}. Explicit transfer confirmation is required for restore."
                            }
                        }
                        button {
                            class: "mm-button primary",
                            onclick: {
                                let review = review.clone();
                                let runtime = transfer_prepare_integration.runtime();
                                move |_| {
                                    let review = review.clone();
                                    let runtime = runtime.clone();
                                    spawn(async move {
                                        let Some(runtime) = runtime else { return; };
                                        match runtime.prepare_snapshot_transfer(&review.listing).await {
                                            Ok(confirmation) => confirm_restore.set(Some(PendingRecoveryRestore {
                                                path: review.listing.path,
                                                snapshot_id: review.listing.id,
                                                authority: review.listing.authority,
                                                confirmation,
                                                rollback: false,
                                            })),
                                            Err(error) => integration_state.write().report_error(error),
                                        }
                                    });
                                }
                            },
                            if review.transfer_confirmation_required { "Review and confirm ownership transfer" } else { "Review and confirm restore" }
                        }
                    }
                }
                if let Some(path) = rollback_snapshot.clone() {
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| confirm_restore.set(Some(PendingRecoveryRestore {
                            path: path.clone(),
                            snapshot_id: None,
                            authority: None,
                            confirmation: None,
                            rollback: true,
                        })),
                        "Roll back last restore"
                    }
                }
                if let Some(pending) = confirm_restore() {
                    div {
                        class: "mm-error-banner",
                        role: "alertdialog",
                        "aria-modal": "true",
                        "aria-labelledby": "snapshot-transfer-title",
                        "aria-describedby": "snapshot-transfer-description",
                        tabindex: "-1",
                        h3 {
                            id: "snapshot-transfer-title",
                            if pending.rollback {
                                "Confirm catalog rollback"
                            } else if pending.confirmation.is_some() {
                                "Confirm snapshot ownership transfer"
                            } else {
                                "Confirm catalog restore"
                            }
                        }
                        p { id: "snapshot-transfer-description", "This replaces the current catalog after verification. Provider credentials are not restored." }
                        if let Some(authority) = pending.authority.as_ref() {
                            dl {
                                dt { "Snapshot profile" }
                                dd { "{authority.profile_id}" }
                                dt { "Previous manager device" }
                                dd { "{authority.manager_device}" }
                                dt { "Ownership generation" }
                                dd { "{authority.ownership_generation}" }
                                if let Some(snapshot_id) = pending.snapshot_id {
                                    dt { "Snapshot ID" }
                                    dd { "{snapshot_id}" }
                                }
                            }
                            if let Some(confirmation) = pending.confirmation.as_ref() {
                                p { "Confirming transfers this snapshot to the current manager and advances its ownership generation. This confirmation expires at {confirmation.valid_until().to_rfc3339()}." }
                            } else {
                                p { "This snapshot authority already matches the current manager." }
                            }
                        }
                        button {
                            class: "mm-button primary",
                            autofocus: true,
                            onclick: {
                                let pending = pending.clone();
                                let integration = transfer_confirm_integration.clone();
                                move |_| {
                                let selected = pending.clone();
                                let mut recovery = recovery_action_integration.clone();
                                let runtime = integration.runtime();
                                confirm_restore.set(None);
                                spawn(async move {
                                    if let Some(confirmation) = selected.confirmation {
                                        let Some(runtime) = runtime else { return; };
                                        match runtime.restore_snapshot_with_confirmation(selected.path, confirmation).await {
                                            Ok(report) => {
                                                if !report.pre_restore_snapshot.as_os_str().is_empty() {
                                                    recovery.state.write().last_pre_restore_snapshot = Some(report.pre_restore_snapshot);
                                                }
                                                recovery.refresh_recovery_snapshots().await;
                                            }
                                            Err(error) => recovery.state.write().report_error(error),
                                        }
                                    } else {
                                        let action = if selected.rollback { RecoveryAction::Rollback } else { RecoveryAction::Restore };
                                        recovery.run_recovery(action, selected.path).await;
                                    }
                                });
                                }
                            },
                            if pending.rollback {
                                "Confirm rollback"
                            } else if pending.confirmation.is_some() {
                                "Confirm ownership transfer and restore"
                            } else {
                                "Confirm restore"
                            }
                        }
                        button {
                            class: "mm-button secondary",
                            onclick: {
                                let pending = pending.clone();
                                let integration = transfer_cancel_integration.clone();
                                move |_| {
                                    if let (Some(runtime), Some(confirmation)) = (integration.runtime(), pending.confirmation.as_ref()) {
                                        if let Err(error) = runtime.cancel_snapshot_transfer(confirmation) {
                                            integration_state.write().report_error(error);
                                        }
                                    }
                                    confirm_restore.set(None);
                                }
                            },
                            "Cancel"
                        }
                    }
                }
            }
        }
        if pending_count + in_flight_count > 0 {
            aside { class: "mm-activity-banner", role: "status", "aria-live": "polite",
                span { class: "mm-activity-indicator", "" }
                span { "Integration work: {pending_count} queued, {in_flight_count} running" }
            }
        }
        if !errors.is_empty() {
            section { class: "mm-error-region", "aria-label": "Music management errors",
                for (index, error) in errors.iter().cloned().enumerate() {
                    ErrorBanner {
                        key: "integration-error-{error.request_id:?}-{index}",
                        index,
                        error: error.clone(),
                        view: view.clone(),
                        failed_event: retry_events[index].clone(),
                        on_details: move |error_index| presentation.write().open(error_index),
                        on_retry: {
                            let view = view.clone();
                            let error = error.clone();
                            let failed_event = retry_events[index].clone();
                            let mut retry_integration = integration.clone();
                            move |_| {
                                if let Some(event) = retry_event(&error, &view, failed_event.as_ref()) {
                                    retry_integration.dispatch(event);
                                }
                            }
                        },
                        on_reconnect: {
                            let error = error.clone();
                            let mut reconnect_integration = integration.clone();
                            move |_| {
                                if let Some(event) = reconnect_event(&error) {
                                    reconnect_integration.dispatch(event);
                                }
                            }
                        },
                        on_dismiss: move |error_index| {
                            presentation.write().after_dismiss(error_index);
                            integration_state.write().dismiss_error(error_index);
                        },
                    }
                }
            }
        }
        MusicManagementDashboard {
            view: view.clone(),
            on_event: move |event| {
                dashboard_integration.dispatch(event);
            },
        }
        if let Some(index) = presentation().expanded {
            if let Some(error) = errors.get(index).cloned() {
                ErrorDetailsDialog {
                    index,
                    error: error.clone(),
                    run_id: related_run_id(&error, &view),
                    history: progress_history(&error, &events),
                    copied: presentation().copied == Some(index),
                    on_copied: move |copied_index| presentation.write().copied = Some(copied_index),
                    on_close: move |_| presentation.write().close(),
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ErrorBannerProps {
    index: usize,
    error: IntegrationError,
    view: MusicManagementView,
    failed_event: Option<MusicManagementEvent>,
    on_details: EventHandler<usize>,
    on_retry: EventHandler<()>,
    on_reconnect: EventHandler<()>,
    on_dismiss: EventHandler<usize>,
}

#[component]
fn ErrorBanner(props: ErrorBannerProps) -> Element {
    let kind = SafeErrorKind::classify(&props.error.message);
    let provider = provider_label(props.error.slice);
    let retry_available =
        retry_event(&props.error, &props.view, props.failed_event.as_ref()).is_some();
    let reconnect_available =
        kind == SafeErrorKind::Authentication && reconnect_event(&props.error).is_some();
    let settings_href = settings_href(props.error.slice);
    let title_id = format!("mm-error-title-{}", props.index);

    rsx! {
        article {
            class: "mm-error-banner",
            role: "alert",
            "aria-labelledby": "{title_id}",
            div { class: "mm-error-icon", "aria-hidden": "true", "!" }
            div { class: "mm-error-content",
                div { class: "mm-error-heading",
                    div {
                        h2 { id: "{title_id}", "{kind.label()}" }
                        p { "{provider} · {request_label(props.error.request_id)}" }
                    }
                    button {
                        class: "mm-modal-close",
                        "aria-label": "Dismiss {kind.label()}",
                        onclick: move |_| props.on_dismiss.call(props.index),
                        "×"
                    }
                }
                p { class: "mm-error-guidance", "{kind.guidance()}" }
                div { class: "mm-error-actions",
                    if retry_available {
                        button { class: "mm-button primary", onclick: move |_| props.on_retry.call(()), "Retry" }
                    }
                    if reconnect_available {
                        button { class: "mm-button secondary", onclick: move |_| props.on_reconnect.call(()), "Reconnect" }
                    }
                    a { class: "mm-button secondary mm-button-link", href: "{settings_href}", "Open settings" }
                    button { class: "mm-link-button", onclick: move |_| props.on_details.call(props.index), "View safe details" }
                    button { class: "mm-link-button", onclick: move |_| props.on_dismiss.call(props.index), "Dismiss" }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ErrorDetailsDialogProps {
    index: usize,
    error: IntegrationError,
    run_id: Option<String>,
    history: Vec<String>,
    copied: bool,
    on_copied: EventHandler<usize>,
    on_close: EventHandler<()>,
}

#[component]
fn ErrorDetailsDialog(props: ErrorDetailsDialogProps) -> Element {
    let kind = SafeErrorKind::classify(&props.error.message);
    let reason = safe_failure_reason(&props.error);
    let diagnostics = safe_diagnostics(&props.error, props.run_id.as_deref(), &props.history);
    let title_id = format!("mm-error-dialog-title-{}", props.index);
    let description_id = format!("mm-error-dialog-description-{}", props.index);

    rsx! {
        div { class: "mm-modal-backdrop", role: "presentation",
            section {
                class: "mm-modal mm-error-dialog",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-labelledby": "{title_id}",
                "aria-describedby": "{description_id}",
                tabindex: "-1",
                onkeydown: move |event| {
                    if event.key() == Key::Escape {
                        props.on_close.call(());
                    }
                },
                div { class: "mm-modal-header",
                    h2 { id: "{title_id}", "Error details" }
                    button { class: "mm-modal-close", "aria-label": "Close error details", autofocus: true, onclick: move |_| props.on_close.call(()), "×" }
                }
                div { class: "mm-modal-body",
                    p { id: "{description_id}", "These details exclude credentials, file paths, raw provider responses, and other sensitive error content." }
                    h3 { "What to check" }
                    p { "{reason}" }
                    dl { class: "mm-dialog-details",
                        dt { "Provider" } dd { "{provider_label(props.error.slice)}" }
                        dt { "Category" } dd { "{kind.label()}" }
                        dt { "Request" } dd { "{request_label(props.error.request_id)}" }
                        if let Some(run_id) = props.run_id.as_deref() {
                            dt { "Run" } dd { "{run_id}" }
                        }
                        dt { "Retryable" } dd { if props.error.retryable { "Yes" } else { "No" } }
                    }
                    if !props.history.is_empty() {
                        div { class: "mm-error-history",
                            h3 { "Progress history" }
                            ol {
                                for item in props.history.iter() { li { "{item}" } }
                            }
                        }
                    }
                    div { class: "mm-dialog-actions",
                        button {
                            class: "mm-button secondary",
                            onclick: move |_| {
                                let script = clipboard_script(&diagnostics);
                                let copied_index = props.index;
                                let on_copied = props.on_copied;
                                spawn(async move {
                                    if document::eval(&script).await.is_ok() {
                                        on_copied.call(copied_index);
                                    }
                                });
                            },
                            "Copy safe diagnostics"
                        }
                        button { class: "mm-button primary", onclick: move |_| props.on_close.call(()), "Close" }
                    }
                    if props.copied {
                        p { class: "mm-copy-status", role: "status", "aria-live": "polite", "Safe diagnostics copied." }
                    }
                }
            }
        }
    }
}

fn provider_label(slice: Option<IntegrationSlice>) -> &'static str {
    match slice {
        Some(IntegrationSlice::LocalLibrary) => "Local library",
        Some(IntegrationSlice::SoundcloudCatalog) => "SoundCloud",
        Some(IntegrationSlice::GoogleDrive) => "Google Drive",
        Some(IntegrationSlice::Backup) => "Backup storage",
        Some(IntegrationSlice::ReconciliationActions) => "Reconciliation",
        None => "Desktop integration",
    }
}

fn request_label(request_id: Option<u64>) -> String {
    request_id.map_or_else(
        || "Startup request".to_owned(),
        |id| format!("Request #{id}"),
    )
}

fn settings_href(slice: Option<IntegrationSlice>) -> &'static str {
    match slice {
        Some(IntegrationSlice::LocalLibrary | IntegrationSlice::SoundcloudCatalog) => {
            "#music-sources"
        }
        Some(IntegrationSlice::GoogleDrive | IntegrationSlice::Backup) => "#music-storage-settings",
        _ => "#music-management-settings",
    }
}

fn retry_event(
    error: &IntegrationError,
    view: &MusicManagementView,
    failed_event: Option<&MusicManagementEvent>,
) -> Option<MusicManagementEvent> {
    if !error.retryable {
        return None;
    }
    if let Some(event) = failed_event {
        return Some(event.clone());
    }
    match error.slice? {
        IntegrationSlice::LocalLibrary | IntegrationSlice::SoundcloudCatalog => {
            let provider = if error.slice == Some(IntegrationSlice::LocalLibrary) {
                ProviderChoice::LocalFolder
            } else {
                ProviderChoice::SoundCloud
            };
            let source = view
                .sources
                .iter()
                .find(|source| source.provider == provider)?;
            source.failed_sync_run_id.clone().map_or_else(
                || Some(MusicManagementEvent::SyncSource(source.id.clone())),
                |sync_run_id| Some(MusicManagementEvent::RetrySyncRun { sync_run_id }),
            )
        }
        IntegrationSlice::GoogleDrive => view
            .primary_storage
            .iter()
            .chain(view.backup_storages.iter())
            .chain(view.unassigned_storages.iter())
            .find(|storage| storage.provider == ProviderChoice::GoogleDrive)
            .map(|storage| MusicManagementEvent::RetryStorage(storage.id.clone())),
        IntegrationSlice::Backup => view
            .backup_storages
            .first()
            .map(|storage| MusicManagementEvent::RetryStorage(storage.id.clone())),
        IntegrationSlice::ReconciliationActions => {
            view.failed_reconciliations.first().map(|failure| {
                MusicManagementEvent::RetryReconciliationResult {
                    result_id: failure.result_id.clone(),
                }
            })
        }
    }
}

fn reconnect_event(error: &IntegrationError) -> Option<MusicManagementEvent> {
    match error.slice? {
        IntegrationSlice::LocalLibrary => {
            Some(MusicManagementEvent::AddSource(ProviderChoice::LocalFolder))
        }
        IntegrationSlice::SoundcloudCatalog => {
            Some(MusicManagementEvent::AddSource(ProviderChoice::SoundCloud))
        }
        _ => None,
    }
}

fn related_run_id(error: &IntegrationError, view: &MusicManagementView) -> Option<String> {
    let provider = match error.slice? {
        IntegrationSlice::LocalLibrary => ProviderChoice::LocalFolder,
        IntegrationSlice::SoundcloudCatalog => ProviderChoice::SoundCloud,
        _ => return None,
    };
    view.sources
        .iter()
        .find(|source| source.provider == provider)
        .and_then(|source| source.failed_sync_run_id.clone())
}

fn progress_history(error: &IntegrationError, events: &[IntegrationEvent]) -> Vec<String> {
    let Some(request_id) = error.request_id else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|event| match event {
            IntegrationEvent::RequestQueued { id, .. } if *id == request_id => {
                Some("Request queued".to_owned())
            }
            IntegrationEvent::RequestStarted { id, .. } if *id == request_id => {
                Some("Provider work started".to_owned())
            }
            IntegrationEvent::RequestCompleted { id, .. } if *id == request_id => {
                Some("Request completed".to_owned())
            }
            IntegrationEvent::RequestFailed(failure) if failure.request_id == Some(request_id) => {
                Some("Request failed".to_owned())
            }
            _ => None,
        })
        .collect()
}

fn safe_failure_reason(error: &IntegrationError) -> &'static str {
    let message = error.message.to_ascii_lowercase();
    if message.contains("soundcloud is not configured")
        || message.contains("soundcloud oauth client id is not configured")
    {
        "SoundCloud desktop OAuth is not configured. Set REC_BAG_SOUNDCLOUD_CLIENT_ID and restart rec_bag."
    } else if message.contains("authorization broker could not be reached") {
        "The SoundCloud broker is unavailable. Start or deploy the rec_bag fullstack web service and verify it is reachable before retrying."
    } else if error.slice.is_none()
        && error.request_id.is_none()
        && SafeErrorKind::classify(&message) == SafeErrorKind::Connectivity
    {
        "rec_bag could not complete desktop startup because its catalog or required runtime service was unavailable. Check the profile data directory, network access for any remote catalog, and then restart rec_bag."
    } else {
        match SafeErrorKind::classify(&message) {
            SafeErrorKind::Authentication => {
                "The saved provider authorization is missing or expired. Reconnect the provider to authorize rec_bag again."
            }
            SafeErrorKind::RateLimited => {
                "The provider has temporarily limited requests. Wait a few minutes before retrying."
            }
            SafeErrorKind::Connectivity => {
                "A required service could not be reached. Check your network and that the configured provider or rec_bag service is running, then retry."
            }
            SafeErrorKind::Permission => {
                "The provider or selected location denied access. Review the configured permissions before retrying."
            }
            SafeErrorKind::Configuration => {
                "A required provider setting is missing or invalid. Review the provider settings and restart if you change environment configuration."
            }
            SafeErrorKind::Unexpected => {
                "The operation returned an unexpected result. Copy these safe diagnostics if the problem persists."
            }
        }
    }
}

fn safe_diagnostics(error: &IntegrationError, run_id: Option<&str>, history: &[String]) -> String {
    let kind = SafeErrorKind::classify(&error.message);
    let mut lines = vec![
        "rec_bag music-management diagnostic".to_owned(),
        format!("provider: {}", provider_label(error.slice)),
        format!("category: {}", kind.diagnostic_code()),
        format!("summary: {}", safe_failure_reason(error)),
        format!(
            "request: {}",
            error
                .request_id
                .map_or_else(|| "startup".to_owned(), |id| id.to_string())
        ),
        format!("retryable: {}", error.retryable),
    ];
    if let Some(run_id) = run_id {
        lines.push(format!("run: {run_id}"));
    }
    if !history.is_empty() {
        lines.push(format!("history: {}", history.join(" -> ")));
    }
    lines.join("\n")
}

fn clipboard_script(diagnostics: &str) -> String {
    let json = serde_json::to_string(diagnostics).expect("serializing a string cannot fail");
    format!("await navigator.clipboard.writeText({json}); return true;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::mock_music_management_view;

    fn error(slice: IntegrationSlice, message: &str, retryable: bool) -> IntegrationError {
        IntegrationError {
            request_id: Some(42),
            slice: Some(slice),
            message: message.to_owned(),
            retryable,
        }
    }

    #[test]
    fn safe_diagnostics_never_include_provider_message_or_secret() {
        let secret = "refresh_token=do-not-copy";
        let error = error(
            IntegrationSlice::SoundcloudCatalog,
            &format!("401 body {secret}"),
            true,
        );
        let diagnostics =
            safe_diagnostics(&error, Some("run-safe-id"), &["Request queued".to_owned()]);
        assert!(!diagnostics.contains(secret));
        assert!(!diagnostics.contains("401 body"));
        assert!(diagnostics.contains("category: authentication"));
        assert!(diagnostics
            .contains("summary: The saved provider authorization is missing or expired."));
        assert!(diagnostics.contains("request: 42"));
        assert!(diagnostics.contains("run: run-safe-id"));
    }

    #[test]
    fn startup_connectivity_has_a_safe_actionable_summary() {
        let error = IntegrationError {
            request_id: None,
            slice: None,
            message: "catalog connection failed: opaque-value=never-expose".to_owned(),
            retryable: false,
        };

        let reason = safe_failure_reason(&error);
        let diagnostics = safe_diagnostics(&error, None, &[]);
        assert!(reason.contains("desktop startup"));
        assert!(diagnostics.contains("summary: rec_bag could not complete desktop startup"));
        assert!(!diagnostics.contains("opaque-value=never-expose"));
    }

    #[test]
    fn action_mapping_uses_stable_context_and_respects_retryability() {
        let view = mock_music_management_view();
        let retry = retry_event(
            &error(IntegrationSlice::SoundcloudCatalog, "timeout", true),
            &view,
            None,
        );
        assert!(
            matches!(retry, Some(MusicManagementEvent::SyncSource(id)) if id == "soundcloud-main")
        );
        assert_eq!(
            reconnect_event(&error(IntegrationSlice::SoundcloudCatalog, "oauth", true)),
            Some(MusicManagementEvent::AddSource(ProviderChoice::SoundCloud))
        );
        assert_eq!(
            retry_event(
                &error(IntegrationSlice::SoundcloudCatalog, "timeout", false),
                &view,
                None,
            ),
            None
        );
    }

    #[test]
    fn retry_replays_the_exact_failed_lifecycle_event() {
        let view = mock_music_management_view();
        let failed_event = MusicManagementEvent::ClearLocalCache {
            source_id: "soundcloud-main".to_owned(),
        };

        assert_eq!(
            retry_event(
                &error(IntegrationSlice::SoundcloudCatalog, "timeout", true),
                &view,
                Some(&failed_event),
            ),
            Some(failed_event)
        );
    }

    #[test]
    fn accessibility_state_tracks_dialog_and_index_changes() {
        let mut state = ErrorAccessibilityState::default();
        state.open(2);
        state.copied = Some(2);
        state.after_dismiss(0);
        assert_eq!(state.expanded, Some(1));
        assert_eq!(state.copied, Some(1));
        state.after_dismiss(1);
        assert_eq!(state, ErrorAccessibilityState::default());
    }

    #[test]
    fn progress_history_contains_only_fixed_lifecycle_labels() {
        let failure = error(IntegrationSlice::Backup, "raw secret body", true);
        let events = vec![
            IntegrationEvent::RequestQueued {
                id: 42,
                slice: IntegrationSlice::Backup,
            },
            IntegrationEvent::RequestStarted {
                id: 42,
                slice: IntegrationSlice::Backup,
            },
            IntegrationEvent::RequestFailed(failure.clone()),
        ];
        assert_eq!(
            progress_history(&failure, &events),
            ["Request queued", "Provider work started", "Request failed"]
        );
    }

    #[test]
    fn clipboard_script_json_escapes_diagnostics() {
        let script = clipboard_script("line\n\"quoted\"");
        assert!(script.contains("line\\n\\\"quoted\\\""));
        assert!(!script.contains("writeText(line"));
    }
}
