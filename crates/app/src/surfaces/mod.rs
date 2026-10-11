//! Surface registry.
//!
//! Five long-lived layer surfaces, created once at startup in a fixed order,
//! hidden by zeroing their exclusive zone and emptying their input region —
//! never by unmapping.
//!
//! ## Map order is load-bearing
//!
//! The exclusive-zone reservation stack order is fixed at map time, and whoever
//! maps first owns the corners. So **horizontal edges (bar, dock) map before
//! vertical edges (sidebars)** — otherwise a sidebar claims the top-left corner
//! and pushes the bar's rounded end outside its own reserved region.
//!
//! `Surfaces::open` encodes that order. Do not reorder the calls.
//!
//! ## Hiding
//!
//! `exclusive_zone = 0` plus `set_input_region(Some(&[]))`. Note `Some(&[])`,
//! not `None` — `None` means the WHOLE surface receives input. An invisible
//! surface with a stale region still eats clicks.
//!
//! ## Why panels translate their content
//!
//! This fork has no `set_margin`, so a layer surface cannot be repositioned
//! after creation, and recreating one to change its margin would reorder the
//! reservation stack. Panels therefore slide by translating content inside a
//! fixed-size surface. That costs no protocol traffic per frame.

use gpui::{
    Animated, AnyWindowHandle, Bounds, Context, Entity, Focusable, FocusHandle,
    AnyElement, LinuxWindowBackground, Pixels, Render, Size, Window, WindowBounds, WindowKind,
    WindowOptions,
    div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    millis, prelude::*, px,
};
use std::sync::Arc;
use std::time::Instant;

pub mod background;
pub mod bar;
pub mod panel;
pub mod picker;

pub const SIDEBAR_WIDTH: f32 = 220.0;
pub const DOCK_HEIGHT: f32 = 64.0;
pub const SLIDE_MS: u64 = 180;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Background,
    Bar,
    Dock,
    SidebarLeft,
    SidebarRight,
}

impl Kind {
    fn app_id(self) -> &'static str {
        match self {
            Kind::Background => "lume-bg",
            Kind::Bar => "lume-bar",
            Kind::Dock => "lume-dock",
            Kind::SidebarLeft => "lume-sidebar-l",
            Kind::SidebarRight => "lume-sidebar-r",
        }
    }

    /// Horizontal edges first, then vertical. See the module docs.
    fn map_order(self) -> u8 {
        match self {
            Kind::Background => 0,
            Kind::Bar => 1,
            Kind::Dock => 2,
            Kind::SidebarLeft => 3,
            Kind::SidebarRight => 4,
        }
    }
}

struct Spec {
    layer: Layer,
    anchor: Anchor,
    keyboard: KeyboardInteractivity,
    width: f32,
    height: f32,
    visible: bool,
    /// The zone to restore when this surface becomes visible.
    restore_zone: f32,
}

fn spec_for(kind: Kind, w: f32, h: f32) -> Spec {
    match kind {
        Kind::Background => Spec {
            restore_zone: 0.0,
            layer: Layer::Background,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            keyboard: KeyboardInteractivity::None,
            width: w,
            height: h,
            visible: true,
        },
        Kind::Bar => Spec {
            restore_zone: bar::BAR_HEIGHT,
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            keyboard: KeyboardInteractivity::None,
            width: w,
            height: bar::BAR_HEIGHT,
            visible: true,
        },
        Kind::Dock => Spec {
            restore_zone: DOCK_HEIGHT,
            layer: Layer::Top,
            anchor: Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            keyboard: KeyboardInteractivity::OnDemand,
            width: w,
            height: DOCK_HEIGHT,
            visible: false,
        },
        // BOTTOM|LEFT, not TOP|LEFT: the compositor does not subtract the bar's
        // exclusive zone, so a TOP|LEFT sidebar would leave a gap under the
        // bar. Letting the bar paint over the sidebar's top strip is what most
        // shells look like anyway, and there is no height to keep in sync.
        Kind::SidebarLeft => Spec {
            restore_zone: SIDEBAR_WIDTH,
            layer: Layer::Top,
            anchor: Anchor::BOTTOM | Anchor::LEFT,
            keyboard: KeyboardInteractivity::OnDemand,
            width: SIDEBAR_WIDTH,
            height: h,
            visible: false,
        },
        Kind::SidebarRight => Spec {
            restore_zone: SIDEBAR_WIDTH,
            layer: Layer::Top,
            anchor: Anchor::BOTTOM | Anchor::RIGHT,
            keyboard: KeyboardInteractivity::OnDemand,
            width: SIDEBAR_WIDTH,
            height: h,
            visible: false,
        },
    }
}

/// The zone to use AT CREATION.
///
/// Derived, never stored twice. A hidden surface maps with 0 so it reserves
/// nothing; a visible one maps with its restore zone. Keeping these as two
/// independent fields is how the bar once shipped with zone 0 and stopped
/// pushing windows down.
fn creation_zone(s: &Spec) -> f32 {
    if s.visible { s.restore_zone } else { 0.0 }
}

fn window_options(kind: Kind, s: &Spec) -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(s.width), px(s.height)),
        })),
        app_id: Some(kind.app_id().to_string()),
        linux_window_background: LinuxWindowBackground::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: kind.app_id().to_string(),
            layer: s.layer,
            anchor: s.anchor,
            exclusive_zone: Some(px(creation_zone(s))),
            exclusive_edge: None,
            margin: Some((px(0.0), px(0.0), px(0.0), px(0.0))),
            keyboard_interactivity: s.keyboard,
        }),
        inactive_frame_interval: None,
        ..Default::default()
    }
}

/// One long-lived layer surface.
///
/// The mutating API is not called from anywhere yet: stage 3 builds the
/// registry and proves map order and zones, stage 5 wires the panel toggles
/// and content. Marked dead_code deliberately rather than wired to nothing.
#[allow(dead_code)]
pub struct SurfaceHandle {
    window: AnyWindowHandle,
    kind: Kind,
    zone: f32,
    visible: bool,
    /// Last values pushed to the compositor, for diffing.
    pushed_zone: Option<Pixels>,
    pushed_region: Option<Vec<Bounds<Pixels>>>,
}

impl SurfaceHandle {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Show or hide. NEVER unmaps — see the module docs.
    pub fn set_visible(&mut self, visible: bool, cx: &mut gpui::App) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;

        let zone = if visible { px(self.zone) } else { px(0.0) };
        let region: Option<Vec<Bounds<Pixels>>> = if visible { None } else { Some(vec![]) };

        let window = self.window.clone();
        window.update(cx, |_, window, _| {
            window.set_exclusive_zone(zone);
            window.set_input_region(region.as_deref());
        });
        self.pushed_zone = Some(zone);
        self.pushed_region = region;
        tracing::debug!(kind = ?self.kind, visible, "visibility changed");
    }

    /// Restrict input to these rectangles. Diffed — this is a surface commit
    /// every time it is called, so calling it per frame would spin.
    pub fn set_region(&mut self, rects: Vec<Bounds<Pixels>>, cx: &mut gpui::App) {
        if !self.visible || self.pushed_region.as_ref() == Some(&rects) {
            return;
        }
        self.pushed_region = Some(rects.clone());
        let window = self.window.clone();
        window.update(cx, |_, window, _| {
            window.set_input_region(Some(&rects));
        });
    }
}

#[allow(dead_code)]
/// Slide state for one panel.
///
/// `Animated::sample()` reports `is_active`, which is what lets the frame loop
/// exit. An idle shell must not be running a loop.
pub struct Slide {
    animated: Animated<f32>,
    shown: bool,
}

impl Slide {
    fn new(shown: bool) -> Self {
        let v = if shown { 1.0 } else { 0.0 };
        Self { animated: Animated::new(v, millis(SLIDE_MS)), shown }
    }

    pub fn set_shown(&mut self, shown: bool, now: Instant) -> bool {
        if self.shown == shown {
            return false;
        }
        self.shown = shown;
        // Retargets from the CURRENT presented value, so reversing mid-slide
        // does not jump.
        self.animated
            .set(if shown { 1.0 } else { 0.0 }, millis(SLIDE_MS), now)
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    /// (progress 0..1, still_animating)
    pub fn sample(&mut self, now: Instant) -> (f32, bool) {
        let s = self.animated.sample(now);
        (s.value, s.is_active)
    }
}

/// The registry.
///
/// Open it once, at startup. Adding a surface later means restarting, which
/// is deliberate: map order is fixed at map time and re-mapping breaks it.
pub struct Surfaces {
    pub background: SurfaceHandle,
    pub bar: SurfaceHandle,
    pub dock: SurfaceHandle,
    pub sidebar_left: SurfaceHandle,
    pub sidebar_right: SurfaceHandle,
}

impl Surfaces {
    /// Wait for outputs, then map every surface.
    ///
    /// GPUI's Linux platform calls the `run` callback with no roundtrip, so
    /// `cx.displays()` is EMPTY in there — `zwlr_layer_shell_v1` binds but
    /// `get_layer_surface` never fires and nothing appears. Measured:
    /// `displays at launch count=0`.
    ///
    /// Every surface goes through this gate. A surface opened before its
    /// output exists silently never appears.
    pub fn open_when_ready(cx: &mut gpui::App) {
        if !cx.displays().is_empty() {
            let (w, h) = cx
                .displays()
                .first()
                .map(|d| {
                    (
                        f32::from(d.bounds().size.width),
                        f32::from(d.bounds().size.height),
                    )
                })
                .unwrap_or((1920.0, 1080.0));
            Self::open(cx, w, h);
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

            cx.update(|cx| {
                // Dedupe by display id: a GPU app's modeset can announce an
                // output twice, and without this you get two of everything.
                let mut seen = std::collections::HashSet::new();
                let mut opened = 0;
                for display in cx.displays() {
                    if !seen.insert(display.id()) {
                        continue;
                    }
                    let w = f32::from(display.bounds().size.width);
                    let h = f32::from(display.bounds().size.height);
                    Self::open(cx, w, h);
                    opened += 1;
                }
                tracing::info!(monitors = opened, "surface registry opened");
            });
        })
        .detach();
    }

    pub fn open(cx: &mut gpui::App, w: f32, h: f32) -> Self {
        let opened: Vec<SurfaceHandle> = [
            Kind::Background,
            Kind::Bar,
            Kind::Dock,
            Kind::SidebarLeft,
            Kind::SidebarRight,
        ]
        .into_iter()
        .map(|kind| Self::open_one(cx, kind, w, h))
        .collect();

        tracing::info!(
            surfaces = opened.len(),
            "registry up: {}",
            opened.iter().map(|s| s.kind.app_id()).collect::<Vec<_>>().join(", ")
        );

        let mut it = opened.into_iter();
        Surfaces {
            background: it.next().expect("background"),
            bar: it.next().expect("bar"),
            dock: it.next().expect("dock"),
            sidebar_left: it.next().expect("sidebar_left"),
            sidebar_right: it.next().expect("sidebar_right"),
        }
    }

    fn open_one(cx: &mut gpui::App, kind: Kind, w: f32, h: f32) -> SurfaceHandle {
        let s = spec_for(kind, w, h);

        // Shared with the 'static builder closure, which cannot borrow.
        let initial_region: Arc<std::sync::Mutex<Vec<gpui::Bounds<gpui::Pixels>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let region_slot = initial_region.clone();

        match cx.open_window(window_options(kind, &s), move |_window, cx| {
            let view = cx.new(|cx| {
                SurfaceView::new(kind, s.visible, gpui::size(px(s.width), px(s.height)), cx)
            });

            // Seed the icon regions now, while a real App is in scope.
            // `Entity::read` needs one, so it cannot be done from render().
            if kind == Kind::Background {
                *region_slot.lock().expect("region mutex poisoned") =
                    view.read(cx).desktop.read(cx).input_regions();
            }

            view
        }) {
            Ok(handle) => {
                let mut handle = SurfaceHandle {
                    window: handle.into(),
                    kind,
                    zone: s.restore_zone,
                    visible: s.visible,
                    pushed_zone: None,
                    pushed_region: None,
                };

                // WindowOptions has no input-region field, so a surface that
                // maps hidden must be given an empty one here. Some(&[]) is
                // "no input"; None would be "the whole surface".
                let rects = initial_region.lock().expect("region mutex poisoned").clone();
                if s.visible && !rects.is_empty() {
                    // Icon rects only. Everything else on this surface stays
                    // click-through, so a click aimed at a terminal reaches it.
                    handle.pushed_region = Some(rects.clone());
                    let w = handle.window.clone();
                    let r = rects.clone();
                    w.update(cx, |_, window, _| {
                        window.set_input_region(Some(&r));
                    });
                } else if !s.visible {
                    handle.pushed_region = Some(vec![]);
                    let window = handle.window.clone();
                    window.update(cx, |_, window, _| {
                        window.set_input_region(Some(&[]));
                    });
                }
                handle.pushed_zone = Some(px(creation_zone(&s)));

                tracing::info!(app_id = kind.app_id(), visible = s.visible, "mapped");
                handle
            }
            Err(e) => {
                tracing::error!(app_id = kind.app_id(), "map failed: {e}");
                panic!("cannot open layer surface {}: {e}", kind.app_id())
            }
        }
    }

    pub fn handle_mut(&mut self, kind: Kind) -> Option<&mut SurfaceHandle> {
        match kind {
            Kind::Background => Some(&mut self.background),
            Kind::Bar => Some(&mut self.bar),
            Kind::Dock => Some(&mut self.dock),
            Kind::SidebarLeft => Some(&mut self.sidebar_left),
            Kind::SidebarRight => Some(&mut self.sidebar_right),
        }
    }
}

/// The root view of every surface. Dispatches on kind.
pub struct SurfaceView {
    kind: Kind,
    focus: FocusHandle,
    bar: Entity<bar::BarView>,
    desktop: Entity<background::Desktop>,
    dock: Entity<panel::Dock>,
    slide: Option<Slide>,
}

impl SurfaceView {
    fn new(kind: Kind, visible: bool, size: Size<Pixels>, cx: &mut Context<Self>) -> Self {
        // The background carries the desktop: icons, context menu, picker.
        // The bar carries its own content. Panels are placeholders until
        // stage 5.
        let bar = cx.new(|cx| bar::BarView::new(cx));
        let desktop = cx.new(|cx| background::Desktop::new(size, cx));
        let dock = cx.new(|cx| panel::Dock::new(cx));
        Self {
            kind,
            focus: cx.focus_handle(),
            bar,
            desktop,
            dock,
            slide: if matches!(kind, Kind::Bar | Kind::Background) {
                None
            } else {
                Some(Slide::new(visible))
            },
        }
    }
}

impl Focusable for SurfaceView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SurfaceView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Pure. No mutation, no window calls, no notify, no surface setters.
        let body: AnyElement = match self.kind {
            Kind::Bar => self.bar.clone().into_any_element(),
            _ => div()
                .id(("lume-placeholder", self.kind.map_order() as usize))
                .size_full()
                .bg(gpui::hsla(0.0, 0.0, 0.0, 0.35))
                .child(gpui::SharedString::from(self.kind.app_id()))
                .into_any_element(),
        };

        // Panels translate their content. The surface does not move — there is
        // no set_margin in this fork — and nothing here touches the compositor,
        // so a slide costs no protocol traffic per frame.
        match self.slide.as_mut() {
            Some(slide) => {
                let (progress, _) = slide.sample(Instant::now());
                let travel = 1.0 - progress;
                let offset = match self.kind {
                    Kind::SidebarLeft => -travel * SIDEBAR_WIDTH,
                    Kind::SidebarRight => travel * SIDEBAR_WIDTH,
                    Kind::Dock => travel * DOCK_HEIGHT,
                    _ => 0.0,
                };
                div()
                    .size_full()
                    .relative()
                    .child(div().absolute().left(px(offset)).top_0().size_full().child(body))
                    .into_any_element()
            }
            // Both arms must be AnyElement or the match arms disagree.
            None => body.into_any_element(),
        }
    }
}