//! The bar's content.
//!
//! The layer surface itself is owned by `surfaces::Surfaces` — this file only
//! knows what goes inside it.

use gpui::{
    App, Context, FocusHandle, Focusable, Render, Stateful, StatefulInteractiveElement, Window, div,
    prelude::*, px, rgb,
};
use services::compositor::Action;

/// Exclusive zone and height. The registry reads this for geometry.
pub const BAR_HEIGHT: f32 = 32.0;

pub struct BarView {
    focus: FocusHandle,
    workspaces: Vec<services::Workspace>,
    active: Option<i64>,
}

impl BarView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.global::<services::AppState>().clone();
        let compositor = state.compositor.clone();

        // One listener for the whole bar, registered once. The service fans out
        // internally; we are not adding a listener per workspace.
        //
        // `cx.spawn` already supplies the WeakEntity. The is_none() break is
        // what lets the loop end when the surface closes — without it this task
        // holds the surface alive forever.
        cx.spawn(async move |weak, cx: &mut gpui::AsyncApp| {
            loop {
                // notified_owned() so the permit survives being moved between
                // tasks; notified() borrows and cannot cross an await.
                compositor.changed().notified_owned().await;

                if weak.upgrade().is_none() {
                    break;
                }

                let _ = weak.update(cx, |bar, cx| {
                    bar.refresh_from(&compositor);
                    cx.notify();
                });
            }
        })
        .detach();

        let mut bar = Self {
            focus: cx.focus_handle(),
            workspaces: Vec::new(),
            active: None,
        };
        // `compositor` moved into the spawn above; `state` is still ours.
        bar.refresh_from(&state.compositor);
        bar
    }

    /// Take the handle rather than a Context so this works from both a sync
    /// context and an AsyncApp update.
    fn refresh_from(&mut self, compositor: &services::CompositorService) {
        // ArcSwap load: an atomic pointer read, never a lock.
        self.workspaces = compositor.workspaces().as_ref().clone();
        self.active = Some(compositor.active().id);
    }

    fn workspace_pips(&self) -> Vec<Stateful<gpui::Div>> {
        self.workspaces
            .iter()
            .map(|ws| {
                let is_active = self.active == Some(ws.id);
                let id = ws.id;

                div()
                    // Stable per-workspace id. Inside a loop this is not
                    // optional: without it every pip shares one global element
                    // id and the hit-test tree is rebuilt every frame.
                    // ElementId accepts only (&'static str, usize|u64|EntityId)
                    // tuples — there is no generic form, so cast.
                    .id(("lume-ws-pip", id as usize))
                    .h(px(20.0))
                    .px_2()
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .bg(if is_active {
                        gpui::hsla(0.0, 0.0, 1.0, 0.16)
                    } else {
                        gpui::hsla(0.0, 0.0, 0.0, 0.0)
                    })
                    .text_color(if is_active {
                        rgb(0xf0f0f0)
                    } else {
                        rgb(0x909090)
                    })
                    .child(gpui::SharedString::from(ws.name.clone()))
                    .on_click(move |_, _window, cx| {
                        // Logged, not surfaced. A failed switch is recoverable
                        // and must not take the bar down with it.
                        if let Err(e) = cx
                            .global::<services::AppState>()
                            .compositor
                            .act(Action::FocusWorkspace { id })
                        {
                            tracing::warn!(?id, "workspace switch failed: {e:#}");
                        }
                    })
            })
            .collect()
    }
}

// The trait is `Focusable`, not `FocusableElement` — that name does not exist
// in this fork.
impl Focusable for BarView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BarView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Pure. No mutation, no window calls, no cx.notify(), no surface
        // setters. All of that belongs in a handler.
        div()
            .id("lume-bar")
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            // Rgba has no .alpha(); alpha is the low byte of an rgba() hex or a
            // component of hsla().
            .bg(gpui::hsla(0.10, 0.05, 0.09, 0.92))
            .text_color(rgb(0xe8e8e8))
            // text_size takes an AbsoluteLength, so px(), not a bare float.
            .text_size(px(12.0))
            .child(div().flex().gap_1().children(self.workspace_pips()))
            .child(gpui::SharedString::from("lume"))
    }
}
