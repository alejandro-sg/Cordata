//! The plan artifact: the contract the state machine checks the
//! work against. [`from_draft`] turns a planner's draft into a [`Plan`], refusing (never repairing) a draft that
//! fails the checks a schema cannot express. The draft schema itself is checked by hand rather than with a JSON
//! Schema engine: it is small and fixed, and the messages only feed the replan prompt.

use std::collections::{BTreeMap, HashSet};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scope;

pub const CRITERION_TYPES: [&str; 6] = ["command", "test", "file_exists", "file_not_changed", "regex_match", "regex_absent"];
pub const RISKS: [&str; 3] = ["read_only", "edits_files", "destructive"];
pub const ROLLBACK: [&str; 2] = ["rollback_checkpoint", "keep_partial"];
pub const COMPLEXITIES: [&str; 3] = ["low", "medium", "high"];
pub const STEP_STATUSES: [&str; 7] = ["pending", "running", "completed", "failed", "partial", "skipped", "blocked"];
pub const MAX_STEPS: usize = 12;
pub const MAX_CRITERIA: usize = 20;

fn d_type() -> String {
    "command".into()
}
fn d_timeout() -> f64 {
    30.0
}
fn d_risk() -> String {
    "edits_files".into()
}
fn d_rollback() -> String {
    "rollback_checkpoint".into()
}
fn d_status() -> String {
    "pending".into()
}
fn d_complexity() -> String {
    "medium".into()
}
fn d_budget() -> Value {
    json!({"max_usd": 2.0, "max_tokens": null})
}
fn d_version() -> i64 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    #[serde(rename = "type", default = "d_type")]
    pub kind: String,
    pub description: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub expected_exit_code: i64,
    #[serde(default = "d_timeout")]
    pub timeout_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub step_id: String,
    pub name: String,
    pub description: String,
    pub file_scope: Vec<String>,
    #[serde(default = "d_risk")]
    pub risk: String,
    #[serde(default)]
    pub checkpoint_hash: Option<String>,
    #[serde(default = "d_rollback")]
    pub rollback_policy: String,
    #[serde(default = "d_status")]
    pub status: String,
    /// Which worker/model ran it last, and how many handoffs it took — filled by the executor.
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub attempts: i64,
    #[serde(default)]
    pub grounded_in: Vec<String>,
    #[serde(default = "d_complexity")]
    pub complexity: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub last_event_at: Option<f64>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: String,
    pub goal: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<Criterion>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default = "d_budget")]
    pub budget_envelope: Value,
    #[serde(default)]
    pub verifier_commands: Vec<String>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default = "d_version")]
    pub version: i64,
}

/// A proposal to evolve the living spec when an executor encounters ambiguity or missing scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecAmendmentRequest {
    pub step_id: String,
    pub ambiguity_reason: String,
    #[serde(default)]
    pub proposed_scope_additions: Vec<String>,
    #[serde(default)]
    pub proposed_criteria: Vec<Criterion>,
}

/// Outcome of evaluating an amendment request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecAmendmentOutcome {
    pub accepted: bool,
    pub rationale: String,
    pub updated_plan: Option<Plan>,
}

impl Plan {
    pub fn scope_union(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for entry in self.steps.iter().flat_map(|s| s.file_scope.iter()) {
            if !seen.contains(&entry.as_str()) {
                seen.push(entry);
            }
        }
        seen
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("a plan serializes")
    }

    pub fn from_json(data: &Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(data.clone())
    }

    /// Evaluates and applies an amendment to evolve the living specification.
    ///
    /// Ensures proposed scope additions are normalized project-relative paths,
    /// increments the plan version, and updates criteria without discarding completed steps.
    pub fn apply_amendment(&mut self, req: &SpecAmendmentRequest) -> Result<(), String> {
        let Some(step) = self.steps.iter_mut().find(|s| s.step_id == req.step_id) else {
            return Err(format!("step {} not found in plan", req.step_id));
        };

        for path in &req.proposed_scope_additions {
            let norm = path.trim().replace('\\', "/");
            let clean = norm.trim_start_matches("./").to_string();
            if !step.file_scope.contains(&clean) {
                step.file_scope.push(clean);
            }
        }

        for crit in &req.proposed_criteria {
            if !self.acceptance_criteria.iter().any(|c| c.id == crit.id) {
                self.acceptance_criteria.push(crit.clone());
            }
        }

        self.assumptions.push(format!("Living spec amendment on {}: {}", req.step_id, req.ambiguity_reason));
        self.version += 1;
        Ok(())
    }

    /// The plan as a person reads it: Mermaid workflow, budget note, steps, criteria, verifiers, assumptions.
    pub fn render(&self) -> String {
        let mut lines = vec![format!("# Plan {} (v{})", self.plan_id, self.version), String::new(), format!("**Goal:** {}", self.goal)];
        lines.extend(["", "```mermaid", "flowchart TD", "    Start([\"Start\"])"].map(String::from));
        let mut prev = "Start".to_owned();
        for (i, step) in self.steps.iter().enumerate() {
            let i = i + 1;
            lines.push(format!("    {prev} --> Step_{i}[\"{i}. {} ({})\"]", step.name.replace('"', "'"), step.risk));
            prev = format!("Step_{i}");
        }
        lines.push(format!("    {prev} --> Audit([\"Audit & Verify\"])"));
        lines.push("```".into());

        let mut budget = match self.budget_envelope.get("max_usd").and_then(Value::as_f64) {
            Some(usd) => format!("${usd:.2} USD"),
            None => "unbounded".into(),
        };
        if let Some(tokens) = self.budget_envelope.get("max_tokens").and_then(Value::as_i64).filter(|t| *t != 0) {
            budget.push_str(&format!(" · {} tokens", thousands(tokens)));
        }
        lines.extend(["".into(), "> [!NOTE]".into(), format!("> Budget envelope: {budget} | Steps: {} | Target files: {}", self.steps.len(), self.scope_union().len())]);

        lines.extend(["".into(), "## Steps".into()]);
        for (i, step) in self.steps.iter().enumerate() {
            lines.push(format!("{}. [{}] **{}** (`{}`, {}, {}) — {}", i + 1, step.status, step.name, step.step_id, step.risk, step.complexity, step.description));
            let scope = if step.file_scope.is_empty() { "(read only)".to_owned() } else { step.file_scope.join(", ") };
            lines.push(format!("   scope: {scope}"));
            if !step.depends_on.is_empty() {
                lines.push(format!("   depends on: {}", step.depends_on.join(", ")));
            }
            if !step.grounded_in.is_empty() {
                lines.push(format!("   grounded in: {}", step.grounded_in.join(", ")));
            }
        }
        lines.extend(["".into(), "## Acceptance criteria".into()]);
        for c in &self.acceptance_criteria {
            let target = if c.target.is_empty() { String::new() } else { format!(" {}", c.target) };
            lines.push(format!("- `{}` {}{} — {}", c.id, c.kind, target, c.description));
        }
        if !self.verifier_commands.is_empty() {
            lines.extend(["".into(), "## Verifiers".into()]);
            lines.extend(self.verifier_commands.iter().map(|v| format!("- `{v}`")));
        }
        if !self.assumptions.is_empty() {
            lines.extend(["".into(), "## Assumptions".into()]);
            lines.extend(self.assumptions.iter().map(|a| format!("- {a}")));
        }
        lines.join("\n") + "\n"
    }
}

fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// A draft that cannot become a plan. `problems` lists every reason, for the replan prompt.
#[derive(Debug, thiserror::Error)]
#[error("{}", .problems.join("; "))]
pub struct PlanError {
    pub problems: Vec<String>,
}

/// A dependency cycle among `edges` (step id → the ids it depends on), as the ids along it.
fn cycle(edges: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    fn visit(node: &str, edges: &BTreeMap<String, Vec<String>>, state: &mut BTreeMap<String, u8>, path: &mut Vec<String>) -> Option<Vec<String>> {
        state.insert(node.to_owned(), 1);
        path.push(node.to_owned());
        for dep in edges.get(node).into_iter().flatten() {
            if state.get(dep) == Some(&1) {
                let start = path.iter().position(|p| p == dep).unwrap_or(0);
                let mut found = path[start..].to_vec();
                found.push(dep.clone());
                return Some(found);
            }
            if !state.contains_key(dep)
                && let Some(found) = visit(dep, edges, state, path) {
                    return Some(found);
                }
        }
        path.pop();
        state.insert(node.to_owned(), 2);
        None
    }
    let mut state = BTreeMap::new();
    for node in edges.keys() {
        if !state.contains_key(node)
            && let Some(found) = visit(node, edges, &mut state, &mut Vec::new()) {
                return Some(found);
            }
    }
    None
}

fn str_list(value: &Value, path: &str, out: &mut Vec<String>) {
    match value.as_array() {
        None => out.push(format!("{path}: {value} is not of type 'array'")),
        Some(items) => {
            for (i, item) in items.iter().enumerate() {
                if !item.is_string() {
                    out.push(format!("{path}/{i}: {item} is not of type 'string'"));
                }
            }
        }
    }
}

fn want_enum(object: &Value, key: &str, allowed: &[&str], path: &str, out: &mut Vec<String>) {
    if let Some(v) = object.get(key)
        && !v.as_str().is_some_and(|s| allowed.contains(&s)) {
            out.push(format!("{path}/{key}: {v} is not one of {allowed:?}"));
        }
}

fn want_string(object: &Value, key: &str, path: &str, out: &mut Vec<String>) {
    match object.get(key) {
        None => out.push(format!("{path}: '{key}' is a required property")),
        Some(v) if !v.is_string() => out.push(format!("{path}/{key}: {v} is not of type 'string'")),
        _ => {}
    }
}

/// `DRAFT_SCHEMA`, checked by hand. Returns every violation as `path: message`.
fn schema_problems(draft: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match draft.get("goal") {
        None => out.push("(root): 'goal' is a required property".into()),
        Some(g) if g.as_str().is_none_or(str::is_empty) => out.push("goal: must be a non-empty string".into()),
        _ => {}
    }
    match draft.get("acceptance_criteria") {
        None => out.push("(root): 'acceptance_criteria' is a required property".into()),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                out.push("acceptance_criteria: [] should be non-empty".into());
            }
            if items.len() > MAX_CRITERIA {
                out.push(format!("acceptance_criteria: has more than {MAX_CRITERIA} items"));
            }
            for (i, c) in items.iter().enumerate() {
                let path = format!("acceptance_criteria/{i}");
                if !c.is_object() {
                    out.push(format!("{path}: {c} is not of type 'object'"));
                    continue;
                }
                for key in ["id", "description"] {
                    want_string(c, key, &path, &mut out);
                }
                match c.get("type") {
                    None => out.push(format!("{path}: 'type' is a required property")),
                    Some(_) => want_enum(c, "type", &CRITERION_TYPES, &path, &mut out),
                }
                if let Some(t) = c.get("target").filter(|t| !t.is_string()) {
                    out.push(format!("{path}/target: {t} is not of type 'string'"));
                }
                if let Some(e) = c.get("expected_exit_code").filter(|e| !(e.is_i64() || e.is_u64())) {
                    out.push(format!("{path}/expected_exit_code: {e} is not of type 'integer'"));
                }
                if let Some(t) = c.get("timeout_s").filter(|t| !t.is_number()) {
                    out.push(format!("{path}/timeout_s: {t} is not of type 'number'"));
                }
            }
        }
        Some(other) => out.push(format!("acceptance_criteria: {other} is not of type 'array'")),
    }
    match draft.get("steps") {
        None => out.push("(root): 'steps' is a required property".into()),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                out.push("steps: [] should be non-empty".into());
            }
            if items.len() > MAX_STEPS {
                out.push(format!("steps: has more than {MAX_STEPS} items"));
            }
            for (i, s) in items.iter().enumerate() {
                let path = format!("steps/{i}");
                if !s.is_object() {
                    out.push(format!("{path}: {s} is not of type 'object'"));
                    continue;
                }
                for key in ["step_id", "name", "description"] {
                    want_string(s, key, &path, &mut out);
                }
                match s.get("file_scope") {
                    None => out.push(format!("{path}: 'file_scope' is a required property")),
                    Some(v) => str_list(v, &format!("{path}/file_scope"), &mut out),
                }
                want_enum(s, "risk", &RISKS, &path, &mut out);
                want_enum(s, "rollback_policy", &ROLLBACK, &path, &mut out);
                want_enum(s, "complexity", &COMPLEXITIES, &path, &mut out);
                for key in ["grounded_in", "depends_on"] {
                    if let Some(v) = s.get(key) {
                        str_list(v, &format!("{path}/{key}"), &mut out);
                    }
                }
            }
        }
        Some(other) => out.push(format!("steps: {other} is not of type 'array'")),
    }
    for key in ["verifier_commands", "assumptions"] {
        if let Some(v) = draft.get(key) {
            str_list(v, key, &mut out);
        }
    }
    out.iter_mut().for_each(|m| m.truncate(m.char_indices().nth(200).map_or(m.len(), |(i, _)| i)));
    out
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_owned()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// Every reason `draft` is not an acceptable plan for the project at `root`. Empty means acceptable.
///
/// `findings` is the set of verified research finding ids: when given, steps may cite only those in `grounded_in`
/// and at least one step must cite one. `None` leaves citations unchecked.
pub fn problems(draft: &Value, root: &str, findings: Option<&HashSet<String>>) -> Vec<String> {
    if !draft.is_object() {
        return vec!["the plan is not a JSON object".into()];
    }
    let mut out = schema_problems(draft);
    if !out.is_empty() {
        return out;
    }
    let steps = draft["steps"].as_array().expect("schema checked");
    let mut step_ids: HashSet<String> = HashSet::new();
    let mut writable: Vec<String> = Vec::new();
    for step in steps {
        let sid = text(step.get("step_id"));
        if !step_ids.insert(sid.clone()) {
            out.push(format!("duplicate step id {}", scope::py_repr(&sid)));
        }
        let risk = step.get("risk").and_then(Value::as_str).unwrap_or("edits_files");
        let file_scope = strings(step.get("file_scope"));
        if risk != "read_only" && file_scope.is_empty() {
            out.push(format!("step {} changes files but declares no file_scope", scope::py_repr(&sid)));
        }
        for entry in &file_scope {
            if let Some(reason) = scope::valid(entry) {
                out.push(format!("step {}: {reason}", scope::py_repr(&sid)));
            }
        }
        if risk != "read_only" {
            writable.extend(file_scope);
        }
    }
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut cited = 0;
    for step in steps {
        let sid = text(step.get("step_id"));
        let deps = strings(step.get("depends_on"));
        for dep in &deps {
            if *dep == sid {
                out.push(format!("step {} depends on itself", scope::py_repr(&sid)));
            } else if !step_ids.contains(dep) {
                out.push(format!("step {} depends on unknown step {}", scope::py_repr(&sid), scope::py_repr(dep)));
            }
        }
        edges.entry(sid.clone()).or_default().extend(deps.into_iter().filter(|d| *d != sid && step_ids.contains(d)));
        if let Some(findings) = findings {
            for reference in strings(step.get("grounded_in")) {
                if findings.contains(&reference) {
                    cited += 1;
                } else {
                    out.push(format!("step {} cites {}, which is not a verified finding", scope::py_repr(&sid), scope::py_repr(&reference)));
                }
            }
        }
    }
    if let Some(found) = cycle(&edges) {
        out.push(format!("the steps' depends_on form a cycle: {}", found.join(" -> ")));
    }
    if findings.is_some() && cited == 0 {
        out.push("no step cites a verified research finding in grounded_in".into());
    }
    let mut checkable = 0;
    let mut ids: HashSet<String> = HashSet::new();
    for criterion in draft["acceptance_criteria"].as_array().expect("schema checked") {
        let cid = text(criterion.get("id"));
        if !ids.insert(cid.clone()) {
            out.push(format!("duplicate criterion id {}", scope::py_repr(&cid)));
        }
        let ctype = text(criterion.get("type"));
        let target = text(criterion.get("target"));
        let cid = scope::py_repr(&cid);
        match ctype.as_str() {
            "command" | "test" => {
                if target.trim().is_empty() {
                    out.push(format!("criterion {cid} ({ctype}) has no command in target"));
                } else if target.split_whitespace().next().is_some_and(|word| word.contains("::")) {
                    // A test's name is not a command: the shell answers 127 and no edit can ever make it pass.
                    out.push(format!("criterion {cid} ({ctype}) needs a shell command in target (for example `cargo test -p <crate> <test name>`), not the name of a test"));
                } else {
                    checkable += 1;
                }
            }
            "file_exists" | "file_not_changed" => {
                let reason = if target.is_empty() { Some("no path in target".to_owned()) } else { scope::valid(&target) };
                match reason {
                    Some(reason) => out.push(format!("criterion {cid}: {reason}")),
                    None => {
                        checkable += 1;
                        if ctype == "file_not_changed" && scope::names(&target, &writable, root) {
                            out.push(format!("criterion {cid} says {} must not change, but a step's scope allows changing it", scope::py_repr(&target)));
                        }
                    }
                }
            }
            "regex_match" | "regex_absent" => match target.split_once("::") {
                Some((path, pattern)) if !path.is_empty() && !pattern.is_empty() => match Regex::new(pattern) {
                    Err(error) => out.push(format!("criterion {cid}: bad regex ({error})")),
                    Ok(_) => match scope::valid(path) {
                        Some(reason) => out.push(format!("criterion {cid}: {reason}")),
                        None => checkable += 1,
                    },
                },
                _ => out.push(format!("criterion {cid} ({ctype}) needs target 'path::pattern'")),
            },
            _ => {}
        }
    }
    if checkable == 0 {
        out.push("no acceptance criterion can be checked without a model".into());
    }
    for command in strings(draft.get("verifier_commands")) {
        if command.trim().is_empty() {
            out.push("an empty verifier command".into());
        }
    }
    out
}

pub struct Options {
    pub max_usd: f64,
    pub max_tokens: Option<i64>,
    pub plan_id: Option<String>,
    pub version: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_usd: 2.0, max_tokens: None, plan_id: None, version: 1 }
    }
}

fn new_plan_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    format!("plan-{:010x}", hasher.finish() & 0xff_ffff_ffff)
}

/// Validates `draft` and turns it into a [`Plan`] (adds the id, the budget envelope and per-step state).
pub fn from_draft(draft: &Value, root: &str, options: Options, findings: Option<&HashSet<String>>) -> Result<Plan, PlanError> {
    let found = problems(draft, root, findings);
    if !found.is_empty() {
        return Err(PlanError { problems: found });
    }
    let steps = draft["steps"]
        .as_array()
        .expect("validated")
        .iter()
        .map(|s| {
            let risk = s.get("risk").and_then(Value::as_str).unwrap_or("edits_files").to_owned();
            Step {
                step_id: text(s.get("step_id")),
                name: text(s.get("name")),
                description: text(s.get("description")),
                file_scope: if risk != "read_only" { strings(s.get("file_scope")).iter().map(|e| scope::normalize(e)).collect() } else { Vec::new() },
                risk,
                checkpoint_hash: None,
                rollback_policy: s.get("rollback_policy").and_then(Value::as_str).unwrap_or("rollback_checkpoint").to_owned(),
                status: d_status(),
                worker: None,
                attempts: 0,
                grounded_in: strings(s.get("grounded_in")),
                complexity: s.get("complexity").and_then(Value::as_str).unwrap_or("medium").to_owned(),
                depends_on: strings(s.get("depends_on")),
                last_event_at: None,
                blocked_reason: None,
            }
        })
        .collect();
    let criteria = draft["acceptance_criteria"]
        .as_array()
        .expect("validated")
        .iter()
        .map(|c| Criterion {
            id: text(c.get("id")),
            kind: text(c.get("type")),
            description: text(c.get("description")),
            target: text(c.get("target")),
            expected_exit_code: c.get("expected_exit_code").and_then(Value::as_i64).unwrap_or(0),
            timeout_s: c.get("timeout_s").and_then(Value::as_f64).filter(|t| *t != 0.0).unwrap_or(30.0),
        })
        .collect();
    Ok(Plan {
        plan_id: options.plan_id.unwrap_or_else(new_plan_id),
        goal: text(draft.get("goal")),
        acceptance_criteria: criteria,
        steps,
        budget_envelope: json!({"max_usd": options.max_usd, "max_tokens": options.max_tokens}),
        verifier_commands: strings(draft.get("verifier_commands")),
        assumptions: strings(draft.get("assumptions")),
        version: options.version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Value {
        json!({
            "goal": "add mul",
            "acceptance_criteria": [{"id": "c1", "type": "test", "description": "tests pass", "target": "pytest -q"}],
            "steps": [
                {"step_id": "s1", "name": "add", "description": "d", "file_scope": ["./calc//add.py"]},
                {"step_id": "s2", "name": "mul", "description": "d", "file_scope": ["calc/mul.py"], "depends_on": ["s1"], "complexity": "high"}
            ],
            "verifier_commands": ["pytest -q"],
            "assumptions": ["py3.12"]
        })
    }

    fn problems_of(d: &Value) -> Vec<String> {
        problems(d, "/nonexistent", None)
    }

    #[test]
    fn a_good_draft_becomes_a_plan_with_normalized_scopes() {
        let plan = from_draft(&draft(), "/nonexistent", Options { plan_id: Some("p1".into()), ..Default::default() }, None).unwrap();
        assert_eq!(plan.steps[0].file_scope, ["calc/add.py"]);
        assert_eq!(plan.steps[1].depends_on, ["s1"]);
        assert_eq!(plan.budget_envelope, json!({"max_usd": 2.0, "max_tokens": null}));
        assert_eq!(plan.scope_union(), ["calc/add.py", "calc/mul.py"]);
        assert_eq!(Plan::from_json(&plan.to_json()).unwrap(), plan);
        assert!(from_draft(&draft(), "/x", Options::default(), None).unwrap().plan_id.starts_with("plan-"));
    }

    #[test]
    fn refusals() {
        let mut d = draft();
        d["steps"][0]["file_scope"] = json!([]);
        assert_eq!(problems_of(&d), ["step 's1' changes files but declares no file_scope"]);
        d["steps"][0]["risk"] = json!("read_only");
        assert!(problems_of(&d).is_empty(), "read-only steps need no scope");

        let mut d = draft();
        d["steps"][0]["file_scope"] = json!(["/etc/x", "../y", ".env", "ok.py"]);
        assert_eq!(problems_of(&d).len(), 3);

        let mut d = draft();
        d["steps"][1]["step_id"] = json!("s1");
        assert!(problems_of(&d).contains(&"duplicate step id 's1'".to_string()));

        let mut d = draft();
        d["steps"][0]["depends_on"] = json!(["s0", "s1"]);
        let p = problems_of(&d);
        assert!(p.contains(&"step 's1' depends on unknown step 's0'".to_string()));
        assert!(p.contains(&"step 's1' depends on itself".to_string()));

        let mut d = draft();
        d["steps"][0]["depends_on"] = json!(["s2"]);
        assert!(problems_of(&d).iter().any(|m| m == "the steps' depends_on form a cycle: s1 -> s2 -> s1"));
    }

    #[test]
    fn criteria_checks() {
        let mut d = draft();
        d["acceptance_criteria"] = json!([{"id":"a","type":"file_not_changed","description":"x","target":"calc/add.py"}]);
        assert!(problems_of(&d).iter().any(|m| m.contains("must not change, but a step's scope allows changing it")));

        let mut d = draft();
        d["acceptance_criteria"] = json!([{"id":"a","type":"regex_absent","description":"gone","target":"calc::Concierge"}]);
        assert!(problems_of(&d).is_empty(), "absence is checkable without a model: {:?}", problems_of(&d));
        d["acceptance_criteria"][0]["target"] = json!("calc");
        assert!(problems_of(&d).iter().any(|m| m.contains("(regex_absent) needs target 'path::pattern'")));

        let mut d = draft();
        d["acceptance_criteria"] = json!([{"id":"a","type":"test","description":"x","target":"retry::tests::it_retries"}]);
        assert!(problems_of(&d).iter().any(|m| m.contains("needs a shell command in target")));

        // A Rust step tolerates cargo rewriting the lockfile; promising it stays unchanged is not a contradiction.
        let mut d = draft();
        d["steps"][0]["file_scope"] = json!(["crates/x/src/lib.rs"]);
        d["acceptance_criteria"] = json!([{"id":"a","type":"file_not_changed","description":"x","target":"Cargo.lock"}]);
        assert!(!problems_of(&d).iter().any(|m| m.contains("must not change")), "{:?}", problems_of(&d));
        d["acceptance_criteria"] = json!([{"id":"a","type":"regex_match","description":"x","target":"f.py::("}]);
        let p = problems_of(&d);
        assert!(p[0].starts_with("criterion 'a': bad regex"), "{p:?}");
        assert!(p.contains(&"no acceptance criterion can be checked without a model".to_string()));
        d["acceptance_criteria"] = json!([{"id":"a","type":"regex_match","description":"x","target":"nopattern"}]);
        assert!(problems_of(&d)[0].contains("needs target 'path::pattern'"));
        d["acceptance_criteria"] = json!([{"id":"a","type":"command","description":"x"},{"id":"a","type":"test","description":"x","target":"t"}]);
        let p = problems_of(&d);
        assert!(p.contains(&"criterion 'a' (command) has no command in target".to_string()));
        assert!(p.contains(&"duplicate criterion id 'a'".to_string()));
        d["verifier_commands"] = json!(["  "]);
        assert!(problems_of(&d).contains(&"an empty verifier command".to_string()));
    }

    #[test]
    fn research_grounding_is_enforced_only_with_findings() {
        let mut d = draft();
        let findings: HashSet<String> = ["f1".to_string()].into();
        assert_eq!(problems(&d, "/x", Some(&findings)), ["no step cites a verified research finding in grounded_in"]);
        d["steps"][0]["grounded_in"] = json!(["f1", "f9"]);
        assert_eq!(problems(&d, "/x", Some(&findings)), ["step 's1' cites 'f9', which is not a verified finding"]);
        assert!(problems(&d, "/x", None).is_empty());
    }

    #[test]
    fn schema_violations_short_circuit_the_semantic_checks() {
        assert_eq!(problems_of(&json!([1])), ["the plan is not a JSON object"]);
        let p = problems_of(&json!({"goal": "", "steps": [{"step_id": 1}], "acceptance_criteria": []}));
        assert!(p.iter().any(|m| m.starts_with("goal:")) && p.iter().any(|m| m.starts_with("acceptance_criteria:")));
        assert!(p.iter().any(|m| m.starts_with("steps/0")));
        let mut d = draft();
        d["steps"][0]["risk"] = json!("yolo");
        assert!(problems_of(&d)[0].starts_with("steps/0/risk"));
    }

    #[test]
    fn render_layout() {
        let mut plan = from_draft(&draft(), "/x", Options { plan_id: Some("p1".into()), max_tokens: Some(1_234_567), ..Default::default() }, None).unwrap();
        plan.steps[1].grounded_in = vec!["f1".into()];
        let text = plan.render();
        assert!(text.starts_with("# Plan p1 (v1)\n\n**Goal:** add mul\n\n```mermaid\nflowchart TD\n    Start([\"Start\"])\n    Start --> Step_1[\"1. add (edits_files)\"]\n"));
        assert!(text.contains("> Budget envelope: $2.00 USD · 1,234,567 tokens | Steps: 2 | Target files: 2"));
        assert!(text.contains("2. [pending] **mul** (`s2`, edits_files, high) — d\n   scope: calc/mul.py\n   depends on: s1\n   grounded in: f1\n"));
        assert!(text.contains("- `c1` test pytest -q — tests pass"));
        assert!(text.ends_with("## Assumptions\n- py3.12\n"));
    }

    #[test]
    fn living_spec_amendment_evolves_plan_without_discarding_steps() {
        let mut plan = from_draft(&draft(), "/x", Options { plan_id: Some("p1".into()), ..Default::default() }, None).unwrap();
        assert_eq!(plan.version, 1);
        assert_eq!(plan.steps[0].file_scope, vec!["calc/add.py"]);

        let req = SpecAmendmentRequest {
            step_id: "s1".into(),
            ambiguity_reason: "add requires helper constants in calc/const.py".into(),
            proposed_scope_additions: vec!["./calc/const.py".into()],
            proposed_criteria: vec![Criterion {
                id: "c2".into(),
                kind: "file_exists".into(),
                description: "const file exists".into(),
                target: "calc/const.py".into(),
                expected_exit_code: 0,
                timeout_s: 10.0,
            }],
        };

        assert!(plan.apply_amendment(&req).is_ok());
        assert_eq!(plan.version, 2);
        assert_eq!(plan.steps[0].file_scope, vec!["calc/add.py", "calc/const.py"]);
        assert!(plan.acceptance_criteria.iter().any(|c| c.id == "c2"));
        assert!(plan.assumptions.iter().any(|a| a.contains("calc/const.py")));
    }
}
