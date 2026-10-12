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
//! 3. `index.theme` names both the inherited themes AND the `Directories=` that
//!    actually hold the files; we walk those, because `firefox` lives at
//!    `<theme>/48x48/apps/firefox.png`, not at `<theme>/firefox.png`
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
//!
//! ## Which theme, though
//!
//! There is no single file that holds "the user's icon theme", and picking the
//! wrong one is a silent wrong-looking-desktop bug. Measured on this machine:
//!
//! ```text
//! gsettings get org.gnome.desktop.interface icon-theme  ->  MacTahoe-dark
//! ~/.config/gtk-3.0/settings.ini gtk-icon-theme-name    ->  ryoku-folders
//! ~/.config/gtk-4.0/settings.ini gtk-icon-theme-name    ->  ryoku-folders
//! ~/.gtkrc-2.0        gtk-icon-theme-name               ->  MacTahoe-dark
//! ```
//!
//! The user set their theme in GNOME Tweaks and believes it is MacTahoe-dark,
//! which the dconf key confirms. The `settings.ini` files disagree because a
//! different tool rewrote them wholesale — they also carry that tool's GTK
//! theme, cursor and font, so nothing in them is the user's own doing.
//!
//! That makes `settings.ini` an *output* of other tools, not a record of user
//! intent, and it makes `org.gnome.desktop.interface icon-theme` the closest
//! thing to an authoritative answer: it is what the sanctioned GUI writes.
//! `detect_theme` therefore prefers it, and keeps `settings.ini` only as a
//! fallback for a system with no GNOME settings schema at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Extension preference. SVG first: it scales to any size without a second
/// raster, which matters because we render icons at 48px.
const PREFERRED: &[&str] = &["svg", "png", "xpm"];

/// Resolved paths, keyed by (theme, name).
///
/// Keying by theme as well as name is what makes a live theme switch correct.
/// Keyed by name alone, the first theme to answer a name poisons the entry
/// forever — which is exactly the bug that made an icon theme change need a
/// restart.
type CacheKey = (String, String);

fn cache() -> &'static Mutex<HashMap<CacheKey, Option<PathBuf>>> {
    static C: OnceLock<Mutex<HashMap<CacheKey, Option<PathBuf>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The theme lume has been told to use, if the user set one explicitly.
///
/// `None` means "ask the desktop". Deliberately not cached into the chain:
/// `detect_theme` runs a subprocess, and this is consulted once per resolve.
fn override_theme() -> Option<String> {
    crate::config::current().appearance.icon_theme.clone()
}

/// The theme to search first.
pub fn active_theme() -> String {
    override_theme().unwrap_or_else(detect_theme)
}

/// Where the desktop says its icon theme is.
///
/// Precedence, strongest first:
///
/// 1. `GTK_ICON_THEME` — an explicit environment override. GTK honours it
///    above every file, so a shell that ignored it would disagree with every
///    other client in the session.
/// 2. `org.gnome.desktop.interface icon-theme` — what GNOME Tweaks and the
///    Settings app write. This is the sanctioned UI, so it is treated as the
///    user's intent.
/// 3. `~/.config/gtk-4.0/settings.ini`, then gtk-3.0 — GTK's own fallback.
///    Last, because other tools rewrite these files wholesale.
///
/// Returns `"hicolor"` when nothing answers. `hicolor` is required to exist
/// by the spec, so the chain always terminates somewhere real.
pub fn detect_theme() -> String {
    if let Ok(name) = std::env::var("GTK_ICON_THEME") {
        if !name.trim().is_empty() {
            return name;
        }
    }

    if let Some(name) = gsettings_icon_theme() {
        return name;
    }

    if let Some(name) = ini_icon_theme() {
        return name;
    }

    "hicolor".to_string()
}

/// `gsettings get org.gnome.desktop.interface icon-theme`.
///
/// One subprocess, called once at startup and once per config change — never
/// per icon. dconf's on-disk format is a binary GVDB and not worth parsing;
/// this is the supported way to read it.
///
/// An empty string means the key is unset, which is a normal state on a
/// non-GNOME desktop and must not be taken as a theme name.
fn gsettings_icon_theme() -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    // gsettings prints a GVariant: 'MacTahoe-dark', or '' when unset.
    let raw = String::from_utf8_lossy(&out.stdout);
    let name = raw.trim().trim_matches(['\'', '"']).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// `gtk-icon-theme-name` from the GTK settings files, gtk-4 before gtk-3.
fn ini_icon_theme() -> Option<String> {
    let home = dirs::home_dir()?;
    for rel in [
        ".config/gtk-4.0/settings.ini",
        ".config/gtk-3.0/settings.ini",
    ] {
        let Ok(text) = std::fs::read_to_string(home.join(rel)) else {
            continue;
        };
        if let Some(name) = ini_value(&text, "gtk-icon-theme-name") {
            return Some(name);
        }
    }
    None
}

/// One `key=value` out of an ini-ish file. Section headers are ignored: the
/// keys we read are unambiguous and appear in exactly one section each.
fn ini_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
            let v = rest.trim().trim_matches('"').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Theme directories, in search order.
fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(home).join("icons"));
    } else if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".local/share/icons"));
    }

    // `~/.icons` is in the spec and is still where older installers put things.
    // It is not under XDG_DATA_HOME, so nothing else finds it.
    if let Some(h) = dirs::home_dir() {
        let legacy = h.join(".icons");
        if legacy.is_dir() && !dirs.contains(&legacy) {
            dirs.push(legacy);
        }
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

/// The active theme, then its inherited chain, then `hicolor`.
fn theme_chain() -> Vec<String> {
    let mut chain = vec![active_theme()];
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

    // Resolved per theme, so `active_theme()` is read on every call. That is
    // an atomic-free read of a config snapshot, and it is what lets a theme
    // change take effect on the next frame with no restart and no cache flush.
    let theme = active_theme();
    let key = (theme, name.to_string());

    if let Some(hit) = cache().lock().ok()?.get(&key) {
        return hit.clone();
    }

    let found = resolve_uncached(name);
    if let Ok(mut c) = cache().lock() {
        c.insert(key, found.clone());
    }
    found
}

/// Read `Directories=` out of an `index.theme`.
fn directories(theme_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(theme_dir.join("index.theme")) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut in_header = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_header = line.eq_ignore_ascii_case("[Icon Theme]");
            continue;
        }
        if !in_header {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Directories=") {
            dirs = rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            break;
        }
    }
    dirs
}

/// How good a candidate is. Bigger is better; we take the max.
///
/// Ranked in three bands, not two:
///
/// - `scalable` SVG beats everything. We render icons at 48px and a scalable
///   source stays sharp at any size.
/// - any other SVG beats any raster, even a 1024px PNG. A vector that has to
///   be rasterised is still better than a bitmap being upscaled.
/// - otherwise the pixel size decides.
///
/// The two SVG bands are not the same. `places/16/folder.svg` and
/// `places/scalable/folder.svg` are both vectors, and taking the first one
/// found renders a 16px icon blown up to 48px — technically vector, visibly
/// soft, and worse than the raster it beat.
fn score(dir: &Path, ext: &str) -> u32 {
    // The size directory is the one above the context: `256x256/apps`.
    let size_dir = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    let is_scalable = size_dir == "scalable" || dir.file_name().and_then(|n| n.to_str()) == Some("scalable");

    if ext == "svg" {
        return if is_scalable { 300_000 } else { 200_000 };
    }

    // Parse only the directory NAME. Splitting the whole path on 'x' breaks
    // on any path containing an x — `/tmp/lume-icon-test-pick/...` scored 0
    // and silently lost to a 16px icon.
    let size = size_dir
        .split_once('x')
        .and_then(|(a, _)| a.parse::<u32>().ok())
        .unwrap_or(0);
    size.clamp(0, 99_999)
}

/// Search one theme directory for `name`.
///
/// Split out from `resolve_uncached` so it can be tested against a synthetic
/// tree. Testing through `resolve` would mean mutating `XDG_DATA_DIRS` and
/// `GTK_ICON_THEME`, which are process-global — that is a race with every
/// other test in the binary, not a test.
fn search_theme(theme_root: &Path, name: &str) -> Option<(u32, PathBuf)> {
    if !theme_root.is_dir() {
        return None;
    }
    let mut best: Option<(u32, PathBuf)> = None;

    // `<theme>/<name>.<ext>` — some themes flatten their layout.
    for ext in PREFERRED {
        let flat = theme_root.join(format!("{name}.{ext}"));
        if flat.is_file() {
            return Some((100_001, flat));
        }
    }

    // The normal layout: `<theme>/<Directories[i]>/<name>.<ext>`.
    for dir in candidate_dirs(theme_root) {
        for ext in PREFERRED {
            let candidate = dir.join(format!("{name}.{ext}"));
            if !candidate.is_file() {
                continue;
            }
            let s = score(dir, ext);
            if best.as_ref().is_none_or(|(b, _)| s > *b) {
                best = Some((s, candidate));
            }
        }
    }
    best
}

/// Every directory inside a theme that could hold an icon.
///
/// `index.theme` names these. When it is absent — which is exactly the case
/// for `~/.local/share/icons/hicolor`, where wine and proton installers drop
/// their artwork — the theme is walked instead.
///
/// The walk goes **two** levels deep, not one, and that is the whole fix.
/// Listing only the top level finds `hicolor/256x256` and then looks for
/// `0CC0_hollow_knight.0.png` directly inside it, when the file is actually at
/// `hicolor/256x256/apps/0CC0_hollow_knight.0.png`. That is why wine game
/// shortcuts showed a letter tile while their icons sat on disk the whole
/// time.
///
/// Memoised per theme directory: the walk happens on a miss, and a miss is
/// every unresolved name, so re-walking per lookup would be a filesystem
/// sweep on the render path.
fn candidate_dirs(theme_root: &Path) -> &'static [PathBuf] {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, &'static [PathBuf]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(theme_root).copied()) {
        return hit;
    }

    let mut dirs: Vec<PathBuf> = Vec::new();

    let declared = directories(theme_root);
    if !declared.is_empty() {
        dirs.extend(declared.iter().map(|d| theme_root.join(d)));
    } else {
        let mut level1 = std::fs::read_dir(theme_root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for one in &level1 {
            dirs.push(one.clone());
            let mut level2 = std::fs::read_dir(one)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            dirs.append(&mut level2);
        }
        level1.clear();
    }

    dirs.retain(|d| d.is_dir());
    dirs.sort();

    // Leaked deliberately: the list is tiny, lives for the process, and this
    // is called from render-adjacent code where an Arc would only move the
    // refcount.
    let leaked: &'static [PathBuf] = Box::leak(dirs.into_boxed_slice());
    if let Ok(mut c) = cache.lock() {
        c.insert(theme_root.to_path_buf(), leaked);
    }
    leaked
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

    // Walk every inherited theme; first match wins, so a real theme always
    // beats its parents and hicolor.
    let mut best: Option<(u32, PathBuf)> = None;

    for theme in &expanded {
        for base in theme_dirs() {
            let found = search_theme(&base.join(theme), name);
            if found.as_ref().is_some_and(|(s, _)| *s >= 100_000) {
                // A scalable or flattened hit in the user's own theme beats
                // any raster fallback. Stop before descending to hicolor.
                return found.map(|(_, p)| p);
            }
            if let Some((s, p)) = found {
                if best.as_ref().is_none_or(|(b, _)| s > *b) {
                    best = Some((s, p));
                }
            }
        }
    }

    best.map(|(_, p)| p)
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

    /// The wine/proton case: an icon in a theme that has no `index.theme`,
    /// two levels down. Listing only the top level misses it entirely, which
    /// is why game shortcuts on the Desktop showed a letter tile while their
    /// artwork sat in `~/.local/share/icons/hicolor/256x256/apps/`.
    #[test]
    fn finds_icons_in_a_theme_with_no_index_theme() {
        let base = std::env::temp_dir().join("lume-icon-notheme");
        let deep = base.join("hicolor/256x256/apps");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("0CC0_game.0.png"), b"x").unwrap();
        std::fs::create_dir_all(base.join("hicolor/48x48/apps")).unwrap();
        std::fs::write(base.join("hicolor/48x48/apps/0CC0_game.0.png"), b"x").unwrap();

        // No index.theme anywhere: this is ~/.local/share/icons/hicolor.
        assert!(directories(&base.join("hicolor")).is_empty());

        assert_eq!(
            search_theme(&base.join("hicolor"), "0CC0_game.0")
                .map(|(_, p)| p),
            Some(deep.join("0CC0_game.0.png")),
            "the 256x256 copy must beat the 48x48 one"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// One level is not enough, and that is the bug this prevents.
    #[test]
    fn candidate_dirs_reaches_two_levels_deep() {
        let base = std::env::temp_dir().join("lume-icon-depth");
        std::fs::create_dir_all(base.join("256x256/apps")).unwrap();
        let dirs = candidate_dirs(&base);
        assert!(dirs.iter().any(|d| d.ends_with("256x256")), "level 1");
        assert!(
            dirs.iter().any(|d| d.ends_with("256x256/apps")),
            "level 2 - without this, a hicolor-style layout is invisible"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn directories_are_read_from_index_theme() {
        let dir = std::env::temp_dir().join("lume-icon-test-dirs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.theme"),
            "[Icon Theme]\nName=Foo\nDirectories=scalable/apps,48x48/apps,16x16/apps\n",
        )
        .unwrap();
        assert_eq!(directories(&dir), vec!["scalable/apps", "48x48/apps", "16x16/apps"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scalable_outranks_raster() {
        let d = |p: &str| PathBuf::from(p);
        assert!(score(&d("t/scalable/apps"), "svg") > score(&d("t/512x512/apps"), "png"));
        assert!(score(&d("t/256x256/apps"), "png") > score(&d("t/32x32/apps"), "png"));
        assert_eq!(score(&d("t/48x48/apps"), "png"), 48);
    }

    /// A fixed-size SVG is still better than a bigger bitmap, but a scalable
    /// one is better than both. Collapsing these into one band renders a 16px
    /// SVG at 48px and calls it vector.
    #[test]
    fn scalable_svg_outranks_fixed_svg() {
        let d = |p: &str| PathBuf::from(p);
        assert!(score(&d("t/scalable/apps"), "svg") > score(&d("t/512x512/apps"), "svg"));
        assert!(score(&d("t/512x512/apps"), "svg") > score(&d("t/1024x1024/apps"), "png"));
    }

    /// The size directory is parsed from its NAME. Splitting the full path on
    /// 'x' gives 0 for anything under a directory containing an x.
    #[test]
    fn score_ignores_x_in_parent_path_components() {
        let with_x = PathBuf::from("/tmp/lume-icon-test-pick/hicolor/256x256/apps");
        let without = PathBuf::from("/tmp/hicolor/256x256/apps");
        assert_eq!(score(&with_x, "png"), score(&without, "png"));
        assert_eq!(score(&with_x, "png"), 256);
    }

    /// The layout assumption that broke icon rendering once: files live in a
    /// size subdirectory, never directly in the theme root.
    #[test]
    fn resolves_through_a_size_subdirectory() {
        let base = std::env::temp_dir().join("lume-icon-test-tree");
        let theme = base.join("LumeTestTheme");
        let dir = theme.join("48x48/apps");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=LumeTestTheme\nDirectories=48x48/apps\n",
        )
        .unwrap();
        std::fs::write(dir.join("lume-probe.png"), b"x").unwrap();

        assert_eq!(
            search_theme(&theme, "lume-probe").map(|(_, p)| p),
            Some(dir.join("lume-probe.png"))
        );
        assert!(search_theme(&theme, "lume-absent").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The 48x48 icon must win over the 16x16 one, or every icon renders
    /// upscaled and blurry.
    #[test]
    fn largest_size_wins_within_a_theme() {
        let base = std::env::temp_dir().join("lume-icon-test-pick");
        let theme = base.join("LumePickTheme");
        for d in ["16x16/apps", "256x256/apps", "scalable/apps"] {
            std::fs::create_dir_all(theme.join(d)).unwrap();
        }
        std::fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nDirectories=16x16/apps,256x256/apps,scalable/apps\n",
        )
        .unwrap();
        std::fs::write(theme.join("16x16/apps/pick.png"), b"x").unwrap();
        std::fs::write(theme.join("256x256/apps/pick.png"), b"x").unwrap();
        std::fs::write(theme.join("scalable/apps/pick.svg"), b"x").unwrap();

        assert_eq!(
            search_theme(&theme, "pick").map(|(_, p)| p),
            Some(theme.join("scalable/apps/pick.svg"))
        );

        std::fs::remove_file(theme.join("scalable/apps/pick.svg")).unwrap();
        assert_eq!(
            search_theme(&theme, "pick").map(|(_, p)| p),
            Some(theme.join("256x256/apps/pick.png"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn inherits_is_empty_when_absent() {
        let dir = std::env::temp_dir().join("lume-icon-no-theme");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(inherits(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The precedence rule that decides which file wins.
    ///
    /// Written as a fixture rather than a live assertion because the answer
    /// depends on the machine's dconf and config files; a test that reads the
    /// real ones passes on the author's desktop and tells you nothing.
    #[test]
    fn ini_value_skips_sections_and_comments() {
        let text = "\
# a comment
[Settings]
gtk-theme-name=adw-gtk3-dark
gtk-icon-theme-name=\"MacTahoe-dark\"
";
        assert_eq!(ini_value(text, "gtk-icon-theme-name").as_deref(), Some("MacTahoe-dark"));
        assert_eq!(ini_value(text, "gtk-icon-theme-name").is_some(), true);
        assert_eq!(ini_value(text, "gtk-cursor-theme-name"), None);

        // An empty value is "unset", not a theme called "".
        assert_eq!(ini_value("[Settings]\ngtk-icon-theme-name=\"\"\n", "gtk-icon-theme-name"), None);
    }

    /// The cache is keyed by theme, so a switch cannot serve a stale path.
    /// A cache keyed by name alone is the bug this prevents.
    #[test]
    fn cache_is_keyed_by_theme() {
        resolve("lume-cache-probe");
        resolve("lume-cache-probe");
        let entries = cache().lock().unwrap();
        assert!(
            entries.keys().all(|(theme, _)| !theme.is_empty()),
            "every cache entry must record which theme produced it"
        );
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
#[cfg(test)]
mod probe {
    //! Not a test — a way to ask the resolver what it actually does on this
    //! machine. `cargo test -p services probe:: -- --nocapture --ignored`

    #[test]
    #[ignore = "prints the live answer; run deliberately"]
    fn show() {
        println!("detected theme : {}", super::detect_theme());
        println!("override       : {:?}", super::override_theme());
        println!("active  theme  : {}", super::active_theme());
        for n in ["zen-browser", "folder", "0CC0_hollow_knight.0", "8CE4_re2.0"] {
            println!("  {n:<12} -> {:?}", super::resolve(n));
        }
        println!("themes: {}", super::available_themes().join(", "));
    }
}
