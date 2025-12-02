//! Library management view for browsing and organizing music files.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::Track;
use uuid::Uuid;

/// Library view for managing music files.
#[component]
pub fn Library() -> Element {
    let app_state = use_app_state();
    let mut folder_path = use_signal(|| String::new());
    let mut scan_error = use_signal(|| Option::<String>::None);
    let mut selected_playlist_for_add: Signal<Option<Uuid>> = use_signal(|| None);

    let mut do_scan = move || {
        let path = folder_path();
        if path.is_empty() {
            scan_error.set(Some("Please enter a folder path".to_string()));
            return;
        }

        // Check if path exists
        let path_obj = std::path::Path::new(&path);
        if !path_obj.exists() {
            scan_error.set(Some(format!("Path does not exist: {}", path)));
            return;
        }
        if !path_obj.is_dir() {
            scan_error.set(Some(format!("Path is not a directory: {}", path)));
            return;
        }

        scan_error.set(None);
        app_state.scan_folder(&path);
    };

    let add_track_to_playlist = move |track: Track| {
        if let Some(playlist_id) = selected_playlist_for_add() {
            app_state.add_track_to_playlist(track, playlist_id);
        }
    };

    let add_all_tracks = move |_| {
        if let Some(playlist_id) = selected_playlist_for_add() {
            let tracks = (app_state.library_tracks)().clone();
            for track in tracks {
                app_state.add_track_to_playlist(track, playlist_id);
            }
        }
    };

    let playlists = (app_state.playlists)();
    let library_tracks = (app_state.library_tracks)();

    rsx! {
        div { class: "library-view",
            h1 { "Music Library" }

            div { class: "folder-scanner",
                input {
                    r#type: "text",
                    placeholder: "Enter folder path to scan (e.g., /home/user/Music)...",
                    value: "{folder_path}",
                    oninput: move |e| folder_path.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            do_scan();
                        }
                    }
                }
                button {
                    class: "btn-scan",
                    onclick: move |_| do_scan(),
                    "Scan Folder"
                }
            }

            if let Some(error) = scan_error() {
                div { class: "error-message", "{error}" }
            }

            div { class: "library-stats",
                "Found {library_tracks.len()} tracks"
            }

            // Playlist selector for adding tracks
            div { class: "add-to-playlist-bar",
                label { "Add to playlist: " }
                select {
                    value: selected_playlist_for_add().map(|id| id.to_string()).unwrap_or_default(),
                    onchange: move |e| {
                        let value = e.value();
                        if value.is_empty() {
                            selected_playlist_for_add.set(None);
                        } else if let Ok(id) = Uuid::parse_str(&value) {
                            selected_playlist_for_add.set(Some(id));
                        }
                    },
                    option { value: "", "Select a playlist..." }
                    for playlist in playlists.iter() {
                        option {
                            key: "{playlist.id}",
                            value: "{playlist.id}",
                            "{playlist.name}"
                        }
                    }
                }
                button {
                    class: "btn-small",
                    disabled: selected_playlist_for_add().is_none() || library_tracks.is_empty(),
                    onclick: add_all_tracks,
                    "Add All"
                }
            }

            if playlists.is_empty() {
                div { class: "warning-message",
                    "No playlists exist. Go to the DJ view and create a playlist first."
                }
            }

            div { class: "library-tracks",
                table {
                    thead {
                        tr {
                            th { "Title" }
                            th { "Artist" }
                            th { "Album" }
                            th { "Duration" }
                            th { "Genre" }
                            th { "Bitrate" }
                            th { "Actions" }
                        }
                    }
                    tbody {
                        for track in library_tracks.iter() {
                            {
                                let track_clone = track.clone();
                                let duration_str = track.duration_string();
                                let bitrate_str = track.bitrate_string();
                                rsx! {
                                    tr { key: "{track.id}",
                                        td { "{track.title}" }
                                        td { class: "col-artist", "{track.artist.clone().unwrap_or_default()}" }
                                        td { class: "col-album", "{track.album.clone().unwrap_or_default()}" }
                                        td { class: "col-duration", "{duration_str}" }
                                        td { class: "col-genre", "{track.genre.clone().unwrap_or_default()}" }
                                        td { class: "col-bitrate", "{bitrate_str}" }
                                        td {
                                            button {
                                                class: "btn-small",
                                                disabled: selected_playlist_for_add().is_none(),
                                                onclick: move |_| add_track_to_playlist(track_clone.clone()),
                                                "Add"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if library_tracks.is_empty() {
                div { class: "empty-state",
                    p { "Enter a folder path above to scan for music files." }
                    p { "Supported formats: MP3, WAV, FLAC, OGG, M4A, AAC" }
                }
            }
        }
    }
}
