//! System integration. Knows nothing about UI.
//!
//! The split is deliberate: this crate can be tested with no compositor and no
//! display, which is what makes `cargo test -p services` meaningful.

pub mod apps;
pub mod compositor;
pub mod battery;
pub mod calc;
pub mod clock;
pub mod config;
pub mod desktop;
pub mod icons;
pub mod intent;
pub mod theme;
pub mod launcher;
pub mod network;
pub mod volume;
pub mod wifi;
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

pub use ipc::handle;

#[cfg(test)]
/// Live probes: assert the readers against this machine, not a fixture.
///
/// A parser can be perfect on a fixture and wrong on the real thing — the
/// `wpctl status` and `/proc/net/wireless` bugs both were. These are
/// `#[ignore]`d so an ordinary `cargo test` stays hermetic and fast; run them
/// with `cargo test -p services -- --ignored`.
mod live {
    #[test]
    #[ignore = "reads the live system"]
    fn reads_this_machine() {
        let battery = crate::battery::Battery::read();
        println!(
            "battery: present={} capacity={:?} charging={} glyph={}",
            battery.present,
            battery.capacity,
            battery.charging,
            battery.glyph()
        );

        let volume = crate::volume::read();
        println!("volume: {volume:?}");

        let ifaces = crate::network::interfaces_from(std::path::Path::new("/sys/class/net"));
        for i in &ifaces {
            println!(
                "net: {} up={} rx={} tx={} visible={}",
                i.name, i.up, i.rx_bytes, i.tx_bytes, i.visible()
            );
        }
        for (name, sig) in crate::network::signals() {
            println!("signal: {name} link={} dbm={} bars={}", sig.link, sig.dbm, sig.bars());
        }

        let now = crate::clock::now();
        println!(
            "clock: {} | {} | {}",
            crate::clock::Format::Time.render(now),
            crate::clock::Format::TimeWithSeconds.render(now),
            crate::clock::Format::WeekdayTime.render(now)
        );

        assert!(battery.present, "no battery on this machine");
        assert!(volume.is_some(), "wpctl returned nothing parseable");
        assert!(!ifaces.is_empty(), "no interfaces read");
    }
}

#[cfg(test)]
mod live_sinks {
    #[test]
    #[ignore = "runs wpctl"]
    fn list() {
        let all = crate::volume::sinks();
        println!("sinks: {}", all.len());
        for s in &all {
            println!(
                "  {:>4}  {:<52} {:.2}  default={} muted={}",
                s.sink_id, s.sink_name, s.level, s.is_default, s.muted
            );
        }
        assert!(!all.is_empty());
    }
}
