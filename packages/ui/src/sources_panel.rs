//! Sources panel component for managing music source folders.

use dioxus::prelude::*;
use rec_core::MusicSource;
use uuid::Uuid;

/// Props for the SourcesPanel component.
#[derive(Props, Clone, PartialEq)]
pub struct SourcesPanelProps {
    /// List of music sources
    pub sources: Vec<MusicSource>,
    /// Whether the panel is collapsed
    #[props(default = false)]
    pub collapsed: bool,
    /// Callback when adding a new source
    pub on_add: EventHandler<String>,
    /// Callback when removing a source
    pub on_remove: EventHandler<Uuid>,
    /// Callback when rescanning a source
    pub on_rescan: EventHandler<Uuid>,
    /// Callback when rescanning all sources
    pub on_rescan_all: EventHandler<()>,
    /// Callback when creating a playlist from source
    pub on_create_playlist: EventHandler<Uuid>,
    /// Callback when collapse state changes
    pub on_toggle_collapse: EventHandler<bool>,
}

/// Format a timestamp for display.
fn format_last_scanned(source: &MusicSource) -> String {
    match source.last_scanned {
        Some(dt) => {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(dt);

            if duration.num_minutes() < 1 {
                "Just now".to_string()
            } else if duration.num_hours() < 1 {
                format!("{}m ago", duration.num_minutes())
            } else if duration.num_days() < 1 {
                format!("{}h ago", duration.num_hours())
            } else {
                format!("{}d ago", duration.num_days())
            }
        }
        None => "Never".to_string(),
    }
}

/// Truncate a path for display.
fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        return path.to_string();
    }

    // Try to show the last part of the path
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return path[..max_len].to_string() + "...";
    }

    let last = parts.last().unwrap();
    if last.len() >= max_len - 4 {
        return format!("...{}", &last[last.len().saturating_sub(max_len - 3)..]);
    }

    format!(".../{}", last)
}

/// Sources panel component.
#[component]
pub fn SourcesPanel(props: SourcesPanelProps) -> Element {
    let mut show_add_form = use_signal(|| false);
    let mut new_source_path = use_signal(|| String::new());
    let mut expanded_source: Signal<Option<Uuid>> = use_signal(|| None);

    let toggle_collapse = move |_| {
        props.on_toggle_collapse.call(!props.collapsed);
    };

    rsx! {
        div { class: "sources-panel",
            // Header
            div {
                class: "sources-header",
                onclick: toggle_collapse,
                div { class: "sources-header-left",
                    span {
                        class: if props.collapsed { "collapse-icon collapsed" } else { "collapse-icon" },
                        "▼"
                    }
                    h3 { "Sources" }
                    span { class: "sources-count", "({props.sources.len()})" }
                }
                div { class: "sources-header-actions",
                    button {
                        class: "btn-icon btn-small",
                        title: "Add source folder",
                        onclick: move |e| {
                            e.stop_propagation();
                            show_add_form.set(true);
                        },
                        "+"
                    }
                    button {
                        class: "btn-icon btn-small",
                        title: "Rescan all sources",
                        onclick: move |e| {
                            e.stop_propagation();
                            props.on_rescan_all.call(());
                        },
                        "↻"
                    }
                }
            }

            if !props.collapsed {
                // Add source form
                if show_add_form() {
                    div { class: "add-source-form",
                        input {
                            r#type: "text",
                            placeholder: "/path/to/music/folder",
                            value: "{new_source_path}",
                            oninput: move |e| new_source_path.set(e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Enter && !new_source_path().is_empty() {
                                    props.on_add.call(new_source_path());
                                    new_source_path.set(String::new());
                                    show_add_form.set(false);
                                } else if e.key() == Key::Escape {
                                    show_add_form.set(false);
                                    new_source_path.set(String::new());
                                }
                            }
                        }
                        div { class: "add-source-buttons",
                            button {
                                class: "btn-small",
                                disabled: new_source_path().is_empty(),
                                onclick: move |_| {
                                    if !new_source_path().is_empty() {
                                        props.on_add.call(new_source_path());
                                        new_source_path.set(String::new());
                                        show_add_form.set(false);
                                    }
                                },
                                "Add"
                            }
                            button {
                                class: "btn-small btn-cancel",
                                onclick: move |_| {
                                    show_add_form.set(false);
                                    new_source_path.set(String::new());
                                },
                                "Cancel"
                            }
                        }
                    }
                }

                // Sources list
                div { class: "sources-list",
                    for source in props.sources.iter() {
                        {
                            let source_id = source.id;
                            let is_expanded = expanded_source() == Some(source_id);
                            let path_display = truncate_path(&source.uri, 24);
                            let last_scanned = format_last_scanned(source);
                            let track_count = source.track_count;
                            let source_name = source.name.clone();

                            rsx! {
                                div {
                                    key: "{source_id}",
                                    class: if is_expanded { "source-item expanded" } else { "source-item" },
                                    // Main row
                                    div {
                                        class: "source-row",
                                        onclick: move |_| {
                                            if is_expanded {
                                                expanded_source.set(None);
                                            } else {
                                                expanded_source.set(Some(source_id));
                                            }
                                        },
                                        div { class: "source-icon", "📁" }
                                        div { class: "source-info",
                                            div { class: "source-name", "{source_name}" }
                                            div { class: "source-meta",
                                                span { class: "source-track-count", "{track_count} tracks" }
                                                span { class: "source-separator", "•" }
                                                span { class: "source-last-scan", "{last_scanned}" }
                                            }
                                        }
                                    }

                                    // Expanded details
                                    if is_expanded {
                                        div { class: "source-details",
                                            div { class: "source-path",
                                                span { class: "source-path-label", "Path:" }
                                                span { class: "source-path-value", title: "{source.uri}", "{path_display}" }
                                            }
                                            div { class: "source-actions",
                                                button {
                                                    class: "btn-small",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        props.on_rescan.call(source_id);
                                                    },
                                                    "↻ Rescan"
                                                }
                                                button {
                                                    class: "btn-small",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        props.on_create_playlist.call(source_id);
                                                    },
                                                    "📋 Create Playlist"
                                                }
                                                button {
                                                    class: "btn-small btn-danger",
                                                    onclick: move |e| {
                                                        e.stop_propagation();
                                                        props.on_remove.call(source_id);
                                                    },
                                                    "✕ Remove"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if props.sources.is_empty() {
                    div { class: "empty-state sources-empty",
                        "No music sources. Add a folder to get started."
                    }
                }
            }
        }
    }
}

