//! Doctor view for analyzing and fixing track issues.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::{Diagnosis, Severity};
use std::collections::HashMap;

/// Doctor view for library health analysis.
#[component]
pub fn DoctorView() -> Element {
    let app_state = use_app_state();

    // Local state for UI
    let mut expanded_groups: Signal<std::collections::HashSet<String>> =
        use_signal(|| std::collections::HashSet::new());

    // Get diagnostic data from state
    let diagnoses = app_state.diagnoses.read().clone();
    let scanning = *app_state.doctor_scanning.read();
    let scanned_count = *app_state.doctor_scanned_count.read();
    let library_count = app_state.library_tracks.read().len();

    // Group diagnoses by check name
    let grouped = use_memo(move || {
        let diagnoses = app_state.diagnoses.read().clone();
        let mut groups: HashMap<String, Vec<Diagnosis>> = HashMap::new();
        for d in diagnoses {
            groups.entry(d.check_name.clone()).or_default().push(d);
        }
        groups
    });

    // Get check descriptions
    let doctor = app_state.doctor.read().clone();
    let check_descriptions: HashMap<String, String> = doctor
        .checks()
        .iter()
        .map(|c| (c.name().to_string(), c.description().to_string()))
        .collect();
    let check_can_fix: HashMap<String, bool> = doctor
        .checks()
        .iter()
        .map(|c| (c.name().to_string(), c.can_fix()))
        .collect();

    // Handlers
    let on_scan = move |_| {
        app_state.run_doctor_scan();
    };

    let mut toggle_group = move |name: String| {
        let mut groups = expanded_groups.write();
        if groups.contains(&name) {
            groups.remove(&name);
        } else {
            groups.insert(name);
        }
    };

    // Count totals
    let total_issues = diagnoses.len();
    let fixable_count = diagnoses.iter().filter(|d| d.fixable).count();

    rsx! {
        div { class: "doctor-view",
            // Header
            div { class: "doctor-header",
                div { class: "doctor-title",
                    span { class: "doctor-icon", "🩺" }
                    h1 { "Library Doctor" }
                }
                div { class: "doctor-actions",
                    button {
                        class: "btn-scan",
                        disabled: scanning,
                        onclick: on_scan,
                        if scanning {
                            "Scanning..."
                        } else {
                            "Scan Library"
                        }
                    }
                    if scanned_count > 0 {
                        span { class: "scan-status",
                            "Scanned: {scanned_count} tracks"
                        }
                    }
                }
            }

            // Summary
            if scanned_count > 0 {
                {
                    let issues_class = if total_issues > 0 { "summary-value has-issues" } else { "summary-value" };
                    rsx! {
                        div { class: "doctor-summary",
                            div { class: "summary-card",
                                div { class: "summary-value", "{library_count}" }
                                div { class: "summary-label", "Total Tracks" }
                            }
                            div { class: "summary-card",
                                div { class: "{issues_class}", "{total_issues}" }
                                div { class: "summary-label", "Issues Found" }
                            }
                            div { class: "summary-card",
                                div { class: "summary-value fixable", "{fixable_count}" }
                                div { class: "summary-label", "Auto-Fixable" }
                            }
                        }
                    }
                }
            }

            // Issue groups
            div { class: "doctor-groups",
                if total_issues == 0 && scanned_count > 0 {
                    div { class: "doctor-success",
                        span { class: "success-icon", "✓" }
                        p { "No issues found! Your library is healthy." }
                    }
                }

                for (check_name, items) in grouped().into_iter() {
                    {
                        let is_expanded = expanded_groups.read().contains(&check_name);
                        let description = check_descriptions.get(&check_name).cloned().unwrap_or_default();
                        let can_fix = check_can_fix.get(&check_name).copied().unwrap_or(false);
                        let fixable_in_group = items.iter().filter(|d| d.fixable).count();
                        let severity = items.first().map(|d| d.severity).unwrap_or(Severity::Info);
                        let severity_class = severity.as_str();
                        let check_name_clone = check_name.clone();
                        let check_name_for_fix = check_name.clone();

                        rsx! {
                            div { class: "issue-group {severity_class}",
                                // Group header
                                div {
                                    class: "issue-group-header",
                                    onclick: move |_| toggle_group(check_name_clone.clone()),
                                    div { class: "issue-group-info",
                                        span { class: "severity-icon",
                                            match severity {
                                                Severity::Error => "⛔",
                                                Severity::Warning => "⚠️",
                                                Severity::Info => "ℹ️",
                                            }
                                        }
                                        div { class: "issue-group-text",
                                            span { class: "issue-group-name", "{format_check_name(&check_name)}" }
                                            span { class: "issue-group-count", "{items.len()} tracks" }
                                        }
                                    }
                                    {
                                        let expand_class = if is_expanded { "expand-icon expanded" } else { "expand-icon" };
                                        rsx! {
                                            div { class: "issue-group-actions",
                                                if can_fix && fixable_in_group > 0 {
                                                    button {
                                                        class: "btn-fix-all",
                                                        onclick: move |e| {
                                                            e.stop_propagation();
                                                            app_state.fix_all_by_check(&check_name_for_fix);
                                                        },
                                                        "Fix All ({fixable_in_group})"
                                                    }
                                                }
                                                span { class: "{expand_class}",
                                                    "▶"
                                                }
                                            }
                                        }
                                    }
                                }

                                // Group description
                                p { class: "issue-group-description", "{description}" }

                                // Expanded content
                                if is_expanded {
                                    div { class: "issue-list",
                                        for diagnosis in items.iter() {
                                            {
                                                let track = app_state.library_tracks.read()
                                                    .iter()
                                                    .find(|t| t.id == diagnosis.track_id)
                                                    .cloned();

                                                let track_title = track.as_ref()
                                                    .map(|t| t.title.clone())
                                                    .unwrap_or_else(|| "Unknown".to_string());
                                                let track_artist = track.as_ref()
                                                    .and_then(|t| t.artist.clone())
                                                    .unwrap_or_else(|| "Unknown Artist".to_string());
                                                let diagnosis_message = diagnosis.message.clone();
                                                let is_fixable = diagnosis.fixable;
                                                let track_id = diagnosis.track_id;
                                                let check_name_fix = diagnosis.check_name.clone();

                                                rsx! {
                                                    div { class: "issue-item",
                                                        div { class: "issue-track-info",
                                                            span { class: "issue-track-title", "{track_title}" }
                                                            span { class: "issue-track-artist", "{track_artist}" }
                                                        }
                                                        span { class: "issue-message", "{diagnosis_message}" }
                                                        div { class: "issue-actions",
                                                            if is_fixable {
                                                                button {
                                                                    class: "btn-fix",
                                                                    onclick: move |_| {
                                                                        let _ = app_state.fix_diagnosis(track_id, &check_name_fix);
                                                                    },
                                                                    "Fix"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Empty state
            if scanned_count == 0 {
                div { class: "doctor-empty",
                    span { class: "empty-icon", "🔍" }
                    h2 { "Scan Your Library" }
                    p { "Click \"Scan Library\" to check your tracks for potential issues like:" }
                    ul {
                        li { "WAV files with CDJ compatibility problems" }
                        li { "Missing metadata (artist, title, BPM, genre)" }
                    }
                }
            }
        }
    }
}

/// Format check name for display (e.g., "wav_extensible" -> "WAV Extensible")
fn format_check_name(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().chain(chars).collect(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

