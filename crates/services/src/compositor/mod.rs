//! Compositor abstraction.
//!
//! Hyprland only, for now. The trait exists so the service layer can be
//! exercised without a running compositor, not for portability — adding a
//! second compositor means adding a backend here, not changing callers.
//!
//! Verified against Hyprland 0.56.2 on this machine. The IPC surface has moved:
//!   - queries are bare command names, NOT `/workspaces`:
//!     `hyprctl -j workspaces` (the old `/workspaces` returns "unknown request")
//!   - dispatch is Lua, NOT the legacy dispatcher language:
//!     `hyprctl dispatch hl.dsp.focus({workspace = 3})`
//!     (`hyprctl dispatch workspace 3` is a Lua syntax error on 0.56.x)

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod detect;
pub mod hyprland;
pub mod service;

pub use service::CompositorService;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    /// Hyprland workspace id. Negative values are the special workspaces.
    pub special: bool,
    pub windows: usize,
}

impl Workspace {
    pub fn is_special(&self) -> bool {
        self.id < 0
    }
}

/// One open window.
///
/// This is what the dock groups by. Without it a dock can only show pins, so it
/// is a launcher with extra steps — running apps have no representation and
/// "which window do I click" has no answer.
///
/// `class` is the compositor's window class, *not* a `.desktop` id. Ryoku keys
/// the whole dock by class for exactly this reason: it is the one identifier
/// Hyprland gives for a live window, and matching it to a `.desktop` id needs
/// `StartupWMClass`, which not every app fills in. Mapping class to app is
/// therefore a separate, deliberately fallible step, and the dock must render
/// correctly when it fails.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Client {
    /// Hyprland's window address, e.g. `0x55d1…`. Stable for the window's life
    /// and the only safe key for focus and close.
    pub address: String,
    /// Window class, e.g. `kitty`. Empty classes are dropped rather than stored:
    /// a client with no class cannot be grouped, and storing it would make every
    /// count off by one.
    pub class: String,
    /// Workspace name this window is on.
    pub workspace: String,
    /// Is this the focused window?
    pub active: bool,
}

#[derive(Debug, Clone)]
pub enum Action {
    FocusWorkspace { id: i64 },
    ToggleSpecialWorkspace,
    FloatWindow { address: String },
    CloseWindow { address: String },
    Quit,
}

#[derive(Debug, Clone)]
pub enum CompositorEvent {
    /// Cheap signal: something changed, go re-read. We do not parse the event
    /// body into a state change, because the poll is the source of truth and
    /// the stream is only a nudge. That removes a whole class of drift bugs.
    Changed,
}

/// Where Hyprland's event socket lives.
///
/// Hyprland 0.56 moved the socket into a per-instance *directory* under
/// $XDG_RUNTIME_DIR/hypr/, with the event stream at `.socket2.sock` inside it.
/// Older versions put a flat `.socket2.sock` directly under hypr/. Probe both;
/// do not guess.
pub fn hypr_socket_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            // 0.56+ layout: a directory named after the signature.
            out.push(
                PathBuf::from(&runtime)
                    .join("hypr")
                    .join(&sig)
                    .join(".socket2.sock"),
            );
            // Older flat layout, still probed because it costs one stat().
            out.push(PathBuf::from(&runtime).join("hypr").join(&sig).join(".socket2.sock"));
        }
        // Some setups relocate to /tmp/hypr.
        out.push(PathBuf::from("/tmp/hypr").join(&sig).join(".socket2.sock"));
    }

    out
}

pub trait Compositor: Send + Sync {
    /// Read the current workspace list.
    fn workspaces(&self) -> anyhow::Result<Vec<Workspace>>;

    /// Read the focused workspace.
    fn active_workspace(&self) -> anyhow::Result<Workspace>;

    /// Read the open windows.
    ///
    /// Sorted by address so the order is stable between reads. Stability is not
    /// cosmetic: the dock diffs its running list against the pinned one, and an
    /// unstable order makes icons jump between reads for no reason the user did.
    fn clients(&self) -> anyhow::Result<Vec<Client>> {
        Ok(Vec::new())
    }

    fn act(&self, action: Action) -> anyhow::Result<()>;

    /// Does `workspace` hold at least one non-floating window?
    ///
    /// The dock's "dodge" mode keys off this. Floating windows are excluded
    /// deliberately: a workspace holding only a floating terminal reads as
    /// empty on screen, and having the dock fill it is the point.
    fn has_tiled_windows(&self, _workspace: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Output refresh rate in Hz, for the animation frame interval.
    fn refresh_rate(&self) -> f64 {
        60.0
    }

    fn frame_duration(&self) -> Duration {
        // Clamp: a broken reading should not divide by zero or produce a
        // thousand-frame-per-second animation loop.
        Duration::from_secs_f64(1.0 / self.refresh_rate().clamp(24.0, 360.0))
    }
}