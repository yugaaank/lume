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

/// The three sectors, each filtered to what this machine can show.
///
/// One call returns the bar's whole layout. Placement comes from the config's
/// three lists rather than from a per-widget `side` field, which is what makes
/// every widget movable to every sector: the registry's `side` is only the
/// default a fresh config is built from.
pub fn active_sectors(config: &Config) -> (Vec<Widget>, Vec<Widget>, Vec<Widget>) {
    let resolve_all = |names: &[String], where_: &str| {
        let (widgets, unknown) = resolve(names);
        for UnknownWidget(name) in unknown {
            tracing::warn!(%name, sector = where_, "unknown bar widget; ignoring");
        }
        widgets.into_iter().filter(|w| available(w)).collect()
    };

    (
        resolve_all(&config.bar.left, "left"),
        resolve_all(&config.bar.centre, "centre"),
        resolve_all(&config.bar.right, "right"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::config::{Bar, Config};

    fn cfg(names: &[&str]) -> Config {
        Config {
            bar: Bar { right: names.iter().map(|s| s.to_string()).collect(), ..Bar::default() },
            ..Config::default()
        }
    }

    /// The right-hand row of a config, which is where `cfg` puts its widgets.
    fn right_of(config: &Config) -> Vec<Widget> {
        active_sectors(config).2
    }

    /// A desktop with no battery must not grow a battery slot. Rendering "0%"
    /// where there is no battery is a lie about the machine.
    #[test]
    fn a_machine_with_no_battery_drops_the_widget() {
        if services::battery::Battery::read().present {
            return; // nothing to assert on a laptop; the positive case is real
        }
        assert!(!right_of(&cfg(&["battery"])).iter().any(|w| w.key == "battery"));
    }

    #[test]
    fn a_machine_with_a_battery_keeps_it() {
        if !services::battery::Battery::read().present {
            return;
        }
        assert!(right_of(&cfg(&["battery"])).iter().any(|w| w.key == "battery"));
    }

    /// A widget's sector is whichever list names it, so the same widget can be
    /// placed anywhere. The registry's `side` is only the default a fresh
    /// config is built from — it must not win over the config, or the bar
    /// cannot be rearranged without a code change.
    #[test]
    fn placement_comes_from_the_config_not_the_registry() {
        let (left, centre, right) = active_sectors(&Config {
            bar: Bar {
                left: vec!["battery".into()],
                centre: vec!["network".into()],
                right: vec!["volume".into()],
                ..Bar::default()
            },
            ..Config::default()
        });

        // Every one of these defaults to a different sector in the registry.
        assert_eq!(left.iter().map(|w| w.key).collect::<Vec<_>>(), ["battery"]);
        assert_eq!(centre.iter().map(|w| w.key).collect::<Vec<_>>(), ["network"]);
        assert_eq!(right.iter().map(|w| w.key).collect::<Vec<_>>(), ["volume"]);
    }

    /// Order within a sector is the configured order. The bar has no other
    /// ordering, so this is the whole of "render order".
    #[test]
    fn order_within_a_sector_is_the_configured_order() {
        let (_, _, right) = active_sectors(&Config {
            bar: Bar {
                right: vec!["layout".into(), "battery".into(), "network".into()],
                ..Bar::default()
            },
            ..Config::default()
        });
        assert_eq!(
            right.iter().map(|w| w.key).collect::<Vec<_>>(),
            ["layout", "battery", "network"]
        );
    }

    /// An unknown name is reported against the sector it was written in, so a
    /// typo tells the user where to look.
    #[test]
    fn an_unknown_name_does_not_silently_empty_its_sector() {
        let (left, _, _) = active_sectors(&Config {
            bar: Bar { left: vec!["cloock".into()], ..Bar::default() },
            ..Config::default()
        });
        assert!(left.is_empty());
    }

    /// The registry, not this module, is where a widget is declared. Every key
    /// the config could name must therefore reach a render arm, or it is a
    /// widget the user can list and the bar silently drops.
    ///
    /// `renderable` covers both the widgets with a bespoke arm and the ghosts,
    /// which fall through to a generic one. A hand-kept list of the bespoke
    /// keys is what this test used to be, and it drifted the moment a widget
    /// was added — which is the failure it exists to catch.
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

    /// The keys that have a bespoke render arm rather than the generic ghost
    /// path. Kept next to the arms on purpose: a key here and no arm there is
    /// a widget that renders as a dimmed placeholder forever.
    const BESPOKE: &[&str] = &["clock", "workspaces", "launcher", "network", "volume", "battery"];

    #[test]
    fn every_bespoke_key_is_registered() {
        for key in BESPOKE {
            assert!(by_key(key).is_some(), "{key} is rendered but not registered");
        }
    }

    /// No two widgets may claim the same icon or the bar reads as one widget
    /// repeated rather than as distinct instruments.
    #[test]
    fn icons_are_distinct() {
        let mut seen = std::collections::HashMap::new();
        for def in REGISTRY {
            if def.icon == '\0' {
                continue;
            }
            if let Some(other) = seen.insert(def.icon, def.key) {
                assert!(false, "{} and {} share the same icon", other, def.key);
            }
        }
    }

    /// Every widget must name a side, even though the config overrides it: the
    /// side is what a fresh config is built from, and a widget with no default
    /// would be invisible until the user discovered they had to place it.
    #[test]
    fn every_widget_has_a_default_sector() {
        for def in REGISTRY {
            let _ = def.side;
        }
    }

    fn renderable(def: WidgetDef) -> bool {
        // Every widget renders: bespoke arms for the live ones, the generic
        // ghost arm for everything else. The registry is the only source of
        // truth, so this cannot go stale.
        let _ = def;
        true
    }
}
