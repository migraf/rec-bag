//! Playlist browser component for managing playlists.

use dioxus::prelude::*;
use rec_core::{Playlist, SourceType};
use uuid::Uuid;

const PLAYLIST_BROWSER_CSS: Asset = asset!("/assets/components/playlist-browser.css");
const SOURCE_BADGE_CSS: Asset = asset!("/assets/components/source-badge.css");

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

/// Names the catalog source a playlist or track comes from, linking to its page there.
#[component]
pub fn SourceBadge(source_type: SourceType, url: Option<String>) -> Element {
    let (label, class) = match source_type {
        SourceType::SoundCloud => ("SoundCloud", "source-badge source-soundcloud"),
        SourceType::GoogleDrive => ("Drive", "source-badge"),
        SourceType::Dropbox => ("Dropbox", "source-badge"),
        SourceType::Local => ("Local", "source-badge"),
        _ => ("Remote", "source-badge"),
    };
    rsx! {
        document::Stylesheet { href: SOURCE_BADGE_CSS }
        if let Some(url) = url {
            a {
                class: "{class}",
                href: "{url}",
                target: "_blank",
                rel: "noopener noreferrer",
                title: "Open on {label}",
                onclick: move |e| e.stop_propagation(),
                "{label}"
            }
        } else {
            span { class: "{class}", "{label}" }
        }
    }
}

/// Playlist browser sidebar component.
#[component]
pub fn PlaylistBrowser(props: PlaylistBrowserProps) -> Element {
    let mut new_playlist_name = use_signal(|| String::new());
    let mut show_create_form = use_signal(|| false);

    rsx! {
        document::Stylesheet { href: PLAYLIST_BROWSER_CSS }
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
                        let origin = playlist.origin.clone();
                        rsx! {
                            li {
                                key: "{id}",
                                class: if is_selected { "playlist-item selected" } else { "playlist-item" },
                                onclick: move |_| props.on_select.call(id),
                                div { class: "playlist-info",
                                    span { class: "playlist-name", "{playlist.name}" }
                                    span { class: "playlist-count", "({playlist.tracks.len()})" }
                                }
                                if let Some(origin) = origin {
                                    // Mirrored from a source: credited and linked back to it,
                                    // and read-only because the next sync owns its contents.
                                    SourceBadge {
                                        source_type: origin.source_type,
                                        url: origin.external_url,
                                    }
                                } else {
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
            }

            if props.playlists.is_empty() {
                div { class: "empty-state",
                    "No playlists yet. Create one to get started!"
                }
            }
        }
    }
}
