//! The launcher: fuzzy application search with an inline calculator.
//!
//! ## Why this exists
//!
//! Without it the only way to start an application is to pin it in the dock.
//! That makes the shell operable only for the person who built it, and the
//! two-user requirement — naive users must be able to click through to
//! anything — has no path at all for anything unpinned.
//!
//! ## Keys, not a text input element
//!
//! The query is built from `KeyDownEvent`s rather than from GPUI's input
//! element. An input element wants focus of its own, a change event, and a
//! cursor, none of which this needs: the query is an append-only string with
//! one caret at the end, and it is rebuilt from scratch on every keystroke
//! anyway. Taking raw keys means the whole search field is ~20 lines of
//! `div().child(...)` with no second focus handle to reconcile with the
//! overlay's.
//!
//! ## The calculator shares the query
//!
//! One box, because that is what every launcher does and because a separate
//! box needs a mode switch that a naive user will never find. `2+2*3` shows
//! `8` and lists the apps matching `2+2*3` at once. Nothing is ambiguous,
//! because a query that does not parse as arithmetic simply has no result to
//! show — see `services::calc`.
//!
//! ## Selection is an index into the visible hits
//!
//! Not an app id. The list is recomputed on every keystroke, so an id-based
//! selection survives a query change and then points at a row the user cannot
//! see. An index cannot outlive the list it indexes.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString,
    Window, div, prelude::*, px, rgb,
};
use services::{apps::AppEntry, calc, launcher};

use super::background;

/// Height of one result row. Fixed so `VISIBLE_ROWS` rows is a known height
/// and the selection can be scrolled to without measuring anything.
const ROW: f32 = 44.0;

/// How many rows to draw. Enough to see there is more; few enough that the
/// list never reaches the bottom of a short screen.
const VISIBLE_ROWS: usize = 8;

pub struct Launcher {
    focus: FocusHandle,
    apps: Arc<services::apps::AppIndex>,
    query: String,
    hits: Vec<launcher::Hit>,
    selected: usize,
    /// The computed result, when the query is arithmetic.
    calculation: Option<f64>,
    on_launch: Option<Arc<dyn Fn(&AppEntry) + Send + Sync>>,
    on_close: Option<Arc<dyn Fn(&mut App) + Send + Sync>>,
}

impl Launcher {
    pub fn new(apps: Arc<services::apps::AppIndex>, cx: &mut Context<Self>) -> Self {
        let mut me = Self {
            focus: cx.focus_handle(),
            apps,
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            calculation: None,
            on_launch: None,
            on_close: None,
        };
        // Populated immediately: the launcher opened with no typing must show
        // something. An empty list here reads as "the search found nothing",
        // which is a different and wrong answer from "type to search".
        me.refresh();
        me
    }

    pub fn set_on_launch(&mut self, f: Arc<dyn Fn(&AppEntry) + Send + Sync>) {
        self.on_launch = Some(f);
    }

    pub fn set_on_close(&mut self, f: Arc<dyn Fn(&mut App) + Send + Sync>) {
        self.on_close = Some(f);
    }

    /// Recompute the hits and the calculator result.
    ///
    /// One function so the two can never disagree. An earlier shape computed
    /// hits on keypress and the result during render, which meant the number
    /// beside the cursor and the list under it were derived from different
    /// moments in time.
    fn refresh(&mut self) {
        let entries = self.apps.all();
        self.hits = launcher::search(&entries, &self.query, launcher::LIMIT);
        self.calculation = calc::evaluate(&self.query);
        // Clamped rather than reset: shrinking the list below the cursor must
        // not silently move the selection back to the top.
        self.selected = self.selected.min(self.hits.len().saturating_sub(1));
    }

    /// Handle a key. Returns whether it was consumed.
    ///
    /// Consumed keys matter because the overlay's own handler sits upstream:
    /// a keystroke the launcher does not understand must not fall through to
    /// whatever the overlay does with it.
    pub fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let key = event.keystroke.key.as_str();
        let lower = key.to_ascii_lowercase();

        match lower.as_str() {
            "down" | "ctrl-n" | "tab" => {
                self.move_selection(1);
            }
            "up" | "ctrl-p" | "shift-tab" => {
                self.move_selection(-1);
            }
            "return" | "enter" => {
                self.launch_selected(cx);
            }
            "backspace" => {
                self.query.pop();
                self.selected = 0;
                self.refresh();
            }
            "escape" => return false, // the overlay owns this
            // A modifier chord that happens to contain a printable key must
            // not type it. Ctrl+C closing the launcher is standard enough
            // that treating it as the letter "c" is worse than not handling it.
            _ if event.keystroke.modifiers.control || event.keystroke.modifiers.alt => {
                return false;
            }
            _ => {
                // `keystroke.keystroke` is the resolved character, which is not
                // always the key name: Shift+6 is "6", not "exclamation". Using
                // the key name here is why the previous version could not type
                // a capital letter.
                let Some(ch) = event.keystroke.key_char.as_deref().and_then(|k| k.chars().next()) else {
                    return false;
                };
                // A control character in the query is never wanted, and
                // `keystroke.chars()` can hand one back for keys like Delete.
                if ch.is_control() {
                    return false;
                }
                self.query.push(ch);
                self.selected = 0;
                self.refresh();
            }
        }
        cx.notify();
        true
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len() as isize;
        // Wraps. A list that stops at the end makes holding Down feel broken,
        // and the launcher is a list you flick through rather than read to a
        // stop.
        self.selected = (((self.selected as isize + delta) % n + n) % n) as usize;
    }

    fn launch_selected(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(self.selected).cloned() else {
            return;
        };
        tracing::info!(id = %hit.entry.id, "launcher: launching");
        if let Some(f) = &self.on_launch {
            f(&hit.entry);
        }
        self.close(cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = self.on_close.take() {
            f(cx);
        }
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let panel_w = px(640.0);
        let list_h = px(ROW * VISIBLE_ROWS as f32);

        div()
            .id("lume-launcher")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            // 12% down, not dead centre: the list grows downward from the
            // query, so centring the whole thing puts the query above the
            // screen's middle and the results below it.
            .pt(viewport.height * 0.12)
            .child(
                div()
                    .w(panel_w)
                    .rounded(px(14.0))
                    .backdrop_blur(px(24.0))
                    // Near-opaque, for the same reason the menu is: this is a
                    // panel you read while a window animates behind it.
                    .bg(gpui::hsla(0.10, 0.05, 0.09, 0.985))
                    .border_1()
                    .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.12))
                    .shadow_lg()
                    .child(self.render_query())
                    .child(
                        div()
                            // An id is required, not decorative: the scroll
                            // methods register per-element state keyed by it,
                            // and without one the call does not exist.
                            .id("lume-launcher-scroll")
                            .h(list_h)
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(self.hits.iter().enumerate().map(|(i, h)| {
                                render_row(i, self.selected == i, h, cx.entity())
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Launcher {
    fn render_query(&self) -> gpui::AnyElement {
        let placeholder = self.query.is_empty();
        // The caret is a real block, not a blinking one. A blinking caret needs
        // a timer to drive it, which means a redraw loop running for as long
        // as the launcher is open; a static block reads as focused and costs
        // nothing.
        let field = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .h(px(56.0))
            .border_b_1()
            .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.10))
            .child(
                div()
                    .flex_1()
                    .text_size(px(18.0))
                    .text_color(rgb(0xf0f0f0))
                    .child(if placeholder {
                        div().text_color(gpui::hsla(0.0, 0.0, 1.0, 0.35)).child("Search applications…")
                    } else {
                        div().child(self.query.clone())
                    }),
            )
            .child(
                div()
                    .w(px(2.0))
                    .h(px(20.0))
                    .rounded(px(1.0))
                    .bg(gpui::hsla(0.0, 0.0, 1.0, 0.7)),
            );

        match self.calculation {
            Some(v) => div()
                .flex()
                .flex_col()
                .child(field)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_4()
                        .h(px(34.0))
                        .text_size(px(15.0))
                        .text_color(gpui::hsla(0.55, 0.60, 0.75, 0.95))
                        .child(SharedString::from("="))
                        .child(SharedString::from(calc::format(v))),
                )
                .into_any_element(),
            None => field.into_any_element(),
        }
    }

}

/// One result row.
///
/// A free function taking the entity, because every handler here is `'static`
/// and a closure inside a method cannot borrow `self`. `selected` is passed in
/// rather than read from the entity so the row's appearance is decided by the
/// render pass that is drawing it, not by a second lookup.
fn render_row(
    index: usize,
    selected: bool,
    hit: &launcher::Hit,
    this: Entity<Launcher>,
) -> gpui::AnyElement {
    let entry = hit.entry.clone();

    div()
        .id(("lume-launcher-row", index))
        .flex()
        .items_center()
        .gap_3()
        .mx_2()
        .px_3()
        .h(px(ROW))
        // Required, and easy to miss. A `flex_col` child shrinks by default,
        // so with the list height fixed at ROW * VISIBLE_ROWS the rows were
        // being compressed to fit instead of scrolling: measured 28px against
        // a declared 44px, and twelve rows in a box sized for eight. Shrink 0
        // makes the declared height mean something and lets the scroll do its
        // job.
        .flex_shrink_0()
        .rounded(px(8.0))
        .cursor_pointer()
        .when(selected, |s| s.bg(gpui::hsla(0.60, 0.45, 0.62, 0.30)))
        // Hover, so a mouse user can see what a click would hit. Without it
        // the only feedback is the selection, which only moves on click.
        .hover(|s| s.bg(gpui::hsla(0.60, 0.45, 0.62, 0.18)))
        .on_click(move |_e, _w, cx| {
            this.update(cx, |me, cx| {
                // Set the selection as well as launching, so a click and Enter
                // are the same path rather than "click whatever was under the
                // cursor".
                me.selected = index;
                if let Some(f) = &me.on_launch {
                    f(&entry);
                }
                me.close(cx);
            });
        })
        .child(super::app_icon(&hit.entry.icon, &hit.entry.name, px(28.0)))
        .child(
            div()
                .flex_1()
                .text_size(px(14.0))
                .text_color(rgb(0xe8e8e8))
                .child(hit.entry.name.clone()),
        )
        .into_any_element()
}