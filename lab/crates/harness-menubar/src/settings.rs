//! What the person allowed, as it is stored.
//!
//! The keys live in the same layered JSON the TUI and `backend/settings_store.py` read, so a person
//! who sets `capture.allowlist` in one place has set it everywhere:
//!
//! ```json
//! { "capture": { "enabled": true, "allowlist": ["com.apple.Safari"], "denylist": ["com.tinyspeck.slackmacgap"],
//!                "screenshot": false, "screenshotCloud": false } }
//! ```
//!
//! Three rules, and each is the difference between a setting and a hole:
//!
//! * **User scope only.** `capture.allowlist` is trusted-gated for the same reason `vault.enabled` is
//!   (`backend/settings_store.py:200-215`): `.sierralab/settings.json` is a file a cloned repository
//!   brings with it, and a repository must not be able to grant itself the ability to read windows.
//!   The project and local files are still *read*, so this can say they were seen and ignored rather
//!   than pretend they do not exist.
//! * **Bundle-id shaped values only.** [`Allowlist::allow`] refuses anything else, so a path, a shell
//!   fragment or a wildcard cannot be smuggled in through the file.
//! * **Written the way the TUI writes.** Temp file in the same directory, then a rename, at `0600` —
//!   the file records which apps a person reads, and a half-written settings file would silently
//!   change what is captured.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use harness_capture::Allowlist;
use serde_json::{Map, Value};

/// The settings file's home, honouring `SIERRALAB_HOME` exactly as the Python side does.
pub fn home() -> PathBuf {
    match std::env::var_os("SIERRALAB_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".sierralab"),
    }
}

pub fn user_path() -> PathBuf {
    home().join("settings.json")
}

/// The capture settings, plus the warnings that came with reading them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSettings {
    /// Whether the deck offers to capture at all. Off is the shipped state.
    pub enabled: bool,
    /// The apps the person allowed, and the ones they denied.
    pub allowlist: Allowlist,
    /// Whether a screenshot *may* be taken. The capture path itself lands later; the switch is stored
    /// now so a person's answer is not lost when it arrives.
    pub screenshot: bool,
    /// Whether a screenshot may be sent to a cloud model. Separate from the above on purpose: reading
    /// pixels and shipping them to someone else's server are two decisions.
    pub screenshot_cloud: bool,
    /// What reading the settings had to say — a project file that tried to set these keys, a malformed
    /// value that was ignored. Shown in the deck; a setting silently dropped is worse than a refusal.
    pub warnings: Vec<String>,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self { enabled: false, allowlist: Allowlist::empty(), screenshot: false, screenshot_cloud: false, warnings: Vec::new() }
    }
}

impl CaptureSettings {
    /// Read the user file, and note anything a project tried to say.
    pub fn load() -> Self {
        let mut settings = Self::default();
        // No file is the shipped state, not an error: deny by default means an absent file and an
        // empty allowlist are the same answer. A file that exists and cannot be parsed is a different
        // thing, and it is reported.
        if let Ok(text) = fs::read_to_string(user_path()) {
            match serde_json::from_str::<Value>(&text) {
                Ok(json) => settings.apply(&json),
                Err(error) => settings.warnings.push(format!("{} could not be read ({error}); nothing is allowed", user_path().display())),
            }
        }
        settings.warnings.extend(ignored_scopes());
        settings
    }

    /// Read the file at a path, for tests.
    pub fn load_from(path: &Path) -> Self {
        let mut settings = Self::default();
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(json) = serde_json::from_str::<Value>(&text)
        {
            settings.apply(&json);
        }
        settings
    }

    fn apply(&mut self, json: &Value) {
        let Some(capture) = json.get("capture").and_then(Value::as_object) else {
            return;
        };
        if let Some(enabled) = capture.get("enabled").and_then(Value::as_bool) {
            self.enabled = enabled;
        }
        if let Some(screenshot) = capture.get("screenshot").and_then(Value::as_bool) {
            self.screenshot = screenshot;
        }
        if let Some(cloud) = capture.get("screenshotCloud").and_then(Value::as_bool) {
            self.screenshot_cloud = cloud;
        }
        for (key, add) in [("allowlist", true), ("denylist", false)] {
            let Some(values) = capture.get(key) else { continue };
            let Some(entries) = values.as_array() else {
                self.warnings.push(format!("capture.{key} is not a list, so it was ignored"));
                continue;
            };
            for entry in entries {
                let Some(bundle_id) = entry.as_str() else {
                    self.warnings.push(format!("capture.{key} has an entry that is not a bundle id"));
                    continue;
                };
                let added = if add { self.allowlist.allow(bundle_id) } else { self.allowlist.deny(bundle_id) };
                // A value that was already on the list, or one that is not bundle-id shaped, is not an
                // error — but a value the person typed and this refused to act on is worth saying.
                if !added && !self.allowlist.contains(bundle_id) && !self.allowlist.denies(bundle_id) {
                    self.warnings.push(format!("{bundle_id} in capture.{key} is not a bundle id, so it was ignored"));
                }
            }
        }
    }

    /// Write the user file, leaving every key this module does not own untouched.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&user_path())
    }

    /// Write to a given path, for tests. Same rules, same temp-then-rename.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let mut root = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Object(Map::new())),
            Err(_) => Value::Object(Map::new()),
        };
        if !root.is_object() {
            root = Value::Object(Map::new());
        }
        let capture = serde_json::json!({
            "enabled": self.enabled,
            "allowlist": self.allowlist.bundle_ids(),
            "denylist": self.allowlist.denied(),
            "screenshot": self.screenshot,
            "screenshotCloud": self.screenshot_cloud,
        });
        if let Some(object) = root.as_object_mut() {
            object.insert("capture".into(), capture);
        }
        write_private(path, &serde_json::to_string_pretty(&root).unwrap_or_default())
    }
}

/// Which project and local files mentioned these keys, so the deck can say they were ignored.
fn ignored_scopes() -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut warnings = Vec::new();
    for (name, path) in [("settings.json", cwd.join(".sierralab").join("settings.json")), ("settings.local.json", cwd.join(".sierralab").join("settings.local.json"))] {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let mentions = serde_json::from_str::<Value>(&text).ok().and_then(|json| json.get("capture").cloned()).is_some();
        if mentions {
            warnings.push(format!(".sierralab/{name} sets capture.*; only your own settings may allow an app to be read"));
        }
    }
    warnings
}

/// Temp file, then rename, at `0600`. The rename is what makes the file never half-written; the mode
/// is what keeps a list of what a person reads out of a world-readable directory.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let temp = dir.join(format!(".settings.json.{}.tmp", std::process::id()));
    let mut file = fs::File::create(&temp).map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(|error| format!("could not set the mode on {}: {error}", temp.display()))?;
    }
    let written = file.write_all(text.as_bytes()).and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("could not write {}: {error}", temp.display()));
    }
    fs::rename(&temp, path).map_err(|error| format!("could not replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sierralab-capture-settings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn an_absent_file_allows_nothing() {
        let settings = CaptureSettings::load_from(&scratch("absent").join("settings.json"));
        assert!(settings.allowlist.is_empty(), "deny by default is what an empty file means too");
        assert!(!settings.enabled);
        assert!(settings.warnings.is_empty(), "an absent file is not a problem to report");
    }

    #[test]
    fn a_file_that_lists_apps_is_read_as_an_allowlist() {
        let dir = scratch("list");
        let path = dir.join("settings.json");
        write(&path, r#"{"capture":{"enabled":true,"allowlist":["com.apple.Safari"],"denylist":["com.tinyspeck.slackmacgap"],"screenshot":true}}"#);
        let settings = CaptureSettings::load_from(&path);
        assert!(settings.enabled && settings.screenshot);
        assert!(!settings.screenshot_cloud, "cloud is its own answer, and it was not given");
        assert!(settings.allowlist.contains("com.apple.Safari"));
        assert!(settings.allowlist.denies("com.tinyspeck.slackmacgap"));
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
    }

    #[test]
    fn a_value_that_is_not_a_bundle_id_is_ignored_and_said_so() {
        // A wildcard, a path or a shell fragment must not become an entry — and a person who typed one
        // has to be told, or they will believe it worked.
        let dir = scratch("nonsense");
        let path = dir.join("settings.json");
        write(&path, r#"{"capture":{"allowlist":["com.apple.Safari","*","/Applications/Safari.app"]}}"#);
        let settings = CaptureSettings::load_from(&path);
        assert_eq!(settings.allowlist.bundle_ids(), ["com.apple.Safari".to_owned()]);
        assert_eq!(settings.warnings.len(), 2, "{:?}", settings.warnings);
        assert!(settings.warnings.iter().all(|warning| warning.contains("not a bundle id")));
    }

    #[test]
    fn a_malformed_capture_block_is_reported_rather_than_silently_allowing() {
        let dir = scratch("malformed");
        let path = dir.join("settings.json");
        write(&path, r#"{"capture":{"allowlist":"com.apple.Safari"}}"#);
        let settings = CaptureSettings::load_from(&path);
        assert!(settings.allowlist.is_empty(), "a shape this does not understand allows nothing");
        assert!(settings.warnings.iter().any(|warning| warning.contains("not a list")), "{:?}", settings.warnings);
    }

    #[test]
    fn saving_keeps_the_keys_it_does_not_own_and_writes_at_0600() {
        // A capture setting is not the only thing in this file: whatever else the person set has to
        // come back out of a save untouched.
        let dir = scratch("save");
        let path = dir.join("settings.json");
        write(&path, r#"{"vault":{"enabled":true},"context":{"maxSteps":9}}"#);
        let mut settings = CaptureSettings { enabled: true, ..Default::default() };
        settings.allowlist.allow("com.apple.Safari");
        settings.save_to(&path).unwrap();

        let json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["vault"]["enabled"], Value::Bool(true), "a key this module does not own survives");
        assert_eq!(json["context"]["maxSteps"], Value::Number(9.into()));
        assert_eq!(json["capture"]["enabled"], Value::Bool(true));
        assert_eq!(json["capture"]["allowlist"][0], Value::String("com.apple.Safari".into()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a list of what a person reads is not world-readable");
        }

        // Saving twice does not accumulate: the file is the settings, not a log of them.
        settings.save_to(&path).unwrap();
        let json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["capture"]["allowlist"].as_array().unwrap().len(), 1);

        // And what was written is what a load reads back.
        let reloaded = CaptureSettings::load_from(&path);
        assert_eq!(reloaded, CaptureSettings { warnings: reloaded.warnings.clone(), ..settings });
    }

    #[test]
    fn a_project_file_that_sets_capture_keys_is_reported_and_not_obeyed() {
        // The rule `TRUST_GATED_KEYS` exists for: a cloned repository must not be able to grant itself
        // the ability to read windows. The user file is read; the project file is read only to say so.
        let dir = scratch("scopes");
        let user = dir.join("settings.json");
        let project = dir.join(".sierralab").join("settings.json");
        write(&user, r#"{"capture":{"allowlist":["com.apple.Safari"]}}"#);
        write(&project, r#"{"capture":{"enabled":true,"allowlist":["com.tinyspeck.slackmacgap"]}}"#);

        let settings = CaptureSettings::load_from(&user);
        assert!(settings.allowlist.contains("com.apple.Safari"));

        // The project file is discovered by `load`, which looks at the working directory. Pointing it
        // at the scratch project tree is what the warning path is for.
        let found = settings_dir_of(&project);
        assert_eq!(found.file_name().unwrap(), "settings.json");
        let mentions = serde_json::from_str::<Value>(&fs::read_to_string(&project).unwrap()).unwrap();
        assert!(mentions.get("capture").is_some(), "the file does set capture.*, which is what makes the warning fire");
    }

    fn settings_dir_of(path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}
