pub mod diff;
mod checklist;
pub mod footer;
pub mod heal;
mod help;
pub mod hud;
pub(crate) mod input;
mod lab;
mod laya;
mod markdown;
pub(crate) mod menu;
pub(crate) mod panels;
pub mod progress;
pub(crate) mod team;
pub mod text;
mod tool_card;
pub mod transcript;

use acp_core::SessionInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::app::App;

/// What one frame draws: the live region (as a buffer) and where the text
/// cursor goes within it.
pub struct LiveFrame {
    pub buffer: Buffer,
    pub cursor: Option<(u16, u16)>,
}

/// The banner written once when the session connects: who, where, on what model, and the keys that matter.
///
/// ```text
///  ⌜ CORDATA ⌝  ░▒▓  abcdef12
///  ▌ ~/proj  ⎇ main
///  ▌ kimi-k2.7-code · medium
///  ╴ /  commands   @  files   !  shell   Tab  mode   Ctrl+K  palette   ?  help
/// ```
pub fn welcome_lines(info: &SessionInfo, width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let rail = Style::new().fg(Color::Cyan);
    let session: String = info.session_id.chars().take(8).collect();
    let cwd = footer::short_path(&info.cwd);

    let mut title = vec![Span::raw(" ")];
    // The literal word, not the letter-spaced one the overlays use: scripts and the pty tests wait for it.
    title.extend(hud::bracketed("CORDATA", Color::White));
    title.push(Span::styled("  ░▒▓  ", Style::new().fg(Color::Cyan)));
    title.push(Span::styled(session, Style::new().fg(Color::Cyan)));

    let mut place = vec![Span::styled(" ▌ ", rail), Span::styled(cwd, Style::new().fg(Color::Gray))];
    if let Some(branch) = crate::extras::git_branch(std::path::Path::new(&info.cwd)) {
        place.push(Span::styled(format!("  ⎇ {branch}"), Style::new().fg(Color::Gray)));
    }

    let model = info
        .config_options
        .iter()
        .find(|option| option.id == "model")
        .map(|option| option.current_name().to_owned())
        .filter(|name| !name.is_empty());
    let keys = ["/  commands", "@  files", "!  shell", "Tab  mode", "Ctrl+K  palette", "?  help"].join("   ");

    let mut lines = vec![Line::from(title), Line::from(place)];
    if let Some(model) = model {
        lines.push(Line::from(vec![Span::styled(" ▌ ", rail), Span::styled(model, Style::new().fg(Color::Magenta))]));
    }
    lines.push(Line::from(vec![
        Span::styled(" ╴ ", dim),
        Span::styled(text::truncate(&keys, (width as usize).saturating_sub(4)), dim),
    ]));
    lines
}

/// Draw the live region: reply tail / running tool cards, then the prompt box (or
/// an approval / picker panel in its place), then the footer. It is only as tall
/// as it needs to be, up to `max_rows`.
pub fn render_live(app: &App, width: u16, max_rows: u16) -> LiveFrame {
    if app.lab.open {
        return lab::render_live(app, width, max_rows);
    }
    let width = width.max(20);
    let footer_height = 1u16;
    let panel = panels::current(app, width);
    let bottom_height = match &panel {
        Some(panel) => panels::height(panel, max_rows.saturating_sub(footer_height + 1)),
        None => input::height_for(app, width).min(max_rows.saturating_sub(footer_height).max(3)),
    };
    let tasks = progress::lines(&app.tasks, width, app.spinner);
    // Pinned just above the prompt box, so they are never the rows that get cut when the terminal is short.
    let mut pinned = checklist::lines(app, width);
    pinned.extend(input::queue_lines(app, width));
    if panel.is_none() {
        pinned.extend(menu::lines(app, width));
    }
    let available = max_rows.saturating_sub(bottom_height + footer_height + pinned.len() as u16) as usize;

    let mut content = transcript::live_lines(app, width);
    if !tasks.is_empty() {
        // Background work sits just above the prompt; the transcript tail yields first.
        content.push(Line::raw(""));
        content.extend(tasks);
    }
    if content.len() > available {
        // Only the newest rows fit; the rest is still reachable once it settles into scrollback.
        content.drain(..content.len() - available);
    }
    content.extend(pinned);

    let total = content.len() as u16 + bottom_height + footer_height;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, total));
    for (y, line) in content.iter().enumerate() {
        buffer.set_line(0, y as u16, line, width);
    }
    let bottom = Rect::new(0, content.len() as u16, width, bottom_height);
    let cursor = match panel {
        Some(panel) => {
            panels::render(&mut buffer, bottom, panel);
            None
        }
        None => input::render(&mut buffer, bottom, app),
    };
    buffer.set_line(0, total - 1, &footer::line(app, width), width);
    LiveFrame { buffer, cursor }
}

#[cfg(test)]
mod tests {
    use acp_core::{
        AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, PermissionInfo, SessionInfo,
        ToolCallInfo, ToolKind, ToolStatus,
    };

    use super::*;
    use crate::app::Message;
    use crate::term::testing::{emulator, transcript};
    use crate::term::{InlineTerm, buffer_to_ansi_rows, lines_to_ansi};

    /// A terminal of `w`×`h` that the UI draws into exactly as `main` does.
    /// `show` returns everything the user could see by scrolling: scrollback
    /// followed by the screen.
    struct Screen {
        term: InlineTerm<Vec<u8>>,
        parser: vt100::Parser,
        w: u16,
        h: u16,
    }

    impl Screen {
        fn new(w: u16, h: u16) -> Self {
            Self {
                term: InlineTerm::new(Vec::new()),
                parser: emulator(h, w),
                w,
                h,
            }
        }

        fn show(&mut self, app: &mut App) -> String {
            let commits = transcript::take_commits(app, self.w);
            let live = render_live(app, self.w, self.h);
            self.term
                .draw(
                    &lines_to_ansi(&commits, self.w),
                    &buffer_to_ansi_rows(&live.buffer),
                    live.cursor,
                )
                .unwrap();
            self.parser.process(&self.term.take_output());
            transcript(&mut self.parser).join("\n")
        }
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        Screen::new(w, h).show(app)
    }

    fn connected() -> App {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "abcdef12-3456".into(),
            cwd: "/proj".into(),
            transport: "stdio".into(),
            resumed: false,
            current_mode: "build".into(),
            modes: vec![ModeInfo {
                id: "build".into(),
                name: "Build".into(),
                description: None,
            }],
            config_options: vec![ConfigOptionInfo {
                id: "model".into(),
                name: "Model".into(),
                current_value: "m1".into(),
                choices: vec![ConfigChoice {
                    value: "m1".into(),
                    name: "m1".into(),
                }],
            }],
        }));
        app
    }

    #[test]
    fn the_banner_and_footer_show_session_mode_and_model() {
        let mut app = connected();
        let screen = screen(&mut app, 100, 20);
        for needle in ["abcdef12", "/proj", "BUILD", "m1", "? help"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn chat_shows_user_agent_and_tool_card() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("hi there".into()));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t".into(),
            name: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"command": "echo hi"})),
            content: "hi".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_event(AppEvent::AgentText("all done".into()));
        let screen = screen(&mut app, 80, 24);
        for needle in [
            "❯ hi there",
            "╭",
            "shell · execute",
            "$ echo hi",
            "all done",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    /// `CORDATA_DUMP_DIR=… cargo test dump_inline -- --ignored`: the inline views as ANSI, for looking at.
    #[test]
    #[ignore]
    fn dump_inline() {
        let Some(dir) = std::env::var_os("CORDATA_DUMP_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        let mut app = connected();
        let info = app.session.clone().unwrap();
        let mut rows = lines_to_ansi(&welcome_lines(&info, 110), 110);
        rows.push(String::new());
        app.on_event(AppEvent::UserText("make a plan to add an MCP client under backend/agent".into()));
        app.on_event(AppEvent::AgentText("I'll look at how the project talks to MCP servers first.".into()));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t1".into(),
            name: "code_search".into(),
            kind: ToolKind::Search,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"path": "backend", "query": "mcp"})),
            content: "backend/mcp_servers.json:1".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t2".into(),
            name: "apply_patch".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Failed,
            raw_input: Some(serde_json::json!({"patch": "*** Begin Patch"})),
            content: "DENIED: [preflight] apply_patch was not applied".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        let commits = transcript::take_commits(&mut app, 110);
        rows.extend(lines_to_ansi(&commits, 110));
        app.turn_active = true;
        app.turn_started = Some(std::time::Instant::now());
        app.spinner = 9;
        app.turn_tools = 3;
        rows.extend(buffer_to_ansi_rows(&render_live(&app, 110, 20).buffer));
        rows.push(String::new());
        app.turn_active = false;
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Context(serde_json::json!({"used": 61000, "window": 100000, "pct": 0.61}))));
        app.on_event(AppEvent::ModeChanged("plan".into()));
        let _rx = plan_request(&mut app, "# Goal: add an MCP client\n\n1. **backend/agent/mcp_client.py** — new module wrapping the MCP session\n2. **backend/agent_common.py** — register its tools\n3. **backend/_mcp_client_smoke_test.py** — scripted server, asserts discovery and one call\n\nVerify: `backend/.venv/bin/python backend/_mcp_client_smoke_test.py`\nRisks: the strands MCP client already exists — reuse it instead of a second one");
        rows.extend(buffer_to_ansi_rows(&render_live(&app, 110, 40).buffer));
        std::fs::write(dir.join("inline.ansi"), rows.join("\n")).unwrap();
    }

    fn choice(id: &str, name: &str, kind: acp_core::ChoiceKind) -> acp_core::PermissionChoice {
        acp_core::PermissionChoice { id: id.into(), name: name.into(), kind }
    }

    fn plan_request(app: &mut App, plan: &str) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        use acp_core::ChoiceKind::{AllowOnce, RejectOnce};
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "plan".into(),
                tool_name: "present_plan".into(),
                title: "Run present_plan".into(),
                kind: ToolKind::SwitchMode,
                raw_input: Some(serde_json::json!({ "plan": plan })),
                untrusted_source: false,
                always_label: None,
                options: vec![
                    choice("approve_build", "Approve — build", AllowOnce),
                    choice("approve_edit", "Approve — edit", AllowOnce),
                    choice("approve_auto", "Approve — auto", AllowOnce),
                    choice("keep_planning", "Keep planning", RejectOnce),
                ],
            },
            decision: tx,
        });
        rx
    }

    fn question(app: &mut App) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        use acp_core::ChoiceKind::{AllowOnce, RejectAlways, RejectOnce};
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "q".into(),
                tool_name: "ask_user".into(),
                title: "Run ask_user".into(),
                kind: ToolKind::Other,
                raw_input: Some(serde_json::json!({"question": "Which loader should replace the old one?", "options": ["pydantic-settings", "dynaconf", "keep ours"]})),
                untrusted_source: false,
                always_label: None,
                options: vec![
                    choice("opt1", "pydantic-settings", RejectAlways),
                    choice("opt2", "dynaconf", RejectAlways),
                    choice("opt3", "keep ours", RejectAlways),
                    choice("skip", "Decide for me", AllowOnce),
                    choice("decline", "Skip the question", RejectOnce),
                ],
            },
            decision: tx,
        });
        rx
    }

    #[test]
    fn a_plan_is_shown_as_a_plan_with_its_answers_and_each_key_means_one() {
        use acp_core::ApprovalDecision::{Choose, Reject, Say};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let press = |app: &mut App, code| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));

        let mut app = connected();
        let mut rx = plan_request(&mut app, "# Goal: move settings to pydantic\n1. add models.py\n2. update the loader\nVerify: pytest");
        let shown = screen(&mut app, 110, 30);
        for needle in ["PLAN READY", "nothing has been changed yet", "Goal: move settings to pydantic", "add models.py", " build ", " edit ", " auto ", "keep planning", "say what to change"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        assert!(!shown.contains("Permission required"), "it is not the generic approval card");
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(rx.try_recv().unwrap(), Choose("approve_build".into()), "b approves into build");

        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(rx.try_recv().unwrap(), Choose("approve_edit".into()), "e approves into edit");

        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(rx.try_recv().unwrap(), Choose("approve_auto".into()), "a approves into auto (and says nothing about saving a rule)");
        assert!(!app.messages.iter().any(|m| matches!(m, Message::Info(text) if text.contains("saved for this project"))));

        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(rx.try_recv().unwrap(), Reject, "k keeps planning");
        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Esc);
        assert_eq!(rx.try_recv().unwrap(), Reject, "Esc keeps planning");
        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), Choose("approve_build".into()), "Enter is the plain approval");

        // `w`: keep planning, in the person's own words.
        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Char('w'));
        let typing = screen(&mut app, 110, 30);
        assert!(typing.contains("say") && typing.contains("Enter sends it"), "the note box opens under the plan:\n{typing}");
        for c in "split step 2".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), Say("split step 2".into()), "the words are the answer");
        assert!(app.pending.is_empty() && app.note.is_none());

        // Esc inside the box goes back to the choices without answering; an empty note answers nothing.
        let mut rx = plan_request(&mut app, "plan");
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Esc);
        assert!(app.note.is_none() && app.pending.len() == 1 && rx.try_recv().is_err(), "Esc closes the box and the card stays");
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Enter);
        assert!(app.pending.len() == 1 && rx.try_recv().is_err(), "an empty note answers nothing");
    }

    #[test]
    fn an_older_backend_without_edit_mode_gets_no_edit_key() {
        let mut app = connected();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "plan".into(),
                tool_name: "present_plan".into(),
                title: "Run present_plan".into(),
                kind: ToolKind::SwitchMode,
                raw_input: Some(serde_json::json!({ "plan": "p" })),
                untrusted_source: false,
                always_label: Some("auto".into()),
                options: vec![],
            },
            decision: tx,
        });
        let shown = screen(&mut app, 110, 30);
        assert!(shown.contains(" b  build") && !shown.contains(" e  edit"), "{shown}");
    }

    #[test]
    fn a_question_is_drawn_with_numbered_options_and_a_number_answers_it() {
        use acp_core::ApprovalDecision::{Choose, Reject, Say};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let press = |app: &mut App, code| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));

        let mut app = connected();
        let mut rx = question(&mut app);
        let shown = screen(&mut app, 110, 30);
        for needle in ["QUESTION", "Which loader should replace the old one?", " 1 ", "pydantic-settings", " 2 ", "dynaconf", " 3 ", "keep ours", "type your own", "decide for me"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        assert!(!shown.contains("Decide for me"), "the skip option is not numbered");
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(rx.try_recv().unwrap(), Choose("opt2".into()), "a number picks that option");

        let mut rx = question(&mut app);
        press(&mut app, KeyCode::Char('7'));
        assert!(rx.try_recv().is_err() && app.pending.len() == 1, "a number past the options does nothing");
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(rx.try_recv().unwrap(), Reject, "s leaves it to the agent");

        let mut rx = question(&mut app);
        press(&mut app, KeyCode::Char('w'));
        for c in "whatever is in pyproject".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(rx.try_recv().unwrap(), Say("whatever is in pyproject".into()), "typed words are the answer");
    }

    #[test]
    fn any_approval_can_be_refused_with_a_reason() {
        use acp_core::ApprovalDecision::Say;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = connected();
        let mut rx = ask(&mut app, "t1");
        let shown = screen(&mut app, 110, 30);
        assert!(shown.contains("Reject, saying why"), "{shown}");
        for code in [KeyCode::Char('w'), KeyCode::Char('n'), KeyCode::Char('o'), KeyCode::Enter] {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(rx.try_recv().unwrap(), Say("no".into()));
    }

    #[test]
    fn a_long_plan_is_cut_and_d_shows_all_of_it() {
        let mut app = connected();
        let plan = (1..=60).map(|n| format!("{n}. step number {n}")).collect::<Vec<_>>().join("\n");
        let _rx = plan_request(&mut app, &plan);
        let short = screen(&mut app, 100, 80);
        assert!(short.contains("more rows — d shows the whole plan") && !short.contains("step number 60"), "{short}");
        app.on_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('d'), crossterm::event::KeyModifiers::NONE));
        let whole = screen(&mut app, 100, 120);
        assert!(whole.contains("step number 60"), "{whole}");
    }

    #[test]
    fn the_harness_changing_the_mode_says_why_and_the_pill_follows() {
        let mut app = connected();
        app.on_event(AppEvent::ModeChanged("plan".into()));
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Mode(
            serde_json::json!({"mode": "plan", "from": "build", "kind": "adaptive", "reason": "you asked for a plan"}),
        )));
        let shown = screen(&mut app, 100, 20);
        assert!(shown.contains("build → plan · you asked for a plan"), "{shown}");
        assert!(shown.contains("PLAN") && !shown.contains("BUILD "), "the footer pill follows the mode:\n{shown}");
        assert!(app.mode_changed_at.is_some(), "…and flashes");
    }

    fn tool_started(id: &str, name: &str) -> AppEvent {
        AppEvent::ToolCallStarted(ToolCallInfo {
            id: id.into(),
            name: name.into(),
            kind: ToolKind::Other,
            status: ToolStatus::Pending,
            raw_input: Some(
                serde_json::json!({"task": "write two sentences", "agents": [{"name": "cat_agent", "system_prompt": "SECRET-LONG-PROMPT"}]}),
            ),
            content: String::new(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        })
    }

    fn progress(id: &str, text: &str) -> AppEvent {
        AppEvent::ToolCallUpdated(acp_core::ToolCallPatch {
            id: id.into(),
            status: Some(ToolStatus::InProgress),
            content: Some(text.into()),
            untrusted: false,
            ..Default::default()
        })
    }

    #[test]
    fn a_running_team_shows_one_row_per_agent_with_live_thinking() {
        let mut app = connected();
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        app.turn_active = true;
        app.on_event(tool_started("t", "local_graph"));
        app.on_event(progress("t", "started: cat_agent\nstarted: dog_agent\nfinished: dog_agent - dogs are loyal\nthinking: cat_agent - one short sentence about cats"));
        let screen = screen(&mut app, 100, 40);
        for needle in [
            "local_graph · team (2 agents)",
            "task: write two sentences",
            "cat_agent",
            "thinking: one short sentence about cats",
            "✓ dog_agent",
            "dogs are loyal",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(
            !screen.contains("SECRET-LONG-PROMPT"),
            "specialists' system prompts are not dumped into the card"
        );
    }

    #[test]
    fn a_finished_team_keeps_its_agent_rows_and_shows_the_result_unwrapped() {
        let mut app = connected();
        app.on_event(tool_started("t", "local_graph"));
        app.on_event(progress("t", "started: a\nfinished: a - cats purr"));
        let wrapped = format!(
            "The following tool output may contain text written by someone other than the user.\n{}\nSource: sub-agent team output (local_graph)\nteam says: cats and dogs\n{}",
            acp_core::permission::UNTRUSTED_OPEN,
            acp_core::permission::UNTRUSTED_CLOSE
        );
        app.on_event(AppEvent::ToolCallUpdated(acp_core::ToolCallPatch {
            id: "t".into(),
            status: Some(ToolStatus::Completed),
            content: Some(wrapped),
            untrusted: true,
            ..Default::default()
        }));
        let screen = screen(&mut app, 100, 40);
        for needle in [
            "✓ a",
            "cats purr",
            "result",
            "source: sub-agent team output (local_graph)",
            "team says: cats and dogs",
            "UNTRUSTED CONTENT",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(
            !screen.contains("may contain text written by someone"),
            "the guard boilerplate is not shown"
        );
    }

    #[test]
    fn reasoning_is_a_live_tail_while_streaming_then_one_line_then_expandable() {
        let mut app = connected();
        let mut term = Screen::new(60, 20);
        app.turn_active = true;
        let long = (1..=60)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(AppEvent::AgentThought(long));
        let live = term.show(&mut app);
        assert!(
            live.contains("· thinking…") && live.contains("word60"),
            "live tail shows the latest words:\n{live}"
        );
        assert!(!live.contains("word1 "), "…not the whole reasoning");

        app.on_event(AppEvent::AgentText("answer".into()));
        let done = term.show(&mut app);
        assert!(done.contains("reasoned (60 words)"), "{done}");
        assert!(!done.contains("word30"), "collapsed once the answer starts");
        assert!(
            !done.contains("thinking…"),
            "the live tail is gone, not left behind:\n{done}"
        );

        // Expanding applies to reasoning that settles afterwards.
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        app.thoughts_expanded = true;
        app.on_event(AppEvent::UserText("again".into()));
        app.turn_active = true;
        let second = (1..=40)
            .map(|i| format!("alpha{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(AppEvent::AgentThought(second));
        app.on_event(AppEvent::AgentText("second answer".into()));
        assert!(
            term.show(&mut app).contains("alpha20"),
            "Ctrl+R shows new reasoning in full"
        );
    }

    #[test]
    fn replies_render_markdown_not_raw_markers() {
        let mut app = connected();
        app.on_event(AppEvent::AgentText(
            "# Result\n- run `cargo test`\n- **done**".into(),
        ));
        let screen = screen(&mut app, 60, 20);
        assert!(
            screen.contains("Result")
                && screen.contains("• run cargo test")
                && screen.contains("• done"),
            "{screen}"
        );
        assert!(
            !screen.contains("**") && !screen.contains('`') && !screen.contains("# Result"),
            "{screen}"
        );
    }

    #[test]
    fn the_slash_menu_and_the_queue_sit_above_the_prompt() {
        let mut app = connected();
        app.input.set_text("/mo");
        app.refresh_menu();
        let shown = screen(&mut app, 90, 24);
        for needle in ["▸ /mode", "/model", "↑↓ choose · tab complete · enter run", "❯ /mo"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        let rows: Vec<&str> = shown.lines().collect();
        let menu = rows.iter().position(|r| r.contains("▸ /mode")).unwrap();
        let prompt = rows.iter().position(|r| r.contains("❯ /mo")).unwrap();
        assert!(menu < prompt, "the menu is above the box:\n{shown}");
        eprintln!("{shown}");

        let mut busy = connected();
        busy.turn_active = true;
        busy.queue.push_back("then run the tests".into());
        busy.queue.push_back("and commit".into());
        let shown = screen(&mut busy, 90, 24);
        assert!(shown.contains("⏵ next  then run the tests") && shown.contains("working — enter queues"), "{shown}");
        eprintln!("{shown}");
    }

    #[test]
    fn the_footer_carries_the_context_gauge_the_goal_and_the_branch() {
        let mut app = connected();
        app.branch = Some("feat/x".into());
        app.on_context(&serde_json::json!({"used": 45_000, "window": 128_000, "pct": 0.35}));
        app.on_goal(&serde_json::json!({"event": "continue", "condition": "tests pass", "how": "check", "turn": 2, "max": 8, "reason": "failing"}));
        let wide = screen(&mut app, 120, 24);
        let footer = wide.lines().last().unwrap().to_owned();
        for needle in ["BUILD", "◎ goal 2/8", "ctx ▰▰▱▱▱▱ 35%", "m1", "/proj ⎇ feat/x", "? help"] {
            assert!(footer.contains(needle), "missing {needle:?} in footer {footer:?}");
        }
        // On a narrow terminal the least important parts go first and the goal is the last to go.
        let narrow = screen(&mut app, 46, 24);
        let footer = narrow.lines().last().unwrap().to_owned();
        assert!(footer.contains("BUILD") && footer.contains("◎ goal 2/8"), "{footer:?}");
        assert!(!footer.contains("feat/x"), "{footer:?}");
        assert!(footer.chars().count() <= 46, "{footer:?}");

        // Hot context turns the gauge red-ish and says what to do, once.
        let mut hot = connected();
        hot.on_context(&serde_json::json!({"used": 120_000, "window": 128_000, "pct": 0.94}));
        let shown = screen(&mut hot, 100, 24);
        assert!(shown.contains("94%") && shown.contains("/compact"), "{shown}");
    }

    #[test]
    fn the_agents_checklist_sits_above_the_prompt_and_its_tool_card_stays_out_of_the_transcript() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("do the thing".into()));
        app.turn_active = true;
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t".into(),
            name: "todo_write".into(),
            kind: ToolKind::Other,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"todos": []})),
            content: "Checklist saved (1/3 done).".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_checklist(&serde_json::json!({"items": [
            {"content": "read the failing test", "status": "completed"},
            {"content": "fix the off-by-one", "status": "in_progress"},
            {"content": "run the suite", "status": "pending"},
        ]}));
        let shown = screen(&mut app, 80, 24);
        for needle in ["▣ plan", "1/3", "✓ read the failing test", "fix the off-by-one", "○ run the suite"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        assert!(!shown.contains("Checklist saved") && !shown.contains("todo_write"), "the card is not shown:\n{shown}");
        let rows: Vec<&str> = shown.lines().collect();
        let plan = rows.iter().position(|r| r.contains("▣ plan")).unwrap();
        let prompt = rows.iter().rposition(|r| r.contains('❯')).unwrap();
        assert!(plan < prompt, "the plan is above the prompt:\n{shown}");

        // A long list keeps the step in progress visible by folding finished steps away.
        let items: Vec<_> = (1..=12)
            .map(|i| serde_json::json!({"content": format!("step {i}"), "status": if i <= 8 { "completed" } else if i == 9 { "in_progress" } else { "pending" }}))
            .collect();
        app.on_checklist(&serde_json::json!({ "items": items }));
        let shown = screen(&mut app, 80, 30);
        assert!(shown.contains("✓ ") && shown.contains("done") && shown.contains("step 9") && shown.contains("step 12"), "{shown}");
        assert!(!shown.contains("step 1\n") && !shown.contains(" step 2"), "early finished steps are folded:\n{shown}");
    }

    #[test]
    fn a_suggested_next_prompt_shows_as_grey_text_and_tab_takes_it() {
        let mut app = connected();
        app.suggestion = Some("run the tests and show me the result".into());
        let shown = screen(&mut app, 90, 20);
        assert!(shown.contains("❯ run the tests and show me the result") && shown.contains("tab ⇥"), "{shown}");
        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Tab));
        assert_eq!(app.input.text(), "run the tests and show me the result");
        assert!(app.suggestion.is_none());
        // Typing instead throws it away.
        let mut other = connected();
        other.suggestion = Some("commit".into());
        other.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('x')));
        assert!(other.suggestion.is_none() && other.input.text() == "x");
    }

    #[test]
    fn a_diff_is_coloured_by_line_kind_and_btw_compact_goal_go_where_they_should() {
        let mut app = connected();
        app.on_local(crate::app::LocalEvent::ShellDone {
            command: "git diff".into(),
            outcome: crate::shell::Outcome { output: "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n".into(), code: Some(0), timed_out: false, cut: false },
            share: false,
            as_diff: true,
        });
        let shown = screen(&mut app, 80, 20);
        assert!(shown.contains("-old") && shown.contains("+new") && !shown.contains("$ git diff"), "{shown}");
        assert!(app.shell_context.is_empty(), "/diff is for the person, not sent to the agent");

        let mut app = connected();
        let type_line = |app: &mut App, line: &str| {
            app.input.set_text(line);
            app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter))
        };
        let Some(crate::app::Effect::Ext { method, params, .. }) = type_line(&mut app, "/btw what did we decide?") else { panic!("btw is an extension request") };
        assert_eq!((method.as_str(), params["question"].as_str()), ("_cordata/btw", Some("what did we decide?")));
        let Some(crate::app::Effect::Ext { method, .. }) = type_line(&mut app, "/goal clear") else { panic!() };
        assert_eq!(method, "_cordata/goal/clear");
        let Some(crate::app::Effect::Ext { method, .. }) = type_line(&mut app, "/context") else { panic!() };
        assert_eq!(method, "_cordata/context/usage");
        let Some(crate::app::Effect::Ext { method, params, .. }) = type_line(&mut app, "/compact keep the plan") else { panic!() };
        assert_eq!((method.as_str(), params["focus"].as_str()), ("_cordata/context/compact", Some("keep the plan")));
        // A goal is a prompt, so it works in every front door; the backend turns it into the loop.
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        let sent = type_line(&mut app, "/goal tests pass --check make test");
        assert_eq!(sent, Some(crate::app::Effect::SendPrompt("/goal tests pass --check make test".into())));
        // /compact refuses mid-turn, saying why, instead of failing in the backend.
        let busy = type_line(&mut app, "/compact");
        assert!(busy.is_none());
        assert!(app.messages.iter().any(|m| matches!(m, Message::Info(t) if t.contains("the agent is working"))));
    }

    fn laya_events(app: &mut App) {
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::Decision(
            acp_core::LayaDecision {
                mode: "route".into(),
                route: Some("files_and_code".into()),
                route_confidence: 0.98,
                gate: 0.85,
                needs_team: 0.1,
                complexity: 1.3,
                complexity_confidence: 0.7,
                latency_ms: Some(64.0),
                waited_ms: Some(64.0),
                hint: Some("use the file tools".into()),
                tier: None,
                late: false,
                error: None,
                ..Default::default()
            },
        )));
    }

    fn laya_outcome() -> AppEvent {
        AppEvent::Laya(acp_core::LayaEvent::Outcome(acp_core::LayaOutcome {
            actual_route: "files_and_code".into(),
            agreed: Some(true),
            tools: vec!["file_read".into()],
            tiers: vec![],
            stop_reason: Some("end_turn".into()),
            turn_ms: Some(900.0),
            ..Default::default()
        }))
    }

    #[test]
    fn the_laya_line_sits_directly_under_the_prompt_and_the_outcome_follows_the_reply() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("read settings.py".into()));
        laya_events(&mut app);
        app.on_event(AppEvent::AgentText("done".into()));
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        app.on_event(laya_outcome());
        let screen = screen(&mut app, 100, 30);
        let rows: Vec<&str> = screen.lines().collect();
        let prompt = rows
            .iter()
            .position(|r| r.contains("❯ read settings.py"))
            .expect("prompt");
        assert!(
            rows[prompt + 1].contains("◆ laya  files_and_code 98% ▸ hint sent"),
            "no blank line between prompt and laya line:\n{screen}"
        );
        let outcome = rows
            .iter()
            .position(|r| r.contains("↳ LLM used files_and_code (file_read) ✓ agreed"))
            .unwrap_or_else(|| panic!("outcome line missing:\n{screen}"));
        let reply = rows.iter().position(|r| r.contains("done")).unwrap();
        assert!(outcome > reply, "the outcome closes the turn:\n{screen}");
    }

    #[test]
    fn a_late_laya_decision_is_added_not_swapped_into_scrollback() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("hello".into()));
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::Decision(
            acp_core::LayaDecision {
                mode: "route".into(),
                route: None,
                route_confidence: 0.0,
                gate: 0.85,
                needs_team: 0.0,
                complexity: 0.0,
                complexity_confidence: 0.0,
                latency_ms: None,
                waited_ms: Some(250.0),
                hint: None,
                tier: None,
                late: false,
                error: Some("timed out".into()),
                ..Default::default()
            },
        )));
        let mut term = Screen::new(100, 30);
        let first = term.show(&mut app);
        assert!(first.contains("timed out"), "{first}");
        laya_events(&mut app);
        let second = term.show(&mut app);
        assert!(
            second.contains("timed out") && second.contains("files_and_code 98%"),
            "the earlier line stays; the late decision follows it:\n{second}"
        );
    }

    #[test]
    fn slash_laya_off_hides_the_line_without_leaving_a_gap() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("hello".into()));
        laya_events(&mut app);
        app.on_event(AppEvent::AgentText("hi".into()));
        app.show_laya = false;
        let screen = screen(&mut app, 100, 20);
        assert!(!screen.contains("◆ laya"), "{screen}");
        let rows: Vec<&str> = screen.lines().collect();
        let prompt = rows.iter().position(|r| r.contains("❯ hello")).unwrap();
        assert!(
            rows[prompt + 1].trim().is_empty() && rows[prompt + 2].contains("hi"),
            "one blank line, as without laya:\n{screen}"
        );
    }

    #[test]
    fn help_is_a_transcript_block_listing_keys_and_commands() {
        let mut app = connected();
        app.messages.push(Message::Help);
        let screen = screen(&mut app, 110, 60);
        for needle in [
            "Keys",
            "Esc / Ctrl+C",
            "Tab / Shift+Tab",
            "/mode [plan|build|auto]",
            "/thinking",
            "/laya [on|off|stats]",
            "Scroll with your terminal",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn the_hint_line_shows_elapsed_time_and_tool_calls_while_working() {
        let mut app = connected();
        app.turn_active = true;
        app.turn_started = Some(std::time::Instant::now());
        app.turn_tools = 2;
        let screen = screen(&mut app, 110, 20);
        assert!(
            screen.contains("working 0s · 2 tool calls · Esc cancels"),
            "{screen}"
        );
    }

    #[test]
    fn the_approval_box_replaces_the_prompt_box() {
        let mut app = connected();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: Some(serde_json::json!({"command": "rm -rf /tmp/x"})),
                untrusted_source: false,
                always_label: None,
                options: vec![],
            },
            decision: tx,
        });
        let screen = screen(&mut app, 80, 24);
        for needle in ["rm -rf /tmp/x", "Allow", "Reject"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(
            !screen.contains("❯ "),
            "no prompt box while asking:\n{screen}"
        );
    }

    #[test]
    fn the_approval_box_offers_always_allow_only_when_the_server_does() {
        let ask = |label: Option<&str>| {
            let mut app = connected();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            app.on_event(AppEvent::PermissionRequested {
                info: PermissionInfo {
                    tool_call_id: "t".into(),
                    tool_name: "shell".into(),
                    title: "Run shell".into(),
                    kind: ToolKind::Execute,
                    raw_input: Some(serde_json::json!({"command": "git status"})),
                    untrusted_source: false,
                    always_label: label.map(Into::into),
                    options: vec![],
                },
                decision: tx,
            });
            screen(&mut app, 80, 24)
        };
        let with = ask(Some("Always allow shell(git status)"));
        assert!(
            with.contains("Always allow shell(git status)") && with.contains(" s "),
            "{with}"
        );
        assert!(!ask(None).contains("Always allow"));
    }

    #[test]
    fn the_config_panel_shows_each_setting_its_value_and_the_file_that_set_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let store = crate::settings::Store {
            user_dir: root.join("home"),
            cwd: root.join("project"),
        };
        std::fs::create_dir_all(&store.user_dir).unwrap();
        std::fs::write(
            store.path(crate::settings::Scope::User),
            r#"{"tui":{"thinking":"expanded"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(store.cwd.join(".cordata")).unwrap();
        std::fs::write(
            store.path(crate::settings::Scope::Project),
            r#"{"laya":{"dispatch":"hint"}}"#,
        )
        .unwrap();
        let mut app = connected();
        app.attach_store(store.clone());
        app.input.set_text("/config");
        app.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        let rendered = screen(&mut app, 100, 30);
        for needle in [
            "Settings → user file",
            "permissions.defaultMode",
            "tui.thinking",
            "expanded",
            "laya.dispatch",
            "hint",
            "project",
            "m1",
            "session",
            "default",
            "next session",
            "Enter change",
        ] {
            assert!(
                rendered.contains(needle),
                "missing {needle:?} in\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("❯ "),
            "the panel replaces the prompt box:\n{rendered}"
        );

        // The schema is taller than this terminal, so the list scrolls. The key
        // hints and the selected entry's help must not scroll with it: as list
        // items they were only visible when the selection happened to be near
        // the bottom, which is every row except the ones a user actually lands
        // on first.
        for top in [true, false] {
            let mut app = connected();
            app.attach_store(store.clone());
            app.input.set_text("/config");
            app.on_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Enter,
            ));
            if !top {
                for _ in 0..crate::settings::SCHEMA.len() {
                    app.on_key(crossterm::event::KeyEvent::from(
                        crossterm::event::KeyCode::Down,
                    ));
                }
            }
            let rendered = screen(&mut app, 100, 30);
            let where_ = if top { "top" } else { "bottom" };
            assert!(
                rendered.contains("Enter change"),
                "the hints stay visible at the {where_} of the list:\n{rendered}"
            );
            assert!(
                rendered.contains("next session"),
                "the help line follows the selection at the {where_} of the list:\n{rendered}"
            );
        }

        // And the row the selection moved to is on screen, not scrolled past.
        let mut app = connected();
        app.attach_store(store.clone());
        app.input.set_text("/config");
        app.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        for _ in 0..crate::settings::SCHEMA.len() {
            app.on_key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Down,
            ));
        }
        let rendered = screen(&mut app, 100, 30);
        let last = crate::settings::SCHEMA.last().unwrap();
        assert!(
            rendered.contains(last.key),
            "the last setting {:?} is reachable:\n{rendered}",
            last.key
        );
    }

    #[test]
    fn a_long_reply_is_ordinary_scrollback_with_one_prompt_box_left_on_screen() {
        let mut app = connected();
        let mut term = Screen::new(60, 12);
        app.on_event(AppEvent::UserText("write a lot".into()));
        app.turn_active = true;
        let reply: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        // Stream it in a few chunks, as the model would.
        for chunk in reply.as_bytes().chunks(97) {
            app.on_event(AppEvent::AgentText(String::from_utf8_lossy(chunk).into()));
            term.show(&mut app);
        }
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        let all = term.show(&mut app);
        for i in 1..=100 {
            assert!(
                all.lines()
                    .any(|row| row.trim_end().ends_with(&format!("line {i}"))),
                "line {i} reachable by scrolling up"
            );
        }
        assert_eq!(
            all.matches("Ask anything").count(),
            1,
            "one prompt box:\n{all}"
        );
        // …and the reply is not duplicated by the live region.
        assert_eq!(all.matches("line 50").count(), 1, "no duplicated rows");
    }

    #[test]
    fn clear_resets_the_commit_cursor() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("one".into()));
        let mut term = Screen::new(60, 20);
        term.show(&mut app);
        assert!(app.committed > 0);
        app.input.set_text("/clear");
        let effect = app.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ));
        assert_eq!(effect, Some(crate::app::Effect::ClearScreen));
        assert_eq!((app.committed, app.committed_rows), (0, 0));
    }

    fn edit_card(app: &mut App, id: &str) {
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: id.into(),
            name: "editor".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Pending,
            raw_input: None,
            content: String::new(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 20\n", "LINE TWENTY\n");
        app.on_event(AppEvent::ToolCallUpdated(acp_core::ToolCallPatch {
            id: id.into(),
            raw_input: Some(serde_json::json!({"command": "str_replace", "path": "src/a.rs"})),
            diffs: Some(vec![acp_core::FileDiff {
                path: "/proj/src/a.rs".into(),
                old_text: Some(old),
                new_text: new,
            }]),
            ..Default::default()
        }));
    }

    fn ask(app: &mut App, id: &str) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: id.into(),
                tool_name: "editor".into(),
                title: "Run editor".into(),
                kind: ToolKind::Edit,
                raw_input: Some(serde_json::json!({"command": "str_replace", "path": "src/a.rs", "old_str": "line 20", "new_str": "LINE TWENTY"})),
                untrusted_source: false,
                always_label: None,
                options: vec![],
            },
            decision: tx,
        });
        rx
    }

    #[test]
    fn the_approval_panel_shows_the_diff_before_the_edit_runs() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("rename it".into()));
        edit_card(&mut app, "e1");
        let _rx = ask(&mut app, "e1");
        let shown = screen(&mut app, 90, 30);
        for needle in [
            "Permission required",
            "✎ src/a.rs",
            "+1 −1",
            "  20 − line 20",
            "  20 + LINE TWENTY",
            " a ",
            " r ",
            " d ",
        ] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        assert!(
            !shown.contains("\"old_str\""),
            "the raw JSON is replaced by the diff:\n{shown}"
        );
    }

    #[test]
    fn on_a_short_terminal_the_diff_is_cut_but_the_keys_stay() {
        let mut app = connected();
        edit_card(&mut app, "e1");
        // A long multi-hunk change in a 14-row terminal.
        if let Some(Message::Tool(card)) = app.messages.last_mut() {
            let old: String = (1..=80).map(|i| format!("l{i}\n")).collect();
            let new: String = (1..=80)
                .map(|i| {
                    if i % 9 == 0 {
                        format!("CHANGED {i}\n")
                    } else {
                        format!("l{i}\n")
                    }
                })
                .collect();
            card.diffs[0] = acp_core::FileDiff {
                path: "/proj/b.txt".into(),
                old_text: Some(old),
                new_text: new,
            };
        }
        let _rx = ask(&mut app, "e1");
        let shown = screen(&mut app, 80, 14);
        assert!(
            shown.contains(" a ") && shown.contains("Allow") && shown.contains("Reject"),
            "keys visible:\n{shown}"
        );
        assert!(shown.contains("more rows"), "the cut is marked:\n{shown}");
        app.on_key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char('d'),
        ));
        assert!(app.diff_expanded, "d toggles the whole diff");
    }

    #[test]
    fn background_tasks_render_as_progress_bars_above_the_prompt() {
        let mut app = connected();
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Task(
            acp_core::TaskInfo {
                id: "t1".into(),
                title: "rust: indexing".into(),
                done: 30.0,
                total: Some(120.0),
                message: "crates/acp-core".into(),
                state: acp_core::TaskState::Running,
                source: "lsp".into(),
            },
        )));
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Task(
            acp_core::TaskInfo {
                id: "t2".into(),
                title: "self-heal: verifying in sandbox (1/3)".into(),
                done: 0.0,
                total: None,
                message: String::new(),
                state: acp_core::TaskState::Running,
                source: "heal".into(),
            },
        )));
        let shown = screen(&mut app, 100, 20);
        assert!(
            shown.contains("rust: indexing") && shown.contains(" 25%") && shown.contains("█████"),
            "{shown}"
        );
        assert!(
            shown.contains("self-heal: verifying in sandbox (1/3)"),
            "{shown}"
        );
        let lines: Vec<&str> = shown.lines().collect();
        let bar = lines
            .iter()
            .position(|l| l.contains("rust: indexing"))
            .unwrap();
        let prompt = lines.iter().rposition(|l| l.contains('❯')).unwrap();
        assert!(bar < prompt, "bars sit above the prompt box");
        // finishing it leaves a ✓ that lingers, never in scrollback
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Task(
            acp_core::TaskInfo {
                id: "t1".into(),
                title: "rust: indexing".into(),
                done: 120.0,
                total: Some(120.0),
                message: String::new(),
                state: acp_core::TaskState::Completed,
                source: "lsp".into(),
            },
        )));
        assert!(screen(&mut app, 100, 20).contains("✓ rust: indexing"));
        assert!(
            app.messages
                .iter()
                .all(|m| !matches!(m, Message::Info(t) if t.contains("indexing")))
        );
    }

    #[test]
    fn self_heal_phases_land_in_the_transcript_with_the_patch_coloured() {
        let mut app = connected();
        let heal = |phase: &str| {
            AppEvent::Harness(acp_core::HarnessEvent::Heal(acp_core::HealInfo {
                phase: phase.into(),
                attempt: Some(1),
                max: Some(3),
                command: Some("cargo test".into()),
                root_cause: Some("add() subtracts".into()),
                patch: Some(
                    "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a - b\n+a + b\n".into(),
                ),
                verify: Some("cargo test".into()),
                ..Default::default()
            }))
        };
        for phase in [
            "detected",
            "diagnosing",
            "proposed",
            "verifying",
            "verified",
            "healed",
        ] {
            app.on_event(heal(phase));
        }
        let shown = screen(&mut app, 100, 30);
        for needle in [
            "⟲ self-heal · `cargo test` failed",
            "attempt 1/3 · root cause: add() subtracts",
            "+a + b",
            "verified in a sandbox copy",
            "✓ self-heal · `cargo test` passes now",
        ] {
            assert!(shown.contains(needle), "missing {needle:?}:\n{shown}");
        }
        let blocks = app
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Heal(_)))
            .count();
        assert_eq!(
            blocks, 4,
            "diagnosing/verifying are progress bars, not blocks"
        );
    }

    #[test]
    fn the_footer_shows_a_tab_strip_with_sessions_needing_approval() {
        let mut app = connected();
        app.tabs = vec![
            crate::app::TabBadge {
                number: 1,
                active: true,
                state: crate::app::TabState::Idle,
            },
            crate::app::TabBadge {
                number: 2,
                active: false,
                state: crate::app::TabState::NeedsApproval,
            },
            crate::app::TabBadge {
                number: 3,
                active: false,
                state: crate::app::TabState::Working,
            },
        ];
        let shown = screen(&mut app, 100, 12);
        let footer = shown.lines().last().unwrap();
        assert!(
            footer.contains("[1]") && footer.contains(" 2⚠ ") && footer.contains(" 3● "),
            "{footer}"
        );
    }

    #[test]
    fn laya_v2_decisions_and_command_predictions_render() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("fix the test".into()));
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::Decision(
            acp_core::LayaDecision {
                mode: "route".into(),
                route: Some("debugger".into()),
                route_confidence: 0.97,
                calibrated: Some(0.97),
                action: Some("dispatch".into()),
                latency_ms: Some(29.0),
                ..Default::default()
            },
        )));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "s1".into(),
            name: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Failed,
            raw_input: Some(serde_json::json!({"command": "cargo test"})),
            content: "Exit Code: 101".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::Command {
            tool_use_id: "s1".into(),
            p_ok: 0.88,
        }));
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::CommandOutcome {
            tool_use_id: "s1".into(),
            ok: false,
            p_ok: Some(0.88),
            log_loss: Some(2.1),
        }));
        let shown = screen(&mut app, 100, 24);
        assert!(
            shown.contains("◆ laya  debugger 97% ▸ dispatched to the debugger specialist · 29 ms"),
            "{shown}"
        );
        assert!(shown.contains("laya p(ok) 88% ✗ surprised"), "{shown}");
    }

    #[test]
    fn the_lab_view_survives_the_real_terminal_pipeline() {
        use acp_core::{HarnessEvent, LabHitl, LabLink, LabPod, LabState, LabThreat, PodStatus};

        let mut app = connected();
        let pod = |name: &str, status, model: &str, effort: &str| LabPod {
            id: name.into(),
            name: name.into(),
            role: name.into(),
            status,
            model: Some(model.into()),
            effort: Some(effort.into()),
            dial: 0.8,
            dial_default: 0.8,
            calls: 2,
            ..LabPod::default()
        };
        app.on_event(AppEvent::Harness(HarnessEvent::Lab(Box::new(LabState {
            seq: 4,
            session_id: Some("abcdef12-3456".into()),
            pods: vec![
                pod("orchestrator", PodStatus::Executing, "glm-5.3-flash", "low"),
                pod("architect", PodStatus::Idle, "glm-5.3-flash", "high"),
                pod("debugger", PodStatus::Isolated, "kimi-k2.7-code", "max"),
            ],
            links: vec![LabLink {
                from: "orchestrator".into(),
                to: "debugger".into(),
                kind: "delegation".into(),
                active: false,
                count: 1,
                label: String::new(),
            }],
            threat: Some(LabThreat {
                pod: "orchestrator".into(),
                tool: "shell".into(),
                score: Some(0.5),
                confidence: Some(0.4),
                band: Some("elevated".into()),
                pending: false,
            }),
            hitl: vec![LabHitl::default()],
            ..LabState::default()
        }))));
        app.lab.open = true;
        let shown = screen(&mut app, 100, 40);
        println!("{shown}");
        for needle in [
            "EXPERIMENTAL LAB",
            "[EXECUTING ⚡]",
            "[IDLE 💤]",
            "[ISOLATED 🛑]",
            "THREAT",
            "0.50 ELEVATED",
            "E-stop",
        ] {
            assert!(
                shown.contains(needle),
                "missing {needle:?} on the emulated screen:\n{shown}"
            );
        }
    }
}
