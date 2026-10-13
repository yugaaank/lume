//! The bar's widget registry: one list, read by everything.
//!
//! ## Why a list and not an enum
//!
//! The widgets used to be a `Widget` enum with a variant each, and a hand-written
//! `match` turning a config string into one. That is fine for rendering and
//! useless for everything else: nothing can *enumerate* the widgets, so a
//! settings screen cannot offer them, an IPC verb cannot name one, and the
//! per-widget settings schema has nowhere to live.
//!
//! Ryoku has the same shape in two places and pays for it — 19 built-ins in its
//! registry, 10 in its settings catalogue, and the difference is reachable in the
//! bar but invisible to settings. One list fixes that by construction.
//!
//! ## Data here, pixels elsewhere
//!
//! This module has no GPUI in it and must not grow any. What a widget *is* — its
//! name, which side it defaults to, how much room it wants, what popup it opens
//! — is data, and data belongs where config and a future settings process can
//! read it. How it draws is the app crate's business, and the app matches on
//! `key`.
//!
//! ## Padding is uniform, `has_fill` is not yet
//!
//! Only fields something reads are here. Per-widget fill and border flags, and
//! the settings schema, are deliberately absent: they belong to the separator
//! work and to the settings process, and shipping them now would be two fields
//! with no reader, which is how a registry starts lying about its contents.

/// Which region of the bar a widget sits in when the user has not said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    /// Dead centre.
    ///
    /// Not a flexbox detail — a third region with its own rules. A time readout
    /// is the one thing on a bar that reads as deliberate when it is centred,
    /// and every tiling shell puts it there.
    Middle,
    Right,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Middle => "middle",
            Side::Right => "right",
        }
    }
}

/// The popup a widget opens when clicked.
///
/// The *identifier*, never the content. Which panel that identifier resolves to
/// is the widget's own business, so a widget can open anything; the registry
/// exists so that settings and IPC can still name it.
///
/// `pages` are deep-link targets, written `id#page`. One entry point covers
/// "the network panel" and "the network panel, on the wifi tab" without a second
/// verb or a second state variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupDef {
    pub id: &'static str,
    pub pages: &'static [&'static str],
}

/// One bar widget, as data.
///
/// Every use here is by key, which is what makes it the identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidgetDef {
    /// The name written in `bar.widgets`. This is the identity, so renaming one
    /// breaks every config that names it — hence `aliases`, which is the
    /// practical mitigation rather than a separate stable key that nothing yet
    /// needs.
    pub key: &'static str,
    /// Other names that resolve to this widget. A rename adds the old name here.
    pub aliases: &'static [&'static str],
    /// Shown in settings.
    pub label: &'static str,
    /// The Nerd Font codepoint this widget draws as its icon.
    ///
    /// Every widget is icon + value, which is what makes the bar read as one
    /// instrument rather than as loose numbers. `'\0'` when the widget is drawn
    /// rather than typed — the battery cell — or has no icon.
    pub icon: char,
    /// Renders a placeholder because lume has no data source for it yet.
    ///
    /// The bar shows these at their final size and position, so nothing moves
    /// when they come alive. A ghost claims nothing about the system, which is
    /// the honest state of a widget with no source behind it.
    pub ghost: bool,
    /// A single decorative glyph. Not a translation and not a label — the
    /// reference uses one kanji per region, and it reads as an identity mark
    /// rather than as text. Empty when there is nothing honest to put here.
    pub gloss: &'static str,
    pub side: Side,
    /// Extra room around this widget's content, in px.
    ///
    /// Padding is per widget because a digit and a wordmark need different
    /// amounts of it, and one global gap makes both look wrong.
    pub popup: Option<PopupDef>,
}

/// Every widget lume can put on a bar.
///
/// The single list. The bar renders from it, the settings process will build
/// from it, and an IPC verb will name from it — which is the point of having it
/// at all.
pub const REGISTRY: &[WidgetDef] = &[
    // ---- left ----
    WidgetDef {
        key: "launcher",
        aliases: &["arch"],
        label: "Launcher",
        // The Arch mark, which is what the reference shows. Drawn, not typed:
        // a capital A would read as somebody's wordmark, not a distribution.
        icon: '\u{e732}',
        ghost: false,
        // Its own plate. This is the first separator in the reference's
        // `barSeps`, and it is what separates the launcher from the workspaces.
        gloss: "",
        side: Side::Left,
        popup: Some(PopupDef { id: "launcher", pages: &[] }),
    },
    WidgetDef {
        key: "workspaces",
        aliases: &["workspace", "ws"],
        label: "Workspaces",
        icon: '\0',
        ghost: false,
        gloss: "",
        side: Side::Left,
        // Drawn by the bar itself rather than by `render_widget`, but it is a
        // widget like any other for ordering and for the region split.
        popup: None,
    },
    WidgetDef {
        key: "notification",
        aliases: &["notifications", "bell"],
        label: "Notifications",
        icon: '\u{f0f3}',
        ghost: true,
        gloss: "",
        side: Side::Left,
        popup: Some(PopupDef { id: "notification", pages: &[] }),
    },
    WidgetDef {
        key: "cpu",
        aliases: &["processor"],
        label: "CPU",
        icon: '\u{f4bc}',
        ghost: true,
        gloss: "",
        side: Side::Left,
        popup: None,
    },
    WidgetDef {
        key: "volume",
        aliases: &["sound", "audio"],
        label: "Volume",
        icon: '\u{f028}',
        ghost: false,
        gloss: "",
        side: Side::Left,
        popup: Some(PopupDef { id: "volume", pages: &[] }),
    },

    // ---- middle ----
    WidgetDef {
        key: "weather",
        aliases: &[],
        label: "Weather",
        icon: '\u{e33d}',
        ghost: true,
        gloss: "",
        side: Side::Middle,
        popup: None,
    },
    WidgetDef {
        key: "clock",
        aliases: &[],
        label: "Clock",
        icon: '\0',
        ghost: false,
        // The clock group is one plate and one click target: weather, time and
        // date are one fact, not three.
        gloss: "",
        side: Side::Middle,
        popup: Some(PopupDef { id: "clock", pages: &[] }),
    },

    // ---- right ----
    WidgetDef {
        key: "media",
        aliases: &["mpris", "music"],
        label: "Media",
        icon: '\u{f04b}',
        ghost: true,
        gloss: "",
        side: Side::Right,
        popup: None,
    },
    WidgetDef {
        key: "network",
        aliases: &["wifi"],
        label: "Network",
        icon: '\u{f1eb}',
        ghost: false,
        gloss: "",
        side: Side::Right,
        popup: Some(PopupDef { id: "network", pages: &[] }),
    },
    WidgetDef {
        key: "battery",
        aliases: &["power"],
        label: "Battery",
        // Drawn as a cell with a proportional fill, not this glyph — but the
        // codepoint is the fallback for a machine that has a battery and no
        // drawable cell.
        icon: '\u{f240}',
        ghost: false,
        gloss: "",
        side: Side::Right,
        popup: Some(PopupDef { id: "battery", pages: &[] }),
    },
    WidgetDef {
        key: "brightness",
        aliases: &["backlight"],
        label: "Brightness",
        icon: '\u{f185}',
        ghost: true,
        gloss: "",
        side: Side::Right,
        popup: None,
    },
    WidgetDef {
        key: "bluetooth",
        aliases: &["bt"],
        label: "Bluetooth",
        icon: '\u{f293}',
        ghost: true,
        gloss: "",
        side: Side::Right,
        popup: Some(PopupDef { id: "bluetooth", pages: &[] }),
    },
    WidgetDef {
        key: "layout",
        aliases: &["keyboard"],
        label: "Keyboard layout",
        icon: '\u{f11c}',
        ghost: true,
        gloss: "",
        side: Side::Right,
        popup: None,
    },
];

/// Look a widget up by any name that resolves to it.
pub fn by_key(name: &str) -> Option<&'static WidgetDef> {
    let name = name.trim().to_ascii_lowercase();
    REGISTRY
        .iter()
        .find(|w| w.key == name || w.aliases.contains(&name.as_str()))
}

/// A name that was configured but is not a widget.
///
/// Returned rather than swallowed so the bar can log it: a typo in
/// `widgets = ["cloock"]` should say so once, at startup, not render nothing and
/// leave the user guessing.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownWidget(pub String);

/// Turn the config list into widgets, dropping duplicates and unknown names.
///
/// Duplicates are dropped because two clocks side by side is never what anyone
/// typed, and rendering both is a bug with no useful interpretation.
pub fn resolve(names: &[String]) -> (Vec<&'static WidgetDef>, Vec<UnknownWidget>) {
    let mut widgets: Vec<&'static WidgetDef> = Vec::new();
    let mut unknown = Vec::new();

    for name in names {
        match by_key(name) {
            Some(w) if widgets.iter().any(|seen| seen.key == w.key) => {}
            Some(w) => widgets.push(w),
            None => unknown.push(UnknownWidget(name.clone())),
        }
    }

    (widgets, unknown)
}

/// Split into the three regions, preserving order within each.
///
/// Identity left, the clock centred, status right — because that is what each
/// is *for*. The configured order decides the order within a region, so
/// `["volume", "clock", "network"]` gives volume then network on the right and
/// the clock alone in the middle.
pub fn split_by_side(
    widgets: &[&'static WidgetDef],
) -> (Vec<&'static WidgetDef>, Vec<&'static WidgetDef>, Vec<&'static WidgetDef>) {
    let mut left = Vec::new();
    let mut middle = Vec::new();
    let mut right = Vec::new();
    for w in widgets {
        match w.side {
            Side::Left => left.push(*w),
            Side::Middle => middle.push(*w),
            Side::Right => right.push(*w),
        }
    }
    (left, middle, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_resolves_to_itself() {
        for w in REGISTRY {
            assert_eq!(by_key(w.key).map(|d| d.key), Some(w.key));
        }
    }

    #[test]
    fn aliases_resolve_to_their_widget() {
        assert_eq!(by_key("wifi").map(|w| w.key), Some("network"));
        assert_eq!(by_key("  Sound ").map(|w| w.key), Some("volume"));
    }

    #[test]
    fn an_unknown_name_is_unknown() {
        assert!(by_key("cloock").is_none());
        assert!(by_key("").is_none());
    }

    /// Keys are unique, or `by_key` returns whichever the registry happens to
    /// list first and the other becomes unreachable.
    #[test]
    fn keys_are_unique() {
        for (i, a) in REGISTRY.iter().enumerate() {
            for b in REGISTRY.iter().skip(i + 1) {
                assert_ne!(a.key, b.key, "{} appears twice", a.key);
                assert!(
                    !a.aliases.contains(&b.key),
                    "{} is an alias of {} and also a key",
                    b.key,
                    a.key
                );
            }
        }
    }

    /// A duplicate in the list is a typo, and rendering it twice is a bug with
    /// no useful reading — two clocks side by side is not what anyone meant.
    #[test]
    fn duplicates_are_dropped() {
        let names = vec!["clock".into(), "clock".into(), "wifi".into()];
        let (widgets, unknown) = resolve(&names);
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[0].key, "clock");
        assert_eq!(widgets[1].key, "network");
        assert!(unknown.is_empty());
    }

    #[test]
    fn an_unknown_name_is_reported_rather_than_dropped_silently() {
        let (widgets, unknown) = resolve(&["cloock".into()]);
        assert!(widgets.is_empty());
        assert_eq!(unknown, vec![UnknownWidget("cloock".into())]);
    }

    /// Order within a region is the configured order, not the registry's.
    ///
    /// Volume is on the left, as the reference puts it, so this is no longer a
    /// test about which region a widget lands in — only about order surviving
    /// the split, which is the part that is easy to break.
    #[test]
    fn regions_keep_configured_order() {
        let names = vec!["network".into(), "clock".into(), "volume".into()];
        let (widgets, _) = resolve(&names);
        let (left, middle, right) = split_by_side(&widgets);
        assert_eq!(left.iter().map(|w| w.key).collect::<Vec<_>>(), ["volume"]);
        assert_eq!(middle.iter().map(|w| w.key).collect::<Vec<_>>(), ["clock"]);
        assert_eq!(right.iter().map(|w| w.key).collect::<Vec<_>>(), ["network"]);
    }

    /// Every popup id is unique, or two widgets can open the same panel and one
    /// of them silently wins the toggle.
    #[test]
    fn popup_ids_are_unique() {
        let mut ids: Vec<&str> = REGISTRY.iter().filter_map(|w| w.popup.map(|p| p.id)).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    /// A page deep-link has to name a page that exists, or `network#wifi`
    /// silently opens the panel on its default tab.
    #[test]
    fn pages_are_not_empty_strings() {
        for w in REGISTRY {
            let Some(popup) = w.popup else { continue };
            assert!(!popup.id.is_empty());
            for page in popup.pages {
                assert!(!page.trim().is_empty(), "{} has a blank page", w.key);
            }
        }
    }
}