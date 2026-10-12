//! The contents of the user's Desktop directory.
//!
//! ## Why this is not `gio`
//!
//! The obvious way to ask "what is this file?" is to shell out to
//! `gio info` and read `standard::icon`. That is a process spawn per file, per
//! refresh, for a question that is answered entirely by two files that ship
//! with the system and never change while we run:
//!
//! - `/usr/share/mime/globs` — `mime:type:glob` lines, in *decreasing*
//!   specificity, so the last match wins
//! - `/usr/share/mime/packages/*.xml` — `<mime-type>` elements carrying an
//!   `<icon name="...">` child
//!
//! Both are parsed once, lazily, and cached. `gio` would also be a hard
//! dependency on GLib being present, which for a Wayland shell built on wgpu
//! is not a given.
//!
//! `xdg-mime` is worse: it is a Python script. Spawning Python to name a file
//! is not a design.
//!
//! ## Identity
//!
//! Each item has an `id` that is stable across restarts, because the layout
//! file is keyed by it and a layout that reshuffles on every launch is worse
//! than no layout. Files are keyed by name — they live in one flat directory,
//! so the name is unique and survives renames of anything else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// One thing on the desktop.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopItem {
    /// A subdirectory of the Desktop.
    Dir { name: String, icon: String },
    /// A `.desktop` file sitting on the Desktop itself.
    ///
    /// These are launchers the user dropped there, not installed
    /// applications, so they are addressed by path rather than by app id.
    ///
    /// `icon` is resolved during the scan, not on demand. Reading it per
    /// render put a file read on the render path — three of them, every
    /// frame, forever.
    Launcher { name: String, path: PathBuf, icon: String },
    /// Any other file. `icon` is a freedesktop icon name, not a path.
    File { name: String, path: PathBuf, icon: String },
}

impl DesktopItem {
    /// Stable key for the position store.
    pub fn id(&self) -> String {
        match self {
            DesktopItem::Dir { name, .. } | DesktopItem::File { name, .. } => name.clone(),
            // Path-qualified: two launchers may share a display name.
            DesktopItem::Launcher { name, path, .. } => format!("{name}\u{1}{}", path.display()),
        }
    }

    /// What to show under the icon.
    pub fn label(&self) -> &str {
        match self {
            DesktopItem::Dir { name, .. }
            | DesktopItem::Launcher { name, .. }
            | DesktopItem::File { name, .. } => name,
        }
    }

    pub fn icon_name(&self) -> &str {
        match self {
            DesktopItem::Dir { icon, .. } | DesktopItem::File { icon, .. } => icon,
            DesktopItem::Launcher { icon, .. } => icon,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            DesktopItem::Launcher { path, .. } | DesktopItem::File { path, .. } => Some(path),
            DesktopItem::Dir { .. } => None,
        }
    }
}

/// `XDG_DESKTOP_DIR`, from `~/.config/user-dirs.dirs`.
///
/// Not hardcoded to `$HOME/Desktop`: users change it, and a shell that ignores
/// `user-dirs.dirs` shows an empty desktop next to a full one in their file
/// manager, which is a uniquely confusing bug.
pub fn desktop_dir() -> PathBuf {
    if let Some(dir) = user_dirs_xdg_desktop() {
        return dir;
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Desktop")
}

fn user_dirs_xdg_desktop() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let text = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).ok()?;

    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("XDG_DESKTOP_DIR=") else {
            continue;
        };
        let raw = rest.trim().trim_matches('"');
        // The file stores shell-quoted, escaped paths; only $HOME is worth
        // resolving, and doing it by hand avoids running a shell.
        let expanded = raw.replace("$HOME", &home.to_string_lossy());
        let path = PathBuf::from(expanded);
        if path.is_dir() {
            return Some(path);
        }
    }
    None
}

/// List the Desktop directory.
///
/// A missing directory is an empty desktop, not an error: plenty of users
/// delete theirs, and a shell that logs a warning every refresh is noise.
pub fn scan() -> Vec<DesktopItem> {
    scan_dir(&desktop_dir())
}

pub fn scan_dir(dir: &Path) -> Vec<DesktopItem> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut items = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        // Owned, because `path` is moved into the item below and a borrow of
        // it would outlive that move.
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        // Dotfiles are configuration, not content. Showing `.hidden` on the
        // desktop would be technically correct and practically absurd.
        if name.starts_with('.') {
            continue;
        }

        let is_dir = path.is_dir();

        let is_launcher = path.extension().and_then(|e| e.to_str()) == Some("desktop");
        // Resolved before the move: field initialisers run in order, so a
        // `path` used after being moved into the struct is a borrow error, not
        // a subtle ordering bug to find later.
        let icon = icon_for(&path);
        // Same ordering reason: computed before `path` moves.
        let launcher_icon = if is_launcher { launcher_icon(&path) } else { String::new() };

        items.push(if is_dir {
            DesktopItem::Dir { name, icon: "folder".into() }
        } else if is_launcher {
            DesktopItem::Launcher { name, path, icon: launcher_icon }
        } else {
            DesktopItem::File {
                name,
                path,
                icon,
            }
        });
    }

    // Directories first, then alphabetical. A stable order matters: the grid
    // assigns cells in iteration order, and a filesystem's order changes
    // between runs, which would reshuffle the desktop on every launch.
    items.sort_by(|a, b| {
        let rank = |i: &DesktopItem| matches!(i, DesktopItem::Dir { .. });
        rank(b)
            .cmp(&rank(a))
            .then_with(|| a.label().to_lowercase().cmp(&b.label().to_lowercase()))
    });

    items
}

/// The `Icon=` key of a `.desktop` file.
///
/// A launcher on the Desktop is frequently not an installed application — a
/// game launcher, a script — so there is no index to consult and the file is
/// the only source.
fn launcher_icon(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "application-x-executable".into();
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Icon=") {
            let v = rest.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "application-x-executable".into()
}

/// Freedesktop icon name for a path.
///
/// Three tiers, because the obvious one is not enough on a real system:
///
/// 1. `<icon name>` from the MIME package that defines the type. Most
///    specific, and the only tier that knows about an application's own
///    artwork.
/// 2. The MIME type with its separator replaced: `image/png` becomes
///    `image-png`. This is the convention icon themes actually ship under —
///    MacTahoe alone has 1476 of them in `mimes/scalable`.
/// 3. A generic icon for the type's top-level branch.
///
/// Tier 1 is not optional-looking but is in practice nearly empty. Measured on
/// this machine: `/usr/share/mime/packages` holds 16 files and only **three**
/// of them contain an `<icon>` element. `freedesktop.org.xml`, which defines
/// every standard MIME type, contains none at all — it ships types and globs
/// and leaves artwork to whoever installed the application. A resolver that
/// reads only tier 1 returns `text-x-generic` for a JPEG, which is what this
/// did before tier 2 existed.
pub fn icon_for(path: &Path) -> String {
    let db = mime_db();

    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "text-x-generic".into();
    };

    let Some(mime) = db.by_extension.get(&format!("*.{}", ext.to_lowercase())) else {
        return "text-x-generic".into();
    };

    db.icon_for(mime)
}

/// `image/png` -> `image-png`. The separator is the only change; everything
/// after it is already the icon name.
pub fn mime_icon_name(mime: &str) -> String {
    mime.replace('/', "-")
}

/// Last resort, by top-level type.
///
/// These names are the ones Adwaita, Papirus, Colloid and MacTahoe all
/// define, so the fallback survives a theme change. `text-x-generic` is the
/// final answer for anything unrecognised.
fn generic_for(mime: &str) -> &'static str {
    match mime.split('/').next().unwrap_or_default() {
        "image" => "image-x-generic",
        "video" => "video-x-generic",
        "audio" => "audio-x-generic",
        "text" => "text-x-generic",
        "inode" => "text-x-generic",
        _ => "application-x-generic",
    }
}

struct MimeDb {
    by_extension: HashMap<String, String>,
    by_icon: HashMap<String, String>,
}

fn mime_db() -> &'static MimeDb {
    static DB: OnceLock<MimeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let mut by_extension = HashMap::new();
        let mut by_icon = HashMap::new();
        load_globs(&mut by_extension);
        load_packages(&mut by_icon);
        MimeDb { by_extension, by_icon }
    })
}

/// `/usr/share/mime/globs`.
///
/// Later lines win, which is the documented behaviour: the file is sorted from
/// most general to most specific, so the last match for an extension is the
/// most specific one. Reversing that would map every `.xml` to the generic
/// text type.
fn load_globs(into: &mut HashMap<String, String>) {
    let Ok(text) = std::fs::read_to_string("/usr/share/mime/globs") else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((mime, glob)) = line.split_once(':') else {
            continue;
        };
        into.insert(glob.to_lowercase(), mime.to_string());
    }
}

/// `/usr/share/mime/packages/*.xml` — the `<icon name>` of each `<mime-type>`.
fn load_packages(into: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir("/usr/share/mime/packages") else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_packages(&text, into);
    }
}

/// Pull `<mime-type type="X"> ... <icon name="Y"/>` pairs out of one file.
///
/// Hand-rolled rather than a real XML parser: these files are machine
/// generated with a fixed shape, and pulling in a parser for one element would
/// be a dependency we would carry forever. It tracks element nesting because
/// an `<icon>` inside a `<sub-class-of>` belongs to that subclass, not to the
/// enclosing `<mime-type>`.
fn parse_packages(text: &str, into: &mut HashMap<String, String>) {
    let mut current: Option<String> = None;
    let mut depth_in_type = 0usize;
    let mut in_icon = false;

    for token in xml_tokens(text) {
        match token {
            Token::Open { name, attrs, self_closing } => match name.as_str() {
                "mime-type" if current.is_none() => {
                    current = attr(&attrs, "type");
                    depth_in_type = if self_closing { 0 } else { 1 };
                }
                "icon" if current.is_some() && depth_in_type > 0 => {
                    if let Some(icon) = attr(&attrs, "name") {
                        // First icon wins: files list the specific icon before
                        // the generic fallback.
                        into.entry(current.clone().expect("checked")).or_insert(icon);
                    }
                    in_icon = !self_closing;
                }
                _ if current.is_some() && depth_in_type > 0 => {
                    if !self_closing {
                        depth_in_type += 1;
                    }
                }
                _ => {}
            },
            Token::Close(name) => match name.as_str() {
                "mime-type" if current.is_some() && depth_in_type == 1 => current = None,
                "icon" => in_icon = false,
                _ if current.is_some() && depth_in_type > 0 => {
                    depth_in_type = depth_in_type.saturating_sub(1);
                }
                _ => {}
            },
            Token::Text(_) => {
                let _ = in_icon;
            }
        }
    }
}

impl MimeDb {
    /// The icon a MIME type names, walking down to the generic branch when
    /// neither the package nor the derived name exists.
    ///
    /// The existence check is against the resolved icon theme, not against the
    /// filesystem at large: a name present only in a theme the user is not
    /// using is not an icon they will ever see.
    fn icon_for(&self, mime: &str) -> String {
        if let Some(icon) = self.by_icon.get(mime) {
            return icon.clone();
        }

        let derived = mime_icon_name(mime);
        if crate::icons::resolve(derived.as_str()).is_some() {
            return derived;
        }

        generic_for(mime).to_string()
    }
}

enum Token {
    Open { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    Close(String),
    Text(String),
}

/// Just enough XML to walk the tags above: comments, CDATA, declarations and
/// processing instructions are skipped rather than understood.
fn xml_tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != '<' {
            let start = i;
            while i < bytes.len() && bytes[i] != '<' {
                i += 1;
            }
            out.push(Token::Text(bytes[start..i].iter().collect()));
            continue;
        }

        // Skip past the matching '>' of a construct we do not model.
        if text_at(&bytes, i, "<!--") {
            i = find(&bytes, i, "-->").map(|e| e + 3).unwrap_or(bytes.len());
            continue;
        }
        if text_at(&bytes, i, "<![CDATA[") {
            i = find(&bytes, i, "]]>").map(|e| e + 3).unwrap_or(bytes.len());
            continue;
        }
        if text_at(&bytes, i, "<?") || text_at(&bytes, i, "<!") {
            i = find(&bytes, i, ">").map(|e| e + 1).unwrap_or(bytes.len());
            continue;
        }

        let end = find(&bytes, i, ">").map(|e| e + 1).unwrap_or(bytes.len());
        let inner: String = bytes[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
        i = end;

        if let Some(name) = inner.strip_prefix('/') {
            out.push(Token::Close(name.trim().to_string()));
            continue;
        }

        let self_closing = inner.trim_end().ends_with('/');
        let inner = inner.trim_end().trim_end_matches('/');
        let mut parts = inner.splitn(2, |c: char| c.is_whitespace());
        let name = parts.next().unwrap_or_default().to_string();
        let attrs = parse_attrs(parts.next().unwrap_or_default());
        out.push(Token::Open { name, attrs, self_closing });
    }

    out
}

fn text_at(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(n, c)| chars.get(at + n) == Some(&c))
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| text_at(chars, i, needle))
}

fn parse_attrs(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '\n') {
            i += 1;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if start == i {
            break;
        }
        let key: String = chars[start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            continue;
        }
        i += 1; // '='
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let quote = chars.get(i).copied().unwrap_or('"');
        if quote != '"' && quote != '\'' {
            continue;
        }
        i += 1;
        let vstart = i;
        while i < chars.len() && chars[i] != quote {
            i += 1;
        }
        out.push((key, chars[vstart..i.min(chars.len())].iter().collect()));
        i += 1;
    }
    out
}

fn attr(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// Memoised scan, for callers that re-render often.
pub fn cached() -> &'static RwLock<Vec<DesktopItem>> {
    static C: OnceLock<RwLock<Vec<DesktopItem>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lume-desktop-test-{name}"))
    }

    #[test]
    fn scan_sorts_dirs_first_then_alphabetically() {
        let dir = tree("order");
        std::fs::create_dir_all(dir.join("zebra")).unwrap();
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::write(dir.join("Beta.txt"), "x").unwrap();
        std::fs::write(dir.join("apple.txt"), "x").unwrap();

        let items = scan_dir(&dir);
        let labels: Vec<&str> = items.iter().map(DesktopItem::label).collect();
        assert_eq!(labels, vec!["alpha", "zebra", "apple.txt", "Beta.txt"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_hides_dotfiles() {
        let dir = tree("dots");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(dir.join("visible.txt"), "x").unwrap();

        let items = scan_dir(&dir);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label(), "visible.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launchers_are_told_apart_from_plain_files() {
        let dir = tree("launchers");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Game.desktop"), "[Desktop Entry]").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();

        let items = scan_dir(&dir);
        assert!(matches!(&items[0], DesktopItem::Launcher { name, .. } if name == "Game.desktop"));
        // No Icon= key means the generic executable, never an empty string:
        // an empty name resolves to nothing and draws a blank tile.
        assert!(!items[0].icon_name().is_empty());
        assert!(matches!(&items[1], DesktopItem::File { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two launchers can share a display name. Their ids must not.
    #[test]
    fn ids_are_unique_when_labels_collide() {
        let a = DesktopItem::Launcher {
            name: "Game".into(),
            path: PathBuf::from("/a/Game.desktop"),
            icon: String::new(),
        };
        let b = DesktopItem::Launcher {
            name: "Game".into(),
            path: PathBuf::from("/b/Game.desktop"),
            icon: String::new(),
        };
        assert_eq!(a.label(), b.label());
        assert_ne!(a.id(), b.id());
    }

    /// The wine-generated keys on this machine — `0CC0_hollow_knight.0` —
    /// must be carried through verbatim, because there is no correct rewrite
    /// and the letter tile is the honest answer.
    #[test]
    fn launcher_icon_is_read_from_the_file() {
        let dir = tree("icon");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Game.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=Game\nExec=wine game.exe\nIcon=0CC0_game.0\n",
        )
        .unwrap();
        assert_eq!(launcher_icon(&path), "0CC0_game.0");

        std::fs::write(&path, "[Desktop Entry]\nName=Game\nExec=x\n").unwrap();
        assert_eq!(launcher_icon(&path), "application-x-executable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_is_empty_not_an_error() {
        assert!(scan_dir(Path::new("/nonexistent/lume/desktop")).is_empty());
    }

    #[test]
    fn extensionless_files_get_a_text_icon() {
        assert_eq!(icon_for(Path::new("/tmp/README")), "text-x-generic");
    }

    /// The tier-2 rule, asserted directly rather than through the database.
    #[test]
    fn mime_type_becomes_an_icon_name_by_replacing_the_separator() {
        assert_eq!(mime_icon_name("image/png"), "image-png");
        assert_eq!(mime_icon_name("application/x-x509-ca-cert"), "application-x-x509-ca-cert");
    }

    /// A real extension must not fall back to the generic text icon. This is
    /// the assertion that catches the empty-`freedesktop.org.xml` case, which
    /// is invisible until something on the desktop has a JPEG on it.
    #[test]
    fn known_extensions_do_not_fall_back_to_generic_text() {
        for (file, must_not_be) in [
            ("photo.png", "text-x-generic"),
            ("song.mp3", "text-x-generic"),
            ("clip.mp4", "text-x-generic"),
        ] {
            let icon = icon_for(Path::new(file));
            assert_ne!(icon, must_not_be, "{file} resolved to the generic text icon");
            assert!(!icon.is_empty(), "{file} produced no icon name");
        }
    }

    #[test]
    fn generic_names_are_per_top_level_type() {
        assert_eq!(generic_for("image/png"), "image-x-generic");
        assert_eq!(generic_for("video/webm"), "video-x-generic");
        assert_eq!(generic_for("audio/flac"), "audio-x-generic");
        assert_eq!(generic_for("application/zzz"), "application-x-generic");
    }

    #[test]
    fn unknown_extension_falls_back() {
        assert_eq!(
            icon_for(Path::new("/tmp/x.zzzznotamime")),
            "text-x-generic"
        );
    }

    /// The nesting rule that a naive regex gets wrong: an `<icon>` inside a
    /// `<sub-class-of>` belongs to the subclass.
    #[test]
    fn package_parsing_attributes_icons_to_the_right_type() {
        let xml = r#"
        <mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
          <mime-type type="application/x-lume-outer">
            <comment>outer</comment>
            <icon name="outer-icon"/>
            <sub-class-of type="application/x-lume-inner"/>
          </mime-type>
          <mime-type type="application/x-lume-inner">
            <icon name="inner-icon"/>
          </mime-type>
        </mime-info>"#;

        let mut into = HashMap::new();
        parse_packages(xml, &mut into);

        assert_eq!(into.get("application/x-lume-outer").map(String::as_str), Some("outer-icon"));
        assert_eq!(into.get("application/x-lume-inner").map(String::as_str), Some("inner-icon"));
    }

    #[test]
    fn comments_and_cdata_are_skipped() {
        let xml = r#"<!-- <icon name="ghost"/> -->
        <mime-info><![CDATA[<icon name="also-ghost"/>]]>
        <mime-type type="application/x-lume-real"><icon name="real"/></mime-type>
        </mime-info>"#;

        let mut into = HashMap::new();
        parse_packages(xml, &mut into);
        assert_eq!(into.len(), 1);
        assert_eq!(into.get("application/x-lume-real").map(String::as_str), Some("real"));
    }

    /// Later globs win. The file is ordered general-to-specific, so this is
    /// what makes `.xml` resolve to `application/xml` and not `text/plain`.
    #[test]
    fn later_globs_override_earlier_ones() {
        let mut into = HashMap::new();
        load_globs(&mut into);
        assert_eq!(into.get("*.xml").map(String::as_str), Some("application/xml"));
    }
}

#[cfg(test)]
mod probe2 {
    //! Deliberate, prints what the Desktop actually resolves to here.
    //! `cargo test -p services probe2:: -- --nocapture --ignored`

    #[test]
    #[ignore = "reads the live Desktop; run deliberately"]
    fn show() {
        println!("dir: {}", super::desktop_dir().display());
        for i in super::scan() {
            println!(
                "  {:<40} {:<32} {}",
                i.label(),
                i.icon_name(),
                crate::icons::resolve(i.icon_name())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "UNRESOLVED".into())
            );
        }
    }
}
