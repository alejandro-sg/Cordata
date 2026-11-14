//! Copy-on-write sandboxes, harvest, the SYNTHESIZE check, and isolated waves through the executor and the machine.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordata_checkpoints::Store;
use cordata_ledger::{Ledger, Pool};
use cordata_orchestrator::audit::{AuditEnv, PatternGuard};
use cordata_orchestrator::checkpoints::GitCheckpoints;
use cordata_orchestrator::executor::{Config, StepExecutor};
use cordata_orchestrator::machine::{Machine, RunConfig, StaticRoster, World};
use cordata_orchestrator::plan::{self, Options};
use cordata_orchestrator::synthesizer::{self, CowIsolation, FileSynthesis, check, create, harvest, sweep};
use cordata_orchestrator::testkit::{ScriptedWorker, runnable};
use cordata_orchestrator::worker::WorkerResult;
use serde_json::{Value, json};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

fn scope(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn a_sandbox_is_a_private_copy_with_dependencies_linked_and_git_left_out() {
    let (dir, root) = project();
    write(&root, "src/a.py", "dirty, uncommitted\n");
    write(&root, "src/deep/b.txt", "b\n");
    write(&root, ".git/config", "[core]\n");
    write(&root, ".cordata/settings.json", "{}\n");
    write(&root, "node_modules/pkg/index.js", "dep\n");
    std::os::unix::fs::symlink("a.py", root.join("src/link.py")).unwrap();
    let sb = create(&root, Some(dir.path()), "step-1", &[], 100).unwrap();
    assert!(sb.root.starts_with(dir.path()) && sb.root.file_name().unwrap().to_string_lossy().starts_with("cordata-sandbox-step-1-"));
    assert_eq!(read(&sb.root, "src/a.py").unwrap(), "dirty, uncommitted\n");
    assert_eq!(read(&sb.root, "src/deep/b.txt").unwrap(), "b\n");
    assert!(!sb.root.join(".git").exists() && !sb.root.join(".cordata").exists());
    assert_eq!(sb.linked, ["node_modules"]);
    assert!(std::fs::symlink_metadata(sb.root.join("node_modules")).unwrap().file_type().is_symlink());
    assert_eq!(read(&sb.root, "node_modules/pkg/index.js").unwrap(), "dep\n", "the toolchain is reachable");
    assert_eq!(std::fs::read_link(sb.root.join("src/link.py")).unwrap(), Path::new("a.py"), "symlinks are recreated, not followed");
    assert_eq!(sb.cloned + sb.copied, 2, "two regular files; the symlink is recreated, not counted");
    #[cfg(target_os = "macos")]
    assert!(sb.cloned > 0, "APFS clones files copy-on-write");

    // Writes in the sandbox do not reach the primary tree.
    write(&sb.root, "src/a.py", "changed in the sandbox\n");
    assert_eq!(read(&root, "src/a.py").unwrap(), "dirty, uncommitted\n");
    sb.discard();
    assert!(!sb.root.exists());
}

#[test]
fn a_project_over_the_file_limit_is_refused_and_leaves_nothing_behind() {
    let (dir, root) = project();
    for i in 0..5 {
        write(&root, &format!("f{i}.txt"), "x");
    }
    let err = create(&root, Some(dir.path()), "big", &[], 4).unwrap_err();
    assert_eq!(err, "the project has more than 4 files");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("cordata-")).collect();
    assert!(leftovers.is_empty());
}

#[test]
fn harvest_copies_in_scope_changes_back_and_skips_strays() {
    let (dir, root) = project();
    for f in ["src/a.py", "src/b.py", "src/gone.py", "other/z.py"] {
        write(&root, f, "original\n");
    }
    let sb = create(&root, Some(dir.path()), "h", &scope(&["src/"]), 1000).unwrap();
    write(&sb.root, "src/a.py", "edited by the step\n");
    std::fs::remove_file(sb.root.join("src/gone.py")).unwrap();
    write(&sb.root, "src/new.py", "created by the step\n");
    write(&sb.root, "other/z.py", "stray\n");
    std::fs::remove_file(sb.root.join("src/b.py")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", sb.root.join("src/planted.py")).unwrap();

    let changed = scope(&["src/a.py", "src/gone.py", "src/new.py", "other/z.py", "src/b.py", "src/planted.py", "../escape.txt", "src/a.py"]);
    let h = harvest(&sb, &changed, &scope(&["src/"]));
    assert_eq!(h.written, ["src/a.py", "src/new.py"]);
    assert_eq!(h.deleted, ["src/b.py", "src/gone.py"]);
    assert!(h.conflicts.is_empty());
    assert_eq!(h.skipped, ["../escape.txt", "other/z.py", "src/planted.py"]);
    assert_eq!(read(&root, "src/a.py").unwrap(), "edited by the step\n");
    assert_eq!(read(&root, "src/gone.py"), None);
    assert_eq!(read(&root, "other/z.py").unwrap(), "original\n", "out-of-scope changes are left behind");
    assert!(!root.join("src/planted.py").exists());
    assert!(!root.join("src/a.py.cordata-harvest").exists(), "the temp file was renamed into place");
}

#[test]
fn a_diverged_project_refuses_the_whole_harvest_and_overwrites_nothing() {
    let (dir, root) = project();
    for f in ["src/a.py", "src/c.py", "src/d.py"] {
        write(&root, f, "original\n");
    }
    let sb = create(&root, Some(dir.path()), "d", &scope(&["src/"]), 1000).unwrap();
    write(&sb.root, "src/a.py", "step's a\n");
    write(&sb.root, "src/c.py", "step's c\n");
    write(&sb.root, "src/new.py", "step's new\n");
    std::fs::remove_file(sb.root.join("src/d.py")).unwrap();
    // The person edits c, someone creates new, d is edited, all in the primary tree while the step ran.
    write(&root, "src/c.py", "the person's edit\n");
    write(&root, "src/new.py", "somebody else created it\n");
    write(&root, "src/d.py", "edited meanwhile\n");

    let changed = scope(&["src/a.py", "src/c.py", "src/new.py", "src/d.py"]);
    assert_eq!(synthesizer::diverged(&sb, &changed), ["src/c.py", "src/d.py", "src/new.py"]);
    let h = harvest(&sb, &changed, &scope(&["src/"]));
    assert_eq!(h.conflicts, ["src/c.py", "src/d.py", "src/new.py"]);
    assert!(h.written.is_empty() && h.deleted.is_empty(), "all or nothing: the clean file a.py is not written either");
    assert_eq!(read(&root, "src/a.py").unwrap(), "original\n");
    assert_eq!(read(&root, "src/c.py").unwrap(), "the person's edit\n");
    assert_eq!(read(&root, "src/new.py").unwrap(), "somebody else created it\n");
    assert_eq!(read(&root, "src/d.py").unwrap(), "edited meanwhile\n");
}

#[test]
fn a_project_edit_between_the_clone_and_the_base_is_a_divergence() {
    let (dir, root) = project();
    write(&root, "src/a.py", "original\n");
    let mut sb = create(&root, Some(dir.path()), "r", &[], 1000).unwrap();
    // The project changes after the clone but before the base is recorded: the base is the clone, not this edit.
    write(&root, "src/a.py", "edited after the clone\n");
    synthesizer::remember(&mut sb, &scope(&["src/a.py"]));
    write(&sb.root, "src/a.py", "step's a\n");
    let h = harvest(&sb, &scope(&["src/a.py"]), &scope(&["src/"]));
    assert_eq!(h.conflicts, ["src/a.py"]);
    assert_eq!(read(&root, "src/a.py").unwrap(), "edited after the clone\n");
}

#[test]
fn sweep_removes_only_old_cordata_sandboxes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("cordata-sandbox-old-1")).unwrap();
    std::fs::create_dir(dir.path().join("keep-me")).unwrap();
    std::fs::create_dir(dir.path().join("cordata-attach-1")).unwrap();
    assert_eq!(sweep(std::time::Duration::from_secs(3600), Some(dir.path())), 0, "too young");
    assert_eq!(sweep(std::time::Duration::ZERO, Some(dir.path())), 1);
    assert!(dir.path().join("keep-me").exists() && dir.path().join("cordata-attach-1").exists() && !dir.path().join("cordata-sandbox-old-1").exists());
}

#[test]
fn check_finds_syntax_errors_in_every_supported_format() {
    let (_d, root) = project();
    write(&root, "ok.py", "def f():\n    return 1\n");
    write(&root, "bad.py", "def f(:\n    pass\n");
    write(&root, "ok.json", "{\"a\": 1}\n");
    write(&root, "bad.json", "{\"a\": 1,\n \"b\": }\n");
    write(&root, "ok.toml", "[a]\nb = 1\n");
    write(&root, "bad.toml", "[a]\nb = \n");
    write(&root, "ok.yaml", "a: 1\n---\nb: [1, 2]\n");
    write(&root, "bad.yaml", "a: [1, 2\nb: 3\n");
    write(&root, "notes.md", "# anything ( goes\n");
    let files = scope(&["ok.py", "bad.py", "ok.json", "bad.json", "ok.toml", "bad.toml", "ok.yaml", "bad.yaml", "notes.md", "missing.py", "bad.py"]);
    let problems = check(&root, &files);
    let got: Vec<(&str, &str)> = problems.iter().map(|p| (p.path.as_str(), p.check)).collect();
    assert_eq!(got, [("bad.json", "syntax"), ("bad.py", "syntax"), ("bad.toml", "syntax"), ("bad.yaml", "syntax")]);
    assert_eq!(problems[0].line, Some(2), "{:?}", problems[0]);
    assert_eq!(problems[2].line, Some(2), "{:?}", problems[2]);
    assert!(problems[1].render().starts_with("bad.py:1: [syntax] "), "{}", problems[1].render());
    assert!(problems.iter().all(|p| p.line.is_some()));
}

#[test]
fn check_finds_the_drift_two_agents_writing_two_sides_of_an_interface_produce() {
    let (_d, root) = project();
    write(&root, "calc/__init__.py", "from .add import add\nfrom .mul import mul, missing_helper\n");
    write(&root, "calc/add.py", "def add(a, b):\n    return a + b\n");
    write(&root, "calc/mul.py", "from calc.add import add, renamed_add\nimport os, sys as system\n\nCONSTANT: int = 1\nTHE, OTHER = 1, 2\n\n@decorated\ndef mul(a, b):\n    return a * b\n\nclass Thing:\n    pass\n");
    write(&root, "app.py", "from calc import add, mul, nope\nfrom calc.mul import Thing, THE, OTHER, CONSTANT, os, system, absent\nfrom calc import mul as mul_module\nfrom json import loads\nfrom third_party import anything\ndef late():\n    from calc.add import gone\n");
    write(&root, "cond.py", "import os\nif os.name == 'nt':\n    def conditional(): pass\n");
    write(&root, "star.py", "from calc.add import *\n");
    write(&root, "user.py", "from cond import conditional, whatever\nfrom star import anything_at_all\nfrom calc.add import add as plus\n");
    let all = scope(&["calc/__init__.py", "calc/add.py", "calc/mul.py", "app.py", "cond.py", "star.py", "user.py"]);
    let rendered: Vec<String> = check(&root, &all).iter().map(|p| p.render()).collect();
    assert_eq!(
        rendered,
        [
            "app.py:1: [import] `from calc import nope`: calc/__init__.py defines no `nope`",
            "app.py:2: [import] `from calc.mul import absent`: calc/mul.py defines no `absent`",
            "app.py:7: [import] `from calc.add import gone`: calc/add.py defines no `gone`",
            "calc/__init__.py:2: [import] `from .mul import missing_helper`: calc/mul.py defines no `missing_helper`",
            "calc/mul.py:1: [import] `from calc.add import renamed_add`: calc/add.py defines no `renamed_add`",
        ],
        "mul (a submodule), loads (a library), the conditional and star modules are not judged"
    );
}

// ------------------------------------------------------------------------------------------ isolated waves

struct Rig {
    _dir: tempfile::TempDir,
    project: PathBuf,
    ledger: Ledger,
    checkpoints: GitCheckpoints,
    events: Arc<Mutex<Vec<Value>>>,
}

fn rig() -> Rig {
    let (dir, project) = project();
    write(&project, "calc/base.txt", "base\n");
    let ledger = Ledger::open(&dir.path().join("state.db"), vec![Pool::new("cc", "claude_code").limit(1_000_000.0)]).unwrap();
    let checkpoints = GitCheckpoints::new(Store::new(dir.path().join("cp")));
    Rig { _dir: dir, project, ledger, checkpoints, events: Arc::default() }
}

fn step_draft(files: &[&str], deps: &[(&str, &[&str])]) -> Value {
    let steps: Vec<Value> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let id = format!("s{}", i + 1);
            let d: Vec<&str> = deps.iter().find(|(s, _)| *s == id).map(|(_, d)| d.to_vec()).unwrap_or_default();
            json!({"step_id": id, "name": format!("write {f}"), "description": "write it", "file_scope": [format!("calc/{f}")], "depends_on": d})
        })
        .collect();
    json!({"goal": "write", "acceptance_criteria": [{"id": "c", "type": "file_exists", "description": "d", "target": "calc/base.txt"}], "steps": steps})
}

#[tokio::test]
async fn parallel_steps_run_in_their_own_sandboxes_and_are_harvested_into_the_project() {
    let r = rig();
    let cwds = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let seen = cwds.clone();
    let project = r.project.clone();
    let worker = ScriptedWorker::slow("w", std::time::Duration::from_millis(30), move |task, _| {
        let step = task.task_id.split(':').nth(1).unwrap().to_owned();
        seen.lock().unwrap().push((step.clone(), task.cwd.clone()));
        // Sibling files are invisible in a sandbox, and the primary tree does not change until harvest.
        let sibling_visible = task.cwd != project.to_string_lossy() && ["a.txt", "b.txt"].iter().any(|f| Path::new(&task.cwd).join("calc").join(f).exists() && !task.file_scope.iter().any(|s| s.ends_with(f)));
        assert!(!sibling_visible, "a sandbox never shows a sibling's files");
        for entry in &task.file_scope {
            write(Path::new(&task.cwd), entry, &format!("ok {step}"));
            assert!(!project.join(entry).exists() || task.cwd == project.to_string_lossy(), "the project is untouched until harvest");
        }
        let mut res = WorkerResult::new("w", "completed");
        res.files_changed = task.file_scope.clone();
        res
    });
    let plan = plan::from_draft(&step_draft(&["a.txt", "b.txt", "c.txt"], &[("s3", &["s1", "s2"])]), "/x", Options::default(), None).unwrap();
    let sink = r.events.clone();
    let cfg = Config { isolation: Some(Arc::new(CowIsolation::default())), on_event: Some(Arc::new(move |e| sink.lock().unwrap().push(e))), ..Default::default() };
    let ex = StepExecutor::new("run-iso", &r.project, plan, vec![runnable(&worker, "m", "claude_code", "anthropic")], &r.ledger, &r.checkpoints, cfg);
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    assert_eq!(read(&r.project, "calc/a.txt").unwrap(), "ok s1");
    assert_eq!(read(&r.project, "calc/b.txt").unwrap(), "ok s2");
    assert_eq!(read(&r.project, "calc/c.txt").unwrap(), "ok s3");
    let mut harvested = out.harvested.clone();
    harvested.sort();
    assert_eq!(harvested, ["calc/a.txt", "calc/b.txt"], "only the wave of two was isolated; the dependent step ran alone in the shared tree");
    let cwds = cwds.lock().unwrap().clone();
    let project_s = r.project.to_string_lossy().into_owned();
    assert!(cwds.iter().filter(|(s, _)| s == "s1" || s == "s2").all(|(_, c)| *c != project_s && Path::new(c).file_name().unwrap().to_string_lossy().starts_with("cordata-sandbox-s")));
    assert_eq!(cwds.iter().find(|(s, _)| s == "s3").unwrap().1, project_s);
    assert!(cwds.iter().filter(|(s, _)| s != "s3").all(|(_, c)| !Path::new(c).exists()), "sandboxes are discarded afterwards");
    assert_eq!(r.events.lock().unwrap().iter().filter(|e| e["kind"] == "harvest").count(), 2);
}

#[tokio::test]
async fn a_harvest_conflict_fails_the_attempt_and_the_retry_builds_on_what_changed() {
    let r = rig();
    let project = r.project.clone();
    let calls = Arc::new(Mutex::new(0));
    let n = calls.clone();
    let worker = ScriptedWorker::new("w", move |task, _| {
        let step = task.task_id.split(':').nth(1).unwrap().to_owned();
        if step == "s1" {
            let mut c = n.lock().unwrap();
            *c += 1;
            if *c == 1 {
                // The person edits the file the step is working on, in the primary tree, while the step runs.
                write(&project, "calc/a.txt", "the person's edit");
            }
        }
        for entry in &task.file_scope {
            write(Path::new(&task.cwd), entry, &format!("ok {step}"));
        }
        let mut res = WorkerResult::new("w", "completed");
        res.files_changed = task.file_scope.clone();
        res
    });
    write(&r.project, "calc/a.txt", "start");
    let plan = plan::from_draft(&step_draft(&["a.txt", "b.txt", "c.txt"], &[("s3", &["s1", "s2"])]), "/x", Options::default(), None).unwrap();
    let cfg = Config { isolation: Some(Arc::new(CowIsolation::default())), ..Default::default() };
    let ex = StepExecutor::new("run-conflict", &r.project, plan, vec![runnable(&worker, "m", "claude_code", "anthropic")], &r.ledger, &r.checkpoints, cfg);
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    assert_eq!(out.conflicts, ["calc/a.txt"], "the first attempt hit the person's edit and was not allowed to overwrite it");
    assert_eq!(*calls.lock().unwrap(), 2);
    let log = &out.results["s1"];
    assert_eq!(log.len(), 2, "two attempts logged");
    // The retry started from a fresh sandbox of the edited tree, so its harvest base matched and it landed.
    assert_eq!(read(&r.project, "calc/a.txt").unwrap(), "ok s1");
}

#[tokio::test]
async fn too_large_a_project_runs_the_wave_shared_instead_of_failing() {
    let r = rig();
    let cwds = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = cwds.clone();
    let worker = ScriptedWorker::new("w", move |task, _| {
        seen.lock().unwrap().push(task.cwd.clone());
        for entry in &task.file_scope {
            write(Path::new(&task.cwd), entry, "ok");
        }
        let mut res = WorkerResult::new("w", "completed");
        res.files_changed = task.file_scope.clone();
        res
    });
    let plan = plan::from_draft(&step_draft(&["a.txt", "b.txt"], &[]), "/x", Options::default(), None).unwrap();
    // No dependencies means no waves at all: force one with a depends_on chain-free pair via a third step.
    let mut plan = plan;
    plan.steps.push(plan.steps[0].clone());
    plan.steps[2].step_id = "s3".into();
    plan.steps[2].file_scope = vec!["calc/c.txt".into()];
    plan.steps[2].depends_on = vec!["s1".into()];
    let cfg = Config { isolation: Some(Arc::new(CowIsolation::default().max_files(0))), ..Default::default() };
    let ex = StepExecutor::new("run-big", &r.project, plan, vec![runnable(&worker, "m", "claude_code", "anthropic")], &r.ledger, &r.checkpoints, cfg);
    let out = ex.run().await;
    assert_eq!(out.status, "completed", "{}", out.reason);
    let project_s = r.project.to_string_lossy().into_owned();
    assert!(cwds.lock().unwrap().iter().all(|c| *c == project_s), "every step fell back to the shared tree");
    assert!(out.harvested.is_empty());
}

// ------------------------------------------------------------------------------------------ the machine

#[tokio::test]
async fn the_machine_synthesizes_a_parallel_wave_and_runs_a_fix_pass_for_drift() {
    let r = rig();
    let project = std::fs::canonicalize(&r.project).unwrap();
    let audits: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new([json!({"verdict": "accept", "items": []})].into()));
    let draft = json!({
        "goal": "write two modules", "acceptance_criteria": [{"id": "c", "type": "file_exists", "description": "d", "target": "calc/a.py"}],
        "steps": [
            {"step_id": "s1", "name": "a", "description": "a", "file_scope": ["calc/a.py"]},
            {"step_id": "s2", "name": "b", "description": "b", "file_scope": ["calc/b.py"]},
            {"step_id": "s3", "name": "c", "description": "c", "file_scope": ["calc/c.py"], "depends_on": ["s1", "s2"]}]});
    let (a2, d2) = (audits.clone(), draft.clone());
    let p2 = project.clone();
    let worker = ScriptedWorker::new("w", move |task, _| {
        let label = task.task_id.split(':').nth(1).unwrap().to_owned();
        let mut res = WorkerResult::new("w", "completed");
        match (task.role.as_str(), label.as_str()) {
            ("plan", _) => res.structured = Some(d2.clone()),
            ("audit", _) => res.structured = a2.lock().unwrap().pop_front(),
            ("harden", _) => {}
            _ => {
                let fixing = task.prompt.contains("Make the parallel steps fit together");
                for entry in &task.file_scope {
                    // s1 writes the interface; s2, written in parallel and blind to it, imports a name that was never defined —
                    // until the synthesis fix pass makes the two sides fit.
                    let body = match entry.as_str() {
                        "calc/a.py" => "def add(a, b):\n    return a + b\n".to_owned(),
                        "calc/b.py" if !fixing => "from calc.a import plus\n".to_owned(),
                        "calc/b.py" => "from calc.a import add\n".to_owned(),
                        other => format!("# {other}\n"),
                    };
                    write(Path::new(&task.cwd), entry, &body);
                    res.files_changed.push(entry.clone());
                }
                let _ = &p2;
            }
        }
        res
    });
    let mut roster = StaticRoster::default();
    let me = runnable(&worker, "m", "claude_code", "anthropic");
    let other = runnable(&worker, "g", "antigravity", "google");
    for role in ["plan", "execute"] {
        roster.roles.insert(role.into(), vec![me.clone()]);
    }
    roster.roles.insert("harden".into(), vec![other.clone()]);
    roster.roles.insert("audit".into(), vec![other]);
    let events = r.events.clone();
    let world = World {
        roster: &mut roster,
        ledger: &r.ledger,
        checkpoints: &r.checkpoints,
        audit_env: AuditEnv { checkpoints: &r.checkpoints, guard: &PatternGuard, environ: std::env::vars().collect() },
        research: None,
        synthesis: Some(&FileSynthesis),
        on_event: Some(Arc::new(move |e| events.lock().unwrap().push(e))),
        controls: None,
        isolation: Some(Arc::new(CowIsolation::default())),
        home: r._dir.path().join("home"),
    };
    let mut m = Machine::new("write two modules", &project.to_string_lossy(), RunConfig { max_usd: 5.0, research: false, ..Default::default() }, world, Some("run-synth-e2e"), None, None);
    m.run().await;
    let record = m.record.clone();
    assert_eq!(record.state, "done", "{}", record.reason);
    let states: Vec<&str> = record.history.iter().map(|h| h["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["plan", "execute", "synthesize", "harden", "audit", "done"]);
    assert_eq!(record.synth_fixes, 1);
    assert_eq!(read(&project, "calc/b.py").unwrap(), "from calc.a import add\n", "the fix pass made the two sides fit");
    let synthesis = record.synthesis.unwrap();
    assert_eq!(synthesis["problems"], json!([]));
    assert_eq!(synthesis["harvested"], json!(["calc/a.py", "calc/b.py"]));
    let kinds: Vec<String> = r.events.lock().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert!(kinds.contains(&"synthesis".to_owned()) && kinds.contains(&"harvest".to_owned()));
    let _ = synthesizer::SKIP;
}
