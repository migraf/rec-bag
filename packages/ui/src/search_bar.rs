//! Search bar component with debounced input.

use dioxus::prelude::*;

const SEARCH_BAR_CSS: Asset = asset!("/assets/components/search-bar.css");

/// Props for the SearchBar component.
#[derive(Props, Clone, PartialEq)]
pub struct SearchBarProps {
    /// Current search query value
    pub value: String,
    /// Placeholder text
    #[props(default = "Search...".to_string())]
    pub placeholder: String,
    /// Callback when search query changes
    pub on_change: EventHandler<String>,
    /// Callback when search is cleared
    #[props(default)]
    pub on_clear: EventHandler<()>,
    /// Number of results (for display)
    #[props(default)]
    pub result_count: Option<usize>,
    /// Whether search is active/focused
    #[props(default = false)]
    pub compact: bool,
}

/// Search bar component with clear button and result count.
#[component]
pub fn SearchBar(props: SearchBarProps) -> Element {
    let mut is_focused = use_signal(|| false);
    let initial_value = props.value.clone();
    let mut local_value = use_signal(move || initial_value);

    let has_value = !local_value().is_empty();
    let show_results = has_value && props.result_count.is_some();

    rsx! {
        document::Stylesheet { href: SEARCH_BAR_CSS }
        div {
            class: if props.compact { "search-bar compact" } else { "search-bar" },
            class: if is_focused() { "focused" } else { "" },
            // Search icon
            span { class: "search-icon", "🔍" }

            // Input field
            input {
                r#type: "text",
                class: "search-input",
                placeholder: "{props.placeholder}",
                value: "{local_value}",
                onfocus: move |_| is_focused.set(true),
                onblur: move |_| is_focused.set(false),
                oninput: move |e| {
                    let value = e.value();
                    local_value.set(value.clone());
                    props.on_change.call(value);
                },
                onkeydown: move |e| {
                    if e.key() == Key::Escape {
                        local_value.set(String::new());
                        props.on_change.call(String::new());
                        props.on_clear.call(());
                    }
                }
            }

            // Result count badge
            if show_results {
                span { class: "search-results-badge",
                    "{props.result_count.unwrap()}"
                }
            }

            // Clear button
            if has_value {
                button {
                    class: "search-clear-btn",
                    onclick: move |_| {
                        local_value.set(String::new());
                        props.on_change.call(String::new());
                        props.on_clear.call(());
                    },
                    "✕"
                }
            }
        }
    }
}
