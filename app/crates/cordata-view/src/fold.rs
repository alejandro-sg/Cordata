//! A quiet transcript (docs/auto-mode.md §1.1, §1.3): consecutive read/search/run tool cards fold into one activity row,
//! and targets are shown short. Pure: no drawing, no I/O beyond reading `$HOME` in [`Context::here`].

use std::path::{Path, PathBuf};

use acp_core::{ToolKind, ToolStatus};

use crate::app::{Message, ToolCard};

/// One display row of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// `messages[i]`, drawn as today.
    Message(usize),
    /// Several quiet tool calls (and the thoughts and notices between them), drawn as one row.
    Activity(Activity),
}

/// A folded run of quiet messages: `messages[first..=last]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    pub first: usize,
    pub last: usize,
    pub reads: usize,
    pub searches: usize,
    pub runs: usize,
    /// Other read-only calls (`called N tools`).
    pub other: usize,
    pub thoughts: usize,
    /// The newest card is still pending or in progress.
    pub running: bool,
    /// The newest card's target, shortened.
    pub hint: Option<String>,
}

impl Activity {
    pub fn tools(&self) -> usize {
        self.reads + self.searches + self.runs + self.other
    }
}

/// What folding needs besides the messages: where paths are shortened from, and which tool calls wait for approval
/// (those flush a group and stay on their own row).
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub cwd: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub waiting: Vec<String>,
}

impl Context {
    /// The session cwd and this process's `$HOME`.
    pub fn here(cwd: impl Into<PathBuf>) -> Context {
        Context { cwd: Some(cwd.into()), home: std::env::var_os("HOME").map(PathBuf::from), waiting: Vec::new() }
    }

    pub fn shorten(&self, text: &str) -> String {
        shorten(text, self.cwd.as_deref(), self.home.as_deref())
    }
}

/// The verb a tool row reads with: `read`, `edit`, `run`, `search` or `call` (matches cordata-app's `transcript::verb`).
pub fn verb(name: &str) -> &'static str {
    match name {
        "read_file" | "code_outline" => "read",
        "edit_file" => "edit",
        "run_command" => "run",
        "code_search" => "search",
        _ => "call",
    }
}

/// Folds with no cwd, no home and nothing waiting for approval.
pub fn rows(messages: &[Message]) -> Vec<Row> {
    rows_with(messages, &Context::default())
}

/// How a quiet card counts in a group, or None when it flushes.
#[derive(Clone, Copy)]
enum Quiet {
    Read,
    Search,
    Run,
    Other,
}

fn quiet(card: &ToolCard, ctx: &Context) -> Option<Quiet> {
    if card.status == ToolStatus::Failed || ctx.waiting.iter().any(|id| *id == card.id) {
        return None;
    }
    match verb(&card.name) {
        "read" => Some(Quiet::Read),
        "search" => Some(Quiet::Search),
        "run" => Some(Quiet::Run),
        "edit" => None,
        // A tool of another name folds only when it says it reads: an MCP lookup, a fetch. Anything that may write
        // (edit, delete, move, execute, other) keeps its own row.
        _ => match card.kind {
            ToolKind::Read | ToolKind::Search | ToolKind::Fetch | ToolKind::Think => Some(Quiet::Other),
            _ => None,
        },
    }
}

/// The transcript as display rows. Rules (docs/auto-mode.md §1.1): consecutive quiet tool cards (read, search, run;
/// not failed, not waiting for approval) merge with the thoughts and notices between them; anything else flushes;
/// a group of one tool stays as plain rows.
pub fn rows_with(messages: &[Message], ctx: &Context) -> Vec<Row> {
    let mut out = Vec::with_capacity(messages.len());
    let mut group: Option<Activity> = None;
    // Thoughts and notices seen after the group's last card: they join it only if another quiet card follows.
    let mut tail: Vec<usize> = Vec::new();

    fn flush(out: &mut Vec<Row>, group: &mut Option<Activity>, tail: &mut Vec<usize>) {
        if let Some(g) = group.take() {
            if g.tools() > 1 {
                out.push(Row::Activity(g));
            } else {
                out.extend((g.first..=g.last).map(Row::Message));
            }
        }
        out.extend(tail.drain(..).map(Row::Message));
    }

    for (i, message) in messages.iter().enumerate() {
        match message {
            Message::Thought(_) | Message::Info(_) if group.is_some() => tail.push(i),
            Message::Tool(card) => match quiet(card, ctx) {
                Some(q) => {
                    let g = group.get_or_insert_with(|| Activity { first: i, ..Activity::default() });
                    for &t in tail.iter() {
                        if matches!(messages[t], Message::Thought(_)) {
                            g.thoughts += 1;
                        }
                    }
                    tail.clear();
                    match q {
                        Quiet::Read => g.reads += 1,
                        Quiet::Search => g.searches += 1,
                        Quiet::Run => g.runs += 1,
                        Quiet::Other => g.other += 1,
                    }
                    g.last = i;
                    g.running = matches!(card.status, ToolStatus::Pending | ToolStatus::InProgress);
                    g.hint = Some(ctx.shorten(&target(card)));
                }
                None => {
                    flush(&mut out, &mut group, &mut tail);
                    out.push(Row::Message(i));
                }
            },
            _ => {
                flush(&mut out, &mut group, &mut tail);
                out.push(Row::Message(i));
            }
        }
    }
    flush(&mut out, &mut group, &mut tail);
    out
}

/// The thing a tool acted on (a command, a path, a pattern), else the tool's own name. Raw: shorten to display.
pub fn target(card: &ToolCard) -> String {
    card.raw_input.as_ref().and_then(crate::view::summarize_input).unwrap_or_else(|| card.name.clone())
}

/// A target as shown (docs/auto-mode.md §1.3): paths under `cwd` relative to it, under `home` as `~/…`, offloaded
/// outputs as `offloaded output N`, and a leading `cd <cwd> && ` (or `cd <cwd>; `) dropped. Display only: the model
/// and the ledger keep the full text.
pub fn shorten(text: &str, cwd: Option<&Path>, home: Option<&Path>) -> String {
    let cwd = cwd.and_then(Path::to_str).map(trim_slash).filter(|c| !c.is_empty());
    let home = home.and_then(Path::to_str).map(trim_slash).filter(|h| !h.is_empty());
    let (dollar, mut body) = match text.strip_prefix("$ ") {
        Some(rest) => ("$ ", rest),
        None => ("", text),
    };
    if let Some(cwd) = cwd {
        body = strip_cd(body, cwd);
    }
    let words: Vec<String> = body.split(' ').map(|word| shorten_word(word, cwd, home)).collect();
    format!("{dollar}{}", words.join(" "))
}

fn trim_slash(p: &str) -> &str {
    if p.len() > 1 { p.trim_end_matches('/') } else { p }
}

fn strip_cd<'a>(body: &'a str, cwd: &str) -> &'a str {
    let Some(rest) = body.strip_prefix("cd ") else { return body };
    let rest = rest.trim_start();
    for quoted in [cwd.to_owned(), format!("{cwd}/"), format!("'{cwd}'"), format!("\"{cwd}\"")] {
        if let Some(after) = rest.strip_prefix(quoted.as_str()) {
            for sep in [" && ", "&& ", ";", "&&"] {
                if let Some(cmd) = after.strip_prefix(sep) {
                    let cmd = cmd.trim_start();
                    if !cmd.is_empty() {
                        return cmd;
                    }
                }
            }
        }
    }
    body
}

/// One space-separated word: a path is shortened, keeping any quote or punctuation around it.
fn shorten_word(word: &str, cwd: Option<&str>, home: Option<&str>) -> String {
    let start = word.find(['/', '~']).unwrap_or(word.len());
    let (lead, rest) = word.split_at(start);
    if rest.is_empty() || !lead.chars().all(|c| matches!(c, '\'' | '"' | '(' | '=' | '<' | '>' | '`')) {
        return word.to_owned();
    }
    let end = rest.trim_end_matches(['\'', '"', ')', ',', ':', ';', '`']).len();
    let (path, trail) = rest.split_at(end);
    match shorten_path(path, cwd, home) {
        Some(short) => format!("{lead}{short}{trail}"),
        None => word.to_owned(),
    }
}

fn shorten_path(path: &str, cwd: Option<&str>, home: Option<&str>) -> Option<String> {
    if let Some(n) = offloaded(path, home) {
        return Some(n);
    }
    if let Some(cwd) = cwd {
        if path == cwd {
            return Some(".".into());
        }
        if let Some(rel) = path.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
            return Some(if rel.is_empty() { ".".into() } else { rel.into() });
        }
    }
    if let Some(home) = home {
        if path == home {
            return Some("~".into());
        }
        if let Some(rel) = path.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
            return Some(format!("~/{rel}"));
        }
    }
    None
}

/// `~/.cordata/offloaded/native-…-16.txt` → `offloaded output 16`.
fn offloaded(path: &str, home: Option<&str>) -> Option<String> {
    const DIR: &str = ".cordata/offloaded/";
    let name = path
        .strip_prefix("~/")
        .or_else(|| home.and_then(|h| path.strip_prefix(h)).and_then(|r| r.strip_prefix('/')))
        .and_then(|r| r.strip_prefix(DIR))
        .filter(|n| !n.is_empty() && !n.contains('/'))?;
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let seq = stem.rsplit_once('-').map(|(_, n)| n).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    Some(match seq {
        Some(n) => format!("offloaded output {n}"),
        None => "offloaded output".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(id: &str, name: &str, status: ToolStatus, input: serde_json::Value) -> Message {
        let kind = match verb(name) {
            "read" => ToolKind::Read,
            "search" => ToolKind::Search,
            "run" => ToolKind::Execute,
            "edit" => ToolKind::Edit,
            _ => ToolKind::Other,
        };
        Message::Tool(ToolCard {
            id: id.into(),
            name: name.into(),
            kind,
            status,
            raw_input: Some(input),
            content: String::new(),
            diffs: Vec::new(),
        })
    }
    fn read(id: &str, path: &str) -> Message {
        tool(id, "read_file", ToolStatus::Completed, json!({ "path": path }))
    }
    fn run(id: &str, cmd: &str) -> Message {
        tool(id, "run_command", ToolStatus::Completed, json!({ "command": cmd }))
    }
    fn search(id: &str, pattern: &str) -> Message {
        tool(id, "code_search", ToolStatus::Completed, json!({ "pattern": pattern }))
    }

    fn activity(rows: &[Row]) -> Vec<&Activity> {
        rows.iter().filter_map(|r| if let Row::Activity(a) = r { Some(a) } else { None }).collect()
    }

    #[test]
    fn fold_groups_quiet_tools_with_thoughts_between() {
        let msgs = vec![
            Message::User("look".into()),
            read("1", "a.rs"),
            Message::Thought("hm".into()),
            search("2", "fn main"),
            Message::Info("note".into()),
            run("3", "cargo test"),
            read("4", "b.rs"),
            Message::Agent("done".into()),
        ];
        let rows = rows(&msgs);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], Row::Message(0));
        assert_eq!(rows[2], Row::Message(7));
        let Row::Activity(a) = &rows[1] else { panic!("{rows:?}") };
        assert_eq!((a.first, a.last), (1, 6));
        assert_eq!((a.reads, a.searches, a.runs, a.other, a.thoughts), (2, 1, 1, 0, 1));
        assert!(!a.running);
        assert_eq!(a.hint.as_deref(), Some("b.rs"));
    }

    #[test]
    fn fold_flushes_on_text_edit_failure_error_and_approval() {
        let msgs = vec![
            read("1", "a"),
            read("2", "b"),
            tool("e", "edit_file", ToolStatus::Completed, json!({ "path": "a" })),
            read("3", "c"),
            read("4", "d"),
            tool("f", "run_command", ToolStatus::Failed, json!({ "command": "false" })),
            read("5", "e"),
            read("6", "f"),
            Message::Error("boom".into()),
            read("7", "g"),
            read("8", "h"),
            tool("w", "run_command", ToolStatus::Pending, json!({ "command": "curl x" })),
            read("9", "i"),
            read("10", "j"),
            Message::User("next".into()),
        ];
        let ctx = Context { waiting: vec!["w".into()], ..Context::default() };
        let rows = rows_with(&msgs, &ctx);
        let groups = activity(&rows);
        assert_eq!(groups.iter().map(|a| (a.first, a.last)).collect::<Vec<_>>(), [(0, 1), (3, 4), (6, 7), (9, 10), (12, 13)]);
        for i in [2, 5, 8, 11, 14] {
            assert!(rows.contains(&Row::Message(i)), "{i} on its own row: {rows:?}");
        }
        // Without the approval, the pending run folds in.
        let groups = activity(&super::rows(&msgs)).into_iter().map(|a| (a.first, a.last)).collect::<Vec<_>>();
        assert!(groups.contains(&(9, 13)), "{groups:?}");
    }

    #[test]
    fn fold_single_tool_group_stays_rows() {
        let msgs = vec![Message::User("u".into()), read("1", "a"), Message::Thought("t".into()), Message::Agent("x".into())];
        assert_eq!(rows(&msgs), (0..4).map(Row::Message).collect::<Vec<_>>());
    }

    #[test]
    fn fold_trailing_thoughts_stay_outside_and_running_hint() {
        let msgs = vec![
            Message::Thought("before".into()),
            read("1", "a"),
            tool("2", "run_command", ToolStatus::InProgress, json!({ "command": "cargo build" })),
            Message::Thought("after".into()),
        ];
        let rows = rows(&msgs);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[0], Row::Message(0));
        assert_eq!(rows[2], Row::Message(3));
        let Row::Activity(a) = &rows[1] else { panic!() };
        assert_eq!((a.first, a.last, a.thoughts, a.runs, a.reads), (1, 2, 0, 1, 1));
        assert!(a.running);
        assert_eq!(a.hint.as_deref(), Some("$ cargo build"));
    }

    #[test]
    fn fold_counts_other_readonly_tools_and_keeps_writers_out() {
        let mut lookup = tool("1", "mcp_lookup", ToolStatus::Completed, json!({ "q": 1 }));
        if let Message::Tool(card) = &mut lookup {
            card.kind = ToolKind::Fetch;
        }
        let msgs = vec![lookup, read("2", "a"), tool("3", "mcp_write", ToolStatus::Completed, json!({}))];
        let rows = rows(&msgs);
        let a = activity(&rows);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].other, a[0].reads, a[0].tools()), (1, 1, 2));
        assert_eq!(rows.last(), Some(&Row::Message(2)));
    }

    #[test]
    fn fold_hint_is_shortened_by_context() {
        let msgs = vec![read("1", "/w/p/a.rs"), run("2", "cd /w/p && ls /h/x")];
        let ctx = Context { cwd: Some("/w/p".into()), home: Some("/h".into()), waiting: vec![] };
        let a = activity(&rows_with(&msgs, &ctx))[0].clone();
        assert_eq!(a.hint.as_deref(), Some("$ ls ~/x"));
    }

    #[test]
    fn shorten_cwd_relative_and_home() {
        let cwd = Some(Path::new("/Users/me/dev/proj/"));
        let home = Some(Path::new("/Users/me"));
        assert_eq!(shorten("/Users/me/dev/proj/src/lib.rs", cwd, home), "src/lib.rs");
        assert_eq!(shorten("/Users/me/dev/proj", cwd, home), ".");
        assert_eq!(shorten("/Users/me/dev/other/x.rs", cwd, home), "~/dev/other/x.rs");
        assert_eq!(shorten("/etc/hosts", cwd, home), "/etc/hosts");
        assert_eq!(shorten("/Users/me/dev/project/x", cwd, home), "~/dev/project/x");
        assert_eq!(shorten("fn main  in /Users/me/dev/proj/src", cwd, home), "fn main  in src");
        assert_eq!(shorten("$ cat '/Users/me/dev/proj/a b'", cwd, home), "$ cat 'a b'");
        assert_eq!(shorten("relative/path.rs", cwd, home), "relative/path.rs");
        assert_eq!(shorten("/Users/me/dev/proj/a.rs", None, None), "/Users/me/dev/proj/a.rs");
    }

    #[test]
    fn shorten_offloaded_outputs() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(shorten("/Users/me/.cordata/offloaded/native-1793689952091-91485-16.txt", None, home), "offloaded output 16");
        assert_eq!(shorten("~/.cordata/offloaded/native-1-2-7.txt", None, None), "offloaded output 7");
        assert_eq!(shorten("/Users/me/.cordata/offloaded/blob", None, home), "offloaded output");
        assert_eq!(
            shorten("$ head -n 50 /Users/me/.cordata/offloaded/native-1-2-3.txt", Some(Path::new("/Users/me/p")), home),
            "$ head -n 50 offloaded output 3"
        );
    }

    #[test]
    fn shorten_strips_cd_cwd_prefix() {
        let cwd = Some(Path::new("/Users/me/dev/Cordata"));
        let home = Some(Path::new("/Users/me"));
        assert_eq!(shorten("$ cd /Users/me/dev/Cordata && cargo test", cwd, home), "$ cargo test");
        assert_eq!(shorten("$ cd /Users/me/dev/Cordata; git status", cwd, home), "$ git status");
        assert_eq!(shorten("cd \"/Users/me/dev/Cordata\" && ls", cwd, home), "ls");
        assert_eq!(shorten("$ cd /Users/me/dev/Cordata/tui && make", cwd, home), "$ cd tui && make");
        assert_eq!(shorten("$ cd /tmp && ls", cwd, home), "$ cd /tmp && ls");
        assert_eq!(shorten("$ cd /Users/me/dev/Cordata", cwd, home), "$ cd .");
    }
}
