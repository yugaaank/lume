//! Hyprland backend.
//!
//! Talks to `hyprctl` for queries and to the event socket for change
//! notification. Verified against 0.56.2.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use super::{Action, Compositor, Workspace, hypr_socket_paths};
/// IPC calls get their own short timeout so a wedged Hyprland cannot hang the
/// UI's poll loop.
const IPC_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Hyprland;

/// Run a `hyprctl` query and return stdout.
///
/// Note the bare command name: `hyprctl -j workspaces`, not `hyprctl -j
/// /workspaces`. The slash form was removed before 0.56.
fn query(args: &[&str]) -> anyhow::Result<String> {
    let mut child = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run hyprctl — is Hyprland in PATH?")?;

    // A wedged hyprctl must not stall the UI's state feed. Poll rather than
    // block, so this stays on a plain thread without pulling in a runtime.
    let deadline = std::time::Instant::now() + IPC_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("hyprctl {args:?} timed out after {IPC_TIMEOUT:?}");
            }
        }
    }

    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "hyprctl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn query_json<T: serde::de::DeserializeOwned>(args: &[&str]) -> anyhow::Result<T> {
    let text = query(args)?;
    serde_json::from_str(&text).with_context(|| format!("bad JSON from hyprctl {args:?}"))
}

// --- the wire shapes -------------------------------------------------------
//
// Field names and types verified against 0.56.2 output. `hasfullscreen` and
// `ispersistent` are booleans in this version, not 0/1 integers; older Hyprland
// emitted numbers and would fail to deserialize here.

#[derive(serde::Deserialize)]
struct RawWorkspace {
    id: i64,
    name: String,
    #[serde(default)]
    windows: usize,
}

impl From<RawWorkspace> for Workspace {
    fn from(w: RawWorkspace) -> Self {
        Self {
            special: w.id < 0,
            id: w.id,
            name: w.name,
            windows: w.windows,
        }
    }
}

#[derive(serde::Deserialize)]
struct RawMonitor {
    #[serde(default)]
    #[allow(dead_code)]
    name: String,
    #[serde(default)]
    refreshrate: f64,
}

impl Compositor for Hyprland {
    fn workspaces(&self) -> anyhow::Result<Vec<Workspace>> {
        let raw: Vec<RawWorkspace> = query_json(&["-j", "workspaces"])?;
        Ok(raw.into_iter().map(Into::into).collect())
    }

    fn active_workspace(&self) -> anyhow::Result<Workspace> {
        let raw: RawWorkspace = query_json(&["-j", "activeworkspace"])?;
        Ok(raw.into())
    }

    fn refresh_rate(&self) -> f64 {
        // A monitor with no backlight reports a nonsense rate. Fall back rather
        // than handing the animation loop 0.
        match query_json::<Vec<RawMonitor>>(&["-j", "monitors"]) {
            Ok(monitors) => {
                let best = monitors
                    .iter()
                    .map(|m| m.refreshrate)
                    .filter(|r| r.is_finite() && *r > 1.0)
                    .fold(60.0_f64, f64::max);
                best
            }
            Err(e) => {
                tracing::debug!("could not read refresh rate: {e:#}");
                60.0
            }
        }
    }

    fn act(&self, action: Action) -> anyhow::Result<()> {
        let lua = match action {
            // Lua dispatch, not the legacy dispatcher language. On 0.56 the
            // legacy form is a syntax error.
            Action::FocusWorkspace { id } => format!("hl.dsp.focus({{ workspace = {id} }})"),
            Action::ToggleSpecialWorkspace => {
                "hl.dsp.workspace.toggle_special()".to_string()
            }
            Action::FloatWindow { address } => {
                format!("hl.dsp.window.float({{ address = {address:?} }})")
            }
            Action::CloseWindow { address } => {
                format!("hl.dsp.window.kill({{ address = {address:?} }})")
            }
            Action::Quit => "hl.dsp.exit()".to_string(),
        };

        query(&["dispatch", &lua]).map(|_| ())
    }
}

/// Block on the Hyprland event socket and invoke `on_event` per line.
///
/// Reconnects on EOF rather than exiting: Hyprland restarts on `exec-once`
/// reloads and a dead listener means a bar that silently stops updating.
pub fn listen_events<F: FnMut()>(mut on_event: F) -> anyhow::Result<()> {
    let mut last_err: Option<std::io::Error> = None;

    for path in hypr_socket_paths() {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "hyprland event stream connected");
                let reader = BufReader::new(stream);

                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.is_empty() {
                        continue;
                    }
                    // We deliberately do not parse the payload. The poll is the
                    // source of truth; the stream is only a nudge to re-read.
                    // Parsing both invites drift when either side guesses wrong.
                    on_event();
                }

                tracing::warn!("hyprland event stream closed, reconnecting");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), "no event socket: {e}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("hyprland: no event socket found")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_workspaces_are_negative() {
        assert!(Workspace {
            id: -99,
            name: "special:magic".into(),
            special: true,
            windows: 0
        }
        .is_special());
        assert!(!Workspace {
            id: 3,
            name: "3".into(),
            special: false,
            windows: 2
        }
        .is_special());
    }

    /// Guards the 0.56 wire shape. `hasfullscreen` is a bool here; older
    /// Hyprland sent 0/1 integers and this would fail to deserialize.
    #[test]
    fn parses_056_workspace_json() {
        let json = r#"[{"id":1,"name":"1","monitor":"eDP-1","monitorID":0,
                        "windows":2,"hasfullscreen":false,"ispersistent":false,
                        "lastwindow":"0x1","tiledLayout":"dwindle"}]"#;
        let parsed: Vec<RawWorkspace> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, 1);
        assert_eq!(parsed[0].windows, 2);
    }
}