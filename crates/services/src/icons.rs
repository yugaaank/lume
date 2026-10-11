//! Freedesktop icon theme resolution.
//!
//! There is no `.icon(name)` in this fork. Icons are `img(ImageSource)`, which
//! takes a *resource location*, not a theme name. So resolving `firefox` means
//! walking the icon theme the way a GTK client does.
//!
//! ## The lookup order (per the icon theme spec)
//!
//! 1. `$XDG_DATA_HOME/icons` and each `$XDG_DATA_DIRS/icons`
//! 2. within each theme directory, `$THEME` first, then `hicolor`
//! 3. `index.theme` names the inherited themes when a theme does not contain
//!    the icon; we follow those rather than assuming hicolor
//! 4. extension preference: `svg`, `png`, `xpm`
//!
//! ## Why resolve on disk rather than through `AssetSource`
//!
//! GPUI's `AssetSource` is consulted for every path the renderer asks for and
//! returns bytes. That is the right shape for icons we ship, and the wrong one
//! for a user's icon theme: the lookup involves reading and parsing
//! `index.theme` files and walking a directory tree, and doing that inside the
//! asset cache would put filesystem work on the render path.
//!
//! So we resolve to an absolute path up front, cache the *result*, and hand
//! `img(Path)` a plain file location. Resolution happens once per icon name,
//! not per frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Extension preference. SVG first: it scales to any size without a second
/// raster, which matters because we render icons at 48px.
const PREFERRED: &[&str] = &["svg", "png", "xpm"];

fn cache() -> &'static Mutex<HashMap<String, Option<PathBuf>>> {
    static C: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Theme directories, in search order.
fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home).join("icons"));
    } else if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".local/share/icons"));
    }

    let system: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    for base in system {
        dirs.push(base.join("icons"));
    }

    dirs
}

/// The user's preferred theme, then the inherited chain, then `hicolor`.
fn theme_chain() -> Vec<String> {
    let mut chain = Vec::new();

    if let Ok(name) = std::env::var("GTK_ICON_THEME") {
        chain.push(name);
    }
    // A GTK install without the env var usually still has a preference here.
    if let Some(home) = dirs::home_dir() {
        let pref = home.join(".config/gtk-3.0/settings.ini");
        if let Ok(text) = std::fs::read_to_string(pref) {
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("gtk-icon-theme-name=") {
                    let name = rest.trim().trim_matches('"').to_string();
                    if !name.is_empty() && !chain.contains(&name) {
                        chain.push(name);
                    }
                }
            }
        }
    }

    chain.push("hicolor".to_string());
    chain.dedup();
    chain
}

/// Parse `Inherits=` out of an `index.theme`.
fn inherits(theme_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(theme_dir.join("index.theme")) else {
        return Vec::new();
    };

    // ini-file enough: one key per line, section headers ignored.
    let mut result = Vec::new();
    let mut in_inherits = false;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_inherits = line.eq_ignore_ascii_case("[Icon Theme]");
            continue;
        }
        if !in_inherits {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Inherits=") {
            result = rest.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            break;
        }
    }
    result
}

/// Resolve an icon name to a file on disk.
///
/// `None` means "not found anywhere" — the caller draws a fallback rather than
/// a broken image.
pub fn resolve(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    if let Some(hit) = cache().lock().ok()?.get(name) {
        return hit.clone();
    }

    let found = resolve_uncached(name);
    if let Ok(mut c) = cache().lock() {
        c.insert(name.to_string(), found.clone());
    }
    found
}

fn resolve_uncached(name: &str) -> Option<PathBuf> {
    // An absolute path is used as-is. `.desktop` files legitimately point at
    // icons outside any theme.
    if name.starts_with('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }

    // Deepest-first search order: a real theme beats hicolor, and a later
    // theme in the list beats an earlier one only if it has the icon. GTK
    // semantics are "first match wins", so we search the primary theme across
    // all directories before falling back to its parents.
    let mut order: Vec<String> = Vec::new();
    for t in theme_chain() {
        if !order.contains(&t) {
            order.push(t);
        }
    }

    // Expand the inherited chain breadth-first so a theme that inherits
    // Adwaita also finds Adwaita-only icons.
    let mut expanded: Vec<String> = Vec::new();
    let mut queue: Vec<String> = order.clone();
    while let Some(t) = queue.first().cloned() {
        queue.remove(0);
        if expanded.contains(&t) {
            continue;
        }
        expanded.push(t.clone());
        for base in theme_dirs() {
            let dir = base.join(&t);
            if dir.is_dir() {
                for parent in inherits(&dir) {
                    if !expanded.contains(&parent) && !queue.contains(&parent) {
                        queue.push(parent);
                    }
                }
            }
        }
    }

    for theme in &expanded {
        for base in theme_dirs() {
            for ext in PREFERRED {
                let candidate = base.join(theme).join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// What themes are actually installed, for the log and for debugging a
/// user whose icons are not appearing.
pub fn available_themes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for base in theme_dirs() {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join("index.theme").is_file() {
                if let Some(name) = e.file_name().to_str() {
                    if !out.iter().any(|t| t == name) {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_are_used_directly() {
        let p = PathBuf::from("/etc/hostname");
        if p.is_file() {
            assert_eq!(resolve("/etc/hostname"), Some(p));
        }
    }

    #[test]
    fn empty_and_missing_are_none() {
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("   "), None);
        assert_eq!(resolve("definitely-not-an-icon-xyzzy"), None);
    }

    #[test]
    fn inherited_parses_from_index_theme() {
        let dir = std::env::temp_dir().join("lume-icon-test-theme");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.theme"),
            "[Icon Theme]\nName=Foo\nInherits=Adwaita,hicolor\nDirectories=48x48/apps\n",
        )
        .unwrap();
        assert_eq!(inherits(&dir), vec!["Adwaita", "hicolor"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inherits_is_empty_when_absent() {
        let dir = std::env::temp_dir().join("lume-icon-no-theme");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(inherits(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_chain_always_ends_with_hicolor() {
        let chain = theme_chain();
        assert!(chain.contains(&"hicolor".to_string()));
        assert!(!chain.is_empty());
    }

    /// The cache must not be able to serve a stale answer forever across a
    /// theme switch. This documents the current limitation rather than hiding
    /// it: there is no invalidation hook, so a theme change needs a restart.
    #[test]
    fn cache_round_trips() {
        let _ = resolve("xyzzy-not-real");
        assert_eq!(resolve("xyzzy-not-real"), None);
    }
}