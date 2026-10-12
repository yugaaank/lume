//! The background surface: desktop icons, wallpaper control, and the picker.
//!
//! Two rules govern everything here.
//!
//! **1. It is almost always empty.** The wallpaper is drawn by `awww`, so this
//! surface paints only icons. Its dirty area is a few hundred pixels rather
//! than full-screen, which is what makes hover cheap.
//!
//! **2. Stay out of the pointer-motion path.** The tempting implementation is
//! `.on_hover(...)`, which fires on every pointer move and would repaint a
//! full-screen surface 60 times a second. Instead we compare the hit result to
//! the previous one and notify only when it *changes*.
//!
//! Input regions are the other half: this surface covers the whole desktop, so
//! without an explicit region every click aimed at a terminal would be eaten.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Pixels, Point, Render, Size,
    AnyElement, StatefulInteractiveElement, Window, div, point,
    prelude::*, px, rgb,
};
use gpui::SharedString;

use services::{AppState, apps};

use super::picker::Picker;

// ---------------------------------------------------------------- grid

#[derive(Clone, Copy)]
pub struct Grid {
    pub cell: f32,
    pub padding: f32,
    pub origin: Point<Pixels>,
    pub columns: usize,
}

impl Grid {
    pub fn for_display(size: Size<Pixels>) -> Self {
        let cell = 96.0;
        let padding = 16.0;
        // Leave the top clear so icons never sit under the bar.
        let origin = point(px(padding), px(64.0));
        let columns =
            (((f32::from(size.width) - padding) / (cell + padding)).floor() as usize).max(1);
        Self { cell, padding, origin, columns }
    }

    /// A rect is a pure function of the cell index. That is what makes layout
    /// reproducible and drag bookkeeping trivial.
    pub fn cell_origin(&self, cell: usize) -> (f32, f32) {
        let cols = self.columns.max(1);
        let col = cell % cols;
        let row = cell / cols;
        (
            f32::from(self.origin.x) + self.padding + col as f32 * (self.cell + self.padding),
            f32::from(self.origin.y) + self.padding + row as f32 * (self.cell + self.padding),
        )
    }

    pub fn cell_at(&self, p: Point<Pixels>) -> Option<usize> {
        let x = f32::from(p.x) - f32::from(self.origin.x) - self.padding;
        let y = f32::from(p.y) - f32::from(self.origin.y) - self.padding;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let step = self.cell + self.padding;
        Some(((y / step).floor() as usize) * self.columns.max(1) + (x / step).floor() as usize)
    }

    pub fn rect(&self, cell: usize) -> Bounds<Pixels> {
        let (x, y) = self.cell_origin(cell);
        Bounds { origin: point(px(x), px(y)), size: gpui::size(px(self.cell), px(self.cell)) }
    }
}

// ---------------------------------------------------------------- launch

/// Launch an app. Parsed argv, exec'd directly. There is no shell.
pub fn launch_public(app: &apps::AppEntry) {
    launch(app)
}

fn launch(app: &apps::AppEntry) {
    let Some(argv) = app.argv(&[]) else {
        tracing::warn!(id = %app.id, "unusable Exec line");
        return;
    };
    if argv.is_empty() {
        return;
    }

    let program = argv[0].clone();
    let args = argv[1..].to_vec();

    // spawn_detached scrubs injected env vars. A child inheriting a
    // crash-handle var can re-execute the shell, which draws a second bar.
    match services::ipc::spawn_detached(&program, &args) {
        Ok(()) => tracing::info!(id = %app.id, "launched"),
        Err(e) => tracing::error!(id = %app.id, "launch failed: {e}"),
    }
}

/// Hand the wallpaper to `awww`. One spawn, no render loop, no texture.
pub fn set_wallpaper(path: &std::path::Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("awww")
        .arg("img")
        .arg(path)
        .status()?;
    if !status.success() {
        anyhow::bail!("awww img failed with {status}");
    }
    tracing::info!(path = %path.display(), "wallpaper set via awww");
    Ok(())
}

// ---------------------------------------------------------------- state

pub struct Desktop {
    focus: FocusHandle,
    grid: Grid,
    /// App id -> cell. Sorted by cell so layout is stable and rendering order
    /// is deterministic.
    placed: Vec<(String, usize)>,
    apps: Vec<apps::AppEntry>,
    hovered: Option<usize>,
    selected: Option<String>,
    /// Icons only. Empty means the whole surface is click-through.
    region: Vec<Bounds<Pixels>>,
    picker: Option<Entity<Picker>>,
    menu_at: Option<Point<Pixels>>,
}

impl Desktop {
    pub fn new(display: Size<Pixels>, cx: &mut Context<Self>) -> Self {
        let state = cx.global::<AppState>().clone();
        let grid = Grid::for_display(display);

        // Only PINNED apps appear on the desktop. Listing every installed
        // application was the previous behaviour and it was wrong twice over:
        // nobody wants 200 icons on their desktop, and 200 rects in one
        // wl_region is a per-commit cost. Measured: +20M private dirty and
        // idle CPU 0.10% -> 0.35%.
        //
        // The set comes from config (`desktop.pinned`); the index supplies
        // the metadata for whatever is named there.
        let index = state.apps.all();
        let pinned = &state.config.get().desktop.pinned;
        let apps: Vec<apps::AppEntry> = pinned
            .iter()
            .filter_map(|id| index.iter().find(|a| &a.id == id).cloned())
            .collect();

        if apps.is_empty() {
            tracing::info!(
                "no pinned desktop icons; pin some in [desktop] pinned = [...]"
            );
        }
        let saved = state.config.icons();

        // Reconcile stored positions with the current app set. A stored cell
        // that two apps both claim is a tie; first come, first served.
        let mut used: Vec<usize> = Vec::new();
        let mut placed: Vec<(String, usize)> = Vec::new();

        for app in &apps {
            let Some(cell) = saved.cells.get(&app.id).copied() else {
                continue;
            };
            if !used.contains(&cell) {
                used.push(cell);
                placed.push((app.id.clone(), cell));
            }
        }
        for app in &apps {
            if placed.iter().any(|(id, _)| id == &app.id) {
                continue;
            }
            let cell = (0..).find(|c| !used.contains(c)).unwrap_or(placed.len());
            used.push(cell);
            placed.push((app.id.clone(), cell));
        }
        placed.sort_by_key(|(_, c)| *c);

        let region = placed.iter().map(|(_, c)| grid.rect(*c)).collect();

        Self {
            focus: cx.focus_handle(),
            grid,
            placed,
            apps,
            hovered: None,
            selected: None,
            region,
            picker: None,
            menu_at: None,
        }
    }

    /// The rectangles that accept input. Empty means invisible to the pointer.
    ///
    /// Recompute only when the layout changes structurally — this becomes a
    /// surface commit, and doing it per frame spins the compositor.
    pub fn input_regions(&self) -> Vec<Bounds<Pixels>> {
        self.region.clone()
    }

    fn entry(&self, id: &str) -> Option<&apps::AppEntry> {
        self.apps.iter().find(|a| a.id == id)
    }

    fn cell_of(&self, id: &str) -> Option<usize> {
        self.placed.iter().find(|(x, _)| x == id).map(|(_, c)| *c)
    }

    fn refresh_region(&mut self) {
        self.region = self.placed.iter().map(|(_, c)| self.grid.rect(*c)).collect();
    }

    fn set_picker(&mut self, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.picker = Some(cx.new(|cx| Picker::new(cx)));
    }

    fn close_picker(&mut self) {
        self.picker = None;
    }
}

// ---------------------------------------------------------------- render

impl Focusable for Desktop {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Deliberately NOT .on_hover. This surface is full-screen; a per-move
        // repaint is the expensive mistake this file exists to avoid. Compare
        // the hit result and notify only when it CHANGES.
        let this = cx.entity();
        let this_move = this.clone();

        let mut layer = div()
            .id("lume-desktop")
            .size_full()
            .on_mouse_move(move |event, _w, cx| {
                let p = event.position;
                this_move.update(cx, |me, cx| {
                    let hit = me.grid.cell_at(p);
                    if hit != me.hovered {
                        me.hovered = hit;
                        cx.notify();
                    }
                });
            });

        // --- icons -------------------------------------------------------
        layer = layer.children(self.placed.iter().map(|(id, cell)| {
            let cell = *cell;
            let Some(app) = self.entry(id).cloned() else {
                return div().id(("lume-icon-missing", hash_of(id)));
            };

            let is_hovered = self.hovered == Some(cell);
            let is_selected = self.selected.as_deref() == Some(app.id.as_str());
            let (x, y) = self.grid.cell_origin(cell);
            let grid = self.grid;

            div()
                .id(("lume-icon", hash_of(&app.id)))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(grid.cell))
                .h(px(grid.cell))
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .bg(if is_selected {
                    gpui::hsla(0.0, 0.0, 1.0, 0.14)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .rounded(px(8.0))
                .child(icon_tile(&app.icon, px(48.0)))
                .child(label(&app.name, grid.cell))
                .on_click({
                    let this = this.clone();
                    let id = app.id.clone();
                    move |_, _w, cx| {
                        // Single click selects; launch is a separate affordance
                        // rather than a surprise.
                        this.update(cx, |me, cx| {
                            me.selected = Some(id.clone());
                            cx.notify();
                        });
                    }
                })
        }));

        // --- context menu -------------------------------------------------
        if let Some(at) = self.menu_at {
            layer = layer.child(self_menu(at, window, cx, this.clone()));
        }


        // --- picker -------------------------------------------------------
        if let Some(picker) = &self.picker {
            layer = layer.child(picker.clone());
        }

        layer
    }
}

fn self_menu(
    at: Point<Pixels>,
    _window: &mut Window,
    _cx: &mut Context<Desktop>,
    this: Entity<Desktop>,
) -> AnyElement {
    // Positioned inside the screen: a click near the bottom-right would
    // otherwise put half the menu off-screen.
    let w = 180.0;
    let h = 240.0;
    let size = _window.viewport_size();
    let x = f32::from(at.x).min(f32::from(size.width) - w - 8.0);
    let y = f32::from(at.y).min(f32::from(size.height) - h - 8.0);

    let item = |label: &'static str| {
        div()
            .id(("lume-menu-item", hash_of(label)))
            .h(px(28.0))
            .px_3()
            .flex()
            .items_center()
            .text_size(px(12.0))
            .text_color(rgb(0xe0e0e0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.10)))
            .child(label)
    };

    div()
        .id("lume-desktop-menu")
        .absolute()
        .left(px(x.max(0.0)))
        .top(px(y.max(0.0)))
        .w(px(w))
        .h(px(h))
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        // Frosted. This is a real backdrop blur, and it is a per-frame GPU
        // cost — measure it, do not assume it is free.
        .backdrop_blur(px(16.0))
        .bg(gpui::hsla(0.10, 0.05, 0.08, 0.80))
        .border_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.10))
        .rounded(px(10.0))
        .shadow_lg()
        .child(item("New folder"))
        .child(item("Change wallpaper"))
        .child(item("Open terminal here"))
        .child(item("Paste"))
        .child(item("Display settings"))
        // Clicking the backdrop closes it. Also handles Esc via the surface's
        // key handler in the registry.
        .child(
            div()
                .id("lume-desktop-menu-scrim")
                .absolute()
                .left_0()
                .top_0()
                .size(px(1.0))
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        this.update(cx, |me, cx| {
                            me.menu_at = None;
                            cx.notify();
                        });
                    }
                }),
        )
        .into_any_element()
}

/// ElementId accepts only `(&'static str, usize | u64 | EntityId)` tuples.
/// Desktop-file ids are runtime strings, so hash them once.
pub fn hash_path(p: &std::path::Path) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish() as usize
}

fn hash_of(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

/// A label readable on any wallpaper.
///
/// No `text_shadow` in this fork, so: a dark copy offset by 1px, then the real
/// copy on top. Two draw calls, no layout change, no shader, and it reads on a
/// light wallpaper as well as a dark one. Cheaper and more portable than a
/// plate behind every label.
fn label(text: &str, width: f32) -> gpui::Div {
    div()
        .relative()
        .w(px(width))
        .child(
            div()
                .id(("lume-icon-label-shadow", hash_of(text)))
                .absolute()
                .left(px(0.0))
                .top(px(1.0))
                .max_w(px(width))
                .truncate()
                .text_size(px(11.0))
                .text_color(gpui::hsla(0.0, 0.0, 0.0, 0.85))
                .child(SharedString::from(text)),
        )
        .child(
            div()
                .id(("lume-icon-label", hash_of(text)))
                .max_w(px(width))
                .truncate()
                .text_size(px(11.0))
                .text_color(rgb(0xf0f0f0))
                .child(SharedString::from(text)),
        )
}

/// An app icon.
///
/// There is no `.icon(name)` in this fork — icons are `img(ImageSource)`, which
/// takes a *resource location*, not a freedesktop icon-theme name. Resolving
/// `firefox` against the user's hicolor/Papirus/etc. means implementing
/// `AssetSource` with a theme lookup, which is its own piece of work.
///
/// Until that exists: an absolute path renders, and a bare theme name gets a
/// neutral tile. Better an obviously-generic icon than a broken one.
fn icon_tile(name: &str, size: Pixels) -> AnyElement {
    let name = name.trim();

    // Resolve against the user's icon theme the way a GTK client does: search
    // $XDG_DATA_DIRS/icons/<theme>, follow Inherits, fall back to hicolor.
    if let Some(path) = services::icons::resolve(name) {
        // PathBuf, not String: see the note in panel::icon. A string path
        // becomes an Embedded asset id, which renders as nothing at all.
        return gpui::img(path).into_any_element();
    }

    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();

    div()
        .size(size)
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::hsla(0.60, 0.25, 0.45, 0.55))
        .text_size(px(22.0))
        .text_color(rgb(0xf0f0f0))
        .child(SharedString::from(initial))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: usize) -> Grid {
        Grid { cell: 96.0, padding: 16.0, origin: point(px(16.0), px(64.0)), columns: cols }
    }

    #[test]
    fn cell_origin_wraps_rows() {
        let g = grid(3);
        assert_eq!(g.cell_origin(0).0, 32.0);
        assert_eq!(g.cell_origin(2).0, 32.0 + 2.0 * 112.0);
        // Cell 3 wraps to the next row, same x as cell 0.
        assert_eq!(g.cell_origin(3).0, 32.0);
        assert_eq!(g.cell_origin(3).1, 64.0 + 16.0 + 112.0);
    }

    #[test]
    fn cell_at_inverts_cell_origin() {
        let g = grid(4);
        for cell in 0..12 {
            let (x, y) = g.cell_origin(cell);
            let inside = point(px(x + 10.0), px(y + 10.0));
            assert_eq!(g.cell_at(inside), Some(cell), "cell {cell}");
        }
    }

    #[test]
    fn cell_at_rejects_above_and_left_of_the_grid() {
        let g = grid(4);
        assert_eq!(g.cell_at(point(px(0.0), px(0.0))), None);
        assert_eq!(g.cell_at(point(px(10.0), px(10.0))), None);
    }

    #[test]
    fn zero_columns_does_not_divide_by_zero() {
        let mut g = grid(0);
        g.columns = 0;
        let _ = g.cell_origin(5); // must not panic
    }
}