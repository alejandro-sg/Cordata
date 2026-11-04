//! Which threads the window had open, so a relaunch puts them back: `threads.toml` beside `prefs.toml`.
//!
//! It holds ids and folders only — never conversation text. The conversations themselves are the engine's session files
//! (`acp_core::history`, `~/.cordata/sessions`); this file is the window's list of which of them were open, in order,
//! and which one was shown. Losing it costs nothing: every conversation is still under **Earlier**.
//!
//! Treated as untrusted on the way in (a stray or hand-edited file must not make the app do more than open a
//! conversation it already has): read at most [`MAX_BYTES`], at most [`CAP`] threads, each folder an existing absolute
//! directory, each id well-formed and naming a session file that exists for that folder, none twice.
//!
//! One window owns the list. The owner holds an exclusive lock on `threads.lock` for as long as it lives; a second copy
//! of the app (or a dev build beside the installed one) cannot take it, so it starts clean and never writes the list —
//! two windows resuming the same conversations would otherwise both append to the same files, and the last writer would
//! decide what comes back. The lock belongs to the open file: it goes when the process does, a crash leaves nothing
//! stale, and the other copy takes over the next time its own list changes.

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

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        crate::prefs::write_private(path, toml::to_string_pretty(self).map_err(std::io::Error::other)?.as_bytes())
    }
}

/// Where the list lives and who may write it (see the module docs). Nothing touches the disk until it is asked to.
#[derive(Default)]
pub struct Store {
    /// `None`: a store that keeps the list in memory only (tests, and the fallback when there is no home folder).
    dir: Option<PathBuf>,
    owner: Option<std::fs::File>,
    /// The list as this window last had it, and whether the file still has to catch up.
    last: Layout,
    dirty: bool,
}

impl Store {
    pub fn open(dir: Option<PathBuf>) -> Self {
        Self { dir, ..Self::default() }
    }

    fn file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("threads.toml"))
    }

    /// Whether this window may read and write the list; takes the lock if it is free.
    pub fn owns(&mut self) -> bool {
        let Some(dir) = &self.dir else { return true };
        if self.owner.is_some() {
            return true;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let locked = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(dir.join("threads.lock")))
            .ok()
            .filter(|file| file.try_lock().is_ok());
        self.owner = locked;
        self.owner.is_some()
    }

    /// What the last run left, if this window is the one that owns the list; nothing otherwise.
    pub fn load(&mut self) -> Layout {
        match self.file() {
            Some(file) if self.owns() => Layout::load_from(&file, acp_core::history::exists),
            _ => Layout::default(),
        }
    }

    /// The list as it stands now. Written when it changed, if this window owns the list; kept until it does.
    pub fn update(&mut self, layout: Layout) {
        if layout != self.last {
            self.last = layout;
            self.dirty = true;
        }
        if self.dirty && self.owns() {
            self.dirty = false;
            if let Some(file) = self.file()
                && let Err(error) = self.last.save_to(&file)
            {
                eprintln!("cordata: could not save {} ({error})", file.display());
            }
        }
    }

    /// Forget the list (the switch was turned off): the file goes, if this window owns it.
    pub fn clear(&mut self) {
        self.last = Layout::default();
        self.dirty = false;
        if let Some(file) = self.file()
            && self.owns()
        {
            let _ = std::fs::remove_file(file);
        }
    }

    #[cfg(test)]
    pub fn last(&self) -> &Layout {
        &self.last
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

    #[test]
    fn only_one_window_owns_the_list_and_the_other_takes_over_when_it_ends() {
        let dir = Dir::new("owner");
        let list = |ids: &[&str]| Layout::new(None, ids.iter().map(|id| open(id, &dir.0)).collect());
        let (mut first, mut second) = (Store::open(Some(dir.0.clone())), Store::open(Some(dir.0.clone())));
        first.update(list(&["a-1"]));
        assert!(first.owns() && !second.owns());
        // The second window neither restores the first's threads nor overwrites its list.
        assert!(second.load().is_empty());
        second.update(list(&["z-9"]));
        assert_eq!(Layout::load_from(&dir.0.join("threads.toml"), always).open, vec![open("a-1", &dir.0)]);
        // Once the first is gone, the second's next change takes the list over, with its current state.
        drop(first);
        second.update(list(&["z-9", "y-8"]));
        assert!(second.owns());
        assert_eq!(Layout::load_from(&dir.0.join("threads.toml"), always).open.len(), 2);
    }

    #[test]
    fn a_detached_store_remembers_in_memory_and_writes_nothing() {
        let mut store = Store::default();
        store.update(Layout::new(Some("a-1".into()), vec![open("a-1", Path::new("/tmp"))]));
        assert_eq!(store.last().open.len(), 1);
        assert!(store.owns());
    }

    #[test]
    fn clearing_removes_the_file_and_what_was_remembered() {
        let dir = Dir::new("clear");
        let mut store = Store::open(Some(dir.0.clone()));
        store.update(Layout::new(None, vec![open("a-1", &dir.0)]));
        assert!(dir.0.join("threads.toml").is_file());
        store.clear();
        assert!(!dir.0.join("threads.toml").exists() && store.last().is_empty());
    }

    #[test]
    fn the_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("lockmode");
        let mut store = Store::open(Some(dir.0.clone()));
        assert!(store.owns());
        assert_eq!(std::fs::metadata(dir.0.join("threads.lock")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
