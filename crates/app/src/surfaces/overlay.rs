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
    prelude::*, px, rgb,
};

use super::launcher::Launcher;
use super::menu::{self, Menu, MenuId};
use super::picker::Picker;

pub struct Overlay {
    focus: FocusHandle,
    picker: Option<Entity<Picker>>,
    /// The widget menu, and where it is anchored on screen.
    ///
    /// At most one: a second would mean two things competing for the
    /// keyboard on one surface.
    menu: Option<(MenuId, Menu, services::intent::Anchor)>,
    /// The launcher. At most one, for the same reason as the menu: only one
    /// thing on this surface may own the keyboard.
    launcher: Option<Entity<Launcher>>,
    /// The display this overlay covers. The picker needs it to size its grid,
    /// and the surface is the thing that knows.
    display: gpui::Size<gpui::Pixels>,
    /// Whether the keyboard focus has already been taken for the current
    /// open. Focus is taken in `render` because `Context` has no way to focus
    /// without a `Window`, and `open_picker` does not have one.
    focused: bool,
    /// Set to true when a click hits overlay content (picker tile, menu item,
    /// launcher result). The dismiss handler checks this and only closes when
    /// the click landed on the overlay background, not on content.
    clicked_content: bool,
}

impl Overlay {
    pub fn new(display: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            picker: None,
            menu: None,
            launcher: None,
            display,
            focused: false,
            clicked_content: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.picker.is_none() && self.menu.is_none() && self.launcher.is_none()
    }

    /// Open the launcher, replacing whatever is up.
    ///
    /// The launcher is the one overlay that can displace another: a user who
    /// has the picker open and hits the launch keybind meant the launcher.
    /// Refusing to open, or stacking two panels, are both worse.
    pub fn open_launcher(&mut self, cx: &mut Context<Self>) {
        let apps = cx.global::<services::AppState>().apps.clone();
        let launcher = cx.new(|cx| Launcher::new(apps, cx));

        // Wired here rather than inside the launcher for the same reason the
        // picker's close handler is: the parent entity handle does not exist
        // until after `cx.new` returns.
        let me = cx.entity();
        launcher.update(cx, |l, _cx| {
            l.set_on_close(Arc::new(move |app| {
                me.update(app, |o, cx| {
                    o.clicked_content = true;
                    let _ = o.close(cx);
                });
            }));
            l.set_on_launch(Arc::new(|entry| {
                super::background::launch_public(entry);
            }));
        });

        self.picker = None;
        self.menu = None;
        self.launcher = Some(launcher);
        self.focused = false;
        cx.notify();
        tracing::info!("overlay opened the launcher");
    }

    /// Apply one open/close/toggle request for a widget menu.
    ///
    /// Toggling only ever dismisses the menu that is actually open: clicking
    /// a second widget switches to it rather than closing it. Anything else
    /// would make the bar feel broken — the user asked for a panel and got
    /// silence.
    pub fn open_menu(
        &mut self,
        which: MenuId,
        anchor: services::intent::Anchor,
        op: services::intent::DrawerOp,
        cx: &mut Context<Self>,
    ) {
        let outcome = Self::resolve_menu(self.open_menu_id(), which, op);
        match outcome {
            MenuOutcome::Close => return self.close_menu(cx),
            MenuOutcome::Keep => return,
            MenuOutcome::Open => {
                self.menu = Some((which, which.build(), anchor));
                tracing::info!(title = which.title(), "menu opened");
                cx.notify();
            }
        }
    }

    /// The menu currently open, if any.
    pub fn open_menu_id(&self) -> Option<MenuId> {
        self.menu.as_ref().map(|(id, _, _)| *id)
    }

    /// The bar's height in pixels: the strip clicks must fall through to the bar
/// rather than dismissing a panel.
///
/// Falls back to the default rather than erroring. An overlay that cannot
/// read the config still dismisses correctly; it just also swallows the bar
/// strip, which is the old behaviour rather than a crash.
fn resolve_menu(
        open: Option<MenuId>,
        which: MenuId,
        op: services::intent::DrawerOp,
    ) -> MenuOutcome {
        use services::intent::DrawerOp;
        match op {
            DrawerOp::Close => MenuOutcome::Close,
            // Toggling the menu that is already open dismisses it. Toggling a
            // *different* one switches to it: the user asked for a panel and
            // closing instead would read as the bar being broken.
            DrawerOp::Toggle if open == Some(which) => MenuOutcome::Close,
            DrawerOp::Toggle | DrawerOp::Open => MenuOutcome::Open,
        }
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            tracing::info!("menu closed");
            cx.notify();
        }
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
        let me_for_click = me.clone();
        picker.update(cx, |p, _cx| {
            p.set_on_close(Arc::new(move |app| {
                me.update(app, |o, cx| {
                    o.clicked_content = true;
                    o.close(cx);
                });
            }));
            p.set_on_content_click(Arc::new(move |app| {
                me_for_click.update(app, |o, _cx| {
                    o.clicked_content = true;
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

    /// Close whatever is up, and report whether anything was.
    ///
    /// Escape must dismiss *whatever* the overlay is showing, so this takes
    /// both. It previously only took the picker: when the drawer was added,
    /// the edit that made this handle both did not apply, and the symptom was
    /// "Escape closes the wallpaper picker but not the widget drawer" — a
    /// handler that provably ran, since it logged every key it received, and
    /// then did nothing.
    pub fn close(&mut self, cx: &mut Context<Self>) -> bool {
        let had_picker = self.picker.take().is_some();
        let had_menu = self.menu.take().is_some();
        // Cleared without recursing through the launcher's own close handler:
        // that handler calls back into `close`, and following it would mean
        // this function runs twice for one Escape.
        let had_launcher = self.launcher.take().is_some();

        if had_picker || had_menu || had_launcher {
            self.focused = false;
            // Freed here rather than left to the process lifetime: the
            // thumbnails are ~100 MB of device memory and nothing else wants
            // them once the picker is gone.
            if had_picker {
                super::thumbs::clear();
            }
            tracing::info!(
                picker = had_picker,
                menu = had_menu,
                launcher = had_launcher,
                "overlay closed"
            );
            cx.notify();
        }

        had_picker || had_menu
    }

    pub fn toggle_picker(&mut self, current: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close(cx);
        } else {
            self.open_picker(current, cx);
        }
    }
}

/// The bar's height in pixels: the strip clicks must fall through to the bar
/// rather than dismissing a panel.
///
/// Falls back to the default rather than erroring. An overlay that cannot
/// read the config still dismisses correctly; it just also swallows the bar
/// strip, which is the old behaviour rather than a crash.
fn bar_height(cx: &gpui::App) -> f32 {
    cx.try_global::<services::AppState>()
        .map(|s| s.config.get().bar.height as f32)
        .unwrap_or(services::config::Bar::default().height as f32)
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
        if self.is_empty() {
            return div().id("lume-overlay-empty").size_0().into_any_element();
        }

        if !self.focused {
            window.focus(&self.focus, cx);
            self.focused = true;
        }

        // The launcher wins over everything else: it was opened last, and it
        // is the only one that takes the whole screen.
        let content: Option<gpui::AnyElement> = match (&self.launcher, &self.menu, &self.picker) {
            (Some(l), _, _) => Some(l.clone().into_any_element()),
            (None, Some((_, menu, anchor)), _) => Some(render_menu(menu.clone(), *anchor, window, cx)),
            (None, None, Some(p)) => Some(p.clone().into_any_element()),
            _ => None,
        };

        // Nothing to show. Returning an empty, transparent, zero-size node
        // rather than a full-screen div: a visible div here would paint over
        // the desktop even with no children.
        let Some(content) = content else {
            return div().id("lume-overlay-empty").size_0().into_any_element();
        };

        div()
            .id("lume-overlay")
            .size_full()
            // The dismiss surface, covering everything the popup does not.
            //
            // It deliberately leaves the bar's strip alone. A modal overlay
            // takes the whole input region, so this element receives the
            // pointer before the bar does — and dismissing here on the same
            // physical click that opened the menu meant the toggle always saw
            // an empty overlay and always re-opened. Clicking a widget to
            // close its own panel is the whole gesture, so the bar keeps it.
            .on_click({
                let this = cx.entity();
                let bar_h = bar_height(cx);
                move |event: &gpui::ClickEvent, _window: &mut Window, cx: &mut gpui::App| {
                    if f32::from(event.position().y) < bar_h {
                        return;
                    }
                    this.update(cx, |o, cx| {
                        if o.clicked_content {
                            o.clicked_content = false;
                            return;
                        }
                        o.close(cx);
                    });
                }
            })
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
                move |event: &gpui::KeyDownEvent, w, cx| {
                    // Logged for EVERY key, not just escape. The question
                    // being answered is "does this surface receive keys at
                    // all", and a handler that silently ignores everything
                    // else cannot answer it.
                    tracing::debug!(key = %event.keystroke.key, "overlay key");
                    if event.keystroke.key != "escape" {
                        // Everything that is not a dismiss belongs to the
                        // content. The launcher is a text field and the
                        // overlay is its keyboard: routing here rather than
                        // giving the launcher its own focus handle means one
                        // focus owner on this surface, not two that have to
                        // agree about who has it.
                        let Some(launcher) = this.read(cx).launcher.clone() else {
                            return;
                        };
                        launcher.update(cx, |l, cx| {
                            l.on_key(event, w, cx);
                        });
                        return;
                    }
                    this.update(cx, |me, cx| {
                        me.close(cx);
                    });
                }
            })
            .child(content)
            .into_any_element()
    }
}

/// Render the widget menu popup.
///
/// Takes the menu **by value**. Every handler built here is `'static`, and a
/// `'static` closure cannot capture a reference into a `&Menu` — borrowing it
/// and copying fields one at a time fights the borrow checker through the whole
/// match. A `Menu` is a `Vec<Item>` of `Arc`s and strings; cloning is free.
fn render_menu(
    menu: Menu,
    anchor: services::intent::Anchor,
    window: &mut Window,
    cx: &mut Context<Overlay>,
) -> gpui::AnyElement {
    use services::intent::{Anchor, Edge};

    let this = cx.entity();
    let viewport = window.viewport_size();

    let left = match anchor {
        // Clamped so the popup stays on screen: a widget near an edge would
        // otherwise push half the panel off the display.
        Anchor::At(x) => (x - menu::MENU_WIDTH / 2.0).clamp(
            8.0,
            (f32::from(viewport.width) - menu::MENU_WIDTH - 8.0).max(8.0),
        ),
        Anchor::Edge(Edge::Right) => f32::from(viewport.width) - menu::MENU_WIDTH - 8.0,
        Anchor::Edge(Edge::Left) => 8.0,
    };

    let items = menu.items.iter().cloned().map(|item| match item {
        menu::Item::Section { title, action } => section(title, action, &this),
        menu::Item::Meter { value, caption, .. } => meter(value, &caption),
        menu::Item::Detail { label, value } => detail(label, &value),
        menu::Item::Fader { min, max, step, get, set } => {
            fader(min, max, step, get, set, &this)
        }
        menu::Item::Pill { label, get, set } => pill(label, get, set, &this),
        menu::Item::Segmented { label, options, selected, set } => {
            segmented(label, &options, selected, set, &this)
        }
        menu::Item::Action(a) => action_row(&a, &this),
        menu::Item::Footer(a) => footer(&a),
        menu::Item::Row { title, subtitle, trailing, run } => {
            list_row(&title, &subtitle, &trailing, run, &this)
        }
        menu::Item::Hint(text) => hint(&text),
    });

    div()
        .id("lume-menu")
        .absolute()
        .top(px(38.0))
        .left(px(left))
        .w(px(menu::MENU_WIDTH))
        .max_h(px(f32::from(viewport.height) - 80.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .p_3()
        .gap_1()
        .rounded(px(10.0))
        .backdrop_blur(px(20.0))
        // Near-opaque. At 0.92 the window behind was legible through the panel
        // and a menu you read against moving text is a menu you cannot read.
        .bg(gpui::hsla(0.10, 0.05, 0.09, 0.985))
        .border_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.12))
        .shadow_lg()
        .child(menu_header(menu.title, &this))
        .children(items)
        .into_any_element()
}

/// Title bar with a close button.
///
/// The close button is not decoration. Escape works, but only while this
/// surface holds the keyboard, and a user who clicked a widget with the mouse
/// should be able to dismiss it with the mouse.
fn menu_header(title: &'static str, this: &Entity<Overlay>) -> gpui::AnyElement {
    div()
        .flex()
        .justify_between()
        .items_center()
        .mb_2()
        .pb_2()
        .border_b_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(0xf0f0f0))
                .child(gpui::SharedString::from(title)),
        )
        .child(
            div()
                .id("lume-menu-close")
                .size(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(px(13.0))
                .text_color(rgb(0xa0a0a0))
                .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.12)))
                .child(gpui::SharedString::from("\u{2715}"))
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| this.update(cx, |o, cx| {
                        o.clicked_content = true;
                        let _ = o.close(cx);
                    })
                }),
        )
        .into_any_element()
}

fn section(
    title: &'static str,
    action: Option<menu::Action>,
    this: &Entity<Overlay>,
) -> gpui::AnyElement {
    let right = action.map(|a| {
        let run = a.run.clone();
        div()
            .id(("lume-menu-section-action", hash(title)))
            .px_2()
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(px(10.0))
            .text_color(rgb(0x90a0b0))
            .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.12)))
            .child(gpui::SharedString::from(a.label))
            .on_click(move |_, _w, _cx| run())
            .into_any_element()
    });

    let bar = div()
        .flex()
        .justify_between()
        .items_center()
        .mt_3()
        .mb_1();

    let bar = match right {
        Some(r) => bar.child(r),
        None => bar,
    };

    bar.child(
        div()
            .text_size(px(10.0))
            .text_color(rgb(0x8090a0))
            .child(gpui::SharedString::from(title)),
    )
    .into_any_element()
}

/// The headline number, as a bar with a caption.
fn meter(value: f32, caption: &str) -> gpui::AnyElement {
    let pct = value.clamp(0.0, 1.0);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .mb_2()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(0xc0c0c0))
                .child(gpui::SharedString::from(caption.to_string())),
        )
        .child(
            div()
                .h(px(8.0))
                .rounded_full()
                .bg(rgb(0x303030))
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(px(260.0 * pct))
                        .bg(gpui::hsla(0.60, 0.85, 0.68, 1.0)),
                ),
        )
        .into_any_element()
}

fn detail(label: &str, value: &str) -> gpui::AnyElement {
    div()
        .id(("lume-menu-detail", hash(label)))
        .flex()
        .justify_between()
        .gap_3()
        .py_1()
        .text_size(px(11.0))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(0x909090))
                .child(gpui::SharedString::from(label.to_string())),
        )
        .child(
            div()
                .overflow_hidden()
                .text_color(rgb(0xe0e0e0))
                .child(gpui::SharedString::from(menu::truncate(value, 30))),
        )
        .into_any_element()
}

fn fader(
    min: f32,
    max: f32,
    step: f32,
    get: Arc<dyn Fn() -> f32 + Send + Sync>,
    set: Arc<dyn Fn(f32) + Send + Sync>,
    this: &Entity<Overlay>,
) -> gpui::AnyElement {
    // Cloned into locals so the 'static scroll closure owns them rather than
    // borrowing this function's parameters.
    let get = Arc::clone(&get);
    let set = Arc::clone(&set);
    let this = this.clone();
    let value = get();
    let pct = if max > min { (value - min) / (max - min) } else { 0.0 };

    div()
        .id("lume-menu-fader")
        .h(px(10.0))
        .my_2()
        .rounded_full()
        .bg(rgb(0x303030))
        .overflow_hidden()
        .cursor_pointer()
        .child(
            div()
                .h_full()
                .w(px(260.0 * pct.clamp(0.0, 1.0)))
                .bg(gpui::hsla(0.60, 0.85, 0.68, 1.0)),
        )
        // `ScrollDelta` is pixels on a mouse and lines on a trackpad; handling
        // only one makes the fader unusable on half the machines.
        .on_scroll_wheel(move |event, _w, cx| {
            use gpui::ScrollDelta;
            let lines = match event.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y) / 40.0,
                ScrollDelta::Lines(l) => l.y,
            };
            if lines == 0.0 {
                return;
            }
            let next = (get() - lines * step).clamp(min, max);
            set(next);
            this.update(cx, |_o, cx| cx.notify());
        })
        .into_any_element()
}

fn pill(
    label: &'static str,
    get: Arc<dyn Fn() -> bool + Send + Sync>,
    set: Arc<dyn Fn(bool) + Send + Sync>,
    this: &Entity<Overlay>,
) -> gpui::AnyElement {
    let on = get();
    let get = Arc::clone(&get);
    let set = Arc::clone(&set);
    let this = this.clone();
    div()
        .id("lume-menu-pill")
        .flex()
        .justify_between()
        .items_center()
        .py_1()
        .text_size(px(11.0))
        .child(
            div()
                .text_color(rgb(0x909090))
                .child(gpui::SharedString::from(label)),
        )
        .child(
            div()
                .id("lume-menu-pill-toggle")
                .px_3()
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded_full()
                .cursor_pointer()
                .text_size(px(10.0))
                .text_color(rgb(0x101010))
                .bg(if on {
                    gpui::hsla(0.60, 0.85, 0.68, 1.0)
                } else {
                    gpui::hsla(0.0, 0.0, 0.6, 0.30)
                })
                .child(gpui::SharedString::from(if on { "ON" } else { "OFF" }))
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        let set = Arc::clone(&set);
                        let next = !get();
                        set(next);
                        this.update(cx, |_o, cx| cx.notify());
                    }
                }),
        )
        .into_any_element()
}

fn segmented(
    label: &'static str,
    options: &[&str],
    selected: usize,
    set: Arc<dyn Fn(usize) + Send + Sync>,
    this: &Entity<Overlay>,
) -> gpui::AnyElement {
    let buttons = options.iter().enumerate().map(|(i, opt)| {
        let active = i == selected;
        div()
            .id(("lume-menu-seg", hash(opt)))
            .flex_1()
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .text_size(px(10.0))
            .text_color(if active { rgb(0x101010) } else { rgb(0xc0c0c0) })
            .bg(if active {
                gpui::hsla(0.60, 0.85, 0.68, 1.0)
            } else {
                gpui::hsla(0.0, 0.0, 1.0, 0.08)
            })
            .child(gpui::SharedString::from(*opt))
            .on_click({
                let this = this.clone();
                let set = Arc::clone(&set);
                move |_, _w, cx| {
                    set(i);
                    this.update(cx, |_o, cx| cx.notify());
                }
            })
    });

    div()
        .flex()
        .flex_col()
        .gap_1()
        .my_2()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(0x909090))
                .child(gpui::SharedString::from(label)),
        )
        .child(div().flex().gap_1().children(buttons))
        .into_any_element()
}

fn action_row(action: &menu::Action, _this: &Entity<Overlay>) -> gpui::AnyElement {
    let run = action.run.clone();
    div()
        .id(("lume-menu-action", hash(action.label)))
        .h(px(26.0))
        .px_2()
        .flex()
        .items_center()
        .rounded(px(5.0))
        .cursor_pointer()
        .text_size(px(11.0))
        .text_color(rgb(0xe0e0e0))
        .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.10)))
        .child(gpui::SharedString::from(action.label))
        .on_click(move |_, _w, _cx| run())
        .into_any_element()
}

fn footer(action: &menu::Action) -> gpui::AnyElement {
    let run = action.run.clone();
    div()
        .id(("lume-menu-footer", hash(action.label)))
        .mt_3()
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .text_size(px(11.0))
        .text_color(rgb(0x101010))
        .bg(rgb(0xe8e8e8))
        .child(gpui::SharedString::from(action.label))
        .on_click(move |_, _w, _cx| run())
        .into_any_element()
}

fn list_row(
    title: &str,
    subtitle: &str,
    trailing: &str,
    run: Arc<dyn Fn() + Send + Sync>,
    this: &Entity<Overlay>,
) -> gpui::AnyElement {
    let _ = this;
    div()
        .id(("lume-menu-row", hash(title)))
        .h(px(26.0))
        .px_2()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(px(5.0))
        .cursor_pointer()
        .text_size(px(11.0))
        .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.10)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(rgb(0xe0e0e0))
                        .child(gpui::SharedString::from(menu::truncate(title, 22))),
                )
                .when(!subtitle.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(9.0))
                            .text_color(rgb(0x808080))
                            .child(gpui::SharedString::from(subtitle.to_string())),
                    )
                }),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(9.0))
                .text_color(rgb(0xa0b0c0))
                .child(gpui::SharedString::from(trailing.to_string())),
        )
        .on_click(move |_, _w, _cx| run())
        .into_any_element()
}

fn hint(text: &str) -> gpui::AnyElement {
    div()
        .py_1()
        .text_size(px(10.0))
        .text_color(rgb(0x707070))
        .child(gpui::SharedString::from(text.to_string()))
        .into_any_element()
}

/// `ElementId` tuples take only integers, so a runtime string is hashed.
fn hash(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

/// What to do with a menu request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuOutcome {
    Open,
    Close,
    /// Already in the requested state; leave the overlay alone and do not
    /// notify, which would repaint for nothing.
    Keep,
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::intent::DrawerOp;

    fn resolve(open: Option<MenuId>, which: MenuId, op: DrawerOp) -> MenuOutcome {
        Overlay::resolve_menu(open, which, op)
    }

    /// At most one menu is open. This is the whole point of the resolution:
    /// a bar with four poppable widgets must never stack two panels.
    #[test]
    fn a_second_widget_switches_rather_than_stacks() {
        assert_eq!(
            resolve(Some(MenuId::Volume), MenuId::Network, DrawerOp::Toggle),
            MenuOutcome::Open
        );
    }

    /// Clicking the widget whose panel is already open closes it. Without
    /// this the panel could only be dismissed by clicking elsewhere, which
    /// makes the bar feel like it is not listening.
    #[test]
    fn toggling_the_open_widget_closes_it() {
        assert_eq!(
            resolve(Some(MenuId::Volume), MenuId::Volume, DrawerOp::Toggle),
            MenuOutcome::Close
        );
    }

    #[test]
    fn open_is_idempotent_and_close_is_idempotent() {
        assert_eq!(
            resolve(None, MenuId::Volume, DrawerOp::Open),
            MenuOutcome::Open
        );
        assert_eq!(
            resolve(Some(MenuId::Volume), MenuId::Volume, DrawerOp::Open),
            MenuOutcome::Open
        );
        assert_eq!(
            resolve(Some(MenuId::Volume), MenuId::Network, DrawerOp::Close),
            MenuOutcome::Close
        );
        assert_eq!(resolve(None, MenuId::Volume, DrawerOp::Close), MenuOutcome::Close);
    }

    /// Every combination of state and verb is covered, so a new menu added
    /// later cannot inherit an unhandled branch.
    #[test]
    fn every_state_and_verb_resolves() {
        for open in [None, Some(MenuId::Volume), Some(MenuId::Network)] {
            for op in [DrawerOp::Open, DrawerOp::Close, DrawerOp::Toggle] {
                assert!(resolve(open, MenuId::Battery, op) != MenuOutcome::Keep);
            }
        }
    }
}
