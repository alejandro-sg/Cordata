//! One shadow repository, two implementations: what Rust snapshots Python lists, diffs and restores, and what Python
//! snapshots Rust restores. Skipped when `backend/.venv` is absent.

use std::path::{Path, PathBuf};
use std::process::Command;

use cordata_checkpoints::{RestoreOptions, Store, TakeOptions};
use serde_json::{Value, json};

fn backend() -> Option<PathBuf> {
    let b = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../backend").canonicalize().ok()?;
    b.join(".venv/bin/python").exists().then_some(b)
}

fn python(home: &Path, code: &str) -> Value {
    let backend = backend().unwrap();
    let out = Command::new(backend.join(".venv/bin/python"))
        .current_dir(&backend)
        .env("PYTHONPATH", &backend)
        .env("CORDATA_HOME", home)
        .args(["-c", code])
        .output()
        .unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("bad json ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn the_two_implementations_share_one_store() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let dir = tempfile::tempdir().unwrap();
    let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
    std::fs::create_dir_all(&project).unwrap();
    let store = Store::new(&home);
    let proj = project.to_string_lossy().into_owned();

    // The same directory for the same project.
    let py_dir = python(&home, &format!("import json,checkpoints\nprint(json.dumps(str(checkpoints.shadow_dir({proj:?}))))"));
    assert_eq!(py_dir.as_str().unwrap(), store.shadow_dir(&project).to_string_lossy());

    // Rust takes; Python lists it (metadata round-trips), diffs against it, and restores it.
    write(&project, "a.txt", "one\n");
    write(&project, "sub/b.txt", "bee\n");
    let rust_id = store
        .take(&project, &TakeOptions { kind: "pre".into(), tool: "edit".into(), agent: "executor".into(), session_id: "s1".into(), turn: "t1".into(), call: "step é".into(), paths: vec!["a.txt".into()], ..Default::default() })
        .id
        .unwrap();
    write(&project, "a.txt", "changed\n");
    std::fs::remove_file(project.join("sub/b.txt")).unwrap();
    let seen = python(
        &home,
        &format!(
            "import json,checkpoints\nrows=checkpoints.list_checkpoints({proj:?})\npatch,stat=checkpoints.diff_since({proj:?},{rust_id:?})\n\
             print(json.dumps({{'rows':rows,'stat':stat,'patch_has':'-one' in patch and '+changed' in patch}}))"
        ),
    );
    let row = &seen["rows"][0];
    assert_eq!((row["id"].as_str(), row["kind"].as_str(), row["tool"].as_str(), row["turn"].as_str(), row["call"].as_str()), (Some(rust_id.as_str()), Some("pre"), Some("edit"), Some("t1"), Some("step é")));
    assert_eq!(row["paths"], json!(["a.txt"]));
    assert_eq!(seen["stat"], "2 files changed, 1 insertion(+), 2 deletions(-)");
    assert_eq!(seen["patch_has"], true);
    let restored = python(&home, &format!("import json,checkpoints\nr=checkpoints.restore({proj:?},{rust_id:?})\nprint(json.dumps({{'ok':r['ok'],'count':r['count']}}))"));
    assert_eq!(restored, json!({"ok": true, "count": 2}));
    assert_eq!(std::fs::read_to_string(project.join("a.txt")).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(project.join("sub/b.txt")).unwrap(), "bee\n");

    // Python takes (and restores once, so the history has a `restore` commit); Rust reads the history and restores it.
    let py_id = python(&home, &format!("import json,checkpoints\nr=checkpoints.take({proj:?},kind='pre',tool='shell',agent='a',session_id='s2',turn='t2',call='py step')\nprint(json.dumps(r.id))"));
    let py_id = py_id.as_str().unwrap().to_owned();
    write(&project, "a.txt", "after python\n");
    write(&project, "extra.txt", "x\n");
    let rows = store.list(&project, 50);
    assert!(rows.iter().any(|r| r.id == py_id && r.meta["tool"] == "shell" && r.meta["session"] == "s2"));
    let turns = store.turns(&project, 8);
    assert_eq!(turns.iter().map(|t| t.turn.as_str()).collect::<Vec<_>>(), ["t2", "t1", ]);
    let r = store.restore(&project, &py_id, &RestoreOptions::default());
    assert!(r.ok, "{r:?}");
    assert_eq!(std::fs::read_to_string(project.join("a.txt")).unwrap(), "one\n");
    assert!(!project.join("extra.txt").exists());

    // And Python reads the `restore` commit Rust just wrote: the turn bookkeeping agrees.
    let undone = python(&home, &format!("import json,checkpoints\nprint(json.dumps([[t['turn'],t['undone'],t['calls']] for t in checkpoints.turns({proj:?})]))"));
    assert_eq!(undone, json!([["t2", false, 1], ["t1", false, 1]]));
}
