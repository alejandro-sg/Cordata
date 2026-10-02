mod chat;
mod help;
mod input;
mod laya;
mod markdown;
mod permission_modal;
mod picker;
mod status_bar;
mod team;
mod text;
mod tool_card;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::App;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let input_height = input::height_for(app, area.width);
    let [status, chat_area, input_area, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);

    status_bar::render(frame, status, app);
    chat::render(frame, chat_area, app);
    input::render(frame, input_area, app);
    render_hint(frame, hint, app);

    permission_modal::render(frame, area, app);
    picker::render(frame, area, app);
    help::render(frame, area, app);
}

fn render_hint(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let dim = Style::new().fg(Color::DarkGray);
    let text = match &app.hint {
        Some(hint) => format!(" {hint}"),
        None if app.turn_active => {
            let elapsed = app.turn_started.map_or(0, |started| started.elapsed().as_secs());
            let tools = match app.turn_tools {
                0 => String::new(),
                1 => " · 1 tool call".to_owned(),
                n => format!(" · {n} tool calls"),
            };
            format!(" working {elapsed}s{tools} · Esc cancels")
        }
        None => " Enter send · Tab mode · Ctrl+O model · Ctrl+R reasoning · Ctrl+E tools · ? help · Ctrl+Q quit".to_owned(),
    };
    let line = if app.turn_active {
        let spinner = SPINNER[app.spinner % SPINNER.len()];
        Line::styled(
            format!(" {spinner} {}", text.trim_start()),
            Style::new().fg(Color::Yellow),
        )
    } else {
        Line::styled(text, dim)
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use acp_core::{
        AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, PermissionInfo, SessionInfo,
        ToolCallInfo, ToolKind, ToolStatus,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn connected() -> App {
        let mut app = App::new();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "abcdef12-3456".into(),
            cwd: "/proj".into(),
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
    fn status_bar_shows_session_mode_and_model() {
        let mut app = connected();
        let screen = screen(&mut app, 100, 20);
        let first = screen.lines().next().unwrap();
        assert!(
            first.contains("connected")
                && first.contains("abcdef12")
                && first.contains("BUILD")
                && first.contains("m1"),
            "{first}"
        );
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
        })
    }

    fn progress(id: &str, text: &str) -> AppEvent {
        AppEvent::ToolCallUpdated(acp_core::ToolCallPatch {
            id: id.into(),
            status: Some(ToolStatus::InProgress),
            content: Some(text.into()),
            untrusted: false,
        })
    }

    #[test]
    fn a_running_team_shows_one_row_per_agent_with_live_thinking() {
        let mut app = connected();
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        app.turn_active = true;
        app.on_event(tool_started("t", "local_graph"));
        app.on_event(progress("t", "started: cat_agent\nstarted: dog_agent\nfinished: dog_agent - dogs are loyal\nthinking: cat_agent - one short sentence about cats"));
        let screen = screen(&mut app, 100, 26);
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
        }));
        let screen = screen(&mut app, 100, 26);
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
        app.turn_active = true;
        let long = (1..=60)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(AppEvent::AgentThought(long));
        let live = screen(&mut app, 60, 20);
        assert!(
            live.contains("· thinking…") && live.contains("word60"),
            "live tail shows the latest words:\n{live}"
        );
        assert!(!live.contains("word1 "), "…not the whole reasoning");

        app.on_event(AppEvent::AgentText("answer".into()));
        let done = screen(&mut app, 60, 20);
        assert!(
            done.contains("reasoned (60 words)") && done.contains("Ctrl+R"),
            "{done}"
        );
        assert!(!done.contains("word30"), "collapsed once the answer starts");

        app.thoughts_expanded = true;
        assert!(
            screen(&mut app, 60, 20).contains("word30"),
            "Ctrl+R shows it all"
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
            },
        )));
    }

    #[test]
    fn the_laya_line_sits_directly_under_the_prompt_and_shows_the_outcome() {
        let mut app = connected();
        app.on_event(AppEvent::UserText("read settings.py".into()));
        laya_events(&mut app);
        app.on_event(AppEvent::Laya(acp_core::LayaEvent::Outcome(
            acp_core::LayaOutcome {
                actual_route: "files_and_code".into(),
                agreed: Some(true),
                tools: vec!["file_read".into()],
                tiers: vec![],
                stop_reason: Some("end_turn".into()),
                turn_ms: Some(900.0),
            },
        )));
        app.on_event(AppEvent::AgentText("done".into()));
        let screen = screen(&mut app, 100, 20);
        let rows: Vec<&str> = screen.lines().collect();
        let prompt = rows
            .iter()
            .position(|r| r.contains("› read settings.py"))
            .expect("prompt");
        assert!(
            rows[prompt + 1].contains("◆ laya  files_and_code 98% ▸ hint sent"),
            "no blank line between prompt and laya line:\n{screen}"
        );
        assert!(
            rows[prompt + 2].contains("↳ LLM used files_and_code (file_read) ✓ agreed"),
            "{screen}"
        );
        assert!(screen.contains("done"));
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
    fn help_overlay_lists_keys_and_commands() {
        let mut app = connected();
        app.help_open = true;
        let screen = screen(&mut app, 110, 40);
        for needle in [
            "Help",
            "Esc / Ctrl+C",
            "Tab / Shift+Tab",
            "/mode [plan|build|auto]",
            "/thinking",
            "/laya [on|off]",
            "◆ laya",
            "Any key closes",
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
    fn permission_modal_renders_over_the_chat() {
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
            },
            decision: tx,
        });
        let screen = screen(&mut app, 80, 24);
        for needle in ["Permission required", "rm -rf /tmp/x", "Allow", "Reject"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }
}
