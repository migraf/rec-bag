//! Sources panel component for managing music source folders.

use crate::FolderBrowser;
use dioxus::prelude::*;
use rec_core::{build_folder_tree, MusicSource};
use std::path::PathBuf;
use uuid::Uuid;

const SOURCES_PANEL_CSS: Asset = asset!("/assets/components/sources-panel.css");

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
    /// Callback when a folder is selected for browsing
    #[props(default)]
    pub on_folder_select: Option<EventHandler<PathBuf>>,
    /// Callback when "All" is selected (clears folder filter)
    #[props(default)]
    pub on_show_all: Option<EventHandler<()>>,
    /// Currently selected folder path
    #[props(default)]
    pub selected_folder: Option<PathBuf>,
    /// Total track count in library
    #[props(default = 0)]
    pub total_track_count: usize,
}

/// Sources panel component with folder browser.
#[component]
pub fn SourcesPanel(props: SourcesPanelProps) -> Element {
    let mut show_add_form = use_signal(|| false);
    let mut new_source_path = use_signal(|| String::new());
    let mut active_source: Signal<Option<Uuid>> = use_signal(|| None);

    let toggle_collapse = move |_| {
        props.on_toggle_collapse.call(!props.collapsed);
    };

    // "All" is selected when no active source is chosen
    let is_all_selected = active_source().is_none();

    rsx! {
        document::Stylesheet { href: SOURCES_PANEL_CSS }
        div { class: "sources-panel",
            // Compact header
            div {
                class: "sp-header",
                onclick: toggle_collapse,

                div { class: "sp-header-left",
                    span {
                        class: if props.collapsed { "sp-chevron collapsed" } else { "sp-chevron" },
                        "›"
                    }
                    span { class: "sp-title", "Browser" }
                }

                div { class: "sp-header-actions",
                    button {
                        class: "sp-btn",
                        title: "Add source",
                        onclick: move |e| {
                            e.stop_propagation();
                            show_add_form.set(!show_add_form());
                        },
                        "+"
                    }
                    button {
                        class: "sp-btn",
                        title: "Rescan all",
                        onclick: move |e| {
                            e.stop_propagation();
                            props.on_rescan_all.call(());
                        },
                        "↻"
                    }
                }
            }

            if !props.collapsed {
                // Add source form (inline)
                if show_add_form() {
                    div { class: "sp-add-form",
                        input {
                            r#type: "text",
                            placeholder: "/path/to/music",
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
                        button {
                            class: "sp-btn sp-btn-add",
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
                    }
                }

                // Source tabs with "All" option
                div { class: "sp-tabs",
                    // "All" tab - shows full library
                    button {
                        class: if is_all_selected { "sp-tab active" } else { "sp-tab" },
                        onclick: move |_| {
                            active_source.set(None);
                            if let Some(handler) = &props.on_show_all {
                                handler.call(());
                            }
                        },
                        "All"
                        span { class: "sp-tab-count", "{props.total_track_count}" }
                    }

                    // Individual source tabs
                    for source in props.sources.iter() {
                        {
                            let source_id = source.id;
                            let is_active = active_source() == Some(source_id);
                            let source_name = source.name.clone();
                            let track_count = source.track_count;

                            rsx! {
                                button {
                                    key: "{source_id}",
                                    class: if is_active { "sp-tab active" } else { "sp-tab" },
                                    onclick: move |_| active_source.set(Some(source_id)),
                                    title: "{source.uri}",
                                    "{source_name}"
                                    span { class: "sp-tab-count", "{track_count}" }
                                }
                            }
                        }
                    }
                }

                // Active source details & folder browser
                if let Some(active_id) = active_source() {
                    if let Some(source) = props.sources.iter().find(|s| s.id == active_id) {
                        div { class: "sp-source-content",
                            // Source path display
                            div { class: "sp-source-path",
                                span { class: "sp-path-text", title: "{source.uri}", "{source.uri}" }
                                div { class: "sp-path-actions",
                                    button {
                                        class: "sp-btn-sm",
                                        title: "Rescan",
                                        onclick: move |_| props.on_rescan.call(active_id),
                                        "↻"
                                    }
                                    button {
                                        class: "sp-btn-sm",
                                        title: "Create playlist",
                                        onclick: move |_| props.on_create_playlist.call(active_id),
                                        "📋"
                                    }
                                    button {
                                        class: "sp-btn-sm sp-btn-danger",
                                        title: "Remove",
                                        onclick: move |_| props.on_remove.call(active_id),
                                        "✕"
                                    }
                                }
                            }

                            // Only local sources support folder browsing. Provider-backed
                            // sources render their own source-kind-specific content instead of
                            // being coerced into a filesystem tree.
                            if source.is_local() {
                                LocalSourceBrowser {
                                    source: source.clone(),
                                    selected_path: props.selected_folder.clone(),
                                    on_select: move |path| {
                                        if let Some(handler) = &props.on_folder_select {
                                            handler.call(path);
                                        }
                                    },
                                }
                            } else {
                                ProviderSourceSummary { source: source.clone() }
                            }
                        }
                    }
                } else {
                    // "All" view - no specific source selected
                    if !props.sources.is_empty() {
                        div { class: "sp-all-view",
                            span { class: "sp-all-label", "Showing all tracks from {props.sources.len()} source(s)" }
                        }
                    }
                }

                if props.sources.is_empty() {
                    div { class: "sp-empty",
                        "Add a music folder to get started"
                    }
                }
            }
        }
    }
}

/// Filesystem browsing is mounted only for an active local source. Keeping tree
/// construction here prevents provider-backed sources from being interpreted as paths.
#[derive(Props, Clone, PartialEq)]
struct LocalSourceBrowserProps {
    source: MusicSource,
    selected_path: Option<PathBuf>,
    on_select: EventHandler<PathBuf>,
}

#[component]
fn LocalSourceBrowser(props: LocalSourceBrowserProps) -> Element {
    let folder_tree = props
        .source
        .local_path()
        .and_then(|path| build_folder_tree(std::path::Path::new(path)));

    rsx! {
        FolderBrowser {
            root: folder_tree,
            selected_path: props.selected_path,
            on_select: move |path| props.on_select.call(path),
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ProviderSourceSummaryProps {
    source: MusicSource,
}

#[component]
fn ProviderSourceSummary(props: ProviderSourceSummaryProps) -> Element {
    rsx! {
        div { class: "sp-provider-source-content",
            strong { "{props.source.name}" }
            span { "Provider collections are managed in Music management." }
        }
    }
}
