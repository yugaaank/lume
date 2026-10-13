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
    super::app_icon(name, fallback_label, px(size))
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

/// How often the dock re-reads the pointer while autohide is on.
pub const DOCK_POLL_MS: u64 = 50;

/// One tile's box. The surface needs these to build an input region that
/// covers the tiles and the peek strip and nothing else, so a visible dock
/// stops swallowing clicks across the width of the screen.
pub const TILE: f32 = 52.0;
/// Gap between tiles.
pub const TILE_GAP: f32 = 8.0;

impl Dock {
    /// Horizontal extent of the tile row, centred on the surface.
    pub fn tiles_bounds(
        &self,
        w: gpui::Pixels,
        h: gpui::Pixels,
    ) -> gpui::Bounds<gpui::Pixels> {
        let n = self.items.len() as f32;
        let width = n * TILE + (n - 1.0).max(0.0) * TILE_GAP;
        let x = (f32::from(w) - width) / 2.0;
        gpui::Bounds::new(gpui::point(px(x), px(0.0)), gpui::size(px(width), h))
    }
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

        /// Distance from the bottom edge that reveals an autohiding dock. Also the
    /// height of the trigger strip we keep in the input region, because a
    /// surface with an empty region gets no pointer events at all.
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

        // Where the pointer is, read once per render. The surface's timer
        // supplies the renders; this supplies the answer.
        //
        // "Inside" is geometric rather than an event: the pointer is over the
        // dock when it is anywhere in this surface, which is a band at the
        // screen edge. That has an off as well as an on, which the earlier
        // `hovered.is_some()` version did not — hover was only ever set, never
        // cleared, so the dock latched open the first time a tile was touched.
        let pos = window.mouse_position();
        let vp = window.viewport_size();
        let inside = pos.x >= px(0.0)
            && pos.y >= px(0.0)
            && pos.x < vp.width
            && pos.y < vp.height;
        if !inside && self.hovered.is_some() {
            self.hovered = None;
        }
        let in_trigger = pos.y >= vp.height - px(self.trigger_px());
        tracing::debug!(x = f32::from(pos.x), y = f32::from(pos.y), inside, "DBG pos");
        self.set_pointer(in_trigger || inside, cx);

        let t = crate::theme::current(cx);

        let buttons = self.items.iter().enumerate().map(|(i, item)| {
            let app = item.app.clone();
            let is_focused = self.focused_app.as_deref() == Some(app.id.as_str());
            let is_hovered = self.hovered == Some(i);

            // The plate. Every dock icon sits on its own hairline-bordered
            // square rather than floating bare on the band: the reference
            // dock's icons are tiles, and a bare icon on a strip reads as a
            // list of files. The border is what makes it a tile.
            div()
                .id(("lume-dock-item", hash_of(&app.id)))
                .size(px(TILE))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(services::theme::radius::CARD))
                .border_1()
                .border_color(if is_focused { t.line_strong() } else { t.line_soft() })
                .cursor_pointer()
                // Hover is a fill, never a border change: swapping the border
                // colour would make the tile appear to change size by a pixel.
                .bg(if is_hovered { t.control_hover() } else { t.ink_a(0.0) })
                .hover(|h| h.bg(t.control_hover()))
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
                    move |is_hovered, _w, cx| {
                        this.update(cx, |me, cx| {
                            // Both transitions, not just enter. `hovered` is one
                            // of the two reveal signals, so a tile that never
                            // reports its leave keeps the dock open for good.
                            if *is_hovered {
                                me.hovered = Some(i);
                            } else if me.hovered == Some(i) {
                                me.hovered = None;
                            }
                            me.refresh(cx);
                        })
                    }
                })
                // The running marker: a corner tick in the accent, drawn in the
                // tile's top-left. A bar underneath would say "this is the
                // focused window"; a corner tick says "this app is running",
                // which is the question a dock actually gets asked, and it is
                // what the reference does.
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .size(px(12.0))
                        // Only drawn when running, so this is conditional rather
                        // than an always-there transparent child.
                        .child(if is_focused {
                            div()
                                .absolute()
                                .left(px(5.0))
                                .top(px(5.0))
                                .size(px(5.0))
                                .border_l_1()
                                .border_t_1()
                                .border_color(t.accent_a(1.0))
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }),
                )
        });

        // No background at all. Each tile carries its own hairline box and the
        // wallpaper shows between and around them.
        //
        // A container fill was tried here and removed: wrapping the tiles in an
        // opaque rounded rect regroups them into a single object, and the eye
        // then reads the whole thing as one bar rather than as a set of apps.
        // The reference dock has no container either — the tiles float on the
        // desktop. So the outer div is layout only and paints nothing.
        div()
            .id("lume-dock")
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
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
