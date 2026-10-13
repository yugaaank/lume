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

## Memory and weight

| Metric | Value (idle, one bar surface) |
|--------|-------------------------------|
| Private dirty | 34 MB |
| PSS | 79 MB |
| RSS | 124 MB |
| Threads | 30 |
| Idle CPU | 0.00% |
| File descriptors | 62 |

Five surfaces (bar, dock, background, two sidebars) add ~6 MB private dirty. The binary is 25 MB on disk. No interpreter, no JIT, no background daemons.

---

## Feature checklist

### Stage 0 — Setup
- [x] Workspace builds, GPUI pinned to verified fork commit

### Stage 1 — Bar
- [x] 32 px top bar on every monitor
- [x] Exclusive zone reserved, maximised windows stop below it
- [x] Workspace pips (1–9) with active indicator
- [x] System tray area (StatusNotifierWatcher)
- [x] Clock, battery, volume, network widgets
- [x] Widget registry — single source of truth for settings and popups
- [x] Three bar shapes: full, fit, islands (notch planned)
- [x] Per-widget padding, geometric separators, clamped centre zone
- [x] Popup state map (one per monitor + anchor), left-click toggles, right-click acts, wheel adjusts, hover shows tooltip

### Stage 2 — Compositor abstraction
- [x] Hyprland IPC client (workspace list, active workspace, client list, fullscreen state)
- [x] `lume workspace N` and `lume workspaces` CLI verbs
- [x] Bar updates on workspace change
- [x] Event socket reader (60 Hz cursor position for dock autohide)

### Stage 3 — Surface registry
- [x] Five surfaces created at startup in fixed map order (horizontal edges before vertical)
- [x] Background (layer `Background`, full screen)
- [x] Bar (layer `Top`, 32 px exclusive zone)
- [x] Dock (layer `Top`, 64 px, bottom edge)
- [x] Left sidebar (layer `Top`, 220 px, hidden by default)
- [x] Right sidebar (layer `Top`, 220 px, hidden by default)
- [x] Hidden surfaces reserve zero: `exclusive_zone = 0` + `set_input_region(Some(&[]))`
- [x] `hyprctl layers` order verified, `hyprctl monitors` reserved insets correct

### Stage 4 — Background engine
- [ ] Wallpaper image (fit, fill, stretch, center, tile)
- [ ] Per-monitor wallpaper
- [ ] Draggable desktop icons from `~/Desktop` and `.desktop` files
- [ ] Icon grid with snap, right-click context menu
- [ ] Wallpaper picker overlay (grid preview, search, apply)

### Stage 5 — Sidebars + Dock
- [ ] Sidebar content: quick settings, workspace list, pinned apps
- [ ] Dock: running-app indicators, pin/unpin, click to focus or launch
- [ ] Autohide with 3 px peek strip, slide animation, input mask = band ∪ peek
- [ ] Seal style: square plates, hairline border, radius 0, bone corner bracket on active app
- [ ] Activation semantics: launch if no windows, cycle if focused, otherwise focus (prefer active workspace)
- [ ] Starter pins from installed apps that exist on this machine

### Stage 6 — Overlays
- [ ] Launcher (fuzzy `.desktop` search, inline calculator, keyboard navigation)
- [ ] OSD (volume, brightness, keyboard layout)
- [ ] Notifications (history, actions, grouping)
- [ ] Clipboard history
- [ ] Screenshot UI (region, window, output, copy to clipboard)

### Stage 7 — Services
- [ ] Audio (PipeWire/Pulse: volume, mute, device list, per-app)
- [ ] Network (NetworkManager: Wi-Fi scan, connect, VPN, Ethernet)
- [ ] Bluetooth (BlueZ: pair, connect, battery)
- [ ] Displays (wlr-randr: mode, scale, transform, arrangement)
- [ ] Battery (UPower: percentage, state, time remaining, health)
- [ ] MPRIS media player controls

### Stage 8 — Theme + Performance policy
- [ ] Single palette file (base, surface, accent, semantic tokens)
- [ ] Light/dark toggle, both verified
- [ ] Performance policy: one place that decides what may animate, frame budget guard, CI gate on regression

### Stage 9 — Hardening
- [ ] Isolation grep (no `unsafe` outside FFI shims, no `unwrap` on external input)
- [ ] Exec safety (no shell interpolation, command allowlist)
- [ ] Dependency audit (cargo-audit, deny.toml)

### Stage 11 — Settings app (separate process)
- [ ] Eight panels: bar, dock, background, sidebars, overlays, services, theme, input
- [ ] Spawns on demand, writes `~/.config/lume/config.toml`, signals running shell to reload

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

- **[GPUI](https://github.com/zed-industries/gpui)** — the UI toolkit from Zed Industries, without which this shell would not exist
- **[Hyprland](https://github.com/hyprwm/Hyprland)** — for a compositor with a clean, scriptable IPC surface
- **[Capsule](https://github.com/capsule-shell/capsule)** — reference architecture for a single-process, single-surface GPUI shell
- **[smithay](https://github.com/Smithay/smithay)** — foundational Wayland primitives and layer-shell implementation
- **[Ryoku](https://github.com/ryoku-shell/ryoku)** — interface discipline, capability-gated compositor abstraction, and proof that a dense, feature-rich bar and dock can run at 140 MB RSS

---

## License

MIT