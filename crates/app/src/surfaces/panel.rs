//! Panels: the sidebars and the dock.
//!
//! Stage 3 gives them a surface and correct geometry; the content arrives in
//! stage 5. What matters now is that the reservation stack order is right and
//! that hiding does not unmap.

/// A workspace row. The sidebar's real content replaces this in stage 5; it
/// exists now so the panel is visibly non-empty while map order is verified.
pub struct PanelBody {
    pub title: &'static str,
    pub accent: u32,
}

impl PanelBody {
    pub fn new(title: &'static str, accent: u32) -> Self {
        Self { title, accent }
    }
}