//! This crate contains all shared UI for the workspace.

mod hero;
pub use hero::Hero;

mod navbar;
pub use navbar::Navbar;

mod echo;
pub use echo::Echo;

mod deck;
pub use deck::{DeckControl, DeckControlProps, DeckDisplayState};

mod playlist_browser;
pub use playlist_browser::{PlaylistBrowser, PlaylistBrowserProps};

mod track_list;
pub use track_list::{TrackList, TrackListProps};
