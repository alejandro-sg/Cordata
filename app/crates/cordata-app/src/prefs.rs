//! The window's own preferences: how it looks and behaves, the recent projects, whether the side panel is open.
//! `~/Library/Application Support/Cordata/prefs.toml`, written atomically and readable only by its owner.
//!
//! What the agent itself decides (the model, what it may run) is not here: those live in the
//! backend's settings files and are changed through `_cordata/settings/*` and `_cordata/trust/*`. Secrets are never
//! here either — the API key is in the Keychain (`keychain.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many recent projects are kept.
pub const RECENTS_CAP: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The person's choice for the left sidebar; `None` defaults to open.
    pub sidebar_open: Option<bool>,
    /// The person's choice for the side panel; `None` follows the window width.
    pub panel_open: Option<bool>,
    /// `system` (follow macOS Light/Dark) · `light` · `2049` · `graphite` · `dim`.
    pub theme: String,
    pub haze: bool,
    /// `compact` · `comfortable`.
    pub density: String,
    pub code_font_size: f32,
    /// `enter` (↵ sends, ⇧↵ newline) · `cmd-enter` (⌘↵ sends, ↵ newline).
    pub send_with: String,
    /// `last` (the last project) · `home` · `picker` (open the project picker).
    pub open_on_launch: String,
    pub notify_approval: bool,
    pub sound_on_completion: bool,
    /// The global quick-ask hotkey brings the window forward with the composer focused.
    pub quick_ask_hotkey: bool,
    /// Which combination it is, `ctrl-alt-space` form (`platform::Combo::parse`).
    pub quick_ask_key: String,
    /// The local Ollama model for ghost-text completions in the editor; empty is off.
    /// Show the session's estimated cost in the status bar (Claude models at list price; `prices` adds others).
    pub show_cost: bool,
    /// Dollars per million tokens, `[input, output]`, keyed by (part of) a model name: overrides the built-in list
    /// prices or prices a model that has none. Only ever set by hand in this file.
    pub prices: BTreeMap<String, [f64; 2]>,
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
            haze: true,
            density: "comfortable".into(),
            code_font_size: 13.0,
            send_with: "enter".into(),
            open_on_launch: "last".into(),
            notify_approval: true,
            sound_on_completion: false,
            quick_ask_hotkey: true,
            quick_ask_key: "ctrl-alt-space".into(),
            show_cost: true,
            prices: BTreeMap::new(),
            recents: Vec::new(),
        }
    }
}

impl gpui::Global for Prefs {}

pub fn path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CORDATA_APP_PREFS_DIR") {
        return Some(PathBuf::from(dir).join("prefs.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support/Cordata/prefs.toml"))
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
                eprintln!("cordata: {} did not parse ({error}); using defaults", path.display());
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
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // A name of its own: two writers never rename each other's half-written file.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or_default();
        let tmp = path.with_extension(format!("toml.{}.{nanos}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
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
        self.prices.retain(|_, [input, output]| input.is_finite() && output.is_finite() && *input >= 0.0 && *output >= 0.0);
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
        prefs.prices.insert("glm-4.6".into(), [0.6, 2.2]);
        prefs.touch_recent(Path::new("/tmp/a"), 1);
        prefs.save_to(&file).unwrap();
        assert_eq!(Prefs::load_from(&file), prefs);
        std::fs::write(&file, "theme = \"neon\"\ncode_font_size = 99.0\nquick_ask_key = \"k\"\n").unwrap();
        let repaired = Prefs::load_from(&file);
        assert_eq!(repaired.theme, "system");
        assert_eq!(repaired.quick_ask_key, "ctrl-alt-space");
        std::fs::write(&file, "[prices]\nok = [1.0, 2.0]\nnegative = [-1.0, 2.0]\n").unwrap();
        assert_eq!(Prefs::load_from(&file).prices.keys().collect::<Vec<_>>(), ["ok"]);
        assert_eq!(repaired.code_font_size, 20.0);
        std::fs::write(&file, "not toml at all [").unwrap();
        assert_eq!(Prefs::load_from(&file), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
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
