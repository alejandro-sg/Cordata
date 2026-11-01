//! The Blade Runner 2049 interactive workspace (F3 or `/panes`):
//! Dual-pane cockpit where the live conversation stream and prompt box sit on the left,
//! and the live mission control deck (agent pods, status badges, thinking tail, handoffs,
//! plan checklist, orchestrator run board, tool activity, and context metrics) sits on the right.
//!
//! Fully interactive: type prompts, hit Enter, navigate history, approve permissions,
//! cycle modes (Tab), scroll history (PgUp/PgDn/Wheel), or toggle back to inline (F3).

use acp_core::ToolStatus;
use cordata_view::key::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{Kind, Outcome, chrome, dim, panel, put};
use crate::app::{App, Effect, Message};
use crate::extras::{TodoStatus, short_count};
use crate::ui::footer::mode_color;
use crate::ui::text::truncate;
use crate::ui::{hud, input, menu, panels, transcript};

pub struct PanesView {
    /// Scroll offset in conversation lines (0 = bottom, auto-scrolling with new content).
    pub scroll: usize,
    /// Scroll offset in telemetry (right pane).
    pub _right_scroll: usize,
    /// Focused pane: 0 = Conversation / Prompt, 1 = Telemetry.
    pub focus: usize,
    /// Exact text cursor location for terminal hardware cursor.
    cursor_pos: Option<(u16, u16)>,
}

impl PanesView {
    pub fn new() -> Self {
        Self {
            scroll: 0,
            _right_scroll: 0,
            focus: 0,
            cursor_pos: None,
        }
    }

    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor_pos
    }

    pub fn on_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl && key.code == KeyCode::Char('q') {
            return Outcome::close(Some(Effect::Quit));
        }

        // F3 toggles the split-pane workspace off, returning to inline scrollback.
        if key.code == KeyCode::F(3) {
            return Outcome::close(None);
        }

        // Ctrl+K opens the Command Deck palette.
        if ctrl && key.code == KeyCode::Char('k') {
            return Outcome::switch(Kind::Palette);
        }

        // F4 opens Diff Review.
        if key.code == KeyCode::F(4) {
            return Outcome::switch(Kind::Review);
        }

        // PageUp / PageDown scrolls conversation history.
        if key.code == KeyCode::PageUp {
            self.scroll += 8;
            return Outcome::stay();
        }
        if key.code == KeyCode::PageDown {
            self.scroll = self.scroll.saturating_sub(8);
            return Outcome::stay();
        }

        // If an approval, plan card, or question is pending, delegate keys to permission handler.
        if !app.pending.is_empty() {
            let effect = app.on_key_permission(key);
            return Outcome::stay_with(effect);
        }

        // Regular input typing, navigation, tab-completion, and prompt submission.
        let is_enter = key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::SHIFT);
        let effect = app.handle_input_key(key);
        if is_enter {
            self.scroll = 0; // jump to newest messages on send
        }
        Outcome::stay_with(effect)
    }

    pub fn on_mouse(&mut self, _app: &mut App, mouse: MouseEvent) -> Outcome {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll += 3;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(3);
            }
            _ => {}
        }
        Outcome::stay()
    }

    pub fn render(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        let body = chrome(
            buffer,
            area,
            app,
            "WORKSPACE",
            "F3 inline/panes · Ctrl+K deck · F4 diff · ? help · PgUp/PgDn scroll",
        );

        if body.width < 80 || body.height < 10 {
            // Narrow terminal fallback: single full-width conversation pane.
            self.render_conversation_pane(app, buffer, body);
            return;
        }

        // Split: ~62% Left (Conversation & Prompt), ~38% Right (Mission Control / Agents & Telemetry).
        let left_width = (body.width * 62 / 100).max(46).min(body.width.saturating_sub(34));
        let right_width = body.width.saturating_sub(left_width);

        let left_rect = Rect::new(body.x, body.y, left_width, body.height);
        let right_rect = Rect::new(body.x + left_width, body.y, right_width, body.height);

        self.render_conversation_pane(app, buffer, left_rect);
        self.render_telemetry_pane(app, buffer, right_rect);
    }

    fn render_conversation_pane(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        let inner = panel(buffer, area, "CONVERSATION", self.focus == 0);
        if inner.width < 10 || inner.height < 3 {
            return;
        }

        // 1. Calculate bottom region height (approval panel, or prompt box + menu/queue).
        let panel_info = panels::current(app, inner.width);
        let (bottom_height, is_panel) = match &panel_info {
            Some(p) => (panels::height(p, inner.height.saturating_sub(2)), true),
            None => {
                let input_h = input::height_for(app, inner.width);
                let menu_h = if app.menu.is_some() { menu::lines(app, inner.width).len() as u16 } else { 0 };
                let queue_h = input::queue_lines(app, inner.width).len() as u16;
                let total = (input_h + menu_h + queue_h).min(inner.height.saturating_sub(2));
                (total, false)
            }
        };

        let conv_height = inner.height.saturating_sub(bottom_height) as usize;
        let bottom_y = inner.y + inner.height - bottom_height;

        // 2. Render conversation transcript.
        let mut all_lines = transcript::all_conversation_lines(app, inner.width);
        if app.turn_active {
            // Include live background task progress above prompt if present.
            let task_lines = crate::ui::progress::lines(&app.tasks, inner.width, app.spinner);
            if !task_lines.is_empty() {
                all_lines.push(Line::raw(""));
                all_lines.extend(task_lines);
            }
        }

        let total_lines = all_lines.len();
        let max_scroll = total_lines.saturating_sub(conv_height);
        self.scroll = self.scroll.min(max_scroll);

        let start = total_lines.saturating_sub(conv_height + self.scroll);
        let end = (start + conv_height).min(total_lines);

        for (offset, line) in all_lines[start..end].iter().enumerate() {
            buffer.set_line(inner.x, inner.y + offset as u16, line, inner.width);
        }

        // Show scroll indicator if scrolled up.
        if self.scroll > 0 {
            let label = format!("▴ {} rows above", self.scroll);
            buffer.set_string(area.x + area.width.saturating_sub(label.len() as u16 + 2), area.y, label, Style::new().fg(Color::Cyan));
        }

        // 3. Render bottom region (approval panel or input prompt box).
        let bottom_rect = Rect::new(inner.x, bottom_y, inner.width, bottom_height);
        if is_panel {
            if let Some(p) = panel_info {
                panels::render(buffer, bottom_rect, p);
            }
            self.cursor_pos = None;
        } else {
            // Render autocomplete menu if active.
            let menu_lines = if app.menu.is_some() { menu::lines(app, inner.width) } else { vec![] };
            let queue_lines = input::queue_lines(app, inner.width);
            let mut y = bottom_y;
            for line in &queue_lines {
                buffer.set_line(inner.x, y, line, inner.width);
                y += 1;
            }
            for line in &menu_lines {
                buffer.set_line(inner.x, y, line, inner.width);
                y += 1;
            }
            let input_h = input::height_for(app, inner.width);
            let input_rect = Rect::new(inner.x, y, inner.width, input_h);
            self.cursor_pos = input::render(buffer, input_rect, app);
        }
    }

    fn render_telemetry_pane(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        // Divide telemetry vertically into 4 Blade Runner panels:
        // 1. AGENTS & HANDOFFS
        // 2. TASKS & ORCHESTRATION
        // 3. LIVE ACTIVITY
        // 4. CONTEXT & METRICS
        let total_h = area.height;
        let p1_h = 9u16.min(total_h / 3);
        let p2_h = 7u16.min(total_h.saturating_sub(p1_h) / 3);
        let p3_h = 7u16.min(total_h.saturating_sub(p1_h + p2_h) / 2);
        let p4_h = total_h.saturating_sub(p1_h + p2_h + p3_h);

        let mut y = area.y;
        let mut draw_section = |title: &str, h: u16, rows: Vec<Vec<Span<'static>>>| {
            if h < 3 {
                return;
            }
            let rect = Rect::new(area.x, y, area.width, h);
            let inner = panel(buffer, rect, title, false);
            if inner.height > 0 && inner.width > 0 {
                for (idx, spans) in rows.into_iter().take(inner.height as usize).enumerate() {
                    put(buffer, inner.x, inner.y + idx as u16, spans, inner.width);
                }
            }
            y += h;
        };

        let w = (area.width as usize).saturating_sub(4);
        draw_section("AGENTS & HANDOFFS", p1_h, agent_handoff_lines(app, w));
        draw_section("TASKS & ORCHESTRATION", p2_h, task_run_lines(app, w));
        draw_section("LIVE ACTIVITY", p3_h, activity_lines(app, w));
        draw_section("CONTEXT & METRICS", p4_h, telemetry_lines(app, w));
    }
}

// ------------------------------------------------------------------------------------------------ telemetry formatters

fn agent_handoff_lines(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();

    // 1. The model, and whether it is working
    let model_name = app
        .config("model")
        .map(|o| o.current_name().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "model".into());
    let (glyph, status_badge, status_color) = if app.turn_active {
        ("◉", "[EXECUTING ⚡]", Color::Green)
    } else {
        ("○", "[IDLE 💤]", Color::DarkGray)
    };
    rows.push(vec![
        Span::styled(format!("{glyph} "), Style::new().fg(if app.turn_active { Color::Cyan } else { Color::DarkGray })),
        Span::styled(format!("{:<14}", truncate(&model_name, 14)), Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{status_badge} "), Style::new().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled(if app.turn_active { "working" } else { "standby" }, dim()),
    ]);

    // 2. Active Thinking / Reasoning stream
    if app.turn_active
        && let Some(Message::Thought(text)) = app.messages.last()
    {
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let room = width.saturating_sub(14);
        rows.push(vec![
            Span::styled("  · thinking: ", Style::new().fg(Color::Cyan)),
            Span::styled(truncate(&one_line, room), Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC)),
        ]);
    }

    // 3. Live Agent Handoffs
    let mut has_handoff = false;
    if let Some(run) = &app.run
        && let Some(handoff) = &run.handoff
    {
        rows.push(vec![
            Span::styled("⇄ ", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(truncate(handoff, width.saturating_sub(3)), Style::new().fg(Color::Yellow)),
        ]);
        has_handoff = true;
    }
    for msg in app.messages.iter().rev().take(10) {
        if let Message::Tool(card) = msg
            && crate::ui::team::is_team_tool(&card.name)
        {
            let rows_team = crate::ui::team::parse(&card.progress);
            for team_row in rows_team.into_iter().rev().take(1) {
                if let crate::ui::team::TeamRow::Handoff { from, to, message } = team_row {
                    rows.push(vec![
                        Span::styled(format!("⇄ {from} → {to}: "), Style::new().fg(Color::Magenta)),
                        Span::styled(truncate(&message, width.saturating_sub(from.len() + to.len() + 6)), dim()),
                    ]);
                    has_handoff = true;
                }
            }
        }
    }

    if !has_handoff {
        rows.push(vec![
            Span::styled("⇄ ", dim()),
            Span::styled(
                if app.turn_active { "direct dispatch · delegating on sub-tasks" } else { "standby · awaiting task delegation" },
                dim(),
            ),
        ]);
    }

    rows
}

fn task_run_lines(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();

    // 1. Checklist items
    for item in &app.checklist {
        let (glyph, color) = match item.status {
            TodoStatus::Pending => ("☐", Color::Gray),
            TodoStatus::InProgress => ("◐", Color::Cyan),
            TodoStatus::Completed => ("☑", Color::Green),
        };
        rows.push(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color)),
            Span::styled(
                truncate(&item.content, width.saturating_sub(3)),
                Style::new().fg(if item.status == TodoStatus::Completed { Color::DarkGray } else { Color::White }),
            ),
        ]);
    }

    // 2. Orchestrated run
    if let Some(run) = &app.run {
        let (glyph, color) = match run.state.as_str() {
            "done" => ("◉", Color::Green),
            "failed" | "budget_exhausted" => ("✕", Color::Red),
            "paused" => ("◈", Color::Yellow),
            // Folding isolated parallel steps back together (orchestrator/synthesizer.py).
            "synthesize" => ("⧉", Color::Magenta),
            _ => ("◉", Color::Cyan),
        };
        let progress = match run.planned {
            Some(total) => format!(" {}/{}", run.steps.iter().filter(|s| s.status != "running").count(), total),
            None => String::new(),
        };
        rows.push(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color)),
            Span::styled(
                truncate(&format!("run {} · {}{progress}", run.run_id, run.state), width.saturating_sub(2)),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]);
        for step in &run.steps {
            let (mark, shade) = match step.status.as_str() {
                "running" => ("◐", Color::Cyan),
                "success" | "ok" | "done" | "completed" => ("✓", Color::Green),
                "rolled back" => ("↺", Color::Yellow),
                "pending" => ("○", Color::Gray),
                _ => ("✕", Color::Red),
            };
            let mut spans = vec![
                Span::styled(format!("{mark} "), Style::new().fg(shade)),
                Span::styled(format!("{:<5}", truncate(&step.id, 5)), Style::new().fg(Color::White)),
                Span::styled(truncate(&step.worker, 14), Style::new().fg(Color::Magenta)),
                Span::styled(format!(" {}", step.status), dim()),
            ];
            if let Some(cost) = step.cost_usd.filter(|&c| c > 0.0) {
                spans.push(Span::styled(format!(" · ${cost:.4}"), Style::new().fg(Color::Yellow)));
            } else if let Some((tin, tout)) = step.tokens_in.zip(step.tokens_out).filter(|&(i, o)| i > 0 || o > 0) {
                spans.push(Span::styled(format!(" · {}/{} tok", short_count(tin), short_count(tout)), dim()));
            }
            rows.push(spans);
        }
    }

    // 3. Background tasks
    for task in &app.tasks {
        let done = task.info.total.map_or(String::new(), |t| format!(" {:.0}/{:.0}", task.info.done, t));
        rows.push(vec![
            Span::styled("◌ ", Style::new().fg(Color::Magenta)),
            Span::styled(truncate(&format!("{}{done}", task.info.title), width.saturating_sub(3)), dim()),
        ]);
    }

    if rows.is_empty() {
        rows.push(vec![Span::styled("no active plan · agent creates checklist on complex tasks", dim())]);
    }

    rows
}

fn activity_lines(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();

    // Scanner beam while active
    if app.turn_active {
        let mut scanner_row = vec![Span::styled("scan ", dim())];
        scanner_row.extend(hud::scanner(app.spinner, width.saturating_sub(6).min(20)));
        rows.push(scanner_row);
    }

    // Recent tool calls
    let mut count = 0;
    for message in app.messages.iter().rev() {
        let Message::Tool(card) = message else { continue };
        let (glyph, color) = match card.status {
            ToolStatus::Pending => ("◌", Color::Gray),
            ToolStatus::InProgress => ("◐", Color::Cyan),
            ToolStatus::Completed => ("✓", Color::Green),
            ToolStatus::Failed => ("✕", Color::Red),
        };
        let input_summary = card.raw_input.as_ref().map(summarize_input).unwrap_or_default();
        let name = format!("{:<14}", truncate(&card.name, 14));
        let room = width.saturating_sub(3 + 14 + 1);
        rows.push(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color)),
            Span::styled(name, Style::new().fg(Color::White)),
            Span::styled(format!(" {}", truncate(&input_summary, room)), dim()),
        ]);
        count += 1;
        if count >= 5 {
            break;
        }
    }

    if rows.is_empty() {
        rows.push(vec![Span::styled("no tool activity yet", dim())]);
    }

    rows
}

fn summarize_input(value: &serde_json::Value) -> String {
    for key in ["command", "path", "query", "task", "patch", "question"] {
        if let Some(text) = value.get(key).and_then(serde_json::Value::as_str) {
            return text.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    value.to_string()
}

fn telemetry_lines(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = Vec::new();

    // 1. Context meter
    if let Some(gauge) = &app.context {
        let approx = if gauge.estimated { "~" } else { "" };
        let bar = (width.saturating_sub(10)).clamp(6, 24);
        match gauge.pct {
            Some(pct) => {
                let color = if pct >= 0.85 { Color::Red } else if pct >= 0.6 { Color::Yellow } else { Color::Cyan };
                let window = gauge.window.map(|w| format!(" of {}", short_count(w))).unwrap_or_default();
                rows.push(vec![
                    Span::styled(hud::segbar(pct, bar), Style::new().fg(color)),
                    Span::styled(format!(" {approx}{:.0}%", pct * 100.0), Style::new().fg(color).add_modifier(Modifier::BOLD)),
                ]);
                rows.push(vec![
                    Span::styled(format!("{approx}{} tokens{window}", short_count(gauge.used)), dim()),
                ]);
            }
            None => {
                rows.push(vec![Span::styled(format!("{approx}{} tokens", short_count(gauge.used)), dim())]);
            }
        }
    }

    // 2. Token usage & turns
    let (tokens_in, tokens_out) = app.session_tokens;
    rows.push(vec![
        Span::styled("usage ", dim()),
        Span::styled(format!("{} in · {} out", short_count(tokens_in), short_count(tokens_out)), Style::new().fg(Color::White)),
        Span::styled(format!(" · {} turns", app.turns_done), dim()),
    ]);

    // 3. Mode
    let adaptive = app.settings.get("modes.adaptive").and_then(serde_json::Value::as_bool).unwrap_or(true);
    let mut mode_row = vec![Span::styled("mode  ", dim())];
    mode_row.extend(hud::pill(&app.mode.to_uppercase(), mode_color(&app.mode)));
    mode_row.push(Span::styled(if adaptive { "  adaptive" } else { "  manual" }, dim()));
    rows.push(mode_row);

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_core::{AppEvent, ConfigChoice, ConfigOptionInfo, ModeInfo, SessionInfo};

    fn test_app() -> App {
        let mut app = crate::overlay::new_app();
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
                current_value: "kimi-k2.7-code".into(),
                choices: vec![ConfigChoice {
                    value: "kimi-k2.7-code".into(),
                    name: "kimi-k2.7-code".into(),
                }],
            }],
        }));
        app
    }

    #[test]
    fn panes_renders_conversation_and_telemetry() {
        let mut app = test_app();
        app.on_event(AppEvent::UserText("hello agent".into()));
        app.on_event(AppEvent::AgentText("hello human".into()));

        let mut panes = PanesView::new();
        let area = Rect::new(0, 0, 120, 30);
        let mut buffer = Buffer::empty(area);
        panes.render(&app, &mut buffer, area);

        let text = super::super::testing::text(&buffer);
        assert!(text.contains("WORKSPACE"), "missing title in:\n{text}");
        assert!(text.contains("CONVERSATION"), "missing conversation panel in:\n{text}");
        assert!(text.contains("AGENTS & HANDOFFS"), "missing agents panel in:\n{text}");
        assert!(text.contains("TASKS & ORCHESTRATION"), "missing tasks panel in:\n{text}");
        assert!(text.contains("hello agent"), "missing user message in:\n{text}");
        assert!(text.contains("hello human"), "missing agent reply in:\n{text}");
        assert!(text.contains("kimi-k2.7-code"), "missing model in:\n{text}");
    }

    #[test]
    fn f3_and_ctrl_k_behave_as_expected() {
        let mut app = test_app();
        let mut panes = PanesView::new();

        // F3 closes panes view
        let outcome = panes.on_key(&mut app, KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        assert!(outcome.close, "F3 should close panes view");

        // Ctrl+K opens Command Deck
        let outcome = panes.on_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(outcome.open, Some(Kind::Palette), "Ctrl+K should switch to palette");
    }

    #[test]
    fn interactive_typing_and_submit_in_panes() {
        let mut app = test_app();
        let mut panes = PanesView::new();

        for c in "cargo test".chars() {
            panes.on_key(&mut app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.input.text(), "cargo test");

        let outcome = panes.on_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome.effect, Some(Effect::SendPrompt("cargo test".into())));
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn panes_handles_plan_approval() {
        use acp_core::{ApprovalDecision, ChoiceKind, PermissionChoice, PermissionInfo, ToolKind};
        let mut app = test_app();
        let mut panes = PanesView::new();

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "plan1".into(),
                tool_name: "present_plan".into(),
                title: "Run present_plan".into(),
                kind: ToolKind::SwitchMode,
                raw_input: Some(serde_json::json!({ "plan": "1. step one\n2. step two" })),
                untrusted_source: false,
                always_label: None,
                options: vec![
                    PermissionChoice { id: "approve_build".into(), name: "Approve — build".into(), kind: ChoiceKind::AllowOnce },
                ],
            },
            decision: tx,
        });

        // Press 'b' to approve into build
        panes.on_key(&mut app, KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Choose("approve_build".into()));
    }

    #[test]
    fn panes_displays_the_run_handoff_and_checklist() {
        let mut app = test_app();
        app.run = Some(cordata_view::run_board::RunBoard {
            run_id: "run-42".into(),
            state: "executing".into(),
            planned: Some(3),
            steps: vec![cordata_view::run_board::RunStep {
                id: "s1".into(),
                worker: "debugger".into(),
                status: "done".into(),
                cost_usd: Some(0.0150),
                tokens_in: Some(1200),
                tokens_out: Some(400),
                duration_s: Some(2.5),
            }],
            handoff: Some("orchestrator → debugger (step 2 dispatched)".into()),
            ..Default::default()
        });
        app.checklist = vec![crate::extras::TodoItem {
            content: "inspect failing unit test".into(),
            status: TodoStatus::Completed,
        }];

        let mut panes = PanesView::new();
        let area = Rect::new(0, 0, 120, 30);
        let mut buffer = Buffer::empty(area);
        panes.render(&app, &mut buffer, area);

        let text = super::super::testing::text(&buffer);
        assert!(text.contains("IDLE"), "missing the model row in:\n{text}");
        assert!(text.contains("orchestrator → debugger"), "missing handoff in:\n{text}");
        assert!(text.contains("$0.0150"), "missing cost metric in:\n{text}");
        assert!(text.contains("inspect failing unit test"), "missing checklist item in:\n{text}");
    }

    #[test]
    fn scrolling_in_panes_with_keys_and_mouse() {
        let mut app = test_app();
        let mut panes = PanesView::new();

        panes.on_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(panes.scroll, 8);

        panes.on_mouse(&mut app, MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(panes.scroll, 11);

        panes.on_key(&mut app, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(panes.scroll, 3);
    }
}

