//! The compositor service: current state, cheap to read, with a change signal.
//!
//! Two loops on purpose:
//!   - a slow poll, which is the source of truth and recovers from anything
//!     the event stream missed
//!   - a fast event stream, which is only a nudge to re-read
//!
//! The stream deliberately does not carry state. Both parsing events and
//! polling is how a shell ends up disagreeing with its own compositor.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::Notify;

use super::{Compositor, Workspace, hyprland};

/// A nudge is coalesced, not counted.
///
/// `notify_one`, never `notify_waiters`: waiters that are not *currently*
/// awaiting are dropped by `notify_waiters`, and the dock spends almost all of
/// its time in the floor sleep rather than awaiting. Switching that one call
/// made the dock's dodge mode look dead — the event arrived, nobody was
/// listening, and it was discarded. `notify_one` stores a permit, so the next
/// `notified()` returns at once and the coalescing is exactly right: "go
/// look" is idempotent, so one pending permit loses nothing.
///
/// How often the poll re-reads state even when nothing has changed.
///
/// Not a substitute for the event stream — it is the floor that guarantees the
/// bar is eventually correct after a dropped event, a missed socket, or a
/// compositor restart.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct CompositorService {
    inner: Arc<dyn Compositor>,
    workspaces: Arc<ArcSwap<Vec<Workspace>>>,
    active: Arc<ArcSwap<Workspace>>,
    changed: Arc<Notify>,
}

impl CompositorService {
    /// `handle` is required rather than optional.
    ///
    /// The event listener runs on a plain `std::thread`, which has no ambient
    /// Tokio context — calling `tokio::spawn` from it panics with "there is no
    /// reactor running". An explicit Handle removes the dependency on ambient
    /// context entirely, which is the whole class of bug.
    pub fn new(handle: tokio::runtime::Handle) -> anyhow::Result<Self> {
        let inner = super::detect::detect()?;

        let active = inner.active_workspace().unwrap_or_else(|_| Workspace {
            id: 1,
            name: "1".into(),
            special: false,
            windows: 0,
        });

        let workspaces = Arc::new(ArcSwap::from_pointee(inner.workspaces().unwrap_or_default()));
        let active = Arc::new(ArcSwap::from_pointee(active));
        let changed = Arc::new(Notify::new());

        // Log before spawning, while the originals are still intact. Every
        // value below is cloned for a loop that outlives this function.
        tracing::info!(
            workspaces = workspaces.load().len(),
            frame = ?inner.frame_duration(),
            "compositor service up"
        );

        let state = Self { inner, workspaces, active, changed };

        spawn_poll_loop(
            handle.clone(),
            state.inner.clone(),
            state.workspaces.clone(),
            state.active.clone(),
            state.changed.clone(),
        );
        spawn_event_loop(
            handle,
            state.inner.clone(),
            state.workspaces.clone(),
            state.active.clone(),
            state.changed.clone(),
        );

        Ok(state)
    }

    /// Cheap: an atomic pointer load. Called from render(), must never block.
    pub fn workspaces(&self) -> Arc<Vec<Workspace>> {
        self.workspaces.load_full()
    }

    pub fn active(&self) -> Arc<Workspace> {
        self.active.load_full()
    }

    /// Clone this once and hold the clone, not the service.
    pub fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    pub fn frame_duration(&self) -> Duration {
        self.inner.frame_duration()
    }

    /// Does the active workspace hold a tiled window?
    ///
    /// Reads the active workspace live rather than from `self.active`, which
    /// only the 5 s poll refreshes. Using the cache here made the dock's dodge
    /// mode lag a workspace switch by up to 5 s: the event arrived instantly
    /// and was then answered against the workspace the user had just left.
    ///
    /// Two `hyprctl` calls, and only when the event stream says something
    /// changed. Caching this would make the poll interval the dock's reaction
    /// time, which is the wrong trade for a layout that is already visible.
    pub fn has_tiled_windows(&self) -> bool {
        let active = match self.inner.active_workspace() {
            Ok(w) => w,
            Err(e) => {
                tracing::debug!("active_workspace failed: {e:#}");
                return false;
            }
        };
        let result = self.inner.has_tiled_windows(&active.name);
        // Logged with the workspace name: "the dock is wrong" is not a
        // debuggable report without knowing which workspace it asked about.
        match &result {
            Ok(tiled) => tracing::debug!(workspace = %active.name, tiled, "dodge query"),
            Err(e) => tracing::debug!(workspace = %active.name, error = %e, "dodge query failed"),
        }
        result.unwrap_or(false)
    }

    pub fn act(&self, action: super::Action) -> anyhow::Result<()> {
        self.inner.act(action)
    }
}

fn spawn_poll_loop(
    handle: tokio::runtime::Handle,
    compositor: Arc<dyn Compositor>,
    workspaces: Arc<ArcSwap<Vec<Workspace>>>,
    active: Arc<ArcSwap<Workspace>>,
    changed: Arc<Notify>,
) {
    handle.spawn(async move {
        loop {
            // Errors here are logged and ignored. A shell that exits because
            // one query failed is worse than a shell with slightly stale data.
            if let Ok(list) = compositor.workspaces() {
                workspaces.store(Arc::new(list));
                changed.notify_one();
            } else {
                tracing::debug!("workspace poll failed");
            }

            if let Ok(current) = compositor.active_workspace() {
                if **active.load() != current {
                    active.store(Arc::new(current));
                    changed.notify_one();
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

fn spawn_event_loop(
    handle: tokio::runtime::Handle,
    _compositor: Arc<dyn Compositor>,
    workspaces: Arc<ArcSwap<Vec<Workspace>>>,
    active: Arc<ArcSwap<Workspace>>,
    changed: Arc<Notify>,
) {
    // The socket listener is blocking, so it gets its own thread rather than a
    // tokio worker. It holds no locks and only touches ArcSwap/Notify.
    std::thread::spawn(move || {
        loop {
            // Re-read on the tokio pool so a blocking hyprctl call never
            // stalls the socket reader. FnMut because listen_events calls it
            // once per line, and a busy compositor emits bursts.
            let mut refresher = {
                let workspaces = workspaces.clone();
                let changed = changed.clone();
                let active = active.clone();
                // FnMut: called once per event line. Capture the Handle by
                // reference and clone per call, or the first call moves it and
                // the second fails to compile — or worse, panics at runtime.
                let handle = handle.clone();
                move || {
                    handle.clone().spawn({
                        let workspaces = workspaces.clone();
                        let active = active.clone();
                        let changed = changed.clone();
                        async move {
                            if let Ok(list) = hyprland::Hyprland.workspaces() {
                                workspaces.store(Arc::new(list));
                            }
                            if let Ok(current) = hyprland::Hyprland.active_workspace() {
                                active.store(Arc::new(current));
                            }
                            changed.notify_one();
                        }
                    });
                }
            };

            if let Err(e) = hyprland::listen_events(&mut refresher) {
                tracing::warn!("hyprland event loop: {e:#}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}
