//! Panel content: the sidebars and the dock.
//!
//! The surfaces themselves are owned by `surfaces::Surfaces`. This file is what
//! goes inside them.
//!
//! Icons resolve through the freedesktop icon theme (`services::icons`), which
//! walks `$XDG_DATA_DIRS/icons/<theme>`, follows `Inherits`, and falls back to
//! `hicolor` — the same lookup a GTK client does.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, IntoElement, Render,
    StatefulInteractiveElement, Window, div,
    SharedString,
    prelude::*, px, rgb,
};
use services::apps::AppEntry;
pub use services::config::DockMode;

use super::{DOCK_HEIGHT, SIDEBAR_WIDTH};
use services::intent::Side;


/// Floor on how long the dock may stay wrong if the event stream drops an
/// event. Matches the compositor service's own poll interval, because the two
/// recover from the same failure.
pub const DOCK_FLOOR: Duration = Duration::from_secs(5);

/// What the dock decides for itself.
///
/// Shared, because two different things feed the decision — the compositor
/// (are there tiled windows?) and the pointer (is it at the edge?) — and the
/// surface that owns the window has to read the result. Writing it into two
/// places is how the dock ends up reserving space it is not drawing.
#[derive(Default)]
pub struct DockDemand {
    /// Pointer is inside the dock or its trigger strip.
    pub pointer: Cell<bool>,
    /// The active workspace has no tiled windows.
    pub empty_workspace: Cell<bool>,
}

/// An icon resolved against the user's theme, with a themed fallback.
fn icon(name: &str, fallback_label: &str, size: f32) -> AnyElement {
    let size = px(size);

    if let Some(path) = services::icons::resolve(name) {
        // A PathBuf, not a String. `impl From<String> for ImageSource` builds
        // `Resource::Embedded`, which is an asset id looked up through the
        // AssetSource — a filesystem path passed as a string is silently an
        // asset that does not exist, and you get a blank rectangle with no
        // error. `From<PathBuf>` is what yields `Resource::Path`.
        return gpui::img(path).size(size).into_any_element();
    }

    // Not found. A neutral tile is better than a broken image. The initial
    // comes from the display name: an unresolvable Icon= key says nothing
    // about the app, and wine writes keys like `0CC0_game.0` that no icon
    // theme will ever have.
    let initial = fallback_label
        .trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .unwrap_or('?')
        .to_uppercase()
        .collect::<String>();

    div()
        .size(size)
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::hsla(0.60, 0.20, 0.45, 0.5))
        .text_color(rgb(0xf0f0f0))
        .child(initial)
        .into_any_element()
}

// ---------------------------------------------------------------- dock

pub struct Dock {
    focus: FocusHandle,
    items: Vec<DockItem>,
    hovered: Option<usize>,
    /// Id of the app whose window is focused, if it is a dock item.
    focused_app: Option<String>,
    mode: DockMode,
    enabled: bool,
    trigger_px: f32,
    demand: Rc<DockDemand>,
    shown: bool,
}

pub struct DockItem {
    pub app: AppEntry,
    pub pinned: bool,
}

impl Dock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.global::<services::AppState>().clone();
        let index = state.apps.all();

        // Pinned apps come from config. A pinned entry with no matching
        // .desktop file is skipped rather than shown as a broken tile, and the
        // skip is logged — an empty dock with no explanation reads as broken.
        let cfg = state.config.get();
        let items: Vec<DockItem> = cfg
            .dock
            .pinned
            .iter()
            .filter_map(|id| {
                let hit = index.iter().find(|a| &a.id == id).cloned();
                if hit.is_none() {
                    tracing::debug!(id = %id, "pinned app has no .desktop file; skipped");
                }
                hit
            })
            .map(|app| DockItem { app, pinned: true })
            .collect();

        if items.is_empty() {
            tracing::info!(
                "dock is empty — pin apps in [dock] pinned = [\"footclient.desktop\", ...]"
            );
        } else {
            tracing::info!(count = items.len(), "dock populated");
        }

        let dock = cfg.dock.clone();
        let shown = dock.enabled && dock.mode == DockMode::AlwaysVisible;

        Self {
            focus: cx.focus_handle(),
            items,
            hovered: None,
            focused_app: None,
            mode: dock.mode,
            enabled: dock.enabled,
            trigger_px: dock.trigger_px,
            demand: Rc::new(DockDemand::default()),
            shown,
        }
    }

    pub fn demand(&self) -> Rc<DockDemand> {
        self.demand.clone()
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    pub fn mode(&self) -> DockMode {
        self.mode
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Should the dock reserve space when shown?
    ///
    /// Autohide never does: it is an overlay that happens to be on screen, and
    /// reserving would push windows up for a bar the user chose to hide.
    pub fn reserves(&self) -> bool {
        self.mode != DockMode::Autohide
    }

    pub fn zone(&self) -> f32 {
        if self.shown && self.reserves() {
            DOCK_HEIGHT
        } else {
            0.0
        }
    }

    /// Re-read the compositor and recompute visibility. Cheap: the expensive
    /// part is skipped unless the mode actually consults it.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let state = cx.global::<services::AppState>();
        if self.mode == DockMode::Dodge {
            let empty = !state.compositor.has_tiled_windows();
            if self.demand.empty_workspace.get() != empty {
                self.demand.empty_workspace.set(empty);
            }
        }

        let shown = self.enabled
            && match self.mode {
                DockMode::AlwaysVisible => true,
                DockMode::Autohide => self.demand.pointer.get(),
                DockMode::Dodge => self.demand.empty_workspace.get(),
            };

        if shown != self.shown {
            self.shown = shown;
            tracing::debug!(?self.mode, shown, "dock visibility");
            cx.notify();
        }
    }

    fn set_pointer(&mut self, inside: bool, cx: &mut Context<Self>) {
        if self.demand.pointer.get() != inside {
            self.demand.pointer.set(inside);
            self.refresh(cx);
        }
    }

    /// Distance from the bottom edge that reveals an autohiding dock. Also the
    /// height of the trigger strip we keep in the input region, because a
    /// surface with an empty region gets no pointer events at all.
    pub fn trigger_px(&self) -> f32 {
        self.trigger_px
    }

    pub fn set_focused_app(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.focused_app != id {
            self.focused_app = id;
            cx.notify();
        }
    }
}

impl Focusable for Dock {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();

        // The pointer is polled from the window rather than tracked through
        // hover events: while the dock is hidden its input region is a thin
        // trigger strip, so hover events on the icons never arrive. One read
        // per render is cheaper than maintaining that state machine.
        let y = window.mouse_position().y;
        let in_trigger = y >= window.viewport_size().height - px(self.trigger_px());
        self.set_pointer(in_trigger || self.hovered.is_some(), cx);

        let accent = gpui::hsla(0.60, 0.85, 0.68, 1.0);

        let buttons = self.items.iter().enumerate().map(|(i, item)| {
            let app = item.app.clone();
            let is_focused = self.focused_app.as_deref() == Some(app.id.as_str());
            let is_hovered = self.hovered == Some(i);

            div()
                .id(("lume-dock-item", hash_of(&app.id)))
                // Lifts on hover. A border and a size change are style-only —
                // no frame loop, nothing animating on its own.
                .size(px(48.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.0))
                .cursor_pointer()
                .bg(if is_hovered {
                    gpui::hsla(0.0, 0.0, 1.0, 0.12)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .child(icon(&app.icon, &app.name, 40.0))
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        let id = app.id.clone();
                        this.update(cx, |me, cx| {
                            if let Some(entry) =
                                cx.global::<services::AppState>().apps.get(&id)
                            {
                                super::background::launch_public(&entry);
                            }
                            cx.notify();
                        });
                    }
                })
                .on_hover({
                    let this = this.clone();
                    move |_e, _w, cx| this.update(cx, |me, _cx| me.hovered = Some(i))
                })
                // A running-indicator bar under the item. Derived state, so
                // it repaints only when the set changes — never animated.
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .h(px(3.0))
                        .w(px(20.0))
                        .rounded_full()
                        .bg(if is_focused { accent } else { gpui::hsla(0.0, 0.0, 0.0, 0.0) }),
                )
        });

        div()
            .id("lume-dock")
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .px_3()
            .bg(gpui::hsla(0.10, 0.05, 0.09, 0.86))
            .border_1()
            .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
            .child(div().h_full().min_w(px(8.0)))
            .children(buttons)
    }
}

fn hash_of(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_for_the_same_input() {
        assert_eq!(hash_of("firefox_firefox.desktop"), hash_of("firefox_firefox.desktop"));
        assert_ne!(hash_of("a.desktop"), hash_of("b.desktop"));
    }
}
// ---------------------------------------------------------------- sidebars

/// A sidebar.
///
/// Same ownership rule as the dock: the view reads the config and pushes its
/// own zone and input region, diffed, from `render`. A registry of window
/// handles would be a second owner, and two owners of an exclusive zone is how
/// the bar once ended up reserving nothing.
pub struct Sidebar {
    focus: FocusHandle,
    side: Side,
    open: bool,
}

impl Sidebar {
    pub fn new(side: Side, cx: &mut Context<Self>) -> Self {
        let cfg = cx.global::<services::AppState>().config.get();
        let open = match side {
            Side::Left => cfg.panels.left,
            Side::Right => cfg.panels.right,
        };
        tracing::info!(side = side.as_str(), open, "sidebar");
        Self { focus: cx.focus_handle(), side, open }
    }

    pub fn open(&self) -> bool {
        self.open
    }

    pub fn side(&self) -> Side {
        self.side
    }

    pub fn zone(&self) -> f32 {
        if self.open {
            SIDEBAR_WIDTH
        } else {
            0.0
        }
    }

    /// Apply an intent. The caller has already written the config; this only
    /// repaints.
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.open != open {
            self.open = open;
            tracing::info!(side = self.side.as_str(), open, "sidebar toggled");
            cx.notify();
        }
    }
}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.side {
            Side::Left => "Left sidebar",
            Side::Right => "Right sidebar",
        };

        div()
            .id(("lume-sidebar", self.side as usize))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(gpui::hsla(0.10, 0.05, 0.09, 0.92))
            .border_1()
            .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
            .child(SharedString::from(title))
            .child(
                gpui::div()
                    .text_size(px(11.0))
                    .text_color(rgb(0x808080))
                    .child(SharedString::from("stage 5")),
            )
            .into_any_element()
    }
}
