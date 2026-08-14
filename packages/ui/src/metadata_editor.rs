//! Metadata editor component for viewing and editing track metadata.
//!
//! Supports both single track editing and batch editing for multiple tracks.

use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;
use uuid::Uuid;

/// Metadata fields that can be edited.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetadataFields {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub bpm: Option<f32>,
    pub key: Option<String>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub comment: Option<String>,
    pub rating: Option<u8>,
}

impl MetadataFields {
    /// Create metadata fields from a track.
    pub fn from_track(track: &Track) -> Self {
        Self {
            title: Some(track.title.clone()),
            artist: track.artist.clone(),
            album: track.album.clone(),
            genre: track.genre.clone(),
            bpm: track.bpm,
            key: track.key.clone(),
            year: track.year,
            track_number: track.track_number,
            comment: track.comment.clone(),
            rating: track.rating,
        }
    }

    /// Create metadata fields for batch editing (shared values only).
    pub fn from_tracks(tracks: &[Track]) -> Self {
        if tracks.is_empty() {
            return Self::default();
        }

        if tracks.len() == 1 {
            return Self::from_track(&tracks[0]);
        }

        // For batch editing, find common values
        let first = &tracks[0];
        let mut fields = Self::default();

        // Check if all tracks have the same artist
        if tracks.iter().all(|t| t.artist == first.artist) {
            fields.artist = first.artist.clone();
        }

        // Check if all tracks have the same album
        if tracks.iter().all(|t| t.album == first.album) {
            fields.album = first.album.clone();
        }

        // Check if all tracks have the same genre
        if tracks.iter().all(|t| t.genre == first.genre) {
            fields.genre = first.genre.clone();
        }

        // Check if all tracks have the same year
        if tracks.iter().all(|t| t.year == first.year) {
            fields.year = first.year;
        }

        fields
    }
}

/// Event emitted when metadata is saved.
#[derive(Debug, Clone)]
pub struct MetadataSaveEvent {
    /// IDs of tracks being edited
    pub track_ids: Vec<Uuid>,
    /// The new metadata fields
    pub fields: MetadataFields,
    /// Whether to write changes to file (not just memory)
    pub write_to_file: bool,
}

/// Props for the MetadataEditor component.
#[derive(Props, Clone, PartialEq)]
pub struct MetadataEditorProps {
    /// Whether the editor is visible
    pub visible: bool,
    /// Tracks being edited
    pub tracks: Vec<Track>,
    /// Callback when editor is closed
    pub on_close: EventHandler<()>,
    /// Callback when metadata is saved
    pub on_save: EventHandler<MetadataSaveEvent>,
}

/// Format duration as MM:SS.
fn format_duration(d: Option<std::time::Duration>) -> String {
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

/// Metadata editor modal component.
#[component]
pub fn MetadataEditor(props: MetadataEditorProps) -> Element {
    if !props.visible || props.tracks.is_empty() {
        return rsx! {};
    }

    let is_batch = props.tracks.len() > 1;
    let initial_fields = MetadataFields::from_tracks(&props.tracks);

    // Editable state
    let mut title = use_signal(|| initial_fields.title.clone().unwrap_or_default());
    let mut artist = use_signal(|| initial_fields.artist.clone().unwrap_or_default());
    let mut album = use_signal(|| initial_fields.album.clone().unwrap_or_default());
    let mut genre = use_signal(|| initial_fields.genre.clone().unwrap_or_default());
    let mut bpm_str = use_signal(|| {
        initial_fields
            .bpm
            .map(|b| format!("{:.1}", b))
            .unwrap_or_default()
    });
    let mut key = use_signal(|| initial_fields.key.clone().unwrap_or_default());
    let mut year_str = use_signal(|| {
        initial_fields
            .year
            .map(|y| y.to_string())
            .unwrap_or_default()
    });
    let mut track_num_str = use_signal(|| {
        initial_fields
            .track_number
            .map(|t| t.to_string())
            .unwrap_or_default()
    });
    let mut comment = use_signal(|| initial_fields.comment.clone().unwrap_or_default());
    let mut rating = use_signal(|| initial_fields.rating.unwrap_or(0));
    let mut write_to_file = use_signal(|| true);

    // Track which fields have been modified in batch mode
    let mut modified_fields: Signal<HashSet<String>> = use_signal(|| HashSet::new());

    // Active tab for extended info
    let mut active_tab = use_signal(|| "edit".to_string());

    let track_count = props.tracks.len();
    let track_ids: Vec<Uuid> = props.tracks.iter().map(|t| t.id).collect();

    // First track for displaying file info
    let first_track = props.tracks.first().cloned();

    let handle_save = move |_| {
        let fields = MetadataFields {
            title: if is_batch && !modified_fields().contains("title") {
                None
            } else {
                let t = title();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            },
            artist: if is_batch && !modified_fields().contains("artist") {
                None
            } else {
                let a = artist();
                if a.is_empty() {
                    None
                } else {
                    Some(a)
                }
            },
            album: if is_batch && !modified_fields().contains("album") {
                None
            } else {
                let a = album();
                if a.is_empty() {
                    None
                } else {
                    Some(a)
                }
            },
            genre: if is_batch && !modified_fields().contains("genre") {
                None
            } else {
                let g = genre();
                if g.is_empty() {
                    None
                } else {
                    Some(g)
                }
            },
            bpm: if is_batch && !modified_fields().contains("bpm") {
                None
            } else {
                bpm_str().parse::<f32>().ok()
            },
            key: if is_batch && !modified_fields().contains("key") {
                None
            } else {
                let k = key();
                if k.is_empty() {
                    None
                } else {
                    Some(k)
                }
            },
            year: if is_batch && !modified_fields().contains("year") {
                None
            } else {
                year_str().parse::<u32>().ok()
            },
            track_number: if is_batch && !modified_fields().contains("track_number") {
                None
            } else {
                track_num_str().parse::<u32>().ok()
            },
            comment: if is_batch && !modified_fields().contains("comment") {
                None
            } else {
                let c = comment();
                if c.is_empty() {
                    None
                } else {
                    Some(c)
                }
            },
            rating: if is_batch && !modified_fields().contains("rating") {
                None
            } else {
                let r = rating();
                if r == 0 {
                    None
                } else {
                    Some(r)
                }
            },
        };

        props.on_save.call(MetadataSaveEvent {
            track_ids: track_ids.clone(),
            fields,
            write_to_file: write_to_file(),
        });
    };

    let mut mark_modified = move |field: &str| {
        if is_batch {
            modified_fields.write().insert(field.to_string());
        }
    };

    rsx! {
        // Backdrop
        div {
            class: "metadata-editor-backdrop",
            onclick: move |_| props.on_close.call(())
        }

        // Editor modal
        div {
            class: "metadata-editor",
            onclick: move |e| e.stop_propagation(),

            // Header
            div { class: "me-header",
                div { class: "me-header-info",
                    span { class: "me-icon", "📝" }
                    h2 { class: "me-title",
                        if is_batch {
                            "Edit {track_count} Tracks"
                        } else {
                            "Edit Metadata"
                        }
                    }
                }
                button {
                    class: "me-close-btn",
                    onclick: move |_| props.on_close.call(()),
                    "×"
                }
            }

            // Tab bar
            div { class: "me-tabs",
                button {
                    class: if active_tab() == "edit" { "me-tab active" } else { "me-tab" },
                    onclick: move |_| active_tab.set("edit".to_string()),
                    "Edit"
                }
                if !is_batch {
                    button {
                        class: if active_tab() == "info" { "me-tab active" } else { "me-tab" },
                        onclick: move |_| active_tab.set("info".to_string()),
                        "File Info"
                    }
                }
            }

            // Content
            div { class: "me-content",
                if active_tab() == "edit" {
                    // Edit form
                    div { class: "me-form",
                        // Batch mode notice
                        if is_batch {
                            div { class: "me-batch-notice",
                                span { class: "me-batch-icon", "ℹ" }
                                "Only modified fields will be applied to all {track_count} tracks"
                            }
                        }

                        // Title (only for single track)
                        if !is_batch {
                            div { class: "me-field",
                                label { class: "me-label", "Title" }
                                input {
                                    class: "me-input",
                                    r#type: "text",
                                    value: "{title}",
                                    oninput: move |e| {
                                        title.set(e.value());
                                        mark_modified("title");
                                    }
                                }
                            }
                        }

                        // Artist
                        div { class: "me-field",
                            label { class: "me-label",
                                "Artist"
                                if is_batch && modified_fields().contains("artist") {
                                    span { class: "me-modified-badge", "modified" }
                                }
                            }
                            input {
                                class: if is_batch && modified_fields().contains("artist") { "me-input modified" } else { "me-input" },
                                r#type: "text",
                                value: "{artist}",
                                placeholder: if is_batch { "Multiple values..." } else { "" },
                                oninput: move |e| {
                                    artist.set(e.value());
                                    mark_modified("artist");
                                }
                            }
                        }

                        // Album
                        div { class: "me-field",
                            label { class: "me-label",
                                "Album"
                                if is_batch && modified_fields().contains("album") {
                                    span { class: "me-modified-badge", "modified" }
                                }
                            }
                            input {
                                class: if is_batch && modified_fields().contains("album") { "me-input modified" } else { "me-input" },
                                r#type: "text",
                                value: "{album}",
                                placeholder: if is_batch { "Multiple values..." } else { "" },
                                oninput: move |e| {
                                    album.set(e.value());
                                    mark_modified("album");
                                }
                            }
                        }

                        // Row with Genre and Year
                        div { class: "me-field-row",
                            div { class: "me-field",
                                label { class: "me-label",
                                    "Genre"
                                    if is_batch && modified_fields().contains("genre") {
                                        span { class: "me-modified-badge", "modified" }
                                    }
                                }
                                input {
                                    class: if is_batch && modified_fields().contains("genre") { "me-input modified" } else { "me-input" },
                                    r#type: "text",
                                    value: "{genre}",
                                    placeholder: if is_batch { "Multiple..." } else { "" },
                                    oninput: move |e| {
                                        genre.set(e.value());
                                        mark_modified("genre");
                                    }
                                }
                            }
                            div { class: "me-field me-field-small",
                                label { class: "me-label",
                                    "Year"
                                    if is_batch && modified_fields().contains("year") {
                                        span { class: "me-modified-badge", "modified" }
                                    }
                                }
                                input {
                                    class: if is_batch && modified_fields().contains("year") { "me-input modified" } else { "me-input" },
                                    r#type: "text",
                                    value: "{year_str}",
                                    placeholder: if is_batch { "..." } else { "" },
                                    oninput: move |e| {
                                        year_str.set(e.value());
                                        mark_modified("year");
                                    }
                                }
                            }
                        }

                        // Row with BPM and Key
                        div { class: "me-field-row",
                            div { class: "me-field me-field-small",
                                label { class: "me-label",
                                    "BPM"
                                    if is_batch && modified_fields().contains("bpm") {
                                        span { class: "me-modified-badge", "modified" }
                                    }
                                }
                                input {
                                    class: if is_batch && modified_fields().contains("bpm") { "me-input modified" } else { "me-input" },
                                    r#type: "text",
                                    value: "{bpm_str}",
                                    placeholder: if is_batch { "..." } else { "" },
                                    oninput: move |e| {
                                        bpm_str.set(e.value());
                                        mark_modified("bpm");
                                    }
                                }
                            }
                            div { class: "me-field me-field-small",
                                label { class: "me-label",
                                    "Key"
                                    if is_batch && modified_fields().contains("key") {
                                        span { class: "me-modified-badge", "modified" }
                                    }
                                }
                                input {
                                    class: if is_batch && modified_fields().contains("key") { "me-input modified" } else { "me-input" },
                                    r#type: "text",
                                    value: "{key}",
                                    placeholder: if is_batch { "..." } else { "" },
                                    oninput: move |e| {
                                        key.set(e.value());
                                        mark_modified("key");
                                    }
                                }
                            }
                            if !is_batch {
                                div { class: "me-field me-field-small",
                                    label { class: "me-label", "Track #" }
                                    input {
                                        class: "me-input",
                                        r#type: "text",
                                        value: "{track_num_str}",
                                        oninput: move |e| {
                                            track_num_str.set(e.value());
                                            mark_modified("track_number");
                                        }
                                    }
                                }
                            }
                        }

                        // Rating
                        div { class: "me-field",
                            label { class: "me-label",
                                "Rating"
                                if is_batch && modified_fields().contains("rating") {
                                    span { class: "me-modified-badge", "modified" }
                                }
                            }
                            div { class: "me-rating",
                                for star in 1..=5 {
                                    button {
                                        class: if rating() >= star { "me-star active" } else { "me-star" },
                                        onclick: move |_| {
                                            rating.set(if rating() == star { 0 } else { star });
                                            mark_modified("rating");
                                        },
                                        "★"
                                    }
                                }
                                if rating() > 0 {
                                    button {
                                        class: "me-rating-clear",
                                        onclick: move |_| {
                                            rating.set(0);
                                            mark_modified("rating");
                                        },
                                        "Clear"
                                    }
                                }
                            }
                        }

                        // Comment
                        div { class: "me-field",
                            label { class: "me-label",
                                "Comment"
                                if is_batch && modified_fields().contains("comment") {
                                    span { class: "me-modified-badge", "modified" }
                                }
                            }
                            textarea {
                                class: if is_batch && modified_fields().contains("comment") { "me-textarea modified" } else { "me-textarea" },
                                rows: 3,
                                value: "{comment}",
                                placeholder: if is_batch { "Multiple values..." } else { "Add notes about this track..." },
                                oninput: move |e| {
                                    comment.set(e.value());
                                    mark_modified("comment");
                                }
                            }
                        }
                    }
                } else if let Some(ref track) = first_track {
                    // File Info tab (single track only)
                    div { class: "me-info",
                        div { class: "me-info-section",
                            h3 { "File Information" }
                            div { class: "me-info-grid",
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Filename" }
                                    span { class: "me-info-value mono",
                                        {track.path.file_name().and_then(|n| n.to_str()).unwrap_or("Unknown")}
                                    }
                                }
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Path" }
                                    span { class: "me-info-value mono path",
                                        {track.path.parent().and_then(|p| p.to_str()).unwrap_or("")}
                                    }
                                }
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Format" }
                                    span { class: "me-info-value",
                                        {track.path.extension().and_then(|e| e.to_str()).unwrap_or("Unknown").to_uppercase()}
                                    }
                                }
                            }
                        }

                        div { class: "me-info-section",
                            h3 { "Audio Properties" }
                            div { class: "me-info-grid",
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Duration" }
                                    span { class: "me-info-value", {format_duration(track.duration)} }
                                }
                                if let Some(sr) = track.sample_rate {
                                    div { class: "me-info-row",
                                        span { class: "me-info-label", "Sample Rate" }
                                        span { class: "me-info-value", "{sr} Hz" }
                                    }
                                }
                                if let Some(br) = track.bitrate {
                                    div { class: "me-info-row",
                                        span { class: "me-info-label", "Bitrate" }
                                        span { class: "me-info-value", "{br} kbps" }
                                    }
                                }
                            }
                        }

                        div { class: "me-info-section",
                            h3 { "Library Info" }
                            div { class: "me-info-grid",
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Added" }
                                    span { class: "me-info-value",
                                        {track.added_at.format("%Y-%m-%d %H:%M").to_string()}
                                    }
                                }
                                if let Some(last) = track.last_played {
                                    div { class: "me-info-row",
                                        span { class: "me-info-label", "Last Played" }
                                        span { class: "me-info-value",
                                            {last.format("%Y-%m-%d %H:%M").to_string()}
                                        }
                                    }
                                }
                                div { class: "me-info-row",
                                    span { class: "me-info-label", "Play Count" }
                                    span { class: "me-info-value", "{track.play_count}" }
                                }
                            }
                        }
                    }
                }
            }

            // Footer
            div { class: "me-footer",
                div { class: "me-footer-options",
                    label { class: "me-checkbox-label",
                        input {
                            r#type: "checkbox",
                            checked: write_to_file(),
                            onchange: move |e| write_to_file.set(e.checked())
                        }
                        "Write changes to file"
                    }
                }
                div { class: "me-footer-actions",
                    button {
                        class: "me-btn me-btn-secondary",
                        onclick: move |_| props.on_close.call(()),
                        "Cancel"
                    }
                    button {
                        class: "me-btn me-btn-primary",
                        onclick: handle_save,
                        if is_batch {
                            "Apply to {track_count} Tracks"
                        } else {
                            "Save"
                        }
                    }
                }
            }
        }
    }
}
