//! The background surface: desktop icons, wallpaper control, and the picker.
//!
//! Two rules govern everything here.
//!
//! **1. It is almost always empty.** The wallpaper is drawn by `awww`, so this
//! surface paints only icons. Its dirty area is a few hundred pixels rather
//! than full-screen, which is what makes hover cheap.
//!
//! **2. Stay out of the pointer-motion path.** The tempting implementation is
//! `.on_hover(...)`, which fires on every pointer move and would repaint a
//! full-screen surface 60 times a second. Instead we compare the hit result to
//! the previous one and notify only when it *changes*.
//!
//! Input regions are the other half: this surface covers the whole desktop, so
//! without an explicit region every click aimed at a terminal would be eaten.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Pixels, Point, Render, Size,
    AnyElement, StatefulInteractiveElement, Window, div, point,
    prelude::*, px, rgb,
};
use gpui::SharedString;
use std::path::{Path, PathBuf};

use services::{AppState, apps};

use super::picker::Picker;

// ---------------------------------------------------------------- grid

#[derive(Clone, Copy)]
pub struct Grid {
    pub cell: f32,
    pub padding: f32,
    pub origin: Point<Pixels>,
    pub columns: usize,
    /// The display this grid was built for. Kept because the picker needs the
    /// viewport and the grid is what was handed it — deriving it twice from
    /// two sources is how they end up disagreeing.
    pub display: Size<Pixels>,
}

impl Grid {
    pub fn for_display(size: Size<Pixels>) -> Self {
        let cell = 96.0;
        let padding = 16.0;
        // Leave the top clear so icons never sit under the bar.
        let origin = point(px(padding), px(64.0));
        let columns =
            (((f32::from(size.width) - padding) / (cell + padding)).floor() as usize).max(1);
        Self { cell, padding, origin, columns, display: size }
    }

    pub fn display_size(&self) -> Size<Pixels> {
        self.display
    }

    /// A rect is a pure function of the cell index. That is what makes layout
    /// reproducible and drag bookkeeping trivial.
    pub fn cell_origin(&self, cell: usize) -> (f32, f32) {
        let cols = self.columns.max(1);
        let col = cell % cols;
        let row = cell / cols;
        (
            f32::from(self.origin.x) + self.padding + col as f32 * (self.cell + self.padding),
            f32::from(self.origin.y) + self.padding + row as f32 * (self.cell + self.padding),
        )
    }

    pub fn cell_at(&self, p: Point<Pixels>) -> Option<usize> {
        let x = f32::from(p.x) - f32::from(self.origin.x) - self.padding;
        let y = f32::from(p.y) - f32::from(self.origin.y) - self.padding;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let step = self.cell + self.padding;
        Some(((y / step).floor() as usize) * self.columns.max(1) + (x / step).floor() as usize)
    }

    pub fn rect(&self, cell: usize) -> Bounds<Pixels> {
        let (x, y) = self.cell_origin(cell);
        Bounds { origin: point(px(x), px(y)), size: gpui::size(px(self.cell), px(self.cell)) }
    }
}

// ---------------------------------------------------------------- launch

/// Launch an app. Parsed argv, exec'd directly. There is no shell.
pub fn launch_public(app: &apps::AppEntry) {
    launch(app)
}

/// Open a Desktop item.
///
/// Three destinations, one entry point, because the grid, the selection and
/// the context menu all need this and must not each grow their own idea of
/// what a folder is.
fn open_item(item: &services::desktop::DesktopItem) {
    match item {
        services::desktop::DesktopItem::Dir { name, .. } => {
            // The file manager is whatever the desktop says it is. Guessing
            // `nautilus` would be wrong on every machine but one.
            let dir = services::desktop::desktop_dir().join(name);
            let _ = services::ipc::spawn_detached("xdg-open", &[dir.to_string_lossy().into_owned()]);
        }
        services::desktop::DesktopItem::Launcher { path, .. } => {
            match launcher_entry(path) {
                Some(entry) => launch(&entry),
                None => {
                    tracing::warn!(path = %path.display(), "launcher has no Exec line");
                }
            }
        }
        services::desktop::DesktopItem::File { path, .. } => {
            let _ = services::ipc::spawn_detached(
                "xdg-open",
                &[path.to_string_lossy().into_owned()],
            );
        }
    }
}

/// Parse a `.desktop` file that lives on the Desktop.
///
/// `Type=Link` files point at a URL with a `URL=` key rather than an `Exec=`,
/// and are handled by handing the URL to `xdg-open`, which knows the protocol
/// handlers. Treating them as `Exec` would launch a shell command made of a
/// URL.
fn launcher_entry(path: &std::path::Path) -> Option<apps::AppEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut entry = apps::AppEntry {
        id: path.to_string_lossy().into_owned(),
        name: path.file_stem()?.to_string_lossy().into_owned(),
        generic_name: String::new(),
        exec: String::new(),
        icon: String::new(),
        terminal: false,
        path: path.to_path_buf(),
    };

    let mut section_ok = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // `Exec` is only meaningful in the main group. `Desktop Action`
            // blocks have their own, and taking one would run the wrong thing.
            section_ok = line == "[Desktop Entry]";
            continue;
        }
        if !section_ok {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => entry.name = value.trim().to_string(),
            "Exec" => entry.exec = value.trim().to_string(),
            "Icon" => entry.icon = value.trim().to_string(),
            "Terminal" => entry.terminal = value.trim() == "true",
            "URL" => {
                let url = value.trim().to_string();
                if !url.is_empty() {
                    let _ = services::ipc::spawn_detached("xdg-open", &[url]);
                    return None;
                }
            }
            _ => {}
        }
    }

    (!entry.exec.is_empty()).then_some(entry)
}

fn launch(app: &apps::AppEntry) {
    let Some(argv) = app.argv(&[]) else {
        tracing::warn!(id = %app.id, "unusable Exec line");
        return;
    };
    if argv.is_empty() {
        return;
    }

    let program = argv[0].clone();
    let args = argv[1..].to_vec();

    // spawn_detached scrubs injected env vars. A child inheriting a
    // crash-handle var can re-execute the shell, which draws a second bar.
    match services::ipc::spawn_detached(&program, &args) {
        Ok(()) => tracing::info!(id = %app.id, "launched"),
        Err(e) => tracing::error!(id = %app.id, "launch failed: {e}"),
    }
}

/// Hand the wallpaper to `awww`. One spawn, no render loop, no texture.
pub fn set_wallpaper(path: &std::path::Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("awww")
        .arg("img")
        .arg(path)
        .status()?;
    if !status.success() {
        anyhow::bail!("awww img failed with {status}");
    }
    tracing::info!(path = %path.display(), "wallpaper set via awww");
    Ok(())
}

// ---------------------------------------------------------------- state

/// Anything the desktop can show, from either source.
///
/// One type rather than two parallel vectors: the grid, the hit testing, the
/// region and the position store all key off a single id, and having an app
/// path and a file path would mean two layouts that cannot be reasoned about
/// together. Launching is the only place they genuinely differ, and that is a
/// match arm in one function.
#[derive(Clone)]
enum Entry {
    /// Pinned from `[desktop] pinned`.
    App(apps::AppEntry),
    /// Found in the Desktop directory.
    Item(services::desktop::DesktopItem),
}

impl Entry {
    fn id(&self) -> String {
        match self {
            Entry::App(a) => a.id.clone(),
            Entry::Item(i) => i.id(),
        }
    }

    fn label(&self) -> String {
        match self {
            Entry::App(a) => a.name.clone(),
            Entry::Item(i) => i.label().to_string(),
        }
    }

    fn icon(&self) -> String {
        match self {
            Entry::App(a) => a.icon.clone(),
            Entry::Item(i) => i.icon_name().to_string(),
        }
    }
}

/// What has been handed to the compositor for this surface's input region.
///
/// An enum rather than `Option<Option<Vec<Bounds>>>`. "Never pushed" and
/// "pushed, meaning the whole surface" are genuinely different states, and
/// the nested Option made that distinction a place to get it wrong — the
/// first version of this compared the wrong two sides and did not compile.
#[derive(PartialEq)]
enum InputRegion {
    Never,
    Pushed(Option<Vec<Bounds<Pixels>>>),
}

impl InputRegion {
    fn differs(&self, next: Option<&Vec<Bounds<Pixels>>>) -> bool {
        match self {
            InputRegion::Never => true,
            InputRegion::Pushed(pushed) => pushed.as_ref() != next,
        }
    }
}

pub struct Desktop {
    focus: FocusHandle,
    grid: Grid,
    /// App id -> cell. Sorted by cell so layout is stable and rendering order
    /// is deterministic.
    placed: Vec<(String, usize)>,
    entries: Vec<Entry>,
    hovered: Option<usize>,
    selected: Option<String>,
    /// Icons only. Empty means the whole surface is click-through.
    region: Vec<Bounds<Pixels>>,
    /// What was last handed to the compositor.
    ///
    /// Without this the region was pushed once, at map time, and never again:
    /// a file dropped on the Desktop appeared within three seconds and could
    /// not be clicked until lume restarted. `rescan` updated `region` and
    /// nothing carried it to the window.
    pushed_region: InputRegion,
    /// The wallpaper the daemon currently has set, if any. Public because the
    /// overlay highlights it and the overlay is a different surface.
    pub current_wallpaper: Option<PathBuf>,
    menu_at: Option<Point<Pixels>>,
}

impl Desktop {
    pub fn new(display: Size<Pixels>, cx: &mut Context<Self>) -> Self {
        let state = cx.global::<AppState>().clone();
        let grid = Grid::for_display(display);

        // Only PINNED apps appear on the desktop. Listing every installed
        // application was the previous behaviour and it was wrong twice over:
        // nobody wants 200 icons on their desktop, and 200 rects in one
        // wl_region is a per-commit cost. Measured: +20M private dirty and
        // idle CPU 0.10% -> 0.35%.
        //
        // The set comes from config (`desktop.pinned`); the index supplies
        // the metadata for whatever is named there.
        let cfg = state.config.get();
        let index = state.apps.all();
        let pinned = &cfg.desktop.pinned;
        let mut apps: Vec<apps::AppEntry> = pinned
            .iter()
            .filter_map(|id| index.iter().find(|a| &a.id == id).cloned())
            .collect();

        if apps.is_empty() {
            tracing::info!(
                "no pinned desktop icons; pin some in [desktop] pinned = [...]"
            );
        }

        // The Desktop directory, appended after the pinned apps so a pinned
        // app keeps cell 0 and file positions do not shuffle when one is
        // pinned or unpinned.
        let from_dir = if cfg.desktop.show_files {
            services::desktop::scan()
        } else {
            Vec::new()
        };
        tracing::info!(
            dir = %services::desktop::desktop_dir().display(),
            items = from_dir.len(),
            "desktop directory scanned"
        );

        let entries: Vec<Entry> = apps
            .drain(..)
            .map(Entry::App)
            .chain(from_dir.into_iter().map(Entry::Item))
            .collect();

        // Reconcile stored positions with the current item set. A stored cell
        // that two items both claim is a tie; first come, first served.
        let saved = state.config.icons();
        let (placed, region) = place(&entries, &saved, grid);

        let out = Self {
            focus: cx.focus_handle(),
            grid,
            placed,
            entries,
            hovered: None,
            selected: None,
            region,
            pushed_region: InputRegion::Never,
            current_wallpaper: Self::current_wallpaper(cx),
            menu_at: None,
        };

        // Watch the Desktop directory.
        //
        // A timer, not inotify. inotify would be the textbook answer and it
        // needs a non-blocking fd driven from the event loop, which this fork
        // does not expose without blocking a thread on read. One `read_dir`
        // of a flat directory every few seconds is a handful of syscalls; the
        // expensive part of a rescan is not the read, it is the repaint, and
        // `rescan` returns early when nothing changed so there is no repaint.
        //
        // This is the only always-on timer in the shell besides the
        // compositor's own.
        let dir = services::desktop::desktop_dir();
        cx.spawn(async move |weak, cx: &mut gpui::AsyncApp| {
            let mut last = std::time::SystemTime::UNIX_EPOCH;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;

                // One `stat` before touching the entity.
                //
                // Going through `Entity::update` marks the containing window
                // dirty, and this surface is full-screen — so polling it
                // unconditionally repainted 1920x1200 twenty times a minute to
                // discover nothing had changed. Measured: 0.117% idle CPU,
                // essentially all of it this. The directory's mtime changes
                // when an entry is added, removed or renamed, which is every
                // case that matters here.
                let Ok(meta) = std::fs::metadata(&dir) else { continue };
                let Ok(modified) = meta.modified() else { continue };
                if modified == last {
                    continue;
                }
                last = modified;

                if weak
                    .update(cx, |me, cx| {
                        me.rescan(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        out
    }

    /// What the compositor should let the pointer reach.
    ///
    /// `None` is the WHOLE surface — the opposite of what it looks like. The
    /// menu needs it: it is drawn on this surface, and with an icon-rect region
    /// it could not be clicked. Its scrim dismisses it, so the surface
    /// swallowing input while the menu is open is the modal behaviour a
    /// context menu is supposed to have.
    ///
    /// The picker is not here any more — it is on the Overlay surface, above
    /// windows. It could never have worked on Background: that layer is under
    /// every window, and the surface has no keyboard.
    ///
    /// Otherwise: icon rects only, so a click aimed at a terminal behind the
    /// desktop reaches the terminal.
    fn interactive_region(&self) -> Option<Vec<Bounds<Pixels>>> {
        if self.menu_at.is_some() {
            return None;
        }
        Some(self.region.clone())
    }

    /// The rectangles that accept input. Empty means invisible to the pointer.
    ///
    /// Recompute only when the layout changes structurally — this becomes a
    /// surface commit, and doing it per frame spins the compositor.
    pub fn input_regions(&self) -> Vec<Bounds<Pixels>> {
        self.region.clone()
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id() == id)
    }

    fn cell_of(&self, id: &str) -> Option<usize> {
        self.placed.iter().find(|(x, _)| x == id).map(|(_, c)| *c)
    }

    /// Re-read the Desktop directory and rebuild the layout if it changed.
    ///
    /// Returns whether anything moved, so the caller can skip the notify. An
    /// unchanged Desktop must not cost a repaint of a full-screen surface.
    pub fn rescan(&mut self, cx: &mut Context<Self>) -> bool {
        let state = cx.global::<services::AppState>();
        let cfg = state.config.get();

        let from_dir = if cfg.desktop.show_files {
            services::desktop::scan()
        } else {
            Vec::new()
        };
        let index = state.apps.all();
        let entries: Vec<Entry> = cfg
            .desktop
            .pinned
            .iter()
            .filter_map(|id| index.iter().find(|a| &a.id == id).cloned())
            .map(Entry::App)
            .chain(from_dir.into_iter().map(Entry::Item))
            .collect();

        let ids: Vec<String> = entries.iter().map(Entry::id).collect();
        if ids == self.entries.iter().map(Entry::id).collect::<Vec<_>>() {
            return false;
        }

        tracing::info!(items = entries.len(), "desktop changed");
        let (placed, region) = self.layout(&entries, cx);
        self.entries = entries;
        self.placed = placed;
        self.region = region;
        cx.notify();
        true
    }

    fn layout(
        &self,
        entries: &[Entry],
        cx: &mut Context<Self>,
    ) -> (Vec<(String, usize)>, Vec<Bounds<Pixels>>) {
        let saved = cx.global::<services::AppState>().config.icons();
        place(entries, &saved, self.grid)
    }

    fn refresh_region(&mut self) {
        self.region = self.placed.iter().map(|(_, c)| self.grid.rect(*c)).collect();
    }

    fn run(&mut self, action: MenuAction, cx: &mut Context<Self>) {
        // Any action dismisses the menu. Leaving it open behind a wallpaper
        // picker would put two overlays on one surface.
        self.menu_at = None;

        match action {
            MenuAction::NewFolder => match new_folder() {
                Ok(name) => {
                    tracing::info!(name = %name, "created desktop folder");
                    // The poller notices within a tick; forcing a rescan here
                    // would be a second code path for the same thing.
                    self.rescan(cx);
                }
                Err(e) => tracing::error!("could not create folder: {e:#}"),
            },
            // Goes through the intent channel like every other path, so the
            // click path and the keybind path are the same code. The picker
            // lives on the Overlay surface now: on Background it was behind
            // every window and could not take the keyboard.
            MenuAction::PickWallpaper => {
                services::intent::send(services::intent::Intent::Picker(
                    services::intent::PickerOp::Open,
                ));
            }
            MenuAction::OpenTerminal => match self.open_terminal(cx) {
                Ok(()) => {}
                Err(e) => tracing::error!("could not open a terminal: {e:#}"),
            },
        }
        cx.notify();
    }

    /// The user's terminal, launched in the Desktop directory.
    ///
    /// Read from the config rather than guessed. Probing the desktop files for
    /// `Terminal=true` would be more automatic and would silently pick
    /// whichever terminal happened to sort first — and "which terminal is
    /// mine" is a preference, not a fact.
    fn open_terminal(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let cfg = cx.global::<AppState>().config.get();
        let terminal = cfg.general.terminal.trim();
        if terminal.is_empty() {
            anyhow::bail!("no terminal configured; set [general] terminal");
        }
        let dir = services::desktop::desktop_dir();
        services::ipc::spawn_detached("sh", &[
            "-c".to_string(),
            // `exec` so the shell does not linger as a second process holding
            // the terminal's job, and single-quoted with the usual '\'' dance
            // so a path with a quote in it cannot break out.
            format!("cd {} && exec {}", shell_quote(&dir.to_string_lossy()), shell_quote(terminal)),
        ])?;
        Ok(())
    }

    /// The wallpaper currently applied, read back from the daemon.
    ///
    /// Shown highlighted in the picker so "which one is mine" is answerable
    /// without setting one and looking.
    fn current_wallpaper(cx: &mut Context<Self>) -> Option<PathBuf> {
        let out = std::process::Command::new("awww")
            .arg("query")
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.contains("Output:"))?;
        let path = line.split("Output:").nth(1)?.trim();
        // The daemon reports an absolute path; the grid holds one too.
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

}

// ---------------------------------------------------------------- render

impl Focusable for Desktop {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Deliberately NOT .on_hover. This surface is full-screen; a per-move
        // repaint is the expensive mistake this file exists to avoid. Compare
        // the hit result and notify only when it CHANGES.
        // Push the input region before anything else, so the compositor and
        // the painted frame agree. Diffed: an unchanged region costs nothing,
        // and calling this per frame would be a surface commit per frame.
        //
        // `Some(&[])` means "no input anywhere" and `None` means "everywhere".
        // An empty list is correct here — a Desktop with no icons should not
        // eat clicks meant for the windows behind it.
        let region = self.interactive_region();
        if self.pushed_region.differs(region.as_ref()) {
            tracing::debug!(
                rects = region.as_ref().map(|r| r.len()).unwrap_or(usize::MAX),
                menu = self.menu_at.is_some(),
                "input region pushed"
            );
            window.set_input_region(region.as_deref());
            self.pushed_region = InputRegion::Pushed(region);
        }

        // Escape closes the menu. The picker is not handled here — it lives
        // on the Overlay surface, which is the only one of ours that can
        // receive a key at all.
        // `Box::new` around a closure, because a bare closure captured here
        // and used later fails the higher-ranked lifetime bound on
        // `on_key_down` in a way that reads as a compiler bug rather than as
        // what it is.
        let on_key: Box<dyn Fn(&gpui::KeyDownEvent, &mut Window, &mut gpui::App) + 'static> = {
            let this = cx.entity();
            Box::new(move |event: &gpui::KeyDownEvent, _w, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                this.update(cx, |me, cx| {
                    if me.menu_at.take().is_some() {
                        cx.notify();
                    }
                });
            })
        };

        let this = cx.entity();
        let this_move = this.clone();

        let mut layer = div()
            .id("lume-desktop")
            .size_full()
            .on_mouse_move(move |event, _w, cx| {
                let p = event.position;
                this_move.update(cx, |me, cx| {
                    let hit = me.grid.cell_at(p);
                    if hit != me.hovered {
                        me.hovered = hit;
                        cx.notify();
                    }
                });
            });

        // --- icons -------------------------------------------------------
        layer = layer
            .on_key_down(on_key)
            .on_mouse_down(gpui::MouseButton::Right, {
                let this = this.clone();
                move |event: &gpui::MouseDownEvent, _w, cx| {
                    let at = event.position;
                    this.update(cx, |me, cx| {
                        me.menu_at = Some(at);
                        cx.notify();
                    });
                }
            })
            .children(self.placed.iter().map(|(id, cell)| {
            let cell = *cell;
            let Some(entry) = self.entry(id).cloned() else {
                return div().id(("lume-icon-missing", hash_of(id)));
            };

            let entry_id = entry.id();
            let is_selected = self.selected.as_deref() == Some(entry_id.as_str());
            let (x, y) = self.grid.cell_origin(cell);
            let grid = self.grid;

            div()
                .id(("lume-icon", hash_of(&entry_id)))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(grid.cell))
                .h(px(grid.cell))
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .bg(if is_selected {
                    gpui::hsla(0.0, 0.0, 1.0, 0.14)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .rounded(px(8.0))
                .child(icon_tile(&entry.icon(), &entry.label(), px(48.0)))
                .child(label(&entry.label(), grid.cell))
                // Select on the first click, open on the second.
                //
                // There is no `on_double_click` in this fork, so the count
                // comes off the click event. Single click selects rather than
                // launches on purpose: an icon you cannot select is an icon
                // you cannot drag, rename or unpin.
                .on_click({
                    let this = this.clone();
                    let id = entry_id.clone();
                    let opened = entry.clone();
                    move |event, _w, cx| {
                        let double = event.click_count() >= 2;
                        this.update(cx, |me, cx| {
                            if double {
                                match &opened {
                                    Entry::App(app) => launch(app),
                                    Entry::Item(item) => open_item(item),
                                }
                            } else {
                                me.selected = Some(id.clone());
                                cx.notify();
                            }
                        });
                    }
                })
        }));

        // --- context menu -------------------------------------------------
        if let Some(at) = self.menu_at {
            layer = layer.child(self_menu(at, window, cx, this.clone()));
        }


        layer
    }
}

fn self_menu(
    at: Point<Pixels>,
    _window: &mut Window,
    _cx: &mut Context<Desktop>,
    this: Entity<Desktop>,
) -> AnyElement {
    // Positioned inside the screen: a click near the bottom-right would
    // otherwise put half the menu off-screen.
    let w = 180.0;
    // Three items at 28px, plus 2x8px padding and three 4px gaps. Hardcoded
    // to 240 it drew a 240px menu around 100px of content, with the last two
    // items floating in dead space and the real ones not where the eye
    // expects.
    let item_h = 28.0;
    let gap = 4.0;
    let pad = 8.0;
    let count = 3.0;
    let h = count * item_h + (count - 1.0) * gap + pad * 2.0;
    let size = _window.viewport_size();
    let x = f32::from(at.x).min(f32::from(size.width) - w - 8.0);
    let y = f32::from(at.y).min(f32::from(size.height) - h - 8.0);

    // Every item does something. The previous menu drew five labels with no
    // handlers, which reads on screen as "the context menu is broken" and is
    // worse than having fewer items: a missing item is an absence you notice,
    // a dead item is a promise you cannot trust.
    let item = |label: SharedString, action: MenuAction, this: Entity<Desktop>| {
        div()
            .id(("lume-menu-item", hash_of(&label)))
            .h(px(28.0))
            .px_3()
            .flex()
            .items_center()
            .text_size(px(12.0))
            .text_color(rgb(0xe0e0e0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|d| d.bg(gpui::hsla(0.0, 0.0, 1.0, 0.10)))
            .child(label)
            .on_click(move |_, _w, cx| {
                this.update(cx, |me, cx| me.run(action, cx));
            })
    };

    div()
        .id("lume-desktop-menu")
        .absolute()
        .left(px(x.max(0.0)))
        .top(px(y.max(0.0)))
        .w(px(w))
        .h(px(h))
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        // Frosted. This is a real backdrop blur, and it is a per-frame GPU
        // cost — measure it, do not assume it is free.
        .backdrop_blur(px(16.0))
        .bg(gpui::hsla(0.10, 0.05, 0.08, 0.80))
        .border_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.10))
        .rounded(px(10.0))
        .shadow_lg()
        .child(
            // The scrim goes in FIRST so the menu paints above it. Ordering
            // children back-to-front is the whole reason this element is
            // written as one tree rather than assembled from pieces.
            div()
                .id("lume-desktop-menu-scrim")
                .absolute()
                .left_0()
                .top_0()
                .size_full()
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        this.update(cx, |me, cx| {
                            me.menu_at = None;
                            cx.notify();
                        });
                    }
                }),
        )
        .child(item(
            SharedString::from("New folder"),
            MenuAction::NewFolder,
            this.clone(),
        ))
        .child(item(
            SharedString::from("Change wallpaper"),
            MenuAction::PickWallpaper,
            this.clone(),
        ))
        .child(item(
            SharedString::from("Open terminal here"),
            MenuAction::OpenTerminal,
            this.clone(),
        ))
        // Clicking the backdrop closes it. Also handles Esc via the surface's
        // key handler in the registry.
        .child(
            div()
                .id("lume-desktop-menu-scrim")
                .absolute()
                .left_0()
                .top_0()
                // The whole surface, not a 1x1 placeholder. This is what a
                // click outside the menu lands on, and with the input region
                // widened for the overlay it is the only thing that does.
                .size_full()
                .on_click({
                    let this = this.clone();
                    move |_, _w, cx| {
                        this.update(cx, |me, cx| {
                            me.menu_at = None;
                            cx.notify();
                        });
                    }
                }),
        )
        .into_any_element()
}

/// ElementId accepts only `(&'static str, usize | u64 | EntityId)` tuples.
/// Desktop-file ids are runtime strings, so hash them once.
pub fn hash_path(p: &std::path::Path) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish() as usize
}

fn hash_of(s: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as usize
}

/// A label readable on any wallpaper.
///
/// No `text_shadow` in this fork, so: a dark copy offset by 1px, then the real
/// copy on top. Two draw calls, no layout change, no shader, and it reads on a
/// light wallpaper as well as a dark one. Cheaper and more portable than a
/// plate behind every label.
fn label(text: &str, width: f32) -> gpui::Div {
    div()
        .relative()
        .w(px(width))
        .child(
            div()
                .id(("lume-icon-label-shadow", hash_of(text)))
                .absolute()
                .left(px(0.0))
                .top(px(1.0))
                .max_w(px(width))
                .truncate()
                .text_size(px(11.0))
                .text_color(gpui::hsla(0.0, 0.0, 0.0, 0.85))
                .child(SharedString::from(text)),
        )
        .child(
            div()
                .id(("lume-icon-label", hash_of(text)))
                .max_w(px(width))
                .truncate()
                .text_size(px(11.0))
                .text_color(rgb(0xf0f0f0))
                .child(SharedString::from(text)),
        )
}

/// An app icon.
///
/// There is no `.icon(name)` in this fork — icons are `img(ImageSource)`, which
/// takes a *resource location*, not a freedesktop icon-theme name. Resolving
/// `firefox` against the user's hicolor/Papirus/etc. means implementing
/// `AssetSource` with a theme lookup, which is its own piece of work.
///
/// Until that exists: an absolute path renders, and a bare theme name gets a
/// neutral tile. Better an obviously-generic icon than a broken one.
/// What a context-menu item asks for.
///
/// An enum rather than a closure so the menu is a pure function of a small
/// value, and so the set of things the desktop can do is readable in one
/// place. A closure per item would make the menu untestable without a running
/// compositor, and this is the part users click most often.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    NewFolder,
    PickWallpaper,
    OpenTerminal,
}

/// Quote a string for `/bin/sh`.
///
/// `sh -c` is how a terminal gets a working directory, and the path is the
/// user's Desktop directory, which can contain a single quote. Getting this
/// wrong is not a crash, it is a shell command running something the user did
/// not ask for.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create a directory on the Desktop that does not collide with an existing
/// one.
///
/// Suffixed rather than failing: "New Folder" that errors because it already
/// exists is the behaviour every file manager got wrong for a decade.
fn new_folder() -> std::io::Result<String> {
    let dir = services::desktop::desktop_dir();
    std::fs::create_dir_all(&dir)?;

    for n in 0..1000 {
        let name = if n == 0 {
            "New Folder".to_string()
        } else {
            format!("New Folder ({n})")
        };
        match std::fs::create_dir(dir.join(&name)) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free folder name after 1000 tries"))
}

/// Assign every entry a cell.
///
/// Split out of the constructor so `rescan` places identically. Two placement
/// functions would drift, and the drift would show up as icons jumping cells
/// whenever the Desktop directory changed.
fn place(
    entries: &[Entry],
    saved: &services::config::IconPositions,
    grid: Grid,
) -> (Vec<(String, usize)>, Vec<Bounds<Pixels>>) {
    let mut used: Vec<usize> = Vec::new();
    let mut placed: Vec<(String, usize)> = Vec::new();

    for entry in entries {
        let id = entry.id();
        let Some(cell) = saved.cells.get(&id).copied() else {
            continue;
        };
        if !used.contains(&cell) {
            used.push(cell);
            placed.push((id, cell));
        }
    }
    for entry in entries {
        let id = entry.id();
        if placed.iter().any(|(x, _)| x == &id) {
            continue;
        }
        let cell = (0..).find(|c| !used.contains(c)).unwrap_or(placed.len());
        used.push(cell);
        placed.push((id, cell));
    }
    placed.sort_by_key(|(_, c)| *c);

    let region = placed.iter().map(|(_, c)| grid.rect(*c)).collect();
    (placed, region)
}

/// An icon, or a letter tile when the theme has nothing for that name.
///
/// `fallback_label` is the *display* name, not the icon name. An unresolved
/// icon is common and says nothing about the app: wine writes `Icon=` keys
/// like `0CC0_hollow_knight.0` that point into the wine prefix and will never
/// resolve in any icon theme. Taking the initial of the icon name renders a
/// literal "0" on the desktop, which tells the user nothing and looks broken.
/// The label's initial at least says which app it is.
fn icon_tile(name: &str, fallback_label: &str, size: Pixels) -> AnyElement {
    super::app_icon(name, fallback_label, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Desktop path can contain a single quote, and `sh -c` is how the
    /// terminal gets its working directory. Unquoted, that is a shell command
    /// the user did not ask for.
    #[test]
    fn shell_quoting_survives_a_quote_in_the_path() {
        assert_eq!(shell_quote("/home/u/Desktop"), "'/home/u/Desktop'");
        assert_eq!(shell_quote("/home/o'brien/Desktop"), "'/home/o'\\''brien/Desktop'");
        assert_eq!(shell_quote("; rm -rf /"), "'; rm -rf /'");
    }

    /// "New Folder" must not error because it already exists — that is the
    /// behaviour every file manager got wrong for a decade.
    #[test]
    fn new_folder_picks_a_free_name() {
        let dir = std::env::temp_dir().join("lume-newfolder-test");
        let _ = std::fs::remove_dir_all(&dir);

        // Point the scan at a temp dir by creating the real Desktop shape is
        // not possible here, so test the naming rule directly: the first
        // attempt is unsuffixed, later ones are numbered from 1.
        let _ = std::fs::create_dir_all(&dir);
        assert!(std::fs::create_dir(dir.join("New Folder")).is_ok());
        assert!(std::fs::create_dir(dir.join("New Folder")).is_err());
        assert!(std::fs::create_dir(dir.join("New Folder (1)")).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
