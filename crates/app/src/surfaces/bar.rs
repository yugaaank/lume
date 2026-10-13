//! The bar's content.
//!
//! The layer surface itself is owned by `surfaces::Surfaces` — this file only
//! knows what goes inside it.

use gpui::{
    App, Context, FocusHandle, Focusable, Render, Stateful, StatefulInteractiveElement, Window, div,
    prelude::*, px, rgb,
};
use services::compositor::Action;
use services::config::BarStyle;

use super::widgets;

/// Exclusive zone and height. The registry reads this for geometry.
/// Smallest bar that still fits one row of body text without clipping.
pub const MIN_HEIGHT: f32 = 28.0;

/// Largest bar. Past this the bar stops being a bar and starts eating the
/// screen, and the exclusive zone silently costs every window a strip.
pub const MAX_HEIGHT: f32 = 72.0;

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
    /// The drawn strip's thickness.
    ///
    /// Public because the surface reads it to build the bar's input region: a
    /// region that disagrees with what is painted swallows clicks below the bar,
    /// or leaves a dead band under it.
    pub height: f32,
    /// Resolved from `config.bar.style` once, beside the widgets it is read
    /// alongside. A shape is static data, not something to re-derive per frame.
    shape: BarShape,
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
        let height = config.bar.height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        let shape = BarShape::of(config.bar.style);
        let mut bar = Self {
            focus: cx.focus_handle(),
            workspaces: Vec::new(),
            active: None,
            widgets,
            height,
            shape,
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

        if self.widgets.iter().any(|w| w.key == "battery") {
            self.status.battery = services::battery::Battery::read();
        }

        if self.widgets.iter().any(|w| w.key == "network") {
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

    /// The workspace row: one connected strip, not a row of loose pills.
    ///
    /// A hairline on the leading edge of every cell but the first welds them
    /// into a single segmented control, which is what they are. Loose rounded
    /// pills read as unrelated buttons; a joined strip reads as one selector.
    ///
    /// The active cell carries a `//` prefix, borrowed from the reference's
    /// selected sidebar rows. It costs three characters and it is the single
    /// detail that makes the strip look considered rather than default.
    fn workspace_pips(&self, t: &crate::theme::Palette) -> Vec<gpui::AnyElement> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(i, ws)| {
                let is_active = self.active == Some(ws.id);
                let id = ws.id;
                let hover_bg = if is_active {
                    t.ink_a(0.0)
                } else {
                    t.surface_hover()
                };

                div()
                    // Stable per-workspace id. Inside a loop this is not
                    // optional: without it every cell shares one global element
                    // id and the hit-test tree is rebuilt every frame.
                    // ElementId accepts only (&'static str, usize|u64|EntityId)
                    // tuples — there is no generic form, so cast.
                    .id(("lume-ws-pip", id as usize))
                    .h(px(24.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .text_size(px(services::theme::type_scale::SMALL))
                    .cursor_pointer()
                    // The separator. Suppressed on the first cell so the strip
                    // has no left edge, and drawn on the leading edge of the
                    // rest so the weld runs the way the cells read.
                    .when(i > 0, |d| d.border_l_1().border_color(t.line_soft()))
                    // The one move the whole design language is built on: the
                    // active cell is an INVERTED pill — bone fill, paper text.
                    // It reads as "chosen" without needing any colour, which a
                    // tinted highlight cannot do: tint reads as "enabled".
                    .bg(if is_active { t.bone_fill() } else { t.ink_a(0.0) })
                    .text_color(if is_active { t.ink_on_bone() } else { t.faint() })
                    // Only inactive cells take a hover. Hovering the active one
                    // would suggest it can be made more active.
                    .hover(|h| h.bg(hover_bg))
                    .child(if is_active {
                        div()
                            .text_color(t.ink_on_bone())
                            .child("//")
                            .into_any_element()
                    } else {
                        div().into_any_element()
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
                    .into_any_element()
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
        // The two zone rules are the left borders of the middle and right
        // columns. Because the columns are equal they land at a third and two
        // thirds — measured at 647 and 1284 on a 1920px bar, so within half a
        // percent, the residue being the inter-column gap the thirds are taken
        // from. "Exactly a third" would be the wrong word; "a third, give or
        // take the gaps" is what it is.
        //
        // They were described here for a while before anything drew them.
        // Each zone is a *card*: a hairline box with a tracked caption on its
        // left edge and the widgets inside it. Three bordered boxes on one
        // black band read as instrumentation; the same widgets loose on the
        // band read as text somebody typed.
        //
        // The zones are still equal-width columns, so the centre one is
        // genuinely centred and its two rules land at 1/3 and 2/3 whatever the
        // content widths.
        // The surface is the whole output height so it never resizes; the bar
        // is a strip at the top of it. This div is layout only and paints
        // nothing — the strip below is the visible bar, and it is the strip the
        // surface's input region describes.
        div().size_full().child(
        // The shell. A full-width bar is the centring row's whole width; a
        // compact one shrinks to its content and this centres it. One renderer,
        // one conditional — which is the difference between a style being a
        // parameter and being a second bar.
        div()
            .h_full()
            .w_full()
            .flex()
            // Centre along the bar's own axis only. `items_center` works both
            // axes, which put a compact shell in the middle of the screen —
            // the surface is full height now, so cross-axis centring has
            // something to centre against.
            .justify_center()
            .items_start()
            .child(
        div()
            .id("lume-bar")
            .h(px(self.height))
            // Always full width. This was conditional on `compact` and that was
            // wrong twice over: the three columns are `flex_1`, which is basis 0
            // plus a share of the free space, so a shrink-to-fit shell has
            // nothing to share and renders empty — and a shell sized to its
            // content packs the three regions against each other instead of
            // putting them at the ends. The shapes differ in their backdrop,
            // never in whether the layout has room.
            .w_full()
            .mx(px(self.shape.inset))
            .flex()
            .gap_3()
            .items_center()
            .gap_3()
            // No horizontal padding here; the two outer zones carry it, so the
            // strip's content box is the full bar width.
            // The surface, when this shape has one. `Bubbles` has none: its
            // regions are the plates, and a surface behind them would make the
            // two shapes the same drawing.
            .when(self.shape.continuous, |d| {
                d.bg(t.paper_a(services::theme::alpha::SURFACE))
                    .rounded(px(self.shape.radius()))
            })
            .text_color(t.ink_a(1.0))
            .font_family(services::theme::type_scale::FAMILY)
            .text_size(px(services::theme::type_scale::BODY))
            // A floating shell is detached from the screen edge, so it is
            // outlined all the way round. A flush one has only a bottom edge
            // that is not against the desktop.
            .when(self.shape.floating(), |d| {
                d.border_1().border_color(t.line())
            })
            .when(!self.shape.floating(), |d| {
                d.border_b_1().border_color(t.line())
            })
            // The accent spine down the left edge, plus a matching tick at the
            // right. Two pixels of theme colour at each end: the bar is the
            // only always-present surface, so it is where the shell states
            // what it is, and the spine is where a theme switch has to be
            // visible.
            .child(div().absolute().left_0().top_0().bottom_0().w(px(2.0)).bg(t.accent_a(1.0)))
            .child(div().absolute().right_0().top_0().bottom_0().w(px(2.0)).bg(t.accent_a(1.0)))
            // Identity: the mark, then the workspace strip.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(self.shape.continuous, |d| d.pl_3())
                    // Bubbles has no surface behind it, so the regions are the
                    // plates. The plate is an inner element rather than the
                    // region itself: a region is a third of the bar, and a
                    // bordered third is a bordered third, not a plate around
                    // some widgets. The region keeps the third so the clock
                    // stays centred; the plate shrinks to what is in it.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!self.shape.continuous, |d| {
                                d.rounded(px(services::theme::radius::TILE))
                                    .border_1()
                                    .border_color(t.line())
                                    .px_2()
                                    .py_1()
                            })
                            .child(
                        div()
                            .id("lume-bar-mark")
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .h(px(24.0))
                            .rounded(px(services::theme::radius::ROW))
                            .border_1()
                            .border_color(t.line())
                            .cursor_pointer()
                            .hover(|h| h.bg(t.surface_hover()))
                            .on_click(move |_, _w, _cx| Click::Launcher.send(0.0))
                            .child(identity_mark(&t))
                            .child(
                                // The mark's caption. Present rather than
                                // decorative: the reference labels every region
                                // of its interface, and an unlabelled box is a
                                // box with no idea what it is.
                                div()
                                    .text_size(px(services::theme::type_scale::TINY))
                                    .text_color(t.faint())
                                    .letter_spacing(px(services::theme::type_scale::TRACK_MARK))
                                    .child("LUME"),
                            ),
                    )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .children(self.workspace_pips(&t)),
                            ),
                    ),
            )
            // Centre: the clock, boxed so it reads as the primary instrument.
            //
            // The left border is the rule at 1/3. It lands there because the
            // columns are equal, which is the one thing the layout is for — the
            // comment above says so, and until now nothing drew it.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // The rule at 1/3, which only means anything where the
                    // columns are thirds of the bar. A compact shell packs its
                    // regions, so there is no third to rule.
                    .when(self.shape.continuous, |d| {
                        d.border_l_1().border_color(t.line())
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .when(!self.shape.continuous, |d| {
                                d.rounded(px(services::theme::radius::TILE))
                                    .border_1()
                                    .border_color(t.line())
                                    .px_3()
                                    .py_1()
                            })
                            .children(middle.iter().flat_map(|w| self.render_widget(w, &t))),
                    ),
            )
            // Status. Its left border is the rule at 2/3.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .when(self.shape.continuous, |d| {
                        d.border_l_1().border_color(t.line())
                    })
                    .when(self.shape.continuous, |d| d.pr_3())
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!self.shape.continuous, |d| {
                                d.rounded(px(services::theme::radius::TILE))
                                    .border_1()
                                    .border_color(t.line())
                                    .px_2()
                                    .py_1()
                            })
                            .children(right.iter().flat_map(|w| self.render_widget(w, &t))),
                    ),
            ),
        )
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
        // What a click does, asked of the registry rather than of a table here.
        //
        // The widget says which popup it owns; this says how to open it. A
        // widget with no popup is simply not clickable, and a new widget gets a
        // click behaviour by declaring a popup id — not by being added to a
        // match somewhere else.
        //
        // The clock is the exception: it opens the launcher. It was a
        // placeholder drawer saying "nothing to configure yet", which is worse
        // than no drawer — a time display is what people click to start doing
        // things.
        let click = match widget.popup.map(|p| p.id) {
            // The clock declares a popup in the registry because it is a popup
            // *slot* like any other, but its content is the launcher, so the id
            // is not consulted here.
            Some("clock") => Some(Click::Launcher),
            Some(id) => super::menu::MenuId::parse(id).map(Click::Drawer),
            None => None,
        };

        let cell = move |content: gpui::AnyElement| {
            let base = div()
                .id(("lume-bar-cell", hash_name(widget.key)))
                .flex()
                .items_center()
                .gap_1()
                // Per-widget, from the registry. A digit and a wordmark want
                // different amounts of room and one global gap makes both look
                // slightly wrong — which is what it did.
                .px(px(widget.pad))
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
        if widget.key == "workspaces" {
            return vec![div()
                .id("lume-bar-workspaces")
                .flex()
                .gap_1()
                .children(self.workspace_pips(t))
                .into_any_element()];
        }

        let Some(out) = (match widget.key {
            // The clock is the bar's anchor and the only thing at full ink.
            // Everything else is muted or faint, which is what makes the centre
            // read as the centre rather than as just another widget.
            "clock" => Some(
                cell(div()
                    .text_size(px(services::theme::type_scale::CLOCK))
                    .text_color(t.ink_a(1.0))
                    .child(gpui::SharedString::from(self.status.clock.clone()))
                    .into_any_element()),
            ),
            "workspaces" => None,
            "title" => Some(cell(
                gpui::SharedString::from("lume").into_any_element(),
            )),
            // Status is a caption over a value, in a hairline box.
            //
            // Stacking them is what buys the bar its vertical room, and the
            // reference does exactly this in its quick settings tiles: a tracked
            // micro label over the live number. It also fixes the flatness that
            // made the first version look like text somebody typed — a bare
            // "25%" says nothing about what is at 25%.
            "network" if self.status.network_up => Some(tile(
                "NET",
                self.status.network.clone(),
                t,
            )),
            "volume" => self.status.volume.as_ref().map(|v| {
                tile("VOL", format!("{}%", v.percent()), t)
            }),
            "battery" => Some(cell(
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

/// A status readout: a tracked caption over a value, inside a hairline box.
///
/// The box is what makes the bar look like instrumentation rather than like a
/// row of numbers. It costs one border and it is the difference between the two.
fn tile(caption: &'static str, value: String, t: &crate::theme::Palette) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .px_3()
        .h_full()
        .border_1()
        .border_color(t.line_soft())
        .child(
            div()
                .text_size(px(services::theme::type_scale::TINY))
                .text_color(t.faint())
                .letter_spacing(px(services::theme::type_scale::TRACK_MARK))
                .child(caption),
        )
        .child(
            div()
                .text_size(px(services::theme::type_scale::SMALL))
                .text_color(t.ink_a(1.0))
                .child(gpui::SharedString::from(value)),
        )
        .into_any_element()
}

/// How one bar draws, for a given [`BarStyle`].
///
/// The style is a *shape*, not a different bar. Ryoku puts it the same way —
/// `barShellStyle` is a property on one `BarSlot`, five values, one renderer —
/// which is why each of its styles costs a parameter rather than an
/// implementation. Here it is three booleans.
///
/// `Islands` is deliberately absent. Clusters on different screen edges are the
/// four-edge frame: a different surface model, not a shape. It used to be a
/// variant that fell back to `Single` and logged, which is worse than not
/// offering it at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarShape {
    /// Gap between the shell and the left and right screen edges. Zero is a bar
    /// flush with the output, which is what `Single` is.
    pub inset: f32,
    /// One continuous surface behind the whole strip. When false, each region is
    /// its own rounded plate instead.
    pub continuous: bool,
}

impl BarShape {
    pub fn of(style: BarStyle) -> Self {
        match style {
            // Flush with the output, one surface, no visible corners.
            BarStyle::Single => Self { inset: 0.0, continuous: true },
            // Flush, no surface: the regions are the plates.
            BarStyle::Bubbles => Self { inset: 0.0, continuous: false },
            // Lifted off both edges and fully rounded, one surface.
            BarStyle::Framed => Self { inset: 10.0, continuous: true },
        }
    }

    /// Detached from the screen edge, so the shell has corners worth rounding.
    fn floating(&self) -> bool {
        self.inset > 0.0
    }

    /// Radius for the shell. A bar flush against the screen edge has no visible
    /// corners to round, so it keeps none.
    fn radius(&self) -> f32 {
        if self.floating() {
            services::theme::radius::CARD
        } else {
            services::theme::radius::CARET
        }
    }
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
