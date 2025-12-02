//! Track list component for displaying tracks in a playlist.

use dioxus::prelude::*;
use rec_core::Track;
use std::time::Duration;
use uuid::Uuid;

/// Props for the TrackList component.
#[derive(Props, Clone, PartialEq)]
pub struct TrackListProps {
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

/// Track list table component.
#[component]
pub fn TrackList(props: TrackListProps) -> Element {
    rsx! {
        div { class: "track-list",
            table {
                thead {
                    tr {
                        th { class: "col-title", "Title" }
                        th { class: "col-artist", "Artist" }
                        th { class: "col-bpm", "BPM" }
                        th { class: "col-key", "Key" }
                        th { class: "col-duration", "Duration" }
                        th { class: "col-actions", "Load" }
                    }
                }
                tbody {
                    for track in props.tracks.iter() {
                        {
                            let id = track.id;
                            let is_selected = props.selected_id == Some(id);
                            rsx! {
                                tr {
                                    key: "{id}",
                                    class: if is_selected { "track-row selected" } else { "track-row" },
                                    onclick: move |_| props.on_select.call(id),
                                    td { class: "col-title", "{track.title}" }
                                    td { class: "col-artist", "{track.artist.clone().unwrap_or_default()}" }
                                    td { class: "col-bpm",
                                        {track.bpm.map(|b| format!("{:.1}", b)).unwrap_or_default()}
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

