//! Hyprland backend.
//!
//! Talks to `hyprctl` for queries and to the event socket for change
//! notification. Verified against 0.56.2.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use super::{Action, Client, Compositor, Workspace, hypr_request_socket_paths, hypr_socket_paths};
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

/// The reply to `j/cursorpos`. One JSON object, one line.
#[derive(serde::Deserialize)]
struct RawCursor {
    x: f32,
    y: f32,
}

/// One request to Hyprland's request socket.
///
/// Connect, write one command, read the reply, close. It *looks* like a stream
/// and is not: Hyprland closes the connection once it has answered, so holding
/// it open gets a broken pipe on the second write. Measured, not assumed.
///
/// Still much cheaper than `hyprctl`: no fork, no exec, no dynamic linking, no
/// re-parse of a whole compositor dump — a connect, a dozen bytes, a reply. At
/// the 20Hz the dock reads the pointer, this is free.
fn request(command: &str) -> anyhow::Result<String> {
    let mut last: Option<std::io::Error> = None;
    for path in hypr_request_socket_paths() {
        let mut stream = match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };

        // The reply is a couple of hundred bytes. A read that takes longer than
        // this is a wedged compositor rather than a slow one, and bounding it
        // keeps the poll cadence a property of the loop, not of the compositor.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

        if let Err(e) = stream.write_all(command.as_bytes()).and_then(|_| stream.write_all(b"\n")) {
            last = Some(e);
            continue;
        }
        let _ = stream.flush();

        return match read_reply(&mut stream) {
            Ok(body) => Ok(body),
            Err(e) => Err(anyhow!("hyprland request {command:?} failed: {e}")),
        };
    }
    Err(anyhow!(
        "no hyprland request socket: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Read one reply, to its closing brace.
///
/// The server closes after answering, but reading to EOF is not the terminator
/// to rely on: a reply is pretty-printed across several lines, so the first
/// `read_line` is not the answer. A leading blank line is also normal on the
/// batch form. Stop at the closing brace instead — that is the end of the
/// document, and anything after it belongs to nobody.
fn read_reply(stream: &mut std::os::unix::net::UnixStream) -> std::io::Result<String> {
    use std::io::Read as _;

    let mut buf = String::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if buf.trim_end().ends_with('}') {
                    break;
                }
            }
            // A timeout here just means the reply was short and is already in
            // the buffer. Whatever arrived is still the answer.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

/// Pull the JSON document out of a raw reply.
///
/// A batch reply is `\n{ ... }\n`, so neither end of the socket buffer is the
/// start of the answer. Cutting at the first brace and the last one is blunt and
/// exact: a reply holds exactly one document.
fn json_body(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
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

#[derive(Clone, serde::Deserialize)]
struct RawClient {
    /// Hyprland's window address. Present on every 0.56 client.
    address: String,
    /// The window class. Empty for a handful of real windows (some launchers,
    /// some Wine apps), which is why the mapping into `Client` drops them.
    #[serde(default)]
    class: String,
    /// Present in 0.56 output; `floating` is a plain bool, not 0/1.
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    workspace: RawClientWorkspace,
}

/// The focused window. Hyprland has no "is focused" flag in the clients list, so
/// this second query is the only way to know which one it is.
#[derive(serde::Deserialize)]
struct RawActiveWindow {
    #[serde(default)]
    address: String,
}

#[derive(Clone, Default, serde::Deserialize)]
struct RawClientWorkspace {
    #[serde(default)]
    name: String,
}

impl RawClient {
    fn is_tiled(&self) -> bool {
        !self.floating
    }
}

#[derive(serde::Deserialize)]
struct RawMonitor {
    #[serde(default)]
    #[allow(dead_code)]
    name: String,
    #[serde(default)]
    refreshrate: f64,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

/// Wire clients into the model: drop the ungroupable, mark the focused one,
/// sort for a stable read-to-read order.
///
/// Separate from the query so it can be tested without a compositor, and so the
/// rules are in one place: every rule about what counts as a dock entry lives
/// here rather than being spread through the dock.
fn to_clients(raw: Vec<RawClient>, active: &str) -> Vec<Client> {
    let mut out: Vec<Client> = raw
        .into_iter()
        .filter_map(|c| {
            // A window with no class cannot be grouped by anything. Keeping it
            // would make every per-app count one too high.
            if c.class.trim().is_empty() {
                return None;
            }
            Some(Client {
                active: !active.is_empty() && c.address == active,
                address: c.address,
                class: c.class,
                workspace: c.workspace.name,
            })
        })
        .collect();

    // Stable order between reads. The dock diffs its running list against the
    // pinned one, and an unstable order makes icons shuffle for no reason.
    out.sort_by(|a, b| a.address.cmp(&b.address));
    out
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

    fn has_tiled_windows(&self, workspace: &str) -> anyhow::Result<bool> {
        let clients: Vec<RawClient> = query_json(&["-j", "clients"])?;
        Ok(clients
            .iter()
            .filter(|c| c.workspace.name == workspace)
            .any(RawClient::is_tiled))
    }

    fn cursor(&self) -> anyhow::Result<(f32, f32)> {
        // The [[BATCH]] prefix is not decoration. Hyprland 0.56's socket looks
        // commands up in a request table, and `cursorpos` is not in it: bare
        // `cursorpos`, `j/cursorpos` and `/j/cursorpos` all answer "unknown
        // request", while the batch form evaluates it and replies. Verified on
        // the running compositor, not read off a wiki.
        let reply = request("[[BATCH]]j/cursorpos")?;
        let body = json_body(&reply)
            .with_context(|| format!("no JSON in cursorpos reply: {reply:?}"))?;
        let raw: RawCursor =
            serde_json::from_str(body).with_context(|| format!("bad cursorpos reply: {reply:?}"))?;
        Ok((raw.x, raw.y))
    }

    fn clients(&self) -> anyhow::Result<Vec<Client>> {
        let raw: Vec<RawClient> = query_json(&["-j", "clients"])?;

        // Two queries, not one: the clients list carries no focus flag. Doing it
        // here rather than in the service keeps one consistent snapshot — a
        // client list and a separately-fetched active window can disagree about
        // which window is focused, and the dock would then mark two tiles active
        // or none.
        let active = query_json::<RawActiveWindow>(&["-j", "activewindow"])
            .map(|w| w.address)
            .unwrap_or_default();

        Ok(to_clients(raw, &active))
    }

    fn output_size(&self) -> (f32, f32) {
        // The first output with a real size. Monitors can report 0x0 while a
        // hotplug is in flight, and a dock sized against that is invisible.
        match query_json::<Vec<RawMonitor>>(&["-j", "monitors"]) {
            Ok(monitors) => monitors
                .iter()
                .find(|m| m.width > 0 && m.height > 0)
                .map(|m| (m.width as f32, m.height as f32))
                .unwrap_or((1920.0, 1080.0)),
            Err(e) => {
                tracing::debug!("could not read output size: {e:#}");
                (1920.0, 1080.0)
            }
        }
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

    /// The exact bytes a batch reply arrives as, captured from the running
    /// compositor: a leading newline, pretty-printed JSON, a trailing newline.
    #[test]
    fn extracts_json_from_a_batch_reply() {
        let raw = "\n{\n    \"x\": 1121,\n    \"y\": 788\n}\n";
        let body = json_body(raw).unwrap();
        let c: RawCursor = serde_json::from_str(body).unwrap();
        assert_eq!((c.x, c.y), (1121.0, 788.0));
    }

    #[test]
    fn a_reply_with_no_json_yields_nothing() {
        assert_eq!(json_body("unknown request"), None);
        assert_eq!(json_body(""), None);
    }

    /// The cursor reply is one JSON object on one line. Guarding the shape
    /// matters because a parse failure here looks like a frozen dock, not like
    /// a bad reply.
    #[test]
    fn parses_cursorpos_reply() {
        let raw: RawCursor = serde_json::from_str(r#"{"x":582,"y":746}"#).unwrap();
        assert_eq!((raw.x, raw.y), (582.0, 746.0));
    }

    #[test]
    fn fractional_cursor_is_accepted() {
        // Scaled outputs report fractional coordinates. Truncating them would
        // leave the reveal edge a pixel out on a HiDPI screen.
        let raw: RawCursor = serde_json::from_str(r#"{"x":582.5,"y":746.25}"#).unwrap();
        assert!((raw.x - 582.5).abs() < f32::EPSILON);
    }

    fn raw(address: &str, class: &str, ws: &str) -> RawClient {
        serde_json::from_str(&format!(
            r#"{{"address":"{address}","class":"{class}",
                "floating":false,"workspace":{{"id":1,"name":"{ws}"}}}}"#
        ))
        .unwrap()
    }

    /// A window with no class is not a dock entry. Not a hypothetical: some
    /// launchers and a fair few Wine apps report one, and storing them would
    /// make every per-app count read one too high.
    #[test]
    fn classless_windows_are_dropped() {
        let out = to_clients(
            vec![raw("0x2", "kitty", "1"), raw("0x1", "", "1"), raw("0x3", "  ", "1")],
            "",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].class, "kitty");
    }

    #[test]
    fn exactly_one_window_is_active() {
        let out = to_clients(
            vec![raw("0x1", "kitty", "1"), raw("0x2", "zen", "2")],
            "0x2",
        );
        assert_eq!(out.iter().filter(|c| c.active).count(), 1);
        assert!(out.iter().find(|c| c.class == "zen").unwrap().active);
    }

    /// An empty active address means "nothing focused", which is the normal
    /// state on a bare desktop. Treating it as a match would mark the first
    /// window active by accident, since no address is equal to "".
    #[test]
    fn no_focused_window_marks_nothing_active() {
        let out = to_clients(vec![raw("0x1", "kitty", "1")], "");
        assert!(!out[0].active);
    }

    /// The order must be identical between two reads of the same set, or the
    /// dock's icons shuffle on every poll for no reason.
    #[test]
    fn order_is_stable_and_sorted_by_address() {
        let input = vec![raw("0x30", "c", "1"), raw("0x10", "a", "1"), raw("0x20", "b", "1")];
        let a = to_clients(input.clone(), "");
        let b = to_clients(input, "");
        assert_eq!(a, b);
        assert_eq!(a.iter().map(|c| &c.address).collect::<Vec<_>>(), ["0x10", "0x20", "0x30"]);
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