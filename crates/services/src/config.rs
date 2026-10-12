//! Persistent user configuration.
//!
//! Two files, because they change on completely different timescales:
//!
//! - `config.toml` — settings the user edits deliberately
//! - `desktop-icons.toml` — icon positions, rewritten on every drag
//!
//! **One writer.** Settings (stage 11) writes `config.toml`; the shell only
//! reads it. If both wrote, you get last-writer-wins races and settings that
//! silently revert.
//!
//! First-run seeding uses an atomic create so two shells racing on first login
//! cannot produce two different defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub appearance: Appearance,
    pub bar: Bar,
    pub desktop: Desktop,
    pub dock: Dock,
}

/// When the dock is shown, and whether it reserves space.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DockMode {
    /// Always on screen, always reserving.
    #[default]
    AlwaysVisible,
    /// Off screen until the pointer reaches the bottom edge. Never reserves.
    Autohide,
    /// Reserve and show only when the active workspace has no tiled windows.
    /// Floating windows are ignored: a desktop with only a floating terminal
    /// looks empty, and the dock filling that gap is what a user wants.
    Dodge,
}

/// Dock contents.
///
/// Pinned desktop-file ids. Running apps appear here too, but only once we
/// have a window event stream to populate them — stage 5.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Dock {
    pub pinned: Vec<String>,
    /// Show the dock at all. `false` is the third power state: off entirely.
    pub enabled: bool,
    pub mode: DockMode,
    /// Pointer distance from the bottom edge that reveals an autohiding dock.
    pub trigger_px: f32,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            enabled: true,
            mode: DockMode::default(),
            trigger_px: 8.0,
        }
    }
}

/// Desktop icon configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Desktop {
    /// Desktop-file ids to show as icons, in order. An empty list means the
    /// desktop shows nothing, which is a perfectly normal setup — the
    /// launcher is how you start things.
    pub pinned: Vec<String>,
    pub show_files: bool,
}

impl Default for Desktop {
    fn default() -> Self {
        // Two well-known entries so a fresh install shows something and the
        // icon plumbing is visibly exercised rather than silently untested.
        Self {
            pinned: vec!["firefox_firefox.desktop".into(), "org.gnome.Terminal.desktop".into()],
            show_files: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub mode: String, // "Dark" | "Light"
    pub accent: String,
    pub font_family: String,
    /// When true, accent colours come from the wallpaper palette.
    pub follow_wallpaper: bool,
    /// Icon theme, by freedesktop theme name ("MacTahoe-dark").
    ///
    /// `None` means "ask the desktop", which is the right default: a shell
    /// that hardcodes an icon theme fights every other tool on the system.
    /// See `icons::detect_theme` for the precedence.
    pub icon_theme: Option<String>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            mode: "Dark".into(),
            accent: "#7aa2f7".into(),
            font_family: "Inter".into(),
            follow_wallpaper: true,
            icon_theme: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bar {
    /// Render order. The order in this vec is the order rendered — there is no
    /// separate position field.
    pub widgets: Vec<String>,
    pub height: f32,
    pub show_seconds: bool,
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            widgets: vec![
                "workspaces".into(),
                "clock".into(),
                "network".into(),
                "volume".into(),
                "battery".into(),
            ],
            height: 32.0,
            show_seconds: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            bar: Bar::default(),
            desktop: Desktop::default(),
            dock: Dock::default(),
        }
    }
}

/// Icon grid positions, keyed by desktop-file id.
///
/// Keyed by file identity, not display name: display names get duplicated and
/// change, and deleting one desktop file must not disturb the others' layout.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct IconPositions {
    #[serde(default)]
    pub cells: HashMap<String, usize>,
}

#[derive(Clone)]
pub struct ConfigService {
    config: Arc<ArcSwap<Config>>,
    icons: Arc<ArcSwap<IconPositions>>,
    dir: PathBuf,
    changed: Arc<tokio::sync::Notify>,
}

/// The live config, readable without an `App` handle.
///
/// `icons::resolve` is called from render paths and from the icon resolver,
/// neither of which should have to thread a `ConfigService` through. The
/// service publishes here on load and on every update, so there is one copy
/// and no second source of truth.
///
/// Publishing rather than sharing the service matters: a global holding a
/// service handle would let unrelated code call `update()` and write the user's
/// config from a render path.
static CURRENT: OnceLock<ArcSwap<Config>> = OnceLock::new();

fn current_slot() -> &'static ArcSwap<Config> {
    CURRENT.get_or_init(|| ArcSwap::from_pointee(Config::default()))
}

/// The current config. Defaults before `ConfigService::load` has run, which is
/// what the tests rely on.
pub fn current() -> Arc<Config> {
    current_slot().load_full()
}

fn publish(config: &Arc<Config>) {
    current_slot().store(config.clone());
}

impl ConfigService {
    pub fn load() -> anyhow::Result<Self> {
        let dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("lume");
        std::fs::create_dir_all(&dir)?;

        let config = load_or_seed::<Config>(&dir.join("config.toml"), "config.toml")?;
        let icons = load_toml::<IconPositions>(&dir.join("desktop-icons.toml")).unwrap_or_default();

        publish(&Arc::new(ArcSwap::from_pointee(config.clone())).load_full());

        Ok(Self {
            config: Arc::new(ArcSwap::from_pointee(config)),
            icons: Arc::new(ArcSwap::from_pointee(icons)),
            dir,
            changed: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Fires after `update`, for views that must re-read.
    ///
    /// `notify_one`, not `notify_waiters`: a view that is between renders is
    /// not a waiter, and a dropped "your config changed" is how a theme switch
    /// silently needs a restart. Coalescing is correct here — the signal says
    /// "go look", and looking twice is harmless.
    pub fn changed(&self) -> Arc<tokio::sync::Notify> {
        self.changed.clone()
    }

    /// Cheap: an atomic pointer load. Called from render().
    pub fn get(&self) -> Arc<Config> {
        self.config.load_full()
    }

    pub fn icons(&self) -> Arc<IconPositions> {
        self.icons.load_full()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Mutate and write through to disk.
    ///
    /// Write-through is not optional: a settings UI that only mutates memory
    /// loses the user's changes on restart, and two writers mean a
    /// last-writer-wins fight.
    pub fn update(&self, f: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        let mut next = (**self.config.load()).clone();
        f(&mut next);
        self.config.store(Arc::new(next.clone()));
        write_atomic(&self.dir.join("config.toml"), &toml::to_string_pretty(&next)?)?;
        // Published AFTER the successful write. A failed write must not leave
        // the running shell showing settings that are not on disk.
        publish(&self.config.load_full());
        self.changed.notify_one();
        Ok(())
    }

    /// Persist an icon position. Called on drag release, not on every pointer
    /// move — this file is rewritten constantly otherwise.
    pub fn set_icon_cell(&self, id: &str, cell: usize) -> anyhow::Result<()> {
        let mut next = (**self.icons.load()).clone();
        if next.cells.get(id) == Some(&cell) {
            return Ok(()); // no change, no write
        }
        next.cells.insert(id.to_string(), cell);
        self.icons.store(Arc::new(next.clone()));
        write_atomic(
            &self.dir.join("desktop-icons.toml"),
            &toml::to_string_pretty(&next)?,
        )?;
        Ok(())
    }

    /// Forget icons whose .desktop file has gone, so the layout file does not
    /// grow forever across package removals.
    pub fn prune_missing_icons(&self, present: &[String]) -> anyhow::Result<()> {
        let mut next = (**self.icons.load()).clone();
        let before = next.cells.len();
        next.cells.retain(|id, _| present.iter().any(|p| p == id));
        if next.cells.len() == before {
            return Ok(());
        }
        tracing::debug!(removed = before - next.cells.len(), "pruned stale icons");
        self.icons.store(Arc::new(next.clone()));
        write_atomic(
            &self.dir.join("desktop-icons.toml"),
            &toml::to_string_pretty(&next)?,
        )?;
        Ok(())
    }
}

fn load_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            // A malformed config is the user's problem to see, but not a reason
            // to refuse to start the shell.
            tracing::error!(path = %path.display(), "malformed config, using defaults: {e}");
            None
        }
    }
}

fn load_or_seed<T: Serialize + serde::de::DeserializeOwned + Default>(
    path: &Path,
    name: &str,
) -> anyhow::Result<T> {
    if let Some(v) = load_toml(path) {
        return Ok(v);
    }

    let defaults = T::default();
    let body = toml::to_string_pretty(&defaults)
        .map_err(|e| anyhow::anyhow!("could not serialise default {name}: {e}"))?;
    seed_atomic(path, &body)?;
    Ok(defaults)
}

/// Create only if genuinely absent. Never overwrite an existing file.
fn seed_atomic(path: &Path, body: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true); // fails if it exists

    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(body.as_bytes())?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write to a temp file then rename, so a crash mid-write cannot leave a
/// truncated config behind.
fn write_atomic(path: &Path, body: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lume-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn round_trips() {
        let dir = tempdir();
        let path = dir.join("c.toml");
        let original = Config {
            appearance: Appearance {
                accent: "#ff0000".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        write_atomic(&path, &toml::to_string_pretty(&original).unwrap()).unwrap();
        let back: Config = load_toml(&path).unwrap();
        assert_eq!(back, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_falls_back_rather_than_panicking() {
        let dir = tempdir();
        let path = dir.join("c.toml");
        std::fs::write(&path, "this is not = valid = toml [[[").unwrap();
        assert!(load_toml::<Config>(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_never_overwrites() {
        let dir = tempdir();
        let path = dir.join("seed.toml");
        seed_atomic(&path, "first").unwrap();
        seed_atomic(&path, "second").unwrap(); // must not clobber
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        // An older config with keys we no longer read must still load.
        let text = r#"
[bar]
height = 40.0
widgets = ["clock"]
future_option = true

[appearance]
mode = "Light"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.bar.height, 40.0);
        assert_eq!(cfg.appearance.mode, "Light");
        assert_eq!(cfg.bar.widgets, vec!["clock".to_string()]);
    }

    #[test]
    fn missing_keys_use_defaults() {
        let cfg: Config = toml::from_str("[bar]\nheight = 40.0\n").unwrap();
        assert_eq!(cfg.bar.height, 40.0);
        assert_eq!(cfg.bar.widgets, Bar::default().widgets);
    }
}