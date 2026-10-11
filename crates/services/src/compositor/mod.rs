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

    fn act(&self, action: Action) -> anyhow::Result<()>;

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