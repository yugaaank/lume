# lume

A Wayland shell built in Rust on GPUI, the UI toolkit that powers Zed. It is a single process that owns a small set of layer surfaces: a bar, a dock, a background, and overlay surfaces for the launcher, notifications, and system menus. The goal is a complete desktop shell that looks sharp, uses little memory, and stays responsive.

---

## What this is

- **Rust + GPUI**: the same GPU-accelerated toolkit Zed uses, running on wgpu and Vulkan
- **One process, five surfaces**: bar, dock, background, left/right sidebars (optional), overlay — no per-surface processes, no IPC mesh
- **Hyprland only**: the compositor abstraction is intentionally narrow; the shell talks to Hyprland over its UNIX socket
- **No Qt, no QML, no JavaScript**: the entire UI is Rust, compiled ahead of time

---

## Why it looks good

GPUI renders with a custom shader pipeline. Corners, shadows, blurs, and motion are drawn on the GPU, not faked with CSS. The bar and dock use a shared shape system: one renderer, four form parameters (full, fit, islands, notch). The launcher and overlays animate with spring physics that feel deliberate, not decorative. Color, spacing, and typography come from a single theme file — change it once, the whole shell updates.

---

## Memory efficiency

A single-process architecture avoids the overhead of per-surface processes, IPC channels, and language runtimes. GPUI only draws a window when its invalidator is dirty — idle surfaces cost zero CPU. No interpreter, no JIT, no background daemons. The binary is ~25 MB on disk. Memory scales with active surfaces, not feature count.

---

## Feature checklist

### Stage 0 — Setup
- [x] Workspace builds, GPUI pinned to verified fork commit

### Stage 1 — Bar
- [x] Top bar implemented with different states (workspaces, system tray, clock, battery, network)
- [x] Multiple bar shapes available (full, fit, islands)
- [x] Widget registry for settings and popups
- [x] Popup interaction model (left-click toggle, right-click action, wheel adjust, hover tooltip)

### Stage 2 — Compositor abstraction
- [x] Hyprland IPC client for workspace and window management
- [x] CLI verbs for workspace control
- [x] Bar updates on workspace change
- [x] Event socket integration for cursor position

### Stage 3 — Surface registry
- [x] Five surfaces created at startup in fixed map order
- [x] Background, bar, dock, left/right sidebars
- [x] Hidden surfaces reserve zero space
- [x] Layer order verified with compositor

### Stage 4 — Background engine
- [ ] Wallpaper with multiple display modes
- [ ] Per-monitor wallpaper support
- [ ] Draggable desktop icons from .desktop files
- [ ] Icon grid with snap and context menu
- [ ] Wallpaper picker overlay

### Stage 5 — Sidebars + Dock
- [ ] Sidebar content (quick settings, workspace list, pinned apps)
- [ ] Dock with running-app indicators and pin/unpin
- [ ] Autohide with peek strip and slide animation
- [ ] Seal visual style
- [ ] Activation semantics (launch, cycle, focus)
- [ ] Starter pins from installed apps

### Stage 6 — Overlays
- [ ] Launcher (fuzzy search, inline calculator, keyboard navigation)
- [ ] OSD (volume, brightness, keyboard layout)
- [ ] Notifications (history, actions, grouping)
- [ ] Clipboard history
- [ ] Screenshot UI (region, window, output)

### Stage 7 — Services
- [ ] Audio (volume, mute, device list, per-app)
- [ ] Network (Wi-Fi scan, connect, VPN, Ethernet)
- [ ] Bluetooth (pair, connect, battery)
- [ ] Displays (mode, scale, transform, arrangement)
- [ ] Battery (percentage, state, time remaining, health)
- [ ] MPRIS media player controls

### Stage 8 — Theme + Performance policy
- [ ] Single palette file with semantic tokens
- [ ] Light/dark toggle
- [ ] Performance policy with frame budget guard

### Stage 9 — Hardening
- [ ] Isolation checks (no unsafe outside FFI, no unwrap on external input)
- [ ] Exec safety (no shell interpolation, command allowlist)
- [ ] Dependency audit

### Stage 11 — Settings app (separate process)
- [ ] Eight panels for all shell components
- [ ] Spawns on demand, writes config, signals reload

### Stage 12 — Power menu
- [ ] Logout, reboot, shutdown, sleep, lock

---

## What is explicitly not in scope

- Lock screen (later)
- Greeter (autologin on this machine)
- Second compositor backend
- Command palette (launcher covers it)
- Window switcher (workspace pips + dock cover it)
- Audio visualizer
- Clipboard persistence across reboots
- Distribution packaging

---

## Building

```bash
# Requires: Rust stable, wgpu dependencies (Vulkan SDK or equivalent), Hyprland 0.56+
cargo build --release --bin lume
```

Run `lume` from a Hyprland session. The bar appears on every output. `SUPER+1..9` switches workspaces. `SUPER+SPACE` opens the launcher (once stage 6 lands).

---

## Configuration

`~/.config/lume/config.toml` — created on first run with defaults. The settings app (stage 11) edits it. A malformed file logs an error and falls back to defaults; check the log if something looks wrong.

---

## Acknowledgments

- **[GPUI](https://github.com/zed-industries/zed)** — the UI toolkit from Zed Industries, without which this shell would not exist
- **[Hyprland](https://github.com/hyprwm/Hyprland)** — for a compositor with a clean, scriptable IPC surface
- **[Capsule](https://github.com/moiCR/Capsule)** — reference architecture for a single-process, single-surface GPUI shell
- **[smithay](https://github.com/Smithay/smithay)** — foundational Wayland primitives and layer-shell implementation
- **[Ryoku](https://github.com/Ryoku-dev/ryoku)** — interface discipline, capability-gated compositor abstraction, and proof that a dense, feature-rich bar and dock can run efficiently

---

## License

MIT