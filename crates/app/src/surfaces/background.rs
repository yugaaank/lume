//! The background surface: wallpaper and desktop icons.
//!
//! Stage 3 opens it with correct geometry and a click-through input region.
//! The wallpaper engine and icons arrive in stage 4.
//!
//! This is the surface with the real worst case: it is full-screen, so its
//! swapchain is the largest in the shell, and it covers everything. If its
//! input region is wrong, the user cannot click their terminal.

use gpui::{Bounds, Size, px};

/// The rectangles that accept input. Empty means the surface is invisible to
/// the pointer.
///
/// `set_input_region(Some(&[]))` is "no input"; `set_input_region(None)` is
/// "the whole surface", which is the opposite of what hiding requires.
pub fn input_regions(_icon_count: usize) -> Vec<Bounds<gpui::Pixels>> {
    // No icons yet. Empty, so clicks pass straight through to the desktop.
    Vec::new()
}

/// The surface is full-screen and transparent apart from what stage 4 paints.
pub fn surface_size(display: gpui::Size<gpui::Pixels>) -> Size<gpui::Pixels> {
    display
}

/// Wallpaper colours, if a palette has been extracted. Stage 8.
pub fn palette_cache_path() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lume")
        .join("palette")
}

pub fn _unused(_: gpui::Pixels) {
    let _ = px(0.0);
}