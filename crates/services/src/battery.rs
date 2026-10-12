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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Battery {
    /// False on a desktop. The widget hides itself rather than showing 0%.
    pub present: bool,
    /// Percentage, 0..=100.
    pub capacity: Option<u8>,
    /// True when charging, discharging, or full-but-plugged-in.
    pub charging: bool,
}

impl Battery {
    /// Read the default sysfs root.
    pub fn read() -> Self {
        Battery::read_from(Path::new("/sys/class/power_supply"))
    }

    /// Read from an arbitrary root.
    pub fn read_from(root: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Battery::default();
        };

        // Several batteries can exist. Take the first with `type=Battery`
        // rather than the first directory: laptops ship an AC adapter and a
        // USB-C port in the same directory as the pack, and both are `Mains`
        // or `USB`.
        let mut names: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| read_trim(p.join("type")).as_deref() == Some("Battery"))
            .collect();
        names.sort();

        let Some(dir) = names.into_iter().next() else {
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
}
