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

// ---------------------------------------------------------------------------------------------- workers

use cordata_orchestrator::worker::TaskPacket;
use cordata_orchestrator::workers::cli::{Dialect, LineParser};
use cordata_orchestrator::workers::{agy, claude, prompting};

fn task_json(t: &TaskPacket) -> Value {
    json!({"task_id": t.task_id, "role": t.role, "prompt": t.prompt, "cwd": t.cwd, "file_scope": t.file_scope, "budget_usd": t.budget_usd,
           "budget_tokens": t.budget_tokens, "timeout_s": t.timeout_s, "model": t.model, "effort": t.effort, "handoff_context": t.handoff_context,
           "json_schema": t.json_schema})
}

fn tasks() -> Vec<TaskPacket> {
    let mk = |role: &str, scope: &[&str], f: &dyn Fn(&mut TaskPacket)| {
        let mut t = TaskPacket::new("run:s1:1", role, "  Fix the thing é  ", "/tmp/p");
        t.file_scope = scope.iter().map(|s| (*s).to_owned()).collect();
        f(&mut t);
        t
    };
    vec![
        mk("execute", &["calc/a.py", "docs/"], &|_| {}),
        mk("execute", &["calc/a.py"], &|t| {
            t.budget_usd = Some(1.5);
            t.model = Some("sonnet".into());
            t.effort = Some("high".into());
            t.timeout_s = 90.9;
        }),
        mk("audit", &["calc/a.py"], &|t| t.json_schema = Some(json!({"type": "object", "description": "résumé 😀", "properties": {"b": {"type": "string"}, "a": {}}}))),
        mk("plan", &[], &|t| t.budget_usd = Some(0.0000001)),
        mk("harden", &["x.rs"], &|t| {
            t.budget_usd = Some(123456789.0);
            t.model = Some("claude-sonnet-5-5-high".into());
            t.effort = Some("max".into());
        }),
        mk("execute", &["a"], &|t| t.budget_usd = Some(1.23456789)),
        mk("execute", &["a"], &|t| t.budget_usd = Some(0.5)),
        mk("execute", &["a"], &|t| {
            let secret = format!("sk-ant-api03-{}", "Q".repeat(40));
            t.handoff_context = Some(json!({"reason": "worker_failed", "note": format!("é😀 {secret} {} end", prompting::FENCE_CLOSE), "list": [3, 1, {"z": 1, "a": [true, null]}], "failed_attempts": ["a", "b"]}));
        }),
    ]
}

#[test]
fn argv_prompts_and_stdin_agree() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let ts = tasks();
    let input = json!({"tasks": ts.iter().map(task_json).collect::<Vec<_>>()});
    let want = python(
        &input,
        "import json,sys\nfrom workers.base import TaskPacket\nfrom workers.claude_worker import ClaudeCodeWorker\nfrom workers.agy_worker import AgyWorker\nfrom workers import prompting\n\
         d=json.load(sys.stdin)\ncw,aw=ClaudeCodeWorker(),AgyWorker()\nout=[]\nfor raw in d['tasks']:\n\
         \tt=TaskPacket(**raw)\n\
         \trow={'claude':[cw.build_argv('claude',t,None),cw.build_argv('claude',t,'sess-9')],'agy':[aw.build_argv('agy',t,None),aw.build_argv('agy',t,'conv-9')],\n\
         \t 'agy_stdin':aw.build_stdin(t,'P'),'prompt_native':prompting.compose(t,native_schema=True),'prompt_text':prompting.compose(t,native_schema=False,can_run_commands=False)}\n\
         \tout.append(row)\nprint(json.dumps(out))",
    );
    let (cd, ad) = (claude::ClaudeDialect, agy::AgyDialect);
    for (i, t) in ts.iter().enumerate() {
        let model = t.model.as_deref();
        let got = json!({
            "claude": [cd.build_argv("claude", t, None, model), cd.build_argv("claude", t, Some("sess-9"), model)],
            "agy": [ad.build_argv("agy", t, None, model), ad.build_argv("agy", t, Some("conv-9"), model)],
            "agy_stdin": ad.build_stdin(t, "P"),
            "prompt_native": prompting::compose(t, true, true),
            "prompt_text": prompting::compose(t, false, false),
        });
        for key in ["claude", "agy", "agy_stdin", "prompt_native", "prompt_text"] {
            assert_eq!(got[key], want[i][key], "task {i}: {key}");
        }
    }
}

fn state_json(p: &mut dyn LineParser) -> Value {
    let s = p.st();
    json!({"session_handle": s.session_handle, "model": s.model, "text": s.text, "structured": s.structured, "cost_usd": s.cost_usd, "tokens_in": s.tokens_in,
           "tokens_out": s.tokens_out, "requests": s.requests, "rate_limited": s.rate_limited, "budget_exhausted": s.budget_exhausted, "error": s.error,
           "ok": s.ok, "denied": s.denied, "quota_info": if s.quota_info.is_null() { json!({}) } else { s.quota_info.clone() }})
}

fn transcripts() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let claude = vec![
        vec![
            json!({"type": "system", "subtype": "init", "session_id": "s1", "model": "claude-sonnet-5-5"}),
            json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 85, "output_tokens": 3}, "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "  pytest   -q  tests/  --maxfail=1 --tb=short -x -v -k 'a or b or c' --durations=10 --color=no end"}},
                {"type": "tool_use", "id": "t2", "name": "StructuredOutput", "input": {"verdict": "pass"}},
                {"type": "tool_use", "id": "t3", "name": "Grep", "input": {"pattern": "needle"}},
                {"type": "text", "text": "first"}, {"type": "text", "text": ""}, "junk"]}}),
            json!({"type": "assistant", "message": {"id": "m2", "usage": {"input_tokens": 1, "output_tokens": 9}, "content": [{"type": "text", "text": "second"}]}}),
            json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 5, "output_tokens": 30}, "content": []}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "is_error": true}, {"type": "tool_result", "tool_use_id": "zz"}]}}),
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour", "unifiedWindows": {"five_hour": {"utilization": 0.5, "resetsAt": 1900000000}, "bad": {"utilization": "x"}}}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "Final text é", "total_cost_usd": 0.4321, "session_id": "s1b", "num_turns": 3,
                   "modelUsage": {"claude-sonnet-5-5": {"inputTokens": 200, "cacheReadInputTokens": 50, "cacheCreationInputTokens": 1, "outputTokens": 60}},
                   "structured_output": {"verdict": "fail"}, "permission_denials": [{"tool_name": "Bash"}, "Write"]}),
        ],
        vec![
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "REJECTED", "rateLimitType": "seven_day", "utilization": 0.99, "resetsAt": 1900000500}}),
            json!({"type": "result", "subtype": "error_max_budget_usd", "is_error": true, "errors": ["over budget", "stop"]}),
        ],
        vec![json!({"type": "result", "subtype": "error_during_execution", "is_error": false, "result": "usage limit reached for today", "terminal_reason": "budget_exhausted"})],
        vec![json!({"type": "result", "subtype": "success", "is_error": true})],
        vec![json!({"type": "mystery"}), json!({"type": "system", "subtype": "other"})],
    ];
    let agy = vec![
        vec![
            json!({"event": "init", "conversation_id": "c1", "init": {"model": "gemini-3.8-flash-high"}}),
            json!({"event": "step_update", "step_update": {"step_type": "tool", "state": "ACTIVE", "tool_name": "run_command", "tool_info": {"parameters": {"CommandLine": "  ls    -la   /tmp "}}}}),
            json!({"event": "step_update", "step_update": {"step_type": "tool", "state": "DONE", "tool_name": "run_command"}}),
            json!({"event": "step_update", "step_update": {"step_type": "tool", "state": "ACTIVE"}}),
            json!({"event": "step_update", "step_update": {"step_type": "agent_response", "state": "ACTIVE", "text_delta": "he"}}),
            json!({"event": "step_update", "step_update": {"step_type": "agent_response", "state": "DONE", "text_delta": "llo", "step_index": 2, "usage": {"input_tokens": 5, "cache_read_tokens": 7, "output_tokens": 2, "thinking_tokens": 1}}}),
            json!({"event": "step_update", "step_update": {"step_type": "agent_response", "state": "DONE", "step_index": 2, "usage": {"input_tokens": 9, "output_tokens": 4}}}),
            json!({"event": "step_update", "step_update": {"step_type": "system_message", "state": "DONE", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
            json!({"event": "result", "result": {"status": "SUCCESS", "response": "Done!", "conversation_id": "c1b", "usage": {"input_tokens": 100, "output_tokens": 1},
                    "structured_output": {"a": 1, "toolAction": "x", "toolSummary": "y"}, "denied_actions": [{"action": "RUN", "display_name": "make"}, "raw", {"action": "EDIT"}], "num_turns": 4}}),
        ],
        vec![json!({"event": "result", "result": {"status": "error", "error": "429 Resource exhausted"}})],
        vec![json!({"event": "result", "result": {"status": "ERROR"}})],
    ];
    (claude, agy)
}

#[test]
fn stream_parsers_agree() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let (claude_t, agy_t) = transcripts();
    let want = python(
        &json!({"claude": claude_t, "agy": agy_t}),
        "import json,sys\nfrom workers.base import TaskPacket\nfrom workers.claude_worker import ClaudeStream\nfrom workers.agy_worker import AgyStream\n\
         d=json.load(sys.stdin)\nt=TaskPacket('t','execute','p','/tmp')\n\
         def drive(cls,runs):\n\tout=[]\n\tfor events in runs:\n\t\tp=cls(t)\n\t\tevs=[[w.event_type,w.payload] for e in events for w in p.apply(e)]\n\
         \t\tout.append({'events':evs,'state':{k:getattr(p,k) for k in ['session_handle','model','text','structured','cost_usd','tokens_in','tokens_out','requests','rate_limited','budget_exhausted','error','ok','denied','quota_info']}})\n\treturn out\n\
         print(json.dumps({'claude':drive(ClaudeStream,d['claude']),'agy':drive(AgyStream,d['agy'])}))",
    );
    let drive = |mk: &dyn Fn() -> Box<dyn LineParser>, runs: &[Vec<Value>]| -> Vec<Value> {
        runs.iter()
            .map(|events| {
                let mut p = mk();
                let evs: Vec<Value> = events.iter().flat_map(|e| p.apply(e)).map(|w| json!([w.event_type, w.payload])).collect();
                json!({"events": evs, "state": state_json(&mut *p)})
            })
            .collect()
    };
    let got_claude = drive(&|| claude::ClaudeDialect.new_parser(), &claude_t);
    let got_agy = drive(&|| agy::AgyDialect.new_parser(), &agy_t);
    for (i, g) in got_claude.iter().enumerate() {
        assert_eq!(g["events"], want["claude"][i]["events"], "claude run {i}: events");
        assert_eq!(g["state"], want["claude"][i]["state"], "claude run {i}: state");
    }
    for (i, g) in got_agy.iter().enumerate() {
        assert_eq!(g["events"], want["agy"][i]["events"], "agy run {i}: events");
        assert_eq!(g["state"], want["agy"][i]["state"], "agy run {i}: state");
    }
}

#[test]
fn scoped_profiles_answers_and_wire_helpers_agree() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let scopes: Vec<Vec<&str>> = vec![vec![], vec!["calc/add.py", "docs/", "*.md"], vec!["src/**", "lib/*.py", "a/b/c.txt", "/abs", ".env", "docs"], vec!["x/[ab]/y.py", "./z//w.rs"]];
    let answers = ["{\"a\":1}", "Here:\n```json\n{\"a\":2}\n```\nbye", "so {\"a\":3} it is", "[1,2]", "", "```\n{\"a\":4}\n```", "{broken} {\"a\":5}"];
    let want = python(
        &json!({"root": root, "scopes": scopes, "answers": answers}),
        "import json,sys\nfrom workers import scope, prompting\nd=json.load(sys.stdin)\n\
         print(json.dumps({'profiles':[scope.scoped_profile(d['root'],s,'/Users/x') for s in d['scopes']],'answers':[prompting.parse_json_answer(a) for a in d['answers']]}))",
    );
    for (i, s) in scopes.iter().enumerate() {
        assert_eq!(Value::from(scope::scoped_profile(&root, s, "/Users/x")), want["profiles"][i], "profile {i}");
    }
    for (i, a) in answers.iter().enumerate() {
        assert_eq!(prompting::parse_json_answer(a).unwrap_or(Value::Null), want["answers"][i], "answer {i}: {a:?}");
    }
}

// ---------------------------------------------------------------------------------------------- machine & audit

use cordata_orchestrator::audit::{self, AuditEnv, PatternGuard};
use cordata_orchestrator::checkpoints::{Checkpoints, GitCheckpoints};
use cordata_orchestrator::machine::{self, RunRecord};

fn python_home(home: &std::path::Path, input: &Value, code: &str) -> Value {
    let backend = backend().unwrap();
    let mut child = Command::new(backend.join(".venv/bin/python"))
        .current_dir(&backend)
        .env("PYTHONPATH", &backend)
        .env("CORDATA_HOME", home)
        .env("CORDATA_STATE_DB", home.join("state.db"))
        .args(["-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("bad json ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn sample_record() -> RunRecord {
    let mut r = RunRecord::new("run-x", "make \"it\" work é", "/tmp/p", "bugfix", Some("sess"));
    r.state = "done".into();
    r.reason = "all good".into();
    let plan = plan::from_draft(&good_draft(), "/nonexistent", Options { plan_id: Some("plan-1".into()), ..Default::default() }, Some(&["f1".to_string()].into_iter().collect())).unwrap();
    let mut plan_json = plan.to_json();
    plan_json["steps"][0]["status"] = json!("completed");
    plan_json["steps"][0]["worker"] = json!("claude_code:sonnet");
    plan_json["steps"][0]["attempts"] = json!(2);
    r.plan = Some(plan_json);
    r.research = Some(json!({"findings": []}));
    r.planner = Some("claude_code:sonnet".into());
    r.workers.insert("plan".into(), vec!["claude_code:sonnet".into()]);
    r.workers.insert("harden".into(), vec!["agy:gemini".into()]);
    r.audit = Some(json!({"tier1_ok": true, "verdict": "accept", "checks": [{"id": "scope", "kind": "scope", "ok": true, "detail": "2 file(s) changed\nmore"}, {"id": "c1", "kind": "test", "ok": false, "detail": ""}], "changed": ["a.py", "b.py"]}));
    r.handoffs = vec!["h1".into(), "h2".into()];
    r.drift_retries = 1;
    r.replans = 1;
    r.usage = vec![json!({"role": "execute", "worker": "claude_code:sonnet", "calls": 3, "tokens_in": 100, "tokens_out": 20, "cost_usd": 0.123456})];
    r.warnings = vec!["careful".into(), "very".into()];
    r
}

#[test]
fn records_and_walkthroughs_agree_both_ways() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let rust = sample_record();
    let ledger = cordata_ledger::Ledger::open(&home.join("state.db"), vec![]).unwrap();
    ledger.save_run(&rust.to_json()).unwrap();

    // Python loads what Rust saved and renders the same walkthrough; it also re-serialises every field Rust wrote.
    let py = python_home(
        home,
        &json!({}),
        "import json\nfrom orchestrator import machine\nr=machine.load('run-x')\nprint(json.dumps({'walk':machine.walkthrough(r),'state':r.state,'keys':sorted(r.to_json().keys()),'plan_md':machine.Plan.from_json(r.plan).render()}))",
    );
    assert_eq!(py["walk"].as_str().unwrap(), machine::walkthrough(&rust));
    assert_eq!(py["plan_md"].as_str().unwrap(), plan::Plan::from_json(rust.plan.as_ref().unwrap()).unwrap().render());
    let mut rust_keys: Vec<String> = rust.to_json().as_object().unwrap().keys().cloned().collect();
    rust_keys.sort();
    assert_eq!(py["keys"], json!(rust_keys), "the same record fields on both sides");

    // And the other way: a Python-written record loads in Rust and renders identically.
    let written = python_home(
        home,
        &json!({"record": rust.to_json()}),
        "import json,sys\nfrom orchestrator import machine\nd=json.load(sys.stdin)['record']\nd['run_id']='run-py'\nr=machine.RunRecord.from_json(d)\nmachine.save(r)\nprint(json.dumps(machine.walkthrough(r)))",
    );
    let loaded = machine::load(&ledger, "run-py").expect("Rust reads the Python-saved run");
    assert_eq!(Value::from(machine::walkthrough(&loaded)), written);
    assert_eq!((loaded.history.len(), loaded.drift_retries, loaded.workers["harden"].clone()), (0, 1, vec!["agy:gemini".to_string()]));
    assert_eq!(machine::recent(&ledger, 10).len(), 2);
}

#[tokio::test]
async fn tier1_and_the_review_prompts_agree() {
    if backend().is_none() {
        return eprintln!("skipped: backend/.venv not found");
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join("calc")).unwrap();
    std::fs::write(project.join("calc/keep.txt"), "keep\n").unwrap();
    std::fs::write(project.join("calc/a.txt"), "old\n").unwrap();
    let root = std::fs::canonicalize(&project).unwrap().to_string_lossy().into_owned();
    // The shared store: Rust takes the plan-start checkpoint; both implementations read changes and diffs from it.
    let store = GitCheckpoints::new(cordata_checkpoints::Store::new(&home));
    let start = store.take(&project, "run", "plan-start", "g").id.unwrap();
    std::fs::write(project.join("calc/a.txt"), "ok a\n").unwrap();
    std::fs::write(project.join("calc/stray.txt"), "oops\n").unwrap();

    let mut d = good_draft();
    d["acceptance_criteria"] = json!([
        {"id": "exists", "type": "file_exists", "description": "a exists", "target": "calc/a.txt"},
        {"id": "dir", "type": "file_exists", "description": "dir exists", "target": "calc"},
        {"id": "missing", "type": "file_exists", "description": "n/a", "target": "calc/none.txt"},
        {"id": "keep", "type": "file_not_changed", "description": "keep unchanged", "target": "calc/keep.txt"},
        {"id": "moved", "type": "file_not_changed", "description": "a unchanged?", "target": "./calc/stray.txt"},
        {"id": "rx", "type": "regex_match", "description": "a has ok", "target": "calc/a.txt::^ok a$"},
        {"id": "rx-no", "type": "regex_match", "description": "n", "target": "calc/a.txt::nope"},
        {"id": "rx-gone", "type": "regex_match", "description": "n", "target": "calc/gone.txt::x"}
    ]);
    d["steps"] = json!([{"step_id": "s1", "name": "n", "description": "d", "file_scope": ["calc/a.txt"]}, {"step_id": "s2", "name": "n2", "description": "d2", "file_scope": ["calc/keep2.txt"], "grounded_in": ["f1"]}]);
    d["verifier_commands"] = json!([]);
    let mut plan = plan::from_draft(&d, "/nonexistent", Options { plan_id: Some("plan-t".into()), ..Default::default() }, None).unwrap();
    plan.steps[0].status = "completed".into();
    plan.steps[1].status = "completed".into();

    let want = python_home(
        &home,
        &json!({"plan": plan.to_json(), "root": root, "start": start}),
        "import json,sys,asyncio\nfrom orchestrator import audit\nfrom orchestrator.plan_artifact import Plan\nd=json.load(sys.stdin)\nplan=Plan.from_json(d['plan'])\n\
         rep=asyncio.run(audit.tier1(plan,d['root'],d['start']))\n\
         diff='+line\\n'*3\n\
         print(json.dumps({'report':rep.to_json(),'t2':audit.tier2_prompt(plan,rep,diff),'conf':audit.conformance_prompt(plan,{'findings':[]},rep,diff),'conf_nobrief':audit.conformance_prompt(plan,None,rep,diff)}))",
    );
    let env = AuditEnv { checkpoints: &store, guard: &PatternGuard, environ: std::env::vars().collect() };
    let report = audit::tier1(&plan, &root, Some(&start), &env).await;
    let mut got = report.to_json();
    let mut expected = want["report"].clone();
    for r in [&mut got, &mut expected] {
        for c in r["checks"].as_array_mut().unwrap() {
            c["seconds"] = json!(0); // timing differs
        }
        r["changed"].as_array_mut().unwrap().sort_by_key(|v| v.as_str().map(str::to_owned));
    }
    assert_eq!(got, expected);
    let diff = "+line\n".repeat(3);
    assert_eq!(Value::from(audit::tier2_prompt(&plan, &report, &diff)), want["t2"]);
    // With a brief the text embeds `research.render(brief)` (Python's); without one the prompts are identical.
    assert_eq!(Value::from(audit::conformance_prompt(&plan, None, &report, &diff)), want["conf_nobrief"]);
    let with = audit::conformance_prompt(&plan, Some("BRIEF-TEXT"), &report, &diff);
    assert!(with.contains("The verified research brief the plan was grounded in:\nBRIEF-TEXT\n"));
    assert_eq!(with.replace("The verified research brief the plan was grounded in:\nBRIEF-TEXT\n", ""), want["conf_nobrief"].as_str().unwrap());
    assert!(want["conf"].as_str().unwrap().contains("The verified research brief the plan was grounded in:"));
}
