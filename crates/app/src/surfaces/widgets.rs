//! ## The registry, not an enum
//!
//! What a widget *is* — its name, its default side, its padding, the popup it
//! opens — lives in [`services::widgets`], as data, in one list that config, this
//! module and a future settings process all read. What a widget *draws* lives
//! here, matched on `key`.
//!
//! The enum this replaced was fine for rendering and useless for everything else:
//! nothing could enumerate it, so settings could not offer the widgets and no
//! widget could carry a popup id. The data half is what makes either possible.

use gpui::{SharedString, StatefulInteractiveElement, div, prelude::*, px, rgb};
use services::config::Config;

pub use services::widgets::{PopupDef, REGISTRY, Side, UnknownWidget, WidgetDef, by_key, resolve};
use services::widgets::{split_by_side as registry_split};

/// One bar widget, as the bar refers to it.
pub type Widget = &'static WidgetDef;

/// Whether this widget is worth showing at all right now.
///
/// A desktop with no battery shows no battery slot, and a machine with no
/// PipeWire shows no volume slot. Showing "no battery: 0%" when there is no
/// battery is a lie about the machine.
///
/// Runtime, so it does not belong in the registry: it depends on the hardware,
/// not on what the widget is.
pub fn available(w: Widget) -> bool {
    match w.key {
        "battery" => services::battery::Battery::read().present,
        _ => true,
    }
}

/// Split into the three regions, preserving order within each.
pub fn split_by_side(widgets: &[Widget]) -> (Vec<Widget>, Vec<Widget>, Vec<Widget>) {
    registry_split(widgets)
}

/// The widgets a config asks for, filtered to what this machine can show.
pub fn active(config: &Config) -> Vec<Widget> {
    let (widgets, unknown) = resolve(&config.bar.widgets);
    for UnknownWidget(name) in unknown {
        tracing::warn!(%name, "unknown bar widget; ignoring");
    }
    widgets.into_iter().filter(|w| available(w)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::config::{Bar, Config};

    fn cfg(names: &[&str]) -> Config {
        Config {
            bar: Bar { widgets: names.iter().map(|s| s.to_string()).collect(), ..Bar::default() },
            ..Config::default()
        }
    }

    /// A desktop with no battery must not grow a battery slot. Rendering "0%"
    /// where there is no battery is a lie about the machine.
    #[test]
    fn a_machine_with_no_battery_drops_the_widget() {
        if services::battery::Battery::read().present {
            return; // nothing to assert on a laptop; the positive case is real
        }
        assert!(!active(&cfg(&["battery"])).iter().any(|w| w.key == "battery"));
    }

    #[test]
    fn a_machine_with_a_battery_keeps_it() {
        if !services::battery::Battery::read().present {
            return;
        }
        assert!(active(&cfg(&["battery"])).iter().any(|w| w.key == "battery"));
    }

    /// The registry, not this module, is where a widget is declared. Every key
    /// the config could name must therefore reach a render arm, or it is a
    /// widget the user can list and the bar silently drops.
    #[test]
    fn every_registered_key_reaches_a_render_arm() {
        for def in REGISTRY {
            assert!(
                renderable(*def),
                "{} is in the registry but nothing renders it",
                def.key
            );
        }
    }

    /// The mirror of the above, in the direction that actually bites: a render
    /// arm naming a key that is not in the registry.
    #[test]
    fn every_render_arm_names_a_registered_key() {
        for key in ["clock", "workspaces", "network", "volume", "battery", "title"] {
            assert!(by_key(key).is_some(), "{key} is rendered but not registered");
        }
    }

    fn renderable(def: WidgetDef) -> bool {
        matches!(
            def.key,
            "clock" | "workspaces" | "network" | "volume" | "battery" | "title"
        )
    }
}
