//! Battery state, from sysfs.
//!
//! ## Why sysfs and not a library
//!
//! `/sys/class/power_supply` is the kernel's own ABI. Every read is a tiny
//! file, every one of them is readable as root and by anyone in `power` or
//! `seat`, and there is no daemon to be absent, no socket to time out, and no
//! version of libupower to disagree with. A shell that shows a battery
//! indicator should not fail because a user-space service is not running.
//!
//! ## The root is a parameter
//!
//! Every reader here takes the sysfs root as an argument. That is what makes
//! these testable: a test writes a fake tree and asserts on the parse, with no
//! battery attached. A reader hard-coded to `/sys` is only testable on a
//! laptop, which is to say not testable.

use std::path::{Path, PathBuf};

/// What the bar needs to draw an indicator.
///
/// Kept `Copy` and tiny on purpose: the bar reads this on every frame and
/// copies it into widget state. The panel's detail rows need `String`s and
/// are read on a much slower cadence, so they live in [`BatteryDetail`]
/// rather than being bolted on here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Battery {
    /// False on a desktop. The widget hides itself rather than showing 0%.
    pub present: bool,
    /// Percentage, 0..=100.
    pub capacity: Option<u8>,
    /// True when charging, discharging, or full-but-plugged-in.
    pub charging: bool,
}

/// The pack's full state, for the battery panel.
///
/// Every field past [`Battery`] is `Option` for one reason: the kernel does
/// not expose all of them everywhere. Some laptops publish `energy_*` in µWh
/// and no `charge_*`; others the reverse. Some omit `power_now` entirely.
/// A panel that renders a hardcoded zero for a field the kernel does not
/// publish is lying, so an absent field is `None` and its row is hidden.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BatteryDetail {
    /// The headline reading, shared with the bar.
    pub battery: Battery,
    /// The sysfs directory name, e.g. `BAT1`. Shown beside Health so a
    /// machine with several packs can tell which one this is.
    pub id: String,
    /// Raw sysfs `status`: Charging, Discharging, Full, Not charging, Unknown.
    pub status: String,

    /// Seconds to full when charging, to empty when discharging.
    pub time_to_full_s: Option<u64>,
    pub time_to_empty_s: Option<u64>,

    /// Full-charge capacity as a fraction of design, 0..=100.
    pub health: Option<u8>,
    /// Instantaneous draw or charge, in watts.
    pub rate_w: Option<f64>,
    /// Design capacity in watt-hours.
    pub size_wh: Option<u64>,
    pub cycles: Option<u64>,
}

impl BatteryDetail {
    /// Read the default sysfs root.
    pub fn read() -> Self {
        BatteryDetail::read_from(Path::new("/sys/class/power_supply"))
    }

    /// Read from an arbitrary root.
    pub fn read_from(root: &Path) -> Self {
        let Some(dir) = cell_dir(root) else {
            return BatteryDetail::default();
        };

        let capacity = read_trim(dir.join("capacity"))
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|v| v.clamp(0, 100) as u8);

        let status = read_trim(dir.join("status")).unwrap_or_default();
        let charging = matches!(status.as_str(), "Charging" | "Full");

        BatteryDetail {
            battery: Battery { present: true, capacity, charging },
            id: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            status,
            // Zero means "no estimate available", not "now". The kernel writes
            // 0 rather than omitting the file when it cannot predict.
            time_to_full_s: read_secs(dir.join("time_to_full_now")),
            time_to_empty_s: read_secs(dir.join("time_to_empty_now")),
            health: health(&dir),
            rate_w: read_watts(&dir),
            size_wh: read_watthours(&dir),
            cycles: read_trim(dir.join("cycle_count"))
                .and_then(|s| s.trim().parse().ok()),
        }
    }

    /// The status as a title, e.g. `Not charging`.
    pub fn status_title(&self) -> String {
        match self.status.as_str() {
            "" => "Unknown".to_string(),
            "Not charging" => "On AC · Full".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        }
    }

    /// `"4h 12m"`, `"12m"`, or `None` when there is no estimate.
    pub fn time_text(&self) -> Option<String> {
        let secs = if self.battery.charging {
            self.time_to_full_s
        } else {
            self.time_to_empty_s
        }?;
        Some(format_duration(secs))
    }

    /// `"Time to full"` or `"Time left"`.
    pub fn time_label(&self) -> &'static str {
        if self.battery.charging {
            "Time to full"
        } else {
            "Time left"
        }
    }
}

/// Locate the pack's sysfs directory.
fn cell_dir(root: &Path) -> Option<PathBuf> {
    // Several batteries can exist. Take the first with `type=Battery`
    // rather than the first directory: laptops ship an AC adapter and a
    // USB-C port in the same directory as the pack, and both are `Mains`
    // or `USB`.
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| read_trim(p.join("type")).as_deref() == Some("Battery"))
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

/// Read a seconds field, treating zero as "no estimate".
fn read_secs(path: PathBuf) -> Option<u64> {
    read_trim(path).and_then(|s| s.trim().parse::<u64>().ok()).filter(|&v| v > 0)
}

/// Full capacity over design, from either unit family.
///
/// The two families are never mixed: a pack reporting `charge_*` in µAh has
/// no `energy_*` files, so mixing them would divide µAh by µWh and produce a
/// ratio that is a real number and completely wrong.
fn health(dir: &Path) -> Option<u8> {
    let pair = |full: &str, design: &str| {
        let f: f64 = read_trim(dir.join(full))?.trim().parse().ok()?;
        let d: f64 = read_trim(dir.join(design))?.trim().parse().ok()?;
        // A zero design capacity is missing data, not an infinitely dead cell.
        if d <= 0.0 {
            return None;
        }
        Some((f / d * 100.0).clamp(0.0, 100.0))
    };
    pair("charge_full", "charge_full_design")
        .or_else(|| pair("energy_full", "energy_full_design"))
        .map(|v| v.round() as u8)
}

/// Instantaneous power, from whichever of the two families exists.
fn read_watts(dir: &Path) -> Option<f64> {
    let raw = read_trim(dir.join("power_now"))
        .or_else(|| read_trim(dir.join("energy_now")))?
        .trim()
        .parse::<f64>()
        .ok()?;
    // Both are µW. Zero means idle, and a panel row reading "0.0 W" on a
    // laptop that is asleep is noise.
    (raw > 0.0).then(|| raw / 1_000_000.0)
}

/// Design capacity in Wh, from the energy family only.
///
/// `charge_full_design` is in µAh and cannot be converted without knowing
/// the cell voltage, so a µAh-only pack has no honest size to show.
fn read_watthours(dir: &Path) -> Option<u64> {
    let raw: f64 = read_trim(dir.join("energy_full_design"))?.trim().parse().ok()?;
    (raw > 0.0).then(|| (raw / 1_000_000.0).round() as u64)
}

/// `"4h 12m"` above an hour, `"12m"` below one.
fn format_duration(secs: u64) -> String {
    let mins = secs / 60;
    match mins / 60 {
        0 => format!("{mins}m"),
        h => format!("{h}h {}m", mins % 60),
    }
}

impl Battery {
    /// Read the default sysfs root.
    pub fn read() -> Self {
        Battery::read_from(Path::new("/sys/class/power_supply"))
    }

    /// Read from an arbitrary root.
    pub fn read_from(root: &Path) -> Self {
        let Some(dir) = cell_dir(root) else {
            return Battery::default();
        };

        let capacity = read_trim(dir.join("capacity"))
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|v| v.clamp(0, 100) as u8);

        // `status` is one of Charging, Discharging, Full, Not charging,
        // Unknown. Only Charging and Full mean "plugged in and not draining".
        // Not charging means plugged in with a full pack, which should not
        // animate a bolt.
        let status = read_trim(dir.join("status")).unwrap_or_default();
        let charging = matches!(status.as_str(), "Charging" | "Full");

        Battery { present: true, capacity, charging }
    }

    /// The symbol the bar shows.
    pub fn glyph(&self) -> &'static str {
        if !self.present {
            return "";
        }
        match self.capacity {
            None => "?",
            Some(p) if p >= 95 => "full",
            Some(p) if p >= 75 => "three-quarters",
            Some(p) if p >= 45 => "half",
            Some(p) if p >= 20 => "quarter",
            Some(_) => "empty",
        }
    }
}

/// Read a sysfs file and trim it.
///
/// sysfs files end in a newline. `parse()` does not tolerate that, and
/// trimming here means no reader has to remember to.
fn read_trim(path: PathBuf) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(PathBuf);

    impl Fake {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("lume-battery-{name}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Fake(p)
        }

        fn cell(&self, name: &str, type_: &str, capacity: &str, status: &str) {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("type"), format!("{type_}\n")).unwrap();
            std::fs::write(d.join("capacity"), format!("{capacity}\n")).unwrap();
            std::fs::write(d.join("status"), format!("{status}\n")).unwrap();
        }

        /// Write one extra sysfs file into a cell.
        fn set(&self, name: &str, key: &str, value: &str) {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(key), format!("{value}\n")).unwrap();
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_desktop_has_no_battery() {
        let f = Fake::new("none");
        let b = Battery::read_from(&f.0);
        assert!(!b.present);
        assert_eq!(b.capacity, None);
        assert_eq!(b.glyph(), "");
    }

    #[test]
    fn a_missing_root_is_absent_not_an_error() {
        let b = Battery::read_from(Path::new("/nonexistent/lume/power_supply"));
        assert!(!b.present);
    }

    /// The adapter directory sorts before `BAT1` on most machines, so taking
    /// the first directory alphabetically would read a mains adapter as a
    /// battery and show 0%.
    #[test]
    fn the_ac_adapter_is_not_mistaken_for_the_pack() {
        let f = Fake::new("ac");
        f.cell("ACAD", "Mains", "0", "Unknown");
        f.cell("BAT1", "Battery", "80", "Not charging");

        let b = Battery::read_from(&f.0);
        assert!(b.present);
        assert_eq!(b.capacity, Some(80));
    }

    #[test]
    fn charging_is_only_charging_or_full() {
        let f = Fake::new("states");
        f.cell("BAT1", "Battery", "50", "Charging");
        assert!(Battery::read_from(&f.0).charging);

        f.cell("BAT1", "Battery", "50", "Full");
        assert!(Battery::read_from(&f.0).charging);

        // Plugged in with a full pack: no bolt animation.
        f.cell("BAT1", "Battery", "100", "Not charging");
        assert!(!Battery::read_from(&f.0).charging);

        f.cell("BAT1", "Battery", "50", "Discharging");
        assert!(!Battery::read_from(&f.0).charging);
    }

    #[test]
    fn capacity_is_clamped_and_a_junk_value_is_none() {
        let f = Fake::new("junk");
        f.cell("BAT1", "Battery", "140", "Discharging");
        assert_eq!(Battery::read_from(&f.0).capacity, Some(100));

        f.cell("BAT1", "Battery", "-5", "Discharging");
        assert_eq!(Battery::read_from(&f.0).capacity, Some(0));

        f.cell("BAT1", "Battery", "not-a-number", "Discharging");
        assert_eq!(Battery::read_from(&f.0).capacity, None);
    }

    #[test]
    fn glyphs_follow_the_usual_bands() {
        let b = |p: u8| Battery { present: true, capacity: Some(p), charging: false };
        assert_eq!(b(100).glyph(), "full");
        assert_eq!(b(80).glyph(), "three-quarters");
        assert_eq!(b(50).glyph(), "half");
        assert_eq!(b(25).glyph(), "quarter");
        assert_eq!(b(5).glyph(), "empty");
    }

    // --- BatteryDetail ---

    #[test]
    fn a_desktop_has_no_detail_either() {
        let f = Fake::new("detail-none");
        let d = BatteryDetail::read_from(&f.0);
        assert!(!d.battery.present);
        assert_eq!(d.health, None);
        assert_eq!(d.status_title(), "Unknown");
    }

    /// The pack this shell actually runs on publishes `charge_*` in µAh and
    /// no `energy_*` at all. Every energy-derived row must hide rather than
    /// showing a hardcoded zero.
    #[test]
    fn a_uah_only_pack_hides_the_energy_rows() {
        let f = Fake::new("detail-uah");
        f.cell("BAT1", "Battery", "80", "Not charging");
        f.set("BAT1", "charge_full", "3867000");
        f.set("BAT1", "charge_full_design", "4210000");

        let d = BatteryDetail::read_from(&f.0);
        assert_eq!(d.health, Some(92));
        // µAh cannot become Wh without the cell voltage, so there is no
        // honest size to show.
        assert_eq!(d.size_wh, None);
        assert_eq!(d.rate_w, None);
        assert_eq!(d.time_to_empty_s, None);
    }

    #[test]
    fn an_energy_pack_reports_watts_watt_hours_and_time() {
        let f = Fake::new("detail-energy");
        f.cell("BAT1", "Battery", "50", "Discharging");
        f.set("BAT1", "energy_full", "44000000");
        f.set("BAT1", "energy_full_design", "50000000");
        f.set("BAT1", "power_now", "12500000");
        f.set("BAT1", "time_to_empty_now", "15120");

        let d = BatteryDetail::read_from(&f.0);
        assert_eq!(d.health, Some(88));
        assert_eq!(d.size_wh, Some(50));
        assert_eq!(d.rate_w, Some(12.5));
        assert_eq!(d.time_to_empty_s, Some(15120));
        assert_eq!(d.time_text().as_deref(), Some("4h 12m"));
        assert_eq!(d.time_label(), "Time left");
    }

    /// Zero means the kernel has no estimate, not "now". Rendering it as
    /// `0m` claims the battery is empty.
    #[test]
    fn a_zero_time_is_absent_not_now() {
        let f = Fake::new("detail-zero");
        f.cell("BAT1", "Battery", "50", "Discharging");
        f.set("BAT1", "time_to_empty_now", "0");

        let d = BatteryDetail::read_from(&f.0);
        assert_eq!(d.time_to_empty_s, None);
        assert_eq!(d.time_text(), None);
    }

    /// A zero design capacity is missing data. Dividing by it would be an
    /// infinity, clamped to a confident-looking 100% health.
    #[test]
    fn a_zero_design_capacity_is_not_a_dead_cell() {
        let f = Fake::new("detail-nodesign");
        f.cell("BAT1", "Battery", "50", "Discharging");
        f.set("BAT1", "charge_full", "3867000");
        f.set("BAT1", "charge_full_design", "0");

        assert_eq!(BatteryDetail::read_from(&f.0).health, None);
    }

    /// The two families never coexist on one pack. Dividing µAh by µWh would
    /// produce a real number that is completely wrong, so the charge family
    /// wins outright and no fallback crosses over.
    #[test]
    fn the_unit_families_are_never_mixed() {
        let f = Fake::new("detail-mixed");
        f.cell("BAT1", "Battery", "50", "Discharging");
        f.set("BAT1", "charge_full", "3867000");
        f.set("BAT1", "charge_full_design", "4210000");
        // Absurd energy figures: if these leaked in, health would be wrong.
        f.set("BAT1", "energy_full", "100");
        f.set("BAT1", "energy_full_design", "100000000");

        assert_eq!(BatteryDetail::read_from(&f.0).health, Some(92));
    }

    #[test]
    fn an_idle_pack_reports_no_rate() {
        let f = Fake::new("detail-idle");
        f.cell("BAT1", "Battery", "100", "Full");
        f.set("BAT1", "power_now", "0");

        assert_eq!(BatteryDetail::read_from(&f.0).rate_w, None);
    }

    #[test]
    fn cycles_are_read_when_they_exist() {
        let f = Fake::new("detail-cycles");
        f.cell("BAT1", "Battery", "50", "Discharging");
        assert_eq!(BatteryDetail::read_from(&f.0).cycles, None);

        f.set("BAT1", "cycle_count", "142");
        assert_eq!(BatteryDetail::read_from(&f.0).cycles, Some(142));
    }

    #[test]
    fn the_id_is_the_sysfs_directory_name() {
        let f = Fake::new("detail-id");
        f.cell("BAT1", "Battery", "50", "Discharging");
        assert_eq!(BatteryDetail::read_from(&f.0).id, "BAT1");
    }

    /// Plugged in with a full pack reads as such, not as the raw kernel string.
    #[test]
    fn not_charging_on_ac_is_titled_as_full() {
        let f = Fake::new("detail-title");
        f.cell("BAT1", "Battery", "100", "Not charging");
        assert_eq!(BatteryDetail::read_from(&f.0).status_title(), "On AC · Full");

        f.cell("BAT1", "Battery", "100", "Charging");
        assert_eq!(BatteryDetail::read_from(&f.0).status_title(), "Charging");
    }

    #[test]
    fn durations_switch_units_at_the_hour() {
        assert_eq!(format_duration(45 * 60), "45m");
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(15120), "4h 12m");
    }
}
