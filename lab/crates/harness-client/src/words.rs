//! Telemetry in plain language.
//!
//! The daemon reports what a program needs: `pending_tool = "read_file: backend/daemon/supervisor.py"`,
//! `status = SYNTHESIZING`. A person supervising a team of agents needs a sentence — "Reading
//! backend/daemon/supervisor.py", "Thinking…", "Waiting for your approval". Everything that turns
//! the one into the other lives here, as pure functions over the wire types, so the window, the
//! activity feed and the tests all say the same thing and none of them re-derives it.

use harness_proto::v1::{LabState, Pod, PodStatus};

/// How a line should be coloured. Meaning, not hue: the window maps these onto its palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Working normally (the accent).
    Info,
    /// Finished / healthy.
    Good,
    /// Wants a person.
    Warn,
    /// Stopped, failed, or dangerous.
    Bad,
    /// Nothing is happening.
    Dim,
}

fn status_of(pod: &Pod) -> PodStatus {
    PodStatus::try_from(pod.status).unwrap_or(PodStatus::Unspecified)
}

fn trim_detail(detail: &str, max: usize) -> String {
    let flat: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// `"read_file: backend/daemon/supervisor.py"` → `"Reading backend/daemon/supervisor.py"`.
/// An unknown tool still reads as a sentence (`"Using my_tool: …"`), never as raw telemetry.
pub fn tool_phrase(raw: &str) -> String {
    let (tool, detail) = raw.split_once(':').map(|(t, d)| (t.trim(), d.trim())).unwrap_or((raw.trim(), ""));
    let detail = trim_detail(detail, 80);
    let with = |verb: &str| if detail.is_empty() { verb.to_owned() } else { format!("{verb} {detail}") };
    match tool.to_ascii_lowercase().as_str() {
        "read_file" | "file_read" | "read" | "view" | "cat" => with("Reading"),
        "write_file" | "file_write" | "write" => with("Writing"),
        "edit" | "multiedit" | "editor" | "apply_patch" | "patch" | "notebookedit" => with("Editing"),
        "shell" | "bash" | "python_repl" | "execute" => {
            if detail.is_empty() {
                "Running a command".to_owned()
            } else {
                format!("Running {detail}")
            }
        }
        "grep" | "glob" | "ls" | "search" | "find" => with("Searching"),
        "http_request" | "webfetch" | "websearch" | "fetch" => with("Fetching"),
        "plan" | "architect" => {
            if detail.is_empty() {
                "Planning".to_owned()
            } else {
                format!("Planning: {detail}")
            }
        }
        "debugger" => with("Debugging"),
        "think" => "Thinking it through".to_owned(),
        "agent_memory" | "local_graph" | "memory" | "context_digest" => with("Checking memory"),
        "team" | "a2a_client" | "local_swarm" => with("Coordinating with other agents"),
        "claude_code_engineer" => "Handing the work to Claude Code".to_owned(),
        "ollama_resource_manager" | "omlx_resource_manager" => "Managing local model memory".to_owned(),
        "" => "Working".to_owned(),
        other => {
            if detail.is_empty() {
                format!("Using {other}")
            } else {
                format!("Using {other}: {detail}")
            }
        }
    }
}

/// The one-word state of an agent, for its chip.
pub fn status_word(pod: &Pod) -> (&'static str, Tone) {
    if pod.isolated {
        return ("Stopped", Tone::Bad);
    }
    match status_of(pod) {
        PodStatus::Isolated => ("Stopped", Tone::Bad),
        PodStatus::Contained => ("Needs you", Tone::Warn),
        PodStatus::Executing => ("Working", Tone::Info),
        PodStatus::Synthesizing => ("Thinking", Tone::Info),
        PodStatus::Idle | PodStatus::Unspecified => ("Idle", Tone::Dim),
    }
}

/// What an agent is doing right now, as one line.
pub fn now_line(pod: &Pod) -> (String, Tone) {
    if pod.isolated || status_of(pod) == PodStatus::Isolated {
        return ("Stopped — it won't act until you release it".to_owned(), Tone::Bad);
    }
    if status_of(pod) == PodStatus::Contained || pod.pending_tool.contains("awaiting a human") {
        return ("Waiting for your approval".to_owned(), Tone::Warn);
    }
    if !pod.pending_tool.is_empty() {
        return (tool_phrase(&pod.pending_tool), Tone::Info);
    }
    match status_of(pod) {
        PodStatus::Synthesizing => ("Thinking…".to_owned(), Tone::Info),
        PodStatus::Executing => ("Working…".to_owned(), Tone::Info),
        _ => {
            let said = trim_detail(pod.stream_tail.trim(), 90);
            if said.is_empty() { ("Idle".to_owned(), Tone::Dim) } else { (format!("Idle — last said “{said}”"), Tone::Dim) }
        }
    }
}

/// How long an agent has been in its current state: `"19s"`, `"4m"`; `None` when the daemon did not say.
pub fn in_state_for(pod: &Pod, now_ms: i64) -> Option<String> {
    if pod.since_ms <= 0 || now_ms < pod.since_ms {
        return None;
    }
    let s = (now_ms - pod.since_ms) / 1000;
    Some(match s {
        0..=1 => "just now".to_owned(),
        2..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    })
}

/// What Laya Guard has done for one agent, as a line — `None` until it has scored anything.
pub fn guard_line(pod: &Pod) -> Option<String> {
    let last = pod.risk_last?;
    let mut parts = vec![format!("last call {last:.2}")];
    if pod.risk_peak > last + 0.005 {
        parts.push(format!("peak {:.2}", pod.risk_peak));
    }
    if pod.auto_approved > 0 {
        parts.push(format!("{} auto-approved", pod.auto_approved));
    }
    if pod.asked > 0 {
        parts.push(format!("{} held for you", pod.asked));
    }
    Some(parts.join(" · "))
}

/// What a permission mode means, in a line a person can act on.
pub fn mode_blurb(mode: &str) -> &'static str {
    match mode {
        "plan" => "Plan — agents think and propose; nothing runs.",
        "build" => "Build — agents work; risky steps ask you first.",
        "auto" => "Auto — agents work without asking. Still stops for anything destructive, deny rules and model deletes.",
        _ => "Mode unknown — this daemon's session did not report one.",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamState {
    Offline,
    NeedsYou,
    Working,
    Stopped,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSummary {
    pub state: TeamState,
    pub working: usize,
    pub waiting: usize,
    pub idle: usize,
    pub stopped: usize,
}

impl TeamSummary {
    pub fn headline(&self) -> &'static str {
        match self.state {
            TeamState::Offline => "Offline",
            TeamState::NeedsYou => "Needs you",
            TeamState::Working => "Working",
            TeamState::Stopped => "Stopped",
            TeamState::Idle => "Idle",
        }
    }

    pub fn tone(&self) -> Tone {
        match self.state {
            TeamState::Offline | TeamState::Stopped => Tone::Bad,
            TeamState::NeedsYou => Tone::Warn,
            TeamState::Working => Tone::Info,
            TeamState::Idle => Tone::Dim,
        }
    }
}

/// The whole team in one word. A person waiting on you outranks everything that is merely busy.
pub fn team_summary(lab: Option<&LabState>, approvals_waiting: usize, online: bool) -> TeamSummary {
    let (mut working, mut waiting, mut idle, mut stopped) = (0, 0, 0, 0);
    let pods = lab.map(|l| l.pods.as_slice()).unwrap_or(&[]);
    for pod in pods {
        match status_word(pod).0 {
            "Working" | "Thinking" => working += 1,
            "Needs you" => waiting += 1,
            "Stopped" => stopped += 1,
            _ => idle += 1,
        }
    }
    let state = if !online {
        TeamState::Offline
    } else if approvals_waiting > 0 || waiting > 0 {
        TeamState::NeedsYou
    } else if working > 0 {
        TeamState::Working
    } else if !pods.is_empty() && stopped == pods.len() {
        TeamState::Stopped
    } else {
        TeamState::Idle
    };
    TeamSummary { state, working, waiting, idle, stopped }
}

/// What changed between two snapshots of the same session, as feed lines: `(pod, tone, sentence)`.
///
/// Deliberately sparse. A feed that narrates every frame is the noise this exists to replace, so it
/// reports what a person would want to be told — an agent starting something, being held for an
/// answer, failing, being stopped — and nothing for a tick that changed only a counter.
pub fn lab_changes(old: &LabState, new: &LabState) -> Vec<(Option<String>, Tone, String)> {
    let mut out = Vec::new();
    if new.mission != old.mission && !new.mission.is_empty() {
        out.push((None, Tone::Info, format!("New task: {}", trim_detail(&new.mission, 140))));
    }
    if new.mode != old.mode && !new.mode.is_empty() && !old.mode.is_empty() {
        out.push((None, if new.mode == "auto" { Tone::Warn } else { Tone::Info }, format!("Mode is now {}", new.mode)));
    }
    for pod in &new.pods {
        let Some(before) = old.pods.iter().find(|p| p.id == pod.id) else {
            out.push((Some(pod.id.clone()), Tone::Info, format!("{} joined the team", pod.name)));
            continue;
        };
        if pod.isolated && !before.isolated {
            out.push((Some(pod.id.clone()), Tone::Bad, format!("{} was stopped", pod.name)));
            continue;
        }
        if !pod.isolated && before.isolated {
            out.push((Some(pod.id.clone()), Tone::Info, format!("{} was released", pod.name)));
        }
        let (now, was) = (status_of(pod), status_of(before));
        if now == PodStatus::Contained && was != PodStatus::Contained {
            out.push((Some(pod.id.clone()), Tone::Warn, format!("{} is waiting for your approval", pod.name)));
        }
        let started = !pod.pending_tool.is_empty() && pod.pending_tool != before.pending_tool && !pod.pending_tool.contains("awaiting a human");
        if started {
            out.push((Some(pod.id.clone()), Tone::Info, format!("{}: {}", pod.name, tool_phrase(&pod.pending_tool))));
        }
        // Auto mode ran something that was not trivially safe: say so, with the number that let it through.
        if pod.auto_approved > before.auto_approved
            && let (Some(risk), Some(last)) = (pod.risk_last, pod.trace.last())
            && risk >= 0.4
        {
            let what = tool_phrase(&format!("{}: {}", last.tool, last.input));
            out.push((Some(pod.id.clone()), if risk >= 0.6 { Tone::Warn } else { Tone::Info }, format!("{}: auto-approved — {what} (Laya risk {risk:.2})", pod.name)));
        }
        // A call that ended in failure is worth a line of its own; the many that succeed are not.
        if let Some(last) = pod.trace.last() {
            let is_new = match before.trace.last() {
                None => true,
                Some(prev) => last.tool != prev.tool || last.ts_ms != prev.ts_ms || prev.status != "failed",
            };
            if last.status == "failed" && last.tool != "ISOLATE" && is_new {
                out.push((Some(pod.id.clone()), Tone::Bad, format!("{}: {} failed", pod.name, last.tool)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::v1::TraceRow;

    fn pod(id: &str, status: PodStatus) -> Pod {
        Pod { id: id.into(), name: id.into(), status: status as i32, ..Default::default() }
    }

    #[test]
    fn tools_read_as_sentences() {
        assert_eq!(tool_phrase("read_file: backend/daemon/supervisor.py"), "Reading backend/daemon/supervisor.py");
        assert_eq!(tool_phrase("Edit: src/lib.rs"), "Editing src/lib.rs");
        assert_eq!(tool_phrase("shell: cargo test --workspace"), "Running cargo test --workspace");
        assert_eq!(tool_phrase("shell"), "Running a command");
        assert_eq!(tool_phrase("Grep: fn main"), "Searching fn main");
        assert_eq!(tool_phrase("plan: split the lookup"), "Planning: split the lookup");
        assert_eq!(tool_phrase("claude_code_engineer: refactor"), "Handing the work to Claude Code");
        assert_eq!(tool_phrase("my_new_tool: x"), "Using my_new_tool: x", "an unknown tool still reads as a sentence");
        assert_eq!(tool_phrase(""), "Working");
        let long = format!("read_file: {}", "a/".repeat(100));
        assert!(tool_phrase(&long).chars().count() < 100, "a long path does not run the line off the card");
    }

    #[test]
    fn what_an_agent_is_doing_in_one_line() {
        let mut p = pod("a", PodStatus::Executing);
        p.pending_tool = "read_file: x.py".into();
        assert_eq!(now_line(&p), ("Reading x.py".to_owned(), Tone::Info));
        let held = Pod { status: PodStatus::Contained as i32, pending_tool: "shell: awaiting a human".into(), ..pod("a", PodStatus::Contained) };
        assert_eq!(now_line(&held), ("Waiting for your approval".to_owned(), Tone::Warn));
        let stopped = Pod { isolated: true, status: PodStatus::Synthesizing as i32, ..pod("a", PodStatus::Synthesizing) };
        assert_eq!(now_line(&stopped).1, Tone::Bad);
        assert_eq!(status_word(&stopped).0, "Stopped", "a stopped pod is stopped even while its status still says it is thinking");
        assert_eq!(now_line(&pod("a", PodStatus::Synthesizing)).0, "Thinking…");
        let idle = Pod { stream_tail: "Done: the lookup is O(1) now.".into(), ..pod("a", PodStatus::Idle) };
        assert!(now_line(&idle).0.starts_with("Idle — last said “Done"));
        assert_eq!(now_line(&pod("a", PodStatus::Idle)).0, "Idle");
    }

    #[test]
    fn the_team_has_one_state_and_a_waiting_person_outranks_busy_work() {
        let lab = LabState { pods: vec![pod("a", PodStatus::Executing), pod("b", PodStatus::Idle)], ..Default::default() };
        let s = team_summary(Some(&lab), 0, true);
        assert_eq!((s.state, s.working, s.idle), (TeamState::Working, 1, 1));
        assert_eq!(team_summary(Some(&lab), 1, true).state, TeamState::NeedsYou, "an approval waiting beats work in progress");
        assert_eq!(team_summary(Some(&lab), 0, false).state, TeamState::Offline);
        let stopped = LabState { pods: vec![Pod { isolated: true, ..pod("a", PodStatus::Idle) }], ..Default::default() };
        assert_eq!(team_summary(Some(&stopped), 0, true).state, TeamState::Stopped);
        assert_eq!(team_summary(None, 0, true).state, TeamState::Idle);
        assert_eq!(team_summary(Some(&lab), 0, true).headline(), "Working");
    }

    #[test]
    fn the_feed_reports_what_a_person_would_want_told_and_not_every_tick() {
        let before = LabState { pods: vec![pod("debugger", PodStatus::Idle)], mode: "build".into(), ..Default::default() };
        // a counter ticks: nothing to say
        let mut ticked = before.clone();
        ticked.pods[0].calls = 9;
        ticked.pods[0].busy_ms = 4000;
        assert!(lab_changes(&before, &ticked).is_empty(), "a tick that changed only a counter is not news");
        // it starts something
        let mut started = before.clone();
        started.pods[0].pending_tool = "shell: cargo test".into();
        started.pods[0].status = PodStatus::Executing as i32;
        let said = lab_changes(&before, &started);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].2, "debugger: Running cargo test");
        // it is held for an answer
        let mut held = before.clone();
        held.pods[0].status = PodStatus::Contained as i32;
        held.pods[0].pending_tool = "shell: awaiting a human".into();
        let said = lab_changes(&before, &held);
        assert!(said.iter().any(|c| c.1 == Tone::Warn && c.2.contains("waiting for your approval")));
        assert!(!said.iter().any(|c| c.2.contains("awaiting a human")), "the placeholder tool is not narrated as work");
        // it fails, and is stopped, and the mode changes
        let mut failed = before.clone();
        failed.pods[0].trace.push(TraceRow { tool: "shell".into(), status: "failed".into(), ..Default::default() });
        assert!(lab_changes(&before, &failed).iter().any(|c| c.1 == Tone::Bad && c.2 == "debugger: shell failed"));
        let mut stopped = before.clone();
        stopped.pods[0].isolated = true;
        assert!(lab_changes(&before, &stopped).iter().any(|c| c.2 == "debugger was stopped"));
        let mut auto = before.clone();
        auto.mode = "auto".into();
        assert!(lab_changes(&before, &auto).iter().any(|c| c.2 == "Mode is now auto" && c.1 == Tone::Warn), "switching to auto is flagged, not buried");
        // a brand-new session's first frame says nothing about "changes"
        let first = LabState { mode: "build".into(), ..Default::default() };
        assert!(lab_changes(&first, &before).iter().any(|c| c.2.contains("joined")), "pods that appear are announced once");
    }

    #[test]
    fn auto_mode_says_what_it_let_through_and_at_what_risk() {
        let before = LabState { pods: vec![pod("debugger", PodStatus::Executing)], ..Default::default() };
        let mut after = before.clone();
        after.pods[0].auto_approved = 1;
        after.pods[0].risk_last = Some(0.65);
        after.pods[0].trace.push(TraceRow { tool: "shell".into(), status: "running".into(), input: "git push origin main".into(), ..Default::default() });
        let said = lab_changes(&before, &after);
        assert!(said.iter().any(|c| c.1 == Tone::Warn && c.2 == "debugger: auto-approved — Running git push origin main (Laya risk 0.65)"), "{said:?}");
        let mut trivial = before.clone();
        trivial.pods[0].auto_approved = 1;
        trivial.pods[0].risk_last = Some(0.05);
        assert!(lab_changes(&before, &trivial).iter().all(|c| !c.2.contains("auto-approved")), "a trivially safe call is not narrated");

        assert_eq!(guard_line(&pod("a", PodStatus::Idle)), None, "nothing scored yet: nothing to claim");
        let mut scored = pod("a", PodStatus::Idle);
        scored.risk_last = Some(0.1);
        scored.risk_peak = 0.9;
        scored.auto_approved = 12;
        scored.asked = 2;
        assert_eq!(guard_line(&scored).as_deref(), Some("last call 0.10 · peak 0.90 · 12 auto-approved · 2 held for you"));
    }

    #[test]
    fn time_in_state_is_a_glance() {
        let p = Pod { since_ms: 1_000_000, ..pod("a", PodStatus::Idle) };
        assert_eq!(in_state_for(&p, 1_000_500).as_deref(), Some("just now"));
        assert_eq!(in_state_for(&p, 1_019_000).as_deref(), Some("19s"));
        assert_eq!(in_state_for(&p, 1_000_000 + 240_000).as_deref(), Some("4m"));
        assert_eq!(in_state_for(&Pod { since_ms: 0, ..p.clone() }, 5), None, "unknown stays unknown");
        assert_eq!(in_state_for(&p, 10), None, "a clock that ran backwards shows nothing rather than nonsense");
    }

    #[test]
    fn modes_say_what_they_mean() {
        assert!(mode_blurb("auto").contains("destructive"), "auto never claims to be unconditional");
        assert!(mode_blurb("plan").contains("nothing runs"));
        assert!(mode_blurb("???").contains("unknown"));
    }
}
