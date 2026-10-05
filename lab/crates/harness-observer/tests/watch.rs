//! The observer against the real file system: real `notify` events, a real debounce, a real git repo.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use harness_observer::{Change, Observer, ObserverConfig, Snippet};

fn start(root: &Path) -> (Observer, Receiver<Vec<Snippet>>) {
    let (tx, rx) = mpsc::channel();
    let mut config = ObserverConfig::new(root);
    config.quiet = Duration::from_millis(120);
    config.max_wait = Duration::from_millis(600);
    let observer = Observer::start(config, move |batch| {
        let _ = tx.send(batch);
    })
    .unwrap();
    // FSEvents needs a moment to be listening before the first write is guaranteed to be seen.
    std::thread::sleep(Duration::from_millis(300));
    (observer, rx)
}

fn batch(rx: &Receiver<Vec<Snippet>>) -> Vec<Snippet> {
    rx.recv_timeout(Duration::from_secs(5)).expect("no snippets arrived within 5s")
}

fn nothing_more(rx: &Receiver<Vec<Snippet>>, wait: Duration) {
    if let Ok(extra) = rx.recv_timeout(wait) {
        panic!("expected silence, got {:?}", extra.iter().map(|s| (&s.path, &s.name)).collect::<Vec<_>>());
    }
}

fn names(batch: &[Snippet]) -> Vec<&str> {
    let mut names: Vec<&str> = batch.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names
}

const V1: &str = "pub fn alpha() -> u32 {\n    1\n}\n\npub fn beta() -> u32 {\n    2\n}\n\npub fn gamma() -> u32 {\n    3\n}\n";

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn a_new_file_reports_every_symbol_as_added() {
    let dir = tmp();
    let (observer, rx) = start(dir.path());
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap();
    let got = batch(&rx);
    assert_eq!(names(&got), ["alpha", "beta", "gamma"]);
    assert!(got.iter().all(|s| s.change == Change::Added && s.path == "lib.rs" && s.language == "rust" && s.node_kind == "function_item"));
    let beta = got.iter().find(|s| s.name == "beta").unwrap();
    assert_eq!((beta.start_line, beta.end_line), (5, 7));
    assert!(beta.text.contains('2') && beta.content_hash.len() == 32);
    observer.stop();
}

#[test]
fn saving_a_rust_file_extracts_only_the_modified_function() {
    let dir = tmp();
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap();
    let (observer, rx) = start(dir.path());
    // Seed the cache with a first save of the unchanged file (no git here: everything is Added once)...
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap();
    let first = batch(&rx);
    assert_eq!(names(&first), ["alpha", "beta", "gamma"]);
    // ...then change one function and save.
    std::fs::write(dir.path().join("lib.rs"), V1.replace("    2\n", "    22\n")).unwrap();
    let second = batch(&rx);
    assert_eq!(names(&second), ["beta"], "only the modified AST node is extracted");
    assert_eq!(second[0].change, Change::Modified);
    assert!(second[0].text.contains("22"));
    nothing_more(&rx, Duration::from_millis(500));
    observer.stop();
}

#[test]
fn a_burst_of_saves_is_debounced_into_one_batch() {
    let dir = tmp();
    let (observer, rx) = start(dir.path());
    for n in 0..20 {
        std::fs::write(dir.path().join("lib.rs"), format!("pub fn only() -> u32 {{ {n} }}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = batch(&rx);
    assert_eq!(names(&got), ["only"]);
    assert!(got[0].text.contains("19"), "the settled content, not an intermediate one: {}", got[0].text);
    nothing_more(&rx, Duration::from_millis(500));
    observer.stop();
}

#[test]
fn identical_content_reports_nothing() {
    let dir = tmp();
    let (observer, rx) = start(dir.path());
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap();
    batch(&rx);
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap(); // touched, not changed
    nothing_more(&rx, Duration::from_millis(800));
    observer.stop();
}

#[test]
fn secrets_build_output_and_non_code_are_never_looked_at() {
    let dir = tmp();
    std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules/p")).unwrap();
    let (observer, rx) = start(dir.path());
    std::fs::write(dir.path().join(".env"), "API_KEY=sk-ABCDEF0123456789ABCDEF0123456789\n").unwrap();
    std::fs::write(dir.path().join(".env.py"), "def leak(): pass\n").unwrap();
    std::fs::write(dir.path().join("target/debug/gen.rs"), "fn generated() {}\n").unwrap();
    std::fs::write(dir.path().join("node_modules/p/index.js"), "function dep() {}\n").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "fn not_code() {}\n").unwrap();
    nothing_more(&rx, Duration::from_millis(900));
    assert_eq!(observer.stats().saves, 0, "none of them was even parsed");
    // ...while real code next to them still is.
    std::fs::write(dir.path().join("real.py"), "def real():\n    return 1\n").unwrap();
    assert_eq!(names(&batch(&rx)), ["real"]);
    observer.stop();
}

#[test]
fn the_first_save_of_a_tracked_file_is_compared_with_git_head() {
    let dir = tmp();
    let run = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir.path()).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["init", "-q"]);
    std::fs::write(dir.path().join("lib.rs"), V1).unwrap();
    run(&["add", "-A"]);
    run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

    let (observer, rx) = start(dir.path());
    std::fs::write(dir.path().join("lib.rs"), V1.replace("    3\n", "    33\n")).unwrap();
    let got = batch(&rx);
    assert_eq!(names(&got), ["gamma"], "the first save reports what changed since HEAD, not the whole file");
    assert_eq!(got[0].change, Change::Modified);
    observer.stop();
}

#[test]
fn a_file_deleted_before_it_settles_is_ignored_quietly() {
    let dir = tmp();
    let (observer, rx) = start(dir.path());
    let path = dir.path().join("gone.rs");
    std::fs::write(&path, V1).unwrap();
    std::fs::remove_file(&path).unwrap();
    nothing_more(&rx, Duration::from_millis(800));
    observer.stop();
}

#[test]
fn python_and_typescript_work_through_the_same_path() {
    let dir = tmp();
    let (observer, rx) = start(dir.path());
    std::fs::write(dir.path().join("m.py"), "class A:\n    def go(self):\n        return 1\n").unwrap();
    assert_eq!(names(&batch(&rx)), ["A", "A.go"]);
    std::fs::write(dir.path().join("s.ts"), "export const f = (x: number) => x + 1;\n").unwrap();
    assert_eq!(names(&batch(&rx)), ["f"]);
    observer.stop();
}

#[test]
fn watching_something_that_is_not_a_directory_is_an_error() {
    let dir = tmp();
    let file = dir.path().join("f.rs");
    std::fs::write(&file, "").unwrap();
    assert!(Observer::start(ObserverConfig::new(&file), |_| {}).is_err());
    assert!(Observer::start(ObserverConfig::new(dir.path().join("missing")), |_| {}).is_err());
}
