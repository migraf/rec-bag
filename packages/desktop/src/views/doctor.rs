//! Doctor view for analyzing and fixing track issues.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::{Diagnosis, Severity};
use std::collections::HashMap;

/// Doctor view for library health analysis.
#[component]
pub fn DoctorView() -> Element {
    let app_state = use_app_state();
    let diagnostics = app_state.diagnostics();

    // Local state for UI
    let mut expanded_groups: Signal<std::collections::HashSet<String>> =
        use_signal(std::collections::HashSet::new);

    // Get diagnostic data from state
    let diagnoses = diagnostics.diagnoses();
    let scanning = diagnostics.scanning();
    let scanned_count = diagnostics.scanned_count();
    let library_count = app_state.projection().track_count();

    // Group diagnoses by check name
    let grouped = use_memo(move || {
        let diagnoses = diagnostics.diagnoses();
        let mut groups: HashMap<String, Vec<Diagnosis>> = HashMap::new();
        for d in diagnoses {
            groups.entry(d.check_name.clone()).or_default().push(d);
        }
        groups
    });

    // Get check descriptions (including built-in duplicate check)
    let doctor = diagnostics.doctor();
    let mut check_descriptions: HashMap<String, String> = doctor
        .checks()
        .iter()
        .map(|c| (c.name().to_string(), c.description().to_string()))
        .collect();
    // Add description for the duplicates check (library-wide, not per-track)
    check_descriptions.insert(
        "duplicates".to_string(),
        "Finds tracks with matching artist and title that may be duplicate files".to_string(),
    );

    let mut check_can_fix: HashMap<String, bool> = doctor
        .checks()
        .iter()
        .map(|c| (c.name().to_string(), c.can_fix()))
        .collect();
    // Duplicates cannot be auto-fixed (user must decide which to keep)
    check_can_fix.insert("duplicates".to_string(), false);

    // Calculate duplicate stats
    let duplicate_count = diagnoses
        .iter()
        .filter(|d| d.check_name == "duplicates")
        .count();
    let duplicate_groups = {
        let mut groups: HashMap<String, Vec<&Diagnosis>> = HashMap::new();
        for d in diagnoses.iter().filter(|d| d.check_name == "duplicates") {
            // Extract group key from message
            if let Some(start) = d.message.find('"') {
                if let Some(end) = d.message.rfind('"') {
                    let key = &d.message[start + 1..end];
                    groups.entry(key.to_string()).or_default().push(d);
                }
            }
        }
        groups.len()
    };

    // Handlers
    let on_scan = move |_| {
        diagnostics.scan();
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
                    let duplicates_class = if duplicate_count > 0 { "summary-value has-duplicates" } else { "summary-value" };
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
                                div { class: "{duplicates_class}", "{duplicate_groups}" }
                                div { class: "summary-label", "Duplicate Groups" }
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
                        let is_duplicates = check_name == "duplicates";

                        // For duplicates, count unique groups instead of individual tracks
                        let display_count = if is_duplicates {
                            let groups = group_duplicates_by_key(&items);
                            format!("{} groups", groups.len())
                        } else {
                            format!("{} tracks", items.len())
                        };

                        rsx! {
                            div { class: "issue-group {severity_class}",
                                // Group header
                                div {
                                    class: "issue-group-header",
                                    onclick: move |_| toggle_group(check_name_clone.clone()),
                                    div { class: "issue-group-info",
                                        span { class: "severity-icon",
                                            {get_check_icon(&check_name, severity)}
                                        }
                                        div { class: "issue-group-text",
                                            span { class: "issue-group-name", "{format_check_name(&check_name)}" }
                                            span { class: "issue-group-count", "{display_count}" }
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
                                                            diagnostics.fix_all_by_check(&check_name_for_fix);
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
                                    if is_duplicates {
                                        // Render duplicates grouped by artist/title
                                        {render_duplicate_groups(&items)}
                                    } else {
                                        // Render regular issues
                                        div { class: "issue-list",
                                            for diagnosis in items.iter() {
                                                {
                                                    let track = app_state.projection().track(diagnosis.track_id);

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
                                                                            let _ = diagnostics.fix(track_id, &check_name_fix);
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
                        li { "Duplicate tracks (same artist + title)" }
                    }
                }
            }
        }
    }
}

/// Format check name for display (e.g., "wav_extensible" -> "WAV Extensible")
fn format_check_name(name: &str) -> String {
    // Special case for common names
    match name {
        "duplicates" => "Duplicate Tracks".to_string(),
        "wav_extensible" => "WAV Extensible (CDJ)".to_string(),
        "metadata_completeness" => "Missing Metadata".to_string(),
        _ => name
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Get the appropriate icon for a check name
fn get_check_icon(name: &str, severity: Severity) -> &'static str {
    match name {
        "duplicates" => "📋",
        _ => match severity {
            Severity::Error => "⛔",
            Severity::Warning => "⚠️",
            Severity::Info => "ℹ️",
        },
    }
}

/// Represents a group of duplicate files with the same artist/title
struct DuplicateDisplayGroup {
    /// The normalized "artist - title" key
    key: String,
    /// File paths in this group
    paths: Vec<std::path::PathBuf>,
}

/// Group duplicate diagnoses by their artist/title key
fn group_duplicates_by_key(diagnoses: &[Diagnosis]) -> Vec<DuplicateDisplayGroup> {
    let mut groups: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();

    for d in diagnoses {
        // The key is stored in track_path's parent context, but we can derive it
        // by normalizing artist + title from the path, or use the path directly
        // For now, group by the duplicate key extracted from message or use path
        let key = extract_duplicate_key(d);
        groups.entry(key).or_default().push(d.track_path.clone());
    }

    groups
        .into_iter()
        .map(|(key, paths)| DuplicateDisplayGroup { key, paths })
        .collect()
}

/// Extract the duplicate key from a diagnosis
fn extract_duplicate_key(d: &Diagnosis) -> String {
    // Try to extract key from message like: Duplicate of "artist - title" (N total copies)
    if let Some(start) = d.message.find('"') {
        if let Some(end) = d.message.rfind('"') {
            if end > start {
                return d.message[start + 1..end].to_string();
            }
        }
    }

    // Fallback: use the file stem as key
    d.track_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Render duplicate groups in a compact format
fn render_duplicate_groups(diagnoses: &[Diagnosis]) -> Element {
    let groups = group_duplicates_by_key(diagnoses);

    rsx! {
        div { class: "duplicate-groups",
            for group in groups.iter() {
                div { class: "duplicate-group",
                    div { class: "duplicate-group-header",
                        span { class: "duplicate-icon", "🔁" }
                        span { class: "duplicate-key", "{group.key}" }
                        span { class: "duplicate-count", "{group.paths.len()} copies" }
                    }
                    div { class: "duplicate-paths",
                        for path in group.paths.iter() {
                            div { class: "duplicate-path",
                                span { class: "path-icon", "📄" }
                                span { class: "path-text", "{path.display()}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
