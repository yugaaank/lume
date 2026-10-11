use gpui::{
    App, Context, FocusHandle, Focusable, LinuxWindowBackground, Render, Window, WindowBounds,
    WindowKind, WindowOptions, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    prelude::*, px, rgb,
};

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
/// surface appears. Verified: `displays at launch count=0`.
///
/// Poll on the background executor until they show up. 200 x 10ms is a 2s
/// ceiling; a machine that has not reported an output by then has none.
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

        // `LayerShellNotSupportedError` renders as "Compositor doesn't support
        // zwlr_layer_shell_v1". Name the display; the user can act on that.
        if let Err(e) = cx.open_window(options, |_window, cx| cx.new(|cx| Bar::new(cx))) {
            // Inside a tracing macro the sigils bind names like `display`, so a
            // variable with that name resolves to the macro helper, not the loop
            // variable. Bind the id first.
            let monitor = display.id();
            tracing::error!(?monitor, "could not open bar: {e}");
        }
    }
}

pub struct Bar {
    focus: FocusHandle,
}

impl Bar {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
        }
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
            .child(gpui::SharedString::from("lume"))
    }
}