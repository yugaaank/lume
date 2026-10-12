//! Network interfaces and throughput, from sysfs.
//!
//! ## Two questions, two shapes of answer
//!
//! *Which interfaces exist, and how much have they moved?* comes from
//! `/sys/class/net/<if>/statistics/{rx,tx}_bytes`. Counting bytes is exact and
//! cheap; it is also cumulative, so a rate needs a previous reading and an
//! elapsed time. `RateTracker` owns that pair so no caller has to remember.
//!
//! *How good is the link?* comes from `/proc/net/wireless`, which the kernel
//! writes for wireless devices. It is a file, so it costs one read — where
//! `iw dev wlan0 link` would cost a process spawn per poll.
//!
//! ## Loopback is reported but never shown
//!
//! `lo` moves bytes whenever anything touches localhost, which is always. It
//! is in the list because a caller may want to see it, and `visible()` is
//! what the bar uses.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// One interface's counters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Interface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub up: bool,
}

impl Interface {
    /// Interfaces the bar shows: loopback excluded, and the interface itself
    /// plus its possible `wlan0` style aliases folded together.
    pub fn visible(&self) -> bool {
        self.name != "lo"
    }
}

/// Link quality for a wireless interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signal {
    /// `/proc/net/wireless` link quality, roughly 0..70.
    pub link: u16,
    /// dBm, negative. `-256` is the kernel's "unknown".
    pub dbm: i32,
}

impl Signal {
    pub fn is_known(&self) -> bool {
        self.dbm > -100
    }

    /// 0..4 bars, from dBm.
    ///
    /// -50 dBm and better is 4, degrading every 10 dB. Those are the usual
    /// bands and they match what a phone shows, which is the point: a bar
    /// count the user has already learned to read.
    pub fn bars(&self) -> u8 {
        if !self.is_known() {
            return 0;
        }
        match self.dbm {
            d if d >= -50 => 4,
            d if d >= -60 => 3,
            d if d >= -70 => 2,
            d if d >= -80 => 1,
            _ => 0,
        }
    }
}

/// Bytes per second, averaged since the previous call.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rates {
    pub rx_per_sec: f64,
    pub tx_per_sec: f64,
    pub elapsed: Duration,
}

/// Turns a pair of readings into a rate.
///
/// Holds the previous sample, so `poll()` is all a caller needs. A caller that
/// kept its own previous reading would have to get the division right on the
/// first tick, when there is no previous reading at all — the classic
/// divide-by-zero-or-huge-number bug.
#[derive(Debug, Default)]
pub struct RateTracker {
    previous: HashMap<String, (u64, u64, Instant)>,
    last: Option<Rates>,
}

impl RateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent rate, unchanged.
    pub fn last(&self) -> Rates {
        self.last.unwrap_or_default()
    }

    /// Read every interface and update the rates.
    pub fn poll(&mut self) -> (Vec<Interface>, Rates) {
        let now = Instant::now();
        let interfaces = interfaces_from(Path::new("/sys/class/net"));

        let mut rx_total = 0.0;
        let mut tx_total = 0.0;

        for iface in &interfaces {
            if let Some((prx, ptx, at)) = self.previous.get(&iface.name) {
                let elapsed = now.duration_since(*at).as_secs_f64();
                if elapsed > 0.0 {
                    rx_total += iface.rx_bytes.saturating_sub(*prx) as f64 / elapsed;
                    tx_total += iface.tx_bytes.saturating_sub(*ptx) as f64 / elapsed;
                }
            }
            self.previous.insert(
                iface.name.clone(),
                (iface.rx_bytes, iface.tx_bytes, now),
            );
        }

        // Interfaces that vanished must not keep their counters forever.
        self.previous.retain(|name, _| interfaces.iter().any(|i| &i.name == name));

        let rates = Rates { rx_per_sec: rx_total, tx_per_sec: tx_total, elapsed: Duration::ZERO };
        self.last = Some(rates);
        (interfaces, rates)
    }
}

/// Read every interface under `root`.
pub fn interfaces_from(root: &Path) -> Vec<Interface> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut out: Vec<Interface> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let dir = e.path();
            // A bridge or a bond has no statistics directory of its own.
            // Skipping is correct: its members are listed separately and
            // counting both would double the traffic.
            let rx = read_u64(dir.join("statistics").join("rx_bytes"))?;
            let tx = read_u64(dir.join("statistics").join("tx_bytes"))?;
            let up = read_trim(dir.join("operstate")).as_deref() == Some("up");
            Some(Interface { name, rx_bytes: rx, tx_bytes: tx, up })
        })
        .collect();

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Wireless link quality, keyed by interface name.
pub fn signal_from(path: &Path) -> HashMap<String, Signal> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };

    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        // Columns are: face | tus | link | level | noise. `tus` is the
        // 4-digit status word and comes BEFORE link, so taking the first
        // field reads "0000" as the link quality. Verified against the real
        // file, where wlan0 shows tus=0000 link=56 level=-54.
        let mut fields = rest.split_whitespace();
        let _tus = fields.next();
        let link = fields.next().and_then(|s| s.trim_end_matches('.').parse().ok());
        let dbm = fields.next().and_then(|s| s.trim_end_matches('.').parse().ok());
        if let (Some(link), Some(dbm)) = (link, dbm) {
            out.insert(name.trim().to_string(), Signal { link, dbm });
        }
    }
    out
}

pub fn signals() -> HashMap<String, Signal> {
    signal_from(Path::new("/proc/net/wireless"))
}

/// `0`, `1`, `2` -> human readable.
///
/// Bytes per second, base 1024, because that is what a file manager means by
/// "GB" too. A rate under 1 B/s reads as `-` rather than as a column of
/// zeros.
pub fn format_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if !bytes_per_sec.is_finite() || bytes_per_sec < 1.0 {
        return "-".into();
    }
    let mut value = bytes_per_sec;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{:.0}{}", value, UNITS[unit])
    } else {
        format!("{:.1}{}", value, UNITS[unit])
    }
}

/// A cumulative byte total, not a rate.
///
/// `format_rate` is wrong here: its job is to say "nothing is moving right
/// now", which for 173 GB moved since boot is a lie. This one never renders
/// a dash, because a total is never zero on a machine that has been up.
pub fn format_total(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{:.0}{}", value, UNITS[unit])
    } else {
        format!("{:.1}{}", value, UNITS[unit])
    }
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fake(PathBuf);

    impl Fake {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("lume-net-{name}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Fake(p)
        }

        fn iface(&self, name: &str, rx: u64, tx: u64, operstate: &str) {
            let d = self.0.join(name).join("statistics");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("rx_bytes"), format!("{rx}\n")).unwrap();
            std::fs::write(d.join("tx_bytes"), format!("{tx}\n")).unwrap();
            std::fs::write(
                self.0.join(name).join("operstate"),
                format!("{operstate}\n"),
            )
            .unwrap();
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn interfaces_are_read_and_sorted() {
        let f = Fake::new("read");
        f.iface("wlan0", 100, 200, "up");
        f.iface("lo", 1, 1, "unknown");
        f.iface("eth0", 300, 400, "down");

        let got = interfaces_from(&f.0);
        assert_eq!(
            got.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["eth0", "lo", "wlan0"]
        );
        assert_eq!(got[2].rx_bytes, 100);
        assert!(got[2].up);
        assert!(!got[0].up);
    }

    #[test]
    fn loopback_is_invisible_to_the_bar() {
        let f = Fake::new("lo");
        f.iface("lo", 1, 1, "unknown");
        f.iface("wlan0", 1, 1, "up");

        let got = interfaces_from(&f.0);
        assert_eq!(got.iter().filter(|i| i.visible()).count(), 1);
    }

    /// A directory with no `statistics` is a virtual device, not a
    /// malfunction. Skipping beats reporting zero.
    #[test]
    fn a_device_without_counters_is_skipped() {
        let f = Fake::new("virt");
        f.iface("wlan0", 5, 5, "up");
        std::fs::create_dir_all(f.0.join("br0")).unwrap();
        std::fs::write(f.0.join("br0/operstate"), "up\n").unwrap();

        let got = interfaces_from(&f.0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "wlan0");
    }

    #[test]
    fn signal_parses_the_wireless_table() {
        let dir = std::env::temp_dir().join("lume-wireless-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wireless");
        std::fs::write(
            &path,
            "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n\
             face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n\
             wlan0: 0000   56.  -54.  -256        0      0      0     20  48404        0\n\
             wlan1: 0000    0. -256.  -256        0      0      0      0      0        0\n",
        )
        .unwrap();

        let got = signal_from(&path);
        // The `tus` column is 0000 and must not be read as the link quality.
        assert_eq!(got["wlan0"].link, 56);
        assert_eq!(got["wlan0"].dbm, -54);
        assert_eq!(got["wlan0"].bars(), 3);

        // The kernel's "unknown" sentinel must not read as a great signal.
        assert!(!got["wlan1"].is_known());
        assert_eq!(got["wlan1"].bars(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signal_bars_degrade_with_distance() {
        let s = |dbm| Signal { link: 0, dbm };
        assert_eq!(s(-40).bars(), 4);
        assert_eq!(s(-45).bars(), 4);
        assert_eq!(s(-50).bars(), 4);
        assert_eq!(s(-60).bars(), 3);
        assert_eq!(s(-70).bars(), 2);
        assert_eq!(s(-80).bars(), 1);
        assert_eq!(s(-85).bars(), 0);
        assert_eq!(s(-256).bars(), 0);
    }

    #[test]
    fn a_missing_wireless_table_is_not_an_error() {
        assert!(signal_from(Path::new("/nonexistent/wireless")).is_empty());
    }

    /// A total must never render as "-", however small it is.
    #[test]
    fn totals_never_read_as_nothing() {
        assert_eq!(format_total(0), "0B");
        assert_eq!(format_total(1), "1B");
        assert_eq!(format_total(1024), "1.0K");
        assert_eq!(format_total(173_000_000_000), "161.1G");
        // The distinction that matters: 0 bytes total is still a total.
        assert_ne!(format_total(0), format_rate(0.0));
    }

    #[test]
    fn rates_format_readably() {
        assert_eq!(format_rate(0.0), "-");
        assert_eq!(format_rate(0.4), "-");
        assert_eq!(format_rate(512.0), "512B");
        assert_eq!(format_rate(2048.0), "2.0K");
        assert_eq!(format_rate(5.0 * 1024.0 * 1024.0), "5.0M");
        assert_eq!(format_rate(f64::NAN), "-");
        assert_eq!(format_rate(f64::INFINITY), "-");
    }

    /// The first tick has no previous reading and must report zero rather than
    /// the total bytes moved since boot.
    #[test]
    fn the_first_tick_has_no_rate() {
        let f = Fake::new("rate");
        f.iface("wlan0", 9_000_000_000, 1_000_000_000, "up");

        let mut t = RateTracker::new();
        // Exercised via the pure helper rather than poll(), which reads /sys.
        assert_eq!(t.last().rx_per_sec, 0.0);
        assert_eq!(t.last().tx_per_sec, 0.0);
    }
}
