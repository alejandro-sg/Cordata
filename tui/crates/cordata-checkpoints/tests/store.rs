//! The shadow-git store against real git in temporary projects: snapshots, restores (whole and scoped), diffs, turns,
//! undo, size limits, secrets, history pruning and the refusals.

use std::path::{Path, PathBuf};

use cordata_checkpoints::{Change, Restore, RestoreOptions, Store, TakeOptions};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    store: Store,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let home = dir.path().join("home");
    Env { store: Store::new(&home), home, project, _dir: dir }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

fn opts(turn: &str, kind: &str) -> TakeOptions {
    TakeOptions { kind: kind.into(), tool: "edit".into(), agent: "executor".into(), session_id: "s1".into(), turn: turn.into(), call: format!("step {turn}"), ..Default::default() }
}

fn restore(e: &Env, id: &str) -> Restore {
    e.store.restore(&e.project, id, &RestoreOptions::default())
}

#[test]
fn a_restore_puts_everything_back_and_never_touches_secrets() {
    let e = env();
    write(&e.project, "a.txt", "one\n");
    write(&e.project, "sub/b.txt", "bee\n");
    write(&e.project, ".env", "KEY=1\n");
    write(&e.project, "node_modules/x/index.js", "dep\n");
    let snap = e.store.take(&e.project, &opts("t1", "pre"));
    assert!(snap.ok, "{:?}", snap.reason);
    let id = snap.id.unwrap();

    write(&e.project, "a.txt", "changed\n");
    std::fs::remove_dir_all(e.project.join("sub")).unwrap();
    write(&e.project, "new/deep/c.txt", "created\n");
    write(&e.project, ".env", "KEY=2\n");

    let dry = e.store.restore(&e.project, &id, &RestoreOptions { dry_run: true, ..Default::default() });
    assert!(dry.ok && dry.dry_run);
    let actions: Vec<(String, &str)> = dry.files.iter().map(|f| (f.path.clone(), f.action)).collect();
    assert_eq!(actions, [("a.txt".into(), "revert"), ("new/deep/c.txt".into(), "delete"), ("sub/b.txt".into(), "recreate")]);
    assert_eq!(read(&e.project, "a.txt").unwrap(), "changed\n", "a dry run changes nothing");

    let done = restore(&e, &id);
    assert!(done.ok, "{:?}", done.failed);
    assert_eq!(read(&e.project, "a.txt").unwrap(), "one\n");
    assert_eq!(read(&e.project, "sub/b.txt").unwrap(), "bee\n");
    assert_eq!(read(&e.project, "new/deep/c.txt"), None);
    assert!(!e.project.join("new").exists(), "directories emptied by the restore are pruned");
    assert_eq!(read(&e.project, ".env").unwrap(), "KEY=2\n", "secrets are neither captured nor restored");
    assert!(e.project.join("node_modules/x/index.js").exists());

    // The restore is itself undoable: undo_with is the checkpoint of the state just before it.
    let undo_with = done.undo_with.expect("the present was snapshotted first");
    assert!(restore(&e, &undo_with).ok);
    assert_eq!(read(&e.project, "a.txt").unwrap(), "changed\n");
    assert_eq!(read(&e.project, "new/deep/c.txt").unwrap(), "created\n");
}

#[test]
fn a_restore_with_nothing_to_do_reports_zero_and_writes_no_commit() {
    let e = env();
    write(&e.project, "a.txt", "x\n");
    let id = e.store.take(&e.project, &opts("t", "pre")).id.unwrap();
    let before = e.store.list(&e.project, 50).len();
    let r = restore(&e, &id);
    assert!((r.ok, r.count, r.undo_with.is_none()) == (true, 0, true));
    assert_eq!(e.store.list(&e.project, 50).len(), before);
}

#[test]
fn scoped_restores_only_touch_the_named_paths() {
    let e = env();
    write(&e.project, "a.txt", "a0\n");
    write(&e.project, "b.txt", "b0\n");
    let id = e.store.take(&e.project, &opts("t", "pre")).id.unwrap();
    write(&e.project, "a.txt", "a1\n");
    write(&e.project, "b.txt", "b1\n");
    let r = e.store.restore(&e.project, &id, &RestoreOptions { paths: Some(vec!["a.txt".into()]), ..Default::default() });
    assert!(r.ok && r.count == 1, "{r:?}");
    assert_eq!((read(&e.project, "a.txt").unwrap(), read(&e.project, "b.txt").unwrap()), ("a0\n".into(), "b1\n".into()));
    let bad = e.store.restore(&e.project, &id, &RestoreOptions { paths: Some(vec!["../x".into()]), ..Default::default() });
    assert_eq!(bad.reason.as_deref(), Some("'../x' is outside the project"));
}

#[test]
fn awkward_file_names_are_literal_not_globs() {
    let e = env();
    write(&e.project, "[a].txt", "bracket\n");
    write(&e.project, "star*.txt", "star\n");
    write(&e.project, "sp ace/é.txt", "uni\n");
    let id = e.store.take(&e.project, &opts("t", "pre")).id.unwrap();
    for f in ["[a].txt", "star*.txt", "sp ace/é.txt"] {
        std::fs::remove_file(e.project.join(f)).unwrap();
    }
    assert!(restore(&e, &id).ok);
    assert_eq!(read(&e.project, "[a].txt").unwrap(), "bracket\n");
    assert_eq!(read(&e.project, "star*.txt").unwrap(), "star\n");
    assert_eq!(read(&e.project, "sp ace/é.txt").unwrap(), "uni\n");
}

#[test]
fn new_files_over_the_size_limit_are_left_out_and_remembered() {
    let e = env();
    write(&e.project, "small.txt", "s\n");
    write(&e.project, "big.bin", &"x".repeat(5_000));
    let small_limit = TakeOptions { max_file_mb: Some(0.001), ..opts("t", "pre") }; // ~1 KB
    let id = e.store.take(&e.project, &small_limit).id.unwrap();
    write(&e.project, "big.bin", &"y".repeat(5_000));
    write(&e.project, "small.txt", "s2\n");
    let r = e.store.restore(&e.project, &id, &RestoreOptions { max_file_mb: Some(0.001), ..Default::default() });
    assert!(r.ok, "{r:?}");
    assert_eq!(read(&e.project, "small.txt").unwrap(), "s\n");
    assert_eq!(read(&e.project, "big.bin").unwrap(), "y".repeat(5_000), "a file over the limit is not covered, so not touched");
    let remembered = std::fs::read_to_string(e.store.shadow_dir(&e.project).join("git/big-files")).unwrap();
    assert_eq!(remembered.trim(), "/big.bin");
}

#[test]
fn diffs_and_changes_since_a_checkpoint() {
    let e = env();
    write(&e.project, "a.txt", "one\ntwo\n");
    write(&e.project, "gone.txt", "bye\n");
    write(&e.project, ".env", "S=1\n");
    let id = e.store.take(&e.project, &opts("t", "pre")).id.unwrap();
    write(&e.project, "a.txt", "one\nTWO\n");
    write(&e.project, "added.txt", "new\n");
    write(&e.project, ".env", "S=2\n");
    std::fs::remove_file(e.project.join("gone.txt")).unwrap();

    let mut changes = e.store.changes_since(&e.project, &id, None).unwrap();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let got: Vec<(&str, Change)> = changes.iter().map(|c| (c.path.as_str(), c.change)).collect();
    assert_eq!(got, [("a.txt", Change::Modified), ("added.txt", Change::Added), ("gone.txt", Change::Deleted)]);

    let (patch, stat) = e.store.diff_since(&e.project, &id, None);
    assert!(patch.contains("-two\n+TWO\n") && patch.contains("+++ b/added.txt") && !patch.contains("S=2"), "secrets never appear in a diff: {patch}");
    assert_eq!(stat, "3 files changed, 2 insertions(+), 2 deletions(-)");
    let (patch, _) = e.store.diff_since(&e.project, &id, Some(&["a.txt".to_owned()]));
    assert!(patch.contains("a.txt") && !patch.contains("added.txt"));
    assert_eq!(e.store.changes_since(&e.project, "deadbeef", None), None, "an unknown checkpoint is unknown, not 'no changes'");
    assert_eq!(e.store.diff_since(&e.project, "zz", None), (String::new(), String::new()));
}

#[test]
fn turns_group_calls_and_undo_goes_back_to_before_the_first_change() {
    let e = env();
    write(&e.project, "a.txt", "v0\n");
    let first = e.store.take(&e.project, &opts("turn-1", "pre")).id.unwrap();
    write(&e.project, "a.txt", "v1\n");
    let _second = e.store.take(&e.project, &TakeOptions { tool: "shell".into(), ..opts("turn-1", "pre") }).id.unwrap();
    write(&e.project, "b.txt", "new\n");
    let end = e.store.take(&e.project, &opts("turn-1", "end")).id.unwrap();

    let turns = e.store.turns(&e.project, 8);
    assert_eq!(turns.len(), 1);
    let t = &turns[0];
    assert_eq!((t.turn.as_str(), t.first.as_deref(), t.end.as_deref(), t.calls, t.undone), ("turn-1", Some(first.as_str()), Some(end.as_str()), 2, false));
    assert_eq!(t.tools, ["shell", "edit"], "newest first");
    assert_eq!(t.file_count, Some(2));
    assert_eq!(t.last_call.as_deref(), Some("step turn-1"));

    write(&e.project, "a.txt", "someone edited after the turn\n");
    let dry = e.store.undo_turn(&e.project, None, true, None, "s1");
    assert_eq!((dry.ok, dry.turn.as_deref(), dry.calls), (true, Some("turn-1"), Some(2)));
    assert_eq!(dry.edited_since, ["a.txt"], "the caller can warn before overwriting later edits");
    let done = e.store.undo_turn(&e.project, None, false, None, "s1");
    assert!(done.ok, "{done:?}");
    assert_eq!((read(&e.project, "a.txt").unwrap(), read(&e.project, "b.txt")), ("v0\n".into(), None));
    assert!(e.store.turns(&e.project, 8)[0].undone);
    assert_eq!(e.store.undo_turn(&e.project, None, false, None, "s1").reason.as_deref(), Some("no turn to undo"));
    assert_eq!(e.store.undo_turn(&e.project, Some("nope"), false, None, "s1").reason.as_deref(), Some("no checkpoints for turn nope"));
}

#[test]
fn refusals_say_why() {
    let e = env();
    let missing = e.store.take(Path::new("/no/such/project"), &opts("t", "pre"));
    assert_eq!((missing.ok, missing.reason.as_deref()), (false, Some("the project directory does not exist")));
    let system = e.store.take(Path::new("/tmp"), &opts("t", "pre"));
    assert_eq!(system.reason.as_deref(), Some("refusing to checkpoint your home directory or a system directory"));
    assert!(!e.home.exists() || !e.home.join("checkpoints").exists(), "nothing was created for a refused project");

    write(&e.project, "a.txt", "x\n");
    let outside = e.store.take(&e.project, &TakeOptions { paths: vec!["a.txt".into(), "/etc/hosts".into(), "../x".into()], ..opts("t", "pre") });
    assert!(outside.ok);
    assert_eq!(outside.outside, ["/etc/hosts", "../x"], "a call that writes outside the project is reported");
    assert_eq!(restore(&e, "nothex").reason.as_deref(), Some("a checkpoint id is 7-40 hex characters"));
    assert!(restore(&e, "abcdef1234").reason.unwrap().starts_with("no checkpoint abcdef1234"));
    assert!(restore(&Env { project: PathBuf::from("/no/such"), ..env() }, "abcdef1").reason.is_some());
}

#[test]
fn history_is_bounded_without_changing_what_the_newest_snapshots_hold() {
    let e = env();
    let mut ids = Vec::new();
    for i in 0..9 {
        write(&e.project, "a.txt", &format!("v{i}\n"));
        ids.push(e.store.take(&e.project, &opts(&format!("t{i}"), "pre")).id.unwrap());
    }
    assert_eq!(e.store.list(&e.project, 200).len(), 9);
    assert!(!e.store.prune(&e.project, 9), "not past 2*keep yet");
    assert!(e.store.prune(&e.project, 3), "9 > 2*3");
    let rows = e.store.list(&e.project, 200);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].meta["turn"], "t8");
    // The rewritten chain has the same trees: the newest still restores v8, and an old id is gone.
    write(&e.project, "a.txt", "scribble\n");
    assert!(restore(&e, &rows[0].id).ok);
    assert_eq!(read(&e.project, "a.txt").unwrap(), "v8\n");
    assert!(restore(&e, &ids[0]).reason.unwrap().starts_with("no checkpoint"));
}

#[test]
fn the_shadow_repository_is_private_and_leaves_the_projects_own_git_alone() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    std::fs::create_dir_all(e.project.join(".git")).unwrap();
    write(&e.project, ".git/config", "[core]\n");
    write(&e.project, "a.txt", "x\n");
    assert!(e.store.take(&e.project, &opts("t", "pre")).ok);
    let shadow = e.store.shadow_dir(&e.project);
    assert_eq!(std::fs::metadata(&shadow).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(read(&e.project, ".git/config").unwrap(), "[core]\n");
    assert_eq!(std::fs::read_to_string(shadow.join("project.txt")).unwrap().trim(), std::fs::canonicalize(&e.project).unwrap().to_string_lossy());
    let exclude = std::fs::read_to_string(shadow.join("git/info/exclude")).unwrap();
    assert!(exclude.starts_with("# cordata checkpoint excludes v2") && exclude.contains(".env.*"));
    let tracked = String::from_utf8(std::process::Command::new("git").arg("--git-dir").arg(shadow.join("git")).args(["ls-tree", "-r", "--name-only", "HEAD"]).output().unwrap().stdout).unwrap();
    assert_eq!(tracked.trim(), "a.txt", "no .git internals in the snapshot");
}
