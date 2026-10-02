//! Sub-agent team activity. `local_swarm` / `local_graph` stream their progress
//! to the client as one cumulative text block of lines the backend writes in
//! `acp_server._render_swarm_progress`:
//!
//! ```text
//! started: <node>
//! thinking: <node> - <tail of its reasoning so far>
//! finished: <node> - <its summary>
//! handoff: <from, ...> -> <to, ...> - <message>
//! ```
//!
//! Parsed here into one row per agent (so a card shows who is running, who is
//! done and what each said) instead of the raw log.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Running,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub name: String,
    pub state: NodeState,
    pub summary: String,
    pub thinking: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamRow {
    Node(NodeRow),
    Handoff {
        from: String,
        to: String,
        message: String,
    },
}

pub fn is_team_tool(name: &str) -> bool {
    matches!(name, "local_swarm" | "local_graph")
}

fn split_detail(rest: &str) -> (&str, &str) {
    rest.split_once(" - ").unwrap_or((rest, ""))
}

pub fn parse(progress: &str) -> Vec<TeamRow> {
    let mut rows: Vec<TeamRow> = Vec::new();
    let node_index = |rows: &Vec<TeamRow>, name: &str| {
        rows.iter()
            .position(|row| matches!(row, TeamRow::Node(node) if node.name == name))
    };

    for line in progress.lines() {
        if let Some(name) = line.strip_prefix("started: ") {
            let name = name.trim();
            if node_index(&rows, name).is_none() {
                rows.push(TeamRow::Node(NodeRow {
                    name: name.to_owned(),
                    state: NodeState::Running,
                    summary: String::new(),
                    thinking: String::new(),
                }));
            }
        } else if let Some(rest) = line.strip_prefix("finished: ") {
            let (name, summary) = split_detail(rest);
            let index = node_index(&rows, name).unwrap_or_else(|| {
                rows.push(TeamRow::Node(NodeRow {
                    name: name.to_owned(),
                    state: NodeState::Running,
                    summary: String::new(),
                    thinking: String::new(),
                }));
                rows.len() - 1
            });
            if let TeamRow::Node(node) = &mut rows[index] {
                node.state = NodeState::Done;
                node.summary = summary.to_owned();
                node.thinking.clear();
            }
        } else if let Some(rest) = line.strip_prefix("thinking: ") {
            let (name, tail) = split_detail(rest);
            if let Some(index) = node_index(&rows, name)
                && let TeamRow::Node(node) = &mut rows[index]
                && node.state == NodeState::Running
            {
                node.thinking = tail.to_owned();
            }
        } else if let Some(rest) = line.strip_prefix("handoff: ") {
            let (route, message) = split_detail(rest);
            let (from, to) = route.split_once(" -> ").unwrap_or((route, ""));
            rows.push(TeamRow::Handoff {
                from: from.to_owned(),
                to: to.to_owned(),
                message: message.to_owned(),
            });
        } else if !line.trim().is_empty() {
            // A summary or message that itself contained newlines.
            match rows.last_mut() {
                Some(TeamRow::Node(node)) if node.state == NodeState::Done => {
                    node.summary.push(' ');
                    node.summary.push_str(line.trim());
                }
                Some(TeamRow::Handoff { message, .. }) => {
                    message.push(' ');
                    message.push_str(line.trim());
                }
                _ => {}
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_running_thinking_and_done_nodes() {
        let rows = parse(
            "started: cat_agent\nstarted: dog_agent\nfinished: dog_agent - dogs are loyal\n\nthinking: cat_agent - one short sentence about cats",
        );
        assert_eq!(rows.len(), 2);
        let TeamRow::Node(cat) = &rows[0] else {
            panic!()
        };
        assert_eq!(
            (cat.state, cat.thinking.as_str()),
            (NodeState::Running, "one short sentence about cats")
        );
        let TeamRow::Node(dog) = &rows[1] else {
            panic!()
        };
        assert_eq!(
            (dog.state, dog.summary.as_str()),
            (NodeState::Done, "dogs are loyal")
        );
    }

    #[test]
    fn finishing_clears_thinking_and_late_thinking_is_ignored() {
        let rows = parse("started: a\nthinking: a - hmm\nfinished: a - done\nthinking: a - stale");
        let TeamRow::Node(a) = &rows[0] else { panic!() };
        assert_eq!(a.state, NodeState::Done);
        assert!(
            a.thinking.is_empty(),
            "a finished node's reasoning is not shown"
        );
    }

    #[test]
    fn parses_handoffs_and_multiline_summaries() {
        let rows = parse(
            "started: a\nfinished: a - first line\nsecond line\nhandoff: a -> b, c - go on\nmore",
        );
        let TeamRow::Node(a) = &rows[0] else { panic!() };
        assert_eq!(a.summary, "first line second line");
        assert_eq!(
            rows[1],
            TeamRow::Handoff {
                from: "a".into(),
                to: "b, c".into(),
                message: "go on more".into()
            }
        );
    }

    #[test]
    fn a_finish_without_a_start_still_creates_the_row() {
        let rows = parse("finished: late - ok");
        assert!(
            matches!(&rows[0], TeamRow::Node(n) if n.name == "late" && n.state == NodeState::Done)
        );
    }

    #[test]
    fn team_tools_are_recognised() {
        assert!(
            is_team_tool("local_swarm") && is_team_tool("local_graph") && !is_team_tool("shell")
        );
    }
}
