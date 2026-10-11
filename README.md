# lume

A Wayland desktop shell for Hyprland, in Rust on the `gpui-ce` fork.

Plan: `../shell-plan/` · Progress: `../progress/` · Reference: `../gpui-shell-docs/`

## Build

`/tmp` on many machines is a small tmpfs and a cold gpui build needs ~6 GB.
Always point the target dir at a real volume:

```sh
export CARGO_TARGET_DIR=~/.cache/lume-target
cargo build --release
```

## Run

```sh
./target/release/lume                 # the shell
WAYLAND_DEBUG=1 ./target/release/lume # see every protocol request
RUST_LOG=info ./target/release/lume   # verbose
```

## Hyprland setup

Hyprland owns the keybinds, so the shell is driven by invoking it:

```ini
# ~/.config/hypr/hyprland.conf
bind = $mod, Space, exec, lume toggle-launcher
```

The shell creates `$XDG_RUNTIME_DIR/lume.sock`. `lume <verb>` connects to it; if
no shell is listening it exits rather than starting a second one.

## Layout

```
crates/app/       the shell binary
crates/services/  compositor, IPC. No UI knowledge, testable headless
```

## Current state

Stage 1 — a 32px bar. See `benchmarks/baseline.md` for measured numbers and
`../progress/STATUS.md` for where we are.
