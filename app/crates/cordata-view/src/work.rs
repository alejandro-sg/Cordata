//! The work board: every place an agent works — Cordata's own threads and the other checkouts of the repository
//! (worktrees Claude Code, agy or a person made) — in one list, ordered by what the person has to do about it.
//!
//! Four lanes, most urgent first: **Needs you** (an approval, a stopped engine), **Working** (longest-running first),
//! **To review** (finished while unseen, or uncommitted changes in a checkout nobody is driving) and **Quiet**. A row
//! carries a human title — the thread's own, else the checkout's last commit message, never a generated branch slug —
//! and one line of why it sits where it does. Pure data in, rows out: the app and the terminal draw the same board.

use std::path::{Path, PathBuf};

use crate::app::TabState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    NeedsYou,
    Working,
    Review,
    Quiet,
}

impl Lane {
    pub fn label(self) -> &'static str {
        match self {
            Lane::NeedsYou => "Needs you",
            Lane::Working => "Working",
            Lane::Review => "To review",
            Lane::Quiet => "Quiet",
        }
    }
}

/// One open thread, as the board needs it.
#[derive(Debug, Clone)]
pub struct ThreadIn {
    pub index: usize,
    pub title: String,
    pub folder: PathBuf,
    pub state: TabState,
    /// What the thread waits for: the first pending approval's title, or why it needs the person.
    pub waiting_on: Option<String>,
    pub working_secs: Option<u64>,
    pub active: bool,
}

/// Another checkout of the repository (a linked worktree, or the main one seen from a worktree).
#[derive(Debug, Clone, Default)]
pub struct PlaceIn {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// `Claude`, `Cordata`, `Antigravity` when the checkout looks like an agent's.
    pub agent: Option<String>,
    pub is_main: bool,
    /// Uncommitted files.
    pub changes: usize,
    pub last_subject: Option<String>,
    pub last_age_secs: Option<u64>,
    /// Its branch is already in the default branch: the checkout can go.
    pub merged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Thread(usize),
    Place(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub lane: Lane,
    pub title: String,
    /// Where: a folder or checkout name, empty for the folder the window shows.
    pub place: String,
    /// Why it is in its lane, in a few words: `waiting · Run edit_file`, `working 4m`, `±3 uncommitted · 2h`, `2d`.
    pub detail: String,
    pub agent: Option<String>,
    pub target: Target,
    pub active: bool,
    /// A merged, clean checkout nobody is working in: the board offers to remove it.
    pub can_remove: bool,
    /// The folder the item lives in: a thread's, or the checkout itself.
    pub folder: PathBuf,
    /// Sort key inside the lane, smaller first.
    rank: i64,
}

/// The board, lanes in order. `here` is the folder the window shows.
pub fn board(threads: &[ThreadIn], places: &[PlaceIn], here: &Path) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for t in threads {
        let place = if t.folder == here { String::new() } else { name_of(&t.folder) };
        let (lane, detail, rank) = match t.state {
            TabState::NeedsApproval => (Lane::NeedsYou, t.waiting_on.as_ref().map_or_else(|| "waiting for you".to_owned(), |w| format!("waiting · {w}")), t.index as i64),
            TabState::Closed => (Lane::NeedsYou, "engine stopped".to_owned(), t.index as i64),
            TabState::Working => {
                let secs = t.working_secs.unwrap_or(0);
                (Lane::Working, if secs < 60 { "working".to_owned() } else { format!("working {}", age(secs)) }, -(secs as i64))
            }
            TabState::Done => (Lane::Review, "finished · unread".to_owned(), t.index as i64),
            TabState::Idle | TabState::Connecting => (Lane::Quiet, String::new(), t.index as i64),
        };
        items.push(Item { lane, title: thread_title(&t.title), place, detail, agent: None, target: Target::Thread(t.index), active: t.active, can_remove: false, folder: t.folder.clone(), rank });
    }
    for p in places {
        // A checkout with an open thread is that thread.
        if threads.iter().any(|t| t.folder == p.path) || p.path == here {
            continue;
        }
        let when = p.last_age_secs.map(age);
        let (lane, detail) = if p.changes > 0 {
            (Lane::Review, [Some(format!("±{} uncommitted", p.changes)), when].into_iter().flatten().collect::<Vec<_>>().join(" · "))
        } else if p.merged && !p.is_main {
            (Lane::Quiet, "merged · can be removed".to_owned())
        } else {
            (Lane::Quiet, when.unwrap_or_default())
        };
        let title = p.last_subject.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| humane_branch(p.branch.as_deref().unwrap_or(""), &p.path));
        // The checkout's name in words; the raw branch is in the tooltip.
        let place = if p.is_main { "main checkout".to_owned() } else { humane_branch(p.branch.as_deref().unwrap_or(""), &p.path) };
        // Merged checkouts sink below live ones.
        let rank = p.last_age_secs.map_or(i64::MAX / 2, |a| a as i64) + if p.merged { i64::MAX / 2 } else { 0 };
        let can_remove = p.merged && p.changes == 0 && !p.is_main;
        items.push(Item { lane, title, place, detail, agent: p.agent.clone(), target: Target::Place(p.path.clone()), active: false, can_remove, folder: p.path.clone(), rank });
    }
    items.sort_by(|a, b| a.lane.cmp(&b.lane).then(matches!(a.target, Target::Place(_)).cmp(&matches!(b.target, Target::Place(_)))).then(a.rank.cmp(&b.rank)));
    items
}

/// The same items grouped by checkout: the folder the window shows first, then each other checkout ordered by its
/// most urgent item. Each group is `(label, items)`, items still in lane order. `here_label` names the window's folder.
pub fn by_place(items: Vec<Item>, here: &Path, here_label: &str) -> Vec<(String, Vec<Item>)> {
    let mut groups: Vec<(PathBuf, Vec<Item>)> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|(f, _)| *f == item.folder) {
            Some((_, list)) => list.push(item),
            None => groups.push((item.folder.clone(), vec![item])),
        }
    }
    groups.sort_by_key(|(folder, list)| (folder != here, list.iter().map(|i| i.lane).min()));
    groups
        .into_iter()
        .map(|(folder, list)| {
            let label = if folder == here {
                here_label.to_owned()
            } else {
                list.iter().find(|i| matches!(i.target, Target::Place(_))).map_or_else(|| name_of(&folder), |i| i.place.clone())
            };
            (label, list)
        })
        .collect()
}

/// One line over the board: `1 needs you · 2 working · 3 to review`, or `All quiet`.
pub fn brief(items: &[Item]) -> String {
    let count = |lane| items.iter().filter(|i| i.lane == lane).count();
    let parts: Vec<String> = [(Lane::NeedsYou, "needs you"), (Lane::Working, "working"), (Lane::Review, "to review")]
        .into_iter()
        .filter_map(|(lane, word)| Some(count(lane)).filter(|n| *n > 0).map(|n| format!("{n} {word}")))
        .collect();
    if parts.is_empty() { "All quiet".to_owned() } else { parts.join(" · ") }
}

fn thread_title(title: &str) -> String {
    let t = title.trim();
    if t.is_empty() { "New thread".to_owned() } else { t.to_owned() }
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// `claude/focused-cannon-21e2fc` → `Focused cannon`: the agent prefix and the random suffix say nothing to a person.
pub fn humane_branch(branch: &str, path: &Path) -> String {
    let raw = branch.rsplit('/').next().filter(|b| !b.is_empty()).map_or_else(|| name_of(path), str::to_owned);
    let mut words: Vec<&str> = raw.split(['-', '_']).filter(|w| !w.is_empty()).collect();
    if words.len() > 1 && words.last().is_some_and(|w| w.len() >= 6 && w.chars().all(|c| c.is_ascii_hexdigit())) {
        words.pop();
    }
    let mut text = words.join(" ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    text
}

/// `now`, `4m`, `2h`, `3d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(index: usize, state: TabState, folder: &str) -> ThreadIn {
        ThreadIn { index, title: format!("t{index}"), folder: folder.into(), state, waiting_on: None, working_secs: None, active: index == 0 }
    }

    #[test]
    fn lanes_order_by_what_the_person_must_do() {
        let mut waiting = thread(3, TabState::NeedsApproval, "/r");
        waiting.waiting_on = Some("Run edit_file".into());
        let mut long = thread(1, TabState::Working, "/r");
        long.working_secs = Some(600);
        let mut short = thread(2, TabState::Working, "/r");
        short.working_secs = Some(30);
        let threads = [thread(0, TabState::Idle, "/r"), long, short, waiting, thread(4, TabState::Done, "/r/.claude/worktrees/x")];
        let items = board(&threads, &[], Path::new("/r"));
        let order: Vec<(Lane, &str, &str)> = items.iter().map(|i| (i.lane, i.title.as_str(), i.detail.as_str())).collect();
        assert_eq!(
            order,
            [
                (Lane::NeedsYou, "t3", "waiting · Run edit_file"),
                (Lane::Working, "t1", "working 10m"),
                (Lane::Working, "t2", "working"),
                (Lane::Review, "t4", "finished · unread"),
                (Lane::Quiet, "t0", ""),
            ]
        );
        assert_eq!(items[3].place, "x", "a thread elsewhere says where");
        assert_eq!(brief(&items), "1 needs you · 2 working · 1 to review");
    }

    #[test]
    fn checkouts_read_as_work_not_branch_slugs() {
        let places = [
            PlaceIn { path: "/r/.claude/worktrees/a".into(), branch: Some("claude/focused-cannon-21e2fc".into()), agent: Some("Claude".into()), changes: 3, last_age_secs: Some(7200), ..Default::default() },
            PlaceIn { path: "/r/.claude/worktrees/b".into(), branch: Some("claude/great-meitner-7924a5".into()), agent: Some("Claude".into()), last_subject: Some("fix(ui): stop duplicate labels".into()), last_age_secs: Some(90_000), ..Default::default() },
            PlaceIn { path: "/r/.claude/worktrees/c".into(), branch: Some("claude/open".into()), ..Default::default() },
        ];
        let threads = [thread(0, TabState::Idle, "/r"), thread(1, TabState::Working, "/r/.claude/worktrees/c")];
        let items = board(&threads, &places, Path::new("/r"));
        let a = items.iter().find(|i| i.target == Target::Place("/r/.claude/worktrees/a".into())).unwrap();
        assert_eq!((a.lane, a.title.as_str(), a.place.as_str(), a.detail.as_str(), a.agent.as_deref()), (Lane::Review, "Focused cannon", "Focused cannon", "±3 uncommitted · 2h", Some("Claude")));
        let b = items.iter().find(|i| i.target == Target::Place("/r/.claude/worktrees/b".into())).unwrap();
        assert_eq!((b.lane, b.title.as_str(), b.detail.as_str()), (Lane::Quiet, "fix(ui): stop duplicate labels", "1d"));
        assert!(!items.iter().any(|i| i.target == Target::Place("/r/.claude/worktrees/c".into())), "a checkout with an open thread is that thread");
        let merged = PlaceIn { path: "/r/m".into(), branch: Some("claude/old-one-abcdef".into()), merged: true, last_age_secs: Some(10), ..Default::default() };
        let live = PlaceIn { path: "/r/l".into(), branch: Some("claude/live".into()), last_age_secs: Some(999_999), ..Default::default() };
        let items = board(&[], &[merged, live], Path::new("/r"));
        assert_eq!(items.iter().map(|i| (i.place.as_str(), i.detail.as_str())).collect::<Vec<_>>(), [("Live", "11d"), ("Old one", "merged · can be removed")]);
        assert_eq!(items.iter().map(|i| i.can_remove).collect::<Vec<_>>(), [false, true]);
        assert_eq!(brief(&[]), "All quiet");
    }

    #[test]
    fn by_place_puts_this_folder_first_then_the_most_urgent_checkout() {
        let threads = [thread(0, TabState::Idle, "/r"), thread(1, TabState::NeedsApproval, "/r/wt/b"), thread(2, TabState::Working, "/r")];
        let places = [
            PlaceIn { path: "/r/wt/a".into(), branch: Some("claude/alpha-123abc".into()), last_age_secs: Some(5), ..Default::default() },
            PlaceIn { path: "/r/wt/b".into(), branch: Some("claude/beta".into()), ..Default::default() },
        ];
        let groups = by_place(board(&threads, &places, Path::new("/r")), Path::new("/r"), "Cordata");
        let shape: Vec<(&str, Vec<String>)> = groups.iter().map(|(l, items)| (l.as_str(), items.iter().map(|i| i.title.clone()).collect())).collect();
        assert_eq!(shape, [("Cordata", vec!["t2".to_owned(), "t0".to_owned()]), ("b", vec!["t1".to_owned()]), ("Alpha", vec!["Alpha".to_owned()])]);
    }

    #[test]
    fn branch_names_become_words() {
        assert_eq!(humane_branch("claude/youthful-curie-c03bf5", Path::new("/x")), "Youthful curie");
        assert_eq!(humane_branch("feature/add_login", Path::new("/x")), "Add login");
        assert_eq!(humane_branch("", Path::new("/r/wt-one")), "Wt one");
        assert_eq!(humane_branch("fix-deadbeef", Path::new("/x")), "Fix", "a hex tail after a word goes");
    }
}
