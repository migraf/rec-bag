//! Deck control component for audio playback.

use crate::{BeatGrid, FrequencyWaveform, Trackbar};
use dioxus::prelude::*;
use rec_core::{SourceType, Track};
use std::time::Duration;

const DECK_CSS: Asset = asset!("/assets/components/deck.css");

/// State of a deck for UI display.
#[derive(Debug, Clone, PartialEq)]
pub enum DeckDisplayState {
    Empty,
    Stopped,
    Playing,
    Paused,
}

/// Credit for a track streamed from a provider rather than played from a local file.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamCredit {
    pub source_type: SourceType,
    /// The track's page at the provider.
    pub url: Option<String>,
    /// Whether only a short preview is playing.
    pub preview: bool,
}

/// A quiet "via SoundCloud" on the artist line, linking to the track's page.
#[component]
fn StreamSource(stream: StreamCredit) -> Element {
    let provider = match stream.source_type {
        SourceType::SoundCloud => "SoundCloud",
        SourceType::GoogleDrive => "Drive",
        SourceType::Dropbox => "Dropbox",
        _ => "stream",
    };
    let label = if stream.preview {
        format!("preview via {provider}")
    } else {
        format!("via {provider}")
    };
    rsx! {
        if let Some(url) = stream.url {
            a {
                class: "track-source",
                href: "{url}",
                target: "_blank",
                rel: "noopener noreferrer",
                title: "Streaming from {provider}. Open the track there",
                "{label}"
            }
        } else {
            span { class: "track-source", title: "Streaming from {provider}", "{label}" }
        }
    }
}

/// Props for the DeckControl component.
#[derive(Props, Clone, PartialEq)]
pub struct DeckControlProps {
    /// Deck number (1, 2, etc.)
    pub deck_number: usize,
    /// Current deck state
    pub state: DeckDisplayState,
    /// Currently loaded track (if any)
    pub track: Option<Track>,
    /// Current playback position
    pub position: Duration,
    /// Current volume (0.0 - 1.0)
    pub volume: f32,
    /// Callback when play is pressed
    pub on_play: EventHandler<()>,
    /// Callback when pause is pressed
    pub on_pause: EventHandler<()>,
    /// Callback when stop is pressed
    pub on_stop: EventHandler<()>,
    /// Callback when volume changes
    pub on_volume_change: EventHandler<f32>,
    /// Callback when seeking to a position
    pub on_seek: EventHandler<Duration>,
    /// Callback when a file is dropped
    pub on_file_drop: EventHandler<String>,
    /// Who is streaming the loaded track, when it is not a local file
    #[props(default)]
    pub stream: Option<StreamCredit>,
    /// The title of the track being prepared for this deck, while one is
    #[props(default)]
    pub loading: Option<String>,
}

/// A single deck control panel.
#[component]
pub fn DeckControl(props: DeckControlProps) -> Element {
    let is_playing = props.state == DeckDisplayState::Playing;
    let is_paused = props.state == DeckDisplayState::Paused;
    let has_track = props.track.is_some();
    let can_play = has_track && !is_playing;
    let can_pause = has_track && is_playing;
    let can_stop = has_track && (is_playing || is_paused);

    let track_title = props
        .track
        .as_ref()
        .map(|t| t.title.clone())
        .unwrap_or_else(|| "No track loaded".to_string());

    let track_artist = props
        .track
        .as_ref()
        .and_then(|t| t.artist.clone())
        .unwrap_or_default();

    let track_bpm = props
        .track
        .as_ref()
        .and_then(|t| t.bpm)
        .map(|b| format!("{:.1}", b))
        .unwrap_or_default();

    let track_key = props
        .track
        .as_ref()
        .and_then(|t| t.key.clone())
        .unwrap_or_default();

    let duration = props
        .track
        .as_ref()
        .and_then(|t| t.duration)
        .unwrap_or_default();

    // Get waveform peaks from track
    let waveform_peaks = props
        .track
        .as_ref()
        .and_then(|t| t.waveform.as_ref())
        .map(|w| w.peaks.clone());

    // Get frequency waveform from track
    let freq_waveform = props.track.as_ref().and_then(|t| {
        t.freq_waveform.as_ref().map(|fw| FrequencyWaveform {
            low: fw.low.clone(),
            mid: fw.mid.clone(),
            high: fw.high.clone(),
        })
    });

    // Get beat grid from track
    let beat_grid = props.track.as_ref().and_then(|t| {
        t.beat_grid.as_ref().map(|bg| BeatGrid {
            beats: bg.beats.clone(),
        })
    });

    rsx! {
        document::Stylesheet { href: DECK_CSS }
        div { class: "deck deck-{props.deck_number}",
            div { class: "deck-header",
                span { class: "deck-number", "Deck {props.deck_number}" }
                // In the header, so the track display keeps its two lines while it loads.
                if let Some(title) = props.loading.clone() {
                    div { class: "deck-loading", role: "status", "aria-live": "polite", title: "Loading {title}",
                        span { class: "deck-spinner", "aria-hidden": "true" }
                        span { class: "deck-loading-title", "Loading {title}…" }
                    }
                }
                div { class: "deck-header-info",
                    if !track_bpm.is_empty() {
                        span { class: "deck-bpm", "{track_bpm}" }
                        span { class: "deck-info-label", "BPM" }
                    }
                    if !track_key.is_empty() {
                        span { class: "deck-key", "{track_key}" }
                        span { class: "deck-info-label", "KEY" }
                    }
                }
            }

            div { class: "deck-display",
                div { class: "track-info",
                    div { class: "track-title", title: "{track_title}", "{track_title}" }
                    if !track_artist.is_empty() || props.stream.is_some() {
                        div { class: "track-artist",
                            span { class: "track-artist-name", "{track_artist}" }
                            // A streamed track credits its uploader (the artist) and the
                            // provider, linking back to the track's page there.
                            if let Some(stream) = props.stream.clone() {
                                StreamSource { stream }
                            }
                        }
                    }
                }
            }

            // Trackbar with waveform, frequency bands, and beat grid
            Trackbar {
                position: props.position,
                duration,
                is_playing,
                has_track,
                waveform_peaks,
                freq_waveform,
                beat_grid,
                on_seek: props.on_seek.clone(),
                deck_number: props.deck_number,
            }

            div { class: "deck-controls-row",
                div { class: "deck-controls",
                    button {
                        class: if can_play { "control-btn play-btn" } else { "control-btn play-btn disabled" },
                        disabled: !can_play,
                        onclick: move |_| props.on_play.call(()),
                        "▶"
                    }
                    button {
                        class: if can_pause { "control-btn pause-btn" } else { "control-btn pause-btn disabled" },
                        disabled: !can_pause,
                        onclick: move |_| props.on_pause.call(()),
                        "⏸"
                    }
                    button {
                        class: if can_stop { "control-btn stop-btn" } else { "control-btn stop-btn disabled" },
                        disabled: !can_stop,
                        onclick: move |_| props.on_stop.call(()),
                        "⏹"
                    }
                }

                div { class: "volume-control",
                    label { "Vol" }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        value: "{(props.volume * 100.0) as i32}",
                        oninput: move |e| {
                            if let Ok(val) = e.value().parse::<f32>() {
                                props.on_volume_change.call(val / 100.0);
                            }
                        }
                    }
                    span { class: "volume-value", "{(props.volume * 100.0) as i32}%" }
                }
            }
        }
    }
}
