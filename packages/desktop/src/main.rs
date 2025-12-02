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

/// Desktop navigation bar.
#[component]
fn DesktopNavbar() -> Element {
    rsx! {
        Navbar {
            Link {
                to: Route::DjView {},
                "DJ"
            }
            Link {
                to: Route::Library {},
                "Library"
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
