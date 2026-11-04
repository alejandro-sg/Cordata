//! Which threads the window had open, so a relaunch puts them back: `threads.toml` beside `prefs.toml`.
//!
//! It holds ids and folders only — never conversation text. The conversations themselves are the engine's session files
//! (`acp_core::history`, `~/.cordata/sessions`); this file is the window's list of which of them were open, in order,
//! and which one was shown. Losing it costs nothing: every conversation is still under **Earlier**.
//!
//! Treated as untrusted on the way in (a stray or hand-edited file must not make the app do more than open a
//! conversation it already has): read at most [`MAX_BYTES`], at most [`CAP`] threads, each folder an existing absolute
//! directory, each id well-formed and naming a session file that exists for that folder, none twice.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Threads kept in the list. Each restored thread is an engine task and a transcript read, so the count is bounded.
pub const CAP: usize = 12;
/// A list of [`CAP`] threads is under 3 KB; anything near this is not ours.
const MAX_BYTES: u64 = 64 * 1024;
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    /// The engine's session id (the file name under the project's sessions folder).
    pub id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    version: u32,
    /// The id of the thread that was shown.
    pub active: Option<String>,
    /// In the order the threads were opened.
    pub open: Vec<Open>,
}

impl Default for Layout {
    fn default() -> Self {
        Self { version: VERSION, active: None, open: Vec::new() }
    }
}

impl Layout {
    pub fn new(active: Option<String>, mut open: Vec<Open>) -> Self {
        open.truncate(CAP);
        Self { version: VERSION, active, open }
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// The thread to show: the saved active one, else the first.
    pub fn active_index(&self) -> usize {
        self.active.as_ref().and_then(|id| self.open.iter().position(|o| &o.id == id)).unwrap_or(0)
    }

    pub fn path() -> Option<PathBuf> {
        crate::prefs::dir().map(|d| d.join("threads.toml"))
    }

    /// What the last run left, checked against the sessions on disk. Nothing, if there is no file or it is unusable.
    pub fn load() -> Self {
        Self::path().map(|p| Self::load_from(&p, acp_core::history::exists)).unwrap_or_default()
    }

    pub fn load_from(path: &Path, exists: impl Fn(&Path, &str) -> bool) -> Self {
        let mut text = String::new();
        let read = std::fs::File::open(path).and_then(|f| f.take(MAX_BYTES).read_to_string(&mut text));
        if read.is_err() {
            return Self::default();
        }
        // Derived state: a file that does not parse is dropped, not kept — the conversations are still on disk.
        let Ok(raw) = toml::from_str::<Layout>(&text) else { return Self::default() };
        if raw.version != VERSION {
            return Self::default();
        }
        let mut seen = std::collections::HashSet::new();
        let open: Vec<Open> = raw
            .open
            .into_iter()
            .filter(|o| o.cwd.is_absolute() && o.cwd.is_dir() && exists(&o.cwd, &o.id) && seen.insert(o.id.clone()))
            .take(CAP)
            .collect();
        Self { version: VERSION, active: raw.active, open }
    }

    pub fn save(&self) {
        if let Some(p) = Self::path()
            && let Err(error) = self.save_to(&p)
        {
            eprintln!("cordata: could not save {} ({error})", p.display());
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        crate::prefs::write_private(path, toml::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cordata-threads-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(std::fs::canonicalize(dir).unwrap())
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open(id: &str, cwd: &Path) -> Open {
        Open { id: id.into(), cwd: cwd.to_path_buf() }
    }

    fn always(_: &Path, _: &str) -> bool {
        true
    }

    #[test]
    fn a_layout_round_trips_in_order_with_its_active_thread() {
        let dir = Dir::new("round");
        let file = dir.0.join("threads.toml");
        let layout = Layout::new(Some("b-2".into()), vec![open("a-1", &dir.0), open("b-2", &dir.0), open("c-3", &dir.0)]);
        layout.save_to(&file).unwrap();
        let back = Layout::load_from(&file, always);
        assert_eq!(back, layout);
        assert_eq!(back.active_index(), 1);
    }

    #[test]
    fn the_file_is_private_and_holds_no_conversation_text() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("private");
        let file = dir.0.join("threads.toml");
        Layout::new(None, vec![open("a-1", &dir.0)]).save_to(&file).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.lines().all(|l| l.starts_with("version") || l.starts_with("[[") || l.starts_with("id") || l.starts_with("cwd") || l.is_empty()), "ids and folders only: {text}");
    }

    #[test]
    fn what_cannot_be_opened_is_dropped_on_load() {
        let dir = Dir::new("drop");
        let file = dir.0.join("threads.toml");
        let gone = dir.0.join("deleted-folder");
        let layout = Layout::new(
            Some("keep-1".into()),
            vec![
                open("keep-1", &dir.0),
                open("forgotten-2", &dir.0), // its session file is gone
                open("keep-1", &dir.0),      // twice
                open("elsewhere-3", &gone),  // its folder is gone
                open("rel-4", Path::new("relative/dir")),
            ],
        );
        layout.save_to(&file).unwrap();
        let back = Layout::load_from(&file, |_, id| id.starts_with("keep"));
        assert_eq!(back.open, vec![open("keep-1", &dir.0)]);
    }

    #[test]
    fn a_missing_active_thread_falls_back_to_the_first() {
        let dir = Dir::new("active");
        let file = dir.0.join("threads.toml");
        Layout::new(Some("closed-9".into()), vec![open("a-1", &dir.0), open("b-2", &dir.0)]).save_to(&file).unwrap();
        assert_eq!(Layout::load_from(&file, always).active_index(), 0);
    }

    #[test]
    fn a_damaged_wrong_version_or_oversized_file_means_a_fresh_start() {
        let dir = Dir::new("bad");
        let file = dir.0.join("threads.toml");
        for text in ["not toml {{{", "version = 99\n[[open]]\nid = \"a-1\"\ncwd = \"/tmp\"\n", &format!("# {}\nversion = 1\n", "x".repeat(70 * 1024))] {
            std::fs::write(&file, text).unwrap();
            assert!(Layout::load_from(&file, always).is_empty(), "{}", &text[..text.len().min(20)]);
        }
        assert!(Layout::load_from(&dir.0.join("absent.toml"), always).is_empty());
    }

    #[test]
    fn no_more_than_the_cap_comes_back() {
        let dir = Dir::new("cap");
        let file = dir.0.join("threads.toml");
        let many: Vec<Open> = (0..CAP + 8).map(|i| open(&format!("{i:x}-aa"), &dir.0)).collect();
        // Written by hand: `Layout::new` already truncates, and a hand-edited file would not.
        let text: String = std::iter::once("version = 1\n".to_owned()).chain(many.iter().map(|o| format!("[[open]]\nid = \"{}\"\ncwd = \"{}\"\n", o.id, o.cwd.display()))).collect();
        std::fs::write(&file, text).unwrap();
        assert_eq!(Layout::load_from(&file, always).open.len(), CAP);
    }

    #[test]
    fn a_session_id_that_climbs_out_of_the_folder_is_not_a_session() {
        // `history::exists` is the real check `load` uses: shape first, so a path never reaches the file system.
        let dir = Dir::new("climb");
        for id in ["../../etc/passwd", "a/b", "", "zz", &"a".repeat(65)] {
            assert!(!acp_core::history::exists(&dir.0, id), "{id:?}");
        }
    }
}
