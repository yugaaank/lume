//! Widget menus: the popup that opens when you click a bar widget.
//!
//! ## This is a control popup, not a settings panel
//!
//! Reading Ryoku's own source settled an argument I had been having with
//! myself. `MenuWidgetHost.qml` hosts a **composable tree of functional
//! widgets** — an `HFader` that sets volume, an `AudioDevicePicker` that
//! switches output, a per-app mixer. It is not a form.
//!
//! Settings live somewhere else entirely: `widgets.json` carries a `settings`
//! schema per widget (`{key, type: toggle, label}`) and the Hub renders it.
//! Mixing the two is the mistake this file exists to avoid — a drawer full of
//! config toggles next to a slider that actually changes something is two
//! different jobs in one panel, and the user cannot tell which control will
//! survive a reboot.
//!
//! ## Anchored to the widget, not to the screen edge
//!
//! The bar hands over the pointer's x when the widget is clicked, and the
//! popup centres under it. Anchoring to a fixed edge means the arrow points at
//! nothing.
//!
//! ## Vocabulary
//!
//! `Section`, `Meter`, `Detail`, `Fader`, `Pill`, `Segmented`, `Action`,
//! `Footer`, `Row`, `Hint` — the same set the reference uses, so the two look
//! and behave alike. Adding one is a variant here and a builder call in a spec.

use std::sync::Arc;

/// Width of the popup. The reference measures ~295px on a 1920 screen.
pub const MENU_WIDTH: f32 = 300.0;

/// Longest value shown before it is elided.
///
/// A PipeWire sink description is routinely 60+ characters and an SSID can be
/// 32. This fork's layout engine has no text ellipsis, so the truncation has
/// to happen here or the value draws straight out of the panel.
pub const MAX_VALUE: usize = 30;

/// Elide to `max` characters, appending an ellipsis.
///
/// Character-based, not byte-based: a byte slice splits a multi-byte glyph and
/// produces mojibake at exactly the boundary.
pub fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// One row in a menu.
#[derive(Clone)]
pub enum Item {
    /// A title with an optional action on the right.
    Section {
        title: &'static str,
        action: Option<Action>,
    },

    /// A horizontal bar with a caption: the headline number.
    Meter {
        /// 0.0 ..= 1.0
        value: f32,
        caption: String,
    },

    /// label / value, the detail grid.
    Detail {
        label: &'static str,
        value: String,
    },

    /// A fader driven by the scroll wheel.
    Fader {
        min: f32,
        max: f32,
        step: f32,
        get: Arc<dyn Fn() -> f32 + Send + Sync>,
        set: Arc<dyn Fn(f32) + Send + Sync>,
    },

    /// An on/off pill.
    Pill {
        label: &'static str,
        get: Arc<dyn Fn() -> bool + Send + Sync>,
        set: Arc<dyn Fn(bool) + Send + Sync>,
    },

    /// One of a fixed set, shown as a row of buttons.
    Segmented {
        label: &'static str,
        options: Vec<&'static str>,
        selected: usize,
        set: Arc<dyn Fn(usize) + Send + Sync>,
    },

    /// A pressable row that runs something.
    Action(Action),

    /// A full-width footer button.
    Footer(Action),

    /// A tappable list row: title, subtitle, trailing text.
    Row {
        title: String,
        subtitle: String,
        trailing: String,
        run: Arc<dyn Fn() + Send + Sync>,
    },

    /// A quiet line with no affordance.
    Hint(String),
}

#[derive(Clone)]
pub struct Action {
    pub label: &'static str,
    pub run: Arc<dyn Fn() + Send + Sync>,
}

/// A whole menu.
#[derive(Clone)]
pub struct Menu {
    pub title: &'static str,
    pub items: Vec<Item>,
}

/// Which widget a menu belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuId {
    Volume,
    Network,
    Battery,
    Clock,
}

impl MenuId {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "volume" => Some(MenuId::Volume),
            "network" => Some(MenuId::Network),
            "battery" => Some(MenuId::Battery),
            "clock" => Some(MenuId::Clock),
            _ => None,
        }
    }

    /// The wire name, matching what `lume drawer <name>` takes.
    pub fn name(&self) -> &'static str {
        match self {
            MenuId::Volume => "volume",
            MenuId::Network => "network",
            MenuId::Battery => "battery",
            MenuId::Clock => "clock",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            MenuId::Volume => "Volume",
            MenuId::Network => "Network",
            MenuId::Battery => "Battery",
            MenuId::Clock => "Clock",
        }
    }

    /// Build the menu.
    ///
    /// Everything is read here, once, at open. A menu that re-read the world
    /// during render would fork `nmcli` and `wpctl` on every frame.
    pub fn build(&self) -> Menu {
        match self {
            MenuId::Volume => volume_menu(),
            MenuId::Network => network_menu(),
            other => Menu {
                title: other.title(),
                items: vec![Item::Hint("nothing to configure yet".into())],
            },
        }
    }
}

fn volume_menu() -> Menu {
    let sinks = services::volume::sinks();
    let sink_id = sinks.first().map(|s| s.sink_id.clone()).unwrap_or_default();
    let level = sinks.first().map(|s| s.level).unwrap_or(0.0);
    let muted = sinks.first().map(|s| s.muted).unwrap_or(false);

    let mut items = vec![
        Item::Meter {
            value: level,
            caption: if muted {
                "muted".to_string()
            } else {
                format!("{}%", (level * 100.0).round() as i32)
            },
        },
        Item::Fader {
            min: 0.0,
            max: 1.0,
            step: 0.05,
            get: Arc::new(move || level),
            set: Arc::new(move |v| {
                let _ = services::volume::set_level(&sink_id, v);
            }),
        },
        Item::Section {
            title: "OUTPUTS",
            action: Some(Action {
                label: "mute",
                run: Arc::new(|| {
                    if let Some(v) = services::volume::read() {
                        let _ = services::volume::set_muted(&v.sink_id, !v.muted);
                    }
                }),
            }),
        },
    ];

    for sink in &sinks {
        let is_default = sink.is_default;
        // Copied out, not captured by reference: the closure is 'static and
        // `sinks` is a local.
        let name = sink.sink_name.clone();
        items.push(Item::Row {
            title: name.clone(),
            subtitle: format!("{}%", sink.percent()),
            trailing: if is_default { "\u{2713}".into() } else { String::new() },
            run: Arc::new(move || {
                if !is_default {
                    if let Err(e) = services::volume::set_default_sink(&name) {
                        tracing::warn!("switch sink failed: {e}");
                    }
                }
            }),
        });
    }

    if sinks.len() <= 1 {
        items.push(Item::Hint("one output device".into()));
    }

    items.push(Item::Footer(Action {
        label: "Sound settings",
        run: Arc::new(|| {
            let _ = services::ipc::spawn_detached("pavucontrol", &[]);
        }),
    }));

    Menu { title: "Volume", items }
}

fn network_menu() -> Menu {
    let link = services::wifi::link();
    let networks = services::wifi::scan(false);

    let signal_fraction = if link.connected {
        networks
            .iter()
            .find(|n| n.active)
            .map(|n| n.signal as f32 / 100.0)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let mut items = vec![Item::Meter {
        value: signal_fraction,
        caption: networks
            .iter()
            .find(|n| n.active)
            .map(|n| format!("{}%", n.signal))
            .unwrap_or_else(|| "offline".to_string()),
    }];

    if link.connected {
        items.push(Item::Detail { label: "SSID", value: link.ssid.clone() });
        items.push(Item::Detail { label: "Type", value: link.kind.clone() });
        items.push(Item::Detail { label: "Interface", value: link.interface.clone() });
        items.push(Item::Detail { label: "IP", value: link.address.clone() });
        if link.frequency_mhz > 0 {
            items.push(Item::Detail {
                label: "Frequency",
                value: format!("{:.1} GHz", link.frequency_mhz as f32 / 1000.0),
            });
        }
        if link.link_mbps > 0 {
            items.push(Item::Detail {
                label: "Link speed",
                value: format!("{} Mbit/s", link.link_mbps),
            });
        }
    } else {
        items.push(Item::Hint("not connected".into()));
    }

    items.push(Item::Section {
        title: "AVAILABLE NETWORKS",
        action: Some(Action {
            // One press, never automatic: a rescan is a probe burst on every
            // network in range.
            label: "rescan",
            run: Arc::new(|| {
                let _ = services::wifi::scan(true);
            }),
        }),
    });

    if networks.is_empty() {
        items.push(Item::Hint("none found".into()));
    }

    for net in &networks {
        let ssid = net.ssid.clone();
        let title = net.ssid.clone();
        items.push(Item::Row {
            title,
            subtitle: if net.active { "connected".into() } else { net.security.clone() },
            trailing: "#".repeat(net.bars() as usize),
            run: Arc::new(move || {
                if let Err(e) = services::wifi::connect(&ssid) {
                    tracing::warn!(%ssid, "connect failed: {e}");
                }
            }),
        });
    }

    items.push(Item::Hint("Connect to a hidden network".into()));
    items.push(Item::Footer(Action {
        label: "Network settings",
        run: Arc::new(|| {
            let _ = services::ipc::spawn_detached("nm-connection-editor", &[]);
        }),
    }));

    Menu { title: "Network", items }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_names_parse() {
        assert_eq!(MenuId::parse("volume"), Some(MenuId::Volume));
        assert_eq!(MenuId::parse(" NETWORK "), Some(MenuId::Network));
        assert_eq!(MenuId::parse("workspaces"), None);
    }

    /// A menu that fails to build opens to nothing, and the user cannot tell
    /// that from a menu that is simply empty.
    #[test]
    fn every_menu_builds() {
        for id in [MenuId::Volume, MenuId::Network, MenuId::Battery, MenuId::Clock] {
            let menu = id.build();
            assert!(!menu.title.is_empty());
            assert!(!menu.items.is_empty(), "{} would open empty", id.title());
        }
    }

    #[test]
    fn the_volume_menu_has_exactly_one_fader() {
        let menu = MenuId::Volume.build();
        let faders = menu.items.iter().filter(|i| matches!(i, Item::Fader { .. })).count();
        assert_eq!(faders, 1);

        let Some(Item::Fader { min, max, step, .. }) =
            menu.items.iter().find(|i| matches!(i, Item::Fader { .. }))
        else {
            unreachable!()
        };
        assert_eq!(*min, 0.0);
        assert_eq!(*max, 1.0);
        assert!(*step > 0.0);
    }

    /// A detail row with an empty value renders as a blank line and reads as
    /// a missing feature.
    #[test]
    fn detail_rows_are_never_blank() {
        let menu = MenuId::Network.build();
        let mut rows = 0;
        for item in &menu.items {
            if let Item::Detail { label, value } = item {
                rows += 1;
                assert!(!value.trim().is_empty(), "detail row {label:?} has no value");
            }
        }
        // Either a connected machine with a grid, or an explicit "not
        // connected" hint. Never neither.
        let offline = menu
            .items
            .iter()
            .any(|i| matches!(i, Item::Hint(h) if h == "not connected"));
        assert!(rows > 0 || offline);
    }

    /// The network list must be sorted so the connected network is first.
    #[test]
    fn the_connected_network_is_listed_first() {
        let menu = MenuId::Network.build();
        let rows: Vec<_> = menu
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Row { title, subtitle, .. } => Some((title.clone(), subtitle.clone())),
                _ => None,
            })
            .collect();
        if let Some(first) = rows.first() {
            // If anything is marked connected it has to be the first row.
            if let Some(pos) = rows.iter().position(|r| r.1 == "connected") {
                assert_eq!(pos, 0, "connected network is not the first row");
            }
            assert!(!first.0.is_empty());
        }
    }

    #[test]
    fn long_values_are_elided_not_overflowed() {
        let long = "Alder Lake PCH-P High Definition Audio Controller Speaker";
        assert!(long.chars().count() > MAX_VALUE);
        assert_eq!(truncate("ears", MAX_VALUE), "ears");
        assert_eq!(truncate(long, MAX_VALUE).chars().count(), MAX_VALUE);
        assert!(truncate(long, MAX_VALUE).ends_with('\u{2026}'));

        // Character-safe: must not split a multi-byte glyph.
        let accents = "a\u{e9}\u{e8}\u{ea}bc\u{fc}d\u{e9}fghij";
        assert_eq!(truncate(accents, 6).chars().count(), 6);
    }
}
