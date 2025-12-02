//! Track list component for displaying tracks in a playlist.

use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

/// Reference to a playlist (for displaying badges).
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistRef {
    pub id: Uuid,
    pub name: String,
}

/// Props for the TrackList component.
#[derive(Props, Clone, PartialEq)]
pub struct TrackListProps {
    /// Tracks to display
    pub tracks: Vec<Track>,
    /// Currently selected track IDs (multi-selection)
    pub selected_ids: HashSet<Uuid>,
    /// Callback when track selection changes
    pub on_select: EventHandler<SelectionEvent>,
    /// Callback when a track is double-clicked (load to deck)
    pub on_load: EventHandler<(Uuid, usize)>,
    /// Callback when tracks are removed
    pub on_remove: EventHandler<Vec<Uuid>>,
    /// Callback when context menu is requested
    pub on_context_menu: EventHandler<ContextMenuEvent>,
    /// Number of available decks
    pub deck_count: usize,
    /// Function to get playlist references for a track
    #[props(default)]
    pub get_playlists: Option<fn(Uuid) -> Vec<PlaylistRef>>,
    /// Whether to show playlist badges column
    #[props(default = false)]
    pub show_playlist_badges: bool,
}

/// Selection event with modifier keys.
#[derive(Debug, Clone)]
pub struct SelectionEvent {
    pub track_id: Uuid,
    pub track_index: usize,
    pub shift_key: bool,
    pub ctrl_key: bool,
}

/// Context menu event with position.
#[derive(Debug, Clone)]
pub struct ContextMenuEvent {
    pub track_id: Uuid,
    pub x: i32,
    pub y: i32,
    pub selected_ids: HashSet<Uuid>,
}

/// Format duration as MM:SS.
fn format_duration(d: Option<Duration>) -> String {
    match d {
        Some(d) => {
            let total_secs = d.as_secs();
            let mins = total_secs / 60;
            let secs = total_secs % 60;
            format!("{:02}:{:02}", mins, secs)
        }
        None => "--:--".to_string(),
    }
}

/// Track list table component with multi-selection support.
#[component]
pub fn TrackList(props: TrackListProps) -> Element {
    // Track the last clicked index for shift-selection
    let mut last_clicked_index: Signal<Option<usize>> = use_signal(|| None);

    rsx! {
        div { class: "track-list",
            table {
                thead {
                    tr {
                        th { class: "col-checkbox", "" }
                        th { class: "col-number", "#" }
                        th { class: "col-title", "Title" }
                        th { class: "col-artist", "Artist" }
                        if props.show_playlist_badges {
                            th { class: "col-playlists", "Playlists" }
                        }
                        th { class: "col-album", "Album" }
                        th { class: "col-bpm", "BPM" }
                        th { class: "col-key", "Key" }
                        th { class: "col-duration", "Time" }
                        th { class: "col-actions", "Load" }
                    }
                }
                tbody {
                    for (idx, track) in props.tracks.iter().enumerate() {
                        {
                            let id = track.id;
                            let is_selected = props.selected_ids.contains(&id);
                            let row_num = idx + 1;
                            let track_title = track.title.clone();
                            let track_artist = track.artist.clone().unwrap_or_default();
                            let track_album = track.album.clone().unwrap_or_default();
                            let track_bpm = track.bpm.map(|b| format!("{:.0}", b)).unwrap_or_default();
                            let track_key = track.key.clone().unwrap_or_default();
                            let track_duration = format_duration(track.duration);

                            // Get playlist badges if function is provided
                            let playlist_badges: Vec<PlaylistRef> = props.get_playlists
                                .map(|f| f(id))
                                .unwrap_or_default();

                            // Clone selected_ids for use in closures
                            let selected_ids_for_context = props.selected_ids.clone();
                            let selected_ids_for_remove = props.selected_ids.clone();

                            rsx! {
                                tr {
                                    key: "{id}",
                                    class: if is_selected { "track-row selected" } else { "track-row" },
                                    onclick: move |e| {
                                        last_clicked_index.set(Some(idx));
                                        props.on_select.call(SelectionEvent {
                                            track_id: id,
                                            track_index: idx,
                                            shift_key: e.modifiers().shift(),
                                            ctrl_key: e.modifiers().ctrl() || e.modifiers().meta(),
                                        });
                                    },
                                    oncontextmenu: move |e| {
                                        e.prevent_default();
                                        // Include this track in selection if not already selected
                                        let mut selected = selected_ids_for_context.clone();
                                        if !selected.contains(&id) {
                                            selected.clear();
                                            selected.insert(id);
                                        }
                                        props.on_context_menu.call(ContextMenuEvent {
                                            track_id: id,
                                            x: e.client_coordinates().x as i32,
                                            y: e.client_coordinates().y as i32,
                                            selected_ids: selected,
                                        });
                                    },
                                    // Checkbox column
                                    td { class: "col-checkbox",
                                        input {
                                            r#type: "checkbox",
                                            checked: is_selected,
                                            onclick: move |e| e.stop_propagation(),
                                            onchange: move |_| {
                                                props.on_select.call(SelectionEvent {
                                                    track_id: id,
                                                    track_index: idx,
                                                    shift_key: false,
                                                    ctrl_key: true, // Toggle behavior
                                                });
                                            }
                                        }
                                    }
                                    td { class: "col-number", "{row_num}" }
                                    td { class: "col-title", "{track_title}" }
                                    td { class: "col-artist", "{track_artist}" }
                                    if props.show_playlist_badges {
                                        td { class: "col-playlists",
                                            div { class: "playlist-badges",
                                                for (badge_idx, badge) in playlist_badges.iter().take(2).enumerate() {
                                                    span {
                                                        key: "{badge.id}",
                                                        class: "playlist-badge",
                                                        title: "{badge.name}",
                                                        "{truncate_name(&badge.name, 8)}"
                                                    }
                                                }
                                                if playlist_badges.len() > 2 {
                                                    span {
                                                        class: "playlist-badge overflow",
                                                        title: "{playlist_badges.iter().skip(2).map(|b| b.name.as_str()).collect::<Vec<_>>().join(\", \")}",
                                                        "+{playlist_badges.len() - 2}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    td { class: "col-album", "{track_album}" }
                                    td { class: "col-bpm", "{track_bpm}" }
                                    td { class: "col-key", "{track_key}" }
                                    td { class: "col-duration", "{track_duration}" }
                                    td { class: "col-actions",
                                        for deck in 1..=props.deck_count {
                                            button {
                                                class: "btn-load",
                                                onclick: move |e| {
                                                    e.stop_propagation();
                                                    props.on_load.call((id, deck));
                                                },
                                                "{deck}"
                                            }
                                        }
                                        button {
                                            class: "btn-remove",
                                            onclick: move |e| {
                                                e.stop_propagation();
                                                // Remove all selected or just this one
                                                if selected_ids_for_remove.contains(&id) && selected_ids_for_remove.len() > 1 {
                                                    props.on_remove.call(selected_ids_for_remove.iter().copied().collect());
                                                } else {
                                                    props.on_remove.call(vec![id]);
                                                }
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if props.tracks.is_empty() {
                div { class: "empty-state",
                    "No tracks in this playlist. Add some music!"
                }
            }
        }
    }
}

/// Truncate a name for display.
fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}…", &name[..max_len - 1])
    }
}

// ==================== Legacy Props (for backward compatibility) ====================

/// Legacy props for the TrackList component (single selection).
#[derive(Props, Clone, PartialEq)]
pub struct TrackListLegacyProps {
    /// Tracks to display
    pub tracks: Vec<Track>,
    /// Currently selected track ID
    pub selected_id: Option<Uuid>,
    /// Callback when a track is selected
    pub on_select: EventHandler<Uuid>,
    /// Callback when a track is double-clicked (load to deck)
    pub on_load: EventHandler<(Uuid, usize)>,
    /// Callback when a track is removed
    pub on_remove: EventHandler<Uuid>,
    /// Number of available decks
    pub deck_count: usize,
}

/// Legacy track list component (single selection, backward compatible).
#[component]
pub fn TrackListLegacy(props: TrackListLegacyProps) -> Element {
    rsx! {
        div { class: "track-list",
            table {
                thead {
                    tr {
                        th { class: "col-number", "#" }
                        th { class: "col-title", "Title" }
                        th { class: "col-artist", "Artist" }
                        th { class: "col-album", "Album" }
                        th { class: "col-bpm", "BPM" }
                        th { class: "col-key", "Key" }
                        th { class: "col-duration", "Time" }
                        th { class: "col-actions", "Load" }
                    }
                }
                tbody {
                    for (idx, track) in props.tracks.iter().enumerate() {
                        {
                            let id = track.id;
                            let is_selected = props.selected_id == Some(id);
                            let row_num = idx + 1;
                            rsx! {
                                tr {
                                    key: "{id}",
                                    class: if is_selected { "track-row selected" } else { "track-row" },
                                    onclick: move |_| props.on_select.call(id),
                                    td { class: "col-number", "{row_num}" }
                                    td { class: "col-title", "{track.title}" }
                                    td { class: "col-artist", "{track.artist.clone().unwrap_or_default()}" }
                                    td { class: "col-album", "{track.album.clone().unwrap_or_default()}" }
                                    td { class: "col-bpm",
                                        {track.bpm.map(|b| format!("{:.0}", b)).unwrap_or_default()}
                                    }
                                    td { class: "col-key",
                                        {track.key.clone().unwrap_or_default()}
                                    }
                                    td { class: "col-duration", "{format_duration(track.duration)}" }
                                    td { class: "col-actions",
                                        for deck in 1..=props.deck_count {
                                            button {
                                                class: "btn-load",
                                                onclick: move |e| {
                                                    e.stop_propagation();
                                                    props.on_load.call((id, deck));
                                                },
                                                "{deck}"
                                            }
                                        }
                                        button {
                                            class: "btn-remove",
                                            onclick: move |e| {
                                                e.stop_propagation();
                                                props.on_remove.call(id);
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if props.tracks.is_empty() {
                div { class: "empty-state",
                    "No tracks in this playlist. Add some music!"
                }
            }
        }
    }
}
