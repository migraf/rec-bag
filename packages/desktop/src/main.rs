use dioxus::prelude::*;

#[cfg(feature = "desktop")]
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(feature = "desktop")]
use services::integration::{DesktopIntegration, DesktopStartup};
#[cfg(feature = "desktop")]
use state::integration::IntegrationError;
use state::{
    deck::Deck, integration::use_integration_state_provider, use_app_state, use_app_state_provider,
};
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

/// Loads `.env` (searched upward from the working directory) so local `REC_BAG_GOOGLE_*` settings
/// reach the desktop process.
///
/// Defaults to on for debug builds and off for release builds; `REC_BAG_LOAD_DOTENV` overrides
/// either default. Process environment always takes precedence over `.env` values.
#[cfg(feature = "desktop")]
fn load_dotenv() {
    let enabled = match std::env::var("REC_BAG_LOAD_DOTENV")
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Ok("true" | "1" | "yes") => true,
        Ok("false" | "0" | "no") => false,
        _ => cfg!(debug_assertions),
    };
    if enabled {
        let _ = dotenvy::dotenv();
    }
}

/// The desktop process is the only runtime permitted to acquire and mutate the local profile.
/// `dx serve --platform desktop` also starts a fullstack server sidecar for server functions;
/// letting that sidecar acquire the lease first would make the native UI block itself.
#[cfg(feature = "desktop")]
fn main() {
    // Before logging and any threads exist, so `.env` can also supply `RUST_LOG`.
    load_dotenv();
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
    let theme = app_state.theme();
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
    let diagnostics = app_state.diagnostics();
    let issue_count = diagnostics.issue_count();
    let fixable_count = diagnostics.fixable_count();

    // Get current theme for toggle button
    let current_theme = app_state.theme();
    let theme_icon = if current_theme == "dark" {
        "☀️"
    } else {
        "🌙"
    };

    // Check if any deck has a track for the playback bar
    // Don't show playback bar on DJ view since it has full deck controls
    let has_any_track = app_state.decks().iter().any(|deck| deck.track().is_some());
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

/// The playback bar, driven by the decks rather than by the audio engine directly.
#[component]
fn PlaybackBarWrapper() -> Element {
    let app_state = use_app_state();
    let decks = app_state.decks();

    rsx! {
        PlaybackBar {
            deck1: deck_info(&decks[0]),
            deck2: deck_info(&decks[1]),
            on_deck1_play_pause: move |_| { decks[0].toggle() },
            on_deck2_play_pause: move |_| { decks[1].toggle() },
            on_deck1_seek: move |position| { decks[0].seek(position) },
            on_deck2_seek: move |position| { decks[1].seek(position) },
        }
    }
}

/// Describe a deck's Loaded Track for the playback bar.
fn deck_info(deck: &Deck) -> DeckInfo {
    let Some(track) = deck.track() else {
        return DeckInfo::default();
    };
    DeckInfo {
        title: track.title.clone(),
        artist: track.artist.clone(),
        position: deck.position(),
        duration: track.duration.unwrap_or(Duration::ZERO),
        is_playing: deck.is_playing(),
        waveform_peaks: track.waveform.as_ref().map(|w| w.peaks.clone()),
        freq_waveform: track.freq_waveform.as_ref().map(|fw| FrequencyWaveform {
            low: fw.low.clone(),
            mid: fw.mid.clone(),
            high: fw.high.clone(),
        }),
        beat_grid: track.beat_grid.as_ref().map(|bg| BeatGrid {
            beats: bg.beats.clone(),
            downbeat_index: bg.downbeat_index,
        }),
        bpm: track.bpm,
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
