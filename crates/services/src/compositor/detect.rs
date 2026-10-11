//! Runtime detection.
//!
//! The one place allowed to name a compositor. `bin/lume-dev-verify-wm-isolation`
//! fails the build if a name leaks out of `compositor/`.

use std::sync::Arc;

use anyhow::{bail, Context as _};

use super::{Compositor, hyprland::Hyprland};

pub fn detect() -> anyhow::Result<Arc<dyn Compositor>> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();

    if desktop.split(':').any(|p| p.eq_ignore_ascii_case("hyprland")) {
        return prove_hyprland().context("XDG_CURRENT_DESKTOP says Hyprland, but");
    }

    // Fall back to the session handle — but PROVE it. A terminal that outlived
    // its compositor still carries HYPRLAND_INSTANCE_SIGNATURE, and hyprctl
    // will happily run against a dead socket. Env presence is not proof of a
    // live session; a successful roundtrip is.
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return prove_hyprland().context(
            "HYPRLAND_INSTANCE_SIGNATURE is set but Hyprland did not answer — \
             this is likely a stale handle inherited from a dead session",
        );
    }

    bail!(
        "no supported compositor (XDG_CURRENT_DESKTOP={desktop:?}). \
         Hyprland is the only supported compositor. Add others in \
         crates/services/src/compositor/detect.rs"
    );
}

/// A compositor that answers `activeworkspace` is alive. Constructing the
/// backend proves nothing on its own.
fn prove_hyprland() -> anyhow::Result<Arc<dyn Compositor>> {
    let compositor: Arc<dyn Compositor> = Arc::new(Hyprland);
    compositor
        .active_workspace()
        .context("Hyprland did not answer an IPC query")?;
    Ok(compositor)
}