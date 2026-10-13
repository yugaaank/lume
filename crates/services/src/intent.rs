//! Intents: the shell's IPC thread talking to its UI thread.
//!
//! ## Why this exists
//!
//! The IPC server is a blocking socket reader on its own thread. The UI lives
//! on GPUI's thread and nothing but `cx` may touch it. So "toggle the left
//! panel" arrives on one thread and has to be applied on the other.
//!
//! The obvious workarounds both lose:
//!
//! - **A global registry of window handles.** Possible here — `Global` is only
//!   `'static`, so a `RefCell` is fine — but it makes the registry a *second*
//!   owner of surface state alongside the view that renders it. That is
//!   exactly the drift that made the dock reserve space it was not drawing.
//! - **A closure per command.** `handle_default` would need a different
//!   signature for every feature, and the set grows with every feature.
//!
//! So: the IPC thread sends a value, the UI thread applies it. One channel,
//! one queue, one owner of the UI. Every later feature that starts outside the
//! UI — keybinds, the compositor event stream, the clipboard watcher, a media
//! key — goes through the same door instead of inventing its own.
//!
//! ## Applied, not stored
//!
//! An intent is a request to change something, not the change itself. The
//! handler writes the config, and the surface reads it back. Nothing caches a
//! second copy of the truth, so a panel cannot be half-toggled.
//!
//! ## Not an unbounded log
//!
//! The queue is bounded and drops the oldest intent when full. A user mashing
//! a keybind must not be able to grow the shell's memory without limit, and
//! every intent here is idempotent — the newest one is the only one that
//! matters. Dropping the oldest is safe for exactly that reason and would not
//! be for a queue of side-effecting commands.

use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;

/// How many intents may be pending before the oldest is dropped.
///
/// Small on purpose. A panel toggle is applied within a frame; anything beyond
/// a handful queued means the UI thread is blocked, and a backlog of stale
/// toggles would then fire in sequence and leave the panel in the wrong state.
const QUEUE: usize = 64;

/// A request from outside the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Show or hide a sidebar. Persisted to the config.
    TogglePanel(Side),
    SetPanel { side: Side, open: bool },
    /// Open, close or toggle the wallpaper picker.
    Picker(PickerOp),
    /// Open, close or toggle the widget menu.
    ///
    /// The anchor travels in the intent rather than being re-derived at open
    /// time: the pointer position at click time is the most accurate answer
    /// available, and re-measuring later is not.
    Drawer {
        widget: Option<String>,
        anchor: Anchor,
        op: DrawerOp,
    },
    /// Open or close the launcher.
    ///
    /// Not a `Toggle`: the launcher is modal and owns the keyboard while it is
    /// up, so a keybind that fired a toggle could be re-entered by a repeat
    /// and close it again. Explicit verbs cannot do that.
    Launcher(LauncherOp),
    /// Quit the shell.
    Quit,
}

/// Where a popup should sit horizontally.
///
/// This was an `Option<f32>` holding a widget's centre, which made three
/// different situations look like the same absence of data: a click on a
/// widget, a keybind with no widget behind it, and an explicit request to
/// dock to one edge. Naming them means a new caller has to pick one, and the
/// renderer's fallback stops being a guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Anchor {
    /// Screen x of the widget's centre, in pixels. The click path.
    ///
    /// A bare `f32` rather than a quantised enum, so this variant cannot
    /// derive `Eq` — and it should not. A `PartialEq` is all the tests need,
    /// and comparing floats for equality is a trap.
    At(f32),
    /// No pointer: dock to an edge.
    ///
    /// The keybind path. A popup that opened without a widget to point at has
    /// to go somewhere, and which side matters for muscle memory — a keybind
    /// should open near where the user is looking.
    Edge(Edge),
}

/// The screen edge a popup docks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
}

impl Edge {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            _ => None,
        }
    }
}

/// What to do about the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherOp {
    Open,
    Close,
}

impl LauncherOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" | "open" => Some(LauncherOp::Open),
            "off" | "close" => Some(LauncherOp::Close),
            // "toggle" is deliberately absent. See the variant's comment.
            _ => None,
        }
    }
}

/// What to do about the widget menu.
///
/// Unlike [`LauncherOp`] this does have a `Toggle`, because the click path
/// genuinely wants one and the click path cannot auto-repeat. The keybind
/// path is given the same verb by the binding generator, but Hyprland's
/// key repeat only fires on held keys, and a held key re-entering the toggle
/// is the exact failure `LauncherOp` refuses to allow. Bindings therefore
/// use explicit verbs; see the `bindings` note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerOp {
    Open,
    Close,
    /// Show the named menu, or close it if that menu is already the open one.
    ///
    /// Toggling a *different* menu switches to it rather than closing — only
    /// the menu that is actually open is ever dismissed.
    Toggle,
}

impl DrawerOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" | "open" => Some(DrawerOp::Open),
            "off" | "close" => Some(DrawerOp::Close),
            "toggle" => Some(DrawerOp::Toggle),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DrawerOp::Open => "open",
            DrawerOp::Close => "close",
            DrawerOp::Toggle => "toggle",
        }
    }
}

/// What to do about the wallpaper picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOp {
    Open,
    Close,
    Toggle,
}

impl PickerOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" | "open" => Some(PickerOp::Open),
            "off" | "close" => Some(PickerOp::Close),
            "toggle" => Some(PickerOp::Toggle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Side::Left),
            "right" => Some(Side::Right),
            _ => None,
        }
    }
}

/// The sending half. Cheap to clone, held by whoever produces intents.
#[derive(Clone)]
pub struct IntentSender {
    tx: mpsc::Sender<Intent>,
}

impl IntentSender {
    /// Queue an intent. Returns false when the UI thread is gone, which means
    /// the shell is shutting down and the caller has nothing useful to do.
    pub fn send(&self, intent: Intent) -> bool {
        self.tx.try_send(intent).is_ok()
    }
}

/// Create the channel. The receiver must be driven on the UI thread.
pub fn channel() -> (IntentSender, mpsc::Receiver<Intent>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (IntentSender { tx }, rx)
}

/// Shared, so the IPC thread and any future producer can publish without
/// threading a sender through every call site.
pub type Shared = Arc<IntentSender>;

static INSTALLED: OnceLock<IntentSender> = OnceLock::new();

/// Publish the process-wide sender.
///
/// A view holding a click handler needs to raise an intent without a handle
/// to `main`. This is the same shape as `config::current`: one copy, written
/// once at startup, read from wherever.
///
/// Deliberately sender-only. A global holding the *receiver* would let a
/// render path consume intents, and intents are the shell's control channel.
pub fn install(sender: IntentSender) {
    let _ = INSTALLED.set(sender);
}

/// Fire an intent from anywhere.
///
/// Returns false when the shell has no UI thread left, which only happens
/// during shutdown.
pub fn send(intent: Intent) -> bool {
    INSTALLED
        .get()
        .is_some_and(|tx| tx.send(intent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_round_trip_in_order() {
        let (tx, mut rx) = channel();
        assert!(tx.send(Intent::TogglePanel(Side::Left)));
        assert!(tx.send(Intent::TogglePanel(Side::Right)));
        assert_eq!(rx.try_recv().unwrap(), Intent::TogglePanel(Side::Left));
        assert_eq!(rx.try_recv().unwrap(), Intent::TogglePanel(Side::Right));
    }

    /// The queue must not grow without bound. Filling it and checking the
    /// bound is the only way this is actually enforced.
    #[test]
    fn the_queue_is_bounded_and_drops_the_oldest() {
        let (tx, mut rx) = channel();
        let mut sent = 0;
        let mut accepted = 0;
        // Well past the bound.
        for i in 0..QUEUE * 4 {
            if tx.send(Intent::SetPanel { side: Side::Left, open: i % 2 == 0 }) {
                accepted += 1;
            }
            sent += 1;
        }
        assert!(accepted <= QUEUE, "queue accepted {accepted} of {sent}");
        assert!(accepted > 0, "queue rejected everything; it is not a queue");
    }

    #[test]
    fn side_parsing_is_total() {
        assert_eq!(Side::parse("left"), Some(Side::Left));
        assert_eq!(Side::parse("right"), Some(Side::Right));
        assert_eq!(Side::parse("up"), None);
        assert_eq!(Side::parse(""), None);
        assert_eq!(Side::Left.as_str(), "left");
    }
}
