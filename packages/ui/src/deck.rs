//! Deck control component for audio playback.

use crate::Trackbar;
use dioxus::prelude::*;
use rec_core::Track;
use std::time::Duration;

/// State of a deck for UI display.
#[derive(Debug, Clone, PartialEq)]
pub enum DeckDisplayState {
    Empty,
    Stopped,
    Playing,
    Paused,
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
        .map(|b| format!("{:.1} BPM", b))
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

    rsx! {
        div { class: "deck deck-{props.deck_number}",
            div { class: "deck-header",
                span { class: "deck-number", "Deck {props.deck_number}" }
                div { class: "deck-header-info",
                    if !track_bpm.is_empty() {
                        span { class: "deck-bpm", "{track_bpm}" }
                    }
                }
            }

            div { class: "deck-display",
                div { class: "track-info",
                    div { class: "track-title", "{track_title}" }
                    if !track_artist.is_empty() {
                        div { class: "track-artist", "{track_artist}" }
                    }
                }
            }

            // Trackbar with waveform
            Trackbar {
                position: props.position,
                duration,
                is_playing,
                has_track,
                waveform_peaks,
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

