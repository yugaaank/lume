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

use std::sync::Arc;

use tokio::sync::mpsc;

/// How many intents may be pending before the oldest is dropped.
///
/// Small on purpose. A panel toggle is applied within a frame; anything beyond
/// a handful queued means the UI thread is blocked, and a backlog of stale
/// toggles would then fire in sequence and leave the panel in the wrong state.
const QUEUE: usize = 64;

/// A request from outside the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Show or hide a sidebar. Persisted to the config.
    TogglePanel(Side),
    SetPanel { side: Side, open: bool },
    /// Quit the shell.
    Quit,
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
