use dioxus::prelude::*;
use std::time::Duration;

use state::{use_app_state, use_app_state_provider, use_storage_init};
use ui::{BeatGrid, DeckInfo, FrequencyWaveform, PlaybackBar, BASE_CSS};
use views::{DjView, DoctorView, Library};

mod state;
mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    #[layout(DesktopNavbar)]
        #[route("/")]
        DjView {},
        #[route("/library")]
        Library {},
        #[route("/doctor")]
        DoctorView {},
        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // Provide global app state to all components
    use_app_state_provider();
    // Initialize SQLite storage and auto-save
    use_storage_init();

    let app_state = use_app_state();
    let theme = app_state.get_theme();
    let theme_class = if theme == "light" { "theme-light" } else { "theme-dark" };

    rsx! {
        // Base theme styles from ui package
        document::Stylesheet { href: BASE_CSS }
        // Desktop-specific styles
        document::Stylesheet { href: MAIN_CSS }
        // Theme wrapper
        div {
            class: "{theme_class}",
            style: "min-height: 100vh;",
            Router::<Route> {}
        }
    }
}

/// Desktop navigation bar with tab-style navigation.
#[component]
fn DesktopNavbar() -> Element {
    let route = use_route::<Route>();
    let app_state = use_app_state();

    let is_dj = matches!(route, Route::DjView {});
    let is_library = matches!(route, Route::Library {});
    let is_doctor = matches!(route, Route::DoctorView {});

    // Get issue count for doctor badge
    let issue_count = app_state.diagnoses.read().len();
    let fixable_count = app_state
        .diagnoses
        .read()
        .iter()
        .filter(|d| d.fixable)
        .count();

    // Get current theme for toggle button
    let current_theme = app_state.get_theme();
    let theme_icon = if current_theme == "dark" { "☀️" } else { "🌙" };

    // Check if any deck has a track for the playback bar
    // Don't show playback bar on DJ view since it has full deck controls
    let deck1_track = app_state.deck1_track.read().clone();
    let deck2_track = app_state.deck2_track.read().clone();
    let has_any_track = deck1_track.is_some() || deck2_track.is_some();
    let show_playback_bar = has_any_track && !is_dj;

    rsx! {
        nav { class: "main-nav",
            div { class: "nav-brand",
                span { class: "brand-icon", "🎧" }
                span { class: "brand-name", "rec_bag" }
            }
            div { class: "nav-tabs",
                Link {
                    to: Route::DjView {},
                    class: if is_dj { "nav-tab active" } else { "nav-tab" },
                    span { class: "nav-tab-icon", "🎛️" }
                    span { class: "nav-tab-label", "DJ" }
                }
                Link {
                    to: Route::Library {},
                    class: if is_library { "nav-tab active" } else { "nav-tab" },
                    span { class: "nav-tab-icon", "📚" }
                    span { class: "nav-tab-label", "Library" }
                }
                Link {
                    to: Route::DoctorView {},
                    class: if is_doctor { "nav-tab active" } else { "nav-tab" },
                    span { class: "nav-tab-icon", "🩺" }
                    span { class: "nav-tab-label", "Doctor" }
                    if fixable_count > 0 {
                        span { class: "nav-tab-badge warning", "{fixable_count}" }
                    } else if issue_count > 0 {
                        span { class: "nav-tab-badge info", "{issue_count}" }
                    }
                }
            }
            div { class: "nav-spacer" }
            // Theme toggle button
            button {
                class: "nav-theme-toggle",
                title: if current_theme == "dark" { "Switch to light theme" } else { "Switch to dark theme" },
                onclick: move |_| {
                    spawn(async move {
                        app_state.toggle_theme().await;
                    });
                },
                "{theme_icon}"
            }
        }
        // Main content with padding for playback bar
        div {
            class: if show_playback_bar { "main-content with-playback-bar" } else { "main-content" },
            Outlet::<Route> {}
        }
        // Sticky playback bar (only when tracks are loaded and not on DJ view)
        if show_playback_bar {
            {render_playback_bar(app_state)}
        }
    }
}

/// Render the playback bar with deck state from AppState.
fn render_playback_bar(app_state: state::AppState) -> Element {
    // Get deck states from audio engine
    let (deck1_position, deck1_playing, deck2_position, deck2_playing) = {
        if let Ok(eng) = app_state.engine.read().lock() {
            let d1_pos = eng.deck(0).map(|d| d.position()).unwrap_or(Duration::ZERO);
            let d1_playing = eng
                .deck(0)
                .map(|d| d.state() == rec_core::DeckState::Playing)
                .unwrap_or(false);
            let d2_pos = eng.deck(1).map(|d| d.position()).unwrap_or(Duration::ZERO);
            let d2_playing = eng
                .deck(1)
                .map(|d| d.state() == rec_core::DeckState::Playing)
                .unwrap_or(false);
            (d1_pos, d1_playing, d2_pos, d2_playing)
        } else {
            (Duration::ZERO, false, Duration::ZERO, false)
        }
    };

    // Build deck info from track state
    let deck1_track = app_state.deck1_track.read().clone();
    let deck2_track = app_state.deck2_track.read().clone();

    let deck1_info = deck1_track
        .map(|t| DeckInfo {
            title: t.title.clone(),
            artist: t.artist.clone(),
            position: deck1_position,
            duration: t.duration.unwrap_or(Duration::ZERO),
            is_playing: deck1_playing,
            waveform_peaks: t.waveform.as_ref().map(|w| w.peaks.clone()),
            freq_waveform: t.freq_waveform.as_ref().map(|fw| FrequencyWaveform {
                low: fw.low.clone(),
                mid: fw.mid.clone(),
                high: fw.high.clone(),
            }),
            beat_grid: t.beat_grid.as_ref().map(|bg| BeatGrid {
                beats: bg.beats.clone(),
            }),
            bpm: t.bpm,
        })
        .unwrap_or_default();

    let deck2_info = deck2_track
        .map(|t| DeckInfo {
            title: t.title.clone(),
            artist: t.artist.clone(),
            position: deck2_position,
            duration: t.duration.unwrap_or(Duration::ZERO),
            is_playing: deck2_playing,
            waveform_peaks: t.waveform.as_ref().map(|w| w.peaks.clone()),
            freq_waveform: t.freq_waveform.as_ref().map(|fw| FrequencyWaveform {
                low: fw.low.clone(),
                mid: fw.mid.clone(),
                high: fw.high.clone(),
            }),
            beat_grid: t.beat_grid.as_ref().map(|bg| BeatGrid {
                beats: bg.beats.clone(),
            }),
            bpm: t.bpm,
        })
        .unwrap_or_default();

    rsx! {
        PlaybackBar {
            deck1: deck1_info,
            deck2: deck2_info,
            on_deck1_play_pause: move |_| {
                if let Ok(mut eng) = app_state.engine.read().lock() {
                    let is_playing = eng
                        .deck(0)
                        .map(|d| d.state() == rec_core::DeckState::Playing)
                        .unwrap_or(false);
                    if is_playing {
                        let _ = eng.pause(0);
                    } else {
                        let _ = eng.play(0);
                    }
                }
            },
            on_deck2_play_pause: move |_| {
                if let Ok(mut eng) = app_state.engine.read().lock() {
                    let is_playing = eng
                        .deck(1)
                        .map(|d| d.state() == rec_core::DeckState::Playing)
                        .unwrap_or(false);
                    if is_playing {
                        let _ = eng.pause(1);
                    } else {
                        let _ = eng.play(1);
                    }
                }
            },
            on_deck1_seek: move |pos: Duration| {
                if let Ok(mut eng) = app_state.engine.read().lock() {
                    let _ = eng.deck_mut(0).map(|d| d.seek(pos));
                }
            },
            on_deck2_seek: move |pos: Duration| {
                if let Ok(mut eng) = app_state.engine.read().lock() {
                    let _ = eng.deck_mut(1).map(|d| d.seek(pos));
                }
            },
        }
    }
}

/// 404 page for unknown routes.
#[component]
fn NotFound(segments: Vec<String>) -> Element {
    rsx! {
        div { class: "not-found",
            h1 { "Page Not Found" }
            p { "The page '/{segments.join(\"/\")}' doesn't exist." }
            Link { to: Route::DjView {}, "Go to DJ View" }
        }
    }
}
