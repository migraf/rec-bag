//! Playlist browser component for managing playlists.

use dioxus::prelude::*;
use rec_core::{Playlist, SourceType};
use uuid::Uuid;

const PLAYLIST_BROWSER_CSS: Asset = asset!("/assets/components/playlist-browser.css");

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

/// The playlists sharing an origin: created here (`None`), or mirrored from one kind of source.
struct PlaylistGroup<'a> {
    source: Option<SourceType>,
    playlists: Vec<&'a Playlist>,
}

impl PlaylistGroup<'_> {
    fn label(&self) -> &'static str {
        match self.source {
            None => "My playlists",
            Some(SourceType::SoundCloud) => "SoundCloud",
            Some(SourceType::GoogleDrive) => "Google Drive",
            Some(SourceType::Dropbox) => "Dropbox",
            Some(SourceType::Local) => "Local",
            Some(_) => "Remote",
        }
    }

    fn class(&self) -> &'static str {
        match self.source {
            Some(SourceType::SoundCloud) => "playlist-group source-soundcloud",
            _ => "playlist-group",
        }
    }
}

/// Groups playlists by origin, own playlists first, keeping their order within a group.
fn group_by_origin(playlists: &[Playlist]) -> Vec<PlaylistGroup<'_>> {
    let mut groups = vec![PlaylistGroup { source: None, playlists: Vec::new() }];
    for playlist in playlists {
        let source = playlist.origin.as_ref().map(|o| o.source_type);
        match groups.iter_mut().find(|g| g.source == source) {
            Some(group) => group.playlists.push(playlist),
            None => groups.push(PlaylistGroup { source, playlists: vec![playlist] }),
        }
    }
    groups.retain(|g| !g.playlists.is_empty());
    groups
}

/// Playlist browser sidebar component.
#[component]
pub fn PlaylistBrowser(props: PlaylistBrowserProps) -> Element {
    let mut new_playlist_name = use_signal(|| String::new());
    let mut show_create_form = use_signal(|| false);
    let mut collapsed_groups = use_signal(Vec::<Option<SourceType>>::new);
    let groups = group_by_origin(&props.playlists);

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

            div { class: "playlist-list",
                for group in groups.iter() {
                    {
                        let source = group.source;
                        let collapsed = collapsed_groups.read().contains(&source);
                        rsx! {
                            section { key: "{group.label()}", class: "{group.class()}",
                                // A lone group of own playlists needs no heading.
                                if groups.len() > 1 || source.is_some() {
                                    button {
                                        class: "playlist-group-header",
                                        "aria-expanded": "{!collapsed}",
                                        onclick: move |_| {
                                            let mut collapsed = collapsed_groups.write();
                                            match collapsed.iter().position(|s| *s == source) {
                                                Some(i) => { collapsed.remove(i); }
                                                None => collapsed.push(source),
                                            }
                                        },
                                        span { class: "playlist-group-chevron", if collapsed { "▸" } else { "▾" } }
                                        span { class: "playlist-group-label", "{group.label()}" }
                                        span { class: "playlist-group-count", "{group.playlists.len()}" }
                                    }
                                }
                                if !collapsed {
                                    ul {
                                        for playlist in group.playlists.iter() {
                                            {
                                                let id = playlist.id;
                                                let is_selected = props.selected_id == Some(id);
                                                let label = group.label();
                                                rsx! {
                                                    li {
                                                        key: "{id}",
                                                        class: if is_selected { "playlist-item selected" } else { "playlist-item" },
                                                        title: if source.is_some() { "Synced from {label}; edit it there" },
                                                        onclick: move |_| props.on_select.call(id),
                                                        span { class: "playlist-name", "{playlist.name}" }
                                                        span { class: "playlist-count", "{playlist.tracks.len()}" }
                                                        if let Some(origin) = &playlist.origin {
                                                            // Mirrored playlists are read-only because the next
                                                            // sync owns their contents; link back to the source instead.
                                                            if let Some(url) = &origin.external_url {
                                                                a {
                                                                    class: "playlist-action",
                                                                    href: "{url}",
                                                                    target: "_blank",
                                                                    rel: "noopener noreferrer",
                                                                    title: "Open on {label}",
                                                                    "aria-label": "Open {playlist.name} on {label}",
                                                                    onclick: move |e| e.stop_propagation(),
                                                                    "↗"
                                                                }
                                                            }
                                                        } else {
                                                            button {
                                                                class: "playlist-action btn-delete",
                                                                title: "Delete playlist",
                                                                "aria-label": "Delete {playlist.name}",
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
