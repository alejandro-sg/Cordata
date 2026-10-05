pub mod diff;
pub mod footer;
pub mod heal;
mod help;
mod input;
mod lab;
mod laya;
mod markdown;
mod panels;
pub mod progress;
mod team;
mod text;
mod tool_card;
pub mod transcript;

use acp_core::SessionInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::App;

/// What one frame draws: the live region (as a buffer) and where the text
/// cursor goes within it.
pub struct LiveFrame {
    pub buffer: Buffer,
    pub cursor: Option<(u16, u16)>,
}

/// The banner written once when the session connects.
pub fn welcome_lines(info: &SessionInfo, width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::Gray);
    let label = Style::new().fg(Color::DarkGray);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "◆ sierralab-agents",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ACP terminal client", dim),
    ])];
    let session: String = info.session_id.chars().take(8).collect();
    let resumed = if info.resumed { " (resumed)" } else { "" };
    for (key, value) in [
        ("session", format!("{session}{resumed}")),
        ("cwd    ", footer::short_path(&info.cwd)),
        ("link   ", info.transport.clone()),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key}  "), label),
            Span::styled(value, dim),
        ]));
    }
    lines.push(Line::styled(
        text::truncate(
            "  ? for help · /config for settings · Tab changes mode · Esc cancels a turn",
            width as usize,
        ),
        label,
    ));
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
    let available = max_rows.saturating_sub(bottom_height + footer_height) as usize;

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
            "› hi there",
            "╭",
            "shell · execute",
            "$ echo hi",
            "all done",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
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
            .position(|r| r.contains("› read settings.py"))
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
        let prompt = rows.iter().position(|r| r.contains("› hello")).unwrap();
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
            },
            decision: tx,
        });
        let screen = screen(&mut app, 80, 24);
        for needle in ["rm -rf /tmp/x", "Allow", "Reject"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(
            !screen.contains("› "),
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
        std::fs::create_dir_all(store.cwd.join(".sierralab")).unwrap();
        std::fs::write(
            store.path(crate::settings::Scope::Project),
            r#"{"laya":{"mode":"shadow"}}"#,
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
            "laya.mode",
            "shadow",
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
            !rendered.contains("› "),
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
            all.matches("› \u{200b}").count() + all.matches("╭").count(),
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
        let prompt = lines.iter().rposition(|l| l.starts_with('╭')).unwrap();
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
