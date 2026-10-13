//! The Experimental Lab: UI-independent state and the Lab Master's keys.
//!
//! The backend (`backend/lab.py`) sends a full snapshot of every pod on each
//! change; this module holds the newest one and turns keystrokes into extension
//! requests (`_cordata/lab/*`) — it never mutates a pod itself. A pod only
//! changes status when the backend says it did, so what the screen shows is what
//! the daemon's agent loop is actually doing, not what the client hoped happened.
//!
//! Keys (the Lab view open):
//!
//! | key            | effect                                                       |
//! |----------------|--------------------------------------------------------------|
//! | ←/→, h/l, Tab  | select a pod                                                 |
//! | Enter          | inspect: system prompt, tool trace, live stream              |
//! | K              | E-Stop: isolate the selected pod and its sub-agents          |
//! | R              | release an isolated pod                                      |
//! | [ / ]  (- / +) | Laya destructiveness dial −/+ 0.05 (tightens only)           |
//! | I              | inject guidance: type, Enter sends, Esc cancels              |
//! | Y / N          | approve / reject the staged intervention (HITL station)      |
//! | Esc, F2        | collapse the inspector, then close                           |

use acp_core::{LabPod, LabState, PodStatus};
use crate::key::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::app::Effect;
use crate::input::Input;

pub const DIAL_STEP: f64 = 0.05;
/// The backend clamps to this floor; stopping here avoids sending a no-op.
pub const DIAL_MIN: f64 = 0.30;

const ISOLATE: &str = "_cordata/lab/isolate";
const RELEASE: &str = "_cordata/lab/release";
const DIAL: &str = "_cordata/lab/dial";
const INJECT: &str = "_cordata/lab/inject";
const SELECT: &str = "_cordata/lab/select";
const STATE: &str = "_cordata/lab/state";

#[derive(Default)]
pub struct LabView {
    pub open: bool,
    pub state: LabState,
    /// Name of the selected pod (by name, so it survives pods appearing).
    pub selected: Option<String>,
    /// The inspector is expanded for the selected pod.
    pub inspecting: bool,
    /// The guidance prompt box, while it is open.
    pub guidance: Option<Input>,
    /// Last control's outcome: (text, is_error).
    pub notice: Option<(String, bool)>,
    /// A snapshot has arrived (before that the view says it is waiting).
    pub seen: bool,
}

fn ext(method: &str, params: Value, tag: &str) -> Effect {
    Effect::Ext {
        method: method.to_owned(),
        params,
        tag: tag.to_owned(),
    }
}

impl LabView {
    pub fn pods(&self) -> &[LabPod] {
        &self.state.pods
    }

    pub fn selected_index(&self) -> Option<usize> {
        let pods = self.pods();
        if pods.is_empty() {
            return None;
        }
        Some(
            self.selected
                .as_deref()
                .and_then(|name| pods.iter().position(|pod| pod.name == name))
                .unwrap_or(0),
        )
    }

    pub fn selected_pod(&self) -> Option<&LabPod> {
        self.selected_index()
            .and_then(|index| self.pods().get(index))
    }

    /// Pods currently doing something (for the footer).
    pub fn busy(&self) -> usize {
        self.pods()
            .iter()
            .filter(|pod| matches!(pod.status, PodStatus::Synthesizing | PodStatus::Executing))
            .count()
    }

    pub fn on_state(&mut self, state: LabState) {
        // Notifications are written from concurrent tasks, so two can arrive
        // swapped; an older snapshot must not undo a newer one. A different
        // session (or a restarted backend, whose counter starts again) is a
        // fresh sequence.
        if self.seen && state.session_id == self.state.session_id && state.seq < self.state.seq {
            return;
        }
        self.seen = true;
        self.state = state;
        if self.selected_pod().is_none() {
            self.selected = None;
        }
    }

    /// Open or close the view. Opening before any snapshot has arrived asks the
    /// backend for one.
    pub fn toggle(&mut self) -> Option<Effect> {
        self.open = !self.open;
        if !self.open {
            self.guidance = None;
            return self.leave_inspector();
        }
        self.notice = None;
        (!self.seen).then(|| ext(STATE, json!({}), "lab/state"))
    }

    fn leave_inspector(&mut self) -> Option<Effect> {
        if !self.inspecting {
            return None;
        }
        self.inspecting = false;
        Some(ext(SELECT, json!({ "pod": null }), "lab/select"))
    }

    /// The reply to one of this view's requests. Controls report in the view, not
    /// the transcript — a dial turned five times is not five lines of scrollback.
    pub fn on_ext_result(&mut self, tag: &str, result: Result<Value, String>) {
        match (tag, result) {
            ("lab/state", Ok(value)) => {
                if let Some(acp_core::HarnessEvent::Lab(state)) =
                    acp_core::parse_harness(acp_core::LAB_METHOD, &value)
                {
                    self.on_state(*state);
                }
            }
            ("lab/select", Ok(_)) => {}
            (_, Ok(value)) => self.notice = Some((describe(tag, &value), false)),
            (_, Err(error)) => self.notice = Some((format!("{tag}: {error}"), true)),
        }
    }

    fn move_selection(&mut self, delta: isize) -> Option<Effect> {
        let pods = self.pods();
        if pods.is_empty() {
            return None;
        }
        let len = pods.len() as isize;
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        let name = pods[next].name.clone();
        self.selected = Some(name.clone());
        self.inspecting
            .then(|| ext(SELECT, json!({ "pod": name }), "lab/select"))
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if self.guidance.is_some() {
            return self.on_key_guidance(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            return None;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => self.move_selection(1),
            KeyCode::Enter => self.toggle_inspector(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.inspecting {
                    self.leave_inspector()
                } else {
                    self.open = false;
                    None
                }
            }
            KeyCode::F(2) => self.toggle(),
            KeyCode::Char('k' | 'K') => self.estop(),
            KeyCode::Char('r' | 'R') => self.release(),
            KeyCode::Char('[' | '-' | ',') => self.dial(-DIAL_STEP),
            KeyCode::Char(']' | '+' | '=' | '.') => self.dial(DIAL_STEP),
            KeyCode::Char('i' | 'I') => self.open_guidance(),
            _ => None,
        }
    }

    fn toggle_inspector(&mut self) -> Option<Effect> {
        let name = self.selected_pod()?.name.clone();
        if self.inspecting {
            return self.leave_inspector();
        }
        self.inspecting = true;
        self.selected = Some(name.clone());
        Some(ext(SELECT, json!({ "pod": name }), "lab/select"))
    }

    /// E-Stop from the HITL station: the pod named on the pending intervention
    /// (else the selected one). The caller also rejects the pending request —
    /// the agent is parked on it, and an isolation it cannot hear about until the
    /// answer arrives would only take effect after the call it was meant to stop.
    pub fn estop_for_pending(&mut self) -> Option<Effect> {
        let staged = self
            .state
            .hitl
            .iter()
            .rev()
            .find(|row| row.status == "pending")
            .map(|row| row.pod.clone());
        let name = staged.or_else(|| self.selected_pod().map(|pod| pod.name.clone()))?;
        self.notice = Some((format!("E-STOP sent for {name}…"), true));
        Some(ext(ISOLATE, json!({ "pod": name }), "lab/isolate"))
    }

    fn estop(&mut self) -> Option<Effect> {
        let pod = self.selected_pod()?;
        if pod.isolated {
            self.notice = Some((
                format!("{} is already isolated — R releases it", pod.name),
                false,
            ));
            return None;
        }
        let name = pod.name.clone();
        self.notice = Some((format!("E-STOP sent for {name}…"), true));
        Some(ext(ISOLATE, json!({ "pod": name }), "lab/isolate"))
    }

    fn release(&mut self) -> Option<Effect> {
        let pod = self.selected_pod()?;
        if !pod.isolated {
            self.notice = Some((format!("{} is not isolated", pod.name), false));
            return None;
        }
        let name = pod.name.clone();
        Some(ext(RELEASE, json!({ "pod": name }), "lab/release"))
    }

    fn dial(&mut self, delta: f64) -> Option<Effect> {
        let pod = self.selected_pod()?;
        let next = round2((pod.dial + delta).clamp(DIAL_MIN, pod.dial_default));
        if (next - pod.dial).abs() < 1e-9 {
            self.notice = Some((
                format!(
                    "{}'s dial is at its {} ({:.2}) — it can only tighten the gate",
                    pod.name,
                    if delta < 0.0 { "floor" } else { "default" },
                    pod.dial
                ),
                false,
            ));
            return None;
        }
        let name = pod.name.clone();
        Some(ext(DIAL, json!({ "pod": name, "value": next }), "lab/dial"))
    }

    fn open_guidance(&mut self) -> Option<Effect> {
        let pod = self.selected_pod()?;
        if pod.isolated {
            self.notice = Some((
                format!(
                    "{} is isolated — release it (R) before guiding it",
                    pod.name
                ),
                true,
            ));
            return None;
        }
        self.guidance = Some(Input::default());
        None
    }

    fn on_key_guidance(&mut self, key: KeyEvent) -> Option<Effect> {
        let input = self.guidance.as_mut()?;
        match key.code {
            KeyCode::Esc => self.guidance = None,
            KeyCode::Enter => {
                let text = input.take().trim().to_owned();
                self.guidance = None;
                let name = self.selected_pod()?.name.clone();
                if text.is_empty() {
                    return None;
                }
                return Some(ext(
                    INJECT,
                    json!({ "pod": name, "text": text }),
                    "lab/inject",
                ));
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            _ => {}
        }
        None
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn describe(tag: &str, value: &Value) -> String {
    let pod = value.get("pod").and_then(Value::as_str).unwrap_or("?");
    match tag {
        "lab/isolate" => {
            let also: Vec<&str> = value
                .get("also")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if also.is_empty() {
                format!("{pod} ISOLATED — every model and tool call is refused")
            } else {
                format!(
                    "{pod} ISOLATED, with {} — every model and tool call is refused",
                    also.join(", ")
                )
            }
        }
        "lab/release" => format!("{pod} released"),
        "lab/dial" => format!(
            "{pod} Laya dial → {:.2}",
            value.get("dial").and_then(Value::as_f64).unwrap_or(0.0)
        ),
        "lab/inject" => format!("guidance queued for {pod} — delivered before its next model call"),
        other => format!("{other}: ok"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn pod(name: &str, status: PodStatus) -> LabPod {
        LabPod {
            id: name.into(),
            name: name.into(),
            role: name.into(),
            status,
            dial: 0.8,
            dial_default: 0.8,
            ..LabPod::default()
        }
    }

    fn view() -> LabView {
        let mut view = LabView::default();
        view.on_state(LabState {
            pods: vec![
                pod("orchestrator", PodStatus::Executing),
                pod("architect", PodStatus::Idle),
                pod("debugger", PodStatus::Contained),
            ],
            ..LabState::default()
        });
        view.open = true;
        view
    }

    fn call(effect: Option<Effect>) -> (String, Value, String) {
        match effect.expect("an effect") {
            Effect::Ext {
                method,
                params,
                tag,
            } => (method, params, tag),
            other => panic!("expected Ext, got {other:?}"),
        }
    }

    #[test]
    fn selection_walks_the_pods_and_wraps() {
        let mut view = view();
        assert_eq!(view.selected_pod().unwrap().name, "orchestrator");
        view.on_key(key(KeyCode::Right));
        view.on_key(key(KeyCode::Char('l')));
        assert_eq!(view.selected_pod().unwrap().name, "debugger");
        view.on_key(key(KeyCode::Tab));
        assert_eq!(
            view.selected_pod().unwrap().name,
            "orchestrator",
            "wraps forward"
        );
        view.on_key(key(KeyCode::Left));
        assert_eq!(
            view.selected_pod().unwrap().name,
            "debugger",
            "and backward"
        );
    }

    #[test]
    fn k_is_the_estop_for_the_selected_pod_and_only_a_request() {
        let mut view = view();
        view.on_key(key(KeyCode::Right));
        let (method, params, tag) = call(view.on_key(key(KeyCode::Char('K'))));
        assert_eq!(method, "_cordata/lab/isolate");
        assert_eq!(params, json!({"pod": "architect"}));
        assert_eq!(tag, "lab/isolate");
        assert_eq!(
            view.pods()[1].status,
            PodStatus::Idle,
            "the view never flips a pod itself — the backend says when it is isolated"
        );
        let (method, ..) = call(view.on_key(key(KeyCode::Char('k'))));
        assert_eq!(method, "_cordata/lab/isolate", "lower-case k works too");
    }

    #[test]
    fn the_hitl_estop_names_the_pod_on_the_pending_intervention() {
        let mut view = view();
        view.state.hitl = vec![acp_core::LabHitl {
            id: "i1".into(),
            pod: "debugger".into(),
            status: "pending".into(),
            ..acp_core::LabHitl::default()
        }];
        let (method, params, _) = call(view.estop_for_pending());
        assert_eq!(method, "_cordata/lab/isolate");
        assert_eq!(
            params,
            json!({"pod": "debugger"}),
            "the staged row's pod, not the selection"
        );
        view.state.hitl.clear();
        let (_, params, _) = call(view.estop_for_pending());
        assert_eq!(
            params,
            json!({"pod": "orchestrator"}),
            "falls back to the selected pod"
        );
    }

    #[test]
    fn an_isolated_pod_offers_release_not_a_second_estop() {
        let mut view = view();
        view.state.pods[0].isolated = true;
        view.state.pods[0].status = PodStatus::Isolated;
        assert!(view.on_key(key(KeyCode::Char('K'))).is_none());
        assert!(view.notice.as_ref().unwrap().0.contains("already isolated"));
        let (method, params, _) = call(view.on_key(key(KeyCode::Char('R'))));
        assert_eq!(method, "_cordata/lab/release");
        assert_eq!(params, json!({"pod": "orchestrator"}));
        view.state.pods[1].isolated = false;
        view.on_key(key(KeyCode::Right));
        assert!(
            view.on_key(key(KeyCode::Char('r'))).is_none(),
            "nothing to release"
        );
    }

    #[test]
    fn the_dial_steps_by_a_twentieth_and_only_tightens() {
        let mut view = view();
        let (method, params, _) = call(view.on_key(key(KeyCode::Char('['))));
        assert_eq!(method, "_cordata/lab/dial");
        assert_eq!(params, json!({"pod": "orchestrator", "value": 0.75}));
        assert!(
            view.on_key(key(KeyCode::Char(']'))).is_none(),
            "at the default (0.80) there is nowhere looser to go"
        );
        assert!(view.notice.as_ref().unwrap().0.contains("only tighten"));
        view.state.pods[0].dial = 0.5;
        let (_, params, _) = call(view.on_key(key(KeyCode::Char('+'))));
        assert_eq!(params["value"], json!(0.55));
        view.state.pods[0].dial = DIAL_MIN;
        assert!(view.on_key(key(KeyCode::Char('-'))).is_none(), "floor");
        assert!(view.notice.as_ref().unwrap().0.contains("floor"));
    }

    #[test]
    fn guidance_is_typed_in_a_box_and_sent_on_enter() {
        let mut view = view();
        view.on_key(key(KeyCode::Right));
        view.on_key(key(KeyCode::Right));
        assert!(view.on_key(key(KeyCode::Char('i'))).is_none());
        assert!(view.guidance.is_some(), "the prompt box opens");
        for c in "stop using sudo".chars() {
            // 'k', 'r', 'i'... typed into the box are text, not commands
            assert!(view.on_key(key(KeyCode::Char(c))).is_none());
        }
        let (method, params, tag) = call(view.on_key(key(KeyCode::Enter)));
        assert_eq!(method, "_cordata/lab/inject");
        assert_eq!(
            params,
            json!({"pod": "debugger", "text": "stop using sudo"})
        );
        assert_eq!(tag, "lab/inject");
        assert!(view.guidance.is_none());

        view.on_key(key(KeyCode::Char('i')));
        view.on_key(key(KeyCode::Char('x')));
        assert!(view.on_key(key(KeyCode::Esc)).is_none());
        assert!(view.guidance.is_none(), "Esc cancels without sending");
        view.on_key(key(KeyCode::Char('i')));
        assert!(
            view.on_key(key(KeyCode::Enter)).is_none(),
            "an empty directive sends nothing"
        );
    }

    #[test]
    fn guidance_to_an_isolated_pod_is_refused_locally() {
        let mut view = view();
        view.state.pods[0].isolated = true;
        assert!(view.on_key(key(KeyCode::Char('i'))).is_none());
        assert!(view.guidance.is_none());
        assert!(view.notice.as_ref().unwrap().1, "flagged as an error");
    }

    #[test]
    fn enter_inspects_and_esc_steps_back_out_before_closing() {
        let mut view = view();
        let (method, params, _) = call(view.on_key(key(KeyCode::Enter)));
        assert_eq!(method, "_cordata/lab/select");
        assert_eq!(params, json!({"pod": "orchestrator"}));
        assert!(view.inspecting);
        let (_, params, _) = call(view.on_key(key(KeyCode::Right)));
        assert_eq!(
            params,
            json!({"pod": "architect"}),
            "moving while inspecting follows the selection"
        );
        let (_, params, _) = call(view.on_key(key(KeyCode::Esc)));
        assert_eq!(
            params,
            json!({"pod": null}),
            "Esc collapses the inspector first"
        );
        assert!(view.open && !view.inspecting);
        assert!(view.on_key(key(KeyCode::Esc)).is_none());
        assert!(!view.open, "a second Esc closes the Lab");
    }

    #[test]
    fn a_stale_snapshot_does_not_undo_a_newer_one() {
        let mut view = LabView::default();
        let snapshot = |seq: u64, status: PodStatus, session: &str| LabState {
            seq,
            session_id: Some(session.into()),
            pods: vec![pod("debugger", status)],
            ..LabState::default()
        };
        view.on_state(snapshot(5, PodStatus::Contained, "a"));
        view.on_state(snapshot(4, PodStatus::Idle, "a"));
        assert_eq!(
            view.pods()[0].status,
            PodStatus::Contained,
            "seq 4 arrived after seq 5"
        );
        view.on_state(snapshot(6, PodStatus::Idle, "a"));
        assert_eq!(view.pods()[0].status, PodStatus::Idle);
        view.on_state(snapshot(1, PodStatus::Executing, "b"));
        assert_eq!(
            view.pods()[0].status,
            PodStatus::Executing,
            "another session starts its own sequence"
        );
    }

    #[test]
    fn opening_before_any_snapshot_asks_for_one() {
        let mut view = LabView::default();
        let (method, _, tag) = call(view.toggle());
        assert_eq!(
            (method.as_str(), tag.as_str()),
            ("_cordata/lab/state", "lab/state")
        );
        assert!(view.open);
        let mut seen = LabView {
            seen: true,
            ..LabView::default()
        };
        assert!(
            seen.toggle().is_none(),
            "no request once a snapshot has arrived"
        );
    }

    #[test]
    fn replies_become_notices_and_a_state_reply_becomes_state() {
        let mut view = view();
        view.on_ext_result(
            "lab/isolate",
            Ok(json!({"pod": "orchestrator", "isolated": true, "also": ["architect", "debugger"]})),
        );
        let (text, error) = view.notice.clone().unwrap();
        assert!(text.contains("ISOLATED, with architect, debugger") && !error);
        view.on_ext_result("lab/dial", Ok(json!({"pod": "orchestrator", "dial": 0.5})));
        assert!(view.notice.as_ref().unwrap().0.contains("dial → 0.50"));
        view.on_ext_result("lab/inject", Err("orchestrator is isolated".into()));
        assert!(view.notice.as_ref().unwrap().1, "an error reply is flagged");
        view.on_ext_result(
            "lab/state",
            Ok(json!({"event": "state", "seq": 9, "pods": [{"name": "solo", "status": "executing"}]})),
        );
        assert_eq!(view.state.seq, 9);
        assert_eq!(view.busy(), 1);
    }
}
