//! The slice of `backend/bandit.py` the dispatcher reads and writes: the per-context arm statistics. The file format
//! is the Python one (`{"version": 1, "seen": […], "contexts": {ctx: {"arms": {arm: {n, wins, mean, cost, …}}}}}`),
//! so both implementations share a table; fields this crate does not know are preserved on save.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

const VERSION: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    pub n: u64,
    pub wins: f64,
    pub mean: f64,
    pub cost: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Bandit {
    path: Option<PathBuf>,
    contexts: BTreeMap<String, Value>,
    seen: Value,
}

fn num(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

impl Bandit {
    /// An in-memory table (offline training).
    pub fn in_memory() -> Self {
        Self { seen: json!([]), ..Self::default() }
    }

    /// Loads `path` if it is a readable version-1 table; otherwise starts empty. `save` writes back to `path`.
    pub fn open(path: &Path) -> Self {
        let mut table = Self { path: Some(path.to_owned()), seen: json!([]), ..Self::default() };
        if let Some(data) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            if data.get("version").and_then(Value::as_u64) == Some(VERSION) {
                if let Some(contexts) = data.get("contexts").and_then(Value::as_object) {
                    table.contexts = contexts.iter().filter(|(_, v)| v.is_object()).map(|(k, v)| (k.clone(), v.clone())).collect();
                }
                if let Some(seen) = data.get("seen").filter(|s| s.is_array()) {
                    table.seen = seen.clone();
                }
            }
        }
        table
    }

    pub fn stat(&self, context: &str, arm: &str) -> Stat {
        let Some(s) = self.contexts.get(context).and_then(|c| c.get("arms")).and_then(|a| a.get(arm)) else { return Stat::default() };
        Stat { n: s.get("n").and_then(Value::as_f64).unwrap_or(0.0) as u64, wins: num(s.get("wins")), mean: num(s.get("mean")), cost: num(s.get("cost")) }
    }

    /// Folds one outcome into `context`/`arm` (running means, same arithmetic and rounding as `observe_arm`).
    pub fn observe_arm(&mut self, context: &str, arm: &str, value: f64, win: bool, cost: f64) {
        let entry = self.contexts.entry(context.to_owned()).or_insert_with(|| json!({"arms": {}}));
        let arms = entry.as_object_mut().expect("context is an object").entry("arms").or_insert_with(|| json!({}));
        let stats = arms
            .as_object_mut()
            .expect("arms is an object")
            .entry(arm.to_owned())
            .or_insert_with(|| json!({"n": 0, "wins": 0.0, "mean": 0.0, "cost": 0.0, "correction": 0.0}));
        let stats = stats.as_object_mut().expect("arm stats are an object");
        let n = stats.get("n").and_then(Value::as_f64).unwrap_or(0.0) as u64 + 1;
        let running = |previous: f64, value: f64| previous + (value - previous) / n as f64;
        let wins = round4(num(stats.get("wins")) + if win { 1.0 } else { 0.0 });
        let mean = round4(running(num(stats.get("mean")), value.clamp(-1.0, 1.0)));
        let cost = round4(running(num(stats.get("cost")), cost));
        let correction = round4(running(num(stats.get("correction")), 0.0));
        for (key, v) in [("wins", wins), ("mean", mean), ("cost", cost), ("correction", correction)] {
            stats.insert(key.into(), json!(v));
        }
        stats.insert("n".into(), json!(n));
    }

    /// Atomic write (tmp + rename), directory 0700. A table without a path (offline training) is not written.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut root = Map::new();
        root.insert("version".into(), json!(VERSION));
        root.insert("seen".into(), self.seen.clone());
        root.insert("contexts".into(), Value::Object(self.contexts.iter().map(|(k, v)| (k.clone(), v.clone())).collect()));
        let text = serde_json::to_string_pretty(&Value::Object(root)).expect("table serializes");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_means_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        let mut t = Bandit::open(&path);
        t.observe_arm("c", "a", 1.0, true, 0.5);
        t.observe_arm("c", "a", 0.0, false, 0.1);
        assert_eq!(t.stat("c", "a"), Stat { n: 2, wins: 1.0, mean: 0.5, cost: 0.3 });
        assert_eq!(t.stat("c", "missing"), Stat::default());
        t.save().unwrap();
        assert_eq!(Bandit::open(&path).stat("c", "a").n, 2);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        std::fs::write(&path, r#"{"version":1,"seen":["x"],"contexts":{"c":{"arms":{"a":{"n":1,"wins":1.0,"mean":1.0,"cost":0.0,"correction":0.0,"extra":7}},"alpha":2}}}"#).unwrap();
        let mut t = Bandit::open(&path);
        t.observe_arm("c", "a", 1.0, true, 0.0);
        t.save().unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["contexts"]["c"]["arms"]["a"]["extra"], 7);
        assert_eq!(back["contexts"]["c"]["alpha"], 2);
        assert_eq!(back["seen"][0], "x");
    }

    #[test]
    fn wrong_version_starts_empty_and_memory_tables_do_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        std::fs::write(&path, r#"{"version":9,"contexts":{"c":{"arms":{"a":{"n":5}}}}}"#).unwrap();
        assert_eq!(Bandit::open(&path).stat("c", "a").n, 0);
        Bandit::in_memory().save().unwrap();
    }
}
