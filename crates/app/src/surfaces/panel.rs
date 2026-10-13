//! Panel content: the sidebars and the dock.
//!
//! The surfaces themselves are owned by `surfaces::Surfaces`. This file is what
//! goes inside them.
//!
//! Icons resolve through the freedesktop icon theme (`services::icons`), which
//! walks `$XDG_DATA_DIRS/icons/<theme>`, follows `Inherits`, and falls back to
//! `hicolor` — the same lookup a GTK client does.

use std::cell::Cell;
use std::collections::HashMap;
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
use services::compositor::Action;
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
    /// Running apps that aren't pinned — populated dynamically from the
    /// compositor's window list so the dock shows both favourites and
    /// currently-running programmes (ryoku behaviour).
    running_items: Vec<DockItem>,
    hovered: Option<usize>,
    mode: DockMode,
    enabled: bool,
    trigger_px: f32,
    demand: Rc<DockDemand>,
    shown: bool,
    /// Aligned with `items` + `running_items`. Recomputed on every compositor
    /// change, never per frame.
    runs: Vec<Run>,
}

/// How often the dock re-reads the pointer while autohide is on.
pub const DOCK_POLL_MS: u64 = 50;

/// How far an icon with no windows is faded back.
///
/// Ryoku desaturates and tints an idle icon toward bone, which needs a shader
/// effect. This fork has no public custom shader API, so the honest available
/// lever is opacity: an icon with nothing open recedes, a running one is at
/// full strength. It carries the same meaning and costs nothing to draw.
const IDLE_ICON: f32 = 0.42;

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
        let n = (self.items.len() + self.running_items.len()) as f32;
        let width = n * TILE + (n - 1.0).max(0.0) * TILE_GAP;
        let x = (f32::from(w) - width) / 2.0;
        gpui::Bounds::new(gpui::point(px(x), px(0.0)), gpui::size(px(width), h))
    }
}

pub struct DockItem {
    pub app: AppEntry,
    pub pinned: bool,
}

impl PartialEq for DockItem {
    fn eq(&self, other: &Self) -> bool {
        self.app.id == other.app.id && self.pinned == other.pinned
    }
}

/// What the compositor says about one tile.
#[derive(Default, Clone, Copy, PartialEq)]
struct Run {
    /// Open windows belonging to this app.
    count: usize,
    /// One of them is focused. Distinct from `count`: "running" and "active"
    /// are different questions, and a dock that answers both with one mark
    /// cannot say either.
    active: bool,
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
            running_items: Vec::new(),
            hovered: None,
            mode: dock.mode,
            enabled: dock.enabled,
            trigger_px: dock.trigger_px,
            demand: Rc::new(DockDemand::default()),
            shown,
            runs: Vec::new(),
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
    /// Recompute which tiles are running and which is active.
    ///
    /// One pass over the compositor's windows, grouped by the app each one
    /// resolves to. Grouping by the *index* rather than by guessing a class per
    /// tile means the two directions cannot disagree: the same `by_class` answer
    /// decides both what a window counts towards and what a tile claims.
    fn refresh_runs(&mut self, cx: &mut Context<Self>) {
        let state = cx.global::<services::AppState>();

        // Track runs for pinned items.
        let mut runs: Vec<Run> = self
            .items
            .iter()
            .map(|_| Run::default())
            .collect();
        let pinned_by_id: HashMap<&str, usize> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.app.id.as_str(), i))
            .collect();

        // Track running (non-pinned) apps.
        let mut running_by_id: HashMap<String, usize> = HashMap::new();
        let mut running_items: Vec<DockItem> = Vec::new();

        for client in state.compositor.clients().iter() {
            let Some(app) = state.apps.by_class(&client.class) else {
                continue;
            };
            let app_id = app.id.clone();

            // Pinned app — update its run.
            if let Some(&slot) = pinned_by_id.get(app_id.as_str()) {
                runs[slot].count += 1;
                runs[slot].active |= client.active;
                continue;
            }

            // Non-pinned running app — add to running list if new.
            let slot = *running_by_id
                .entry(app_id.clone())
                .or_insert_with(|| {
                    let idx = running_items.len();
                    running_items.push(DockItem { app, pinned: false });
                    idx
                });
            // Ensure runs vector has space for running items too.
            if runs.len() <= self.items.len() + slot {
                runs.resize(self.items.len() + running_items.len(), Run::default());
            }
            runs[self.items.len() + slot].count += 1;
            runs[self.items.len() + slot].active |= client.active;
        }

        // Update running_items if changed.
        if self.running_items != running_items {
            self.running_items = running_items;
        }

        if self.runs != runs {
            self.runs = runs;
            cx.notify();
        }
    }

    /// What a click does, following Ryoku's `activate`.
    ///
    /// No windows → launch, because that is the only way to get the app up.
    /// Already focused → the next window of that app, which is what a dock
    /// click has always meant and is the reason a dock is faster than a window
    /// switcher. Otherwise → focus one, preferring the active workspace so a
    /// click does not drag you sideways across desktops.
    ///
    /// Launching unconditionally instead — which is what this did — spawns a
    /// second copy of an app you already have open, every time.
    fn activate(&mut self, id: &str, cx: &mut Context<Self>) {
        let state = cx.global::<services::AppState>();

        let windows: Vec<services::compositor::Client> = state
            .compositor
            .clients()
            .iter()
            .filter(|c| state.apps.by_class(&c.class).is_some_and(|a| a.id == id))
            .cloned()
            .collect();

        let target = if windows.is_empty() {
            None
        } else {
            let workspace = state.compositor.active().name.clone();
            Some(match windows.iter().position(|c| c.active) {
                // Focused already: advance to the next window, wrapping.
                Some(i) => &windows[(i + 1) % windows.len()],
                None => windows
                    .iter()
                    .find(|c| c.workspace == workspace)
                    .unwrap_or(&windows[0]),
            })
        };

        match target {
            Some(client) => {
                if let Err(e) = state.compositor.act(Action::FocusWindow {
                    address: client.address.clone(),
                    monitor: client.monitor,
                }) {
                    tracing::warn!(address = %client.address, "could not focus window: {e:#}");
                }
            }
            None => {
                if let Some(entry) = state.apps.get(id) {
                    super::background::launch_public(&entry);
                }
            }
        }
        self.refresh(cx);
    }

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

        self.refresh_runs(cx);
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

}

impl Focusable for Dock {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();

        // Where the pointer is, from the compositor — not from this window.
        //
        // `window.mouse_position()` is the obvious thing and it does not work
        // here: it is only ever written by a pointer event this surface
        // receives, and Hyprland does not reliably send one to this surface. It
        // measured frozen at x=1212, y=-498 while the cursor sat elsewhere,
        // whatever the input region was. The compositor knows where the pointer
        // is regardless of who is asking, so it is asked.
        //
        // The answer is screen coordinates, and this surface is a band at the
        // bottom of the screen, so "on the dock" is a screen-space rectangle:
        // the tiles' footprint, or the trigger strip at the very bottom. That is
        // Ryoku's mask — band rect ∪ peek strip — expressed in the space the
        // pointer is actually reported in.
        let state = cx.global::<services::AppState>();
        let (out_w, out_h) = state.compositor.output_size();
        let tiles = self.tiles_bounds(px(out_w), px(DOCK_HEIGHT));
        let pointer = state.compositor.cursor();
        let inside = pointer.is_some_and(|(x, y)| {
            y >= out_h - self.trigger_px()
                || (y >= out_h - DOCK_HEIGHT
                    && x >= f32::from(tiles.origin.x)
                    && x <= f32::from(tiles.right()))
        });

        if !inside && self.hovered.is_some() {
            self.hovered = None;
        }
        self.set_pointer(inside || self.hovered.is_some(), cx);

        // Input region: only the tile row accepts clicks so icons work but
        // clicks between/around them fall through to windows below.
        let tiles = self.tiles_bounds(px(out_w), px(DOCK_HEIGHT));
        window.set_input_region(Some(&[tiles]));

        let t = crate::theme::current(cx);

        // Combine pinned + running items for rendering.
        let all_items: Vec<&DockItem> = self
            .items
            .iter()
            .chain(self.running_items.iter())
            .collect();

        let buttons = all_items.iter().enumerate().map(|(i, item)| {
            let app = item.app.clone();
            let run = self.runs.get(i).copied().unwrap_or_default();
            let is_hovered = self.hovered == Some(i);

            // The plate. Every dock icon sits on its own hairline-bordered
            // square rather than floating bare on the band: the reference
            // dock's icons are tiles, and a bare icon on a strip reads as a
            // list of files. The border is what makes it a tile.
            //
            // Square, not rounded. Ryoku's `seal` is the style this dock is
            // built from and its plate has radius 0; the other four styles
            // round theirs. Rounding here is decoration borrowed from a
            // different style.
            div()
                .id(("lume-dock-item", hash_of(&app.id)))
                .size(px(TILE))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(if run.active { t.line_strong() } else { t.line_soft() })
                .cursor_pointer()
                // Hover is a fill, never a border change: swapping the border
                // colour would make the tile appear to change size by a pixel.
                .bg(if is_hovered { t.control_hover() } else { t.ink_a(0.0) })
                .hover(|h| h.bg(t.control_hover()))
                .child(
                    div()
                        .opacity(if run.count == 0 { IDLE_ICON } else { 1.0 })
                        .child(icon(&app.icon, &app.name, 40.0)),
                )
                .on_click({
                    let this = this.clone();
                    let id = app.id.clone();
                    move |_, _w, cx| {
                        this.update(cx, |me, cx| me.activate(&id, cx));
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
                // The active mark: a bone L-bracket in the corner.
                //
                // Bone, not the accent — the reference draws it in the inverted
                // ink, and an accent bracket says "this app has something going
                // on" rather than "this is the window you are looking at".
                // Accent was wrong here.
                //
                // Two rules, not a filled corner and not an outline: a filled
                // corner reads as a tab, and a whole-square outline reads as a
                // second border competing with the plate's own.
                .child(if run.count > 0 {
                    div()
                        .absolute()
                        .left(px(4.0))
                        .top(px(4.0))
                        .size(px(9.0))
                        .border_l_2()
                        .border_t_2()
                        .border_color(t.bone_fill())
                        .into_any_element()
                } else {
                    div().into_any_element()
                })
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
