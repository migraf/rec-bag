//! Trackbar component for seeking within a track.
//!
//! Designed with a placeholder area for future waveform display.

use dioxus::prelude::*;
use std::time::Duration;

/// Props for the Trackbar component.
#[derive(Props, Clone, PartialEq)]
pub struct TrackbarProps {
    /// Current playback position
    pub position: Duration,
    /// Total track duration
    pub duration: Duration,
    /// Whether the track is currently playing
    pub is_playing: bool,
    /// Whether a track is loaded
    pub has_track: bool,
    /// Callback when user seeks to a new position
    pub on_seek: EventHandler<Duration>,
    /// Deck number for styling (1 or 2)
    #[props(default = 1)]
    pub deck_number: usize,
}

/// Format duration as MM:SS.
fn format_time(d: Duration) -> String {
    let total_secs = d.as_secs();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}

/// Calculate remaining time.
fn remaining_time(position: Duration, duration: Duration) -> Duration {
    duration.saturating_sub(position)
}

/// A seekable trackbar with waveform placeholder area.
#[component]
pub fn Trackbar(props: TrackbarProps) -> Element {
    let progress = if props.duration.as_secs_f64() > 0.0 {
        (props.position.as_secs_f64() / props.duration.as_secs_f64() * 100.0).min(100.0)
    } else {
        0.0
    };

    let remaining = remaining_time(props.position, props.duration);
    let deck_class = if props.has_track {
        format!("trackbar deck-{}-trackbar", props.deck_number)
    } else {
        format!("trackbar deck-{}-trackbar disabled", props.deck_number)
    };

    // Handle click to seek (using range input for now, waveform click handling for future)
    let on_seek = props.on_seek.clone();

    let handle_click = move |_e: MouseEvent| {
        // Future: Calculate seek position from click coordinates
        // For now, the invisible range slider handles seeking
    };

    rsx! {
        div {
            class: "{deck_class}",

            // Waveform placeholder area (clickable for seeking)
            div {
                class: "trackbar-waveform",
                onclick: handle_click,

                // Progress fill
                div {
                    class: "trackbar-progress",
                    style: "width: {progress}%",
                }

                // Playhead indicator
                div {
                    class: "trackbar-playhead",
                    style: "left: {progress}%",
                }

                // Waveform placeholder pattern
                div { class: "trackbar-waveform-pattern" }

                // Seek slider (invisible, inside waveform for proper positioning)
                input {
                    r#type: "range",
                    class: "trackbar-slider",
                    min: "0",
                    max: "{props.duration.as_millis()}",
                    value: "{props.position.as_millis()}",
                    disabled: !props.has_track,
                    oninput: move |e| {
                        if let Ok(ms) = e.value().parse::<u64>() {
                            on_seek.call(Duration::from_millis(ms));
                        }
                    }
                }
            }

            // Time display
            div { class: "trackbar-times",
                span { class: "trackbar-current", "{format_time(props.position)}" }
                span { class: "trackbar-remaining", "-{format_time(remaining)}" }
            }
        }
    }
}

