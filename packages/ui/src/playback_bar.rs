//! Sticky playback bar component showing both decks.
//!
//! Appears at the bottom of the screen when any track is playing.

use crate::{BeatGrid, FrequencyWaveform, Trackbar};
use dioxus::prelude::*;
use std::time::Duration;

const PLAYBACK_BAR_CSS: Asset = asset!("/assets/components/playback-bar.css");

/// State of a deck for the playback bar.
#[derive(Debug, Clone, PartialEq)]
pub struct DeckInfo {
    /// Track title
    pub title: String,
    /// Track artist (optional)
    pub artist: Option<String>,
    /// Current playback position
    pub position: Duration,
    /// Total duration
    pub duration: Duration,
    /// Whether this deck is playing
    pub is_playing: bool,
    /// Simple waveform peaks (fallback)
    pub waveform_peaks: Option<Vec<f32>>,
    /// Frequency-band waveform
    pub freq_waveform: Option<FrequencyWaveform>,
    /// Beat grid
    pub beat_grid: Option<BeatGrid>,
    /// BPM value
    pub bpm: Option<f32>,
}

impl Default for DeckInfo {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: None,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            is_playing: false,
            waveform_peaks: None,
            freq_waveform: None,
            beat_grid: None,
            bpm: None,
        }
    }
}

impl DeckInfo {
    /// Check if this deck has a track loaded.
    pub fn has_track(&self) -> bool {
        !self.title.is_empty()
    }
}

/// Props for the PlaybackBar component.
#[derive(Props, Clone, PartialEq)]
pub struct PlaybackBarProps {
    /// Deck 1 info
    pub deck1: DeckInfo,
    /// Deck 2 info
    pub deck2: DeckInfo,
    /// Callback when play/pause is clicked on deck 1
    pub on_deck1_play_pause: EventHandler<()>,
    /// Callback when play/pause is clicked on deck 2
    pub on_deck2_play_pause: EventHandler<()>,
    /// Callback when seeking on deck 1
    pub on_deck1_seek: EventHandler<Duration>,
    /// Callback when seeking on deck 2
    pub on_deck2_seek: EventHandler<Duration>,
}

/// Format duration as MM:SS.
fn format_time(d: Duration) -> String {
    let total_secs = d.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

/// A sticky bottom playback bar showing both decks.
#[component]
pub fn PlaybackBar(props: PlaybackBarProps) -> Element {
    let has_any_track = props.deck1.has_track() || props.deck2.has_track();

    // Don't render if no tracks loaded
    if !has_any_track {
        return rsx! {};
    }

    rsx! {
        document::Stylesheet { href: PLAYBACK_BAR_CSS }
        div { class: "playback-bar",
            // Deck 1 section
            div { class: "playback-bar-deck deck-1",
                // Play/Pause button
                button {
                    class: if props.deck1.is_playing { "playback-btn playing" } else { "playback-btn" },
                    disabled: !props.deck1.has_track(),
                    onclick: move |_| props.on_deck1_play_pause.call(()),
                    if props.deck1.is_playing { "⏸" } else { "▶" }
                }

                // Track info
                div { class: "playback-info",
                    if props.deck1.has_track() {
                        div { class: "playback-title", "{props.deck1.title}" }
                        if let Some(ref artist) = props.deck1.artist {
                            div { class: "playback-artist", "{artist}" }
                        }
                    } else {
                        div { class: "playback-empty", "Deck 1" }
                    }
                }

                // Mini waveform
                div { class: "playback-waveform",
                    Trackbar {
                        position: props.deck1.position,
                        duration: props.deck1.duration,
                        is_playing: props.deck1.is_playing,
                        has_track: props.deck1.has_track(),
                        waveform_peaks: props.deck1.waveform_peaks.clone(),
                        freq_waveform: props.deck1.freq_waveform.clone(),
                        on_seek: props.on_deck1_seek.clone(),
                        deck_number: 1,
                        show_beat_grid: false,
                        compact: true,
                    }
                }

                // Time display
                div { class: "playback-time",
                    span { class: "time-current", "{format_time(props.deck1.position)}" }
                    if let Some(bpm) = props.deck1.bpm {
                        span { class: "time-bpm", "{bpm:.0}" }
                    }
                }
            }

            // Divider
            div { class: "playback-bar-divider" }

            // Deck 2 section
            div { class: "playback-bar-deck deck-2",
                // Play/Pause button
                button {
                    class: if props.deck2.is_playing { "playback-btn playing" } else { "playback-btn" },
                    disabled: !props.deck2.has_track(),
                    onclick: move |_| props.on_deck2_play_pause.call(()),
                    if props.deck2.is_playing { "⏸" } else { "▶" }
                }

                // Track info
                div { class: "playback-info",
                    if props.deck2.has_track() {
                        div { class: "playback-title", "{props.deck2.title}" }
                        if let Some(ref artist) = props.deck2.artist {
                            div { class: "playback-artist", "{artist}" }
                        }
                    } else {
                        div { class: "playback-empty", "Deck 2" }
                    }
                }

                // Mini waveform
                div { class: "playback-waveform",
                    Trackbar {
                        position: props.deck2.position,
                        duration: props.deck2.duration,
                        is_playing: props.deck2.is_playing,
                        has_track: props.deck2.has_track(),
                        waveform_peaks: props.deck2.waveform_peaks.clone(),
                        freq_waveform: props.deck2.freq_waveform.clone(),
                        on_seek: props.on_deck2_seek.clone(),
                        deck_number: 2,
                        show_beat_grid: false,
                        compact: true,
                    }
                }

                // Time display
                div { class: "playback-time",
                    span { class: "time-current", "{format_time(props.deck2.position)}" }
                    if let Some(bpm) = props.deck2.bpm {
                        span { class: "time-bpm", "{bpm:.0}" }
                    }
                }
            }
        }
    }
}
