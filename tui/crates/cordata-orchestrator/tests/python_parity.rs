//! The Rust port against the Python it was ported from, on the same inputs. Skipped when `backend/.venv` is absent.
//! Free-text schema messages are not compared (the Rust draft check is hand-written); every semantic problem is.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use cordata_orchestrator::bandit::Bandit;
use cordata_orchestrator::dispatch::{self, Candidate, LearnedDispatcher, Mode, Outcome, Spec};
use cordata_orchestrator::plan::{self, Options};
use cordata_orchestrator::scope;
use serde_json::{Value, json};

fn backend() -> Option<PathBuf> {
    let b = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../backend").canonicalize().ok()?;
    b.join(".venv/bin/python").exists().then_some(b)
}

fn python(input: &Value, code: &str) -> Value {
    let backend = backend().unwrap();
    let mut child = Command::new(backend.join(".venv/bin/python"))
        .current_dir(&backend)
        .env("PYTHONPATH", &backend)
        .args(["-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn scope_agrees() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let entries = ["src/a.py", "./src//a.py", "src/", "src/**", "lib", "*.py", "src/*.py", "**/x.py", "a[0-9].py", "a[!a-z].py", "", "/abs", "a/../b", ".env", "x/.env.local", ".env.example", "src/a.py.tmp"];
    let paths = ["src/a.py", "src/a.py.tmp", "src/a.py.d/x", "src/deep/x.rs", "srcx/a.rs", "deep/x.py", "a1.py", "ab.py", "lib/x", "b.py", "./src//a.py"];
    let cases = json!({"entries": entries, "paths": paths});
    let want = python(
        &cases,
        "import json,sys\nfrom workers import scope\nd=json.load(sys.stdin)\n\
         print(json.dumps({'normalize':[scope.normalize(e) for e in d['entries']],\n\
         'valid':[scope.valid(e,'/nonexistent') for e in d['entries']],\n\
         'matches':[[scope.matches(p,[e],'/nonexistent') for p in d['paths']] for e in d['entries']],\n\
         'oos':scope.out_of_scope(d['paths'],['src/','a1.py'],'/nonexistent')}))",
    );
    let got = json!({
        "normalize": entries.iter().map(|e| scope::normalize(e)).collect::<Vec<_>>(),
        "valid": entries.iter().map(|e| scope::valid(e)).collect::<Vec<_>>(),
        "matches": entries.iter().map(|e| paths.iter().map(|p| scope::matches(p, &[e], "/nonexistent")).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "oos": scope::out_of_scope(&paths, &["src/", "a1.py"], "/nonexistent"),
    });
    assert_eq!(got, want);
}

fn good_draft() -> Value {
    json!({
        "goal": "add mul",
        "acceptance_criteria": [
            {"id": "c1", "type": "test", "description": "tests pass", "target": "pytest -q"},
            {"id": "c2", "type": "regex_match", "description": "has mul", "target": "calc/mul.py::def mul"}],
        "steps": [
            {"step_id": "s1", "name": "add \"x\"", "description": "d", "file_scope": ["./calc//add.py"], "grounded_in": ["f1"]},
            {"step_id": "s2", "name": "mul", "description": "d2", "file_scope": ["calc/mul.py", "docs/"], "depends_on": ["s1"], "complexity": "high", "risk": "destructive"},
            {"step_id": "s3", "name": "look", "description": "d3", "file_scope": ["ignored.py"], "risk": "read_only"}],
        "verifier_commands": ["pytest -q"],
        "assumptions": ["py3.12", "macos"]
    })
}

#[test]
fn plans_agree() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let mut drafts = vec![good_draft()];
    let mut mutate = |f: &dyn Fn(&mut Value)| {
        let mut d = good_draft();
        f(&mut d);
        drafts.push(d);
    };
    mutate(&|d| d["steps"][0]["file_scope"] = json!([]));
    mutate(&|d| d["steps"][0]["file_scope"] = json!(["/etc/x", "../y", ".env", "ok.py"]));
    mutate(&|d| d["steps"][1]["step_id"] = json!("s1"));
    mutate(&|d| d["steps"][0]["depends_on"] = json!(["s0", "s1"]));
    mutate(&|d| d["steps"][0]["depends_on"] = json!(["s2"]));
    mutate(&|d| d["acceptance_criteria"] = json!([{"id":"a","type":"file_not_changed","description":"x","target":"calc/add.py"}]));
    mutate(&|d| d["acceptance_criteria"] = json!([{"id":"a","type":"regex_match","description":"x","target":"nopattern"}]));
    mutate(&|d| d["acceptance_criteria"] = json!([{"id":"a","type":"command","description":"x"},{"id":"a","type":"test","description":"x","target":"t"}]));
    mutate(&|d| d["verifier_commands"] = json!(["  "]));
    mutate(&|d| d["steps"][1]["grounded_in"] = json!(["f1", "f9"]));
    let findings = ["f1"];
    let want = python(
        &json!({"drafts": drafts, "findings": findings}),
        "import json,sys\nfrom orchestrator import plan_artifact as pa\nd=json.load(sys.stdin)\n\
         out=[]\nfor draft in d['drafts']:\n\
         \tp=pa.problems(draft,'/nonexistent',set(d['findings']))\n\
         \tplan=None if p else pa.from_draft(draft,'/nonexistent',plan_id='plan-fixed',max_usd=3.5,max_tokens=1234567,findings=set(d['findings']))\n\
         \tout.append({'problems':p,'plan':plan.to_json() if plan else None,'render':plan.render() if plan else None})\n\
         print(json.dumps(out))",
    );
    let findings: HashSet<String> = findings.iter().map(|s| (*s).into()).collect();
    for (i, draft) in drafts.iter().enumerate() {
        let problems = plan::problems(draft, "/nonexistent", Some(&findings));
        assert_eq!(Value::from(problems.clone()), want[i]["problems"], "draft {i}");
        if problems.is_empty() {
            let options = Options { max_usd: 3.5, max_tokens: Some(1_234_567), plan_id: Some("plan-fixed".into()), version: 1 };
            let plan = plan::from_draft(draft, "/nonexistent", options, Some(&findings)).unwrap();
            assert_eq!(plan.to_json(), want[i]["plan"], "plan {i}");
            assert_eq!(Value::from(plan.render()), want[i]["render"], "render {i}");
        }
    }
}

fn py_candidates() -> &'static str {
    "class Cand:\n\
     \tdef __init__(s,w,m,f,v,e=None): s.worker_id=w; s.model=m; s.family=f; s.model_family=v; s.effort=e\n\
     \t@property\n\
     \tdef key(s): return f'{s.worker_id}:{s.model or \"default\"}'\n"
}

#[test]
fn dispatch_agrees() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let goals = ["Fix the crash in the parser", "update the README", "add unit tests", "rename the module", "implement caching", "hello", "fix a, and then b, also c, plus d"];
    // (worker, model, family, vendor, effort, tier)
    let roster = [("cc", "opus", "claude_code", "anthropic", Some("high"), "strong"), ("cc", "haiku", "claude_code", "anthropic", None, "fast"), ("agy", "gemini", "antigravity", "google", None, "standard")];
    // (role, class, arm index, ok, cost, reward)
    let history: Vec<(&str, &str, usize, bool, f64, Option<f64>)> = (0..30)
        .flat_map(|i| {
            [("execute", "bugfix", 0, true, 1.0, None), ("execute", "bugfix", 1, i % 3 != 0, 0.08, Some(0.4)), ("execute", "bugfix", 2, i % 2 == 0, 0.3, None), ("plan", "docs", 1, true, 0.08, None), ("plan", "docs", 2, true, 0.3, None)]
        })
        .collect();
    let queries = [("execute", "bugfix"), ("execute", "general"), ("plan", "docs"), ("audit", "docs"), ("harden", "bugfix")];
    let input = json!({"goals": goals, "roster": roster, "history": history, "queries": queries});

    let want = python(
        &input,
        &format!(
            "import json,sys,tempfile,os\nfrom orchestrator import dispatch as dp\nimport bandit\n{}\
             d=json.load(sys.stdin)\n\
             cands=[Cand(w,m,f,v,e) for (w,m,f,v,e,t) in d['roster']]\n\
             specs=[dp.Spec(c,tier=t) for c,(w,m,f,v,e,t) in zip(cands,d['roster'])]\n\
             table=bandit.Bandit(None,path=os.path.join(tempfile.mkdtemp(),'t.json'))\n\
             disp=dp.LearnedDispatcher(specs=specs,mode='live',explore=0.0,table=table,log_outcomes=False)\n\
             for role,cls,i,ok,cost,reward in d['history']:\n\
             \toutcome={{'ok':ok,'norm_cost':cost}}\n\
             \tif reward is not None: outcome['reward']=reward\n\
             \tdisp.observe(role,cls,cands[i],outcome)\n\
             ranks=[]\n\
             for role,cls in d['queries']:\n\
             \tout=disp.rank(role,cls,exclude_model_families=set())\n\
             \tranks.append({{'order':[c.key for c in out],'scores':disp.decisions[-1]['scores'] if disp.decisions else None}})\n\
             ex=disp.rank('harden','bugfix',exclude_model_families={{'anthropic'}})\n\
             print(json.dumps({{'classes':[dp.classify(g) for g in d['goals']],'ranks':ranks,'harden_ex':[c.key for c in ex]}}))",
            py_candidates()
        ),
    );

    let cands: Vec<Candidate> = roster
        .iter()
        .map(|(w, m, f, v, e, _)| Candidate { worker_id: (*w).into(), family: (*f).into(), model: Some((*m).into()), effort: e.map(Into::into), model_family: (*v).into() })
        .collect();
    let specs: Vec<Spec> = cands.iter().zip(roster.iter()).map(|(c, r)| Spec::new(c.clone(), r.5)).collect();
    let mut d = LearnedDispatcher::new(specs, Bandit::in_memory());
    (d.mode, d.explore, d.log_outcomes) = (Mode::Live, 0.0, false);
    for (role, class, i, ok, cost, reward) in &history {
        d.observe(role, class, &cands[*i], &Outcome { ok: *ok, norm_cost: Some(*cost), reward: *reward, ..Default::default() });
    }
    let none = HashSet::new();
    let mut ranks = Vec::new();
    for (role, class) in queries {
        let order: Vec<String> = d.rank(role, class, &none).iter().map(Candidate::key).collect();
        ranks.push(json!({"order": order, "scores": d.decisions.last().map(|x| x.scores.clone())}));
    }
    let anthropic: HashSet<String> = ["anthropic".to_string()].into();
    let ex: Vec<String> = d.rank("harden", "bugfix", &anthropic).iter().map(Candidate::key).collect();
    let got = json!({"classes": goals.iter().map(|g| dispatch::classify(g, None)).collect::<Vec<_>>(), "ranks": ranks, "harden_ex": ex});

    assert_eq!(got["classes"], want["classes"]);
    assert_eq!(got["harden_ex"], want["harden_ex"]);
    for (g, w) in got["ranks"].as_array().unwrap().iter().zip(want["ranks"].as_array().unwrap()) {
        assert_eq!(g["order"], w["order"]);
        let (gs, ws) = (g["scores"].as_object().unwrap(), w["scores"].as_object().unwrap());
        assert_eq!(gs.len(), ws.len());
        for (k, v) in gs {
            assert!((v.as_f64().unwrap() - ws[k].as_f64().unwrap()).abs() < 1.5e-4, "score {k}: {v} vs {}", ws[k]);
        }
    }
}

#[test]
fn secret_scrubbing_agrees() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let key = format!("sk-ant-api03-{}", "Z".repeat(40));
    let cases: Vec<String> = vec![
        format!("key {key} ok"),
        "Authorization: Bearer abcdefgh12345678".into(),
        "authorization = basic QWxhZGRpbjpvcGVu".into(),
        "curl -H 'Bearer abcdefgh1234567890xyz'".into(),
        "bearer lowercaseonlytokenvalue".into(),
        "API_KEY=\"abcd1234efgh5678ijkl\"".into(),
        "password: 'abcd1234efgh5678'".into(),
        "token = abcd1234efgh5678".into(),
        "db_secret=onlyletterslongvalueabcdef".into(),
        "max_tokens = 4096".into(),
        "token_count = compute_budget".into(),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----\nafter".into(),
        "-----BEGIN PRIVATE KEY-----\ncut off".into(),
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U".into(),
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789 and AKIAABCDEFGHIJKLMNOP and hf_abcdefghijklmnopqrstuvwxyz".into(),
        "nothing to see here".into(),
        String::new(),
    ];
    let want = python(&json!(cases), "import json,sys\nimport secret_patterns as sp\nprint(json.dumps([list(sp.scrub(t)) for t in json.load(sys.stdin)]))");
    for (i, case) in cases.iter().enumerate() {
        let (out, n) = cordata_orchestrator::secrets::scrub(case);
        assert_eq!(json!([out, n]), want[i], "case {i}: {case:?}");
    }
}
