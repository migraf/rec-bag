//! Folder browser component for navigating music folders with subfolders.

use dioxus::prelude::*;
use rec_core::FolderNode;
use std::collections::HashSet;
use std::path::PathBuf;

const FOLDER_BROWSER_CSS: Asset = asset!("/assets/components/folder-browser.css");

/// Props for the FolderBrowser component.
#[derive(Props, Clone, PartialEq)]
pub struct FolderBrowserProps {
    /// The root folder node to display
    pub root: Option<FolderNode>,
    /// Currently selected folder path
    pub selected_path: Option<PathBuf>,
    /// Callback when a folder is selected
    pub on_select: EventHandler<PathBuf>,
}

/// A compact folder browser with collapsible tree navigation.
#[component]
pub fn FolderBrowser(props: FolderBrowserProps) -> Element {
    let mut expanded_paths: Signal<HashSet<PathBuf>> = use_signal(HashSet::new);

    // Clone root for use_effect to avoid move issues
    let root_for_effect = props.root.clone();

    // Auto-expand root on first render
    use_effect(move || {
        if let Some(ref root) = root_for_effect {
            expanded_paths.write().insert(root.path.clone());
        }
    });

    let Some(root) = props.root.clone() else {
        return rsx! {
            div { class: "folder-browser-empty",
                "No folders found"
            }
        };
    };

    rsx! {
        document::Stylesheet { href: FOLDER_BROWSER_CSS }
        div { class: "folder-browser",
            FolderTreeNode {
                node: root,
                depth: 0,
                selected_path: props.selected_path.clone(),
                expanded_paths: expanded_paths,
                on_select: props.on_select.clone(),
            }
        }
    }
}

/// Props for a single folder tree node.
#[derive(Props, Clone, PartialEq)]
struct FolderTreeNodeProps {
    node: FolderNode,
    depth: usize,
    selected_path: Option<PathBuf>,
    expanded_paths: Signal<HashSet<PathBuf>>,
    on_select: EventHandler<PathBuf>,
}

/// A single node in the folder tree.
#[component]
fn FolderTreeNode(mut props: FolderTreeNodeProps) -> Element {
    let is_expanded = props.expanded_paths.read().contains(&props.node.path);
    let is_selected = props.selected_path.as_ref() == Some(&props.node.path);
    let has_children = props.node.has_children();
    let node_path = props.node.path.clone();
    let toggle_path = node_path.clone();

    let toggle_expand = move |e: Event<MouseData>| {
        e.stop_propagation();
        let mut expanded = props.expanded_paths.write();
        if expanded.contains(&toggle_path) {
            expanded.remove(&toggle_path);
        } else {
            expanded.insert(toggle_path.clone());
        }
    };

    let select_folder = {
        let path = node_path.clone();
        move |_| {
            props.on_select.call(path.clone());
        }
    };

    let indent = props.depth * 12;

    rsx! {
        div { class: "folder-tree-item",
            div {
                class: if is_selected { "folder-row selected" } else { "folder-row" },
                style: "padding-left: {indent}px;",
                onclick: select_folder,

                // Expand/collapse chevron
                if has_children {
                    span {
                        class: if is_expanded { "folder-chevron expanded" } else { "folder-chevron" },
                        onclick: toggle_expand,
                        "›"
                    }
                } else {
                    span { class: "folder-chevron-spacer" }
                }

                // Folder icon
                span { class: "folder-icon",
                    if is_expanded && has_children { "📂" } else { "📁" }
                }

                // Folder name
                span { class: "folder-name", "{props.node.name}" }

                // File count badge
                span { class: "folder-count", "{props.node.file_count}" }
            }

            // Children (if expanded)
            if is_expanded && has_children {
                div { class: "folder-children",
                    for child in props.node.children.iter() {
                        FolderTreeNode {
                            key: "{child.path:?}",
                            node: child.clone(),
                            depth: props.depth + 1,
                            selected_path: props.selected_path.clone(),
                            expanded_paths: props.expanded_paths,
                            on_select: props.on_select.clone(),
                        }
                    }
                }
            }
        }
    }
}
