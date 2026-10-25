//! A rotary knob for bipolar controls such as EQ and trim.
//!
//! The knob's value runs from -1 (fully left) to 1 (fully right), with 0 at twelve
//! o'clock. Drag it up or down, scroll over it, or focus it and use the keyboard:
//! arrows nudge it, Page Up/Down move it further, Home/End send it to either end,
//! and Delete, Backspace, 0, or a double click put it back at twelve o'clock. Holding
//! Shift makes every gesture finer.

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

const KNOB_CSS: Asset = asset!("/assets/components/knob.css");

/// How far the knob turns either side of twelve o'clock, in degrees.
const SWEEP: f32 = 135.0;
/// One arrow-key press or scroll notch.
const STEP: f32 = 0.05;
/// One step with Shift held.
const FINE_STEP: f32 = 0.01;
/// One Page Up/Down press.
const PAGE_STEP: f32 = 0.25;
/// Pixels of vertical drag that turn the knob from one end to the other.
const DRAG_TRAVEL: f64 = 160.0;
/// How close to twelve o'clock a drag snaps to it.
const DRAG_DETENT: f32 = 0.03;

const CENTER: f32 = 18.0;
const RING_RADIUS: f32 = 15.0;

/// Props for the Knob component.
#[derive(Props, Clone, PartialEq)]
pub struct KnobProps {
    /// Position from -1 to 1; 0 is twelve o'clock.
    pub value: f32,
    /// What the knob controls, for assistive technology and the tooltip.
    pub label: String,
    /// The value as the user should read it, such as "+3.0 dB".
    pub value_text: String,
    /// Called with the new position whenever the knob is turned.
    pub on_change: EventHandler<f32>,
    /// Extra classes, for example to pick an accent color.
    #[props(default)]
    pub class: String,
}

/// A drag in progress: where the pointer was last, and the unsnapped position.
#[derive(Clone, Copy)]
struct Drag {
    last_y: f64,
    position: f32,
}

#[component]
pub fn Knob(props: KnobProps) -> Element {
    let mut drag = use_signal(|| None::<Drag>);
    let value = props.value.clamp(-1.0, 1.0);
    let on_change = props.on_change;
    let set = move |position: f32| on_change.call(position.clamp(-1.0, 1.0));

    let angle = value * SWEEP;
    let track = arc(-SWEEP, SWEEP);
    let fill = (value != 0.0).then(|| arc(angle.min(0.0), angle.max(0.0)));
    let (pointer_x1, pointer_y1) = point(angle, 5.0);
    let (pointer_x2, pointer_y2) = point(angle, 10.5);

    let mut classes = vec!["knob"];
    if value <= -1.0 {
        classes.push("knob-at-min");
    }
    if drag.read().is_some() {
        classes.push("knob-dragging");
    }
    if !props.class.is_empty() {
        classes.push(&props.class);
    }
    let class = classes.join(" ");

    rsx! {
        document::Stylesheet { href: KNOB_CSS }
        div {
            class: "{class}",
            role: "slider",
            tabindex: "0",
            title: "{props.label}: {props.value_text}",
            "aria-label": "{props.label}",
            "aria-valuemin": "-1",
            "aria-valuemax": "1",
            "aria-valuenow": "{value}",
            "aria-valuetext": "{props.value_text}",
            onmousedown: move |e| {
                if e.trigger_button() == Some(MouseButton::Primary) {
                    drag.set(Some(Drag { last_y: e.client_coordinates().y, position: value }));
                }
            },
            ondoubleclick: move |_| set(0.0),
            onwheel: move |e| {
                e.prevent_default();
                let notches = -e.delta().strip_units().y.signum() as f32;
                let step = if e.modifiers().contains(Modifiers::SHIFT) { FINE_STEP } else { STEP };
                set(value + notches * step);
            },
            onkeydown: move |e| {
                let step = if e.modifiers().contains(Modifiers::SHIFT) { FINE_STEP } else { STEP };
                let target = match e.key() {
                    Key::ArrowUp | Key::ArrowRight => value + step,
                    Key::ArrowDown | Key::ArrowLeft => value - step,
                    Key::PageUp => value + PAGE_STEP,
                    Key::PageDown => value - PAGE_STEP,
                    Key::Home => -1.0,
                    Key::End => 1.0,
                    Key::Delete | Key::Backspace => 0.0,
                    Key::Character(c) if c == "0" => 0.0,
                    _ => return,
                };
                e.prevent_default();
                set(target);
            },
            svg {
                class: "knob-dial",
                view_box: "0 0 36 36",
                "aria-hidden": "true",
                path { class: "knob-track", d: "{track}" }
                if let Some(fill) = fill {
                    path { class: "knob-fill", d: "{fill}" }
                }
                circle { class: "knob-cap", cx: "{CENTER}", cy: "{CENTER}", r: "11" }
                line {
                    class: "knob-pointer",
                    x1: "{pointer_x1}",
                    y1: "{pointer_y1}",
                    x2: "{pointer_x2}",
                    y2: "{pointer_y2}",
                }
            }
        }
        // While dragging, a surface over the whole window keeps receiving the pointer
        // even once it leaves the knob.
        if drag.read().is_some() {
            div {
                class: "knob-drag-surface",
                onmousemove: move |e| {
                    let Some(current) = drag() else { return };
                    let y = e.client_coordinates().y;
                    let travel = if e.modifiers().contains(Modifiers::SHIFT) { DRAG_TRAVEL * 4.0 } else { DRAG_TRAVEL };
                    let position = (current.position + ((current.last_y - y) / travel * 2.0) as f32)
                        .clamp(-1.0, 1.0);
                    drag.set(Some(Drag { last_y: y, position }));
                    set(if position.abs() < DRAG_DETENT { 0.0 } else { position });
                },
                onmouseup: move |_| drag.set(None),
                onmouseleave: move |_| drag.set(None),
            }
        }
    }
}

/// The point `radius` from the dial's centre at `angle` degrees clockwise from twelve.
fn point(angle: f32, radius: f32) -> (f32, f32) {
    let (sin, cos) = angle.to_radians().sin_cos();
    (CENTER + radius * sin, CENTER - radius * cos)
}

/// An SVG path along the ring, clockwise from `from` to `to` degrees.
fn arc(from: f32, to: f32) -> String {
    let (x1, y1) = point(from, RING_RADIUS);
    let (x2, y2) = point(to, RING_RADIUS);
    let large = if to - from > 180.0 { 1 } else { 0 };
    format!("M {x1:.2} {y1:.2} A {RING_RADIUS} {RING_RADIUS} 0 {large} 1 {x2:.2} {y2:.2}")
}
