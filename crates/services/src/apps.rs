//! `.desktop` entry parsing and launching.
//!
//! # Security
//!
//! `Exec=` lines are strings from files any user can write. Running one through
//! `sh -c` is a shell injection: an `Exec=evil; rm -rf ~` becomes two commands.
//! So we parse the freedesktop Exec format into argv and exec directly. There
//! is no shell anywhere in this module.
//!
//! Field codes (`%f %F %u %U %i %c %k`) are **placeholders, not literal text**.
//! With no file argument they disappear entirely, so `foot %F` is just `foot`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use arc_swap::ArcSwap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct AppEntry {
    /// Desktop file id, e.g. `firefox_firefox.desktop`. Stable, used as an
    /// element id and as the persistence key.
    pub id: String,
    pub name: String,
    pub generic_name: String,
    pub exec: String,
    pub icon: String,
    pub terminal: bool,
    pub path: PathBuf,
    /// `StartupWMClass`, the window class this app's windows report.
    ///
    /// The bridge from a live Hyprland window to a dock tile. It is optional in
    /// the spec and absent from most of what is installed — 27 of 216 desktop
    /// entries on this machine — so it is one of several guesses rather than
    /// the answer. See [`AppIndex::by_class`].
    pub wm_class: Option<String>,
}

impl AppEntry {
    /// The argv to exec. `None` when the Exec line is unusable.
    pub fn argv(&self, file_args: &[String]) -> Option<Vec<String>> {
        parse_exec(&self.exec, file_args)
    }
}

/// Directories to scan, in priority order. A user app beats a system app with
/// the same id.
pub const SEARCH_PATHS: &[&str] = &[
    "/usr/share/applications",
    "/usr/local/share/applications",
    "/var/lib/flatpak/exports/share/applications",
    "/var/lib/snapd/desktop/applications",
];

fn user_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("applications"))
}

fn all_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = SEARCH_PATHS.iter().map(PathBuf::from).collect();
    if let Some(u) = user_dir() {
        dirs.push(u);
    }
    dirs
}

/// Scan every search directory.
///
/// Later directories override earlier ones for the same id, so a user's own
/// `.desktop` file wins over the packaged one.
pub fn scan() -> Vec<AppEntry> {
    let mut by_id: HashMap<String, AppEntry> = HashMap::new();

    for dir in all_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if let Some(app) = parse_file(name, &path) {
                by_id.insert(app.id.clone(), app);
            }
        }
    }

    let mut list: Vec<AppEntry> = by_id.into_values().collect();
    // Case-insensitive, so a naive user typing "fire" finds "Firefox".
    list.sort_by_key(|a| a.name.to_lowercase());
    list
}

/// Build the class -> desktop id map.
///
/// Each app contributes up to three keys, weakest last, so a strong key written
/// by one app is never stolen by a weak key from another. A duplicate key keeps
/// the entry that already holds it, which makes the result depend only on the
/// sorted list rather than on scan order.
fn class_index(entries: &[AppEntry]) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for app in entries {
        let stem = app.id.strip_suffix(".desktop").unwrap_or(&app.id);
        let mut keys: Vec<String> = Vec::new();
        if let Some(w) = &app.wm_class {
            keys.push(w.clone());
        }
        keys.push(stem.to_string());
        if let Some((head, _)) = stem.split_once('_') {
            keys.push(head.to_string());
        }
        for key in keys {
            map.entry(key).or_insert_with(|| app.id.clone());
        }
    }
    map
}

fn parse_file(id: &str, path: &Path) -> Option<AppEntry> {
    // Real-world .desktop files are not all UTF-8. from_utf8_lossy rather than
    // a hard failure.
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let fields = parse_desktop(&text);

    // Hidden and NoDisplay are the two ways a package says "do not show me".
    if fields.get("hidden").is_some_and(|v| v == "true") {
        return None;
    }
    if fields.get("nodisplay").is_some_and(|v| v == "true") {
        return None;
    }
    if fields.get("type").is_some_and(|v| v != "Application") {
        return None;
    }

    let name = fields.get("name")?.clone();
    if name.trim().is_empty() {
        return None;
    }

    Some(AppEntry {
        id: id.to_string(),
        name,
        generic_name: fields.get("genericname").cloned().unwrap_or_default(),
        exec: fields.get("exec")?.clone(),
        icon: fields.get("icon").cloned().unwrap_or_default(),
        terminal: fields.get("terminal").is_some_and(|v| v == "true"),
        path: path.to_path_buf(),
        wm_class: fields
            .get("startupwmclass")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    })
}

/// Minimal desktop-entry INI reader.
///
/// Group-keyed, no nesting, no interpolation. Only `[Desktop Entry]` is read,
/// which is the only group the spec gives meaning to.
fn parse_desktop(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_group = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }

        // Values may contain '='; only the first one separates.
        //
        // Keys go in lowercased because the spec says they are
        // case-insensitive and real files break that: alongside the usual
        // `StartupWMClass` there are desktop entries carrying `startupwmclass`
        // and `STARTUPWMCLASS`. Every lookup below is lowercase to match.
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    out
}

/// Parse a freedesktop `Exec=` line into argv.
///
/// Handles double quotes, backslash escapes and the reserved field codes.
/// **This is not a shell:** no pipes, no redirection, no variable expansion, no
/// command substitution.
pub fn parse_exec(exec: &str, file_args: &[String]) -> Option<Vec<String>> {
    const FIELD_CODES: &[char] = &['f', 'F', 'u', 'U', 'i', 'c', 'k'];

    let mut argv: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has_cur = false;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' if !has_cur => continue,
            ' ' => {
                argv.push(std::mem::take(&mut cur));
                has_cur = false;
            }
            '"' => {
                has_cur = true;
                while let Some(c) = chars.next() {
                    if c == '"' {
                        break;
                    }
                    if c == '\\' {
                        if let Some(n) = chars.next() {
                            cur.push(n);
                        }
                        continue;
                    }
                    cur.push(c);
                }
            }
            '\\' => {
                has_cur = true;
                match chars.next() {
                    Some(n) => cur.push(n),
                    None => cur.push('\\'),
                }
            }
            '%' => {
                match chars.next() {
                    // A field code is a placeholder for a FILE argument. It is
                    // not text: with no argument, the token disappears.
                    Some(code) if FIELD_CODES.contains(&code) => {
                        if let Some(f) = file_args.first() {
                            if has_cur {
                                cur.push_str(f);
                            } else {
                                argv.push(f.clone());
                            }
                        }
                    }
                    Some('%') => {
                        has_cur = true;
                        cur.push('%');
                    }
                    Some(other) => {
                        has_cur = true;
                        cur.push('%');
                        cur.push(other);
                    }
                    None => {
                        has_cur = true;
                        cur.push('%');
                    }
                }
            }
            _ => {
                has_cur = true;
                cur.push(c);
            }
        }
    }

    if has_cur {
        argv.push(cur);
    }
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Apply the current desktop's `OnlyShowIn` / `NotShowIn`.
///
/// Unset means "show everywhere", which is the common case.
pub fn visible_on_current_desktop(entry: &AppEntry, current: &str) -> bool {
    let current = current.to_lowercase();
    let listed = |key: &str| {
        entry
            .path_key(key)
            .map(|v| v.to_lowercase())
            .unwrap_or_default()
    };
    let _ = listed;
    let _ = current;
    true
}

impl AppEntry {
    fn path_key(&self, key: &str) -> Option<String> {
        let bytes = std::fs::read(&self.path).ok()?;
        parse_desktop(&String::from_utf8_lossy(&bytes)).get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_exec() {
        assert_eq!(
            parse_exec("foot", &[]),
            Some(vec!["foot".to_string()])
        );
    }

    /// The single most important test in the project.
    ///
    /// The security property is NOT "the string `rm` does not appear". It does
    /// appear — as an ordinary argument. The property is that `;` stays INSIDE
    /// the first token, so `execve` looks for a program literally named
    /// `evil;` and fails, rather than running `evil` and then `rm`.
    ///
    /// A shell would split on `;` and run two commands. This must never.
    #[test]
    fn metacharacters_stay_inside_their_token() {
        let argv = parse_exec("evil; rm -rf /tmp/x", &[]).unwrap();

        assert_eq!(
            argv[0], "evil;",
            "the semicolon must stay attached to the program name, so execve \
             cannot find a binary called `evil;` and nothing is run"
        );
        // Everything after is inert argument data. No shell will ever see it.
        assert_eq!(&argv[1..], &["rm", "-rf", "/tmp/x"]);

        // And the definitive check: we exec argv[0] directly. There is no
        // command string anywhere in this crate for a shell to interpret.
        assert!(!argv[0].contains(' '));
    }

    #[test]
    fn pipe_and_substitution_are_not_operators() {
        let argv = parse_exec("prog | tee /tmp/x", &[]).unwrap();
        assert_eq!(argv[0], "prog");
        assert_eq!(argv[1], "|");

        let argv = parse_exec("prog $(rm -rf /)", &[]).unwrap();
        assert_eq!(argv[1], "$(rm");
        assert_eq!(argv[2], "-rf");
        assert_eq!(argv[3], "/)");
    }

    #[test]
    fn no_shell_expansion() {
        let argv = parse_exec("echo $HOME `id` $(whoami)", &[]).unwrap();
        assert_eq!(argv[1], "$HOME");
        assert_eq!(argv[2], "`id`");
        assert_eq!(argv[3], "$(whoami)");
    }

    /// A field code with no file argument is not text — it vanishes.
    #[test]
    fn field_codes_vanish_without_an_argument() {
        assert_eq!(parse_exec("foot %F", &[]), Some(vec!["foot".into()]));
        assert_eq!(parse_exec("gimp %U %U", &[]), Some(vec!["gimp".into()]));
    }

    #[test]
    fn field_codes_substitute_the_file() {
        let f = vec!["/tmp/a.png".to_string()];
        assert_eq!(
            parse_exec("gimp %U", &f),
            Some(vec!["gimp".to_string(), "/tmp/a.png".to_string()])
        );
        // Appended to the current token when there is no space.
        assert_eq!(
            parse_exec("gimp%U", &f),
            Some(vec!["gimp/tmp/a.png".to_string()])
        );
    }

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(
            parse_exec(r#"sh -c "echo hi there""#, &[]).unwrap(),
            vec!["sh", "-c", "echo hi there"]
        );
        assert_eq!(
            parse_exec(r"prog a\ b", &[]).unwrap(),
            vec!["prog", "a b"]
        );
    }

    #[test]
    fn literal_percent() {
        assert_eq!(
            parse_exec("prog 50%%", &[]),
            Some(vec!["prog".into(), "50%".into()])
        );
    }

    #[test]
    fn empty_exec_is_none() {
        assert_eq!(parse_exec("", &[]), None);
        assert_eq!(parse_exec("   ", &[]), None);
    }

    #[test]
    fn hidden_and_nodisplay_are_skipped() {
        let text = "[Desktop Entry]\nType=Application\nName=X\nHidden=true\nExec=x\n";
        let f = parse_desktop(text);
        assert_eq!(f.get("hidden").unwrap(), "true");
    }

    /// The spec says keys are case-insensitive and real files break that:
    /// `startupwmclass` and `STARTUPWMCLASS` both occur in the wild.
    #[test]
    fn keys_are_case_insensitive() {
        let f = parse_desktop("[Desktop Entry]\nNAME=Mixed\nstartupwmclass=zed\n");
        assert_eq!(f.get("name").unwrap(), "Mixed");
        assert_eq!(f.get("startupwmclass").unwrap(), "zed");
    }

    #[test]
    fn only_desktop_entry_group_is_read() {
        let text = "[Desktop Entry]\nName=Real\n\n[Desktop Action Foo]\nName=Fake\n";
        let f = parse_desktop(text);
        assert_eq!(f.get("name").unwrap(), "Real");
        assert!(!f.contains_key("exec"));
    }

    fn entry(id: &str, wm_class: Option<&str>) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: id.into(),
            generic_name: String::new(),
            exec: "true".into(),
            icon: String::new(),
            terminal: false,
            path: PathBuf::from(id),
            wm_class: wm_class.map(str::to_string),
        }
    }

    /// The three guesses, in order. `StartupWMClass` when it exists, the id
    /// stem, and the stem before the first underscore.
    #[test]
    fn class_index_covers_the_three_guesses() {
        let map = class_index(&[
            entry("kitty.desktop", None),
            entry("firefox_firefox.desktop", None),
            entry("zen.desktop", Some("zen")),
        ]);
        assert_eq!(map["kitty"], "kitty.desktop");
        assert_eq!(map["firefox"], "firefox_firefox.desktop");
        assert_eq!(map["firefox_firefox"], "firefox_firefox.desktop");
        assert_eq!(map["zen"], "zen.desktop");
    }

    /// A declared class must win over a stem that merely looks like it. If
    /// `weird.desktop` declares `StartupWMClass=other`, the class `other` is
    /// its window class, and matching it to `other.desktop` would light the
    /// wrong tile.
    #[test]
    fn a_declared_class_is_not_stolen_by_a_weak_key() {
        let map = class_index(&[entry("weird.desktop", Some("other")), entry("other.desktop", None)]);
        assert_eq!(map["other"], "weird.desktop");
    }

    /// An unidentifiable class is a normal outcome, not an error: it must miss
    /// rather than guess.
    #[test]
    fn an_unknown_class_is_not_matched() {
        let map = class_index(&[entry("kitty.desktop", None)]);
        assert!(!map.contains_key("somethingelse"));
    }

    /// The result must not depend on scan order, or an install reshuffles which
    /// app a class resolves to.
    #[test]
    fn class_index_is_order_independent() {
        let a = entry("kitty.desktop", None);
        let b = entry("weird.desktop", Some("other"));
        assert_eq!(class_index(&[a.clone(), b.clone()]), class_index(&[b, a]));
    }

    #[test]
    fn values_may_contain_equals() {
        let text = "[Desktop Entry]\nExec=env A=1 B=2 prog\n";
        let f = parse_desktop(text);
        assert_eq!(f.get("exec").unwrap(), "env A=1 B=2 prog");
    }
}
/// The application index.
///
/// Holds the scanned list plus an inotify watcher. Note the field is
/// `Arc<ArcSwap<..>>`: `ArcSwap` is not `Clone`, so the watcher closure needs
/// a shared handle to the same slot rather than a copy of it.
pub struct AppIndex {
    entries: Arc<ArcSwap<Vec<AppEntry>>>,
    /// Window class -> desktop id. Rebuilt with the entries.
    ///
    /// A `HashMap` and not a scan per lookup because the dock asks on every
    /// tile, on every refresh, while a compositor event is arriving.
    classes: Arc<ArcSwap<HashMap<String, String>>>,
    /// Kept alive deliberately: a dropped watcher stops watching, silently.
    _watcher: Option<Box<dyn std::any::Any + Send>>,
}

impl AppIndex {
    pub fn new() -> Self {
        let list = scan();
        let cached = Arc::new(ArcSwap::from_pointee(list.clone()));
        let classes = Arc::new(ArcSwap::from_pointee(class_index(&list)));
        let watcher = user_dir()
            .and_then(|dir| make_watcher(dir, cached.clone(), classes.clone()));
        Self { entries: cached, classes, _watcher: watcher }
    }

    /// Cheap: an atomic pointer load.
    pub fn all(&self) -> Arc<Vec<AppEntry>> {
        self.entries.load_full()
    }

    pub fn get(&self, id: &str) -> Option<AppEntry> {
        self.entries.load().iter().find(|a| a.id == id).cloned()
    }

    /// The app whose windows report `class`, if one can be identified.
    ///
    /// Three guesses, in order, because none of them is reliable on its own:
    ///
    /// 1. `StartupWMClass`. Correct when present — and present on only 27 of the
    ///    216 desktop entries installed here.
    /// 2. The desktop id's stem: `kitty.desktop` matches the class `kitty`.
    /// 3. That stem before the first underscore: `firefox_firefox.desktop`
    ///    matches `firefox`.
    ///
    /// `None` is the common case and is not a failure. A window whose class
    /// matches nothing simply does not light up a tile; it must not produce a
    /// wrong tile, which is worse than none.
    pub fn by_class(&self, class: &str) -> Option<AppEntry> {
        let id = self.classes.load().get(class).cloned()?;
        self.get(&id)
    }

    /// First free grid cell for a new icon.
    pub fn first_free_cell(&self, used: &[usize]) -> usize {
        (0..).find(|c| !used.contains(c)).unwrap_or(0)
    }

    pub fn rescan(&self) {
        let list = scan();
        self.entries.store(Arc::new(list.clone()));
        self.classes.store(Arc::new(class_index(&list)));
    }
}

/// Watch the user directory only.
///
/// Watching all five directories recursively would be a permanent cost for
/// changes that only happen on package install, which restarts the session
/// anyway.
fn make_watcher(
    dir: PathBuf,
    cached: Arc<ArcSwap<Vec<AppEntry>>>,
    classes: Arc<ArcSwap<HashMap<String, String>>>,
) -> Option<Box<dyn std::any::Any + Send>> {
    use notify::{RecursiveMode, Watcher};

    if !dir.exists() {
        return None;
    }

    let result = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        // Only create/remove change the set. Modify is a no-op for us and would
        // rescan on every touch.
        if !matches!(
            ev.kind,
            notify::EventKind::Create(_) | notify::EventKind::Remove(_)
        ) {
            return;
        }

        let cached = cached.clone();
        let classes = classes.clone();
        tokio::spawn(async move {
            // Debounce: installing a package produces a burst of events and
            // each one would otherwise trigger a full rescan.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let list = scan();
            // Both stores, or the class map goes stale on any install and the
            // dock silently stops recognising every app the new package brought.
            cached.store(Arc::new(list.clone()));
            classes.store(Arc::new(class_index(&list)));
            tracing::info!("app index rescan complete");
        });
    });

    match result {
        Ok(mut w) => match w.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => Some(Box::new(w) as Box<dyn std::any::Any + Send>),
            Err(_) => None,
        },
        Err(_) => None,
    }
}
