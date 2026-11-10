//! The window's own preferences: how it looks and behaves, the recent projects, whether the side panel is open.
//! `~/Library/Application Support/Cordata/prefs.toml`, written atomically and readable only by its owner.
//!
//! What the agent itself decides (the model, what it may run) is not here: those live in the
//! backend's settings files and are changed through `_cordata/settings/*` and `_cordata/trust/*`. Secrets are never
//! here either — the API key is in the Keychain (`keychain.rs`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many recent projects are kept.
pub const RECENTS_CAP: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The person's choice for the left sidebar; `None` defaults to closed (Logbook drawer ⌘B).
    pub sidebar_open: Option<bool>,
    /// The person's choice for the side panel; `None` follows the window width.
    pub panel_open: Option<bool>,
    /// `system` (follow macOS Light/Dark) · `light` · `2049` · `graphite` · `dim`.
    pub theme: String,
    /// `compact` · `comfortable`.
    pub density: String,
    pub code_font_size: f32,
    /// `enter` (↵ sends, ⇧↵ newline) · `cmd-enter` (⌘↵ sends, ↵ newline).
    pub send_with: String,
    /// `last` (the last project) · `home` · `picker` (open the project picker).
    pub open_on_launch: String,
    /// Default workspace view: `canvas` (Control Room) · `transcript` · `sheet`.
    pub default_mode: String,
    /// Put the threads that were open back at launch (`threads.rs`). Off: a launch starts one new thread.
    pub restore_threads: bool,
    pub notify_approval: bool,
    /// A notification when a thread finishes while the window is in the background.
    pub notify_done: bool,
    /// Cordata lives in the menu bar: the mark shows what needs you, the close button hides the app instead of ending it.
    pub menu_bar: bool,
    /// Cordata's two sounds (`sound.rs`): needs-you and done, for what happens out of sight. The key keeps its old
    /// name so saved preferences carry over.
    pub sound_on_completion: bool,
    /// The global quick-ask hotkey brings the window forward with the composer focused.
    pub quick_ask_hotkey: bool,
    /// Which combination it is, `ctrl-alt-space` form (`platform::Combo::parse`).
    pub quick_ask_key: String,
    /// The chat model new threads start with (the switcher's last pick); `None` follows the engine's default.
    pub model: Option<String>,
    /// The rail's and the side panel's widths, as the person dragged them; `None` is the layout's default.
    pub rail_w: Option<f32>,
    pub dock_w: Option<f32>,
    /// The meter under the header: time to first word, turn time, tokens, context fill.
    pub show_meter: bool,
    /// The context gauge in the meter (how full the model's window is).
    pub show_context: bool,
    /// Most recent first.
    pub recents: Vec<RecentProject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentProject {
    pub path: PathBuf,
    /// Seconds since the epoch.
    pub opened: u64,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            sidebar_open: None,
            panel_open: None,
            theme: "system".into(),
            density: "comfortable".into(),
            code_font_size: 13.0,
            send_with: "enter".into(),
            open_on_launch: "last".into(),
            default_mode: "transcript".into(),
            restore_threads: true,
            notify_approval: true,
            notify_done: true,
            menu_bar: true,
            sound_on_completion: false,
            quick_ask_hotkey: true,
            quick_ask_key: "ctrl-alt-space".into(),
            model: None,
            rail_w: None,
            dock_w: None,
            show_meter: true,
            show_context: true,
            recents: Vec::new(),
        }
    }
}

impl gpui::Global for Prefs {}

/// The window's state folder: `~/Library/Application Support/Cordata` (`CORDATA_APP_PREFS_DIR` moves it, for tests).
pub fn dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CORDATA_APP_PREFS_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support/Cordata"))
}

pub fn path() -> Option<PathBuf> {
    dir().map(|d| d.join("prefs.toml"))
}

impl Prefs {
    /// The saved preferences, or the defaults (a missing or damaged file is not an error worth a dialog).
    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else { return Self::default() };
        match toml::from_str::<Prefs>(&text) {
            Ok(mut prefs) => {
                prefs.normalise();
                prefs
            }
            Err(error) => {
                // The next save would replace it: keep the person's file first, as herdr does with session.json.
                let kept = keep_damaged(path, &text, now()).map_or_else(|e| format!("could not keep a copy: {e}"), |p| format!("kept as {}", p.display()));
                eprintln!("cordata: {} did not parse ({error}); using defaults, {kept}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if let Some(p) = path()
            && let Err(error) = self.save_to(&p)
        {
            eprintln!("cordata: could not save {} ({error})", p.display());
        }
    }

    /// Write to a temporary file beside it and rename over: a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        write_private(path, toml::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes())
    }

    fn normalise(&mut self) {
        let fallback = Self::default();
        if !matches!(self.theme.as_str(), "system" | "light" | "2049" | "graphite" | "dim") {
            self.theme = fallback.theme;
        }
        if !matches!(self.density.as_str(), "compact" | "comfortable") {
            self.density = fallback.density;
        }
        if !matches!(self.send_with.as_str(), "enter" | "cmd-enter") {
            self.send_with = fallback.send_with;
        }
        if !matches!(self.open_on_launch.as_str(), "last" | "home" | "picker") {
            self.open_on_launch = fallback.open_on_launch;
        }
        if crate::platform::Combo::parse(&self.quick_ask_key).is_err() {
            self.quick_ask_key = fallback.quick_ask_key;
        }
        self.code_font_size = self.code_font_size.clamp(10.0, 20.0);
        self.recents.truncate(RECENTS_CAP);
    }

    /// Put `path` first in the recents.
    pub fn touch_recent(&mut self, path: &Path, now: u64) {
        self.recents.retain(|r| r.path != path);
        self.recents.insert(0, RecentProject { path: path.to_path_buf(), opened: now });
        self.recents.truncate(RECENTS_CAP);
    }

    pub fn last_project(&self) -> Option<&Path> {
        self.recents.first().map(|r| r.path.as_path())
    }
}

/// Writes `bytes` to `path` through a temporary file of its own beside it (owner-only), then renames over: a crash never
/// leaves half a file, and two writers never rename each other's.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or_default();
    let tmp = path.with_extension(format!("toml.{}.{nanos}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// How many damaged copies are kept beside the file; older ones go only after a new one is safely written.
const DAMAGED_KEEP: usize = 3;

/// Copies a file that did not parse to `<name>.damaged-<seconds>` beside it (owner-only), keeping the newest few.
/// Nothing restores them on its own: the person copies one back.
fn keep_damaged(path: &Path, text: &str, now: u64) -> std::io::Result<PathBuf> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Err(std::io::Error::other("no folder"));
    };
    let prefix = format!("{name}.damaged-");
    let mut existing: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix)))
        .collect();
    // The same bytes kept already (every launch reads the damaged file again): nothing new to keep.
    if let Some(same) = existing.iter().find(|p| std::fs::read_to_string(p).is_ok_and(|t| t == text)) {
        return Ok(same.clone());
    }
    let copy = dir.join(format!("{prefix}{now}"));
    let tmp = dir.join(format!(".{prefix}{now}.tmp"));
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &copy)?;
    existing.push(copy.clone());
    existing.sort();
    let surplus = existing.len().saturating_sub(DAMAGED_KEEP);
    for old in existing.into_iter().take(surplus) {
        let _ = std::fs::remove_file(old);
    }
    Ok(copy)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_repairs_bad_values() {
        let dir = std::env::temp_dir().join(format!("cordata-prefs-{}", std::process::id()));
        let file = dir.join("prefs.toml");
        let mut prefs = Prefs { theme: "dim".into(), panel_open: Some(false), ..Prefs::default() };
        prefs.touch_recent(Path::new("/tmp/a"), 1);
        prefs.save_to(&file).unwrap();
        assert_eq!(Prefs::load_from(&file), prefs);
        std::fs::write(&file, "theme = \"neon\"\ncode_font_size = 99.0\nquick_ask_key = \"k\"\n").unwrap();
        let repaired = Prefs::load_from(&file);
        assert_eq!(repaired.theme, "system");
        assert_eq!(repaired.quick_ask_key, "ctrl-alt-space");
        assert_eq!(repaired.code_font_size, 20.0);
        std::fs::write(&file, "not toml at all [").unwrap();
        assert_eq!(Prefs::load_from(&file), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_is_kept_before_the_defaults_replace_it() {
        struct Dir(PathBuf);
        impl Dir {
            fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Dir(std::env::temp_dir().join(format!("cordata-damaged-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        let file = dir.path().join("prefs.toml");
        let copies = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".damaged-")).collect();
            names.sort();
            names
        };
        assert_eq!(keep_damaged(&file, "v1 [", 10).unwrap(), dir.path().join("prefs.toml.damaged-10"));
        assert_eq!(keep_damaged(&file, "v1 [", 11).unwrap(), dir.path().join("prefs.toml.damaged-10"), "the same bytes once");
        for (n, text) in [(12, "v2 ["), (13, "v3 ["), (14, "v4 [")] {
            keep_damaged(&file, text, n).unwrap();
        }
        assert_eq!(copies(dir.path()), ["prefs.toml.damaged-12", "prefs.toml.damaged-13", "prefs.toml.damaged-14"], "the newest three");
        std::fs::write(&file, "not toml [").unwrap();
        assert_eq!(Prefs::load_from(&file), Prefs::default());
        assert_eq!(copies(dir.path()).len(), 3);
        assert!(copies(dir.path()).iter().any(|n| std::fs::read_to_string(dir.path().join(n)).unwrap() == "not toml ["));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("prefs.toml.damaged-14")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn recents_are_most_recent_first_and_capped() {
        let mut prefs = Prefs::default();
        for i in 0..25 {
            prefs.touch_recent(&PathBuf::from(format!("/p/{i}")), i);
        }
        assert_eq!(prefs.recents.len(), RECENTS_CAP);
        assert_eq!(prefs.recents[0].path, PathBuf::from("/p/24"));
        prefs.touch_recent(Path::new("/p/20"), 99);
        assert_eq!(prefs.recents[0].path, PathBuf::from("/p/20"));
        assert_eq!(prefs.recents.iter().filter(|r| r.path == Path::new("/p/20")).count(), 1);
    }
}
