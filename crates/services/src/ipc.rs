//! Single-instance IPC.
//!
//! Required because Hyprland owns the keybinds: the user's `hyprland.conf`
//! invokes `lume workspace 3`, which has to reach the already-running shell.
//! Without this there is no way to drive the shell at all.
//!
//! Line protocol over a Unix socket. One line in, one line out.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};

use crate::intent;

use crate::compositor::{Action, CompositorService, Workspace};

pub fn socket_path() -> anyhow::Result<PathBuf> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR unset; cannot place the IPC socket")?;
    Ok(PathBuf::from(runtime).join("lume.sock"))
}

/// What a CLI invocation can ask for.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Ping,
    Workspaces,
    FocusWorkspace(i64),
    ToggleSpecial,
    Quit,
    /// Show or hide a sidebar.
    Panel(intent::Side, Option<bool>),
    /// Open, close or toggle the wallpaper picker.
    Picker(intent::PickerOp),
    /// Open, close or toggle a widget menu. No name closes it.
    ///
    /// The CLI path has no pointer, so it docks to an edge. Which one is
    /// part of the request rather than a fallback, because a keybind should
    /// open the panel where the user is looking.
    Drawer(Option<String>, intent::DrawerOp),
    /// Open or close the launcher. No argument is a malformed request.
    Launcher(intent::LauncherOp),
    /// No command — the shell was already running.
    Duplicate,
}

impl Command {
    /// The wire form.
    ///
    /// One function, used by the client that sends and by the test that checks
    /// the round trip. When the client had its own copy of this mapping — as it
    /// did — the test exercised its own duplicate and would have passed while
    /// the two had drifted apart.
    pub fn wire(&self) -> String {
        match self {
            Command::Ping => "ping".into(),
            Command::Workspaces => "workspaces".into(),
            Command::FocusWorkspace(id) => format!("workspace {id}"),
            Command::ToggleSpecial => "toggle-special".into(),
            Command::Quit => "quit".into(),
            Command::Picker(op) => format!("wallpaper {}", match op {
            intent::PickerOp::Open => "on",
            intent::PickerOp::Close => "off",
            intent::PickerOp::Toggle => "toggle",
        }),
        Command::Launcher(op) => format!("launcher {}", match op {
            intent::LauncherOp::Open => "on",
            intent::LauncherOp::Close => "off",
        }),
        Command::Panel(side, None) => format!("panel {}", side.as_str()),
            Command::Panel(side, Some(open)) => format!(
                "panel {} {}",
                side.as_str(),
                if *open { "on" } else { "off" }
            ),
            // Never sent. `Duplicate` is a result of decoding, not a request.
            Command::Picker(op) => match op {
                intent::PickerOp::Open => "wallpaper on".into(),
                intent::PickerOp::Close => "wallpaper off".into(),
                intent::PickerOp::Toggle => "wallpaper toggle".into(),
            },
            Command::Drawer(Some(name), op) => format!("drawer {name} {}", op.as_str()),
            // A nameless close has to carry its verb too. `drawer close`
            // would decode back as a nameless *toggle* — the verb is
            // ambiguous with the name position, and a close that silently
            // became a toggle would close a panel and reopen it.
            Command::Drawer(None, op) => format!("drawer close {}", op.as_str()),
            Command::Duplicate => "duplicate".into(),
        }
    }
}

pub fn decode(line: &str) -> anyhow::Result<Command> {
    let mut parts = line.split_whitespace();
    let verb = parts.next().unwrap_or_default();
    let arg = parts.next();

    Ok(match verb {
        "" => bail!("empty command"),
        "ping" => Command::Ping,
        "workspaces" => Command::Workspaces,
        "workspace" => {
            let raw = arg.context("usage: lume workspace <id>")?;
            let id = raw
                .parse::<i64>()
                .with_context(|| format!("not a workspace id: {raw}"))?;
            Command::FocusWorkspace(id)
        }
        "toggle-special" => Command::ToggleSpecial,
        "drawer" => {
            let name = match arg {
                Some("close") | None => None,
                Some(a) => Some(a.to_string()),
            };
            // The verb is the third token, after the name. `drawer volume` and
            // `drawer volume open` both work; absent means toggle, which is
            // what a bare `lume drawer volume` should mean.
            let op = match parts.next() {
                None => intent::DrawerOp::Toggle,
                Some(word) => intent::DrawerOp::parse(word)
                    .context("usage: lume drawer [name] [open|close|toggle]")?,
            };
            Command::Drawer(name, op)
        }
        "wallpaper" => {
            // `arg`, not `parts.next()`: the second token was already
            // consumed above, so reading from the iterator again always
            // yielded None and every wallpaper command decoded as Toggle.
            // The round-trip test is what caught it.
            let op = match arg {
                None => intent::PickerOp::Toggle,
                Some(word) => intent::PickerOp::parse(word)
                    .context("usage: lume wallpaper [on|off|toggle]")?,
            };
            Command::Picker(op)
        }
        "launcher" => {
            // Required, unlike `wallpaper`. There is no sensible default:
            // "toggle" would be the obvious one and it is the one that can
            // close the launcher with a key repeat, so the verb is explicit.
            let op = intent::LauncherOp::parse(arg.context("usage: lume launcher on|off")?)
                .context("usage: lume launcher on|off")?;
            Command::Launcher(op)
        }
        "panel" => {
            let side = intent::Side::parse(arg.context("usage: lume panel <left|right> [on|off]")?)
                .context("usage: lume panel <left|right> [on|off]")?;
            let open = match parts.next() {
                None => None,
                Some("on") => Some(true),
                Some("off") => Some(false),
                Some(other) => bail!("usage: lume panel <left|right> [on|off]; got {other:?}"),
            };
            Command::Panel(side, open)
        }
        "quit" => Command::Quit,
        other => bail!("unknown command: {other}"),
    })
}

/// Try to hand `cmd` to a running shell.
///
/// `Ok(None)` means no shell is listening — the caller should start one.
/// Never auto-spawns: spawning a second shell over a live one would give the
/// user two bars.
pub fn send(cmd: &str) -> anyhow::Result<Option<String>> {
    let path = socket_path()?;

    let Ok(mut stream) = UnixStream::connect(&path) else {
        return Ok(None);
    };

    stream.write_all(format!("{cmd}\n").as_bytes())?;
    stream.flush()?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(Some(reply.trim_end().to_string()))
}

/// Serve commands until the process exits. Blocking; call it from a thread.
pub fn serve<F>(compositor: Arc<CompositorService>, mut handle: F)
where
    F: FnMut(Command, &CompositorService) -> String,
{
    let path = match socket_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("ipc: {e:#}");
            return;
        }
    };

    // A socket file left behind by a crashed shell would make bind() fail.
    // Removing it is safe: if a live shell held it, connect() above would have
    // succeeded and we would never have reached here.
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("ipc: bind {}: {e}", path.display());
            return;
        }
    };

    tracing::info!(path = %path.display(), "ipc listening");

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };

        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }

        let reply = match decode(line.trim()) {
            Ok(cmd) => {
                let reply = handle(cmd, &compositor);
                if reply == "__quit__" {
                    let _ = stream.write_all(b"ok\n");
                    let _ = stream.flush();
                    tracing::info!("ipc: quit requested");
                    break;
                }
                reply
            }
            Err(e) => format!("error: {e}"),
        };

        let _ = stream.write_all(format!("{reply}\n").as_bytes());
        let _ = stream.flush();
    }

    let _ = std::fs::remove_file(&path);
}

pub fn handle_default(cmd: Command, compositor: &CompositorService) -> String {
    handle(cmd, compositor, None)
}

/// Handle a command, with somewhere to send UI intents if the caller has one.
///
/// `intents` is `None` in the CLI process, which is the right answer: the
/// client half of the protocol is a separate process that must not be
/// mutating the running shell's UI. It forwards and exits.
pub fn handle(
    cmd: Command,
    compositor: &CompositorService,
    intents: Option<&intent::Shared>,
) -> String {
    match cmd {
        Command::Ping => "pong".to_string(),
        Command::Workspaces => {
            // Hoist: calling active() per row would re-load the ArcSwap each
            // time, and the nested derefs read terribly.
            // One clone of a 4-field struct, once, instead of an ArcSwap load
            // per row. Compare by reference.
            let current: Workspace = (*compositor.active()).clone();
            let current = &current;
            compositor
                .workspaces()
                .iter()
                .map(|w| {
                    let marker = if current == w { "*" } else { "" };
                    format!("{}{}", w.id, marker)
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        Command::FocusWorkspace(id) => match compositor.act(Action::FocusWorkspace { id }) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("error: {e:#}"),
        },
        Command::Panel(side, open) => {
            let Some(tx) = intents else {
                return "error: no shell attached".into();
            };
            let intent = match open {
                Some(open) => intent::Intent::SetPanel { side, open },
                None => intent::Intent::TogglePanel(side),
            };
            // `false` means the UI thread is gone, which only happens during
            // shutdown. Saying so beats a silent success.
            if tx.send(intent) {
                "ok".to_string()
            } else {
                "error: shell is shutting down".into()
            }
        }
        Command::Drawer(name, op) => {
            let Some(tx) = intents else {
                return "error: no shell attached".into();
            };
            let intent = intent::Intent::Drawer {
                widget: name,
                // No pointer on the CLI path. Right is the default because the
                // bar's status cluster lives there; a left-docked request is
                // an explicit third token.
                anchor: intent::Anchor::Edge(intent::Edge::Right),
                op,
            };
            if tx.send(intent) {
                "ok".to_string()
            } else {
                "error: shell is shutting down".into()
            }
        }
        Command::Picker(op) => {
            let Some(tx) = intents else {
                return "error: no shell attached".into();
            };
            if tx.send(intent::Intent::Picker(op)) {
                "ok".to_string()
            } else {
                "error: shell is shutting down".into()
            }
        }
        Command::Launcher(op) => {
            let Some(tx) = intents else {
                return "error: no shell attached".into();
            };
            if tx.send(intent::Intent::Launcher(op)) {
                "ok".to_string()
            } else {
                "error: shell is shutting down".into()
            }
        }
        Command::ToggleSpecial => match compositor.act(Action::ToggleSpecialWorkspace) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("error: {e:#}"),
        },
        Command::Quit => "__quit__".to_string(),
        Command::Duplicate => "ok".to_string(),
    }
}

/// The one place this crate spawns a process.
///
/// Anything a user can name goes through here, so the environment scrub and the
/// working directory are applied once instead of at each call site. If a child
/// inherits an injected debug handle it can re-execute *us*.
pub fn spawn_detached(program: &str, args: &[String]) -> std::io::Result<()> {
    std::process::Command::new(program)
        .args(args)
        .current_dir(
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("/")),
        )
        .env_remove("__LUME_CRASH_INFO_FD")
        .env_remove("__LUME_CRASH_DUMP_PID")
        .env_remove("__LUME_CRASH_DUMP_FD")
        .env_remove("__LUME_CRASH_LOG_FD")
        .env_remove("__LUME_CRASH_SIGNAL")
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_verbs() {
        assert_eq!(decode("ping").unwrap(), Command::Ping);
        assert_eq!(decode("  workspaces  ").unwrap(), Command::Workspaces);
        assert_eq!(decode("workspace 3").unwrap(), Command::FocusWorkspace(3));
        assert_eq!(
            decode("workspace -99").unwrap(),
            Command::FocusWorkspace(-99)
        );
        assert_eq!(decode("toggle-special").unwrap(), Command::ToggleSpecial);
        assert_eq!(decode("quit").unwrap(), Command::Quit);
    }

    /// Every command must survive encode-then-decode. The wallpaper decoder read
    /// the wrong token and silently made every wallpaper command a Toggle
    /// while its own round-trip test still passed, because the test exercised
    /// the encoding rather than the decoding.
    #[test]
    fn every_command_survives_a_round_trip() {
        let commands = [
            Command::Ping,
            Command::Workspaces,
            Command::FocusWorkspace(-2),
            Command::ToggleSpecial,
            Command::Quit,
            Command::Panel(intent::Side::Left, Some(true)),
            Command::Picker(intent::PickerOp::Toggle),
            Command::Drawer(Some("volume".into()), intent::DrawerOp::Open),
            Command::Drawer(Some("network".into()), intent::DrawerOp::Close),
            Command::Launcher(intent::LauncherOp::Open),
        ];

        for cmd in commands {
            let wire = cmd.wire();
            let back = decode(&wire).unwrap_or_else(|e| panic!("{wire:?} did not decode: {e}"));
            assert_eq!(cmd, back, "{wire:?} did not round trip");
        }
    }

    /// The verb is the *third* token. Reading it from the wrong position is
    /// what silently turned every wallpaper command into a toggle.
    #[test]
    fn the_drawer_verb_is_the_third_token() {
        assert_eq!(
            decode("drawer volume").unwrap(),
            Command::Drawer(Some("volume".into()), intent::DrawerOp::Toggle)
        );
        assert_eq!(
            decode("drawer volume open").unwrap(),
            Command::Drawer(Some("volume".into()), intent::DrawerOp::Open)
        );
        // Naming a widget and closing it is legal: `Close` ignores the name,
        // so this and a bare `close` do the same thing.
        assert_eq!(
            decode("drawer volume close").unwrap(),
            Command::Drawer(Some("volume".into()), intent::DrawerOp::Close)
        );
        // A bare `close` is a name, not a verb: `close` in the name slot is
        // ambiguous, so a nameless close must spell out its own verb.
        assert_eq!(
            decode("drawer close close").unwrap(),
            Command::Drawer(None, intent::DrawerOp::Close)
        );
        assert_eq!(
            decode("drawer close").unwrap(),
            Command::Drawer(None, intent::DrawerOp::Toggle)
        );
        assert_eq!(
            decode("drawer").unwrap(),
            Command::Drawer(None, intent::DrawerOp::Toggle)
        );
        // An unknown verb is an error, not a silent default.
        assert!(decode("drawer volume frobnicate").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("frobnicate").is_err());
        assert!(decode("workspace").is_err());
        assert!(decode("workspace abc").is_err());
    }
}