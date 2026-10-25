//! Desktop adapter for the provider-independent music-management dashboard.

use dioxus::prelude::*;
use ui::{
    HealthTone, MusicManagementDashboard, MusicManagementEvent, MusicManagementView, ProviderChoice,
};

use crate::state::integration::{
    use_integration_state, IntegrationError, IntegrationEvent, IntegrationSlice,
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
                "not configured",
                "configuration",
                "registered desktop callback",
                "redirect uri",
            ],
        ) {
            Self::Configuration
        } else if contains_any(
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

fn backup_date(value: Option<chrono::DateTime<chrono::Utc>>) -> String {
    value.map_or_else(|| "Unknown date".into(), |date| date.to_rfc3339())
}

fn contains_any(value: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| value.contains(marker))
}

/// Whether the error comes from a slice that works with local folders (local
/// sources, or local storages through the music storage slice), so local
/// folder wording in it is meaningful.
fn touches_local_folders(error: &IntegrationError) -> bool {
    matches!(
        error.slice,
        Some(IntegrationSlice::LocalLibrary | IntegrationSlice::MusicStorage)
    )
}

fn classify_error(error: &IntegrationError) -> SafeErrorKind {
    let message = error.message.to_ascii_lowercase();
    if touches_local_folders(error) {
        if contains_any(&message, &["permission", "access denied", "read-only"]) {
            return SafeErrorKind::Permission;
        }
        if contains_any(
            &message,
            &[
                "folder chooser",
                "folder picker",
                "local root is unavailable",
            ],
        ) {
            return SafeErrorKind::Configuration;
        }
    }
    SafeErrorKind::classify(&message)
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
    let startup_failed = errors.iter().any(|error| error.request_id.is_none());
    let retry_events: Vec<_> = errors
        .iter()
        .map(|error| snapshot.failed_event(error.request_id).cloned())
        .collect();
    let events = snapshot.events.clone();
    let ownership_blocked_by = snapshot.ownership_blocked_by.clone();
    let database_backups = snapshot.database_backups.clone();
    let staged_restore = snapshot.staged_restore.clone();
    drop(snapshot);

    let mut presentation = use_signal(ErrorAccessibilityState::default);
    let mut confirm_takeover = use_signal(|| false);
    let mut integration_state = integration.state;
    let mut dashboard_integration = integration.clone();
    let takeover_integration = integration.clone();
    let backup_refresh_integration = integration.clone();
    let backup_now_integration = integration.clone();
    let backup_restore_integration = integration.clone();
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
            section { class: "mm-section", "aria-label": "Database backups",
                div { class: "mm-section-header",
                    div {
                        h2 { "Database backups" }
                        p { "A backup is a copy of the catalog database, kept on every storage configured as a backup. Nothing runs on a schedule." }
                    }
                    div { class: "mm-section-actions",
                        button {
                            class: "mm-button secondary",
                            onclick: move |_| {
                                let mut backups = backup_refresh_integration.clone();
                                spawn(async move { backups.refresh_database_backups().await; });
                            },
                            "Refresh"
                        }
                        button {
                            class: "mm-button primary",
                            onclick: move |_| {
                                let mut backups = backup_now_integration.clone();
                                spawn(async move { backups.back_up_database().await; });
                            },
                            "Back up now"
                        }
                    }
                }
                if let Some(staged) = staged_restore.clone() {
                    p { class: "mm-notice", role: "status",
                        "A restored database is staged at {staged.display()}. Restart the application to put it in place."
                    }
                }
                if database_backups.is_empty() {
                    p { "No database backups are listed. Configure a backup storage, then back up." }
                } else {
                    ul { class: "mm-list",
                        for backup in database_backups.iter() {
                            li { key: "{backup.storage_id}:{backup.object_id}", class: "mm-list-row",
                                strong { "{backup_date(backup.created_at)}" }
                                span { " on {backup.storage_name} \u{2014} {backup.size_bytes} bytes" }
                                button {
                                    class: "mm-button secondary",
                                    onclick: {
                                        let backup = backup.clone();
                                        let integration = backup_restore_integration.clone();
                                        move |_| {
                                            let mut backups = integration.clone();
                                            let backup = backup.clone();
                                            spawn(async move { backups.restore_database_backup(backup).await; });
                                        }
                                    },
                                    "Restore"
                                }
                            }
                        }
                    }
                }
            }
        }
        if pending_count + in_flight_count > 0 {
            aside { class: "mm-activity-banner", role: "status", "aria-live": "polite",
                span { class: "mm-activity-indicator", "" }
                if !runtime_ready && startup_failed {
                    span { "Desktop integration startup failed; {pending_count} queued request(s) cannot run. Review the startup error below." }
                } else if !runtime_ready {
                    span { "Desktop integration is starting; {pending_count} queued request(s) will begin when the catalog is ready." }
                } else {
                    span { "Integration work: {pending_count} queued, {in_flight_count} running" }
                }
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
    let kind = classify_error(&props.error);
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
    let kind = classify_error(&props.error);
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
        Some(IntegrationSlice::MusicStorage) => "Music storage",
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
        Some(IntegrationSlice::MusicStorage | IntegrationSlice::Backup) => {
            "#music-storage-settings"
        }
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
            // SoundCloud syncs run synchronously; a queued RetrySyncRun would never execute.
            match source.failed_sync_run_id.clone() {
                Some(sync_run_id) if provider != ProviderChoice::SoundCloud => {
                    Some(MusicManagementEvent::RetrySyncRun { sync_run_id })
                }
                _ => Some(MusicManagementEvent::SyncSource(source.id.clone())),
            }
        }
        // Without the failed event, retry the storage that looks unwell.
        IntegrationSlice::MusicStorage => {
            let mut storages = view
                .primary_storage
                .iter()
                .chain(view.backup_storages.iter())
                .chain(view.unassigned_storages.iter());
            let first = storages.clone().next();
            storages
                .find(|storage| storage.health != HealthTone::Healthy)
                .or(first)
                .map(|storage| MusicManagementEvent::RetryStorage(storage.id.clone()))
        }
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
        "SoundCloud desktop OAuth is not configured. Open Music Management, choose Configure SoundCloud, then enter the public client ID and registered redirect URI."
    } else if message.contains("soundcloud redirect uri")
        || message.contains("registered desktop callback")
    {
        "The SoundCloud redirect URI is invalid. In Music Management → Configure SoundCloud, use the exact loopback redirect registered with SoundCloud and allowed by the broker."
    } else if message.contains("authorization broker could not be reached") {
        "The SoundCloud broker is unavailable. Start or deploy the rec_bag fullstack web service and verify it is reachable before retrying."
    } else if touches_local_folders(error) && message.contains("local storage is read-only") {
        "The selected local primary folder is read-only. Choose a folder rec_bag can write to, or update that folder's permissions, then create the primary again."
    } else if touches_local_folders(error)
        && contains_any(&message, &["folder chooser", "folder picker"])
    {
        "rec_bag cannot open a local-folder chooser in this session. Run the native desktop app and install zenity or kdialog on Linux, or set REC_BAG_LOCAL_SOURCE to an absolute writable folder before starting rec_bag."
    } else if touches_local_folders(error) && message.contains("local root is unavailable") {
        "rec_bag could not open the selected local folder. Confirm the folder exists, its drive is mounted, and your account can access every parent directory, then try again."
    } else if touches_local_folders(error) && classify_error(error) == SafeErrorKind::Permission {
        "rec_bag could not access the selected local folder. Choose a folder your account can read and write, ensure its drive is mounted read-write, then create the primary again."
    } else if error.slice.is_none()
        && error.request_id.is_none()
        && SafeErrorKind::classify(&message) == SafeErrorKind::Connectivity
    {
        "rec_bag could not complete desktop startup because its catalog or required runtime service was unavailable. Check the profile data directory, network access for any remote catalog, and then restart rec_bag."
    } else {
        match classify_error(error) {
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
    let kind = classify_error(error);
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
    fn soundcloud_redirect_validation_is_a_configuration_error() {
        let error = error(
            IntegrationSlice::SoundcloudCatalog,
            "SoundCloud redirect URI is not a valid registered desktop callback",
            false,
        );

        assert_eq!(
            SafeErrorKind::classify(&error.message),
            SafeErrorKind::Configuration
        );
        assert!(safe_failure_reason(&error).contains("redirect URI is invalid"));
    }

    #[test]
    fn read_only_local_primary_has_a_safe_actionable_summary() {
        let error = error(
            IntegrationSlice::LocalLibrary,
            "local root is unavailable: local storage is read-only",
            false,
        );

        let diagnostics = safe_diagnostics(&error, None, &[]);
        assert_eq!(classify_error(&error), SafeErrorKind::Permission);
        assert!(diagnostics.contains("selected local primary folder is read-only"));
        assert!(!diagnostics.contains("local root is unavailable"));
    }

    #[test]
    fn missing_local_folder_chooser_is_a_configuration_error() {
        let error = error(
            IntegrationSlice::LocalLibrary,
            "local root is unavailable: No native folder chooser is installed",
            false,
        );

        let diagnostics = safe_diagnostics(&error, None, &[]);
        assert_eq!(classify_error(&error), SafeErrorKind::Configuration);
        assert!(diagnostics.contains("install zenity or kdialog"));
        assert!(diagnostics.contains("category: configuration"));
    }

    /// Local storages are connected through the music storage slice, which
    /// reports the local driver's own wording.
    #[test]
    fn music_storage_local_folder_failures_get_the_local_summaries() {
        let read_only = error(
            IntegrationSlice::MusicStorage,
            "local storage is read-only",
            false,
        );
        assert_eq!(classify_error(&read_only), SafeErrorKind::Permission);
        assert!(
            safe_failure_reason(&read_only).contains("selected local primary folder is read-only")
        );

        let chooser = error(
            IntegrationSlice::MusicStorage,
            "local root is unavailable: No native folder chooser is installed",
            false,
        );
        assert_eq!(classify_error(&chooser), SafeErrorKind::Configuration);
        assert!(safe_failure_reason(&chooser).contains("install zenity or kdialog"));
        assert_eq!(
            settings_href(Some(IntegrationSlice::MusicStorage)),
            "#music-storage-settings"
        );
        assert_eq!(
            provider_label(Some(IntegrationSlice::MusicStorage)),
            "Music storage"
        );
    }

    #[test]
    fn music_storage_retry_targets_an_unwell_storage_of_any_provider() {
        let mut view = mock_music_management_view();
        let retry = |view: &MusicManagementView| {
            retry_event(
                &error(IntegrationSlice::MusicStorage, "offline", true),
                view,
                None,
            )
        };
        for storage in view
            .primary_storage
            .iter_mut()
            .chain(view.backup_storages.iter_mut())
            .chain(view.unassigned_storages.iter_mut())
        {
            storage.health = HealthTone::Healthy;
        }
        assert_eq!(
            retry(&view),
            Some(MusicManagementEvent::RetryStorage("primary-local".into()))
        );
        view.unassigned_storages[0].health = HealthTone::Offline;
        assert_eq!(
            retry(&view),
            Some(MusicManagementEvent::RetryStorage(
                "primary-candidate".into()
            ))
        );
    }

    #[test]
    fn local_primary_permission_denied_has_a_safe_actionable_summary() {
        let error = error(
            IntegrationSlice::LocalLibrary,
            "filesystem operation failed: Permission denied",
            true,
        );

        let diagnostics = safe_diagnostics(&error, None, &[]);
        assert!(diagnostics.contains("selected local folder"));
        assert!(diagnostics.contains("mounted read-write"));
        assert!(!diagnostics.contains("filesystem operation failed"));
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
