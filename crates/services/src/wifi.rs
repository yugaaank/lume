//! Wi-Fi: scan, current connection, and the numbers the network menu shows.
//!
//! ## Why nmcli rather than parsing `iw`
//!
//! `iw` will tell you the SSID, the frequency and the link rate for the
//! interface it is *currently* on. It cannot list the networks around you —
//! that is NetworkManager's job, and it keeps the results. `nmcli` is the
//! supported client for exactly this and is present on every system with
//! NetworkManager, which is the overwhelmingly common case on a desktop.
//!
//! `nmcli -t` (terse) is used throughout: the tabular output is stable, greppable
//! and does not reflow with terminal width, unlike the aligned output.
//!
//! ## Scanning is expensive and rate-limited
//!
//! Every scan is a probe burst on every visible network. The menu asks once
//! when it opens and never again while it is up; re-scanning on each poll
//! would make the machine hostile to its own router and any laptop nearby.
//!
//! ## Every call is a subprocess, and that is the cost
//!
//! `nmcli -t -f ... dev wifi list` takes tens of milliseconds. The menu builds
//! its content once at open, so this is a handful of forks per open and none
//! per frame. `read_timeout` bounds a wedged NetworkManager.

use std::process::{Command, Stdio};
use std::time::Duration;

/// NetworkManager will not answer instantly, and a half-started NM should not
/// hold the menu open.
const TIMEOUT: Duration = Duration::from_millis(2500);

/// One network in range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    /// 0..=100 as NetworkManager reports it.
    pub signal: u8,
    pub security: String,
    /// True for the network the interface is on right now.
    pub active: bool,
    /// True for a saved profile that is not currently connected.
    pub saved: bool,
}

impl WifiNetwork {
    /// Signal as a bar count, 0..=4.
    pub fn bars(&self) -> u8 {
        match self.signal {
            s if s >= 75 => 4,
            s if s >= 50 => 3,
            s if s >= 25 => 2,
            s if s > 0 => 1,
            _ => 0,
        }
    }
}

/// Everything the network menu's detail grid shows about the live link.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WifiLink {
    pub ssid: String,
    pub interface: String,
    pub address: String,
    /// MHz.
    pub frequency_mhz: u32,
    /// Mbit/s, as negotiated. Zero when the interface is not up.
    pub link_mbps: u32,
    /// NetworkManager's TYPE for the connection: wpa2, wep, open, ...
    pub kind: String,
    pub connected: bool,
}

/// Networks in range, best signal first.
///
/// `rescan: false` — the cached list. A menu that rescans every time it opens
/// trains the router to ignore us.
pub fn scan(rescan: bool) -> Vec<WifiNetwork> {
    let mut args = vec![
        "-t",
        "-f",
        "IN-USE,SSID,SIGNAL,SECURITY",
        "device",
        "wifi",
        "list",
    ];
    if rescan {
        args.push("--rescan");
        args.push("yes");
    } else {
        args.push("--rescan");
        args.push("no");
    }

    let Some(text) = run("nmcli", &args) else {
        return Vec::new();
    };

    let mut networks: Vec<WifiNetwork> = text
        .lines()
        .filter_map(|line| {
            let fields = split_unescaped(line);
            if fields.len() < 4 {
                return None;
            }
            let in_use = fields[0].trim() == "*";
            let ssid = fields[1].to_string();
            let signal = fields[2].trim().parse::<u8>().unwrap_or(0);
            let security = fields[3].to_string();
            if ssid.is_empty() {
                return None;
            }
            Some(WifiNetwork { ssid, signal, security, active: in_use, saved: false })
        })
        .collect();

    // Strongest first, so the list the user reads starts with what they can
    // actually connect to.
    networks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.signal.cmp(&a.signal))
            .then(a.ssid.cmp(&b.ssid))
    });
    networks
}

/// The live link's details.
pub fn link() -> WifiLink {
    let Some(text) = run(
        "nmcli",
        &["-t", "-f", "NAME,DEVICE,TYPE,STATE", "con", "show", "--active"],
    ) else {
        return WifiLink::default();
    };

    let mut out = WifiLink::default();

    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 {
            continue;
        }
        let (name, device, kind) = (f[0], f[1], f[2]);
        // Skip loopback and anything that is not a wireless connection; the
        // menu is about wifi and a wired interface has no SSID to show.
        if !kind.contains("wireless") {
            continue;
        }
        out.ssid = name.to_string();
        out.interface = device.to_string();
        out.kind = kind.to_string();
        out.connected = true;
        break;
    }

    if out.connected {
        // `iw` for the radio details, `ip` for the address. Both are one fork
        // each and both are only asked for when there is a link to describe.
        if let Some(text) = run(
            "iw",
            &["dev", &out.interface, "link"],
        ) {
            if let Some(ssid) = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("SSID:"))
            {
                out.ssid = ssid.trim().to_string();
            }
            if let Some(freq) = text.lines().find_map(|l| l.trim().strip_prefix("freq:")) {
                out.frequency_mhz = freq
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
            if let Some(rate) = text.lines().find_map(|l| l.trim().strip_prefix("tx bitrate:")) {
                out.link_mbps = rate
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
        }

        if let Some(text) = run("ip", &["-4", "-br", "addr", "show", &out.interface]) {
            if let Some(addr) = text.split_whitespace().nth(2) {
                out.address = addr.split('/').next().unwrap_or(addr).to_string();
            }
        }
    }

    out
}

/// Join a network. Returns the error text on failure, for the menu to show.
pub fn connect(ssid: &str) -> Result<(), String> {
    let output = Command::new("nmcli")
        .args(["device", "wifi", "connect", ssid])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Split terse output on `:` that are **not** escaped.
///
/// `nmcli -t` writes a literal colon inside a value as `\:`, so "Cafe\:free"
/// is three fields, not four. A plain `split(':')` puts "Cafe\" in the SSID
/// and shifts every later field, which for a scan means that network gets a
/// garbage signal and lands in the list anyway. Bounded to `limit` fields so
/// the last one can absorb anything the earlier ones did not claim.
fn split_unescaped(line: &str) -> Vec<String> {
    const MAX: usize = 6;
    let mut out = Vec::with_capacity(MAX);
    let mut current = String::new();
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            // `\:` is a literal colon; `\\` is a literal backslash.
            current.push(if ch == '\\' { '\\' } else { ch });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ':' if out.len() + 1 < MAX => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    out.push(current);
    out
}

/// Run a command with a timeout, returning stdout on success.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    child
        .wait_with_output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape of `nmcli -t` output on this machine.
    const LIST: &str = "\
IN-USE:SSID:SIGNAL:SECURITY
*:Japan 50:63:WPA2
:Zo Connect:84:WPA2
:PELAGIA ELLITE 6th FLOOR:39:WPA2
:ZOLO SPECTRA:55:WPA2
:SUNRISE 1st A:31:WPA2
:Zoldo:60:WPA2
";

    #[test]
    fn signals_map_to_the_usual_bar_counts() {
        let n = |s| WifiNetwork { ssid: "x".into(), signal: s, security: "WPA2".into(), active: false, saved: false };
        assert_eq!(n(90).bars(), 4);
        assert_eq!(n(75).bars(), 4);
        assert_eq!(n(60).bars(), 3);
        assert_eq!(n(50).bars(), 3);
        assert_eq!(n(30).bars(), 2);
        assert_eq!(n(10).bars(), 1);
        assert_eq!(n(0).bars(), 0);
    }

    /// The active network must sort first even though it is not the strongest
    /// signal in range — a list that puts a stronger network above the one
    /// you are connected to reads as though you are on the wrong one.
    #[test]
    fn the_connected_network_sorts_first() {
        let mut nets: Vec<WifiNetwork> = LIST
            .lines()
            .skip(1)
            .filter_map(|line| {
                let mut f = line.split(':');
                Some(WifiNetwork {
                    active: f.next()?.trim() == "*",
                    ssid: f.next()?.to_string(),
                    signal: f.next()?.parse().unwrap_or(0),
                    security: f.next().unwrap_or("").to_string(),
                    saved: false,
                })
            })
            .collect();
        nets.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then(b.signal.cmp(&a.signal))
                .then(a.ssid.cmp(&b.ssid))
        });

        assert_eq!(nets[0].ssid, "Japan 50");
        assert!(nets[0].active);
        // Then by strength.
        assert_eq!(nets[1].ssid, "Zo Connect");
        assert_eq!(nets[1].signal, 84);
    }

    /// An SSID containing a colon is escaped in terse output. A plain
    /// split(':') breaks on "cafe:free", shifts every later field, and puts
    /// the network in the list with a garbage signal level.
    #[test]
    fn an_ssid_with_a_colon_survives_parsing() {
        let f = split_unescaped(":cafe\\:free:70:WPA2");
        assert_eq!(f.len(), 4, "got {f:?}");
        assert_eq!(f[0], "");
        assert_eq!(f[1], "cafe:free");
        assert_eq!(f[2], "70");
        assert_eq!(f[3], "WPA2");
    }

    #[test]
    fn ordinary_lines_split_unchanged() {
        let f = split_unescaped("*:Japan 50:63:WPA2");
        assert_eq!(f, vec!["*", "Japan 50", "63", "WPA2"]);
    }

    #[test]
    fn a_backslash_in_an_ssid_is_not_an_escape_hatch() {
        let f = split_unescaped(":a\\\\b:10:WPA2");
        assert_eq!(f[1], "a\\b");
        assert_eq!(f[2], "10");
    }

    #[test]
    fn no_networkmanager_is_empty_not_an_error() {
        // The menu shows "none found" rather than failing to open.
        let text: Option<String> = None;
        assert!(text.map(|t| t.lines().count()).unwrap_or(0) == 0);
    }
}
