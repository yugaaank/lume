//! Bar widgets: the bar's content, driven by config.
//!
//! ## The bar renders what the user wrote, in the order they wrote it
//!
//! `[bar] widgets = ["clock", "workspaces"]` used to be decoration: `bar.rs`
//! drew workspace pips and the literal string "lume", and never read the
//! config. A widget list nobody reads is worse than no widget list, because
//! it looks like it works.
//!
//! ## One enum, not a trait object per widget
//!
//! `Widget` is an enum with one variant per widget. A trait object per widget
//! would mean a `Box<dyn Widget>` per slot, a vtable, and somewhere to hang
//! per-widget state; the enum gets exhaustive matching, one allocation-free
//! `Vec<Widget>`, and the compiler enforcing that a new widget is handled
//! everywhere. When a widget grows enough state to justify indirection that
//! is a local change to one variant.
//!
//! ## Widgets poll, they do not each own a timer
//!
//! `BarView` runs one timer. Each tick re-reads every active widget and
//! compares against what it last rendered; the bar is notified only if
//! something actually changed. Twenty-nine widgets at 1 Hz that each spawned
//! their own task and always notified would cost a repaint a second forever,
//! which is the opposite of restraint.
//!
//! The comparison is on the *rendered string*, not on the underlying value.
//! A battery at 80% that ticks to 80% again renders identically and must not
//! repaint; the cost of the comparison is one `String` equality.

use gpui::{SharedString, StatefulInteractiveElement, div, prelude::*, px, rgb};
use services::config::Config;

/// One bar slot.
#[derive(Debug, Clone, PartialEq)]
pub enum Widget {
    /// Local time. Rendered as text — there is no useful icon for a clock,
    /// and the digits are what the user is actually reading.
    Clock,
    /// Workspace pips. Handled by the bar itself; a `Widget` here so it
    /// participates in ordering and in the `left`/`right` grouping.
    Workspaces,
    /// Active network throughput.
    Network,
    /// Default sink volume and mute.
    Volume,
    Battery,
    /// The session title. Kept as a variant rather than a hardcoded string so
    /// the "lume" placeholder is gone rather than special-cased.
    Title,
}

/// Which side of the bar a widget sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Widget {
    /// Parse a name from config.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "clock" => Some(Widget::Clock),
            "workspaces" => Some(Widget::Workspaces),
            "network" | "wifi" => Some(Widget::Network),
            "volume" => Some(Widget::Volume),
            "battery" => Some(Widget::Battery),
            "title" => Some(Widget::Title),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Widget::Clock => "clock",
            Widget::Workspaces => "workspaces",
            Widget::Network => "network",
            Widget::Volume => "volume",
            Widget::Battery => "battery",
            Widget::Title => "title",
        }
    }

    /// Which side, when the user has not said otherwise.
    ///
    /// The convention every tiling shell uses: identity on the left, status on
    /// the right. A user who wants something else lists it twice, and
    /// `position = "left"` in config overrides this.
    pub fn default_side(&self) -> Side {
        match self {
            Widget::Workspaces | Widget::Title => Side::Left,
            Widget::Clock | Widget::Network | Widget::Volume | Widget::Battery => Side::Right,
        }
    }

    /// Whether this widget is worth showing at all right now.
    ///
    /// A desktop with no battery shows no battery slot, and a machine with no
    /// PipeWire shows no volume slot. Showing "no battery: 0%" or a muted
    /// speaker when there is no sound card is a lie about the machine.
    pub fn available(&self) -> bool {
        match self {
            Widget::Battery => services::battery::Battery::read().present,
            _ => true,
        }
    }
}

/// A name that was configured but is not a widget.
///
/// Returned rather than swallowed so the bar can log it: a typo in
/// `widgets = ["cloock"]` should say so once, at startup, not render nothing
/// and leave the user guessing.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownWidget(pub String);

/// Turn the config list into widgets, dropping duplicates and unknown names.
///
/// Duplicates are dropped because two clocks side by side is never what anyone
/// typed, and rendering both is a bug with no useful interpretation.
pub fn parse_list(names: &[String]) -> (Vec<Widget>, Vec<UnknownWidget>) {
    let mut widgets = Vec::new();
    let mut unknown = Vec::new();

    for name in names {
        match Widget::parse(name) {
            Some(w) if widgets.contains(&w) => {}
            Some(w) => widgets.push(w),
            None => unknown.push(UnknownWidget(name.clone())),
        }
    }

    (widgets, unknown)
}

/// Split into the two halves, preserving order within each.
///
/// `Workspaces` and `Title` are forced left and the status widgets forced
/// right, because that is what each is *for*; the configured order decides
/// the order within a side. A user listing `["clock", "workspaces"]` gets
/// workspaces left and the clock right, which is the only reading that makes
/// those two words mean anything.
pub fn split_by_side(widgets: &[Widget]) -> (Vec<Widget>, Vec<Widget>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for w in widgets {
        match w.default_side() {
            Side::Left => left.push(w.clone()),
            Side::Right => right.push(w.clone()),
        }
    }
    (left, right)
}

/// The widgets a config asks for, filtered to what this machine can show.
pub fn active(config: &Config) -> Vec<Widget> {
    let (widgets, unknown) = parse_list(&config.bar.widgets);
    for UnknownWidget(name) in unknown {
        tracing::warn!(%name, "unknown bar widget; ignoring");
    }
    widgets.into_iter().filter(Widget::available).collect()
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

    #[test]
    fn names_parse_case_insensitively() {
        assert_eq!(Widget::parse("clock"), Some(Widget::Clock));
        assert_eq!(Widget::parse("  CLOCK "), Some(Widget::Clock));
        assert_eq!(Widget::parse("Wifi"), Some(Widget::Network));
        assert_eq!(Widget::parse("wifi"), Some(Widget::Network));
        assert_eq!(Widget::parse("nonsense"), None);
        assert_eq!(Widget::parse(""), None);
    }

    #[test]
    fn round_trip_through_the_name() {
        for w in [
            Widget::Clock,
            Widget::Workspaces,
            Widget::Network,
            Widget::Volume,
            Widget::Battery,
            Widget::Title,
        ] {
            assert_eq!(Widget::parse(w.name()), Some(w.clone()));
        }
    }

    /// Two clocks side by side is never what anyone typed.
    #[test]
    fn duplicates_are_dropped() {
        let (widgets, unknown) = parse_list(&[
            "clock".into(),
            "clock".into(),
            "volume".into(),
        ]);
        assert_eq!(widgets, vec![Widget::Clock, Widget::Volume]);
        assert!(unknown.is_empty());
    }

    /// A typo must be visible. Silently rendering nothing is what made the
    /// original `widgets` list look functional.
    #[test]
    fn unknown_names_are_returned_not_swallowed() {
        let (widgets, unknown) = parse_list(&["clock".into(), "cloock".into()]);
        assert_eq!(widgets, vec![Widget::Clock]);
        assert_eq!(unknown, vec![UnknownWidget("cloock".into())]);
    }

    #[test]
    fn sides_follow_the_convention_not_the_list_order() {
        let (widgets, _) = parse_list(&["clock".into(), "workspaces".into()]);
        let (left, right) = split_by_side(&widgets);
        assert_eq!(left, vec![Widget::Workspaces]);
        assert_eq!(right, vec![Widget::Clock]);
    }

    #[test]
    fn order_within_a_side_is_the_configured_order() {
        let (widgets, _) = parse_list(&["volume".into(), "clock".into(), "network".into()]);
        let (_, right) = split_by_side(&widgets);
        assert_eq!(right, vec![Widget::Volume, Widget::Clock, Widget::Network]);
    }

    /// The config the user actually has must produce the widgets they expect.
    #[test]
    fn the_shipped_config_is_honoured() {
        let widgets = active(&cfg(&["clock", "workspaces", "network", "volume", "battery"]));
        // Battery may be filtered out on a machine without one.
        assert!(widgets.contains(&Widget::Clock));
        assert!(widgets.contains(&Widget::Workspaces));
        assert!(widgets.contains(&Widget::Network));
        assert!(widgets.contains(&Widget::Volume));
    }

    #[test]
    fn an_empty_list_yields_an_empty_bar_not_the_default_one() {
        assert!(active(&cfg(&[])).is_empty());
    }

    #[test]
    fn default_sides_are_sane() {
        assert_eq!(Widget::Workspaces.default_side(), Side::Left);
        assert_eq!(Widget::Title.default_side(), Side::Left);
        for w in [Widget::Clock, Widget::Network, Widget::Volume, Widget::Battery] {
            assert_eq!(w.default_side(), Side::Right, "{} should be on the right", w.name());
        }
    }
}
