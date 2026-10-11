//! System integration. Knows nothing about UI.
//!
//! The split is deliberate: this crate can be tested with no compositor and no
//! display, which is what makes `cargo test -p services` meaningful.

pub mod apps;
pub mod compositor;
pub mod config;
pub mod ipc;

pub use compositor::{Action, CompositorService, Workspace};
pub use apps::{AppEntry, AppIndex};
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
    pub config: Arc<config::ConfigService>,
    pub apps: Arc<apps::AppIndex>,
}

impl Global for AppState {}

impl AppState {
    /// The compositor propagates: no compositor means no shell, and failing
    /// loudly with a clear message beats a shell that quietly does nothing.
    /// Everything else falls back internally.
    pub fn new(handle: tokio::runtime::Handle) -> anyhow::Result<Self> {
        let config = Arc::new(config::ConfigService::load()?);
        let apps = Arc::new(apps::AppIndex::new());

        // A .desktop file that vanished should not leave a stale layout entry
        // behind forever.
        let present: Vec<String> = apps.all().iter().map(|a| a.id.clone()).collect();
        if let Err(e) = config.prune_missing_icons(&present) {
            tracing::warn!("could not prune stale icon positions: {e:#}");
        }

        Ok(Self {
            compositor: Arc::new(CompositorService::new(handle)?),
            config,
            apps,
        })
    }
}