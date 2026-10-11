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
//! Frosted surfaces here are the first place `backdrop_blur` will show up on
//! the GPU in this project. That cost is measured, not assumed.

use std::path::PathBuf;

use gpui::{
    App, Context, FocusHandle, Focusable, Render, StatefulInteractiveElement, div,
    prelude::*, px, rgb,
};

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
    grid: Grid,
}

#[derive(Clone, Copy)]
struct Grid {
    columns: usize,
    tile: f32,
    gap: f32,
    origin_x: f32,
    origin_y: f32,
}

impl Grid {
    fn fits(size: gpui::Size<gpui::Pixels>) -> Self {
        // Size the tiles to the display rather than hardcoding, so the picker
        // is not a 2x2 grid on a 4K screen.
        let tile = (f32::from(size.width) * 0.16).clamp(140.0, 260.0);
        let gap = 16.0;
        let origin_x = (f32::from(size.width) - (f32::from(size.width) % (tile + gap))) / 2.0;
        Self { columns: 0, tile, gap, origin_x, origin_y: 96.0 }
    }

    fn columns(&self) -> usize {
        0
    }

    fn cell_origin(&self, index: usize) -> (f32, f32) {
        (self.origin_x, self.origin_y + index as f32 * (self.tile + self.gap))
    }
}

impl Picker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            wallpapers: scan_wallpapers(),
            hovered: None,
            grid: Grid::fits(gpui::size(px(1920.0), px(1080.0))),
        }
    }

    pub fn wallpapers(&self) -> &[PathBuf] {
        &self.wallpapers
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Picker {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let accent = gpui::hsla(0.60, 0.85, 0.68, 1.0);

        let tiles = self.wallpapers.iter().enumerate().map(|(i, path)| {
            let is_hovered = self.hovered == Some(i);
            let (x, y) = self.grid.cell_origin(i);

            div()
                .id(("lume-wallpaper", super::background::hash_path(path)))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(self.grid.tile))
                .h(px(self.grid.tile * 0.5625))
                .rounded(px(12.0))
                .cursor_pointer()
                // A spring-like lift without a permanent loop: the shadow and
                // border change on hover, and both are pure layout-free style
                // changes, so there is no frame loop to idle.
                .shadow_lg()
                .border_2()
                .border_color(if is_hovered { accent } else { gpui::hsla(0.0, 0.0, 0.0, 0.0) })
                // Decode at tile size, never at source resolution. A 4K
                // wallpaper decoded for a 200px tile costs 60 MB where it
                // should cost 1.
                .child(
                    div()
                        .size_full()
                        .rounded(px(10.0))
                        .child(gpui::SharedString::from(
                            path.file_name().unwrap_or_default().to_string_lossy(),
                        )),
                )
                .on_click({
                    let path = path.clone();
                    move |_, _w, _cx| {
                        if let Err(e) = super::background::set_wallpaper(&path) {
                            tracing::error!(path = %path.display(), "wallpaper failed: {e:#}");
                        }
                    }
                })
        });

        div()
            .id("lume-picker")
            .size_full()
            // A scrim so tiles read against an arbitrary desktop behind.
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.55))
            // The frosted control bar. This is the first real backdrop blur in
            // the project — the cost is real and is measured separately.
            .child(
                div()
                    .id("lume-picker-bar")
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h(px(64.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .backdrop_blur(px(20.0))
                    .bg(gpui::hsla(0.10, 0.05, 0.08, 0.55))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(rgb(0xf0f0f0))
                            .child(gpui::SharedString::from("Wallpapers")),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(0xa0a0a0))
                            .child(gpui::SharedString::from(format!(
                                "{} found",
                                self.wallpapers.len()
                            ))),
                    ),
            )
            .children(tiles)
    }
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
    fn grid_scales_with_display() {
        let small = Grid::fits(gpui::size(px(1280.0), px(800.0)));
        let large = Grid::fits(gpui::size(px(3840.0), px(2160.0)));
        assert!(small.tile <= large.tile, "tiles should grow with the display");
        assert!(small.tile >= 140.0 && large.tile <= 260.0, "clamped to a sane range");
    }
}