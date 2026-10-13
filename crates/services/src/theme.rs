//! The visual identity: one token layer for every surface.
//!
//! ## Where this came from
//!
//! `ryoku/ryoku/ui/Singletons/Tokens.qml`, whose aesthetic is Neon Genesis
//! Evangelion — the black-and-red danger signage of a NERV installation, drawn
//! in a terminal face. Reproducing that system rather than inventing one is the
//! whole point: the reference has already solved the hard part, which is that
//! you do not need many colours.
//!
//! ## The one idea
//!
//! There is **one ink colour** and **one accent**. Every surface, border,
//! divider, hover and disabled state is that ink at a different alpha. That is
//! why the reference reads as coherent rather than busy, and it is the reason
//! this module looks like a list of opacities instead of a list of colours.
//!
//! `bone #cdc4ba` — not white. The "selected" pills that read as white in the
//! reference are warm bone, and using pure `#ffffff` cools the whole interface
//! and makes the accent red fight it.
//!
//! ## Why this lives in `services` and not `app`
//!
//! The settings process is a separate binary. Tokens the app crate owns cannot
//! be previewed or edited by it and would have to move later. `services` already
//! holds `config`, which is what these derive from.
//!
//! ## Why no light mode
//!
//! `config.appearance.mode` says `"Dark"`. That field is still dead, and
//! deliberately so. Light inverts the ink/bone roles, so every shadow, border
//! and panel opacity needs re-deciding against a light wallpaper — and a
//! half-implemented light mode is worse than none. It is a later pass, not a
//! missing token.

use serde::{Deserialize, Serialize};

/// The accent each preset carries.
pub mod accent {
    /// NERV red. `Tokens.qml`'s own `sun`/`alert`.
    pub const RED: [u8; 3] = [0xe2, 0x34, 0x2a];
    /// The green of NERV personnel signage.
    pub const GREEN: [u8; 3] = [0x6e, 0xa8, 0x4f];
    /// Eva Unit-01.
    pub const PURPLE: [u8; 3] = [0x8b, 0x5c, 0xf6];
    /// Amber phosphor.
    pub const AMBER: [u8; 3] = [0xff, 0xb0, 0x00];
    /// No accent at all. The most restrained, and the most terminal.
    pub const NONE: [u8; 3] = [0xcd, 0xc4, 0xba];
}

/// The resolved palette. Everything a surface needs to draw itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    /// `[r, g, b]`, so it can be stored and compared without a GPUI type.
    pub accent: [u8; 3],
    /// `[r, g, b]` of the ink. Bone for every preset but tinted ones.
    pub ink: [u8; 3],
    /// `[r, g, b]` of the paper. Black for every preset but tinted ones.
    pub paper: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: accent::RED,
            ink: [0xcd, 0xc4, 0xba],
            paper: [0x00, 0x00, 0x00],
        }
    }
}

impl Palette {
    pub fn preset(id: &str) -> Self {
        match id {
            "green" => Self {
                accent: accent::GREEN,
                ..Self::default()
            },
            "purple" => Self {
                accent: accent::PURPLE,
                ..Self::default()
            },
            "amber" => Self {
                accent: accent::AMBER,
                ..Self::default()
            },
            // "bone" is monochrome: the accent is the ink, so nothing in the
            // interface is coloured and every accent-coloured element reads as
            // emphasis purely because it is brighter.
            "bone" => Self {
                accent: accent::NONE,
                ..Self::default()
            },
            // Anything unknown falls back to red rather than erroring. A
            // hand-edited config naming a preset that does not exist should
            // still produce a usable shell.
            _ => Self::default(),
        }
    }

    /// Ink at an alpha, as GPUI `hsla`-free RGBA floats.
    ///
    /// The ink is stored as bytes and converted here rather than kept as an
    /// `Rgba`, because this module must not depend on GPUI: the settings
    /// process needs to read and write these values without a renderer.
    pub fn ink_at(&self, alpha: f32) -> (f32, f32, f32, f32) {
        at(self.ink, alpha)
    }

    pub fn accent_at(&self, alpha: f32) -> (f32, f32, f32, f32) {
        at(self.accent, alpha)
    }

    pub fn paper_at(&self, alpha: f32) -> (f32, f32, f32, f32) {
        at(self.paper, alpha)
    }
}

fn at(rgb: [u8; 3], alpha: f32) -> (f32, f32, f32, f32) {
    (
        f32::from(rgb[0]) / 255.0,
        f32::from(rgb[1]) / 255.0,
        f32::from(rgb[2]) / 255.0,
        alpha.clamp(0.0, 1.0),
    )
}

/// The alpha ramp, named for what it is for rather than how big it is.
///
/// `Tokens.qml` spells these out as inline `Qt.rgba(..., 0.26)` calls. Naming
/// them is the entire value of this module: `line_soft` cannot drift away from
/// the divider in the panel four files over, because there is only one.
pub mod alpha {
    /// Hairline divider and card border.
    pub const LINE: f32 = 0.26;
    /// A border that must read as a border against an already-dark surface.
    pub const LINE_STRONG: f32 = 0.42;
    /// An almost-invisible divider, e.g. between two rows of one card.
    pub const LINE_SOFT: f32 = 0.13;
    /// Hover on a surface.
    pub const SURFACE_HOVER: f32 = 0.05;
    /// Hover on a control.
    pub const CONTROL_HOVER: f32 = 0.10;
    /// Pressed.
    pub const PRESSED: f32 = 0.16;
    /// Secondary body text.
    pub const MUTED: f32 = 0.88;
    /// Tertiary text: labels, hints, disabled.
    pub const FAINT: f32 = 0.68;
    /// The grain overlay.
    pub const GRAIN: f32 = 0.10;
    /// A surface that must be readable over anything: the bar, a menu, the
    /// launcher, the quick settings drawer.
    ///
    /// Near-opaque on purpose, and there is no separate "slightly see-through"
    /// value. A translucent bar was tried and looks like a bug: a fullscreen
    /// window ignores the bar's exclusive zone, renders underneath it, and its
    /// text bleeds through at a ratio that reads as a rendering fault rather
    /// than as depth. Translucency only earns its keep paired with a backdrop
    /// blur, which the bar does not use. The reference bar is solid black.
    pub const SURFACE: f32 = 0.985;

}

/// Corner radii.
pub mod radius {
    /// A row, a tile, a control.
    pub const ROW: f32 = 4.0;
    /// An icon tile.
    pub const TILE: f32 = 8.0;
    /// A card.
    pub const CARD: f32 = 10.0;
    /// A panel: the launcher, a menu, the quick settings drawer.
    pub const PANEL: f32 = 14.0;
    /// The caret.
    pub const CARET: f32 = 1.0;
    /// Fully round, for a circular icon button.
    pub const PILL: f32 = 999.0;
}

/// Type scale. Sizes and tracking both in px.
pub mod type_scale {
    /// Tracking for micro labels — the reference's `trackLabel`.
    ///
    /// The reference writes this as `1.4` in a QML `font.letterSpacing`, whose
    /// default unit is a percentage of the text size — 1.4% of a 10px label is
    /// invisible, which cannot be what the reference looks like. Rather than
    /// guess the unit, these are stated in px for the size they are used at and
    /// tuned against a screenshot of the real bar.
    pub const TRACK_LABEL: f32 = 0.9;
    /// Tracking for eyebrows and section marks — the reference's `trackMark`.
    pub const TRACK_MARK: f32 = 1.6;

    /// Section headers: "LANGUAGE & REGION", "CONNECT".
    pub const SECTION: f32 = 10.0;
    /// Row labels, list entries.
    pub const BODY: f32 = 12.0;
    /// Bar status values. One step below body, and at faint contrast, so the
    /// clock reads as the bar's anchor rather than as just another widget.
    pub const SMALL: f32 = 11.0;
    /// Captions. The reference's tracked micro labels: section headers,
    /// region names, the marks inside the identity box.
    pub const TINY: f32 = 8.0;
    /// The clock.
    pub const CLOCK: f32 = 18.0;
    /// Display type.
    pub const DISPLAY: f32 = 44.0;

    /// The one family, for everything.
    ///
    /// The reference uses Fraunces for display and Space Grotesk for UI. This
    /// is the terminal look instead, so the whole interface is one family and
    /// there is no display/UI split to reason about.
    ///
    /// Must match name ID 1 or 16 in the font's own `name` table, not a
    /// human-readable spelling of it. The face installed here reports
    /// `JetBrainsMono NF` (ID 1) and `JetBrainsMono Nerd Font` (ID 16), so
    /// "JetBrains Mono NF" with different spacing resolves to nothing and every
    /// glyph silently falls back to a default face. That fallback is a plain
    /// mono, which looks close enough to the real thing that nothing is wrong
    /// until a Nerd Font codepoint renders as tofu.
    pub const FAMILY: &str = "JetBrainsMono NF";
}

/// Which theme the shell is wearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    #[default]
    NervRed,
    NervGreen,
    Unit01Purple,
    AmberCrt,
    Bone,
}

impl Preset {
    /// The id used in config and in `theme.preset`.
    pub fn id(self) -> &'static str {
        match self {
            Preset::NervRed => "red",
            Preset::NervGreen => "green",
            Preset::Unit01Purple => "purple",
            Preset::AmberCrt => "amber",
            Preset::Bone => "bone",
        }
    }

    /// The name shown in the settings picker. Not the id: "NERV red" reads as a
    /// choice, "red" reads as a colour.
    pub fn title(self) -> &'static str {
        match self {
            Preset::NervRed => "NERV red",
            Preset::NervGreen => "NERV green",
            Preset::Unit01Purple => "Unit-01 purple",
            Preset::AmberCrt => "Amber CRT",
            Preset::Bone => "Bone monochrome",
        }
    }

    pub fn all() -> &'static [Preset] {
        &[
            Preset::NervRed,
            Preset::NervGreen,
            Preset::Unit01Purple,
            Preset::AmberCrt,
            Preset::Bone,
        ]
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace('_', "-");
        match s.as_str() {
            "red" | "nerv-red" | "nervred" => Some(Preset::NervRed),
            "green" | "nerv-green" | "nervgreen" => Some(Preset::NervGreen),
            "purple" | "unit-01-purple" | "unit01" | "unit-01" => Some(Preset::Unit01Purple),
            "amber" | "amber-crt" | "ambert" => Some(Preset::AmberCrt),
            "bone" | "monochrome" | "mono" => Some(Preset::Bone),
            _ => None,
        }
    }
}

/// Parse a colour from the strings a config file can hold.
///
/// Accepts `#rgb`, `#rrggbb` and `#rrggbbaa`, with or without the hash, plus
/// `rgb()`/`rgba()`. Returns `None` rather than a default for anything else, so
/// a typo is visibly wrong instead of silently becoming red.
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);

    // `rgb(...)` / `rgba(...)`, comma or slash separated. Checked before the
    // hex path because "rgb(1,2,3)" is not hex digits and would otherwise fall
    // through to the length match and be rejected as a bad length.
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("rgba")
        .or_else(|| lower.strip_prefix("rgb"))
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(&[',', '/'][..]).map(str::trim).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let mut out = [255u8; 4];
        for (i, p) in parts.iter().enumerate() {
            out[i] = p.parse::<u8>().ok()?;
        }
        return Some(out);
    }

    let valid = hex.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return None;
    }
    match hex.len() {
        3 | 4 => {
            let mut out = [255u8; 4];
            for (i, c) in hex.chars().enumerate() {
                let v = c.to_digit(16)? as u8;
                out[i] = v * 17; // 0xf -> 0xff
            }
            Some(out)
        }
        6 | 8 => {
            let mut out = [255u8; 4];
            for i in 0..hex.len() / 2 {
                out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
            }
            Some(out)
        }
        _ => None,
    }
}

/// Format a colour the way a config file wants it.
pub fn format_color(c: [u8; 4]) -> String {
    if c[3] == 255 {
        format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nerv_red_is_the_reference_value() {
        assert_eq!(accent::RED, [0xe2, 0x34, 0x2a]);
        assert_eq!(Palette::default().accent, accent::RED);
    }

    #[test]
    fn paper_is_black_and_ink_is_bone() {
        let p = Palette::default();
        assert_eq!(p.paper, [0, 0, 0]);
        assert_eq!(p.ink, [0xcd, 0xc4, 0xba]);
    }

    /// Every preset must be reachable by the id that goes in the config file.
    #[test]
    fn preset_ids_round_trip() {
        for p in Preset::all() {
            assert_eq!(Preset::parse(p.id()), Some(*p), "{} did not round trip", p.id());
            assert!(!p.title().is_empty());
        }
    }

    #[test]
    fn preset_parse_is_forgiving_about_spelling() {
        assert_eq!(Preset::parse("NERV_Red"), Some(Preset::NervRed));
        assert_eq!(Preset::parse("unit-01"), Some(Preset::Unit01Purple));
        assert_eq!(Preset::parse(" mono "), Some(Preset::Bone));
        assert_eq!(Preset::parse("chartreuse"), None);
    }

    /// Bone is monochrome: its accent IS its ink, so nothing in the interface
    /// is coloured and an accent-coloured element reads as emphasis purely
    /// because it is brighter.
    #[test]
    fn bone_preset_is_monochrome() {
        let p = Palette::preset("bone");
        assert_eq!(p.accent, p.ink);
    }

    /// A hand-edited config naming a preset that does not exist must still
    /// produce a usable shell rather than a panic at startup.
    #[test]
    fn an_unknown_preset_falls_back_instead_of_failing() {
        assert_eq!(Palette::preset("chartreuse"), Palette::default());
        assert_eq!(Palette::preset(""), Palette::default());
    }

    #[test]
    fn hex_parsing_handles_the_forms_a_config_really_contains() {
        assert_eq!(parse_color("#e2342a"), Some([0xe2, 0x34, 0x2a, 0xff]));
        assert_eq!(parse_color("e2342a"), Some([0xe2, 0x34, 0x2a, 0xff]));
        assert_eq!(parse_color("#e2342a80"), Some([0xe2, 0x34, 0x2a, 0x80]));
        // Short form: f -> ff, 0 -> 00.
        assert_eq!(parse_color("#f00"), Some([0xff, 0, 0, 0xff]));
        assert_eq!(parse_color("#f008"), Some([0xff, 0, 0, 0x88]));
        assert_eq!(parse_color("rgb(226, 52, 42)"), Some([0xe2, 0x34, 0x2a, 0xff]));
        assert_eq!(parse_color("rgba(1,2,3,4)"), Some([1, 2, 3, 4]));
    }

    #[test]
    fn garbage_is_rejected_rather_than_becoming_red() {
        // Note what is NOT here: "#e2342a " with a trailing space. Surrounding
        // whitespace is trimmed, so a value picked up from a config file with a
        // stray space parses. That is the forgiving behaviour, not a bug.
        for bad in ["", "#", "#12", "#12345", "#gggggg", "red", "rgb(1,2)", "rgb(1,2,3,4,5)"] {
            assert_eq!(parse_color(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn colours_round_trip_through_formatting() {
        for c in [
            [0xe2, 0x34, 0x2a, 0xff],
            [0x00, 0x00, 0x00, 0xff],
            [0xcd, 0xc4, 0xba, 0xff],
            [0x01, 0x02, 0x03, 0x04],
        ] {
            assert_eq!(parse_color(&format_color(c)), Some(c));
        }
    }

    #[test]
    fn alpha_outside_zero_to_one_is_clamped() {
        let p = Palette::default();
        assert_eq!(p.ink_at(1.5).3, 1.0);
        assert_eq!(p.ink_at(-0.5).3, 0.0);
    }

    /// Bone is warm, not white. If this ever "passes" with a cool grey the
    /// whole design language has drifted.
    #[test]
    fn bone_is_warm_not_white() {
        let p = Palette::default();
        let (r, g, b, _) = p.ink_at(1.0);
        assert!(r > g, "red channel should lead for a warm bone");
        assert!(g > b, "blue channel should trail");
        assert_ne!(p.ink, [255, 255, 255]);
    }
}