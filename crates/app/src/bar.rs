use gpui::{
    App, Context, FocusHandle, Focusable, LinuxWindowBackground, Render, Stateful, Window,
    WindowBounds, WindowKind, WindowOptions, StatefulInteractiveElement, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    prelude::*, px, rgb,
};
use services::compositor::Action;

/// What a user writes in their compositor config to target this surface.
///
/// Every surface gets its own app_id, and these strings are the shell's public
/// API to compositor users. Document them.
pub const BAR_APP_ID: &str = "lume-bar";

pub const BAR_HEIGHT: f32 = 32.0;

fn window_options(width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        // No titlebar. We are not a normal window.
        titlebar: None,

        // Must be Some with an explicit size. A layer surface with no bounds
        // lands on the primary display and may end up 0x0, which the protocol
        // rejects.
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(width), px(height)),
        })),

        // How the compositor identifies this surface to the user.
        app_id: Some(BAR_APP_ID.to_string()),

        // We paint our own background. Without this the compositor puts an
        // opaque rect behind the whole surface, and a rounded bar gets a black
        // slab around it.
        linux_window_background: LinuxWindowBackground::Transparent,

        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: BAR_APP_ID.to_string(),

            // Top, not Overlay. Overlay is for things that must sit above
            // other layer surfaces; the bar belongs below fullscreen-ish
            // content in a way users expect.
            layer: Layer::Top,

            // Bitflags: TOP pins to the top edge, LEFT|RIGHT stretches across
            // the full screen width. The width above is then a maximum, not a
            // target.
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,

            // Positive: the compositor guarantees nothing else covers the top
            // `height` pixels and sizes windows below it. This is what makes it
            // a panel rather than an overlapping decoration.
            exclusive_zone: Some(px(height)),

            // Derived from `anchor` when None. Named explicitly because every
            // field must be listed.
            exclusive_edge: None,

            // CSS order: top, right, bottom, left.
            margin: Some((px(0.0), px(0.0), px(0.0), px(0.0))),

            // An idle bar must never take the keyboard from the user's
            // editor. Stage 3 flips this to OnDemand when a focusable surface
            // opens.
            keyboard_interactivity: KeyboardInteractivity::None,
        }),

        // No idle throttling. Without this an inactive window renders at a
        // reduced frame rate and you pay for frames nobody sees.
        inactive_frame_interval: None,

        ..Default::default()
    }
}

/// Open a bar on every display, waiting for outputs to be enumerated first.
///
/// **Why the wait:** GPUI's Linux platform calls the `run` callback
/// immediately, with no roundtrip. Outputs arrive asynchronously from
/// `wl_output` events, so `cx.displays()` is EMPTY inside that callback --
/// `zwlr_layer_shell_v1` is bound, but `get_layer_surface` never fires and no
/// surface appears. Measured: `displays at launch count=0`.
///
/// Every surface added later must go through this same gate, or it will
/// silently fail to appear.
pub fn open_when_ready(cx: &mut App) {
    if !cx.displays().is_empty() {
        open(cx);
        return;
    }

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        for _ in 0..200 {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(10))
                .await;
            if cx.update(|cx| !cx.displays().is_empty()) {
                break;
            }
        }

        let count = cx.update(|cx| {
            open(cx);
            cx.displays().len()
        });

        tracing::info!(monitors = count, "bar opened");
    })
    .detach();
}

fn open(cx: &mut App) {
    // Dedupe by display id. A GPU app's modeset can announce an output twice,
    // and without this you get two bars on one monitor.
    let mut seen = std::collections::HashSet::new();

    for display in cx.displays() {
        if !seen.insert(display.id()) {
            continue;
        }

        let width = f32::from(display.bounds().size.width);
        let mut options = window_options(width, BAR_HEIGHT);

        // Without this every bar lands on the primary display, so a
        // multi-monitor setup gets N bars stacked on monitor 1.
        options.display_id = Some(display.id());

        if let Err(e) = cx.open_window(options, |_window, cx| cx.new(|cx| Bar::new(cx))) {
            // Inside a tracing macro the sigils bind names like `display`, so a
            // variable with that name resolves to the macro helper.
            let monitor = display.id();
            tracing::error!(?monitor, "could not open bar: {e}");
        }
    }
}

pub struct Bar {
    focus: FocusHandle,
    workspaces: Vec<services::Workspace>,
    active: Option<i64>,
}

impl Bar {
    fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.global::<services::AppState>().clone();

        // One listener per bar, registered once. The CompositorService fans out
        // internally; we are not adding a listener per workspace.
        //
        // `WeakEntity` + the is_none() break is what lets the loop end when the
        // bar closes. Without it this task holds the surface alive forever.
        let compositor = state.compositor.clone();

        cx.spawn(async move |weak, cx: &mut gpui::AsyncApp| {
            loop {
                // notified_owned() so the permit survives moving across tasks;
                // notified() borrows and cannot be held across an await.
                compositor.changed().notified_owned().await;

                // The bar is gone. Stop, or this task holds it alive forever.
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

    /// Pull current state from the service. Takes the handle rather than a
    /// Context so it works from both a sync context and an AsyncApp update.
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
                        // Errors are logged, not surfaced. A failed switch is
                        // recoverable and must not take the bar down with it.
                        if let Err(e) =
                            cx.global::<services::AppState>()
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
impl Focusable for Bar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Bar {
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
            // Rgba has no .alpha(); alpha is either the low byte of an rgba()
            // hex or a component of hsla().
            .bg(gpui::hsla(0.10, 0.05, 0.09, 0.92))
            .text_color(rgb(0xe8e8e8))
            // text_size takes an AbsoluteLength, so it needs px(), not a bare
            // float.
            .text_size(px(12.0))
            .child(div().flex().gap_1().children(self.workspace_pips()))
            .child(gpui::SharedString::from("lume"))
    }
}