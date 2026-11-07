//! The rail's data: three kinds of thing, kept apart because they are different things.
//!
//! - **Threads** — conversations with Cordata's own agent that are open in the window. They keep the order they were
//!   opened in (a row that jumps is a row you misclick); what each is doing shows as a mark, and ⌘J goes to the next
//!   one that wants you.
//! - **Earlier** — this project's saved conversations that are not open. Opening one resumes it as a thread.
//! - **Checkouts** — the repository's other git worktrees (Claude Code, agy or a person made them). They are places
//!   with files, not conversations: opening one switches the window's folder. Named by branch, never by a commit
//!   message, so nothing here reads like a thread.
//!
//! Pure data in, rows out: no drawing, no I/O.

use std::path::{Path, PathBuf};

use crate::app::TabState;

/// What a thread is doing, as the rail marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// An approval or a stopped engine: the person has to act.
    NeedsYou,
    Working,
    /// Finished while the person was elsewhere.
    Unread,
    Idle,
}

/// One open thread, as the rail needs it.
#[derive(Debug, Clone)]
pub struct ThreadIn {
    pub index: usize,
    pub session_id: String,
    pub title: String,
    pub folder: PathBuf,
    pub state: TabState,
    /// What the thread waits for: the first pending approval's title, or why it needs the person.
    pub waiting_on: Option<String>,
    pub working_secs: Option<u64>,
    pub active: bool,
}

/// A saved conversation of this project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedIn {
    pub id: String,
    pub title: String,
    pub prompts: usize,
    /// Seconds since it was last saved.
    pub age_secs: u64,
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
pub struct ThreadRow {
    pub index: usize,
    pub title: String,
    /// Why the mark is what it is, in a few words: `waiting · Run edit_file`, `working 4m`, `finished · unread`.
    pub detail: String,
    pub mark: Mark,
    /// The folder's name when the thread works somewhere other than the window's folder.
    pub elsewhere: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRow {
    pub id: String,
    pub title: String,
    /// `2h · 3 prompts`.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRow {
    pub path: PathBuf,
    /// The branch in words: `Focused cannon`.
    pub name: String,
    pub branch: String,
    pub agent: Option<String>,
    /// `±3 uncommitted · 2h`, `2d`, `merged`.
    pub detail: String,
    /// The last commit's subject, for the tooltip.
    pub subject: Option<String>,
    /// Uncommitted work: worth a look before anything else.
    pub dirty: bool,
    /// A merged, clean checkout nobody is working in: the rail offers to remove it.
    pub can_remove: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rail {
    pub threads: Vec<ThreadRow>,
    pub earlier: Vec<SavedRow>,
    pub checkouts: Vec<CheckoutRow>,
}

/// Builds the rail. `here` is the folder the window shows.
pub fn rail(threads: &[ThreadIn], saved: &[SavedIn], places: &[PlaceIn], here: &Path) -> Rail {
    let thread_rows = threads
        .iter()
        .map(|t| {
            let (mark, detail) = match t.state {
                TabState::NeedsApproval => (Mark::NeedsYou, t.waiting_on.as_ref().map_or_else(|| "waiting for you".to_owned(), |w| format!("waiting · {w}"))),
                TabState::Closed => (Mark::NeedsYou, "engine stopped".to_owned()),
                TabState::Working => {
                    let secs = t.working_secs.unwrap_or(0);
                    (Mark::Working, if secs < 60 { "working".to_owned() } else { format!("working {}", age(secs)) })
                }
                TabState::Done => (Mark::Unread, "finished · unread".to_owned()),
                TabState::Idle | TabState::Connecting => (Mark::Idle, String::new()),
            };
            ThreadRow { index: t.index, title: thread_title(&t.title), detail, mark, elsewhere: (t.folder != here).then(|| name_of(&t.folder)), active: t.active }
        })
        .collect();

    // A saved conversation that is open is that thread, not a second row.
    let earlier = saved
        .iter()
        .filter(|s| s.prompts > 0 && !threads.iter().any(|t| t.session_id == s.id))
        .map(|s| SavedRow {
            id: s.id.clone(),
            title: if s.title.trim().is_empty() { "Untitled".to_owned() } else { s.title.trim().to_owned() },
            detail: format!("{} · {}", age(s.age_secs), if s.prompts == 1 { "1 prompt".to_owned() } else { format!("{} prompts", s.prompts) }),
        })
        .collect();

    // A checkout with an open thread is already in the list above, as that thread's place.
    let mut checkouts: Vec<(i64, CheckoutRow)> = places
        .iter()
        .filter(|p| p.path != here && !threads.iter().any(|t| t.folder == p.path))
        .map(|p| {
            let branch = p.branch.clone().unwrap_or_default();
            let name = if p.is_main { "Main checkout".to_owned() } else { humane_branch(&branch, &p.path) };
            let when = p.last_age_secs.map(age);
            let dirty = p.changes > 0;
            let detail = if dirty {
                [Some(format!("±{} uncommitted", p.changes)), when].into_iter().flatten().collect::<Vec<_>>().join(" · ")
            } else if p.merged && !p.is_main {
                "merged".to_owned()
            } else {
                when.unwrap_or_default()
            };
            // Uncommitted work first, then by recency; merged ones sink.
            let rank = i64::from(!dirty) * (i64::MAX / 4) + p.last_age_secs.map_or(i64::MAX / 8, |a| a as i64) + if p.merged { i64::MAX / 4 } else { 0 };
            let row = CheckoutRow {
                path: p.path.clone(),
                name,
                branch,
                agent: p.agent.clone(),
                detail,
                subject: p.last_subject.clone().filter(|s| !s.trim().is_empty()),
                dirty,
                can_remove: p.merged && !dirty && !p.is_main,
            };
            (rank, row)
        })
        .collect();
    checkouts.sort_by_key(|(rank, _)| *rank);

    Rail { threads: thread_rows, earlier, checkouts: checkouts.into_iter().map(|(_, r)| r).collect() }
}

impl Rail {
    /// One line over the threads — `1 needs you · 2 working · 1 unread` — or nothing when there is nothing to say.
    pub fn brief(&self) -> Option<String> {
        let count = |mark| self.threads.iter().filter(|t| t.mark == mark).count();
        let parts: Vec<String> = [(Mark::NeedsYou, "needs you"), (Mark::Working, "working"), (Mark::Unread, "unread")]
            .into_iter()
            .filter_map(|(mark, word)| Some(count(mark)).filter(|n| *n > 0).map(|n| format!("{n} {word}")))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    pub fn needs_you(&self) -> bool {
        self.threads.iter().any(|t| t.mark == Mark::NeedsYou)
    }

    /// Checkouts with uncommitted work: the one number worth showing while the list is folded.
    pub fn dirty_checkouts(&self) -> usize {
        self.checkouts.iter().filter(|c| c.dirty).count()
    }
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

/// Specialized role assigned to or inferred for an agent in the orchestration topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentRole {
    #[default]
    Worker,
    Supervisor,
    InvariantProbe,
    DomainWorker,
    Synthesizer,
    Reviewer,
}

impl AgentRole {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Worker => "WORKER",
            Self::Supervisor => "SUPERVISOR",
            Self::InvariantProbe => "PROBE",
            Self::DomainWorker => "DOMAIN",
            Self::Synthesizer => "SYNTH",
            Self::Reviewer => "REVIEW",
        }
    }

    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Supervisor => "⌘",
            Self::InvariantProbe => "⌕",
            Self::DomainWorker => "⚡",
            Self::Synthesizer => "⎇",
            Self::Reviewer => "🛡",
            Self::Worker => "⚙",
        }
    }

    pub fn mission(&self) -> &'static str {
        match self {
            Self::Supervisor => "Decompose objectives & govern concurrent waves",
            Self::InvariantProbe => "Continuous invariant testing & falsification probes",
            Self::DomainWorker => "APFS clone sandbox code synthesis & compilation",
            Self::Synthesizer => "APFS clone patch reconciliation & 3-way merge",
            Self::Reviewer => "Security audit, secret scan & zero regression validation",
            Self::Worker => "Autonomous task execution in sandbox",
        }
    }
}

/// Confidence signal for human governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfidenceSignal {
    #[default]
    HighSolid,
    Medium,
    LowHatched,
}

impl ConfidenceSignal {
    pub fn label(&self) -> &'static str {
        match self {
            Self::HighSolid => "high confidence · safe sandbox",
            Self::Medium => "normal · standard review",
            Self::LowHatched => "low confidence · verification gate required",
        }
    }
}

/// Supervisory Canvas view modes for progressive exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RackViewMode {
    #[default]
    Graph,
    Pipeline,
    Tree,
}

impl RackViewMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Graph => "Topology Graph",
            Self::Pipeline => "Pipeline Stages",
            Self::Tree => "Decomposition Tree",
        }
    }
}

/// Multi-agent orchestration pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PipelineStage {
    #[default]
    Plan,
    Probe,
    Gate,
    Synthesize,
    Done,
}

impl PipelineStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "1. Plan & Decompose",
            Self::Probe => "2. Probes & Sandbox",
            Self::Gate => "3. Gate & Review",
            Self::Synthesize => "4. Synthesize & Commit",
            Self::Done => "5. Completed",
        }
    }
}

/// Decomposed sub-task item for progressive exposure.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubTaskItem {
    pub label: String,
    pub done: bool,
    pub in_progress: bool,
}

/// Recent backend tool invocation action telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentActionItem {
    pub tool_name: String,
    pub summary: String,
    pub status: String,
}

/// Directed connection between agents or stages in the orchestration graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    pub from_slot: usize,
    pub to_slot: usize,
    pub label: String,
    pub is_conflict: bool,
    pub active: bool,
}

/// Interactive orchestration topology & multi-agent work graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkGraph {
    pub nodes: Vec<TriageIn>,
    pub edges: Vec<GraphEdge>,
    pub scope_collisions: Vec<(usize, usize, String)>,
    pub active_wave: usize,
    pub high_risk_count: usize,
}

/// One open thread summarized as a flight strip on the Multi-Agent Triage Rack (`⌘0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageIn {
    pub index: usize,
    pub slot: usize,
    pub title: String,
    pub folder: PathBuf,
    pub state: TabState,
    pub active: bool,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    pub check: Option<(String, crate::desk::CheckState)>,
    pub falsification_warning: bool,
    pub gate_title: Option<String>,
    pub gate_signature: Option<String>,
    pub gate_high_risk: bool,
    pub working_secs: Option<u64>,
    pub overlaps: Vec<usize>,
    pub role: AgentRole,
    pub current_action: Option<String>,
    pub confidence: ConfidenceSignal,
    pub wave: usize,
    pub parent_slot: Option<usize>,
    pub stage: PipelineStage,
    pub subtasks: Vec<SubTaskItem>,
    pub recent_actions: Vec<AgentActionItem>,
    pub touched_files: Vec<String>,
}

/// Two or more open threads waiting on the exact same tool call (`tool_name` + canonical target/command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchGate {
    pub signature: String,
    pub title: String,
    pub thread_indices: Vec<usize>,
    pub thread_slots: Vec<usize>,
    pub high_risk: bool,
}

/// Supervisory Flight Strip Rack: groups parallel threads by exception rather than opening order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriageRack {
    pub at_gate: Vec<TriageIn>,
    pub ready: Vec<TriageIn>,
    pub in_sandbox: Vec<TriageIn>,
    pub idle: Vec<TriageIn>,
    pub batch_gates: Vec<BatchGate>,
}

pub fn triage_rack(items: &[TriageIn]) -> TriageRack {
    let mut rack = TriageRack::default();
    let mut by_sig: std::collections::BTreeMap<String, BatchGate> = std::collections::BTreeMap::new();
    for item in items {
        let mut normalized = item.clone();
        normalized.title = thread_title(&item.title);
        match item.state {
            TabState::NeedsApproval | TabState::Closed => {
                if item.state == TabState::NeedsApproval
                    && let Some(sig) = &item.gate_signature
                {
                    let entry = by_sig.entry(sig.clone()).or_insert_with(|| BatchGate {
                        signature: sig.clone(),
                        title: item.gate_title.clone().unwrap_or_else(|| sig.clone()),
                        thread_indices: Vec::new(),
                        thread_slots: Vec::new(),
                        high_risk: false,
                    });
                    entry.thread_indices.push(item.index);
                    entry.thread_slots.push(item.slot);
                    entry.high_risk |= item.gate_high_risk;
                }
                rack.at_gate.push(normalized);
            }
            TabState::Working => rack.in_sandbox.push(normalized),
            TabState::Done => rack.ready.push(normalized),
            TabState::Idle | TabState::Connecting => {
                if item.files > 0 {
                    rack.ready.push(normalized);
                } else {
                    rack.idle.push(normalized);
                }
            }
        }
    }
    rack.batch_gates = by_sig.into_values().filter(|g| g.thread_indices.len() >= 2).collect();
    rack
}

/// Infers the agent's specialization role from its slot and brief/title.
pub fn infer_role(slot: usize, title: &str) -> AgentRole {
    let t = title.to_lowercase();
    if slot == 1 {
        AgentRole::Supervisor
    } else if t.contains("inv") || t.contains("test") || t.contains("probe") {
        AgentRole::InvariantProbe
    } else if t.contains("synth") || t.contains("merge") {
        AgentRole::Synthesizer
    } else if t.contains("review") || t.contains("audit") {
        AgentRole::Reviewer
    } else {
        AgentRole::DomainWorker
    }
}

/// Infers visual confidence signaling (solid high autonomy vs hatched human-gate required).
pub fn infer_confidence(working: bool, has_warning: bool, high_risk: bool) -> ConfidenceSignal {
    if high_risk || has_warning {
        ConfidenceSignal::LowHatched
    } else if working {
        ConfidenceSignal::HighSolid
    } else {
        ConfidenceSignal::Medium
    }
}

/// Infers pipeline stage for multi-agent visual boards.
pub fn infer_stage(state: TabState, gate_title: Option<&str>, role: AgentRole, files: usize) -> PipelineStage {
    if gate_title.is_some() || state == TabState::NeedsApproval {
        PipelineStage::Gate
    } else if state == TabState::Done {
        PipelineStage::Done
    } else if role == AgentRole::Supervisor && files == 0 {
        PipelineStage::Plan
    } else if role == AgentRole::InvariantProbe {
        PipelineStage::Probe
    } else if role == AgentRole::Synthesizer || role == AgentRole::Reviewer || files > 0 {
        PipelineStage::Synthesize
    } else {
        PipelineStage::Probe
    }
}

/// Decomposes an agent's current high-level goal into structured sub-tasks for progressive exposure.
pub fn infer_subtasks(role: AgentRole, files: usize, working: bool, has_gate: bool) -> Vec<SubTaskItem> {
    match role {
        AgentRole::Supervisor => vec![
            SubTaskItem { label: "Decompose goal into modular briefs".into(), done: true, in_progress: false },
            SubTaskItem { label: "Dispatch invariant probes & worker pods".into(), done: true, in_progress: false },
            SubTaskItem { label: "Monitor concurrency & avoid scope collisions".into(), done: working, in_progress: working },
            SubTaskItem { label: "Synthesize APFS clone diffs into change sheet".into(), done: files > 0, in_progress: !working && files == 0 },
        ],
        AgentRole::InvariantProbe => vec![
            SubTaskItem { label: "Scan repository invariants & tests".into(), done: true, in_progress: false },
            SubTaskItem { label: "Execute hermetic verification probes".into(), done: !working, in_progress: working },
            SubTaskItem { label: "Verify falsification boundaries".into(), done: !working && !has_gate, in_progress: has_gate },
        ],
        AgentRole::DomainWorker | AgentRole::Worker => vec![
            SubTaskItem { label: "Target assigned subsystem files".into(), done: true, in_progress: false },
            SubTaskItem { label: "Execute localized implementation changes".into(), done: files > 0, in_progress: working },
            SubTaskItem { label: "Pass build and offline unit tests".into(), done: !working && files > 0, in_progress: !working && files == 0 },
        ],
        AgentRole::Synthesizer => vec![
            SubTaskItem { label: "Collect parallel pod diffs".into(), done: true, in_progress: false },
            SubTaskItem { label: "Resolve overlapping file edits".into(), done: !has_gate, in_progress: has_gate },
            SubTaskItem { label: "Finalize atomic review patch".into(), done: files > 0, in_progress: working },
        ],
        AgentRole::Reviewer => vec![
            SubTaskItem { label: "Audit change against AGENTS.md rules".into(), done: true, in_progress: false },
            SubTaskItem { label: "Verify zero plaintext secrets".into(), done: true, in_progress: false },
            SubTaskItem { label: "Approve wave completion".into(), done: !working, in_progress: working },
        ],
    }
}

/// Extracts recent backend tool actions for live telemetry feeds.
pub fn extract_recent_actions(messages: &[crate::app::Message]) -> Vec<AgentActionItem> {
    messages
        .iter()
        .rev()
        .filter_map(|m| match m {
            crate::app::Message::Tool(t) => {
                let summary = if !t.diffs.is_empty() {
                    t.diffs[0].path.clone()
                } else if let Some(raw) = &t.raw_input {
                    crate::view::summarize_input(raw).unwrap_or_else(|| t.name.clone())
                } else {
                    t.name.clone()
                };
                Some(AgentActionItem {
                    tool_name: t.name.clone(),
                    summary,
                    status: t.status.label().to_owned(),
                })
            }
            _ => None,
        })
        .take(4)
        .collect()
}

/// Builds the interactive multi-agent orchestration graph with roles, dependency edges, and scope collisions.
pub fn build_work_graph(items: &[TriageIn]) -> WorkGraph {
    let mut high_risk = 0usize;
    let mut max_wave = 1usize;

    for item in items {
        if item.gate_high_risk || item.falsification_warning {
            high_risk += 1;
        }
        if item.wave > max_wave {
            max_wave = item.wave;
        }
    }

    let mut edges = Vec::new();
    let mut collisions = Vec::new();

    // 1. Structural orchestration links (supervisor -> workers, probes -> synthesizer)
    for item in items {
        let is_working = item.state == TabState::Working;
        if let Some(parent) = item.parent_slot {
            let parent_working = items.iter().find(|i| i.slot == parent).is_some_and(|p| p.state == TabState::Working);
            edges.push(GraphEdge {
                from_slot: parent,
                to_slot: item.slot,
                label: "spawned".into(),
                is_conflict: false,
                active: is_working || parent_working,
            });
        } else if (item.role == AgentRole::InvariantProbe || item.role == AgentRole::DomainWorker)
            && let Some(sup) = items.iter().find(|i| i.role == AgentRole::Supervisor)
            && sup.slot != item.slot
        {
            edges.push(GraphEdge {
                from_slot: sup.slot,
                to_slot: item.slot,
                label: "probe".into(),
                is_conflict: false,
                active: is_working || sup.state == TabState::Working,
            });
        }
        if item.role == AgentRole::Synthesizer || item.role == AgentRole::Reviewer {
            for prev in items {
                if prev.slot != item.slot && (prev.role == AgentRole::InvariantProbe || prev.role == AgentRole::DomainWorker) {
                    edges.push(GraphEdge {
                        from_slot: prev.slot,
                        to_slot: item.slot,
                        label: "synthesize".into(),
                        is_conflict: false,
                        active: is_working || prev.state == TabState::Working,
                    });
                }
            }
        }
    }

    // 2. Cross-agent scope collisions on same repository
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let a = &items[i];
            let b = &items[j];
            if a.folder == b.folder && a.overlaps.contains(&b.slot) {
                let desc = format!("Scope collision on {}: #{} & #{}", a.folder.display(), a.slot, b.slot);
                collisions.push((a.slot, b.slot, desc));
                edges.push(GraphEdge {
                    from_slot: a.slot,
                    to_slot: b.slot,
                    label: "collision".into(),
                    is_conflict: true,
                    active: a.state == TabState::Working || b.state == TabState::Working,
                });
            }
        }
    }

    WorkGraph {
        nodes: items.to_vec(),
        edges,
        scope_collisions: collisions,
        active_wave: max_wave,
        high_risk_count: high_risk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(index: usize, state: TabState, folder: &str) -> ThreadIn {
        ThreadIn { index, session_id: format!("s{index}"), title: format!("t{index}"), folder: folder.into(), state, waiting_on: None, working_secs: None, active: index == 0 }
    }

    #[test]
    fn threads_keep_the_order_they_were_opened_in() {
        let mut waiting = thread(3, TabState::NeedsApproval, "/r");
        waiting.waiting_on = Some("Run edit_file".into());
        let mut long = thread(1, TabState::Working, "/r");
        long.working_secs = Some(600);
        let threads = [thread(0, TabState::Idle, "/r"), long, thread(2, TabState::Working, "/r"), waiting, thread(4, TabState::Done, "/r/wt/x")];
        let rail = rail(&threads, &[], &[], Path::new("/r"));
        let shape: Vec<(&str, Mark, &str)> = rail.threads.iter().map(|t| (t.title.as_str(), t.mark, t.detail.as_str())).collect();
        assert_eq!(
            shape,
            [
                ("t0", Mark::Idle, ""),
                ("t1", Mark::Working, "working 10m"),
                ("t2", Mark::Working, "working"),
                ("t3", Mark::NeedsYou, "waiting · Run edit_file"),
                ("t4", Mark::Unread, "finished · unread"),
            ]
        );
        assert_eq!(rail.threads[4].elsewhere.as_deref(), Some("x"), "a thread in another folder says where");
        assert_eq!(rail.threads[0].elsewhere, None);
        assert_eq!(rail.brief().as_deref(), Some("1 needs you · 2 working · 1 unread"));
        assert!(rail.needs_you());
    }

    #[test]
    fn a_quiet_rail_says_nothing() {
        let rail = rail(&[thread(0, TabState::Idle, "/r")], &[], &[], Path::new("/r"));
        assert_eq!(rail.brief(), None);
        assert!(!rail.needs_you());
    }

    #[test]
    fn saved_conversations_are_the_ones_not_open_and_not_empty() {
        let saved = [
            SavedIn { id: "s0".into(), title: "open already".into(), prompts: 2, age_secs: 10 },
            SavedIn { id: "a".into(), title: "fix the sidebar".into(), prompts: 3, age_secs: 7200 },
            SavedIn { id: "b".into(), title: "  ".into(), prompts: 1, age_secs: 90_000 },
            SavedIn { id: "c".into(), title: "never asked".into(), prompts: 0, age_secs: 5 },
        ];
        let rail = rail(&[thread(0, TabState::Idle, "/r")], &saved, &[], Path::new("/r"));
        let shape: Vec<(&str, &str, &str)> = rail.earlier.iter().map(|s| (s.id.as_str(), s.title.as_str(), s.detail.as_str())).collect();
        assert_eq!(shape, [("a", "fix the sidebar", "2h · 3 prompts"), ("b", "Untitled", "1d · 1 prompt")]);
    }

    #[test]
    fn checkouts_are_named_by_branch_and_never_by_commit_message() {
        let places = [
            PlaceIn { path: "/r/wt/a".into(), branch: Some("claude/focused-cannon-21e2fc".into()), agent: Some("Claude".into()), changes: 3, last_subject: Some("feat: a thing".into()), last_age_secs: Some(7200), ..Default::default() },
            PlaceIn { path: "/r/wt/b".into(), branch: Some("claude/great-meitner-7924a5".into()), agent: Some("Claude".into()), last_subject: Some("fix(ui): stop duplicate labels".into()), last_age_secs: Some(90_000), ..Default::default() },
            PlaceIn { path: "/r/wt/c".into(), branch: Some("claude/open".into()), ..Default::default() },
            PlaceIn { path: "/r/wt/d".into(), branch: Some("claude/old-one-abcdef".into()), merged: true, last_age_secs: Some(10), ..Default::default() },
            PlaceIn { path: "/r".into(), branch: Some("main".into()), is_main: true, ..Default::default() },
        ];
        let threads = [thread(0, TabState::Idle, "/r/wt/e"), thread(1, TabState::Working, "/r/wt/c")];
        let rail = rail(&threads, &[], &places, Path::new("/r/wt/e"));
        let shape: Vec<(&str, &str, bool, bool)> = rail.checkouts.iter().map(|c| (c.name.as_str(), c.detail.as_str(), c.dirty, c.can_remove)).collect();
        assert_eq!(
            shape,
            [
                ("Focused cannon", "±3 uncommitted · 2h", true, false),
                ("Great meitner", "1d", false, false),
                ("Main checkout", "", false, false),
                ("Old one", "merged", false, true),
            ],
            "uncommitted first, then newest, undated after dated; a checkout with an open thread is not listed twice; merged sinks"
        );
        assert_eq!(rail.checkouts[1].subject.as_deref(), Some("fix(ui): stop duplicate labels"));
        assert_eq!(rail.dirty_checkouts(), 1);
    }

    #[test]
    fn branch_names_become_words() {
        assert_eq!(humane_branch("claude/youthful-curie-c03bf5", Path::new("/x")), "Youthful curie");
        assert_eq!(humane_branch("feature/add_login", Path::new("/x")), "Add login");
        assert_eq!(humane_branch("", Path::new("/r/wt-one")), "Wt one");
        assert_eq!(humane_branch("fix-deadbeef", Path::new("/x")), "Fix", "a hex tail after a word goes");
    }

    #[test]
    fn triage_rack_groups_by_supervisory_band_and_clusters_identical_gates() {
        let strip = |index: usize, state: TabState, sig: Option<&str>, files: usize| TriageIn {
            index,
            slot: index + 1,
            title: format!("task {index}"),
            folder: "/r".into(),
            state,
            active: index == 0,
            files,
            added: files * 4,
            removed: files,
            check: None,
            falsification_warning: false,
            gate_title: sig.map(|s| format!("Run {s}")),
            gate_signature: sig.map(str::to_owned),
            gate_high_risk: false,
            working_secs: Some(30),
            overlaps: Vec::new(),
            role: AgentRole::Worker,
            current_action: None,
            confidence: ConfidenceSignal::HighSolid,
            wave: 1,
            parent_slot: None,
            stage: infer_stage(state, sig, AgentRole::Worker, files),
            subtasks: infer_subtasks(AgentRole::Worker, files, state == TabState::Working, sig.is_some()),
            recent_actions: Vec::new(),
            touched_files: Vec::new(),
        };
        let all = [
            strip(0, TabState::NeedsApproval, Some("run_command:cargo test"), 1),
            strip(1, TabState::Working, None, 0),
            strip(2, TabState::NeedsApproval, Some("run_command:cargo test"), 2),
            strip(3, TabState::Done, None, 3),
            strip(4, TabState::Idle, None, 0),
        ];
        let rack = triage_rack(&all);
        assert_eq!(rack.at_gate.len(), 2);
        assert_eq!(rack.in_sandbox.len(), 1);
        assert_eq!(rack.ready.len(), 1);
        assert_eq!(rack.idle.len(), 1);
        assert_eq!(rack.batch_gates.len(), 1);
        assert_eq!(rack.batch_gates[0].thread_indices, vec![0, 2]);
        assert_eq!(rack.batch_gates[0].thread_slots, vec![1, 3]);
    }

    #[test]
    fn work_graph_builds_edges_and_detects_scope_collisions() {
        let root = TriageIn {
            index: 0,
            slot: 1,
            title: "supervisor task".into(),
            folder: "/repo/crate-a".into(),
            state: TabState::Working,
            active: true,
            files: 2,
            added: 10,
            removed: 0,
            check: None,
            falsification_warning: false,
            gate_title: None,
            gate_signature: None,
            gate_high_risk: false,
            working_secs: Some(15),
            overlaps: Vec::new(),
            role: AgentRole::Supervisor,
            current_action: Some("coordinating wave 1".into()),
            confidence: ConfidenceSignal::HighSolid,
            wave: 1,
            parent_slot: None,
            stage: PipelineStage::Plan,
            subtasks: infer_subtasks(AgentRole::Supervisor, 2, true, false),
            recent_actions: Vec::new(),
            touched_files: vec!["src/lib.rs".into()],
        };
        let probe = TriageIn {
            index: 1,
            slot: 2,
            title: "invariant probe: test harness".into(),
            folder: "/repo/crate-a".into(),
            state: TabState::Working,
            active: false,
            files: 1,
            added: 5,
            removed: 0,
            check: None,
            falsification_warning: false,
            gate_title: None,
            gate_signature: None,
            gate_high_risk: false,
            working_secs: Some(8),
            overlaps: vec![3],
            role: AgentRole::InvariantProbe,
            current_action: Some("testing invariant c6".into()),
            confidence: ConfidenceSignal::HighSolid,
            wave: 1,
            parent_slot: Some(1),
            stage: PipelineStage::Probe,
            subtasks: infer_subtasks(AgentRole::InvariantProbe, 1, true, false),
            recent_actions: Vec::new(),
            touched_files: vec!["tests/harness.rs".into()],
        };
        let worker = TriageIn {
            index: 2,
            slot: 3,
            title: "domain worker: hooks".into(),
            folder: "/repo/crate-a".into(),
            state: TabState::NeedsApproval,
            active: false,
            files: 3,
            added: 20,
            removed: 5,
            check: None,
            falsification_warning: true,
            gate_title: Some("Run edit_file".into()),
            gate_signature: Some("edit_file".into()),
            gate_high_risk: true,
            working_secs: Some(12),
            overlaps: Vec::new(),
            role: AgentRole::DomainWorker,
            current_action: Some("editing hooks.rs".into()),
            confidence: ConfidenceSignal::LowHatched,
            wave: 1,
            parent_slot: Some(1),
            stage: PipelineStage::Gate,
            subtasks: infer_subtasks(AgentRole::DomainWorker, 3, false, true),
            recent_actions: Vec::new(),
            touched_files: vec!["src/hooks.rs".into()],
        };

        let graph = build_work_graph(&[root, probe, worker]);
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.active_wave, 1);
        assert_eq!(graph.high_risk_count, 1);
        // Expect parent-child edges from slot 1 to slot 2 and slot 3, plus 1 conflict edge
        assert_eq!(graph.edges.len(), 3);
        assert!(graph.edges.iter().any(|e| e.from_slot == 1 && e.to_slot == 2));
        assert!(graph.edges.iter().any(|e| e.from_slot == 1 && e.to_slot == 3));
        assert!(graph.edges.iter().any(|e| e.is_conflict));
        // Slots 2 and 3 both share "/repo/crate-a"
        assert!(!graph.scope_collisions.is_empty());
        assert!(graph.scope_collisions[0].2.contains("collision on"));
    }
}
