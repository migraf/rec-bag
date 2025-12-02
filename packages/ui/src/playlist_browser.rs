//! Playlist browser component for managing playlists.

use dioxus::prelude::*;
use rec_core::Playlist;
use uuid::Uuid;

/// Props for the PlaylistBrowser component.
#[derive(Props, Clone, PartialEq)]
pub struct PlaylistBrowserProps {
    /// List of playlists to display
    pub playlists: Vec<Playlist>,
    /// Currently selected playlist ID
    pub selected_id: Option<Uuid>,
    /// Callback when a playlist is selected
    pub on_select: EventHandler<Uuid>,
    /// Callback when creating a new playlist
    pub on_create: EventHandler<String>,
    /// Callback when deleting a playlist
    pub on_delete: EventHandler<Uuid>,
}

/// Playlist browser sidebar component.
#[component]
pub fn PlaylistBrowser(props: PlaylistBrowserProps) -> Element {
    let mut new_playlist_name = use_signal(|| String::new());
    let mut show_create_form = use_signal(|| false);

    rsx! {
        div { class: "playlist-browser",
            div { class: "playlist-header",
                h3 { "Playlists" }
                button {
                    class: "btn-icon",
                    onclick: move |_| show_create_form.set(true),
                    "+"
                }
            }

            if show_create_form() {
                div { class: "create-playlist-form",
                    input {
                        r#type: "text",
                        placeholder: "Playlist name",
                        value: "{new_playlist_name}",
                        oninput: move |e| new_playlist_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter && !new_playlist_name().is_empty() {
                                props.on_create.call(new_playlist_name());
                                new_playlist_name.set(String::new());
                                show_create_form.set(false);
                            } else if e.key() == Key::Escape {
                                show_create_form.set(false);
                            }
                        }
                    }
                    button {
                        class: "btn-small",
                        disabled: new_playlist_name().is_empty(),
                        onclick: move |_| {
                            if !new_playlist_name().is_empty() {
                                props.on_create.call(new_playlist_name());
                                new_playlist_name.set(String::new());
                                show_create_form.set(false);
                            }
                        },
                        "Create"
                    }
                    button {
                        class: "btn-small btn-cancel",
                        onclick: move |_| show_create_form.set(false),
                        "Cancel"
                    }
                }
            }

            ul { class: "playlist-list",
                for playlist in props.playlists.iter() {
                    {
                        let id = playlist.id;
                        let is_selected = props.selected_id == Some(id);
                        rsx! {
                            li {
                                key: "{id}",
                                class: if is_selected { "playlist-item selected" } else { "playlist-item" },
                                onclick: move |_| props.on_select.call(id),
                                div { class: "playlist-info",
                                    span { class: "playlist-name", "{playlist.name}" }
                                    span { class: "playlist-count", "{playlist.tracks.len()} tracks" }
                                }
                                button {
                                    class: "btn-delete",
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        props.on_delete.call(id);
                                    },
                                    "×"
                                }
                            }
                        }
                    }
                }
            }

            if props.playlists.is_empty() {
                div { class: "empty-state",
                    "No playlists yet. Create one to get started!"
                }
            }
        }
    }
}

