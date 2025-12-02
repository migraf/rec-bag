use dioxus::prelude::*;

use state::{use_app_state_provider, use_storage_init};
use ui::Navbar;
use views::{DjView, Library};

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

    rsx! {
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        Router::<Route> {}
    }
}

/// Desktop navigation bar with tab-style navigation.
#[component]
fn DesktopNavbar() -> Element {
    let route = use_route::<Route>();
    let is_dj = matches!(route, Route::DjView {});
    let is_library = matches!(route, Route::Library {});

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
            }
            div { class: "nav-spacer" }
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
