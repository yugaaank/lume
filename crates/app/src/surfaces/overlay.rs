//! The overlay surface: things that must be above windows.
//!
//! One full-screen `Overlay` layer surface, holding whatever is modal or
//! transient. Today that is the wallpaper picker; later it is the OSD,
//! notification toasts and the power menu.
//!
//! ## Why it exists
//!
//! Both things it replaces were broken in ways that looked like working code.
//!
//! * **The picker was on the Background surface.** The Background layer is
//!   *below* every window. The picker opened, scanned 29 wallpapers and
//!   decoded every thumbnail, and was invisible under a single fullscreen
//!   window.
//! * **The Background surface cannot receive keys.** Its spec is
//!   `KeyboardInteractivity::None`, so "press Escape to close" could never
//!   have worked there no matter how carefully it was wired. The Overlay
//!   surface is `OnDemand`, so it can take focus while it is up and give it
//!   back when it closes.
//!
//! ## Why it is mapped hidden
//!
//! With `exclusive_zone = 0` and an empty input region, a mapped-but-hidden
//! overlay costs one surface and zero compositor traffic. Mapping it visible
//! would put a full-screen click target over the whole desktop for the entire
//! life of the shell.
//!
//! ## Escape
//!
//! Handled here rather than in the picker. The picker is content; this is the
//! surface that has the focus, and putting the dismiss key on the content
//! means every future overlay has to remember to re-implement it.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    StatefulInteractiveElement, Window, div,
    prelude::*, px,
};

use super::picker::Picker;

pub struct Overlay {
    focus: FocusHandle,
    picker: Option<Entity<Picker>>,
    /// The display this overlay covers. The picker needs it to size its grid,
    /// and the surface is the thing that knows.
    display: gpui::Size<gpui::Pixels>,
    /// Whether the keyboard focus has already been taken for the current
    /// open. Focus is taken in `render` because `Context` has no way to focus
    /// without a `Window`, and `open_picker` does not have one.
    focused: bool,
}

impl Overlay {
    pub fn new(display: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) -> Self {
        Self { focus: cx.focus_handle(), picker: None, display, focused: false }
    }

    pub fn is_empty(&self) -> bool {
        self.picker.is_none()
    }

    pub fn open_picker(&mut self, current: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        let display = self.display;
        let picker = cx.new(|cx| Picker::new(display, current, cx));

        // Wired here rather than in the picker, for the same reason the
        // desktop cannot wire its own: the parent handle does not exist until
        // after `cx.new`.
        let me = cx.entity();
        picker.update(cx, |p, _cx| {
            p.set_on_close(Arc::new(move |app| {
                me.update(app, |o, cx| {
                    o.close(cx);
                });
            }));
        });

        self.picker = Some(picker);
        // Without this nothing repaints and the surface stays empty. The
        // picker decodes 29 thumbnails into a cache nobody displays, which is
        // what made the first attempt look like it had opened.
        cx.notify();
        tracing::info!("overlay opened the wallpaper picker");
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            self.focused = false;
            // Freed here rather than left to the process lifetime: the
            // thumbnails are ~100 MB of device memory and nothing else wants
            // them once the picker is gone.
            super::thumbs::clear();
            tracing::info!("overlay closed");
            cx.notify();
        }
    }

    pub fn toggle_picker(&mut self, current: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close(cx);
        } else {
            self.open_picker(current, cx);
        }
    }
}

impl Focusable for Overlay {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take the keyboard once per open. On a surface with
        // `KeyboardInteractivity::OnDemand` this is the whole reason Escape
        // can work; on the Background surface's `None` it could not.
        if self.picker.is_some() && !self.focused {
            window.focus(&self.focus, cx);
            self.focused = true;
        }

        let Some(picker) = self.picker.clone() else {
            // Nothing to show. Returning an empty, transparent, zero-size node
            // rather than a full-screen div: a visible div here would paint
            // over the desktop even with no children.
            return div().id("lume-overlay-empty").size_0().into_any_element();
        };

        div()
            .id("lume-overlay")
            .size_full()
            // Registers this element as the NODE for `self.focus` in the
            // rendered frame.
            //
            // Without it, `Window::dispatch_key_event` calls
            // `focus_node_id_in_rendered_frame(self.focus)` and gets nothing
            // back, because a FocusHandle is not an element until something
            // calls `track_focus` on it. The consequences looked like "Escape
            // does not work": focus was set, the surface had exclusive
            // keyboard, Hyprland delivered the key, and GPUI discarded it
            // looking for a node that was never registered.
            .track_focus(&self.focus)
            .on_key_down({
                let this = cx.entity();
                move |event: &gpui::KeyDownEvent, _w, cx| {
                    // Logged for EVERY key, not just escape. The question
                    // being answered is "does this surface receive keys at
                    // all", and a handler that silently ignores everything
                    // else cannot answer it.
                    tracing::debug!(key = %event.keystroke.key, "overlay key");
                    if event.keystroke.key != "escape" {
                        return;
                    }
                    this.update(cx, |me, cx| me.close(cx));
                }
            })
            .child(picker)
            .into_any_element()
    }
}
