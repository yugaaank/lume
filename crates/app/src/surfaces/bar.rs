//! The bar's content.
//!
//! The layer surface itself is owned by `surfaces::Surfaces` — this file only
//! knows what goes inside it.

use gpui::{
    App, Context, FocusHandle, Focusable, Render, Stateful, StatefulInteractiveElement, Window, div,
    prelude::*, px, rgb,
};
use services::compositor::Action;

use super::widgets;

/// Exclusive zone and height. The registry reads this for geometry.
pub const BAR_HEIGHT: f32 = 32.0;

/// How often the clock is re-read.
///
/// One second, because a clock with seconds needs it. Pure computation — no
/// I/O, no subprocess — so a tick that renders identically costs a string
/// compare and nothing else.
const CLOCK_TICK: std::time::Duration = std::time::Duration::from_millis(1000);

/// How often the status widgets are re-read.
///
/// Three seconds, and the reason is `wpctl`: reading the default sink shells
/// out, so one poll a second is one fork, one PipeWire connection and one
/// ~2 KB pipe read every second, forever. Measured at 0.192% idle CPU.
///
/// A volume readout that updates every three seconds is indistinguishable
/// from one that updates every second — the value only moves when something
/// moves it, and when the user moves it they are looking at the OSD, not the
/// bar. Battery and network are plain file reads and are cheap enough that
/// they could share this tick without complaint.
const STATUS_TICK: std::time::Duration = std::time::Duration::from_millis(3000);

/// Everything a widget renders, as text. Diffed as a whole.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status {
    clock: String,
    network: String,
    network_signal: Option<services::network::Signal>,
    volume: Option<services::volume::Volume>,
    battery: services::battery::Battery,
    /// Whether the network widget drew anything at all.
    network_up: bool,
}

pub struct BarView {
    focus: FocusHandle,
    workspaces: Vec<services::Workspace>,
    active: Option<i64>,
    widgets: Vec<widgets::Widget>,
    status: Status,
    /// What the last render produced, so a tick that changes nothing does not
    /// repaint a 32px strip across 1920px.
    rendered: Status,
    /// The previous byte counters. State, not output, so it is kept out of
    /// `Status` — putting it there made the change-diff compare a tracker
    /// that changes on every tick, which is exactly the repaint this is
    /// meant to avoid.
    rates: services::network::RateTracker,
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

        let config = state.config.get();
        let widgets = widgets::active(&config);
        let mut bar = Self {
            focus: cx.focus_handle(),
            workspaces: Vec::new(),
            active: None,
            widgets,
            status: Status::default(),
            rendered: Status::default(),
            rates: services::network::RateTracker::new(),
        };
        bar.refresh_status(cx);

        // One timer for every widget, rather than one task per widget.
        //
        // The alternative — twenty-nine tasks each notifying on their own
        // schedule — repaints the bar continuously for values that mostly have
        // not changed. Here a tick is one comparison and usually nothing
        // happens.
        // Clock: fast, pure.
        cx.spawn(async move |weak, cx: &mut gpui::AsyncApp| {
            loop {
                cx.background_executor().timer(CLOCK_TICK).await;
                let alive = weak
                    .update(cx, |bar, cx| {
                        bar.refresh_clock(cx);
                        bar.notify_if_changed(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        // Status widgets: slower, because one of them forks a process.
        cx.spawn(async move |weak, cx: &mut gpui::AsyncApp| {
            loop {
                cx.background_executor().timer(STATUS_TICK).await;
                let alive = weak
                    .update(cx, |bar, cx| {
                        bar.refresh_status(cx);
                        bar.notify_if_changed(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        // `compositor` moved into the spawn above; `state` is still ours.
        bar.refresh_from(&state.compositor);
        bar
    }

    /// Take the handle rather than a Context so this works from both a sync
    /// context and an AsyncApp update.
    /// Re-read every active widget.
    ///
    /// `workspace_pips` is deliberately not touched: those come from the
    /// compositor's own change signal, which fires exactly when they change,
    /// and re-reading them on a 1 Hz tick would be 60 wakeups a minute for
    /// data that does not move.
    /// Repaint only if something the bar renders actually moved.
    fn notify_if_changed(&mut self, cx: &mut Context<Self>) {
        if self.status != self.rendered {
            cx.notify();
        }
    }

    /// The clock alone. Separated so the 1 Hz tick never touches the network,
    /// the battery or `wpctl`.
    fn refresh_clock(&mut self, cx: &mut Context<Self>) {
        self.status.clock = services::clock::format(Self::show_seconds(cx)).render(services::clock::now());
    }

    fn show_seconds(cx: &gpui::App) -> bool {
        cx.global::<services::AppState>().config.get().bar.show_seconds
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.refresh_clock(cx);

        let volume = services::volume::read();
        self.status.volume = volume.clone();

        if self.widgets.iter().any(|w| *w == widgets::Widget::Battery) {
            self.status.battery = services::battery::Battery::read();
        }

        if self.widgets.iter().any(|w| *w == widgets::Widget::Network) {
            let (interfaces, _) = self.rates.poll();
            let visible: Vec<_> = interfaces.iter().filter(|i| i.visible() && i.up).collect();
            self.status.network_up = !visible.is_empty();

            let signals = services::network::signals();
            self.status.network_signal = visible
                .first()
                .and_then(|i| signals.get(&i.name).copied());

            let rates = self.rates.last();
            self.status.network = if self.status.network_up {
                let up = services::network::format_rate(rates.tx_per_sec);
                let down = services::network::format_rate(rates.rx_per_sec);
                format!("{down} {up}")
            } else {
                String::new()
            };
        }
    }

    fn refresh_from(&mut self, compositor: &services::CompositorService) {
        // ArcSwap load: an atomic pointer read, never a lock.
        self.workspaces = compositor.workspaces().as_ref().clone();
        self.active = Some(compositor.active().id);
    }

    fn workspace_pips(&self, t: &crate::theme::Palette) -> Vec<Stateful<gpui::Div>> {
        self.workspaces
            .iter()
            .map(|ws| {
                let is_active = self.active == Some(ws.id);
                let id = ws.id;
                let hover_bg = if is_active {
                    t.ink_a(0.0)
                } else {
                    t.surface_hover()
                };

                div()
                    // Stable per-workspace id. Inside a loop this is not
                    // optional: without it every pip shares one global element
                    // id and the hit-test tree is rebuilt every frame.
                    // ElementId accepts only (&'static str, usize|u64|EntityId)
                    // tuples — there is no generic form, so cast.
                    .id(("lume-ws-pip", id as usize))
                    .h(px(20.0))
                    .min_w(px(20.0))
                    .px_1()
                    .rounded(px(services::theme::radius::ROW))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(services::theme::type_scale::BODY))
                    .cursor_pointer()
                    // The one move the whole design language is built on: the
                    // active row is an INVERTED pill — bone fill, paper text.
                    // It is what the reference's selected sidebar row does, and
                    // it reads as "chosen" without needing any colour at all.
                    // A tinted highlight would read as "enabled".
                    .bg(if is_active { t.bone_fill() } else { t.ink_a(0.0) })
                    .text_color(if is_active { t.ink_on_bone() } else { t.faint() })
                    // Only the inactive pips take a hover, so hovering the
                    // active one cannot suggest it can be made more active.
                    // Computed outside because the hover closure gets a
                    // `StyleRefinement`, which has no `when`.
                    .hover(|h| h.bg(hover_bg))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current(cx);
        // Pure. No mutation, no window calls, no cx.notify(), no surface
        // setters. All of that belongs in a handler.
        //
        // The snapshot of what was last rendered is taken here and nowhere
        // else, so "did this tick change anything" and "what is on screen" can
        // never disagree.
        self.rendered = self.status.clone();

        let (left, middle, right) = widgets::split_by_side(&self.widgets);

        // Three equal columns rather than justify_between with three children.
        //
        // With justify_between the middle group is only centred when the two
        // outer groups happen to be the same width, so the clock drifts left
        // or right as widgets are added and removed. Equal columns make the
        // middle region genuinely centred at any content width, which is the
        // entire reason it is its own region.
        //
        // Those equal columns are also where the two zone rules go. Because the
        // columns are exactly one third each, a border on the left edge of the
        // middle and right columns lands at 1/3 and 2/3 of the bar, and stays
        // there however wide the content is. The reference's ruled "system
        // dossier" look comes from these two hairlines and nothing else.
        let gap = px(0.0);
        let _ = gap;

        div()
            .id("lume-bar")
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .bg(t.paper_a(services::theme::alpha::SURFACE))
            .text_color(t.ink_a(1.0))
            .font_family(services::theme::type_scale::FAMILY)
            .text_size(px(services::theme::type_scale::BODY))
            // The hairline that separates the shell from the desktop. Without
            // it the bar reads as a band of wallpaper, not as a surface.
            .border_b_1()
            .border_color(t.line())
            // The accent spine. Two pixels of the theme's colour down the very
            // left edge: the cheapest possible statement of "this surface is
            // the shell", and the one place the accent is load-bearing rather
            // than decorative. It also gives the bar a left and a right — a
            // full-width rule with nothing at either end reads as a band of
            // wallpaper rather than as a surface.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(t.accent_a(1.0)),
            )
            // The launcher mark, at the far left of the identity region. Click
            // it and the shell starts doing things, which is why it is first and
            // why it is the one element allowed to be the accent colour: it is
            // the shell's own signature, and a theme switch has to be visible
            // somewhere.
            .child(
                div()
                    // An id is required before any handler: `on_click` makes
                    // the element stateful, and a stateful element must be
                    // addressable.
                    .id("lume-bar-mark")
                    .flex()
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .cursor_pointer()
                    .hover(|h| h.bg(t.surface_hover()))
                    .on_click(move |_, _w, _cx| {
                        Click::Launcher.send(0.0);
                    })
                    .child(identity_mark(&t)),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_l_1()
                    .border_color(t.line_soft())
                    .pl_3()
                    .children(left.iter().flat_map(|w| self.render_widget(w, &t))),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .border_l_1()
                    .border_color(t.line_soft())
                    .children(middle.iter().flat_map(|w| self.render_widget(w, &t))),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_3()
                    .border_l_1()
                    .border_color(t.line_soft())
                    .pl_3()
                    .children(right.iter().flat_map(|w| self.render_widget(w, &t))),
            )
            .into_any_element()
    }
}

impl BarView {
    /// Render one widget. `Option` because a widget can be unavailable on this
    /// machine right now.
    fn render_widget(
        &self,
        widget: &widgets::Widget,
        t: &crate::theme::Palette,
    ) -> Vec<gpui::AnyElement> {
        // Clickable when it leads somewhere. A widget with nothing behind it is
        // not clickable and does not pretend to be — the cursor is the tell,
        // and a pointer cursor over something that does nothing is a lie the
        // user has to discover by clicking.
        //
        // The clock is the exception to "drawer for every clickable widget": it
        // opens the launcher. It was a placeholder drawer saying "nothing to
        // configure yet", which is worse than no drawer — a time display is
        // what people click to start doing things, and REQUIREMENTS.md says so
        // explicitly.
        let click = match widget {
            widgets::Widget::Volume => Some(Click::Drawer(super::menu::MenuId::Volume)),
            widgets::Widget::Network => Some(Click::Drawer(super::menu::MenuId::Network)),
            widgets::Widget::Battery => Some(Click::Drawer(super::menu::MenuId::Battery)),
            widgets::Widget::Clock => Some(Click::Launcher),
            _ => None,
        };

        let cell = move |content: gpui::AnyElement| {
            let base = div()
                .id(("lume-bar-cell", hash_name(widget.name())))
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .rounded(px(services::theme::radius::ROW))
                .text_color(t.muted())
                .child(content);

            match click {
                None => base.into_any_element(),
                Some(a) => base
                    .cursor_pointer()
                    .hover(|h| h.bg(t.control_hover()))
                    .on_click(move |_, w, _cx| {
                        // The pointer is the most accurate answer to "which
                        // widget is this", and the popup is anchored to it.
                        // Without it the keybind path falls back to the right
                        // edge, which is what a keybind wants anyway.
                        let anchor_x = f32::from(w.mouse_position().x);
                        a.send(anchor_x);
                    })
                    .into_any_element(),
            }
        };

        // Workspaces first and out of the status match: its content is the
        // compositor's, not a `Status` field. Returning `None` from the match
        // and bailing there — which is what this did first — skipped the pips
        // entirely, because the early return came before the append.
        if *widget == widgets::Widget::Workspaces {
            return vec![div()
                .id("lume-bar-workspaces")
                .flex()
                .gap_1()
                .children(self.workspace_pips(t))
                .into_any_element()];
        }

        let Some(out) = (match widget {
            // The clock is the bar's anchor and the only thing at full ink.
            // Everything else is muted or faint, which is what makes the centre
            // read as the centre rather than as just another widget.
            widgets::Widget::Clock => Some(
                cell(div()
                    .text_size(px(services::theme::type_scale::CLOCK))
                    .text_color(t.ink_a(1.0))
                    .child(gpui::SharedString::from(self.status.clock.clone()))
                    .into_any_element()),
            ),
            widgets::Widget::Workspaces => None,
            widgets::Widget::Title => Some(cell(
                gpui::SharedString::from("lume").into_any_element(),
            )),
            // Status is deliberately one step down from the clock in both size
            // and contrast. Without that step every widget is the same weight
            // and the bar has no focal point — which is exactly how this looked
            // before: correct, and with nothing to look at.
            widgets::Widget::Network if self.status.network_up => Some(cell(
                div()
                    .text_size(px(services::theme::type_scale::SMALL))
                    .text_color(t.faint())
                    .child(gpui::SharedString::from(self.status.network.clone()))
                    .into_any_element(),
            )),
            widgets::Widget::Volume => self.status.volume.as_ref().map(|v| {
                cell(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(services::theme::type_scale::SMALL))
                        .text_color(t.faint())
                        .child(gpui::SharedString::from(format!("{}%", v.percent())))
                        .into_any_element(),
                )
            }),
            widgets::Widget::Battery => Some(cell(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(gpui::SharedString::from(
                        self.status
                            .battery
                            .capacity
                            .map(|p| format!("{p}%"))
                            .unwrap_or_default(),
                    ))
                    .into_any_element(),
            )),
            _ => None,
        }) else {
            return Vec::new();
        };

        vec![out]
    }
}

/// The launcher mark at the head of the bar.
///
/// Drawn rather than typed: the reference's identity card is a kanji inside a
/// bordered box, and that reads as a logo. `LU` typed in the body face would
/// read as two letters somebody typed.
///
/// The bracket corners are four 1px rules in the accent colour. They are the
/// cheapest available echo of the reference's framing motif, and they are what
/// makes a theme switch visible on the bar at a glance.
fn identity_mark(t: &crate::theme::Palette) -> gpui::AnyElement {
    let arm = 5.0;
    let a = t.accent_a(1.0);
    let box_size = px(20.0);

    // Four corner rules, positioned absolutely over a fixed-size box.
    let corner = |x: f32, y: f32, w: f32, h: f32| {
        div().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h)).bg(a)
    };

    div()
        .relative()
        .size(box_size)
        .flex()
        .items_center()
        .justify_center()
        // Sits inside the brackets, so it must be the brighter of the two.
        .text_color(t.ink_a(1.0))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .letter_spacing(px(services::theme::type_scale::TRACK_MARK))
        .child("LU")
        .child(corner(0.0, 0.0, arm, 1.0))
        .child(corner(0.0, 0.0, 1.0, arm))
        .child(corner(f32::from(box_size) - arm, 0.0, arm, 1.0))
        .child(corner(f32::from(box_size) - 1.0, 0.0, 1.0, arm))
        .child(corner(0.0, f32::from(box_size) - 1.0, arm, 1.0))
        .child(corner(0.0, f32::from(box_size) - arm, 1.0, arm))
        .child(corner(f32::from(box_size) - arm, f32::from(box_size) - 1.0, arm, 1.0))
        .child(corner(f32::from(box_size) - 1.0, f32::from(box_size) - arm, 1.0, arm))
        .into_any_element()
}

/// What a bar widget does when clicked.
#[derive(Clone, Copy)]
enum Click {
    /// Open a widget menu, anchored at the click.
    Drawer(super::menu::MenuId),
    /// Open the launcher. The anchor is discarded: the launcher is centred and
    /// has no cell to point at.
    Launcher,
}

impl Click {
    fn send(self, anchor_x: f32) {
        match self {
            Click::Drawer(m) => {
                services::intent::send(services::intent::Intent::Drawer {
                    widget: Some(m.name().to_string()),
                    anchor_x: Some(anchor_x),
                });
            }
            Click::Launcher => {
                services::intent::send(services::intent::Intent::Launcher(
                    services::intent::LauncherOp::Open,
                ));
            }
        }
    }
}

/// Stable element-id suffix for a widget name.
///
/// `ElementId` tuples take only `usize`, `u64` or `EntityId`, so a runtime
/// string has to be hashed — exactly as the workspace pips do.
fn hash_name(name: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    h.finish() as usize
}
