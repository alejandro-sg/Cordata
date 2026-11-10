//! What the menu bar item says and offers, as plain data.
//!
//! The menu bar item is for the moments the window is not in front: is anything waiting on me, is anything running,
//! and can I answer from here. So it shows, in this order: what needs you (with the exact command, and Allow or Reject
//! when that is safe to do without the thread in view), runs you can hold or resume, the threads, then the few things
//! you would otherwise switch apps for. Nothing in it is decoration.
//!
//! Pure: the window turns this into a native menu and turns a click back into an [`Action`].

use crate::app::TabState;

/// Longest line the menu shows; the rest is cut with an ellipsis so the menu stays a menu.
const LINE: usize = 56;
/// Gates listed before the rest are counted, and threads listed.
const MAX_GATES: usize = 4;
const MAX_THREADS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A line that names a section or a subject. Not clickable.
    Header(String),
    /// A line of detail under a header. Not clickable.
    Note(String),
    /// A line you can click; `id` comes back as [`parse`].
    Action { id: String, title: String },
    Sep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tray {
    /// The mark in the menu bar: where the cursor cell sits says the state (see `app/assets/brand/README.md`).
    pub title: String,
    /// The same thing as a sentence, for hovering and for VoiceOver.
    pub tooltip: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateIn {
    pub slot: usize,
    pub thread: String,
    /// The tool and what it was asked to do.
    pub command: String,
    pub high_risk: bool,
    /// [`sig_hash`] of the call the gate holds, so a click answers the call that was shown and no other.
    pub sig: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadIn {
    pub slot: usize,
    pub title: String,
    pub now: String,
    pub state: TabState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIn {
    pub run_id: String,
    pub goal: String,
    pub state: String,
    pub can_hold: bool,
    pub can_resume: bool,
    pub last_handoff: Option<String>,
}

/// What a click means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Open,
    ShowField,
    NewThread,
    Settings,
    Quit,
    OpenThread(usize),
    Allow { slot: usize, sig: u64 },
    Reject { slot: usize, sig: u64 },
    Hold(String),
    Resume(String),
}

impl Action {
    pub fn id(&self) -> String {
        match self {
            Action::Open => "open".into(),
            Action::ShowField => "field".into(),
            Action::NewThread => "new".into(),
            Action::Settings => "settings".into(),
            Action::Quit => "quit".into(),
            Action::OpenThread(slot) => format!("thread:{slot}"),
            Action::Allow { slot, sig } => format!("allow:{slot}:{sig:x}"),
            Action::Reject { slot, sig } => format!("reject:{slot}:{sig:x}"),
            Action::Hold(id) => format!("hold:{id}"),
            Action::Resume(id) => format!("resume:{id}"),
        }
    }
}

pub fn parse(id: &str) -> Option<Action> {
    let mut parts = id.splitn(3, ':');
    let kind = parts.next()?;
    let (a, b) = (parts.next(), parts.next());
    Some(match (kind, a, b) {
        ("open", None, None) => Action::Open,
        ("field", None, None) => Action::ShowField,
        ("new", None, None) => Action::NewThread,
        ("settings", None, None) => Action::Settings,
        ("quit", None, None) => Action::Quit,
        ("thread", Some(slot), None) => Action::OpenThread(slot.parse().ok()?),
        ("allow", Some(slot), Some(sig)) => Action::Allow { slot: slot.parse().ok()?, sig: u64::from_str_radix(sig, 16).ok()? },
        ("reject", Some(slot), Some(sig)) => Action::Reject { slot: slot.parse().ok()?, sig: u64::from_str_radix(sig, 16).ok()? },
        ("hold", Some(run), None) if is_run_id(run) => Action::Hold(run.to_owned()),
        ("resume", Some(run), None) if is_run_id(run) => Action::Resume(run.to_owned()),
        _ => return None,
    })
}

fn is_run_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A stable hash of a gate's call (FNV-1a), small enough to carry in a menu item.
pub fn sig_hash(sig: &str) -> u64 {
    sig.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn build(gates: &[GateIn], threads: &[ThreadIn], runs: &[RunIn]) -> Tray {
    let working = threads.iter().filter(|t| t.state == TabState::Working).count() + runs.iter().filter(|r| r.can_hold).count();
    let waiting = gates.len();

    // Where the cell sits: in the mouth when you are needed, inside the ring while work runs, hollow when quiet.
    let (mark, said) = if waiting > 0 {
        ("C█", format!("{} waiting on you", waiting))
    } else if working > 0 {
        ("(█", format!("{} working", working))
    } else {
        ("C▯", "all quiet".to_owned())
    };
    let count = if waiting > 0 { waiting } else if working > 1 { working } else { 0 };
    let title = if count > 0 { format!("{mark} {count}") } else { mark.to_owned() };
    let sentence = match (waiting, working) {
        (0, 0) => "Cordata: all quiet".to_owned(),
        (0, w) => format!("Cordata: {w} working"),
        (n, 0) => format!("Cordata: {n} waiting on you"),
        (n, w) => format!("Cordata: {n} waiting on you, {w} working"),
    };

    let mut items = vec![Item::Header(match (waiting, working) {
        (0, 0) => "All quiet".to_owned(),
        (0, w) => format!("{w} working"),
        (n, 0) => format!("{n} waiting on you"),
        (n, w) => format!("{n} waiting on you · {w} working"),
    })];
    let _ = said;

    for g in gates.iter().take(MAX_GATES) {
        items.push(Item::Sep);
        items.push(Item::Header(clip(&format!("#{} {}", g.slot, g.thread), LINE)));
        items.push(Item::Note(clip(&g.command, LINE)));
        if g.high_risk {
            // Risky calls are read in the thread, with its diff and its reason beside them.
            items.push(Item::Action { id: Action::OpenThread(g.slot).id(), title: "Review in Cordata…".into() });
        } else {
            items.push(Item::Action { id: Action::Allow { slot: g.slot, sig: g.sig }.id(), title: "Allow".into() });
            items.push(Item::Action { id: Action::Reject { slot: g.slot, sig: g.sig }.id(), title: "Reject".into() });
        }
    }
    if waiting > MAX_GATES {
        items.push(Item::Note(format!("{} more in Cordata", waiting - MAX_GATES)));
    }

    for r in runs {
        items.push(Item::Sep);
        items.push(Item::Header(clip(&r.goal, LINE)));
        let state = if r.state == "paused" { "held".to_owned() } else { r.state.clone() };
        items.push(Item::Note(clip(&match &r.last_handoff {
            Some(h) => format!("run · {state} · {h}"),
            None => format!("run · {state}"),
        }, LINE)));
        if r.can_hold {
            items.push(Item::Action { id: Action::Hold(r.run_id.clone()).id(), title: "Hold at next safe point".into() });
        }
        if r.can_resume {
            items.push(Item::Action { id: Action::Resume(r.run_id.clone()).id(), title: "Resume".into() });
        }
    }

    if !threads.is_empty() {
        items.push(Item::Sep);
        items.push(Item::Header(plural(threads.len(), "thread", "threads")));
        let mut ordered: Vec<&ThreadIn> = threads.iter().collect();
        ordered.sort_by_key(|t| (t.state != TabState::Working, t.slot)); // stable: what is moving first
        for t in ordered.into_iter().take(MAX_THREADS) {
            let line = if matches!(t.state, TabState::Working) { format!("#{} {} · {}", t.slot, t.title, t.now) } else { format!("#{} {}", t.slot, t.title) };
            items.push(Item::Action { id: Action::OpenThread(t.slot).id(), title: clip(&line, LINE) });
        }
        if threads.len() > MAX_THREADS {
            items.push(Item::Note(format!("{} more in Cordata", threads.len() - MAX_THREADS)));
        }
    }

    items.push(Item::Sep);
    items.push(Item::Action { id: Action::Open.id(), title: "Open Cordata".into() });
    items.push(Item::Action { id: Action::ShowField.id(), title: "Show the field".into() });
    items.push(Item::Action { id: Action::NewThread.id(), title: "New thread".into() });
    items.push(Item::Sep);
    items.push(Item::Action { id: Action::Settings.id(), title: "Settings…".into() });
    items.push(Item::Action { id: Action::Quit.id(), title: "Quit Cordata".into() });
    Tray { title, tooltip: sentence, items }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(slot: usize, risky: bool) -> GateIn {
        GateIn { slot, thread: format!("thread {slot}"), command: "bash · cargo add serde_json".into(), high_risk: risky, sig: sig_hash(&format!("call {slot}")) }
    }

    fn thread(slot: usize, state: TabState) -> ThreadIn {
        ThreadIn { slot, title: format!("t{slot}"), now: "editing a.rs".into(), state }
    }

    fn actions(t: &Tray) -> Vec<&str> {
        t.items.iter().filter_map(|i| if let Item::Action { id, .. } = i { Some(id.as_str()) } else { None }).collect()
    }

    #[test]
    fn the_mark_says_who_is_needed_then_who_is_working_then_nothing() {
        assert_eq!(build(&[], &[], &[]).title, "C▯");
        assert_eq!(build(&[], &[thread(1, TabState::Working)], &[]).title, "(█", "one working: no count");
        assert_eq!(build(&[], &[thread(1, TabState::Working), thread(2, TabState::Working)], &[]).title, "(█ 2");
        let t = build(&[gate(1, false), gate(2, false)], &[thread(3, TabState::Working)], &[]);
        assert_eq!(t.title, "C█ 2", "waiting on you outranks working");
        assert_eq!(t.tooltip, "Cordata: 2 waiting on you, 1 working");
        assert_eq!(build(&[], &[], &[]).tooltip, "Cordata: all quiet");
    }

    #[test]
    fn a_live_run_counts_as_working_and_a_held_one_does_not() {
        let live = RunIn { run_id: "run-a".into(), goal: "g".into(), state: "execute".into(), can_hold: true, can_resume: false, last_handoff: None };
        let held = RunIn { run_id: "run-b".into(), goal: "g".into(), state: "paused".into(), can_hold: false, can_resume: true, last_handoff: None };
        assert_eq!(build(&[], &[], &[live.clone()]).title, "(█");
        assert_eq!(build(&[], &[], &[held.clone()]).title, "C▯");
        let t = build(&[], &[], &[live, held]);
        assert_eq!(actions(&t)[..2], ["hold:run-a", "resume:run-b"]);
    }

    #[test]
    fn a_safe_gate_can_be_answered_here_and_a_risky_one_must_be_read_in_the_thread() {
        let t = build(&[gate(1, false), gate(2, true)], &[], &[]);
        let ids = actions(&t);
        assert!(ids.iter().any(|i| i.starts_with("allow:1:")) && ids.iter().any(|i| i.starts_with("reject:1:")));
        assert!(!ids.iter().any(|i| i.starts_with("allow:2:")), "no one-click allow for a risky call");
        assert!(ids.contains(&"thread:2"));
        assert!(t.items.contains(&Item::Note("bash · cargo add serde_json".into())), "the exact command is shown");
    }

    #[test]
    fn long_lists_are_cut_and_counted_not_hidden() {
        let gates: Vec<GateIn> = (1..=6).map(|i| gate(i, false)).collect();
        let threads: Vec<ThreadIn> = (1..=9).map(|i| thread(i, TabState::Idle)).collect();
        let t = build(&gates, &threads, &[]);
        assert_eq!(actions(&t).iter().filter(|i| i.starts_with("allow:")).count(), MAX_GATES);
        assert!(t.items.contains(&Item::Note("2 more in Cordata".into())));
        assert_eq!(actions(&t).iter().filter(|i| i.starts_with("thread:")).count(), MAX_THREADS);
        assert!(t.items.contains(&Item::Note("3 more in Cordata".into())));
    }

    #[test]
    fn threads_that_are_moving_come_first_and_lines_are_clipped() {
        let mut long = thread(1, TabState::Idle);
        long.title = "x".repeat(200);
        let t = build(&[], &[long, thread(2, TabState::Working)], &[]);
        let titles: Vec<&str> = t.items.iter().filter_map(|i| if let Item::Action { id, title } = i { id.starts_with("thread:").then_some(title.as_str()) } else { None }).collect();
        assert!(titles[0].starts_with("#2 t2 · editing a.rs"));
        assert!(titles[1].chars().count() <= LINE && titles[1].ends_with('…'));
    }

    #[test]
    fn every_action_survives_the_trip_through_its_id() {
        let all = [
            Action::Open, Action::ShowField, Action::NewThread, Action::Settings, Action::Quit, Action::OpenThread(7),
            Action::Allow { slot: 3, sig: u64::MAX }, Action::Reject { slot: 12, sig: 0xabc }, Action::Hold("run-ab_1".into()), Action::Resume("run-ab_1".into()),
        ];
        for a in all {
            assert_eq!(parse(&a.id()), Some(a.clone()), "{a:?}");
        }
        for bad in ["", "nope", "thread:x", "allow:1", "allow:1:zz", "hold:../x", "hold:", "quit:1", "open:x"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_hash_is_stable_and_tells_calls_apart() {
        assert_eq!(sig_hash("a|b"), sig_hash("a|b"));
        assert_ne!(sig_hash("cargo add serde_json"), sig_hash("cargo add serde"));
        assert_eq!(sig_hash(""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn the_footer_is_always_there() {
        let t = build(&[], &[], &[]);
        assert_eq!(actions(&t), ["open", "field", "new", "settings", "quit"]);
        assert_eq!(t.items[0], Item::Header("All quiet".into()));
    }
}
