//! The flight deck's model: what each agent is doing, read from its real tool calls and nothing else.
//!
//! Two questions the window answers without a transcript:
//! - **When** (lanes): every open thread is a lane of beats, one per action it took, so a glance shows who is
//!   reading, editing, running or stuck.
//! - **Where** (ground): the project's components, and which threads stand on each. Two writers on one component
//!   is a collision, found from the files they really touched, not guessed from their titles.
//!
//! Nothing here is inferred from names or invented: no roles, no stages, no checklists.

use std::path::{Path, PathBuf};

use acp_core::{ToolKind, ToolStatus};

use crate::app::{Message, TabState, ToolCard};
use crate::desk::{Area, Touch};

/// How many beats a lane keeps.
pub const LANE_LEN: usize = 40;

/// One thing an agent did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Beat {
    /// Read or searched; nothing changed.
    Look,
    /// Wrote, deleted or moved a file.
    Edit,
    /// Ran a command.
    Run,
    /// Reasoned between actions.
    Think,
    /// Fetched something from outside.
    Fetch,
    /// A call that failed.
    Failed,
}

impl Beat {
    pub fn label(self) -> &'static str {
        match self {
            Beat::Look => "look",
            Beat::Edit => "edit",
            Beat::Run => "run",
            Beat::Think => "think",
            Beat::Fetch => "fetch",
            Beat::Failed => "failed",
        }
    }
}

/// The beat a tool call is.
pub fn beat_of(card: &ToolCard) -> Beat {
    if card.status == ToolStatus::Failed {
        return Beat::Failed;
    }
    if !card.diffs.is_empty() {
        return Beat::Edit;
    }
    match card.kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Other => Beat::Look,
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Beat::Edit,
        ToolKind::Execute => Beat::Run,
        ToolKind::Think => Beat::Think,
        ToolKind::Fetch => Beat::Fetch,
    }
}

/// The last [`LANE_LEN`] actions, oldest first, each with a few words on what it was. A run of thoughts is one.
pub fn steps(messages: &[Message]) -> Vec<(Beat, String)> {
    let mut out: Vec<(Beat, String)> = Vec::new();
    for m in messages {
        let step = match m {
            Message::Tool(card) => {
                let what = match target_of(card) {
                    Some(t) => clip(&crate::shown(&t), 90),
                    None => card.name.clone(),
                };
                (beat_of(card), format!("{} {what}", beat_of(card).label()))
            }
            Message::Thought(_) => (Beat::Think, "thinking".to_owned()),
            _ => continue,
        };
        if step.0 == Beat::Think && out.last().is_some_and(|(b, _)| *b == Beat::Think) {
            continue;
        }
        out.push(step);
    }
    let skip = out.len().saturating_sub(LANE_LEN);
    out.split_off(skip)
}

/// The beats of [`steps`] alone.
pub fn beats(messages: &[Message]) -> Vec<Beat> {
    steps(messages).into_iter().map(|(b, _)| b).collect()
}

/// Counts per beat kind, e.g. "5 look · 3 edit · 1 run".
pub fn tally(beats: &[Beat]) -> String {
    const ORDER: [Beat; 6] = [Beat::Look, Beat::Edit, Beat::Run, Beat::Think, Beat::Fetch, Beat::Failed];
    ORDER
        .iter()
        .filter_map(|&kind| {
            let n = beats.iter().filter(|b| **b == kind).count();
            (n > 0).then(|| format!("{n} {}", kind.label()))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What a call is aimed at: the file it changes, the path it reads, or the command it runs.
pub fn target_of(card: &ToolCard) -> Option<String> {
    if let Some(d) = card.diffs.first() {
        return Some(d.path.clone());
    }
    let input = card.raw_input.as_ref()?;
    if let Some(path) = ["path", "file_path", "file"].iter().find_map(|k| input.get(*k).and_then(|v| v.as_str())) {
        return Some(path.to_owned());
    }
    crate::view::summarize_input(input)
}

fn clip(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut s: String = one_line.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// The first sentence of the agent's last reply: what it says it did.
pub fn outcome(messages: &[Message]) -> Option<String> {
    let text = messages.iter().rev().find_map(|m| match m {
        Message::Agent(t) if !t.trim().is_empty() => Some(t.as_str()),
        _ => None,
    })?;
    let plain: String = text.chars().filter(|ch| !matches!(ch, '*' | '`' | '#' | '_')).collect();
    let flat = crate::shown(&plain.split_whitespace().collect::<Vec<_>>().join(" "));
    let end = flat.char_indices().find(|(i, ch)| matches!(ch, '.' | '!' | '?') && flat[i + 1..].starts_with(' ')).map(|(i, _)| i + 1);
    Some(clip(&flat[..end.unwrap_or(flat.len())], 110))
}

/// A thread is ready for you to review when it changed files, nothing waits on you, nothing collides, and its check
/// has not failed.
pub fn ready(item: &crate::work::TriageIn) -> bool {
    matches!(item.state, TabState::Done | TabState::Idle)
        && item.files > 0
        && item.gate_title.is_none()
        && item.overlaps.is_empty()
        && !item.falsification_warning
        && !matches!(item.check, Some((_, crate::desk::CheckState::Failed)))
}

/// One plain sentence for what the thread is doing this moment.
pub fn now_line(messages: &[Message], state: TabState, gate: Option<&str>, cwd: &Path) -> String {
    if let Some(gate) = gate {
        return format!("waiting on you: {}", clip(gate, 80));
    }
    match state {
        TabState::Closed => return "disconnected".into(),
        TabState::Connecting => return "connecting".into(),
        TabState::Done => {
            return match outcome(messages) {
                Some(o) => format!("finished · {o}"),
                None => "finished".into(),
            };
        }
        TabState::Idle => {
            return match (outcome(messages), beats(messages).last()) {
                (Some(o), _) => format!("idle · {o}"),
                (None, Some(last)) => format!("idle · last action: {}", last.label()),
                (None, None) => "idle".into(),
            };
        }
        TabState::Working | TabState::NeedsApproval => {}
    }
    let running = messages.iter().rev().find_map(|m| match m {
        Message::Tool(card) if matches!(card.status, ToolStatus::Pending | ToolStatus::InProgress) => Some(card),
        _ => None,
    });
    let Some(card) = running else {
        return match messages.iter().rev().find(|m| matches!(m, Message::Tool(_) | Message::Thought(_) | Message::Agent(_))) {
            Some(Message::Agent(_)) => "writing a reply".into(),
            _ => "thinking".into(),
        };
    };
    let target = target_of(card).map(|t| {
        let rel = Path::new(&t).strip_prefix(cwd).map(|p| p.to_string_lossy().into_owned()).unwrap_or(t);
        clip(&crate::shown(&rel), 70)
    });
    let verb = match beat_of(card) {
        Beat::Edit => "editing",
        Beat::Run => "running",
        Beat::Fetch => "fetching",
        Beat::Think => "thinking",
        Beat::Look | Beat::Failed => match card.kind {
            ToolKind::Search => "searching",
            _ => "reading",
        },
    };
    match target {
        Some(t) => format!("{verb} {t}"),
        None => format!("{verb} ({})", card.name),
    }
}

/// How the lanes and the ground are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RackViewMode {
    #[default]
    Lanes,
    Ground,
    /// Every thread as a node in its checkout (see [`crate::field`]).
    Field,
}

impl RackViewMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lanes => "Lanes",
            Self::Ground => "Ground",
            Self::Field => "Field",
        }
    }
}

/// A thread standing on a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occupant {
    pub slot: usize,
    pub touch: Touch,
}

impl Occupant {
    fn writes(self) -> bool {
        matches!(self.touch, Touch::Edited | Touch::Working | Touch::Waiting)
    }
}

/// One component of one checkout and who is on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub name: String,
    /// The checkout's folder name, so the same component in two worktrees is two zones.
    pub place: String,
    pub occupants: Vec<Occupant>,
    /// Two or more threads write here.
    pub contested: bool,
}

/// Components with the threads on them, contested first, then the most pressing touch, then by name.
pub fn territory(per_thread: &[(usize, PathBuf, Vec<Area>)]) -> Vec<Zone> {
    let mut zones: Vec<(PathBuf, Zone)> = Vec::new();
    for (slot, cwd, areas) in per_thread {
        for area in areas {
            let occupant = Occupant { slot: *slot, touch: area.touch() };
            match zones.iter_mut().find(|(f, z)| f == cwd && z.name == area.name) {
                Some((_, z)) => z.occupants.push(occupant),
                None => zones.push((
                    cwd.clone(),
                    Zone {
                        name: area.name.clone(),
                        place: cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        occupants: vec![occupant],
                        contested: false,
                    },
                )),
            }
        }
    }
    let mut out: Vec<Zone> = zones
        .into_iter()
        .map(|(_, mut z)| {
            z.occupants.sort_by_key(|o| (o.touch, o.slot));
            z.contested = z.occupants.iter().filter(|o| o.writes()).count() >= 2;
            z
        })
        .collect();
    let rank = |z: &Zone| z.occupants.first().map(|o| o.touch).unwrap_or(Touch::Read);
    out.sort_by(|a, b| b.contested.cmp(&a.contested).then_with(|| rank(a).cmp(&rank(b))).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_core::FileDiff;

    fn card(id: &str, name: &str, kind: ToolKind, status: ToolStatus, input: Option<serde_json::Value>) -> ToolCard {
        ToolCard { id: id.into(), name: name.into(), kind, status, raw_input: input, content: String::new(), diffs: Vec::new() }
    }

    fn tool(kind: ToolKind, status: ToolStatus) -> Message {
        Message::Tool(card("1", "t", kind, status, None))
    }

    fn area(name: &str, touch: Touch) -> Area {
        Area {
            name: name.into(),
            places: vec![crate::desk::Place { path: format!("{name}/x.rs"), touch, added: 0, removed: 0, sensitive: false, shared_threads: Vec::new() }],
        }
    }

    #[test]
    fn a_lane_is_one_beat_per_action_and_a_run_of_thoughts_is_one() {
        let msgs = vec![
            tool(ToolKind::Read, ToolStatus::Completed),
            Message::Thought("a".into()),
            Message::Thought("b".into()),
            tool(ToolKind::Execute, ToolStatus::Completed),
            tool(ToolKind::Execute, ToolStatus::Failed),
            Message::User("ignored".into()),
        ];
        assert_eq!(beats(&msgs), vec![Beat::Look, Beat::Think, Beat::Run, Beat::Failed]);
    }

    #[test]
    fn each_step_says_what_it_was() {
        let read = Message::Tool(card("1", "read_file", ToolKind::Read, ToolStatus::Completed, Some(serde_json::json!({ "path": "src/a.rs" }))));
        let steps = steps(&[read, Message::Thought("x".into())]);
        assert_eq!(steps, vec![(Beat::Look, "look src/a.rs".to_owned()), (Beat::Think, "thinking".to_owned())]);
    }

    #[test]
    fn a_lane_keeps_only_the_latest_beats() {
        let msgs: Vec<Message> = (0..LANE_LEN + 9).map(|_| tool(ToolKind::Edit, ToolStatus::Completed)).collect();
        assert_eq!(beats(&msgs).len(), LANE_LEN);
    }

    #[test]
    fn an_edit_with_a_diff_is_an_edit_even_when_the_tool_says_other() {
        let mut c = card("1", "patch", ToolKind::Other, ToolStatus::Completed, None);
        c.diffs.push(FileDiff { path: "a.rs".into(), old_text: None, new_text: "x".into() });
        assert_eq!(beat_of(&c), Beat::Edit);
    }

    #[test]
    fn now_names_the_running_call_and_its_target_relative_to_the_project() {
        let cwd = Path::new("/repo");
        let reading = Message::Tool(card("1", "read_file", ToolKind::Read, ToolStatus::InProgress, Some(serde_json::json!({ "path": "/repo/src/lib.rs" }))));
        assert_eq!(now_line(&[reading], TabState::Working, None, cwd), "reading src/lib.rs");
        let running = Message::Tool(card("2", "bash", ToolKind::Execute, ToolStatus::Pending, Some(serde_json::json!({ "command": "cargo test" }))));
        assert!(now_line(&[running], TabState::Working, None, cwd).starts_with("running "));
    }

    #[test]
    fn now_puts_a_gate_first_and_says_plainly_when_nothing_runs() {
        let cwd = Path::new("/repo");
        assert_eq!(now_line(&[], TabState::NeedsApproval, Some("run_command · rm -rf x"), cwd), "waiting on you: run_command · rm -rf x");
        assert_eq!(now_line(&[], TabState::Working, None, cwd), "thinking");
        assert_eq!(now_line(&[Message::Agent("hi".into())], TabState::Working, None, cwd), "writing a reply");
        assert_eq!(now_line(&[], TabState::Idle, None, cwd), "idle");
        let said = [Message::Agent("**Fixed** the `parser`. Tests pass now.".into())];
        assert_eq!(now_line(&said, TabState::Done, None, cwd), "finished · Fixed the parser.");
        let done = [tool(ToolKind::Execute, ToolStatus::Completed)];
        assert_eq!(now_line(&done, TabState::Idle, None, cwd), "idle · last action: run");
    }

    #[test]
    fn two_writers_on_one_component_are_a_collision_and_readers_are_not() {
        let repo = PathBuf::from("/repo");
        let zones = territory(&[
            (1, repo.clone(), vec![area("engine", Touch::Edited), area("docs", Touch::Read)]),
            (2, repo.clone(), vec![area("engine", Touch::Working), area("docs", Touch::Edited)]),
            (3, repo.clone(), vec![area("docs", Touch::Read)]),
        ]);
        assert_eq!(zones[0].name, "engine");
        assert!(zones[0].contested);
        assert_eq!(zones[0].occupants.len(), 2);
        let docs = zones.iter().find(|z| z.name == "docs").unwrap();
        assert!(!docs.contested, "one writer and two readers share without conflict");
        assert_eq!(docs.occupants.len(), 3);
    }

    #[test]
    fn the_same_component_in_two_checkouts_is_two_zones() {
        let zones = territory(&[
            (1, PathBuf::from("/a/repo"), vec![area("engine", Touch::Edited)]),
            (2, PathBuf::from("/b/repo-wt"), vec![area("engine", Touch::Edited)]),
        ]);
        assert_eq!(zones.len(), 2);
        assert!(zones.iter().all(|z| !z.contested));
        assert_ne!(zones[0].place, zones[1].place);
    }

    #[test]
    fn tally_of_nothing_is_empty() {
        assert_eq!(tally(&[]), "");
    }

    #[test]
    fn tally_counts_mixed_unordered_input_in_fixed_order() {
        let beats_in = [Beat::Run, Beat::Look, Beat::Look, Beat::Edit, Beat::Look];
        assert_eq!(tally(&beats_in), "3 look · 1 edit · 1 run");
    }

    #[test]
    fn tally_lists_all_six_kinds_in_fixed_order_regardless_of_input_order() {
        let beats_in = [Beat::Failed, Beat::Fetch, Beat::Think, Beat::Run, Beat::Edit, Beat::Look];
        assert_eq!(tally(&beats_in), "1 look · 1 edit · 1 run · 1 think · 1 fetch · 1 failed");
    }

    #[test]
    fn tally_of_a_single_kind_has_no_separator() {
        assert_eq!(tally(&[Beat::Think, Beat::Think]), "2 think");
    }

    #[test]
    fn tally_omits_zero_counts() {
        assert_eq!(tally(&[Beat::Failed]), "1 failed");
        assert_eq!(tally(&[Beat::Look, Beat::Failed, Beat::Failed]), "1 look · 2 failed");
    }
}
