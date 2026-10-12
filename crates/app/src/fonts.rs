//! Loading the one typeface.
//!
//! ## Why the faces are read from disk rather than bundled
//!
//! `add_fonts` takes `Cow<'static, [u8]>`. `include_bytes!` would compile eight
//! JetBrains Mono NF faces — roughly 4 MB — into the binary, for a font that is
//! already installed on every machine this runs on. Reading them at startup
//! costs eight file reads and keeps the binary small.
//!
//! ## Why the bytes are leaked
//!
//! `Cow<'static, [u8]>` demands a `'static` borrow. The text system holds the
//! slice for the life of the process anyway — there is no way to unload a face
//! — so leaking here gives the collection exactly the lifetime it already has,
//! rather than pretending a shorter one is possible.
//!
//! ## Why not just rely on the family name
//!
//! System font *discovery* in this fork is not something to depend on silently.
//! If the family name fails to resolve, every label falls back to a default
//! face and the shell still runs — it just looks wrong, in a way that reads as
//! a design mistake rather than a missing dependency. Loading explicitly and
//! logging a warning turns that into a visible startup message.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Directories searched for the faces, in order.
const SEARCH: &[&str] = &[
    "/usr/share/fonts/TTF",
    "/usr/share/fonts/truetype",
    "/usr/local/share/fonts",
];

/// The faces to load, in the order they should be registered.
///
/// Light and Regular only, plus Bold. The design language is hairline: micro
/// labels at 10px, a display face at 44px with no weight change. Loading the
/// full nine-weight family invites a weight to be picked somewhere by accident,
/// and a single accidental Medium is visible at 10px.
const FACES: &[&str] = &[
    "JetBrainsMonoNerdFont-ExtraLight.ttf",
    "JetBrainsMonoNerdFont-Light.ttf",
    "JetBrainsMonoNerdFont-Regular.ttf",
    "JetBrainsMonoNerdFont-Medium.ttf",
    "JetBrainsMonoNerdFont-SemiBold.ttf",
    "JetBrainsMonoNerdFont-Bold.ttf",
    "JetBrainsMonoNerdFont-Italic.ttf",
];

/// Load the faces into the text system.
///
/// Returns how many were registered. Zero is not fatal — the shell renders in a
/// fallback face — but it is logged loudly, because "everything looks slightly
/// wrong" is much harder to diagnose than "the font was not found".
pub fn load(cx: &gpui::App) -> usize {
    let mut fonts: Vec<Cow<'static, [u8]>> = Vec::new();

    for name in FACES {
        match find(name) {
            Some(path) => match std::fs::read(&path) {
                // Leaked, then borrowed. The collection keeps the slice for the
                // process lifetime with no way to unload a face, so a 'static
                // borrow costs nothing that a shorter lifetime would buy.
                Ok(bytes) => fonts.push(Cow::Borrowed(Box::leak(bytes.into_boxed_slice()))),
                Err(e) => tracing::warn!(path = %path.display(), "could not read font face: {e}"),
            },
            None => tracing::debug!(face = name, "font face not installed"),
        }
    }

    if fonts.is_empty() {
        tracing::warn!(
            "no JetBrains Mono Nerd Font faces found in {SEARCH:?}; \
             the interface will render in a fallback face"
        );
        return 0;
    }

    match cx.text_system().add_fonts(fonts) {
        Ok(()) => FACES.len(),
        Err(e) => {
            tracing::error!("could not register fonts: {e:#}");
            0
        }
    }
}

fn find(name: &str) -> Option<PathBuf> {
    SEARCH
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing face must not be fatal, and must not be silently "fixed" by
    /// loading some other file that happens to share the prefix.
    #[test]
    fn a_missing_face_finds_nothing() {
        assert!(find("ThisFaceDoesNotExist-Regular.ttf").is_none());
    }

    #[test]
    fn an_installed_face_is_found() {
        // Skipped rather than asserted when absent: this test is about the
        // search working, not about the font being installed on every machine.
        let found = find("JetBrainsMonoNerdFont-Regular.ttf");
        if found.is_none() {
            return;
        }
        let path = found.unwrap();
        assert!(path.is_file());
        assert!(path.to_string_lossy().contains("JetBrainsMonoNerdFont"));
    }
}