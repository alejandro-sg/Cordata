//! What the menu bar shows, derived from the model — pure, so it is testable without a window.
//!
//! The tray has to answer one question at a glance from across the desk: *does the agent need me?*
//! That is a three-state reading, and the elevation rule behind it is the same one the Lab uses to
//! decide whether a request takes over the window (`interrupts`), kept here rather than in the app
//! so both front doors and the headless tests read one source of truth.
//!
//! Everything here is a fold over [`LabModel`]: no GPUI, no clock of its own, no I/O.

use harness_proto::v1::InterventionRequest;

use crate::model::LabModel;

/// The router's own gate. At or above this a request is a destructive reading and takes the screen;
/// below it, the request waits quietly and must never pull the person out of what they are doing.
pub const RISK_GATE: f64 = 0.8;

/// Does this request take over by itself? A destructive reading does — at or above the router's own
/// gate — and so does a request Laya marked armed without scoring it (when in doubt, interrupt).
/// Everything else waits in the tray.
pub fn interrupts(request: &InterventionRequest) -> bool {
    match request.risk_score {
        Some(score) => score >= RISK_GATE,
        None => request.armed,
    }
}

/// What the status item shows.
///
/// `Idle` is quiet; `Executing` says work is happening; `ActionRequired` says it stopped and is
/// waiting for a person. Deliberately no fourth state: offline is not a menu-bar state — the daemon
/// being unreachable does not ask anything of the person, and it is said in words in the deck's
/// status strip, where it can be read rather than guessed from a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Executing,
    ActionRequired,
}

impl TrayState {
    /// The word for the deck's status strip, and for the accessibility description of the icon.
    pub fn label(self) -> &'static str {
        match self {
            TrayState::Idle => "idle",
            TrayState::Executing => "working",
            TrayState::ActionRequired => "needs you",
        }
    }

    /// The SF Symbol to ask `NSStatusItem` for; the caller falls back to an embedded glyph.
    pub fn symbol(self) -> &'static str {
        match self {
            TrayState::Idle => "circle",
            TrayState::Executing => "circle.dotted",
            TrayState::ActionRequired => "exclamationmark.circle",
        }
    }
}

/// The state the tray should be in, given the last picture. Action-required outranks executing: a
/// question that is going unanswered matters more than another agent still working.
pub fn tray_state(model: &LabModel) -> TrayState {
    if waiting_count(model) > 0 {
        TrayState::ActionRequired
    } else if executing_count(model) > 0 {
        TrayState::Executing
    } else {
        TrayState::Idle
    }
}

/// How many questions a person must answer — the badge count. Only the requests that interrupt:
/// a `0.58` reading sits in the deck's list and must not put a number on the menu bar, or the
/// person learns to ignore the number.
pub fn waiting_count(model: &LabModel) -> usize {
    model.open_interventions().filter(|open| !open.stale && interrupts(&open.request)).count()
}

/// Questions waiting that do *not* interrupt — shown in the deck, never on the icon.
pub fn quiet_count(model: &LabModel) -> usize {
    model.open_interventions().filter(|open| !open.stale && !interrupts(&open.request)).count()
}

/// Agents currently working. Isolated pods are excluded: an E-Stopped agent is not working, and a
/// tray that says otherwise would hide the one fact the person most needs.
pub fn executing_count(model: &LabModel) -> usize {
    model
        .lab()
        .map(|lab| lab.pods.iter().filter(|pod| !pod.isolated && harness_proto::v1::PodStatus::try_from(pod.status) == Ok(harness_proto::v1::PodStatus::Executing)).count())
        .unwrap_or(0)
}

/// The deck's status strip: one sentence, in words, so the icon never has to carry state by colour
/// alone. `None` means there is nothing to say and the strip stays empty.
pub fn status_line(model: &LabModel) -> Option<String> {
    let waiting = waiting_count(model);
    if waiting > 0 {
        return Some(if waiting == 1 { "One agent is waiting for you".to_owned() } else { format!("{waiting} agents are waiting for you") });
    }
    // Only a known-offline daemon gets a sentence: a client that is still connecting, or that has
    // never had a daemon file, is saying something quite different from "the daemon went away".
    if matches!(model.connection, crate::model::ConnectionState::Offline { .. }) {
        return Some("The daemon is offline — showing the last picture".to_owned());
    }
    let working = executing_count(model);
    if working > 0 {
        return Some(if working == 1 { "One agent is working".to_owned() } else { format!("{working} agents are working") });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use harness_proto::v1::{InterventionEvent, InterventionRequest, LabState, Pod, PodStatus, TelemetryFrame, intervention_event};

    use crate::model::{ClientEvent, LabModel};

    fn frame(pods: Vec<Pod>) -> TelemetryFrame {
        TelemetryFrame { seq: 1, session_id: "s1".into(), lab: Some(LabState { lab_seq: 1, pods, ..Default::default() }), ..Default::default() }
    }

    fn pod(id: &str, status: PodStatus) -> Pod {
        Pod { id: id.into(), name: id.into(), status: status as i32, ..Default::default() }
    }

    /// A held request as it arrives from the daemon.
    fn held(id: &str, risk: Option<f64>, armed: bool) -> InterventionEvent {
        let request = InterventionRequest { intervention_id: id.into(), tool: "shell".into(), risk_score: risk, armed, ..Default::default() };
        InterventionEvent { event: Some(intervention_event::Event::Request(request)) }
    }

    fn offline(now: Instant) -> ClientEvent {
        ClientEvent::Connection(crate::model::ConnectionState::Offline { since: now, attempt: 1, retry_at: now, error: "gone".into() })
    }

    #[test]
    fn the_three_states_follow_the_agents() {
        let now = Instant::now();
        let mut model = LabModel::default();
        assert_eq!(super::tray_state(&model), super::TrayState::Idle, "nothing happening is idle");

        model.apply(ClientEvent::Telemetry(Box::new(frame(vec![pod("architect", PodStatus::Executing)]))), now);
        assert_eq!(super::tray_state(&model), super::TrayState::Executing);

        model.apply(ClientEvent::Telemetry(Box::new(frame(vec![pod("architect", PodStatus::Synthesizing), pod("debugger", PodStatus::Idle)]))), now);
        assert_eq!(super::tray_state(&model), super::TrayState::Idle, "thinking and idling are not the executing state");

        model.apply(ClientEvent::Telemetry(Box::new(frame(vec![Pod { isolated: true, ..pod("architect", PodStatus::Executing) }]))), now);
        assert_eq!(super::tray_state(&model), super::TrayState::Idle, "a stopped agent is not working");
    }

    #[test]
    fn only_a_request_that_interrupts_puts_a_badge_on_the_icon() {
        let now = Instant::now();
        let mut model = LabModel::default();
        // The tiering rule, exactly as the Lab applies it: 0.58 waits in the list, 0.93 takes over.
        model.apply(ClientEvent::Intervention(held("q/1", Some(0.58), false)), now);
        assert_eq!(super::tray_state(&model), super::TrayState::Idle, "a quiet request must not change the tray");
        assert_eq!(super::waiting_count(&model), 0);
        assert_eq!(super::quiet_count(&model), 1, "…but it is still there to be answered");

        model.apply(ClientEvent::Intervention(held("q/2", Some(0.93), true)), now);
        assert_eq!(super::tray_state(&model), super::TrayState::ActionRequired, "a destructive reading does");
        assert_eq!(super::waiting_count(&model), 1, "the badge counts only the ones that interrupt");
    }

    #[test]
    fn an_unscored_armed_request_interrupts_and_an_unscored_quiet_one_does_not() {
        let now = Instant::now();
        let mut model = LabModel::default();
        model.apply(ClientEvent::Intervention(held("q/1", None, true)), now);
        assert_eq!(super::tray_state(&model), super::TrayState::ActionRequired, "armed without a score is interrupt-when-in-doubt");
        model.apply(ClientEvent::Intervention(held("q/2", None, false)), now);
        assert_eq!(super::waiting_count(&model), 1, "unarmed and unscored does not");
    }

    #[test]
    fn a_stale_question_is_shown_but_never_counted() {
        let now = Instant::now();
        let mut model = LabModel::default();
        model.apply(ClientEvent::Intervention(held("q/1", Some(0.9), true)), now);
        assert_eq!(super::waiting_count(&model), 1);
        // The connection drops: the question stays on screen but cannot be answered.
        model.apply(offline(now), now);
        assert_eq!(super::waiting_count(&model), 0, "the icon must not ask for an answer nobody can give");
        assert_eq!(super::tray_state(&model), super::TrayState::Idle);
        assert_eq!(super::status_line(&model).as_deref(), Some("The daemon is offline — showing the last picture"));
    }

    #[test]
    fn the_status_line_says_it_in_words() {
        let now = Instant::now();
        let mut model = LabModel::default();
        assert!(super::status_line(&model).is_none(), "nothing to say, nothing said");
        model.apply(ClientEvent::Connection(crate::model::ConnectionState::Online { since: now }), now);
        model.apply(ClientEvent::Telemetry(Box::new(frame(vec![pod("a", PodStatus::Executing), pod("b", PodStatus::Executing)]))), now);
        assert_eq!(super::status_line(&model).as_deref(), Some("2 agents are working"));
        model.apply(ClientEvent::Intervention(held("q/1", Some(0.9), true)), now);
        assert_eq!(super::status_line(&model).as_deref(), Some("One agent is waiting for you"), "a question outranks work in progress");
    }
}
