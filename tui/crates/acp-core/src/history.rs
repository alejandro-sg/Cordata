//! Chat sessions kept on disk so they can be resumed: `~/.cordata/sessions/<project>/<id>.json`, one file per session,
//! rewritten after every turn.
//!
//! What protects them: the folder is created `0700` and each file `0600`; a command the agent runs cannot read them
//! (the sandbox denies `~/.cordata`); credential-shaped text (keys, tokens, private keys, passwords) is masked before
//! anything is written, so a key pasted into a prompt or printed by a command never lands on disk; a project keeps its
//! newest [`KEEP`] sessions. `chat.history: "off"` keeps nothing, `/forget` (or [`forget`]) deletes a project's sessions.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::native::Msg;

/// Sessions kept per project; older ones are deleted when a new one is saved.
pub const KEEP: usize = 50;
const VERSION: u32 = 1;

/// A saved session, as `/history` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    /// Seconds since the Unix epoch.
    pub updated: u64,
    /// The first prompt, shortened.
    pub title: String,
    pub prompts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Saved {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) cwd: String,
    pub(crate) model: String,
    pub(crate) updated: u64,
    pub(crate) messages: Vec<Msg>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A new session id: the start time and a few bits to keep two sessions started in the same second apart.
pub(crate) fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mix = (u64::from(nanos) ^ u64::from(std::process::id()).wrapping_mul(2_654_435_761)) & 0xff_ffff;
    format!("{:x}-{mix:06x}", now())
}

/// FNV-1a of the project's resolved path: a stable folder name that does not spell the path out.
fn project_key(cwd: &Path) -> String {
    let path = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let hash = path.to_string_lossy().bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}

fn folder_in(home: &Path, cwd: &Path) -> PathBuf {
    home.join("sessions").join(project_key(cwd))
}

fn folder(cwd: &Path) -> Option<PathBuf> {
    crate::policy::cordata_home().map(|home| folder_in(&home, cwd))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Every save masks the whole conversation again; the earlier turns are unchanged, so their masked form is kept and
/// only new text meets the regexes. Keyed by a per-process SipHash and the length — the cache holds masked text only,
/// never the raw. Bounded; dropped whole when full.
fn mask(text: &str) -> String {
    use std::collections::HashMap;
    use std::hash::{BuildHasher as _, RandomState};
    use std::sync::{LazyLock, Mutex};
    const MIN: usize = 256;
    const BYTES: usize = 32 << 20;
    type Cache = (RandomState, HashMap<(u64, usize), String>, usize);
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);
    if text.len() < MIN {
        return cordata_guard::redact::scrub(text).0;
    }
    let Ok(mut cache) = CACHE.lock() else { return cordata_guard::redact::scrub(text).0 };
    let key = (cache.0.hash_one(text), text.len());
    if let Some(hit) = cache.1.get(&key) {
        return hit.clone();
    }
    let out = cordata_guard::redact::scrub(text).0;
    if cache.2 + out.len() > BYTES {
        cache.1.clear();
        cache.2 = 0;
    }
    cache.2 += out.len();
    cache.1.insert(key, out.clone());
    out
}

/// Masks every string inside `value`: the JSON text itself would hide `KEY=\"…\"` behind escapes.
fn mask_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(mask(text)),
        Value::Array(items) => Value::Array(items.iter().map(mask_value).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), mask_value(v))).collect()),
        other => other.clone(),
    }
}

/// The same conversation with every credential-shaped span masked, call arguments included.
fn masked(messages: &[Msg]) -> Vec<Msg> {
    messages
        .iter()
        .map(|m| match m {
            Msg::User { text } => Msg::User { text: mask(text) },
            Msg::Assistant { text, calls } => Msg::Assistant {
                text: mask(text),
                calls: calls
                    .iter()
                    .map(|c| {
                        let mut c = c.clone();
                        c.args = mask_value(&c.args);
                        c
                    })
                    .collect(),
            },
            Msg::ToolResult { id, output } => Msg::ToolResult { id: id.clone(), output: mask(output) },
        })
        .collect()
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes the session (masked) through a temporary file, so a crash never leaves half a file; then prunes.
pub(crate) fn save_in(home: &Path, cwd: &Path, id: &str, model: &str, messages: &[Msg]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if !valid_id(id) {
        return Err(std::io::Error::other("invalid session id"));
    }
    let dir = folder_in(home, cwd);
    private_dir(&dir)?;
    let saved = Saved { version: VERSION, id: id.to_owned(), cwd: cwd.display().to_string(), model: model.to_owned(), updated: now(), messages: masked(messages) };
    let body = serde_json::to_vec(&saved).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".{id}.tmp"));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(&body)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dir.join(format!("{id}.json")))?;
    prune(&dir);
    Ok(())
}

/// Keeps the newest [`KEEP`] files by modification time; nothing is parsed, so a save stays cheap.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH), e.path()))
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, path) in files.into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

fn read(path: &Path) -> Option<Saved> {
    let saved: Saved = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (saved.version == VERSION && valid_id(&saved.id)).then_some(saved)
}

fn title(messages: &[Msg]) -> String {
    let first = messages.iter().find_map(|m| if let Msg::User { text } = m { Some(text.as_str()) } else { None }).unwrap_or("");
    let line = first.lines().next().unwrap_or("").trim();
    if line.chars().count() > 72 { format!("{}…", line.chars().take(71).collect::<String>()) } else { line.to_owned() }
}

fn summaries(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Summary> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read(&e.path()))
        .map(|s| Summary {
            prompts: s.messages.iter().filter(|m| matches!(m, Msg::User { .. })).count(),
            title: title(&s.messages),
            id: s.id,
            updated: s.updated,
        })
        .collect();
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Whether `id` names a saved session of this project: a shape check, then one `stat` (nothing is read or parsed).
pub fn exists(cwd: &Path, id: &str) -> bool {
    crate::policy::cordata_home().is_some_and(|home| exists_in(&home, cwd, id))
}

pub(crate) fn exists_in(home: &Path, cwd: &Path, id: &str) -> bool {
    valid_id(id) && folder_in(home, cwd).join(format!("{id}.json")).is_file()
}

/// This project's saved sessions, newest first.
pub fn list(cwd: &Path) -> Vec<Summary> {
    folder(cwd).map(|dir| summaries(&dir)).unwrap_or_default()
}

/// [`list`] under another Cordata home (`CORDATA_HOME`), for front ends that carry their own.
pub fn list_in(home: &Path, cwd: &Path) -> Vec<Summary> {
    summaries(&folder_in(home, cwd))
}

/// `last`, a full id or a unique prefix of one.
pub(crate) fn load_in(home: &Path, cwd: &Path, wanted: &str) -> Result<Saved, String> {
    let dir = folder_in(home, cwd);
    let all = summaries(&dir);
    let wanted = wanted.trim();
    let found: Vec<&Summary> = if wanted.is_empty() || wanted == "last" {
        all.first().into_iter().collect()
    } else {
        all.iter().filter(|s| s.id.starts_with(wanted)).collect()
    };
    match found.as_slice() {
        [] if all.is_empty() => Err("no saved session in this project".into()),
        [] => Err(format!("no saved session matches '{wanted}' (see /history)")),
        [one] => read(&dir.join(format!("{}.json", one.id))).ok_or_else(|| format!("session {} cannot be read", one.id)),
        many => Err(format!("'{wanted}' matches {} sessions; type more of the id", many.len())),
    }
}

/// Deletes every saved session of this project; how many there were.
pub(crate) fn forget_in(home: &Path, cwd: &Path) -> std::io::Result<usize> {
    let dir = folder_in(home, cwd);
    let count = summaries(&dir).len();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(count),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Deletes every saved session of the project at `cwd`. A running session keeps writing unless it was told to
/// forget too (`SessionHandle::forget`), which is what `/forget` does.
pub fn forget(cwd: &Path) -> std::io::Result<usize> {
    let home = crate::policy::cordata_home().ok_or_else(|| std::io::Error::other("no home folder"))?;
    forget_in(&home, cwd)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;
    use crate::native::Call;

    fn user(t: &str) -> Msg {
        Msg::User { text: t.into() }
    }

    #[test]
    fn a_session_round_trips_privately_with_credentials_masked() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let key = format!("sk-ant-api03-{}", "Q7zR9kMp3XwB".repeat(4));
        let messages = vec![
            user(&format!("use {key} for the call")),
            Msg::Assistant {
                text: "ok".into(),
                calls: vec![Call { id: "c1".into(), name: "run_command".into(), args: json!({"cmd": format!("curl -H 'x-api-key: {key}'"), "env": ["API_TOKEN=\"q7Zr9KmP3xW5vB8nL2cT\""]}), thought_signatures: None }],
            },
            Msg::ToolResult { id: "c1".into(), output: format!("token = {key}") },
        ];
        save_in(home.path(), cwd.path(), "abc-000001", "m", &messages).unwrap();
        let dir = folder_in(home.path(), cwd.path());
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(dir.join("abc-000001.json")).unwrap().permissions().mode() & 0o777, 0o600);
        let raw = std::fs::read_to_string(dir.join("abc-000001.json")).unwrap();
        assert!(!raw.contains(&key), "{raw}");
        assert!(!raw.contains("q7Zr9KmP3xW5vB8nL2cT"), "a quoted value inside a call argument is masked too: {raw}");
        let back = load_in(home.path(), cwd.path(), "last").unwrap();
        assert_eq!(back.messages.len(), 3);
        assert!(matches!(&back.messages[0], Msg::User { text } if text.contains("[REDACTED]")));
    }

    #[test]
    fn sessions_are_listed_newest_first_found_by_prefix_and_pruned() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for i in 0..KEEP + 3 {
            save_in(home.path(), cwd.path(), &format!("a{i:03x}-000000"), "m", &[user(&format!("prompt {i}"))]).unwrap();
        }
        let all = summaries(&folder_in(home.path(), cwd.path()));
        assert_eq!(all.len(), KEEP);
        assert_eq!(load_in(home.path(), cwd.path(), "a005").unwrap().id, "a005-000000");
        assert!(load_in(home.path(), cwd.path(), "a0").unwrap_err().contains("matches"));
        assert!(load_in(home.path(), cwd.path(), "ffff").unwrap_err().contains("no saved session matches"));
        assert_eq!(all[0].prompts, 1);
    }

    #[test]
    fn forgetting_removes_only_this_projects_sessions() {
        let home = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        save_in(home.path(), a.path(), "a-000001", "m", &[user("x")]).unwrap();
        save_in(home.path(), a.path(), "a-000002", "m", &[user("y")]).unwrap();
        save_in(home.path(), b.path(), "b-000001", "m", &[user("z")]).unwrap();
        assert_eq!(forget_in(home.path(), a.path()).unwrap(), 2);
        assert!(load_in(home.path(), a.path(), "last").is_err());
        assert!(load_in(home.path(), b.path(), "last").is_ok());
        assert_eq!(forget_in(home.path(), a.path()).unwrap(), 0);
    }

    #[test]
    fn exists_is_one_stat_for_this_project_and_a_well_formed_id() {
        let home = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        save_in(home.path(), a.path(), "a-000001", "m", &[user("x")]).unwrap();
        assert!(exists_in(home.path(), a.path(), "a-000001"));
        assert!(!exists_in(home.path(), b.path(), "a-000001"), "another project's folder");
        assert!(!exists_in(home.path(), a.path(), "a-000002"));
        assert!(!exists_in(home.path(), a.path(), "../a-000001"), "a path is not an id");
        forget_in(home.path(), a.path()).unwrap();
        assert!(!exists_in(home.path(), a.path(), "a-000001"));
    }

    #[test]
    fn ids_cannot_name_a_path() {
        let home = tempfile::tempdir().unwrap();
        assert!(save_in(home.path(), Path::new("/p"), "../../etc/x", "m", &[]).is_err());
        assert!(new_id().chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }
}

/// `cargo test --release -p acp-core bench_ -- --ignored --nocapture` (scripts/bench.sh).
#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore]
    fn bench_save_and_list() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let body = "let token_count = request.auth_header.len(); // key lookup\n".repeat(40);
        let mut messages = Vec::new();
        let mut per_turn = Vec::new();
        for turn in 0..100 {
            messages.push(Msg::User { text: format!("prompt {turn}\n{body}") });
            messages.push(Msg::ToolResult { id: format!("c{turn}"), output: body.clone() });
            let start = std::time::Instant::now();
            save_in(home.path(), cwd.path(), "abc-000001", "m", &messages).unwrap();
            per_turn.push(start.elapsed().as_secs_f64() * 1e3);
        }
        for i in 2..=50 {
            save_in(home.path(), cwd.path(), &format!("abc-{i:06}"), "m", &messages[..20]).unwrap();
        }
        let start = std::time::Instant::now();
        let listed = list_in(home.path(), cwd.path());
        let list_ms = start.elapsed().as_secs_f64() * 1e3;
        println!(
            "BENCH history save: turn 1 {:.2}ms, turn 100 ({} msgs, {}KB) {:.2}ms | list {} sessions {:.2}ms",
            per_turn[0],
            messages.len(),
            messages.len() * body.len() / 1024,
            per_turn[99],
            listed.len(),
            list_ms
        );
    }
}
