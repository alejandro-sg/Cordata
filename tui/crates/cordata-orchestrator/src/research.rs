//! Research brief and deterministic grounding.
//!
//! The research role investigates the codebase before planning, producing a structured brief (findings, constraints,
//! risks, approach, open questions). Findings must be grounded in verifiable evidence: `kind="file"` is `path:line` with
//! a short verbatim quote within ±3 lines; `kind="doc"` must name an existing project file; `kind="command"` counts only
//! when a doc ref names an existing project file. A brief is verified deterministically, with no model call; findings
//! without verifiable evidence are dropped, and a brief where fewer than half survive (or none exist) is refused.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::machine::{Research, Verified};
use crate::scope::py_repr;

pub const RESEARCH_BRIEF: &str = "You are a research worker exploring the repository.\nYour role is read-only: do not edit or create files.\nAll findings must be grounded in evidence.\nCite file evidence as 'path:line' with a short verbatim quote from that location.\nDoc evidence must cite an existing project file path.\nReturn a structured brief conforming to the schema with findings, constraints, risks, approach, and open_questions.";

/// The schema a research worker answers against.
pub fn research_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema", "title": "ResearchBrief", "type": "object", "required": ["findings"],
        "properties": {
            "findings": {"type": "array", "items": {"type": "object", "required": ["id", "claim", "evidence"], "properties": {
                "id": {"type": "string"}, "claim": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "object", "required": ["kind", "ref"], "properties": {
                    "kind": {"type": "string", "enum": ["file", "doc", "command"]}, "ref": {"type": "string"}, "quote": {"type": "string"}}}}}}},
            "constraints": {"type": "array", "items": {"type": "string"}},
            "risks": {"type": "array", "items": {"type": "string"}},
            "approach": {"type": "string"},
            "open_questions": {"type": "array", "items": {"type": "string"}}}})
}

// ---------------------------------------------------------------------------------------------- Python-isms

/// Python's `str()` of a JSON value (`None`/`True`/`False` spelled the Python way; containers as JSON).
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        other => other.to_string(),
    }
}

/// Python's `repr()` of a JSON value, for the `{x!r}` in messages.
fn py_repr_value(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr(s),
        other => py_str(other),
    }
}

/// Python truthiness.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `int(text)` for the forms a line number takes: optional sign, digits with single underscores between them.
fn py_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") || !digits.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    digits.replace('_', "").parse::<i64>().ok().map(|n| sign * n)
}

/// `str.splitlines()`: splits on \n, \r\n, \r, \v, \f, \x1c-\x1e, \x85,  ,  .
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, n)| *n == '\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `os.path.realpath` (non-strict): resolves symlinks in the part that exists and normalises the rest lexically.
fn realpath(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for part in rest.iter().rev() {
                if part == ".." {
                    out.pop();
                } else if part != "." {
                    out.push(part);
                }
            }
            return out;
        }
        match (existing.file_name().map(ToOwned::to_owned), existing.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn join(root: &str, p: &str) -> PathBuf {
    if Path::new(p).is_absolute() { PathBuf::from(p) } else { Path::new(root).join(p) }
}

fn inside(root: &Path, resolved: &Path) -> bool {
    resolved == root || resolved.starts_with(root)
}

fn is_forbidden_path(rel: &str) -> bool {
    let clean = rel.replace('\\', "/");
    clean.trim_matches('/').split('/').filter(|p| !p.is_empty() && *p != ".").any(|p| {
        let template = [".example", ".sample", ".template", ".dist"].iter().any(|s| p.ends_with(s));
        p == ".git" || p == ".env" || (p.starts_with(".env.") && !template) || (p.starts_with(".env") && !template)
    })
}

// ---------------------------------------------------------------------------------------------- verification

/// A doc reference must point to an existing file in the project.
fn check_doc_ref(reference: &Value, root_real: &Path) -> Result<(), String> {
    let Some(text) = reference.as_str().filter(|s| !s.trim().is_empty()) else { return Err("doc ref is empty".into()) };
    let root = root_real.to_string_lossy();
    let mut clean = text.trim().to_owned();
    let mut target = join(&root, &clean);
    if !target.exists() {
        if let Some((candidate, _)) = clean.rsplit_once(':').filter(|(c, _)| !c.is_empty()) {
            let alt = join(&root, candidate);
            if alt.exists() {
                target = alt;
                clean = candidate.to_owned();
            }
        }
    }
    let resolved = realpath(&target);
    let shown = py_repr_value(reference);
    if !inside(root_real, &resolved) {
        return Err(format!("doc ref {shown} escapes root directory"));
    }
    if resolved == root_real {
        return Err(format!("doc ref {shown} is root directory"));
    }
    let rel = resolved.strip_prefix(root_real).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
    if is_forbidden_path(&rel) || is_forbidden_path(&clean) {
        return Err(format!("doc ref {shown} references forbidden path (.env or .git)"));
    }
    if !resolved.exists() {
        return Err(format!("doc ref {shown} does not exist"));
    }
    if !resolved.is_file() {
        return Err(format!("doc ref {shown} is not a file"));
    }
    Ok(())
}

/// `kind="file"` evidence: `path:line`, root-contained, not `.env`/`.git`, line in range, quote within ±3 lines.
fn verify_file_evidence(ev: &Value, root_real: &Path) -> Result<(), String> {
    let reference = ev.get("ref");
    let Some(text) = reference.and_then(Value::as_str).filter(|s| s.contains(':')) else {
        return Err(format!("file ref {} must parse as 'path:line'", py_repr_value(reference.unwrap_or(&Value::Null))));
    };
    let shown = py_repr(text);
    let (path_part, line_part) = text.rsplit_once(':').expect("contains ':'");
    let (path_part, line_part) = (path_part.trim(), line_part.trim());
    if path_part.is_empty() || line_part.is_empty() {
        return Err(format!("file ref {shown} must parse as 'path:line'"));
    }
    let Some(line_num) = py_int(line_part) else { return Err(format!("file ref {shown} line {} is not an integer", py_repr(line_part))) };
    if line_num < 1 {
        return Err(format!("file ref {shown} line {line_num} must be >= 1"));
    }
    let resolved = realpath(&join(&root_real.to_string_lossy(), path_part));
    let path_shown = py_repr(path_part);
    if !inside(root_real, &resolved) {
        return Err(format!("path {path_shown} escapes root directory"));
    }
    if resolved == root_real {
        return Err(format!("path {path_shown} is root directory"));
    }
    let rel = resolved.strip_prefix(root_real).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
    if is_forbidden_path(&rel) || is_forbidden_path(path_part) {
        return Err(format!("path {path_shown} references forbidden path (.env or .git)"));
    }
    if !resolved.exists() {
        return Err(format!("file {path_shown} does not exist"));
    }
    if !resolved.is_file() {
        return Err(format!("path {path_shown} is not a regular file"));
    }
    let content = match std::fs::read(&resolved) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return Err(format!("failed to read file {path_shown}: {e}")),
    };
    let lines = py_splitlines(&content);
    let total = lines.len() as i64;
    if total == 0 {
        return Err(format!("file {path_shown} is empty"));
    }
    if line_num > total {
        return Err(format!("line {line_num} out of range (1..{total}) for {path_shown}"));
    }
    let Some(quote) = ev.get("quote").and_then(Value::as_str) else { return Err(format!("file evidence {shown} missing quote string")) };
    let norm_quote = normalize_ws(quote);
    if norm_quote.is_empty() {
        return Err(format!("file evidence {shown} quote is empty"));
    }
    let (start, end) = ((line_num - 3).max(1), (line_num + 3).min(total));
    let window: Vec<&str> = (start..=end).map(|i| lines[(i - 1) as usize]).collect();
    if window.iter().any(|l| normalize_ws(l).contains(&norm_quote)) {
        return Ok(());
    }
    // The quote may span lines within the ±3 window.
    let joined = window.iter().map(|l| normalize_ws(l)).collect::<Vec<_>>().join(" ");
    if normalize_ws(&joined).contains(&norm_quote) || joined.contains(&norm_quote) {
        return Ok(());
    }
    Err(format!("quote {} not found within ±3 lines of line {line_num} in {path_shown}", py_repr(quote)))
}

/// Verifies `brief` against the project `root`: findings without verifiable evidence are dropped; the brief is refused when
/// none survive or fewer than half do.
pub fn verify_brief(brief: Option<&Value>, root: &str) -> Verified {
    let root_real = realpath(Path::new(root));
    let Some(brief_obj) = brief.filter(|b| b.is_object()) else {
        let kept = match brief {
            None | Some(Value::Null) => json!({}),
            Some(other) => other.clone(),
        };
        return Verified { brief: kept, dropped: vec![], refused: true, reasons: vec!["brief is not a dictionary".into()] };
    };
    let raw = brief_obj.get("findings").and_then(Value::as_array).filter(|a| !a.is_empty());
    let Some(raw) = raw else {
        let mut clean = brief_obj.clone();
        clean["findings"] = json!([]);
        return Verified { brief: clean, dropped: vec![], refused: true, reasons: vec!["no findings in brief".into()] };
    };
    let total = raw.len();
    let (mut survivors, mut dropped, mut reasons): (Vec<Value>, Vec<String>, Vec<String>) = (Vec::new(), Vec::new(), Vec::new());
    for f in raw {
        let Some(finding) = f.as_object() else {
            dropped.push(String::new());
            reasons.push("finding item is not a dictionary".into());
            continue;
        };
        let fid = finding.get("id").map_or(String::new(), py_str);
        let evidence = finding.get("evidence").and_then(Value::as_array).filter(|e| !e.is_empty());
        let Some(evidence) = evidence else {
            reasons.push(format!("finding {} has no evidence", py_repr(&fid)));
            dropped.push(fid);
            continue;
        };
        let has_valid_doc_ref = evidence.iter().any(|e| e.is_object() && e.get("kind").and_then(Value::as_str) == Some("doc") && check_doc_ref(&Value::String(e.get("ref").map_or(String::new(), py_str)), &root_real).is_ok());
        let (mut verifiable, mut ev_reasons): (Vec<Value>, Vec<String>) = (Vec::new(), Vec::new());
        for ev in evidence {
            if !ev.is_object() {
                ev_reasons.push("evidence item is not a dictionary".into());
                continue;
            }
            let kind = ev.get("kind");
            let reference = Value::String(ev.get("ref").map_or(String::new(), py_str));
            let outcome = match kind.and_then(Value::as_str) {
                Some("file") => verify_file_evidence(ev, &root_real),
                Some("doc") => check_doc_ref(&reference, &root_real),
                Some("command") => {
                    // doc and command evidence count as verifiable only when doc refs name an existing project file
                    if has_valid_doc_ref || check_doc_ref(&reference, &root_real).is_ok() {
                        Ok(())
                    } else {
                        Err(format!("command evidence {} is not verifiable without an existing doc ref", py_repr_value(&reference)))
                    }
                }
                _ => Err(format!("unknown evidence kind {}", py_repr_value(kind.unwrap_or(&Value::Null)))),
            };
            match outcome {
                Ok(()) => verifiable.push(ev.clone()),
                Err(e) => ev_reasons.push(e),
            }
        }
        if verifiable.is_empty() {
            let detail = if ev_reasons.is_empty() { "no verifiable evidence".to_owned() } else { ev_reasons.join("; ") };
            reasons.push(format!("finding {} dropped: {detail}", py_repr(&fid)));
            dropped.push(fid);
        } else {
            let mut survivor = finding.clone();
            survivor.insert("evidence".into(), Value::Array(verifiable));
            survivors.push(Value::Object(survivor));
        }
    }
    let n = survivors.len();
    let refused = n == 0 || (n as f64) < total as f64 / 2.0;
    if refused {
        reasons.push(if n == 0 { "refused: all findings were dropped".to_owned() } else { format!("refused: survivors ({n}) are fewer than half of findings ({total})") });
    }
    let mut clean = brief_obj.clone();
    clean["findings"] = Value::Array(survivors);
    Verified { brief: clean, dropped, refused, reasons }
}

// ---------------------------------------------------------------------------------------------- rendering

fn list_section(lines: &mut Vec<String>, title: &str, value: Option<&Value>) {
    if !truthy(value) {
        return;
    }
    lines.push(format!("## {title}"));
    match value {
        Some(Value::String(s)) => lines.push(s.clone()),
        Some(Value::Array(items)) => lines.extend(items.iter().map(|i| format!("- {}", py_str(i)))),
        Some(Value::Object(map)) => lines.extend(map.keys().map(|k| format!("- {k}"))),
        _ => {}
    }
    lines.push(String::new());
}

/// A research brief as markdown, for `research.md`.
pub fn render_brief(brief: &Value) -> String {
    let empty = Map::new();
    let data = brief.as_object().unwrap_or(&empty);
    let mut lines: Vec<String> = vec!["# Research Brief".into(), String::new()];
    if let Some(findings) = data.get("findings").filter(|f| truthy(Some(f))).and_then(Value::as_array) {
        lines.push("## Findings".into());
        for f in findings.iter().filter_map(Value::as_object) {
            let fid = f.get("id").map_or(String::new(), |v| if truthy(Some(v)) { py_str(v) } else { String::new() });
            let claim = f.get("claim").map_or(String::new(), py_str);
            lines.push(format!("- {}{claim}", if fid.is_empty() { String::new() } else { format!("**[{fid}]** ") }));
            for ev in f.get("evidence").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
                let (kind, reference) = (ev.get("kind").map_or(String::new(), py_str), ev.get("ref").map_or(String::new(), py_str));
                match ev.get("quote").filter(|q| truthy(Some(q))) {
                    Some(quote) => lines.push(format!("  - [{kind}] `{reference}`: {}", py_repr_value(quote))),
                    None => lines.push(format!("  - [{kind}] `{reference}`")),
                }
            }
        }
        lines.push(String::new());
    }
    list_section(&mut lines, "Constraints", data.get("constraints"));
    list_section(&mut lines, "Risks", data.get("risks"));
    if let Some(approach) = data.get("approach").filter(|a| truthy(Some(a))) {
        lines.push("## Approach".into());
        match approach {
            Value::String(s) => lines.push(s.clone()),
            Value::Array(items) => lines.extend(items.iter().map(|a| format!("- {}", py_str(a)))),
            _ => {}
        }
        lines.push(String::new());
    }
    list_section(&mut lines, "Open Questions", data.get("open_questions"));
    lines.join("\n").trim_end().to_owned() + "\n"
}

/// The prompt for the research role, optionally with feedback from a refused attempt.
pub fn research_prompt(goal: &str, feedback: &[String]) -> String {
    let mut parts: Vec<String> = vec![format!("Goal: {goal}"), String::new(), RESEARCH_BRIEF.to_owned()];
    if !feedback.is_empty() {
        parts.extend(["".into(), "## Feedback from previous attempt".into()]);
        parts.extend(feedback.iter().map(|i| format!("- {i}")));
        parts.extend(["".into(), "Address the feedback above: ground all findings with verifiable file or doc evidence.".into()]);
    }
    parts.join("\n").trim().to_owned() + "\n"
}

/// The research backend the machine uses: the schema, prompt, evidence check and renderer above.
pub struct FileResearch;

impl Research for FileResearch {
    fn schema(&self) -> Value {
        research_schema()
    }
    fn prompt(&self, goal: &str, feedback: &[String]) -> String {
        research_prompt(goal, feedback)
    }
    fn verify(&self, structured: Option<&Value>, project: &str) -> Verified {
        verify_brief(structured, project)
    }
    fn render(&self, brief: &Value) -> String {
        render_brief(brief)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "line one\nfn   main() {\n    run();\n}\nlast\n").unwrap();
        std::fs::write(root.join("README.md"), "# readme\n").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        (dir, root.to_string_lossy().into_owned())
    }

    fn finding(id: &str, evidence: Value) -> Value {
        json!({"id": id, "claim": format!("claim {id}"), "evidence": evidence})
    }

    #[test]
    fn file_evidence_needs_a_real_line_and_a_quote_near_it() {
        let (_d, root) = project();
        let ok = |ev: Value| verify_file_evidence(&ev, Path::new(&root)).is_ok();
        assert!(ok(json!({"kind": "file", "ref": "src/lib.rs:2", "quote": "fn main() {"})), "whitespace is normalised");
        assert!(ok(json!({"kind": "file", "ref": "src/lib.rs:5", "quote": "run(); }"})), "a quote may span lines in the window");
        assert!(!ok(json!({"kind": "file", "ref": "src/lib.rs:2", "quote": "nowhere"})));
        let err = |ev: Value| verify_file_evidence(&ev, Path::new(&root)).unwrap_err();
        assert_eq!(err(json!({"ref": "src/lib.rs", "quote": "x"})), "file ref 'src/lib.rs' must parse as 'path:line'");
        assert_eq!(err(json!({"ref": "src/lib.rs:x", "quote": "x"})), "file ref 'src/lib.rs:x' line 'x' is not an integer");
        assert_eq!(err(json!({"ref": "src/lib.rs:0", "quote": "x"})), "file ref 'src/lib.rs:0' line 0 must be >= 1");
        assert_eq!(err(json!({"ref": "src/lib.rs:99", "quote": "x"})), "line 99 out of range (1..5) for 'src/lib.rs'");
        assert_eq!(err(json!({"ref": "../etc/passwd:1", "quote": "x"})), "path '../etc/passwd' escapes root directory");
        assert_eq!(err(json!({"ref": ".env:1", "quote": "S"})), "path '.env' references forbidden path (.env or .git)");
        assert_eq!(err(json!({"ref": "src/lib.rs:1"})), "file evidence 'src/lib.rs:1' missing quote string");
        assert_eq!(err(json!({"ref": "src/lib.rs:1", "quote": "  "})), "file evidence 'src/lib.rs:1' quote is empty");
        assert!(err(json!({"ref": "src/nope.rs:1", "quote": "x"})).contains("does not exist"));
        assert!(err(json!({"ref": "src:1", "quote": "x"})).contains("not a regular file"));
    }

    #[test]
    fn findings_without_evidence_are_dropped_and_a_weak_brief_is_refused() {
        let (_d, root) = project();
        let good = finding("f1", json!([{"kind": "file", "ref": "src/lib.rs:3", "quote": "run();"}]));
        let doc = finding("f2", json!([{"kind": "doc", "ref": "README.md"}]));
        let cmd_only = finding("f3", json!([{"kind": "command", "ref": "cargo test"}]));
        let cmd_with_doc = finding("f4", json!([{"kind": "command", "ref": "cargo test"}, {"kind": "doc", "ref": "README.md:1"}]));
        let none = finding("f5", json!([]));
        let v = verify_brief(Some(&json!({"findings": [good, doc, cmd_only, cmd_with_doc, none, 7], "approach": "a"})), &root);
        assert_eq!(v.dropped, ["f3", "f5", ""]);
        assert!(v.refused == false, "3 of 6 survive: exactly half is enough");
        let ids: Vec<&str> = v.brief["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["f1", "f2", "f4"]);
        assert_eq!(v.brief["approach"], "a");
        assert_eq!(v.reasons[0], "finding 'f3' dropped: command evidence 'cargo test' is not verifiable without an existing doc ref");
        assert!(v.reasons.contains(&"finding 'f5' has no evidence".to_owned()) && v.reasons.contains(&"finding item is not a dictionary".to_owned()));
        let weak = verify_brief(Some(&json!({"findings": [finding("a", json!([])), finding("b", json!([])), finding("c", json!([{"kind": "doc", "ref": "README.md"}]))]})), &root);
        assert!(weak.refused && weak.reasons.last().unwrap() == "refused: survivors (1) are fewer than half of findings (3)");
        let all = verify_brief(Some(&json!({"findings": [finding("a", json!([]))]})), &root);
        assert!(all.refused && all.reasons.last().unwrap() == "refused: all findings were dropped");
    }

    #[test]
    fn malformed_briefs_are_refused_with_a_reason() {
        let (_d, root) = project();
        let not_dict = verify_brief(Some(&json!([1])), &root);
        assert_eq!((not_dict.refused, not_dict.reasons, not_dict.brief), (true, vec!["brief is not a dictionary".to_owned()], json!([1])));
        assert_eq!(verify_brief(None, &root).brief, json!({}));
        let empty = verify_brief(Some(&json!({"findings": [], "risks": ["r"]})), &root);
        assert!(empty.refused && empty.reasons == ["no findings in brief"] && empty.brief == json!({"findings": [], "risks": ["r"]}));
        let unknown = verify_brief(Some(&json!({"findings": [finding("a", json!([{"kind": "web", "ref": "x"}]))]})), &root);
        assert!(unknown.reasons[0].contains("unknown evidence kind 'web'"));
    }

    #[test]
    fn rendering_and_prompts_have_the_documented_shape() {
        let brief = json!({"findings": [{"id": "f1", "claim": "main exists", "evidence": [{"kind": "file", "ref": "src/lib.rs:2", "quote": "it's"}, {"kind": "doc", "ref": "README.md"}]}],
                           "constraints": ["c1"], "risks": "plain risk", "approach": ["step a", "step b"], "open_questions": []});
        assert_eq!(
            render_brief(&brief),
            "# Research Brief\n\n## Findings\n- **[f1]** main exists\n  - [file] `src/lib.rs:2`: \"it's\"\n  - [doc] `README.md`\n\n## Constraints\n- c1\n\n## Risks\nplain risk\n\n## Approach\n- step a\n- step b\n"
        );
        assert_eq!(render_brief(&json!({})), "# Research Brief\n");
        assert_eq!(research_prompt("fix it", &[]), format!("Goal: fix it\n\n{RESEARCH_BRIEF}\n"));
        assert!(research_prompt("g", &["a".into(), "b".into()]).ends_with("## Feedback from previous attempt\n- a\n- b\n\nAddress the feedback above: ground all findings with verifiable file or doc evidence.\n"));
    }

    #[test]
    fn python_flavoured_helpers() {
        assert_eq!([py_int(" 5 "), py_int("+7"), py_int("-3"), py_int("1_0"), py_int("1__0"), py_int("_1"), py_int("")], [Some(5), Some(7), Some(-3), Some(10), None, None, None]);
        assert_eq!(py_splitlines("a\r\nb\rc\u{2028}d\n"), ["a", "b", "c", "d"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\\b\n"), "'a\\\\b\\n'");
        assert!(is_forbidden_path("a/.git/config") && is_forbidden_path(".env.local") && !is_forbidden_path("x/.env.example") && !is_forbidden_path("env.rs"));
    }
}
