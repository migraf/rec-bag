//! Trackbar component for seeking within a track.
//!
//! Displays waveform visualization when available.

use dioxus::prelude::*;
use std::time::Duration;

const TRACKBAR_CSS: Asset = asset!("/assets/components/trackbar.css");

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
    /// Waveform peaks data (optional)
    #[props(default)]
    pub waveform_peaks: Option<Vec<f32>>,
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

/// Downsample waveform peaks to fit display width.
fn downsample_peaks(peaks: &[f32], target_count: usize) -> Vec<f32> {
    if peaks.is_empty() || target_count == 0 {
        return vec![];
    }
    if peaks.len() <= target_count {
        return peaks.to_vec();
    }
    
    let chunk_size = peaks.len() / target_count;
    peaks
        .chunks(chunk_size)
        .map(|chunk| {
            chunk
                .iter()
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .copied()
                .unwrap_or(0.0)
        })
        .take(target_count)
        .collect()
}

/// A seekable trackbar with waveform visualization.
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

    // Handle click to seek
    let on_seek = props.on_seek.clone();

    let handle_click = move |_e: MouseEvent| {
        // Future: Calculate seek position from click coordinates
        // For now, the invisible range slider handles seeking
    };

    // Downsample waveform to ~200 bars for display
    let display_peaks = props
        .waveform_peaks
        .as_ref()
        .map(|p| downsample_peaks(p, 200))
        .unwrap_or_default();

    let has_waveform = !display_peaks.is_empty();

    rsx! {
        document::Stylesheet { href: TRACKBAR_CSS }
        div {
            class: "{deck_class}",

            // Waveform area (clickable for seeking)
            div {
                class: "trackbar-waveform",
                onclick: handle_click,

                // Render waveform bars if data is available
                if has_waveform {
                    div { class: "waveform-bars",
                        for (i, peak) in display_peaks.iter().enumerate() {
                            {
                                let bar_progress = (i as f64 / display_peaks.len() as f64) * 100.0;
                                let is_played = bar_progress < progress;
                                let height_pct = (peak * 100.0).max(2.0);
                                let bar_class = if is_played { "waveform-bar played" } else { "waveform-bar" };
                                rsx! {
                                    div {
                                        key: "{i}",
                                        class: "{bar_class}",
                                        style: "height: {height_pct}%",
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Fallback pattern when no waveform data
                    div { class: "trackbar-waveform-pattern" }
                }

                // Progress overlay (semi-transparent)
                div {
                    class: "trackbar-progress",
                    style: "width: {progress}%",
                }

                // Playhead indicator
                div {
                    class: "trackbar-playhead",
                    style: "left: {progress}%",
                }

                // Seek slider (invisible, for precise control)
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
