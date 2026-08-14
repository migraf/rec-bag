//! Context menu component for track actions.

use dioxus::prelude::*;
use uuid::Uuid;

const CONTEXT_MENU_CSS: Asset = asset!("/assets/components/context-menu.css");

/// A menu item in the context menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// Unique ID for this item
    pub id: String,
    /// Display label
    pub label: String,
    /// Optional icon
    pub icon: Option<String>,
    /// Whether this item is disabled
    pub disabled: bool,
    /// Whether this is a separator
    pub separator: bool,
    /// Submenu items (if any)
    pub submenu: Option<Vec<MenuItem>>,
}

impl MenuItem {
    /// Create a new menu item.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            disabled: false,
            separator: false,
            submenu: None,
        }
    }

    /// Create a separator.
    pub fn separator() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            icon: None,
            disabled: false,
            separator: true,
            submenu: None,
        }
    }

    /// Add an icon.
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Set disabled state.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Add a submenu.
    pub fn with_submenu(mut self, items: Vec<MenuItem>) -> Self {
        self.submenu = Some(items);
        self
    }
}

/// Playlist info for the add-to-playlist submenu.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistInfo {
    pub id: Uuid,
    pub name: String,
}

/// Props for the ContextMenu component.
#[derive(Props, Clone, PartialEq)]
pub struct ContextMenuProps {
    /// Whether the menu is visible
    pub visible: bool,
    /// X position of the menu
    pub x: i32,
    /// Y position of the menu
    pub y: i32,
    /// List of playlists for "Add to playlist" submenu
    pub playlists: Vec<PlaylistInfo>,
    /// Number of selected tracks (for batch operations)
    pub selected_count: usize,
    /// Callback when menu item is clicked
    pub on_action: EventHandler<ContextMenuAction>,
    /// Callback when menu should close
    pub on_close: EventHandler<()>,
    /// Whether selected tracks have fixable issues (shows "Fix Issues" option)
    #[props(default = false)]
    pub has_fixable_issues: bool,
}

/// Actions that can be triggered from the context menu.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextMenuAction {
    /// Add to existing playlist
    AddToPlaylist(Uuid),
    /// Create new playlist with selected tracks
    AddToNewPlaylist,
    /// Remove from current playlist
    RemoveFromPlaylist,
    /// Load to deck 1
    LoadToDeck1,
    /// Load to deck 2
    LoadToDeck2,
    /// Show track info
    ShowInfo,
    /// Fix issues for the selected track(s)
    FixIssues,
    /// Edit metadata for selected track(s)
    EditMetadata,
    /// Analyze track(s) (waveform, BPM, key, beat grid)
    Analyze,
}

/// Context menu component.
#[component]
pub fn ContextMenu(props: ContextMenuProps) -> Element {
    let mut show_playlist_submenu = use_signal(|| false);

    if !props.visible {
        return rsx! {};
    }

    let track_label = if props.selected_count > 1 {
        format!("{} tracks", props.selected_count)
    } else {
        "track".to_string()
    };

    rsx! {
        document::Stylesheet { href: CONTEXT_MENU_CSS }
        // Backdrop to catch clicks outside
        div {
            class: "context-menu-backdrop",
            onclick: move |_| props.on_close.call(())
        }

        // Menu container
        div {
            class: "context-menu",
            style: "left: {props.x}px; top: {props.y}px;",
            // Add to playlist (with submenu)
            div {
                class: "context-menu-item has-submenu",
                onmouseenter: move |_| show_playlist_submenu.set(true),
                onmouseleave: move |_| show_playlist_submenu.set(false),
                span { class: "menu-icon", "📋" }
                span { class: "menu-label", "Add to playlist" }
                span { class: "menu-arrow", "▶" }

                // Submenu
                if show_playlist_submenu() {
                    div { class: "context-submenu",
                        // New playlist option
                        div {
                            class: "context-menu-item",
                            onclick: move |_| {
                                props.on_action.call(ContextMenuAction::AddToNewPlaylist);
                                props.on_close.call(());
                            },
                            span { class: "menu-icon", "+" }
                            span { class: "menu-label", "New playlist..." }
                        }

                        if !props.playlists.is_empty() {
                            div { class: "context-menu-separator" }
                        }

                        // Existing playlists
                        for playlist in props.playlists.iter() {
                            {
                                let playlist_id = playlist.id;
                                let playlist_name = playlist.name.clone();
                                rsx! {
                                    div {
                                        key: "{playlist_id}",
                                        class: "context-menu-item",
                                        onclick: move |_| {
                                            props.on_action.call(ContextMenuAction::AddToPlaylist(playlist_id));
                                            props.on_close.call(());
                                        },
                                        span { class: "menu-label", "{playlist_name}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "context-menu-separator" }

            // Load to deck options
            div {
                class: "context-menu-item",
                onclick: move |_| {
                    props.on_action.call(ContextMenuAction::LoadToDeck1);
                    props.on_close.call(());
                },
                span { class: "menu-icon", "1" }
                span { class: "menu-label", "Load to Deck 1" }
            }

            div {
                class: "context-menu-item",
                onclick: move |_| {
                    props.on_action.call(ContextMenuAction::LoadToDeck2);
                    props.on_close.call(());
                },
                span { class: "menu-icon", "2" }
                span { class: "menu-label", "Load to Deck 2" }
            }

            div { class: "context-menu-separator" }

            // Edit Metadata option
            div {
                class: "context-menu-item",
                onclick: move |_| {
                    props.on_action.call(ContextMenuAction::EditMetadata);
                    props.on_close.call(());
                },
                span { class: "menu-icon", "📝" }
                span { class: "menu-label",
                    if props.selected_count > 1 {
                        "Edit Metadata ({props.selected_count})"
                    } else {
                        "Edit Metadata"
                    }
                }
            }

            // Analyze option
            div {
                class: "context-menu-item",
                onclick: move |_| {
                    props.on_action.call(ContextMenuAction::Analyze);
                    props.on_close.call(());
                },
                span { class: "menu-icon", "🔄" }
                span { class: "menu-label",
                    if props.selected_count > 1 {
                        "Analyze ({props.selected_count})"
                    } else {
                        "Analyze"
                    }
                }
            }

            // Fix Issues option (only shown if tracks have fixable issues)
            if props.has_fixable_issues {
                div {
                    class: "context-menu-item",
                    onclick: move |_| {
                        props.on_action.call(ContextMenuAction::FixIssues);
                        props.on_close.call(());
                    },
                    span { class: "menu-icon", "🔧" }
                    span { class: "menu-label", "Fix Issues" }
                }
            }

            div { class: "context-menu-separator" }

            // Remove from playlist
            div {
                class: "context-menu-item danger",
                onclick: move |_| {
                    props.on_action.call(ContextMenuAction::RemoveFromPlaylist);
                    props.on_close.call(());
                },
                span { class: "menu-icon", "✕" }
                span { class: "menu-label", "Remove {track_label}" }
            }
        }
    }
}
