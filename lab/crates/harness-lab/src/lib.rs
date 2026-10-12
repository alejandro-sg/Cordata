//! The Experimental Lab: the window, its theme, and its widgets, as a library.
//!
//! This crate is both the Lab binary (`harness-lab`) and a library, so a second front door — the
//! menu-bar app — can reuse the same visual language instead of re-deriving it. The view is a *view*:
//! the Python daemon decides everything about an agent, and this draws what it decided.
//!
//! * [`app`]   — `LabApp`: the window's entity, its tabs, and what a keystroke does to it
//! * [`theme`] — the design tokens, and the pure mappings from state to colour
//! * [`ui`]    — what is drawn: header, canvas, side column, overlays, and the widgets they share

pub mod app;
pub mod theme;
pub mod ui;

/// The workspace the Lab's observer watches. The first `--watch` wins; `CORDATA_LAB_WATCH` is the
/// environment's way to say the same thing; otherwise the directory the app was started in. Here
/// rather than in the binary so it is testable, and so the menu bar can read the same rule.
pub fn workspace_root(args: impl Iterator<Item = String>) -> std::path::PathBuf {
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--watch" {
            if let Some(path) = args.next() {
                return std::path::PathBuf::from(path);
            }
        } else if let Some(path) = arg.strip_prefix("--watch=") {
            return std::path::PathBuf::from(path);
        }
    }
    if let Some(path) = std::env::var_os("CORDATA_LAB_WATCH").filter(|p| !p.is_empty()) {
        return std::path::PathBuf::from(path);
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use gpui::{TestAppContext, px, size};
    use harness_client::{ClientEvent, ConnectionState};
    use harness_proto::v1::{DiffRef, DiffSet, InterventionEvent, InterventionRequest, LabState, Link, OmlxModel, OmlxState, Pod, PodStatus, TelemetryFrame, Threat, TraceRow, intervention_event};

    use crate::app::{DiffView, LabApp, Tab};
    use crate::workspace_root;

    fn bare(cx: &mut TestAppContext) -> (gpui::Entity<LabApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|_window, cx| LabApp::new(Err("offline".into()), PathBuf::from("."), cx))
    }

    fn pod(id: &str, role: &str, parent: &str, status: PodStatus) -> Pod {
        Pod {
            id: id.into(),
            name: id.into(),
            role: role.into(),
            parent: parent.into(),
            model: "glm-5.3-flash:cloud".into(),
            effort: "low".into(),
            status: status as i32,
            dial: 0.8,
            dial_default: 0.8,
            calls: 12,
            busy_ms: 3400,
            effort_adjustable: role == "orchestrator",
            pending_tool: if status == PodStatus::Executing { "read_file: backend/daemon/supervisor.py".into() } else { String::new() },
            stream_tail: "Refactoring the supervisor so the session lookup no longer clones the table".into(),
            trace: vec![
                TraceRow { tool: "shell".into(), status: "completed".into(), input: "ls -la".into(), ms: 40, ts_ms: 1 },
                TraceRow { tool: "read_file".into(), status: "running".into(), input: "x.py".into(), ms: 0, ts_ms: 2 },
            ],
            risk_last: Some(0.35),
            risk_peak: 0.6,
            auto_approved: 3,
            asked: 1,
            ..Default::default()
        }
    }

    fn team_frame() -> TelemetryFrame {
        TelemetryFrame {
            session_id: "s1".into(),
            lab: Some(LabState {
                lab_seq: 9,
                focus: "orchestrator".into(),
                default_dial: 0.8,
                mode: "build".into(),
                mission: "Refactor the supervisor so the session lookup stops copying the table".into(),
                pods: vec![
                    pod("orchestrator", "orchestrator", "", PodStatus::Executing),
                    pod("architect", "architect", "orchestrator", PodStatus::Idle),
                    pod("claude_code_engineer", "claude_code_engineer", "orchestrator", PodStatus::Contained),
                ],
                links: vec![Link { from: "orchestrator".into(), to: "architect".into(), kind: "delegation".into(), active: true, count: 3, ..Default::default() }],
                threat: Some(Threat { pod: "claude_code_engineer".into(), tool: "shell".into(), score: Some(0.62), confidence: Some(0.9), band: "elevated".into(), pending: false }),
                ..Default::default()
            }),
            omlx: Some(OmlxState {
                up: true,
                budget_gb: 38.0,
                used_gb: 17.0,
                free_gb: 21.0,
                models: vec![
                    OmlxModel { id: "Ternary-Bonsai-2-27B-mlx-2bit".into(), loaded: true, size_gb: 8.41, pinned: false },
                    OmlxModel { id: "Qwen2.5-Coder-7B-Instruct".into(), loaded: true, size_gb: 8.5, pinned: false },
                ],
                ..Default::default()
            }),
            diffs: Some(DiffSet { items: vec![DiffRef { id: "d1".into(), source: "claude_code_engineer".into(), files: vec!["backend/supervisor.py".into(), "backend/server.py".into()], lines: 10_000, ..Default::default() }] }),
            ..Default::default()
        }
    }

    fn feed(view: &gpui::Entity<LabApp>, vcx: &mut gpui::VisualTestContext, frame: TelemetryFrame) {
        view.update(vcx, |app, cx| {
            app.model.apply(ClientEvent::Telemetry(Box::new(frame)), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
    }

    fn online(view: &gpui::Entity<LabApp>, vcx: &mut gpui::VisualTestContext) {
        view.update(vcx, |app, cx| {
            app.model.apply(ClientEvent::Connection(ConnectionState::Online { since: Instant::now() }), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
    }

    fn notices(view: &gpui::Entity<LabApp>, vcx: &mut gpui::VisualTestContext) -> Vec<String> {
        view.read_with(vcx, |app, _| app.model.notices.iter().map(|n| n.text.clone()).collect())
    }

    fn destructive(id: &str) -> ClientEvent {
        ClientEvent::Intervention(InterventionEvent {
            event: Some(intervention_event::Event::Request(InterventionRequest {
                intervention_id: id.into(),
                session_id: "s1".into(),
                pod: "debugger".into(),
                tool: "shell".into(),
                command: "rm -rf build".into(),
                armed: true,
                risk_score: Some(0.93),
                risk_band: "destructive".into(),
                redactions: 2,
                ..Default::default()
            })),
        })
    }

    fn elevated(id: &str) -> ClientEvent {
        ClientEvent::Intervention(InterventionEvent {
            event: Some(intervention_event::Event::Request(InterventionRequest {
                intervention_id: id.into(),
                session_id: "s1".into(),
                pod: "claude_code_engineer".into(),
                tool: "apply_patch".into(),
                diff: "--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-old\n+new\n".into(),
                risk_score: Some(0.58),
                risk_band: "elevated".into(),
                armed: false,
                ..Default::default()
            })),
        })
    }

    // ------------------------------------------------------------------------ the frame

    /// A workspace with no daemon: the window must still open and draw, and say why.
    #[gpui::test]
    async fn the_window_draws_with_no_daemon_and_says_so(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        vcx.run_until_parked();
        for part in ["lab-root", "lab-header", "lab-banner", "lab-canvas", "lab-side", "lab-composer", "empty-team", "team-state"] {
            assert!(vcx.debug_bounds(part).is_some(), "{part} did not lay out");
        }
        assert!(vcx.debug_bounds("containment-modal").is_none(), "nothing is open, so nothing may interrupt");
        assert!(vcx.window_title().unwrap_or_default().contains("no daemon config"), "{:?}", vcx.window_title());
        let _ = view;
    }

    /// The window answers its four questions in the order a person asks them: the header, the
    /// banner, then the canvas with the side column beside it — and the command deck floating over
    /// the foot of the canvas rather than taking a row of its own.
    #[gpui::test]
    async fn the_regions_stack_in_priority_order_and_nothing_overlaps(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        let (header, banner, canvas, side, dock, root) = (
            vcx.debug_bounds("lab-header").unwrap(),
            vcx.debug_bounds("lab-banner").unwrap(),
            vcx.debug_bounds("lab-canvas").unwrap(),
            vcx.debug_bounds("lab-side").unwrap(),
            vcx.debug_bounds("lab-dock").unwrap(),
            vcx.debug_bounds("lab-root").unwrap(),
        );
        assert!(header.bottom() <= banner.origin.y + px(0.5), "the header sits above the banner");
        assert!(banner.bottom() <= canvas.origin.y + px(0.5) && banner.bottom() <= side.origin.y + px(0.5), "the banner sits above the canvas and the side column");
        assert!(canvas.right() <= side.origin.x + px(1.5), "the canvas and the side column do not overlap");
        assert_eq!(f32::from(side.size.width), 410.0, "the side column keeps its width");
        // The deck is anchored to the foot of the canvas and centred on it, and does not reach the
        // side column.
        assert!(dock.bottom() <= root.bottom() + px(0.5), "the command deck is not cut off");
        assert!((dock.bottom() - canvas.bottom()).abs() < px(24.), "the command deck sits at the foot of the canvas: {dock:?} vs {canvas:?}");
        assert!(dock.origin.x >= canvas.origin.x && dock.right() <= side.origin.x + px(0.5), "the command deck stays over the canvas, clear of the side column");
        let dock_middle = (dock.origin.x + dock.right()) / 2.0;
        let canvas_middle = (canvas.origin.x + canvas.right()) / 2.0;
        assert!((dock_middle - canvas_middle).abs() < px(1.0), "the command deck is centred on the canvas");
    }

    // ------------------------------------------------------------------- the canvas

    /// The canvas places the team by shape: the orchestrator on the top row, its children a tier
    /// below and side by side, nothing overlapping, everything inside the surface that was laid out.
    #[gpui::test]
    async fn the_team_is_drawn_as_a_canvas_with_the_orchestrator_on_the_top_row(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        let layer = vcx.debug_bounds("lab-canvas-layer").unwrap();
        let root = vcx.debug_bounds("pod-orchestrator").expect("the orchestrator's pod");
        let architect = vcx.debug_bounds("pod-architect").expect("the architect's pod");
        let claude = vcx.debug_bounds("pod-claude_code_engineer").expect("claude code's pod");

        assert!(root.origin.y + root.size.height <= architect.origin.y, "the orchestrator sits on the row above its children");
        assert_eq!(architect.origin.y, claude.origin.y, "siblings share a row");
        assert!(architect.right() <= claude.origin.x, "and do not overlap on it");
        let (a, b) = ((architect.origin.x + architect.right()) / 2.0, (claude.origin.x + claude.right()) / 2.0);
        assert!((a + b) / 2.0 - (root.origin.x + root.right()) / 2.0 < px(1.0), "the orchestrator is centred over them");

        for id in ["pod-orchestrator", "pod-architect", "pod-claude_code_engineer"] {
            let b = vcx.debug_bounds(id).unwrap();
            assert!(b.origin.x >= layer.origin.x - px(0.5) && b.right() <= layer.right() + px(0.5), "{id} sticks out of the canvas surface");
            assert!(b.origin.y >= layer.origin.y - px(0.5) && b.bottom() <= layer.bottom() + px(0.5), "{id} runs off the bottom of the canvas");
        }
        assert!(vcx.debug_bounds("risk-meter").is_some(), "an agent Laya has scored carries its meter");
        assert!(vcx.debug_bounds("lab-conduits").is_some(), "the conduits are drawn under the pods");
    }

    /// Telemetry binding: the state the daemon reports is the state the card draws. A pod that is
    /// running shows its busy strip; an idle pod and one waiting on a human do not, even though the
    /// frame gave all three a busy time — the strip follows the status, not the number.
    #[gpui::test]
    async fn a_pod_draws_the_state_the_daemon_reported(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        assert!(vcx.debug_bounds("busy-orchestrator").is_some(), "an executing pod shows what it is running");
        assert!(vcx.debug_bounds("busy-architect").is_none(), "an idle pod shows no busy strip");
        assert!(vcx.debug_bounds("busy-claude_code_engineer").is_none(), "a contained pod is waiting, not working");

        // The same three states, moved: the strip follows the status rather than staying put.
        let mut moved = team_frame();
        let pods = &mut moved.lab.as_mut().unwrap().pods;
        pods[0].status = PodStatus::Idle as i32;
        pods[1].status = PodStatus::Synthesizing as i32;
        feed(&view, vcx, moved);
        assert!(vcx.debug_bounds("busy-orchestrator").is_none(), "a pod that stopped working loses its strip");
        assert!(vcx.debug_bounds("busy-architect").is_some(), "a pod that started thinking gains one");
    }

    #[gpui::test]
    async fn the_banner_leads_with_the_team_state_and_the_mission(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        online(&view, vcx);
        for part in ["team-state", "mission", "threat-gauge", "threat-score", "memory-ring", "button-unload-memory", "banner-safety"] {
            let b = vcx.debug_bounds(part).unwrap_or_else(|| panic!("{part} is not in the banner"));
            let banner = vcx.debug_bounds("lab-banner").unwrap();
            assert!(b.origin.y >= banner.origin.y && b.bottom() <= banner.bottom() + px(0.5) && b.right() <= banner.right() + px(0.5), "{part} runs out of the banner: {b:?} vs {banner:?}");
        }
        // An agent waiting on a person makes the whole team "Needs you" — checked on the pure function.
        let lab = view.read_with(vcx, |app, _| app.model.lab().cloned());
        let summary = harness_client::words::team_summary(lab.as_ref(), 0, true);
        assert_eq!(summary.headline(), "Needs you");
    }

    // ------------------------------------------------------------------ autonomy + steering

    #[gpui::test]
    async fn the_autonomy_selector_acts_only_when_it_can(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        for id in ["segment-mode-plan", "segment-mode-build", "segment-mode-auto"] {
            assert!(vcx.debug_bounds(id).is_some(), "{id} is not drawn");
        }
        // Offline: the keyboard path reports instead of pretending.
        vcx.simulate_keystrokes("m");
        vcx.run_until_parked();
        assert!(notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "{:?}", notices(&view, vcx));
        // The mode comes from the daemon's telemetry, not from a local guess.
        assert_eq!(view.read_with(vcx, |app, _| app.current_mode().map(str::to_owned)).as_deref(), Some("build"));
        let mut auto = team_frame();
        auto.lab.as_mut().unwrap().mode = "auto".into();
        feed(&view, vcx, auto);
        assert_eq!(view.read_with(vcx, |app, _| app.current_mode().map(str::to_owned)).as_deref(), Some("auto"));
        assert!(view.read_with(vcx, |app, _| app.model.feed.iter().any(|f| f.text == "Mode is now auto")), "switching to auto is in the activity feed");
    }

    /// Typing into the steer bar can never answer an approval: every printable key goes into the text.
    #[gpui::test]
    async fn typing_in_the_steer_bar_cannot_approve_anything(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        view.update(vcx, |app, cx| {
            app.model.apply(destructive("d1"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("containment-modal").is_some(), "a destructive request must interrupt by itself");

        vcx.simulate_keystrokes("/"); // take the keyboard
        assert!(view.read_with(vcx, |app, _| app.composing), "/ opens the steer bar");
        vcx.simulate_keystrokes("y e s space n k");
        vcx.run_until_parked();
        assert_eq!(view.read_with(vcx, |app, _| app.composer.clone()), "yes nk", "the keys went into the text");
        assert!(!notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "typing y/n/k answered a request: {:?}", notices(&view, vcx));
        assert_eq!(view.read_with(vcx, |app, _| app.model.interventions.len()), 1, "the question is still open");

        vcx.simulate_keystrokes("backspace backspace");
        assert_eq!(view.read_with(vcx, |app, _| app.composer.clone()), "yes ");
        vcx.simulate_keystrokes("escape");
        assert!(view.read_with(vcx, |app, _| !app.composing), "esc gives the keyboard back first");
        assert!(vcx.debug_bounds("containment-modal").is_some(), "…and does not also dismiss the modal");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.modal_intervention().is_none()), "the second esc sends the modal to the tray");
    }

    #[gpui::test]
    async fn a_directive_goes_to_the_selected_agent_and_is_reported_when_there_is_no_daemon(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        view.update(vcx, |app, cx| {
            app.selected = Some("architect".into());
            cx.notify();
        });
        assert_eq!(view.read_with(vcx, |app, _| app.steer_target()).as_deref(), Some("architect"));
        vcx.simulate_keystrokes("/ f o c u s");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "the directive was not silently dropped: {:?}", notices(&view, vcx));
        // An empty bar sends nothing.
        vcx.simulate_keystrokes("escape");
        let before = notices(&view, vcx).len();
        vcx.simulate_keystrokes("/ enter");
        assert_eq!(notices(&view, vcx).len(), before, "an empty directive is not sent");
    }

    // -------------------------------------------------------------------- the agent drawer

    #[gpui::test]
    async fn the_drawer_offers_controls_only_when_they_can_act(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        let drawer = vcx.debug_bounds("lab-drawer").expect("drawer");
        let side = vcx.debug_bounds("lab-side").unwrap();
        assert!(drawer.origin.x >= side.origin.x && drawer.right() <= side.right() + px(0.5), "the drawer fits its column");
        assert!(vcx.debug_bounds("segment-effort-low").is_some() && vcx.debug_bounds("laya-dial").is_some() && vcx.debug_bounds("guard-line").is_some(), "the knobs and Laya Guard's line are in the drawer");
        assert!(vcx.debug_bounds("button-stop-orchestrator").is_none(), "offline, there is nothing a Stop button could do, so it is not offered");
        online(&view, vcx);
        assert!(vcx.debug_bounds("button-stop-orchestrator").is_some(), "online, Stop is there — and says it takes the sub-agents with it");
        // A stopped agent offers Release instead.
        let mut stopped = team_frame();
        stopped.lab.as_mut().unwrap().pods[0].isolated = true;
        feed(&view, vcx, stopped);
        assert!(vcx.debug_bounds("button-release-orchestrator").is_some() && vcx.debug_bounds("button-stop-orchestrator").is_none());
        assert!(view.read_with(vcx, |app, _| app.model.feed.iter().any(|f| f.text == "orchestrator was stopped")), "being stopped is in the feed");
    }

    // --------------------------------------------------------------------------- the tabs

    #[gpui::test]
    async fn one_tab_at_a_time_and_the_noise_lives_behind_system(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        // (Absence is asserted only for things never drawn: GPUI's test harness keeps the bounds of
        // elements that have since gone, so "disappeared" is asserted through the app's state.)
        assert!(vcx.debug_bounds("activity").is_some() && vcx.debug_bounds("system").is_none(), "Activity is the default and nothing diagnostic is shown");
        assert!(vcx.debug_bounds("button-unload-ollama").is_none() && vcx.debug_bounds("diff-row-d1").is_none(), "the rest is not competing for attention");
        view.update(vcx, |app, cx| app.set_tab(Tab::System, cx));
        vcx.run_until_parked();
        assert_eq!(view.read_with(vcx, |app, _| app.tab), Tab::System);
        assert!(vcx.debug_bounds("system").is_some(), "System is drawn");
        assert!(vcx.debug_bounds("button-unload-ollama").is_some() && vcx.debug_bounds("threat-gauge").is_some(), "System holds the full gauge and the memory controls");
        view.update(vcx, |app, cx| app.set_tab(Tab::Outputs, cx));
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("diff-row-d1").is_some(), "Outputs lists the diff…");
        assert!(view.read_with(vcx, |app, _| app.diff.is_none()), "…and never opens it by itself");
    }

    // --------------------------------------------------------------------- the interruptions

    /// The containment modal is an *overlay*: a scrim over the whole window with the modal centred on
    /// it. It takes no room in the layout — the board and side column keep their size.
    #[gpui::test]
    async fn the_containment_modal_overlays_the_window_instead_of_taking_layout_space(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        let (body, composer, root) = (vcx.debug_bounds("lab-body").unwrap(), vcx.debug_bounds("lab-composer").unwrap(), vcx.debug_bounds("lab-root").unwrap());
        assert!(vcx.debug_bounds("modal-scrim").is_none());
        view.update(vcx, |app, cx| {
            app.model.apply(destructive("iv1"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert_eq!(vcx.debug_bounds("lab-body"), Some(body), "the modal pushed the board around");
        assert_eq!(vcx.debug_bounds("lab-composer"), Some(composer), "the modal pushed the deck's input around");
        assert_eq!(vcx.debug_bounds("modal-scrim").expect("no scrim"), root, "the scrim must cover the whole window");
        let modal = vcx.debug_bounds("containment-modal").expect("no modal");
        let (mx, my) = (modal.origin.x + modal.size.width / 2.0, modal.origin.y + modal.size.height / 2.0);
        let (rx, ry) = (root.origin.x + root.size.width / 2.0, root.origin.y + root.size.height / 2.0);
        assert!((f32::from(mx) - f32::from(rx)).abs() < 1.0 && (f32::from(my) - f32::from(ry)).abs() < 1.0, "not centred: {modal:?} in {root:?}");
        for button in ["button-approve", "button-reject", "button-isolate", "button-later"] {
            let b = vcx.debug_bounds(button).unwrap_or_else(|| panic!("{button} is not drawn"));
            assert!(b.origin.x >= modal.origin.x && b.right() <= modal.right() && b.bottom() <= modal.bottom(), "{button} falls outside the modal");
        }
        // Split pane: the impact summary on the left, the payload it is about on the right, and the
        // payload keeps the room — a summary may never push the thing being approved off the modal.
        let (impact, payload) = (vcx.debug_bounds("modal-impact").expect("no impact pane"), vcx.debug_bounds("modal-payload").expect("no payload pane"));
        for (name, b) in [("modal-impact", impact), ("modal-payload", payload)] {
            assert!(b.origin.x >= modal.origin.x && b.right() <= modal.right() && b.bottom() <= modal.bottom(), "{name} falls outside the modal: {b:?} vs {modal:?}");
            assert!(f32::from(b.size.width) > 4.0 && f32::from(b.size.height) > 4.0, "{name} has no size: {b:?}");
        }
        assert!(impact.right() <= payload.origin.x + px(0.5), "the impact pane and the payload overlap");
        assert!(f32::from(payload.size.width) > f32::from(impact.size.width), "the payload must be the wider pane");
    }

    /// An ordinary question is a card in "Needs you", not a wall: the whole window stays usable.
    #[gpui::test]
    async fn an_elevated_request_waits_in_needs_you_and_blocks_nothing(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        view.update(vcx, |app, _| app.config_error = None); // a real session has a config; "no config" is itself a Needs-you card
        feed(&view, vcx, team_frame());
        let body = vcx.debug_bounds("lab-body").unwrap();
        assert!(vcx.debug_bounds("needs-you").is_none(), "nothing needs you, so the section is not there");
        view.update(vcx, |app, cx| {
            app.model.apply(elevated("e1"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.modal_intervention().is_none()), "an elevated request took over the window");
        let needs = vcx.debug_bounds("needs-you").expect("Needs you is missing");
        let side = vcx.debug_bounds("lab-side").unwrap();
        assert!(needs.origin.x >= side.origin.x && needs.right() <= side.right() + px(0.5), "the section sits in the side column");
        let drawer = vcx.debug_bounds("lab-drawer").unwrap();
        assert!(needs.bottom() <= drawer.origin.y + px(0.5), "Needs you is above everything else in the column");
        assert_eq!(vcx.debug_bounds("lab-body"), Some(body), "the card did not move the layout");
        for id in ["button-tray-approve-e1", "button-tray-reject-e1", "button-tray-review-e1"] {
            assert!(vcx.debug_bounds(id).is_some(), "{id} is not drawn");
        }
        assert!(vcx.debug_bounds("pod-orchestrator").is_some() && vcx.debug_bounds("button-unload-memory").is_some(), "the board and its controls are all still there");

        // `y` answers only what is on screen in the modal: with only a card, it does nothing at all.
        vcx.simulate_keystrokes("y");
        vcx.run_until_parked();
        assert!(!notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "a keypress answered a request nobody opened");
        vcx.simulate_keystrokes("r");
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("containment-modal").is_some(), "r opens the review");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.modal_intervention().is_none()) && vcx.debug_bounds("needs-you").is_some());
        assert_eq!(view.read_with(vcx, |app, _| app.model.interventions.len()), 1, "dismissing a view must not answer it");
    }

    /// `esc` on the destructive modal moves it to Needs you — it does not answer it, and it does not
    /// pop back up on the next redraw. The agent stays suspended throughout.
    #[gpui::test]
    async fn decide_later_sends_a_destructive_modal_to_needs_you_without_answering_it(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        view.update(vcx, |app, cx| {
            app.model.apply(destructive("d1"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("containment-modal").is_some() && vcx.debug_bounds("button-later").is_some());
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.modal_intervention().is_none()) && vcx.debug_bounds("tray-d1").is_some(), "it moved to Needs you");
        for _ in 0..3 {
            view.update(vcx, |_, cx| cx.notify());
            vcx.run_until_parked();
        }
        assert!(view.read_with(vcx, |app, _| app.modal_intervention().is_none()), "the modal popped back up by itself");
        let (pending, title) = view.read_with(vcx, |app, _| (app.model.interventions.len(), crate::app::window_title(app)));
        assert_eq!(pending, 1, "the question is still open");
        assert!(title.contains("1 approval waiting"), "{title}");
        assert!(!notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "dismissing sent an answer");
    }

    #[gpui::test]
    async fn an_open_request_interrupts_and_a_decision_without_a_daemon_is_reported(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        view.update(vcx, |app, cx| {
            app.model.apply(destructive("iv1"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("containment-modal").is_some(), "the request did not interrupt");
        vcx.simulate_keystrokes("y");
        vcx.run_until_parked();
        assert!(notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")), "{:?}", notices(&view, vcx));
    }

    #[gpui::test]
    async fn queued_requests_are_answered_one_at_a_time(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        view.update(vcx, |app, cx| {
            app.model.apply(destructive("iv1"), Instant::now());
            app.model.apply(destructive("iv2"), Instant::now());
            cx.notify();
        });
        vcx.run_until_parked();
        assert_eq!(view.read_with(vcx, |app, _| app.modal_intervention().map(|o| o.request.intervention_id.clone())), Some("iv1".into()));
        assert!(vcx.debug_bounds("containment-modal").is_some());
    }

    // ----------------------------------------------------------------------------- diffs

    #[gpui::test]
    async fn the_diff_overlay_opens_and_escape_closes_it(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        view.update(vcx, |app, cx| {
            let mut diff = DiffView::new("d1".into(), "claude_code_engineer".into(), vec!["src/lib.rs".into()], 3, true);
            diff.set_body("@@ -1 +1 @@\n-old\n+new\n".into());
            app.diff = Some(diff);
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("diff-overlay").is_some(), "the diff did not open");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.diff.is_none()), "escape did not close the diff");
    }

    /// A diff the daemon merely *references* is listed, never opened on its own — and `esc` keeps it closed.
    #[gpui::test]
    async fn a_referenced_diff_is_listed_not_forced_open(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        assert!(vcx.debug_bounds("diff-overlay").is_none(), "a referenced diff opened by itself");
        vcx.simulate_keystrokes("d");
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("diff-overlay").is_some(), "d did not open the newest diff");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        view.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
        assert!(view.read_with(vcx, |app, _| app.diff.is_none()), "the overlay came back by itself");
    }

    /// A 10,000-line diff must open without a hitch: the body is split once and only the rows in view
    /// are laid out, so opening and redrawing cost the same as a short diff.
    #[gpui::test]
    async fn a_ten_thousand_line_diff_opens_and_redraws_quickly(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        let body: String = (0..10_000).map(|n| if n % 3 == 0 { format!("+added line {n}\n") } else if n % 3 == 1 { format!("-removed line {n}\n") } else { format!(" context {n}\n") }).collect();
        let opened = Instant::now();
        view.update(vcx, |app, cx| {
            let mut diff = DiffView::new("big".into(), "claude_code_engineer".into(), vec!["a.rs".into()], 10_000, false);
            diff.set_body(body);
            app.diff = Some(diff);
            cx.notify();
        });
        vcx.run_until_parked();
        let first = opened.elapsed();
        assert!(vcx.debug_bounds("diff-overlay").is_some());
        let redraw = Instant::now();
        for _ in 0..20 {
            view.update(vcx, |_, cx| cx.notify());
            vcx.run_until_parked();
        }
        let per_frame = redraw.elapsed() / 20;
        println!("10,000-line diff: opened in {first:?}, {per_frame:?} per redraw");
        assert!(first < Duration::from_millis(1500), "opening took {first:?}");
        assert!(per_frame < Duration::from_millis(40), "a redraw took {per_frame:?}");
    }

    // ------------------------------------------------------------------------- memory

    fn pool(free: f64) -> TelemetryFrame {
        TelemetryFrame { omlx: Some(OmlxState { up: true, budget_gb: 38.0, used_gb: 38.0 - free, free_gb: free, ..Default::default() }), ..Default::default() }
    }

    #[gpui::test]
    async fn a_nearly_full_pool_is_announced_in_needs_you(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, pool(4.0));
        assert!(vcx.debug_bounds("memory-alarm").is_some(), "4 GB free of 38 raised no alarm");
        assert!(vcx.debug_bounds("button-unload-memory-alarm").is_some() && vcx.debug_bounds("button-unload-memory").is_some(), "Unload is on the alarm and in the banner");
    }

    #[gpui::test]
    async fn plenty_of_memory_raises_no_alarm(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, pool(28.0));
        assert!(vcx.debug_bounds("memory-alarm").is_none(), "the alarm is up with 28 GB free");
        assert!(vcx.debug_bounds("button-unload-memory").is_some(), "…but the control is still in the banner");
    }

    #[gpui::test]
    async fn the_unload_key_reaches_the_same_path_as_the_button(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("button-unload-memory").is_some(), "the OOM button is always offered, even with no telemetry");
        vcx.simulate_keystrokes("shift-u");
        vcx.run_until_parked();
        assert!(notices(&view, vcx).iter().any(|n| n.contains("every resident model")), "{:?}", notices(&view, vcx));
        assert!(notices(&view, vcx).iter().any(|n| n.contains("no daemon connection")));
    }

    // -------------------------------------------------------------------- the minimum size

    /// A busy team at the smallest window the app allows: nothing off the right edge, every region
    /// where it should be, and the controls inside the column they belong to.
    #[gpui::test]
    async fn a_busy_window_fits_at_the_minimum_size(cx: &mut TestAppContext) {
        let (view, vcx) = bare(cx);
        feed(&view, vcx, team_frame());
        online(&view, vcx);
        view.update(vcx, |app, cx| {
            app.model.apply(elevated("e1"), Instant::now());
            cx.notify();
        });
        vcx.simulate_resize(size(px(980.), px(640.)));
        vcx.run_until_parked();

        let root = vcx.debug_bounds("lab-root").expect("root");
        assert_eq!(f32::from(root.size.width), 980.0, "the window did not take the minimum size");
        for region in ["lab-header", "lab-banner", "lab-canvas", "lab-side", "lab-composer"] {
            let b = vcx.debug_bounds(region).unwrap_or_else(|| panic!("{region} is missing"));
            assert!(b.origin.x >= root.origin.x && b.right() <= root.right() + px(0.5) && b.bottom() <= root.bottom() + px(0.5), "{region} runs off the window: {b:?}");
        }
        let (canvas, layer, side) = (vcx.debug_bounds("lab-canvas").unwrap(), vcx.debug_bounds("lab-canvas-layer").unwrap(), vcx.debug_bounds("lab-side").unwrap());
        assert!(layer.right() >= canvas.right() - px(0.5), "the canvas surface fills the width it was given");
        for id in ["pod-orchestrator", "pod-architect", "pod-claude_code_engineer"] {
            let b = vcx.debug_bounds(id).unwrap_or_else(|| panic!("{id} is not drawn"));
            assert!(b.right() <= layer.right() + px(0.5), "{id} sticks out of the canvas surface: {b:?}");
            assert!(b.bottom() <= layer.bottom() + px(0.5), "{id} runs off the canvas surface: {b:?}");
        }
        for id in ["lab-drawer", "segment-effort-low", "segment-effort-max", "laya-dial", "button-stop-orchestrator", "tray-e1", "button-tray-approve-e1", "tab-activity", "tab-outputs", "tab-system"] {
            let b = vcx.debug_bounds(id).unwrap_or_else(|| panic!("{id} is not drawn"));
            assert!(b.origin.x >= side.origin.x && b.right() <= side.right() + px(0.5), "{id} runs out of the side column: {b:?} vs {side:?}");
            assert!(f32::from(b.size.width) > 4.0 && f32::from(b.size.height) > 4.0, "{id} has no size: {b:?}");
        }
        // The mode dial and the steer input live in the command deck now, not the header.
        let dock = vcx.debug_bounds("lab-dock").unwrap_or_else(|| panic!("the command deck is not drawn"));
        for id in ["segment-mode-plan", "segment-mode-auto", "lab-composer", "button-estop-all"] {
            let b = vcx.debug_bounds(id).unwrap_or_else(|| panic!("{id} is not drawn"));
            assert!(b.origin.x >= dock.origin.x && b.right() <= dock.right() + px(0.5), "{id} falls out of the command deck: {b:?} vs {dock:?}");
            assert!(b.bottom() <= dock.bottom() + px(0.5), "{id} falls out of the command deck: {b:?} vs {dock:?}");
        }
    }

    #[test]
    fn the_workspace_root_is_an_argument_then_the_environment_then_the_cwd() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert_eq!(workspace_root(args(&["--watch", "/tmp/one"])), PathBuf::from("/tmp/one"));
        assert_eq!(workspace_root(args(&["--watch=/tmp/two"])), PathBuf::from("/tmp/two"));
        // No flag: the environment, which this test sets for itself (nothing else reads it here).
        // SAFETY: single-threaded at this point of the test binary.
        unsafe { std::env::set_var("CORDATA_LAB_WATCH", "/tmp/three") };
        assert_eq!(workspace_root(args(&["--other"])), PathBuf::from("/tmp/three"));
        unsafe { std::env::remove_var("CORDATA_LAB_WATCH") };
        assert_eq!(workspace_root(args(&[])), std::env::current_dir().unwrap());
    }
}
