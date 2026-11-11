//! Agent detection for the dock terminal: which coding agent is running there and whether it is working, waiting for
//! you, or idle. The rules are data (`detect.toml`, plus your own file beside the prefs), so a new agent or a changed
//! prompt needs a rule, not a release. See `detect.toml` for the format.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

const BUILTIN: &str = include_str!("detect.toml");
/// Rows a `bottom` rule reads when it does not say.
const DEFAULT_LINES: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Working,
    Blocked,
    Idle,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Working => "working",
            State::Blocked => "blocked",
            State::Idle => "idle",
        }
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    agent: Vec<AgentSpec>,
}

#[derive(Deserialize)]
struct AgentSpec {
    id: String,
    name: String,
    #[serde(default)]
    process: Vec<String>,
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

#[derive(Deserialize)]
struct RuleSpec {
    state: State,
    #[serde(default)]
    priority: i32,
    region: String,
    lines: Option<usize>,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    any: Vec<String>,
    #[serde(default)]
    not: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Region {
    Title,
    Bottom(usize),
}

#[derive(Debug)]
struct Rule {
    state: State,
    priority: i32,
    region: Region,
    all: Vec<String>,
    any: Vec<Regex>,
    not: Vec<String>,
}

#[derive(Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    process: Vec<String>,
    rules: Vec<Rule>,
}

#[derive(Debug, Default)]
pub struct Rules {
    agents: Vec<Agent>,
}

/// What is in the terminal now.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    pub title: Option<&'a str>,
    pub lines: &'a [String],
    /// Output arrived just now, not as the echo of a key.
    pub active: bool,
    /// Declared state from OSC 7501, if any.
    pub declared: Option<State>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub id: String,
    pub name: String,
    pub state: State,
    pub declared: bool,
}

fn parse(text: &str, origin: &str) -> Result<Vec<Agent>, String> {
    let file: File = toml::from_str(text).map_err(|e| format!("{origin}: {e}"))?;
    file.agent
        .into_iter()
        .map(|spec| {
            let rules = spec
                .rule
                .into_iter()
                .map(|r| {
                    let region = match r.region.as_str() {
                        "title" => Region::Title,
                        "bottom" => Region::Bottom(r.lines.unwrap_or(DEFAULT_LINES).clamp(1, 200)),
                        other => return Err(format!("{origin}: agent {}: unknown region `{other}` (title or bottom)", spec.id)),
                    };
                    let any = r
                        .any
                        .iter()
                        .map(|p| Regex::new(p).map_err(|e| format!("{origin}: agent {}: bad pattern `{p}`: {e}", spec.id)))
                        .collect::<Result<Vec<_>, _>>()?;
                    let lower = |v: Vec<String>| v.into_iter().map(|s| s.to_lowercase()).collect();
                    Ok(Rule { state: r.state, priority: r.priority, region, all: lower(r.all), any, not: lower(r.not) })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let process = spec.process.iter().map(|p| p.to_lowercase()).collect();
            Ok(Agent { id: spec.id, name: spec.name, process, rules })
        })
        .collect()
}

/// The program a process runs, as rules name it: argv[0]'s file name, or the script's when argv[0] is a runtime
/// (`node …/bin/gemini` is `gemini`). Lower case, without a script extension.
pub fn program_names(argv: &[String]) -> Vec<String> {
    let base = |s: &str| {
        let name = Path::new(s).file_name().and_then(|n| n.to_str()).unwrap_or(s).trim_start_matches('-').to_lowercase();
        ["js", "mjs", "cjs", "ts", "py"].iter().fold(name, |n, ext| n.strip_suffix(&format!(".{ext}")).map_or(n.clone(), str::to_owned))
    };
    let mut names = Vec::new();
    if let Some(first) = argv.first() {
        let program = base(first);
        let runtime = matches!(program.as_str(), "node" | "bun" | "deno" | "python" | "python3" | "npx" | "uv" | "uvx" | "sh" | "bash" | "zsh");
        names.push(program);
        if runtime && let Some(script) = argv.iter().skip(1).find(|a| !a.starts_with('-')) {
            names.push(base(script));
        }
    }
    names
}

impl Rules {
    pub fn builtin() -> Self {
        Self { agents: parse(BUILTIN, "detect.toml").expect("the built-in rules parse (tested)") }
    }

    /// The built-in rules with the person's file over them; a file that does not parse is reported and left out.
    pub fn with_user(path: Option<&Path>) -> (Self, Option<String>) {
        let mut rules = Self::builtin();
        let Some(path) = path else { return (rules, None) };
        let Ok(text) = std::fs::read_to_string(path) else { return (rules, None) };
        match parse(&text, &path.display().to_string()) {
            Ok(mine) => {
                rules.agents.retain(|a| !mine.iter().any(|m| m.id == a.id));
                rules.agents.splice(0..0, mine);
                (rules, None)
            }
            Err(error) => (rules, Some(error)),
        }
    }

    pub fn agent(&self, names: &[String]) -> Option<&Agent> {
        self.agents.iter().find(|a| names.iter().any(|n| a.process.contains(n)))
    }

    /// `agent`'s state in `view`.
    pub fn state(agent: &Agent, view: View<'_>) -> State {
        let title = view.title.unwrap_or_default();
        let mut bottoms: Vec<(usize, String)> = Vec::new();
        let mut text_for = |region: &Region| -> String {
            match region {
                Region::Title => title.to_owned(),
                Region::Bottom(n) => {
                    if let Some((_, text)) = bottoms.iter().find(|(k, _)| k == n) {
                        return text.clone();
                    }
                    let rows: Vec<&str> = view.lines.iter().map(String::as_str).filter(|l| !l.trim().is_empty()).collect();
                    let text = rows[rows.len().saturating_sub(*n)..].join("\n");
                    bottoms.push((*n, text.clone()));
                    text
                }
            }
        };
        let mut best: Option<&Rule> = None;
        for rule in &agent.rules {
            if best.is_some_and(|b| b.priority >= rule.priority) {
                continue;
            }
            let text = text_for(&rule.region);
            let lower = text.to_lowercase();
            let matched = rule.all.iter().all(|s| lower.contains(s))
                && (rule.any.is_empty() || rule.any.iter().any(|re| re.is_match(&text)))
                && !rule.not.iter().any(|s| lower.contains(s));
            if matched {
                best = Some(rule);
            }
        }
        match best {
            Some(rule) => rule.state,
            None if view.active => State::Working,
            None => State::Idle,
        }
    }

    /// Which agent runs in the foreground (its argv) and what it is doing.
    pub fn detect(&self, argv: &[String], view: View<'_>) -> Option<Detection> {
        let agent = self.agent(&program_names(argv))?;
        if let Some(state) = view.declared {
            return Some(Detection { id: agent.id.clone(), name: agent.name.clone(), state, declared: true });
        }
        Some(Detection { id: agent.id.clone(), name: agent.name.clone(), state: Self::state(agent, view), declared: false })
    }
}

/// `~/Library/Application Support/Cordata/detect.toml`, beside the prefs.
pub fn user_file() -> Option<PathBuf> {
    crate::prefs::path().and_then(|p| p.parent().map(|d| d.join("detect.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn argv(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn state(rules: &Rules, program: &str, title: Option<&str>, screen: &str, active: bool) -> Option<State> {
        let rows = lines(screen);
        rules.detect(&argv(&[program]), View { title, lines: &rows, active, declared: None }).map(|d| d.state)
    }

    #[test]
    fn the_builtin_rules_parse() {
        let rules = Rules::builtin();
        assert!(rules.agents.len() >= 6);
        assert!(rules.agents.iter().all(|a| !a.rules.is_empty() && !a.process.is_empty()));
    }

    #[test]
    fn programs_are_named_through_their_runtime() {
        assert_eq!(program_names(&argv(&["/Users/x/.local/bin/claude", "--resume"])), ["claude"]);
        assert_eq!(program_names(&argv(&["node", "--no-warnings", "/opt/homebrew/lib/node_modules/@google/gemini-cli/dist/index.js"])), ["node", "index"]);
        assert_eq!(program_names(&argv(&["node", "/opt/homebrew/bin/gemini"])), ["node", "gemini"]);
        assert_eq!(program_names(&argv(&["-zsh"])), ["zsh"], "a login shell's dash");
        assert_eq!(program_names(&argv(&["/bin/sh", "/opt/wrappers/claude"])), ["sh", "claude"], "a wrapper script");
        assert!(program_names(&[]).is_empty());
        let rules = Rules::builtin();
        assert_eq!(rules.agent(&program_names(&argv(&["node", "/opt/homebrew/bin/gemini"]))).map(|a| a.id.as_str()), Some("gemini"));
        assert!(rules.agent(&program_names(&argv(&["-zsh"]))).is_none(), "a shell is not an agent");
    }

    #[test]
    fn claude_code_working_blocked_and_idle() {
        let rules = Rules::builtin();
        let ask = "╭──────────╮\n│ Bash command │\n  rm -rf target\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n";
        assert_eq!(state(&rules, "claude", Some("✳ Fix tests"), ask, true), Some(State::Blocked), "a question beats the spinner");
        assert_eq!(state(&rules, "claude", Some("⠂ Fix tests"), "> ", false), Some(State::Working), "the title spinner");
        assert_eq!(state(&rules, "claude", None, "✻ Thinking… (12s · esc to interrupt)\n> ", false), Some(State::Working));
        assert_eq!(state(&rules, "claude", Some("Claude Code"), "> \n? for shortcuts", false), Some(State::Idle));
        assert_eq!(state(&rules, "claude", Some("Claude Code"), "> \n? for shortcuts", true), Some(State::Working), "output with no rule: working");
    }

    #[test]
    fn other_agents_and_cordata_itself() {
        let rules = Rules::builtin();
        assert_eq!(state(&rules, "codex", None, "Allow command?\n▌ Yes", false), Some(State::Blocked));
        assert_eq!(state(&rules, "codex", None, "• Working (3s • esc to interrupt)", false), Some(State::Working));
        assert_eq!(state(&rules, "gemini", None, "│ Allow execution of: rm? │", false), Some(State::Blocked));
        assert_eq!(state(&rules, "opencode", None, "△ Permission required", false), Some(State::Blocked));
        assert_eq!(state(&rules, "acp-tui", Some("⚠ fix the parser · cordata"), "", true), Some(State::Blocked));
        assert_eq!(state(&rules, "acp-tui", Some("⠋ fix the parser · cordata"), "", false), Some(State::Working));
        assert_eq!(state(&rules, "acp-tui", Some("✓ fix the parser · cordata"), "", true), Some(State::Idle), "the title says idle, whatever drew");
        assert_eq!(state(&rules, "vim", None, "", true), None);
    }

    #[test]
    fn only_the_bottom_rows_count() {
        let rules = Rules::builtin();
        let mut screen = String::from("Do you want to proceed?\n");
        for i in 0..30 {
            screen.push_str(&format!("line {i}\n"));
        }
        assert_eq!(state(&rules, "claude", None, &screen, false), Some(State::Idle), "an old question scrolled up is not a question");
    }

    #[test]
    fn your_file_replaces_an_agent_or_is_reported() {
        let dir = std::env::temp_dir().join(format!("cordata-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("detect.toml");
        std::fs::write(&file, "[[agent]]\nid = \"claude\"\nname = \"Mine\"\nprocess = [\"claude\"]\n[[agent.rule]]\nstate = \"blocked\"\nregion = \"bottom\"\nall = [\"halt\"]\n").unwrap();
        let (rules, warning) = Rules::with_user(Some(&file));
        assert!(warning.is_none());
        let rows = lines("HALT");
        let found = rules.detect(&argv(&["claude"]), View { title: None, lines: &rows, active: false, declared: None }).unwrap();
        assert_eq!((found.name.as_str(), found.state), ("Mine", State::Blocked));
        std::fs::write(&file, "[[agent]]\nid = \"x\"\nname = \"x\"\n[[agent.rule]]\nstate = \"working\"\nregion = \"middle\"\n").unwrap();
        let (rules, warning) = Rules::with_user(Some(&file));
        assert!(warning.unwrap().contains("unknown region"));
        assert!(rules.agent(&argv(&["claude"])).is_some_and(|a| a.name == "Claude Code"), "the built-in rules stay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn declared_state_beats_screen_rules() {
        let rules = Rules::builtin();
        let rows = lines("Thinking...");
        let found = rules.detect(&argv(&["claude"]), View { title: None, lines: &rows, active: true, declared: Some(State::Blocked) }).unwrap();
        assert_eq!(found.state, State::Blocked);
        assert!(found.declared);
    }
}
