//! Panel content: the sidebars and the dock.
//!
//! The surfaces themselves are owned by `surfaces::Surfaces`. This file is what
//! goes inside them.
//!
//! Icons resolve through the freedesktop icon theme (`services::icons`), which
//! walks `$XDG_DATA_DIRS/icons/<theme>`, follows `Inherits`, and falls back to
//! `hicolor` — the same lookup a GTK client does.

use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, IntoElement, Render, Stateful,
    StatefulInteractiveElement, div,
    prelude::*, px, rgb,
};
use services::apps::AppEntry;

use super::DOCK_HEIGHT;

/// An icon resolved against the user's theme, with a themed fallback.
fn icon(name: &str, size: f32) -> AnyElement {
    let size = px(size);

    if let Some(path) = services::icons::resolve(name) {
        return gpui::img(path.to_string_lossy().into_owned())
            .size(size)
            .into_any_element();
    }

    // Not found. A neutral tile is better than a broken image, and the initial
    // tells the user which app it is.
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .collect::<String>();

    div()
        .size(size)
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::hsla(0.60, 0.20, 0.45, 0.5))
        .text_color(rgb(0xf0f0f0))
        .child(initial)
        .into_any_element()
}

// ---------------------------------------------------------------- dock

pub struct Dock {
    focus: FocusHandle,
    items: Vec<DockItem>,
    hovered: Option<usize>,
    /// Id of the app whose window is focused, if it is a dock item.
    focused_app: Option<String>,
}

pub struct DockItem {
    pub app: AppEntry,
    pub pinned: bool,
}

impl Dock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.global::<services::AppState>().clone();
        let index = state.apps.all();

        // Pinned apps come from config. A pinned entry with no matching
        // .desktop file is skipped rather than shown as a broken tile, and the
        // skip is logged — an empty dock with no explanation reads as broken.
        let cfg = state.config.get();
        let items: Vec<DockItem> = cfg
            .dock
            .pinned
            .iter()
            .filter_map(|id| {
                let hit = index.iter().find(|a| &a.id == id).cloned();
                if hit.is_none() {
                    tracing::debug!(id = %id, "pinned app has no .desktop file; skipped");
                }
                hit
            })
            .map(|app| DockItem { app, pinned: true })
            .collect();

        if items.is_empty() {
            tracing::info!(
                "dock is empty — pin apps in [dock] pinned = [\"footclient.desktop\", ...]"
            );
        } else {
            tracing::info!(count = items.len(), "dock populated");
        }

        Self { focus: cx.focus_handle(), items, hovered: None, focused_app: None }
    }

    pub fn set_focused_app(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.focused_app != id {
            self.focused_app = id;
            cx.notify();
        }
    }
}

impl Focusable for Dock {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Dock {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let accent = gpui::hsla(0.60, 0.85, 0.68, 1.0);

        let buttons = self.items.iter().enumerate().map(|(i, item)| {
            let app = item.app.clone();
            let is_focused = self.focused_app.as_deref() == Some(app.id.as_str());
            let is_hovered = self.hovered == Some(i);

            div()
                .id(("lume-dock-item", hash_of(&app.id)))
                // Lifts on hover. A border and a size change are style-only —
                // no frame loop, nothing animating on its own.
                .size(px(48.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.0))
                .cursor_pointer()
                .bg(if is_hovered {
                    gpui::hsla(0.0, 0.0, 1.0, 0.12)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .child(icon(&app.icon, 40.0))
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        let id = app.id.clone();
                        this.update(cx, |me, cx| {
                            if let Some(entry) =
                                cx.global::<services::AppState>().apps.get(&id)
                            {
                                super::background::launch_public(&entry);
                            }
                            cx.notify();
                        });
                    }
                })
                .on_hover({
                    let this = this.clone();
                    move |_e, _w, cx| this.update(cx, |me, _cx| me.hovered = Some(i))
                })
                // A running-indicator bar under the item. Derived state, so
                // it repaints only when the set changes — never animated.
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .h(px(3.0))
                        .w(px(20.0))
                        .rounded_full()
                        .bg(if is_focused { accent } else { gpui::hsla(0.0, 0.0, 0.0, 0.0) }),
                )
        });

        div()
            .id("lume-dock")
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .px_3()
            .bg(gpui::hsla(0.10, 0.05, 0.09, 0.86))
            .border_1()
            .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
            .child(div().h_full().min_w(px(8.0)))
            .children(buttons)
    }
}

fn hash_of(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_for_the_same_input() {
        assert_eq!(hash_of("firefox_firefox.desktop"), hash_of("firefox_firefox.desktop"));
        assert_ne!(hash_of("a.desktop"), hash_of("b.desktop"));
    }
}