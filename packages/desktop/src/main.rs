use dioxus::prelude::*;

#[cfg(feature = "desktop")]
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(feature = "desktop")]
use services::integration::{DesktopIntegration, DesktopStartup};
#[cfg(feature = "desktop")]
use state::integration::IntegrationError;
use state::{integration::use_integration_state_provider, use_app_state, use_app_state_provider};
use ui::{BeatGrid, DeckInfo, FrequencyWaveform, PlaybackBar, BASE_CSS};
use views::{DjView, DoctorView, Library, MusicManagement};

mod services;
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
        #[route("/music-management")]
        MusicManagement {},
        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

const MAIN_CSS: Asset = asset!("/assets/main.css");

#[cfg(feature = "desktop")]
static INTEGRATION_STARTUP: OnceLock<Result<DesktopStartup, IntegrationError>> = OnceLock::new();

/// Installs one process-wide subscriber before Dioxus or Turso initialize tracing.
///
/// Development defaults expose the application's background lifecycle without enabling noisy
/// dependency spans (including Turso). `RUST_LOG` remains an explicit developer override.
fn initialize_logging() {
    let default_filter = if cfg!(debug_assertions) {
        "warn,desktop=debug"
    } else {
        "warn,desktop=info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .try_init();
}

/// The desktop process is the only runtime permitted to acquire and mutate the local profile.
/// `dx serve --platform desktop` also starts a fullstack server sidecar for server functions;
/// letting that sidecar acquire the lease first would make the native UI block itself.
#[cfg(feature = "desktop")]
fn main() {
    initialize_logging();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create the startup runtime");
    // Launch the UI immediately — integration opens in the background so the
    // window appears without waiting for catalog, migration, and service setup.
    dioxus::launch(App);
    if let Some(Ok(DesktopStartup::Ready(integration))) = INTEGRATION_STARTUP.get() {
        let _ = runtime.block_on(integration.shutdown());
    }
}

/// The fullstack server only hosts server functions and assets. It must never own a desktop
/// profile, because the companion native process is the sole catalog manager.
#[cfg(not(feature = "desktop"))]
fn main() {
    initialize_logging();
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    use_app_state_provider();
    use_integration_state_provider(None);
    let integration = state::integration::use_integration_state();
    use_drop(move || {
        if let Some(runtime) = integration.runtime() {
            runtime.begin_shutdown();
            spawn(async move {
                let _ = runtime.shutdown().await;
            });
        }
    });

    let app_state = use_app_state();
    let theme = app_state.get_theme();
    let theme_class = if theme == "light" {
        "theme-light"
    } else {
        "theme-dark"
    };

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
    let is_music_management = matches!(route, Route::MusicManagement {});

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
    let theme_icon = if current_theme == "dark" {
        "☀️"
    } else {
        "🌙"
    };

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
                Link {
                    to: Route::MusicManagement {},
                    class: if is_music_management { "nav-tab active" } else { "nav-tab" },
                    span { class: "nav-tab-icon", "☁️" }
                    span { class: "nav-tab-label", "Music management" }
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
            PlaybackBarWrapper {}
        }
    }
}

/// Wrapper component for the playback bar with reactive position updates.
#[component]
fn PlaybackBarWrapper() -> Element {
    let app_state = use_app_state();

    // Reactive signals for deck positions
    let mut deck1_position = use_signal(|| Duration::ZERO);
    let mut deck2_position = use_signal(|| Duration::ZERO);
    let mut deck1_playing = use_signal(|| false);
    let mut deck2_playing = use_signal(|| false);

    // Periodically update positions (every 50ms for smooth display)
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;

            if let Ok(eng) = app_state.engine.read().lock() {
                // Update deck 1
                if let Ok(deck) = eng.deck(0) {
                    deck1_position.set(deck.position());
                    deck1_playing.set(deck.state() == rec_core::DeckState::Playing);
                }
                // Update deck 2
                if let Ok(deck) = eng.deck(1) {
                    deck2_position.set(deck.position());
                    deck2_playing.set(deck.state() == rec_core::DeckState::Playing);
                }
            }
        }
    });

    // Build deck info from track state
    let deck1_track = app_state.deck1_track.read().clone();
    let deck2_track = app_state.deck2_track.read().clone();

    let deck1_info = deck1_track
        .map(|t| DeckInfo {
            title: t.title.clone(),
            artist: t.artist.clone(),
            position: deck1_position(),
            duration: t.duration.unwrap_or(Duration::ZERO),
            is_playing: deck1_playing(),
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
            position: deck2_position(),
            duration: t.duration.unwrap_or(Duration::ZERO),
            is_playing: deck2_playing(),
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
                // Immediately update position signal for responsive UI
                deck1_position.set(pos);
            },
            on_deck2_seek: move |pos: Duration| {
                if let Ok(mut eng) = app_state.engine.read().lock() {
                    let _ = eng.deck_mut(1).map(|d| d.seek(pos));
                }
                // Immediately update position signal for responsive UI
                deck2_position.set(pos);
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
