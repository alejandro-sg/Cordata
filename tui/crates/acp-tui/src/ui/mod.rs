mod chat;
mod input;
mod permission_modal;
mod picker;
mod status_bar;
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
}

fn render_hint(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let dim = Style::new().fg(Color::DarkGray);
    let text = match &app.hint {
        Some(hint) => format!(" {hint}"),
        None => " Enter send · Ctrl+J newline · Tab mode · Ctrl+O model · Ctrl+T temp · Ctrl+E tools · Ctrl+R thoughts · PgUp/PgDn scroll · Ctrl+Q quit".to_owned(),
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
