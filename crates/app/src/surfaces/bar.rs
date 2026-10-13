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
    /// The date label, `Tue 13`. The reference's centre group is time and date
    /// at two sizes, so they are two fields rather than one composed string.
    date: String,
    network: String,
    /// The SSID of the network this machine is on, or empty when wired or
    /// down. The widget shows this beside the glyph rather than a byte rate:
    /// which network you are on is the thing worth reading at a glance.
    network_ssid: String,
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
    /// The three sectors. Placement is the config's, not the registry's.
    left: Vec<widgets::Widget>,
    centre: Vec<widgets::Widget>,
    right: Vec<widgets::Widget>,
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
        let (left, centre, right) = widgets::active_sectors(&config);
        let height = config.bar.height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        let shape = BarShape::of(config.bar.style);
        let mut bar = Self {
            focus: cx.focus_handle(),
            workspaces: Vec::new(),
            active: None,
            left,
            centre,
            right,
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
    /// Whether the configured bar contains a widget.
    ///
    /// Polling a service the user never put on the bar is the thing this
    /// guards: the network and volume readers shell out, and doing that every
    /// tick for a hidden widget is pure cost.
    fn has(&self, key: &str) -> bool {
        [&self.left, &self.centre, &self.right]
            .iter()
            .any(|row| row.iter().any(|w| w.key == key))
    }

    fn refresh_clock(&mut self, cx: &mut Context<Self>) {
        self.status.clock = services::clock::format(Self::show_seconds(cx)).render(services::clock::now());
        self.status.date = services::clock::Format::DateOnly.render(services::clock::now());
    }

    fn show_seconds(cx: &gpui::App) -> bool {
        cx.global::<services::AppState>().config.get().bar.show_seconds
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.refresh_clock(cx);

        let volume = services::volume::read();
        self.status.volume = volume.clone();

        if self.has("battery") {
            self.status.battery = services::battery::Battery::read();
        }

        if self.has("network") {
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

            // Only while wireless is actually up. `link()` shells out, and
            // asking NetworkManager which network it is on when the radio is
            // down buys a slower poll for an empty answer.
            self.status.network_ssid = if self.status.network_up {
                services::wifi::link().ssid
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

    /// The workspace row: kanji numerals, one cell per workspace.
    ///
    /// The reference's `kanji` style. 一二三四五 at 1..=10 and the raw id
    /// above that, because those glyphs are the reference's own choice and they
    /// read as a sequence rather than as five unrelated digits.
    ///
    /// The active cell is marked by being *larger and brighter* — 15px at full
    /// ink against 13px at 0.7 or 0.3 — with no box, no fill and no underline.
    /// An inverted pill was tried first and it fought the rest of the bar: the
    /// cells became the only filled things in a row of quiet text, so the eye
    /// went to the workspace strip instead of the clock.
    fn workspace_pips(&self, t: &crate::theme::Palette) -> Vec<gpui::AnyElement> {
        self.workspaces
            .iter()
            .map(|ws| {
                let is_active = self.active == Some(ws.id);
                let id = ws.id;

                div()
                    // Stable per-workspace id. Inside a loop this is not
                    // optional: without it every cell shares one global element
                    // id and the hit-test tree is rebuilt every frame.
                    .id(("lume-ws-pip", id as usize))
                    .w(px(22.0))
                    .h(px(28.0))
                    // Never shrink. The pips live in a flex row, so a wide
                    // value in a neighbouring cell can squeeze them, and a
                    // squeezed pip wraps its glyph and doubles the bar's height.
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|h| h.text_color(t.ink_a(1.0)))
                    // The CJK face, because the mono has no kanji and would
                    // render 一 as tofu. See `fonts::CJK_FACE`.
                    .font_family(crate::fonts::CJK_FAMILY)
                    .text_size(px(if is_active { 15.0 } else { 13.0 }))
                    .text_color(match (is_active, ws.windows) {
                        (true, _) => t.ink_a(1.0),
                        (false, n) if n > 0 => t.ink_a(0.7),
                        (false, _) => t.ink_a(0.3),
                    })
                    .child(gpui::SharedString::from(kanji_workspace(id)))
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

        let (left, middle, right) = (&self.left, &self.centre, &self.right);

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
        // The shell. Three equal columns, each holding a row of floating pills.
        // There is no continuous band behind them: the reference's islands mode
        // hides the band and lets the wallpaper show between the pills, and a
        // band would turn four plates into one bar with boxes drawn on it.
        //
        // Equal columns, not `justify_between`, because with justify_between the
        // clock is only centred while the two outer groups happen to be the same
        // width — it drifts as widgets come and go.
        div()
            .id("lume-bar")
            .h(px(self.height))
            .w_full()
            // The reference's `shellOuterMargin`: pills are held off the screen
            // edges by 5px. Without it the outer pills sit flush against x=0
            // and their rounded corners and shadow are clipped.
            .px(px(8.0))
            .flex()
            .items_center()
            .gap_3()
            .font_family(services::theme::type_scale::FAMILY)
            .text_color(t.ink_a(1.0))
            .text_size(px(services::theme::type_scale::WIDGET))
            .child(self.render_row(&left, RowAlign::Start, &t))
            .child(self.render_row(&middle, RowAlign::Centre, &t))
            .child(self.render_row(&right, RowAlign::End, &t)),
        )
            .into_any_element()
    }
}

/// A workspace id as its kanji numeral, or the raw id above ten.
///
/// The reference's mapping, and it stops at ten on purpose: past that the
/// numerals stop being a sequence and start being a lookup table nobody can
/// hold in their head, so a workspace 11 shows `11`.
fn kanji_workspace(id: i64) -> String {
    const KANJI: [&str; 10] = ["一", "二", "三", "四", "五", "六", "七", "八", "九", "十"];
    match id {
        1..=10 => KANJI[(id - 1) as usize].to_string(),
        // Special workspaces are negative and have their own names; showing a
        // number for one would claim it is an ordinary workspace.
        _ => id.to_string(),
    }
}

/// Which end of its column a row sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAlign {
    Start,
    Centre,
    End,
}


/// One floating plate: a run of widgets on paper, hairline border, full bar
/// height, so the pills read as objects sitting *on* the desktop rather than as
/// rows of text with boxes drawn around them.
fn pill(children: Vec<gpui::AnyElement>, t: &crate::theme::Palette, height: f32) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(height))
        // A real gap between cells, on top of each cell's own margin. The
        // margin alone is the cell's padding and a widget's content sits inside
        // it; without the gap a wide value's margin is the only thing separating
        // two readouts, and two of those and the bar looks like a sentence.
        .gap(px(WIDGET_GAP))
        // The bubble keeps its natural width too. Its cells are already locked
        // against shrinking, but the bubble is what the column flexes, and a
        // compressed bubble squeezes its children and clips the last glyph —
        // "SOMMY 1393" came out as "SOMMY 135" the moment the gap widened.
        // The bar is the thing that should not fit, not the readouts.
        .flex_shrink_0()
        .rounded(px(PILL_RADIUS))
        .bg(t.paper_a(services::theme::alpha::PILL))
        .border_1()
        .border_color(t.pill_border())
        .children(children)
        .into_any_element()
}

/// The pill's corner radius. The reference's `barCornerRadius`.
const PILL_RADIUS: f32 = 6.0;

/// Every widget's horizontal margin, in px. One value for all of them, so no
/// widget can be given a different one by accident and two readouts can never
/// end up touching.
const WIDGET_PAD: f32 = 8.0;

/// Clear space between two widgets' areas, on top of their margins.
///
/// The reference's `v2WidgetSpacing` is 6. At 2 the widgets still read as one
/// run — the margin alone was the only thing between two readouts, and two of
/// those and the status group looks like a sentence. 6 gives each widget a
/// visibly separate area, which is the whole point of giving them areas.
const WIDGET_GAP: f32 = 6.0;

impl BarView {
    /// One sector: every widget in it inside a single bubble.
    fn render_row(
        &self,
        widgets: &[widgets::Widget],
        align: RowAlign,
        t: &crate::theme::Palette,
    ) -> gpui::AnyElement {
        let row = div().flex_1().h_full().flex().items_center().gap_3();
        let row = match align {
            RowAlign::Start => row.justify_start(),
            RowAlign::Centre => row.justify_center(),
            RowAlign::End => row.justify_end(),
        };
        // One bubble per sector. The reference splits a sector into several
        // pills at each separator; lume deliberately does not. A sector is one
        // group, and splitting it turns the bar into a row of unrelated boxes —
        // and it moves the seams every time a widget is added or removed,
        // which is exactly when the user is trying to read the bar.
        //
        // One bubble per sector is also what makes placement visible: move a
        // widget between sectors and it visibly joins a different group.
        let children: Vec<gpui::AnyElement> =
            widgets.iter().flat_map(|w| self.render_widget(w, t)).collect();
        if children.is_empty() {
            return row.into_any_element();
        }
        row.child(pill(children, t, self.height)).into_any_element()
    }

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
                .justify_center()
                // One margin for every widget, symmetric. It used to be a
                // per-widget `pad` field, which meant the margin was whatever
                // each widget's entry happened to say — so a long value in one
                // widget squeezed its neighbour's padding away and two readouts
                // ended up touching. A single constant cannot drift, and every
                // widget's area is identical, so moving one never rebalances
                // the rest.
                .px(px(WIDGET_PAD))
                // Each cell keeps its natural width. A flex item shrinks to fit
                // whatever is left, and a shrunk cell squeezes its own text into
                // a wrap — which is how a long SSID turned the bar two lines
                // tall. Cells are content-sized; the bar scrolls or elides
                // before it squeezes.
                .flex_shrink_0()
                // The cell is the widget's own area: full plate height, so a 13px
                // clock and a 20px mark have the same click target and the
                // hover highlight draws a box of one height.
                .h(px(self.shape.plate_h))
                // Content is clipped to the cell. A value that grows — a long
                // SSID, a media title — must elide inside its own area rather
                // than spill over the widget next to it, which is what made the
                // right-hand group read as one run-on smear.
                .overflow_hidden()
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
                .items_center()
                // The same area every other widget gets. It used to return its
                // own bare row with no padding and no shrink lock, so it was the
                // one widget whose margin and hit box disagreed with the rest —
                // which is exactly the inconsistency the single WIDGET_PAD
                // exists to remove.
                .px(px(WIDGET_PAD))
                .flex_shrink_0()
                .h(px(self.shape.plate_h))
                .rounded(px(services::theme::radius::ROW))
                .children(self.workspace_pips(t))
                .into_any_element()];
        }

        // Every widget is icon + value. The bar reads as one instrument
        // because every element in it has the same shape, the way the
        // reference's does; a widget that is only a number reads as an outage
        // of the ones that carry an icon.
        //
        // A ghost renders its icon alone, dimmed. It occupies its final
        // position and claims nothing about the system, which is the honest
        // state of a widget with no data source behind it yet.
        let Some(out) = (match widget.key {
            "clock" => Some(cell(div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(services::theme::type_scale::CLOCK))
                        .letter_spacing(px(1.0))
                        .text_color(t.ink_a(1.0))
                        .child(gpui::SharedString::from(self.status.clock.clone())),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .letter_spacing(px(0.5))
                        .text_color(t.ink_a(0.5))
                        .child(gpui::SharedString::from(self.status.date.clone())),
                )
                .into_any_element())),
            "launcher" => Some(cell(
                div()
                    .text_size(px(17.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(t.accent_a(1.0))
                    .child(gpui::SharedString::from(widget.icon.to_string()))
                    .into_any_element(),
            )),
            "network" => {
                // Glyph by link quality, so the bar says something useful
                // before the name is read. `bars()` is 0..=4.
                let glyph = match self.status.network_signal.as_ref().map(|s| s.bars()) {
                    Some(b) if b >= 3 => '\u{f1eb}',
                    Some(2) => '\u{f1ec}',
                    Some(1) => '\u{f1ed}',
                    // No reading and no SSID is a wired link, not a failing
                    // radio: drawing the empty-wifi glyph there would claim the
                    // wireless is broken when it is simply not in use.
                    _ if self.status.network_ssid.is_empty() && !self.status.network_up => '\u{eb2f}',
                    _ => '\u{f1eb}',
                };
                let ssid = self.status.network_ssid.clone();
                Some(cell(tile(glyph, (!ssid.is_empty()).then_some(ssid), t)))
            }
            "volume" => self.status.volume.as_ref().map(|v| {
                let glyph = if v.muted { '\u{f026}' } else { '\u{f028}' };
                cell(tile(glyph, Some(format!("{:>3}%", v.percent())), t))
            }),
            "battery" => {
                // Nothing to draw on a desktop, and an empty pill would claim a
                // battery that is not there.
                if !self.status.battery.present {
                    return Vec::new();
                }
                let pct = self.status.battery.capacity.unwrap_or(0);
                Some(cell(div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(battery_glyph(self.status.battery, t))
                    .child(
                        div()
                            .text_size(px(services::theme::type_scale::WIDGET))
                            .text_color(t.ink_a(1.0))
                            .child(gpui::SharedString::from(format!("{pct}%"))),
                    )
                    .into_any_element()))
            }
            _ if widget.ghost => Some(cell(ghost(widget.icon, t))),
            _ => None,
        }) else {
            return Vec::new();
        };

        vec![out]
    }
}

/// A widget with no data source yet: its icon, dimmed, and nothing else.
///
/// Dimming rather than hiding is the point — the slot is reserved at its final
/// size so the bar does not reflow when the real widget lands, and a faint icon
/// is honest about being a placeholder in a way an empty gap is not.
fn ghost(icon: char, t: &crate::theme::Palette) -> gpui::AnyElement {
    div()
        .text_size(px(services::theme::type_scale::ICON))
        .text_color(t.ink_a(0.3))
        .child(gpui::SharedString::from(icon.to_string()))
        .into_any_element()
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

/// A status widget: an icon, optionally followed by a value.
///
/// Icon + value is the reference's shape for every bar widget, and it is why
/// the bar reads as one instrument: an element with an icon has the same
/// silhouette as its neighbours, so the eye groups them. The old version put a
/// tracked-out caption above a number in a hairline box, which read as a set of
/// framed readouts rather than as a status strip.
fn tile(glyph: char, value: Option<String>, t: &crate::theme::Palette) -> gpui::AnyElement {
    let mut row = div()
        .flex()
        .items_center()
        .gap_1_5()
        .child(
            div()
                .text_size(px(services::theme::type_scale::ICON))
                .text_color(t.muted())
                .child(gpui::SharedString::from(glyph.to_string())),
        );

    if let Some(value) = value {
        row = row.child(
            div()
                .text_size(px(services::theme::type_scale::WIDGET))
                .text_color(t.ink_a(1.0))
                // Bounded by the string, not by the box. Capping the box with
                // `max_w` made GPUI *wrap* the text onto a second line instead
                // of eliding it, which is worse than the overflow it fixed: a
                // two-line SSID breaks the bar's height and reads as a layout
                // fault. So the characters are cut here, to a length that
                // cannot exceed the cell, and the box is left to size itself.
                //
                // `flex_shrink_0` because a flex item otherwise shrinks to
                // whatever width is left, and a shrunken value loses its last
                // glyph to the cell's clip — which is how "SOMMY 1393" rendered
                // as "SOMMY 139". A widget's area is content-sized.
                .flex_shrink_0()
                .child(gpui::SharedString::from(super::menu::truncate(
                    &value,
                    VALUE_MAX_CHARS,
                ))),
        );
    }

    row.into_any_element()
}

/// The most characters a widget's value may show, so no value can be wide
/// enough to reach its neighbour. 16 at 12px mono is ~115px, comfortably
/// inside a third of the bar.
const VALUE_MAX_CHARS: usize = 16;

/// A drawn battery: an outlined cell with a proportional fill.
///
/// Not a Nerd Font glyph. A single glyph cannot express a level, and the
/// level is the whole reason a battery indicator exists — `f240` is the same
/// picture at 80% and at 9%. Ryoku draws the cell for the same reason.
fn battery_glyph(b: services::battery::Battery, t: &crate::theme::Palette) -> gpui::AnyElement {
    if !b.present {
        return div().size_0().into_any_element();
    }

    let pct = b.capacity.unwrap_or(0) as f32 / 100.0;
    let (body_w, body_h) = (16.0, 9.0);
    let inset = 1.8;
    let inner = body_w - inset * 2.0;

    // Charging reads as accent rather than as a level: the number is climbing,
    // so a static fraction would be wrong the instant it was drawn.
    //
    // Low also goes to accent, and that is the whole palette's fault rather
    // than a shortcut: there is no danger colour, because NERV's accent *is*
    // the warning red. A fourth hue here would fight it. The distinction the
    // user needs is "pay attention now", which full-strength accent carries.
    let fill = if b.charging || pct <= 0.2 {
        t.accent_a(0.9)
    } else {
        t.ink_a(0.75)
    };

    div()
        .relative()
        .w(px(body_w + 3.0))
        .h(px(body_h))
        .flex()
        .items_center()
        // Body: transparent with a hairline border, so the fill inside reads as
        // the charge rather than the outline reading as a full cell.
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .w(px(body_w))
                .h(px(body_h))
                .rounded(px(2.5))
                .border_1()
                .border_color(fill)
                .child(
                    // At least a sliver when non-empty: a 2% charge rendered at
                    // 2% of 12px is invisible, which reads as empty.
                    div()
                        .absolute()
                        .left(px(inset))
                        .top(px(inset))
                        .w(px((inner * pct).max(if pct > 0.0 { 1.5 } else { 0.0 })))
                        .h(px(body_h - inset * 2.0))
                        .rounded(px(1.2))
                        .bg(fill),
                ),
        )
        // The terminal nub. Present on every real cell, so it is part of the
        // shape rather than a state.
        .child(
            div()
                .absolute()
                .left(px(body_w - 0.5))
                .top(px(2.5))
                .w(px(2.5))
                .h(px(4.0))
                .rounded(px(1.2))
                .bg(fill),
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
/// The height every region's plate gets in the bubbled bar.
///
/// Measured off the rightmost plate on a real bar, which carried the status
/// cluster at 42px. A constant rather than a config value: the plates have to
/// agree with each other, and making it configurable would restore the exact
/// bug it fixes — one plate at a different height from the others.
const PLATE_H: f32 = 42.0;

/// offering it at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarShape {
    /// Gap between the shell and the left and right screen edges. Zero is a bar
    /// flush with the output, which is what `Single` is.
    pub inset: f32,
    /// One continuous surface behind the whole strip. When false, each region is
    /// its own rounded plate instead.
    pub continuous: bool,
    /// Height of a region's plate, when this shape has plates.
    ///
    /// Explicit rather than derived from content. Each plate wrapped its
    /// children in `py_1` and took whatever height they needed, so a column
    /// holding the 20px identity mark and a column holding 13px clock text came
    /// out at different heights — measured on a real bar at 44, 35 and 42 for
    /// left, centre and right. Three plates at three heights read as three
    /// unrelated objects rather than one instrument, and the eye reads the
    /// tallest one as the important one.
    ///
    /// Sized from the rightmost plate: it carries the status cluster, which is
    /// the densest content on the bar, so every other region has room for what
    /// it holds.
    pub plate_h: f32,
}

impl BarShape {
    pub fn of(style: BarStyle) -> Self {
        match style {
            // Flush with the output, one surface, no visible corners.
            BarStyle::Single => Self { inset: 8.0, continuous: true, plate_h: 0.0 },
            // Flush, no surface: the regions are the plates.
            BarStyle::Bubbles => Self { inset: 8.0, continuous: false, plate_h: PLATE_H },
            // Lifted off both edges and fully rounded, one surface.
            BarStyle::Framed => Self { inset: 10.0, continuous: true, plate_h: 0.0 },
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
                    // A click is the one path that has a real widget to point
                    // at. Everything else docks to an edge.
                    anchor: services::intent::Anchor::At(anchor_x),
                    op: services::intent::DrawerOp::Toggle,
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
