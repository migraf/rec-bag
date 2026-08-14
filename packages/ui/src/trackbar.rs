//! Trackbar component for seeking within a track.
//!
//! Displays waveform visualization with frequency bands and beat grid.

use dioxus::prelude::*;
use std::time::Duration;

const TRACKBAR_CSS: Asset = asset!("/assets/components/trackbar.css");

/// Frequency-band waveform data for colored visualization.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrequencyWaveform {
    /// Low frequency (bass) peaks.
    pub low: Vec<f32>,
    /// Mid frequency peaks.
    pub mid: Vec<f32>,
    /// High frequency (treble) peaks.
    pub high: Vec<f32>,
}

impl FrequencyWaveform {
    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.low.is_empty() && self.mid.is_empty() && self.high.is_empty()
    }
}

/// Beat grid data for visualization.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeatGrid {
    /// All beat positions in milliseconds.
    pub beats: Vec<f64>,
}

impl BeatGrid {
    /// Check if a beat is the start of a bar (every 4th beat).
    pub fn is_bar_start(&self, beat_index: usize) -> bool {
        beat_index % 4 == 0
    }
}

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
    /// Waveform peaks data (optional, legacy)
    #[props(default)]
    pub waveform_peaks: Option<Vec<f32>>,
    /// Frequency-band waveform data (preferred)
    #[props(default)]
    pub freq_waveform: Option<FrequencyWaveform>,
    /// Beat grid data for markers
    #[props(default)]
    pub beat_grid: Option<BeatGrid>,
    /// Callback when user seeks to a new position
    pub on_seek: EventHandler<Duration>,
    /// Deck number for styling (1 or 2)
    #[props(default = 1)]
    pub deck_number: usize,
    /// Whether to show beat grid markers
    #[props(default = true)]
    pub show_beat_grid: bool,
    /// Whether this is a mini/compact waveform (for playback bar)
    #[props(default = false)]
    pub compact: bool,
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

/// Downsample peaks to fit display width.
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

/// Downsample frequency waveform.
fn downsample_freq_waveform(fw: &FrequencyWaveform, target_count: usize) -> FrequencyWaveform {
    FrequencyWaveform {
        low: downsample_peaks(&fw.low, target_count),
        mid: downsample_peaks(&fw.mid, target_count),
        high: downsample_peaks(&fw.high, target_count),
    }
}

/// A seekable trackbar with frequency-band waveform and beat grid visualization.
#[component]
pub fn Trackbar(props: TrackbarProps) -> Element {
    let progress = if props.duration.as_secs_f64() > 0.0 {
        (props.position.as_secs_f64() / props.duration.as_secs_f64() * 100.0).min(100.0)
    } else {
        0.0
    };

    let remaining = remaining_time(props.position, props.duration);
    let deck_class = if props.has_track {
        if props.compact {
            format!("trackbar deck-{}-trackbar compact", props.deck_number)
        } else {
            format!("trackbar deck-{}-trackbar", props.deck_number)
        }
    } else if props.compact {
        format!(
            "trackbar deck-{}-trackbar compact disabled",
            props.deck_number
        )
    } else {
        format!("trackbar deck-{}-trackbar disabled", props.deck_number)
    };

    // Handle click to seek
    let on_seek = props.on_seek.clone();

    let _handle_click = move |_e: MouseEvent| {
        // Future: Calculate seek position from click coordinates
        // For now, the invisible range slider handles seeking
    };

    // Determine bar count based on compact mode
    let bar_count = if props.compact { 100 } else { 200 };

    // Use frequency waveform if available, otherwise fall back to simple waveform
    let has_freq_waveform = props
        .freq_waveform
        .as_ref()
        .map(|fw| !fw.is_empty())
        .unwrap_or(false);

    let display_freq_waveform = props
        .freq_waveform
        .as_ref()
        .map(|fw| downsample_freq_waveform(fw, bar_count));

    let display_peaks = if !has_freq_waveform {
        props
            .waveform_peaks
            .as_ref()
            .map(|p| downsample_peaks(p, bar_count))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let has_waveform = has_freq_waveform || !display_peaks.is_empty();

    // Calculate beat marker positions
    let duration_ms = props.duration.as_millis() as f64;
    let beat_markers: Vec<(f64, bool)> = if props.show_beat_grid && !props.compact {
        props
            .beat_grid
            .as_ref()
            .map(|bg| {
                bg.beats
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &beat_ms)| {
                        if duration_ms > 0.0 {
                            let position_pct = (beat_ms / duration_ms) * 100.0;
                            if position_pct >= 0.0 && position_pct <= 100.0 {
                                Some((position_pct, bg.is_bar_start(i)))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    rsx! {
        document::Stylesheet { href: TRACKBAR_CSS }
        div {
            class: "{deck_class}",

            // Waveform area (clickable for seeking)
            div {
                class: "trackbar-waveform",

                // Render frequency-band waveform if available
                if has_freq_waveform {
                    if let Some(ref fw) = display_freq_waveform {
                        div { class: "waveform-freq-bars",
                            for i in 0..fw.low.len() {
                                {
                                    let bar_progress = (i as f64 / fw.low.len() as f64) * 100.0;
                                    let is_played = bar_progress < progress;

                                    let low = fw.low.get(i).copied().unwrap_or(0.0);
                                    let mid = fw.mid.get(i).copied().unwrap_or(0.0);
                                    let high = fw.high.get(i).copied().unwrap_or(0.0);

                                    // Stack the bands - scale each to use more vertical space
                                    // Bass gets more visual weight, highs less
                                    let low_pct = (low * 45.0).max(0.5);
                                    let mid_pct = (mid * 35.0).max(0.5);
                                    let high_pct = (high * 20.0).max(0.5);

                                    let bar_class = if is_played {
                                        "waveform-freq-bar played"
                                    } else {
                                        "waveform-freq-bar"
                                    };

                                    rsx! {
                                        div {
                                            key: "{i}",
                                            class: "{bar_class}",
                                            // Low band (bass) - bottom, blue
                                            div {
                                                class: "freq-band low",
                                                style: "height: {low_pct}%",
                                            }
                                            // Mid band - middle, green
                                            div {
                                                class: "freq-band mid",
                                                style: "height: {mid_pct}%",
                                            }
                                            // High band (treble) - top, orange
                                            div {
                                                class: "freq-band high",
                                                style: "height: {high_pct}%",
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if has_waveform {
                    // Fallback to simple waveform
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

                // Beat grid markers
                if !beat_markers.is_empty() {
                    div { class: "beat-grid-overlay",
                        for (idx, (position, is_bar_start)) in beat_markers.iter().enumerate() {
                            {
                                let marker_class = if *is_bar_start {
                                    "beat-marker bar-start"
                                } else {
                                    "beat-marker"
                                };
                                rsx! {
                                    div {
                                        key: "beat-{idx}",
                                        class: "{marker_class}",
                                        style: "left: {position}%",
                                    }
                                }
                            }
                        }
                    }
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

            // Time display (hidden in compact mode)
            if !props.compact {
                div { class: "trackbar-times",
                    span { class: "trackbar-current", "{format_time(props.position)}" }
                    span { class: "trackbar-remaining", "-{format_time(remaining)}" }
                }
            }
        }
    }
}
