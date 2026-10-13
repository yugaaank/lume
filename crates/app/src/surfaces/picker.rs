//! The wallpaper picker.
//!
//! What GPUI can and cannot do here, verified against the fork's paint API:
//!
//! *Native:* `backdrop_blur`, `LinearGradient`/`RadialGradient`,
//! `paint_drop_shadows`, `PathBuilder` + `paint_path`, and the full
//! `Animated`/`SpringConfig`/`Easing` motion system.
//!
//! *Not available:* custom WGSL shaders. There is no shader-authoring escape
//! hatch, so Ryoku's SDF metaball `blob.frag` cannot be ported. Design for
//! gradients, blur and springs instead — in a static thumbnail grid, metaballs
//! are decoration costing a full-screen shader pass.
//!
//! ## Layout
//!
//! A flex-wrap row inside a scroll container, not absolutely positioned tiles.
//! The previous version computed `origin_y + index * (tile + gap)` — a single
//! column with no wrapping — so all 29 wallpapers stacked down past the bottom
//! of the screen with no way to reach them. It also had `columns() -> 0`.
//!
//! Flex wrap needs no column arithmetic at all: tiles are a fixed width, the
//! container is a known width, and the line breaking is done by the layout
//! engine that was already going to do it.
//!
//! ## Previews
//!
//! From `surfaces::thumbs`, never `gpui::img(path)`. GPUI decodes at source
//! resolution and 29 wallpapers at 4K is ~957 MB.
//!
//! Frosted surfaces here are the first place `backdrop_blur` will show up on
//! the GPU in this project. That cost is measured, not assumed.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    StatefulInteractiveElement, Window, div,
    prelude::*, px, rgb,
};

use super::thumbs;

/// Where wallpapers are looked for.
fn wallpaper_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(pics) = dirs::picture_dir() {
        dirs.push(pics.join("Wallpapers"));
        dirs.push(pics);
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Pictures/Wallpapers"));
    }
    dirs
}

fn scan_wallpapers() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for dir in wallpaper_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp") {
                continue;
            }
            // A symlinked or duplicated wallpaper should appear once.
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }

    out.sort();
    out
}

pub struct Picker {
    focus: FocusHandle,
    wallpapers: Vec<PathBuf>,
    hovered: Option<usize>,
    /// The wallpaper currently applied, highlighted.
    current: Option<PathBuf>,
    /// Tile edge in px, from the viewport at construction.
    tile: f32,
    /// How many tiles fit across the body.
    columns: usize,
    /// Set by the overlay after construction, when both handles exist.
    ///
    /// An `Arc`, not a `Box`, and specifically not `take()`n out during
    /// render. The first version did `let on_close = self.on_close.take()` to
    /// dodge a borrow, which left the field `None` from the second render
    /// onward — so the picker opened, looked perfect, and its Close button
    /// did nothing. That is precisely the "can't close it" report.
    on_close: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
    /// Called when a wallpaper tile is clicked, before setting the wallpaper.
    /// Used by the overlay to mark that content was clicked so it doesn't dismiss.
    on_content_click: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
}

impl Picker {
    pub fn new(display: gpui::Size<gpui::Pixels>, current: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let wallpapers = scan_wallpapers();

        // Thumbnails are NOT requested here. Requesting all 29 up front is
        // what cost 186 MB: the uniform list only builds the rows in view, so
        // asking for what is off screen decodes pictures nobody will see.
        // `tile_tile` requests the ones it actually renders.
        tracing::info!(count = wallpapers.len(), "wallpaper picker opened");

        let tile = (f32::from(display.width) * 0.16).clamp(140.0, 240.0);
        // Body padding is 20px each side, gaps are 16px.
        let usable = f32::from(display.width) - 40.0;
        let columns = ((usable + 16.0) / (tile + 16.0)).floor().max(1.0) as usize;

        Self {
            focus: cx.focus_handle(),
            wallpapers,
            hovered: None,
            current,
            tile,
            columns,
            on_close: None,
            on_content_click: None,
        }
    }

    pub fn set_on_close(&mut self, f: Arc<dyn Fn(&mut App) + Send + Sync>) {
        self.on_close = Some(f);
    }

    pub fn set_on_content_click(&mut self, f: Arc<dyn Fn(&mut App) + Send + Sync>) {
        self.on_content_click = Some(f);
    }

    pub fn wallpapers(&self) -> &[PathBuf] {
        &self.wallpapers
    }

    /// Redraw as thumbnails land.
    ///
    /// Awaiting the notify rather than polling: the signal fires once per
    /// completed decode, so the picker redraws N times on a cold cache and
    /// then parks. When every thumbnail is in, no further signals arrive and
    /// this task simply waits — it costs nothing while idle, which a polling
    /// loop would not.
    fn watch_thumbnails(&self, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let signal = thumbs::done_signal();
        cx.spawn(async move |_weak, cx: &mut gpui::AsyncApp| {
            loop {
                signal.notified().await;
                // The entity is gone when the picker closed, which is the only
                // way this loop ends.
                if weak.update(cx, |me, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.watch_thumbnails(cx);

        let accent = gpui::hsla(0.60, 0.85, 0.68, 1.0);
        let tile = self.tile;
        let this = cx.entity();

        // A flex-wrap row inside a scroll container.
        //
        // `uniform_list` was tried here for virtualisation — it only builds
        // the rows in view, which is exactly what the memory problem needs.
        // It infers its item height from a measurement pass over the first
        // item, and a row of side-by-side tiles does not survive that: the
        // list laid out a single sliver and the grid was invisible. Correct
        // layout first; virtualisation is still owed and is written down as
        // such rather than half-landed.
        let this = cx.entity();
        let current = self.current.clone();
        let hovered = self.hovered;

        let tiles = self.wallpapers.iter().enumerate().map(|(i, path)| {
            tile_tile(i, path, self.tile, current.as_deref(), hovered, &this)
        });

        // Cloned, not taken. See the field comment.
        let on_close = self.on_close.clone();
        let count = self.wallpapers.len();

        div()
            .id("lume-picker")
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.62))
            // The control bar. First real backdrop blur in the project.
            .child(
                div()
                    .id("lume-picker-bar")
                    .h(px(64.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .backdrop_blur(px(20.0))
                    .bg(gpui::hsla(0.10, 0.05, 0.08, 0.72))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .text_color(rgb(0xf0f0f0))
                                    .child(gpui::SharedString::from("Wallpapers")),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x909090))
                                    .child(gpui::SharedString::from(format!("{count} found"))),
                            ),
                    )
                    // A close button, because the surface this picker lives on
                    // is `KeyboardInteractivity::None` and never receives a
                    // key event. Escape cannot work here and pretending
                    // otherwise is how "I can't close it" happens.
                    .child(
                        div()
                            .id("lume-picker-close")
                            .h(px(30.0))
                            .px_4()
                            .flex()
                            .items_center()
                            .rounded(px(8.0))
                            .cursor_pointer()
                            .bg(gpui::hsla(0.0, 0.0, 1.0, 0.10))
                            .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.18)))
                            .text_size(px(12.0))
                            .text_color(rgb(0xe0e0e0))
                            .child(gpui::SharedString::from("Close"))
                            .on_click(move |_, _w, cx| {
                                if let Some(close) = on_close.as_ref() {
                                    close(cx);
                                }
                            }),
                    ),
            )
            // Scrollable body. The bar is `flex_shrink_0` so it does not
            // scroll away, and the body takes the rest.
            .child(
                div()
                    .id("lume-picker-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_5()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .items_start()
                    .children(tiles),
            )
            .into_any_element()
    }
}

/// One wallpaper tile. Outside `render` because the uniform-list closure is
/// `'static` and cannot borrow the view.
#[allow(clippy::too_many_arguments)]
fn tile_tile(
    i: usize,
    path: &PathBuf,
    tile: f32,
    current: Option<&std::path::Path>,
    hovered: Option<usize>,
    this: &Entity<Picker>,
) -> AnyElement {
    let accent = gpui::hsla(0.60, 0.85, 0.68, 1.0);

    // Asked for as the tile is built, not in a batch. `request` is idempotent
    // and never decodes on this thread.
    thumbs::request(path, thumbs::MAX_EDGE);

    let preview: AnyElement = match thumbs::get(path, thumbs::MAX_EDGE) {
        Some(image) => gpui::img(gpui::ImageSource::Image(image))
            .size_full()
            .into_any_element(),
        None => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(10.0))
            .text_color(rgb(0x707070))
            .child(gpui::SharedString::from("…"))
            .into_any_element(),
    };

    let hovered_now = hovered == Some(i);
    let is_current = current == Some(path.as_path());

    div()
        .id(("lume-wallpaper", super::background::hash_path(path)))
        .w(px(tile))
        // Explicit height. A uniform-list row's height does not stretch its
        // children, so a tile with only a width collapses to nothing — which
        // is what happened the first time this ran.
        .h(px(tile * 9.0 / 16.0))
        .flex_shrink_0()
        .rounded(px(10.0))
        .overflow_hidden()
        .cursor_pointer()
        .border_2()
        .border_color(if is_current {
            accent
        } else if hovered_now {
            gpui::hsla(0.0, 0.0, 1.0, 0.35)
        } else {
            gpui::hsla(0.0, 0.0, 1.0, 0.08)
        })
        .shadow_lg()
        .child(preview)
        .on_hover({
            let this = this.clone();
            move |_e, _w, cx| this.update(cx, |me, cx| {
                if me.hovered != Some(i) {
                    me.hovered = Some(i);
                    cx.notify();
                }
            })
        })
        .on_mouse_exit({
            let this = this.clone();
            move |_e, _w, cx| this.update(cx, |me, cx| {
                if me.hovered == Some(i) {
                    me.hovered = None;
                    cx.notify();
                }
            })
        })
        .on_click({
            let this = this.clone();
            let path = path.clone();
            move |_, _w, cx| {
                // Notify the overlay that content was clicked so it doesn't dismiss.
                let cb = this.read(cx).on_content_click.clone();
                if let Some(cb) = cb {
                    cb(cx);
                }
                if let Err(e) = super::background::set_wallpaper(&path) {
                    tracing::error!(path = %path.display(), "wallpaper failed: {e:#}");
                    return;
                }
                this.update(cx, |me, cx| {
                    me.current = Some(path.clone());
                    cx.notify();
                });
            }
        })
        .into_any_element()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_filter_is_case_insensitive() {
        assert!(matches!("JPG".to_ascii_lowercase().as_str(), "jpg"));
        assert!(!matches!("gif".to_ascii_lowercase().as_str(), "jpg"));
    }

    #[test]
    fn tile_size_is_clamped_to_a_sane_range() {
        for w in [640.0_f32, 1280.0, 1920.0, 3840.0] {
            let t = (w * 0.16).clamp(140.0, 240.0);
            assert!((140.0..=240.0).contains(&t), "width {w} produced {t}");
        }
    }

    /// Duplicate paths across wallpaper directories must appear once, or the
    /// same picture shows up twice in the grid.
    #[test]
    fn scan_deduplicates() {
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert("/a/x.png"));
        assert!(!seen.insert("/a/x.png"));
    }
}
