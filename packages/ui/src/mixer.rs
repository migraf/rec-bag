//! The mixer between the two decks: a gain and three-band EQ per deck, and a
//! crossfader.

use crate::Knob;
use dioxus::prelude::*;
use rec_core::mixer::{eq_db, trim_db};
use rec_core::ChannelMix;

const MIXER_CSS: Asset = asset!("/assets/components/mixer.css");

/// One knob on a channel strip.
#[derive(Clone, Copy, PartialEq)]
enum Control {
    Gain,
    High,
    Mid,
    Low,
}

impl Control {
    const ALL: [Control; 4] = [Control::Gain, Control::High, Control::Mid, Control::Low];

    fn label(self) -> &'static str {
        match self {
            Control::Gain => "Gain",
            Control::High => "Hi",
            Control::Mid => "Mid",
            Control::Low => "Low",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Control::Gain => "gain",
            Control::High => "high EQ",
            Control::Mid => "mid EQ",
            Control::Low => "low EQ",
        }
    }

    fn get(self, mix: ChannelMix) -> f32 {
        match self {
            Control::Gain => mix.trim,
            Control::High => mix.high,
            Control::Mid => mix.mid,
            Control::Low => mix.low,
        }
    }

    fn with(self, mut mix: ChannelMix, value: f32) -> ChannelMix {
        match self {
            Control::Gain => mix.trim = value,
            Control::High => mix.high = value,
            Control::Mid => mix.mid = value,
            Control::Low => mix.low = value,
        }
        mix
    }

    /// The knob's setting as the user should read it.
    fn describe(self, value: f32) -> String {
        let db = match self {
            Control::Gain => Some(trim_db(value)),
            _ => eq_db(value),
        };
        match db {
            None => "Kill".to_string(),
            Some(db) if db.abs() < 0.05 => "0 dB".to_string(),
            Some(db) => format!("{db:+.1} dB"),
        }
    }
}

/// Props for the Mixer component.
#[derive(Props, Clone, PartialEq)]
pub struct MixerProps {
    /// Deck one's channel strip.
    pub deck1: ChannelMix,
    /// Deck two's channel strip.
    pub deck2: ChannelMix,
    /// Crossfader position, from -1 (only deck one) to 1 (only deck two).
    pub crossfader: f32,
    /// Called with a deck number and its new channel strip when a knob turns.
    pub on_mix_change: EventHandler<(usize, ChannelMix)>,
    /// Called with the new crossfader position.
    pub on_crossfader_change: EventHandler<f32>,
}

#[component]
pub fn Mixer(props: MixerProps) -> Element {
    let on_mix_change = props.on_mix_change;
    let on_crossfader_change = props.on_crossfader_change;
    let strips = [(1, props.deck1), (2, props.deck2)];
    let crossfader = (props.crossfader * 100.0).round() as i32;
    let crossfader_text = match crossfader {
        0 => "Centre".to_string(),
        c if c < 0 => format!("{}% towards deck 1", -c),
        c => format!("{c}% towards deck 2"),
    };

    rsx! {
        document::Stylesheet { href: MIXER_CSS }
        div { class: "mixer", role: "group", "aria-label": "Mixer",
            div { class: "mixer-strips",
                span { class: "mixer-deck mixer-deck-1", "1" }
                span {}
                span { class: "mixer-deck mixer-deck-2", "2" }
                for control in Control::ALL {
                    if control == Control::High {
                        div { class: "mixer-divider" }
                    }
                    for (number, mix) in strips {
                        if number == 2 {
                            span { class: "mixer-label", "{control.label()}" }
                        }
                        Knob {
                            value: control.get(mix),
                            label: format!("Deck {number} {}", control.name()),
                            value_text: control.describe(control.get(mix)),
                            class: "mixer-knob-{number}",
                            on_change: move |value| on_mix_change.call((number, control.with(mix, value))),
                        }
                    }
                }
            }
            div { class: "mixer-crossfader",
                input {
                    r#type: "range",
                    class: "crossfader",
                    min: "-100",
                    max: "100",
                    step: "1",
                    value: "{crossfader}",
                    title: "Crossfader: {crossfader_text}",
                    "aria-label": "Crossfader",
                    "aria-valuetext": "{crossfader_text}",
                    oninput: move |e| {
                        if let Ok(value) = e.value().parse::<f32>() {
                            on_crossfader_change.call(value / 100.0);
                        }
                    },
                    ondoubleclick: move |_| on_crossfader_change.call(0.0),
                }
            }
        }
    }
}
