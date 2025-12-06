use dioxus::prelude::*;

use state::{use_app_state, use_app_state_provider, use_storage_init};
use ui::BASE_CSS;
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
        Outlet::<Route> {}
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
