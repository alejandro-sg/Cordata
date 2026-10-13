//! Custom slash commands: a markdown file is a prompt you can run by name.
//!
//! ```text
//! ---
//! description: Review the staged diff
//! argument-hint: [focus]
//! ---
//! Review `git diff --staged` with a focus on $ARGUMENTS. List real bugs only.
//! ```
//!
//! saved as `.cordata/commands/review.md` is `/review security`. Folders namespace (`team/ship.md` is `/team:ship`).
//! The format is Claude Code's, so a project's `.claude/commands` works unchanged. Looked up, later wins:
//!
//! 1. `~/.claude/commands`            (yours, from another tool)
//! 2. `<user dir>/commands`           (yours: `~/.cordata/commands`)
//! 3. `<project>/.claude/commands`
//! 4. `<project>/.cordata/commands`  
//!
//! A command is only ever text sent to the model as the person's own prompt — it cannot run anything by itself, so
//! the permission gate applies to what the agent then does exactly as it would to a prompt typed by hand.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Custom {
    /// What follows the slash: `review`, `team:ship`.
    pub name: String,
    pub description: String,
    /// Shown after the name in the menu (`[focus]`).
    pub hint: String,
    pub body: String,
    /// Where it came from, for the menu: `user` or `project`.
    pub scope: &'static str,
}

/// The directories to read, lowest precedence first.
pub fn dirs(user_dir: &Path, cwd: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push((PathBuf::from(home).join(".claude/commands"), "user"));
    }
    dirs.push((user_dir.join("commands"), "user"));
    dirs.push((cwd.join(".claude/commands"), "project"));
    dirs.push((cwd.join(".cordata").join("commands"), "project"));
    dirs
}

pub fn load(user_dir: &Path, cwd: &Path) -> Vec<Custom> {
    let mut commands: Vec<Custom> = Vec::new();
    for (dir, scope) in dirs(user_dir, cwd) {
        let mut found = Vec::new();
        collect(&dir, &dir, scope, &mut found);
        for command in found {
            match commands.iter_mut().find(|c| c.name == command.name) {
                Some(existing) => *existing = command,
                None => commands.push(command),
            }
        }
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn collect(root: &Path, dir: &Path, scope: &'static str, out: &mut Vec<Custom>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(root, &path, scope, out);
        } else if path.extension().is_some_and(|e| e == "md")
            && let Ok(text) = fs::read_to_string(&path)
            && let Some(name) = name_of(root, &path)
        {
            out.push(parse(&name, &text, scope));
        }
    }
}

/// `review.md` → `review`; `team/ship.md` → `team:ship`. Names that could not be typed as one word are skipped.
fn name_of(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?.with_extension("");
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        && !name.starts_with(['-', ':', '.']);
    valid.then_some(name)
}

fn parse(name: &str, text: &str, scope: &'static str) -> Custom {
    let (front, body) = split_front_matter(text);
    let field = |key: &str| {
        front
            .lines()
            .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')))
            .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
            .unwrap_or_default()
    };
    let mut description = field("description");
    if description.is_empty() {
        // The first line of the prompt says what it does, well enough.
        description = body.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or_default().chars().take(80).collect();
    }
    Custom {
        name: name.to_owned(),
        description,
        hint: field("argument-hint"),
        body: body.trim().to_owned(),
        scope,
    }
}

fn split_front_matter(text: &str) -> (&str, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))
        && let Some(end) = rest.find("\n---")
    {
        let after = &rest[end + 4..];
        return (&rest[..end], after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")).unwrap_or(after));
    }
    ("", text)
}

impl Custom {
    /// The prompt to send for `/name <args>`: `$ARGUMENTS` is everything after the name, `$1`…`$9` its words. A body
    /// that mentions neither gets the arguments appended, so `/review security` never silently drops "security".
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        let words: Vec<&str> = args.split_whitespace().collect();
        let mut used = self.body.contains("$ARGUMENTS");
        let mut text = self.body.replace("$ARGUMENTS", args);
        for n in (1..=9).rev() {
            let token = format!("${n}");
            if text.contains(&token) {
                used = true;
                text = text.replace(&token, words.get(n - 1).copied().unwrap_or(""));
            }
        }
        if !used && !args.is_empty() {
            text.push_str("\n\n");
            text.push_str(args);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn front_matter_gives_the_menu_its_description_and_hint() {
        let c = parse("review", "---\ndescription: Review the diff\nargument-hint: [focus]\n---\nLook at $ARGUMENTS\n", "project");
        assert_eq!((c.description.as_str(), c.hint.as_str(), c.body.as_str()), ("Review the diff", "[focus]", "Look at $ARGUMENTS"));
    }

    #[test]
    fn without_front_matter_the_first_line_describes_it() {
        let c = parse("ship", "# Ship\n\nRun the tests, then commit.\n", "user");
        assert_eq!(c.description, "Run the tests, then commit.");
        assert!(c.body.starts_with("# Ship"));
    }

    #[test]
    fn arguments_fill_the_placeholders_or_follow_the_prompt() {
        let c = parse("x", "Fix $1 in $2, then $ARGUMENTS", "user");
        assert_eq!(c.render("a.rs b.rs"), "Fix a.rs in b.rs, then a.rs b.rs");
        let plain = parse("y", "Review the diff.", "user");
        assert_eq!(plain.render(""), "Review the diff.");
        assert_eq!(plain.render("security"), "Review the diff.\n\nsecurity");
    }

    #[test]
    fn folders_namespace_and_the_project_beats_the_user() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(user.path(), "commands/review.md", "user review");
        write(user.path(), "commands/team/ship.md", "ship it");
        write(project.path(), ".cordata/commands/review.md", "---\ndescription: project review\n---\nproject review");
        write(project.path(), ".claude/commands/lint.md", "lint it");
        write(project.path(), ".cordata/commands/not a command.md", "spaces are not typable");
        let commands = load(user.path(), project.path());
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"team:ship") && names.contains(&"lint"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(' ')), "{names:?}");
        let review = commands.iter().find(|c| c.name == "review").unwrap();
        assert_eq!((review.body.as_str(), review.scope), ("project review", "project"));
        assert_eq!(names.iter().filter(|n| **n == "review").count(), 1);
    }
}
