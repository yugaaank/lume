//! GPUI-side adapter over [`services::theme`].
//!
//! ## Why this file exists and `services::theme` does not
//!
//! The palette itself lives in `services` so the settings process — a separate
//! binary — can read and write it without a renderer. This is the half that
//! needs GPUI: turning `[u8; 3]` into an `Rgba`, and resolving which palette is
//! current from the config.
//!
//! It is a thin adapter on purpose. Every rule about what a colour *means*
//! belongs in `services::theme`; this only converts.

use gpui::{App, Rgba};

use services::{AppState, theme};

/// Resolve the current palette from config.
///
/// Called during render, so it must be cheap. `config.get()` clones the whole
/// `Config`, which is not cheap — see [`Palette`] for the cached path.
pub fn current(cx: &App) -> Palette {
    Palette::resolve(&cx.global::<AppState>().config.get().appearance)
}

/// A palette plus its GPUI colours, resolved once per render pass.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: Rgba,
    pub ink: Rgba,
    pub paper: Rgba,
}

impl Palette {
    pub fn resolve(appearance: &services::config::Appearance) -> Self {
        let mut p = theme::Palette::preset(appearance.preset.id());

        // An explicit accent overrides the preset's. Parsed here rather than at
        // write time so a hand-edited config with a bad colour falls back to
        // the preset instead of failing to draw.
        if let Some(text) = &appearance.accent
            && let Some([r, g, b, _]) = theme::parse_color(text)
        {
            p.accent = [r, g, b];
        }

        Self {
            accent: rgba(p.accent_at(1.0)),
            ink: rgba(p.ink_at(1.0)),
            paper: rgba(p.paper_at(1.0)),
        }
    }

    /// Ink at an alpha. The workhorse: almost every rule in the design system
    /// is "the ink, quieter".
    ///
    /// The alpha field is set rather than multiplied. `ColorExt::opacity`
    /// scales the existing alpha, which would compound across nested calls and
    /// quietly darken anything built from two faded colours.
    pub fn ink_a(&self, alpha: f32) -> Rgba {
        faded(self.ink, alpha)
    }

    pub fn accent_a(&self, alpha: f32) -> Rgba {
        faded(self.accent, alpha)
    }

    pub fn paper_a(&self, alpha: f32) -> Rgba {
        faded(self.paper, alpha)
    }

    /// `line` — a hairline divider or card border.
    pub fn line(&self) -> Rgba {
        self.ink_a(theme::alpha::LINE)
    }

    /// `line_soft` — an almost-invisible divider between rows of one card.
    pub fn line_soft(&self) -> Rgba {
        self.ink_a(theme::alpha::LINE_SOFT)
    }

    /// The island pill's border: ink mixed 22% into paper, fully opaque.
    ///
    /// Not `line()`, which is ink at an alpha. Over a near-opaque pill that is
    /// already lighter than the desktop, a translucent border reads as a smear;
    /// mixing produces the same visual weight as an opaque hairline and stays
    /// opaque when the wallpaper underneath changes. The reference's
    /// `v2BarBorder`, and the same reason it is defined as a mix there.
    pub fn pill_border(&self) -> Rgba {
        let m = 0.22;
        mix(self.paper, self.ink, m)
    }

    pub fn line_strong(&self) -> Rgba {
        self.ink_a(theme::alpha::LINE_STRONG)
    }

    /// The inverted fill used for a selected row or a pressed pill: bone
    /// background, black text. The single most recognisable move in the whole
    /// design language.
    pub fn bone_fill(&self) -> Rgba {
        self.ink_a(1.0)
    }

    pub fn ink_on_bone(&self) -> Rgba {
        self.paper_a(1.0)
    }

    pub fn surface_hover(&self) -> Rgba {
        self.ink_a(theme::alpha::SURFACE_HOVER)
    }

    pub fn control_hover(&self) -> Rgba {
        self.ink_a(theme::alpha::CONTROL_HOVER)
    }

    pub fn pressed(&self) -> Rgba {
        self.ink_a(theme::alpha::PRESSED)
    }

    /// Secondary body text.
    pub fn muted(&self) -> Rgba {
        self.ink_a(theme::alpha::MUTED)
    }

    /// Labels, hints, disabled text.
    pub fn faint(&self) -> Rgba {
        self.ink_a(theme::alpha::FAINT)
    }
}

/// `Rgba` from the 0..1 floats `services::theme` hands back.
///
/// Built through the `From` impls rather than a struct literal: `Rgba` is
/// `Alpha<Rgb, f32>`, so the channels live on a nested `color` field.
fn rgba(c: (f32, f32, f32, f32)) -> Rgba {
    Rgba::from((c.0, c.1, c.2, c.3))
}

/// Replace alpha outright, keeping the colour.
///
/// `ColorExt::opacity` multiplies; this sets. Every alpha in the design system is
/// an absolute value from `theme::alpha`, so multiplying would be wrong — and it
/// would compound across nested calls, quietly darkening anything built from two
/// already-faded colours.
fn faded(c: Rgba, alpha: f32) -> Rgba {
    Rgba::from((c.red, c.green, c.blue, alpha.clamp(0.0, 1.0)))
}

/// Blend `b` into `a` by `t`, opaque.
///
/// Per-channel rather than an alpha composite, so `a` must be opaque. Used for
/// the one rule that is a colour in its own right — the pill border — rather
/// than "the ink, quieter", which is what every other rule in the bar is.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba::from((
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
        1.0,
    ))
}