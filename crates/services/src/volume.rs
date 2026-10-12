//! Volume and mute, through `wpctl`.
//!
//! ## One subprocess, not four
//!
//! `wpctl` in this version has no `get-mute` and no JSON output:
//!
//! ```text
//! $ wpctl get-mute 131
//! Usage: ...                       # no such subcommand
//! $ wpctl get-volume 131
//! Volume: 0.35
//! ```
//!
//! Mute state only appears inside `wpctl status`, on the line for the default
//! sink:
//!
//! ```text
//!  ├─ Sinks:
//!  │  *  131. ears                                [vol: 0.50 MUTED]
//! ```
//!
//! So `read()` runs `wpctl status` once and takes the id, the level and the
//! mute flag out of that single line. Four spawns per poll would be four times
//! the fork cost to learn three facts that are printed together.
//!
//! ## Parsing `wpctl status` is fragile, and that is the accepted tradeoff
//!
//! It is human-formatted output with no stability guarantee. The parse is
//! isolated in `parse_status` and pinned by a fixture test containing the real
//! shape of the output, so a PipeWire upgrade that changes it fails a test
//! rather than showing a wrong number. When that happens the fix is the
//! fixture, not a redesign.
//!
//! `set_*` take the cached sink id rather than re-deriving it, so adjusting
//! the volume does not pay for another `status` call.

use std::process::{Command, Stdio};
use std::time::Duration;

/// The same timeout reasoning as `hyprctl`: a wedged sound daemon must not
/// stall the bar's tick.
const TIMEOUT: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    pub sink_id: String,
    pub sink_name: String,
    /// 0.0 ..= 1.0
    pub level: f32,
    pub muted: bool,
}

impl Default for Volume {
    fn default() -> Self {
        Self { sink_id: String::new(), sink_name: String::new(), level: 0.0, muted: false }
    }
}

impl Volume {
    /// The symbol the bar shows.
    ///
    /// Muted is its own glyph rather than "empty speaker": a muted sink still
    /// has a level, and showing it as empty loses the information that
    /// unmuting will be audible.
    pub fn glyph(&self) -> &'static str {
        if self.muted {
            return "audio-volume-muted";
        }
        match self.level {
            l if l <= 0.001 => "audio-volume-muted",
            l if l < 0.34 => "audio-volume-low",
            l if l < 0.67 => "audio-volume-medium",
            _ => "audio-volume-high",
        }
    }

    /// 0..=100, for a label or a slider.
    pub fn percent(&self) -> u8 {
        (self.level.clamp(0.0, 1.0) * 100.0).round() as u8
    }
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
                std::thread::sleep(Duration::from_millis(5));
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

/// The default sink's state.
///
/// Returns `None` when PipeWire is not reachable, which the bar treats as
/// "hide the widget" rather than "show zero".
pub fn read() -> Option<Volume> {
    let status = run("wpctl", &["status"])?;
    let volume = parse_status(&status);

    if volume.is_some() {
        tracing::trace!(
            sink = %volume.as_ref().unwrap().sink_name,
            level = volume.as_ref().unwrap().level,
            muted = volume.as_ref().unwrap().muted,
            "volume read"
        );
    }
    volume
}

/// Pull the default sink out of `wpctl status`.
///
/// The default sink is the one marked with `*` inside the `Sinks:` block. The
/// block ends at `Sources:`, which matters: a `*` further down would be a
/// default *source*, and a microphone has no mute flag in the same sense.
pub fn parse_status(text: &str) -> Option<Volume> {
    let mut in_sinks = false;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // `contains`, not `starts_with`: the real line is `├─ Sinks:` after
        // trimming, so `starts_with("Sinks:")` is false and the whole block
        // is skipped. This returned None for every real `wpctl status` and
        // only the fixture-less tests noticed.
        if trimmed.contains("Sinks:") {
            in_sinks = true;
            continue;
        }
        if trimmed.contains("Sources:") {
            in_sinks = false;
            continue;
        }
        if !in_sinks {
            continue;
        }

        let Some(marker) = trimmed.find('*') else {
            continue;
        };

        let line = &trimmed[marker + 1..];
        // `131. ears   [vol: 0.50 MUTED]`
        let (id, rest) = line.split_once('.')?;
        let id = id.trim();

        let Some(bracket) = rest.find('[') else { continue };
        let name = rest[..bracket].trim().to_string();
        let detail = rest[bracket + 1..].trim_end_matches(']');

        // `[vol: 0.50 MUTED]` splits on whitespace into "vol:", "0.50" and
        // "MUTED" — the value is the token AFTER the marker, not part of it.
        // Treating "vol:" as the number gave `""` and every sink parsed as
        // having no volume.
        let mut fields = detail.split_whitespace().peekable();
        let level = loop {
            match fields.next() {
                Some("vol:") => break fields.next().and_then(|v| v.parse::<f32>().ok()),
                Some(_) => continue,
                None => break None,
            }
        }
        .map(|v: f32| v.clamp(0.0, 1.0));

        let Some(level) = level else { continue };

        return Some(Volume {
            sink_id: id.to_string(),
            sink_name: name,
            level,
            muted: detail.contains("MUTED"),
        });
    }

    None
}

pub fn set_level(sink_id: &str, level: f32) -> anyhow::Result<()> {
    if sink_id.is_empty() {
        anyhow::bail!("no default sink");
    }
    let level = level.clamp(0.0, 1.0);
    let mut child = Command::new("wpctl")
        .args(["set-volume", sink_id, &format!("{level:.4}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("wpctl set-volume failed: {status}");
    }
    Ok(())
}

/// Change by a delta. Saturating: `0.35` with `-0.1` is `0.25`, and `0.02`
/// with `-0.1` is `0`, not an error and not an underflow.
pub fn step(sink_id: &str, delta: f32, current: f32) -> anyhow::Result<()> {
    set_level(sink_id, (current + delta).clamp(0.0, 1.0))
}

pub fn set_muted(sink_id: &str, muted: bool) -> anyhow::Result<()> {
    if sink_id.is_empty() {
        anyhow::bail!("no default sink");
    }
    let status = Command::new("wpctl")
        .args(["set-mute", sink_id, if muted { "1" } else { "0" }])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("wpctl set-mute failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape of `wpctl status` from PipeWire on this machine, with
    /// the noise that matters: several sinks, a default marker on one of them,
    /// and a `Sources:` block after it that also has a `*`.
    const FIXTURE: &str = "\
PipeWire 'pipewire-0' [1.0.5, root@host, cookie:1234]
 └─ Clients:
 │      33. firefox
 │
 ├─ Sinks:
 │      59. HDMI / DisplayPort 3 Output (Stereo) [vol: 1.00]
 │      60. HDMI / DisplayPort 2 Output (Stereo) [vol: 1.00]
 │  *  131. ears                                [vol: 0.50]
 │
 ├─ Sources:
 │  *  132. Webcam                              [vol: 1.00]
";

    #[test]
    fn the_starred_sink_wins() {
        let v = parse_status(FIXTURE).unwrap();
        assert_eq!(v.sink_id, "131");
        assert_eq!(v.sink_name, "ears");
        assert_eq!(v.level, 0.50);
        assert!(!v.muted);
    }

    /// The `Sources:` block also contains a `*`. Taking that one would report
    /// a microphone as the output device.
    #[test]
    fn a_default_source_is_not_a_sink() {
        let v = parse_status(FIXTURE).unwrap();
        assert_ne!(v.sink_id, "132");
    }

    #[test]
    fn a_muted_sink_is_reported_as_muted_at_its_level() {
        let text = FIXTURE.replace("[vol: 0.50]", "[vol: 0.85 MUTED]");
        let v = parse_status(&text).unwrap();
        assert!(v.muted);
        assert_eq!(v.level, 0.85);
        assert_eq!(v.glyph(), "audio-volume-muted");
    }

    #[test]
    fn no_default_sink_is_none_not_a_zero() {
        let text = "PipeWire 'pipewire-0'\n ├─ Sinks:\n │      59. HDMI [vol: 1.00]\n";
        assert_eq!(parse_status(text), None);
        assert_eq!(parse_status("nothing here"), None);
        assert_eq!(parse_status(""), None);
    }

    #[test]
    fn out_of_range_levels_are_clamped() {
        let text = " ├─ Sinks:\n │  *  1. loud  [vol: 4.20]\n";
        assert_eq!(parse_status(text).unwrap().level, 1.0);

        let text = " ├─ Sinks:\n │  *  1. quiet [vol: -1.00]\n";
        assert_eq!(parse_status(text).unwrap().level, 0.0);
    }

    #[test]
    fn glyphs_track_the_level_bands() {
        let g = |level: f32, muted: bool| Volume {
            sink_id: "1".into(),
            sink_name: "x".into(),
            level,
            muted,
        };
        assert_eq!(g(0.0, false).glyph(), "audio-volume-muted");
        assert_eq!(g(0.2, false).glyph(), "audio-volume-low");
        assert_eq!(g(0.5, false).glyph(), "audio-volume-medium");
        assert_eq!(g(0.9, false).glyph(), "audio-volume-high");
        // A muted sink keeps its own glyph regardless of level.
        assert_eq!(g(0.9, true).glyph(), "audio-volume-muted");
    }

    #[test]
    fn percent_rounds_and_clamps() {
        let v = |l: f32| Volume { sink_id: "1".into(), sink_name: "x".into(), level: l, muted: false };
        assert_eq!(v(0.0).percent(), 0);
        assert_eq!(v(0.5).percent(), 50);
        assert_eq!(v(0.356).percent(), 36);
        assert_eq!(v(1.0).percent(), 100);
        assert_eq!(v(9.0).percent(), 100);
    }

    #[test]
    fn stepping_saturates_at_both_ends() {
        let next = |cur: f32, d: f32| (cur + d).clamp(0.0, 1.0);
        assert_eq!(next(0.35, 0.05), 0.40);
        assert_eq!(next(0.35, -0.10), 0.25);
        assert_eq!(next(0.02, -0.10), 0.0);
        assert_eq!(next(0.98, 0.10), 1.0);
    }

    #[test]
    fn an_empty_sink_id_is_an_error_not_a_spawn() {
        assert!(set_level("", 0.5).is_err());
        assert!(set_muted("", true).is_err());
    }
}
