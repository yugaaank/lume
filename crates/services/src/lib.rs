//! System integration. Knows nothing about UI.
//!
//! The split is deliberate: this crate can be tested with no compositor and no
//! display, which is what makes `cargo test -p services` meaningful.

pub mod compositor;
pub mod ipc;

pub use compositor::{Action, CompositorService, Workspace};
pub use ipc::{Command, decode, handle_default, send, serve, socket_path, spawn_detached};

use gpui::Global;
use std::sync::Arc;

/// The service registry, installed once as a GPUI global.
///
/// Clone is cheap (one Arc bump) and lets a view hold the service handles it
/// needs, instead of re-borrowing the global on every call.
#[derive(Clone)]
pub struct AppState {
    pub compositor: Arc<CompositorService>,
}

impl Global for AppState {}

impl AppState {
    /// The compositor propagates: no compositor means no shell, and failing
    /// loudly with a clear message beats a shell that quietly does nothing.
    /// Everything else falls back internally.
    pub fn new(handle: tokio::runtime::Handle) -> anyhow::Result<Self> {
        Ok(Self {
            compositor: Arc::new(CompositorService::new(handle)?),
        })
    }
}