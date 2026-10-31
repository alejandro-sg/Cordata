//! The session HUD (F3 or `/hud`): one screen that answers "what is going on?".
//!
//! Six panels — SESSION, CONTEXT, USAGE on the left; TASKS, AGENTS, ACTIVITY on the right (stacked when the terminal
//! is narrow). Tab or a click focuses a panel; the wheel or ↑/↓ scrolls the focused list. Everything shown is already
//! in `App` — the HUD adds no requests and no state of its own beyond focus and scroll.

use acp_core::{PodStatus, ToolStatus};
use cordata_view::key::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::{Kind, Outcome, chrome, contains, dim, panel, put};
use crate::app::{App, ConnState, Message};
use crate::extras::{TodoStatus, short_count};
use crate::ui::footer::{mode_color, short_path};
use crate::ui::hud;
use crate::ui::text::{display_width, truncate};

const PANELS: [&str; 7] = ["SESSION", "CONTEXT", "USAGE", "TASKS", "AGENTS", "RUN", "ACTIVITY"];

pub struct Dashboard {
    focus: usize,
    scroll: [usize; 7],
    hits: Vec<(Rect, usize)>,
}

impl Dashboard {
    pub fn new() -> Self {
        Self { focus: 6, scroll: [0; 7], hits: Vec::new() }
    }

    pub fn on_key(&mut self, _app: &mut App, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(3) => Outcome::close(None),
            KeyCode::Char('c') if ctrl => Outcome::close(None),
            KeyCode::Char('k') if ctrl => Outcome::switch(Kind::Palette),
            KeyCode::Tab => {
                self.focus = (self.focus + 1) % PANELS.len();
                Outcome::stay()
            }
            KeyCode::BackTab => {
                self.focus = (self.focus + PANELS.len() - 1) % PANELS.len();
                Outcome::stay()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll[self.focus] = self.scroll[self.focus].saturating_sub(1);
                Outcome::stay()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll[self.focus] += 1;
                Outcome::stay()
            }
            KeyCode::PageUp => {
                self.scroll[self.focus] = self.scroll[self.focus].saturating_sub(8);
                Outcome::stay()
            }
            KeyCode::PageDown => {
                self.scroll[self.focus] += 8;
                Outcome::stay()
            }
            KeyCode::Char('r') => Outcome::switch(Kind::Review),
            _ => Outcome::stay(),
        }
    }

    pub fn on_mouse(&mut self, _app: &mut App, mouse: MouseEvent) -> Outcome {
        let over = self.hits.iter().find(|(rect, _)| contains(*rect, mouse.column, mouse.row)).map(|(_, index)| *index);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = over {
                    self.focus = index;
                }
            }
            MouseEventKind::ScrollUp => {
                let target = over.unwrap_or(self.focus);
                self.focus = target;
                self.scroll[target] = self.scroll[target].saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                let target = over.unwrap_or(self.focus);
                self.focus = target;
                self.scroll[target] += 3;
            }
            _ => {}
        }
        Outcome::stay()
    }

    pub fn render(&mut self, app: &App, buffer: &mut Buffer, area: Rect) {
        let body = chrome(buffer, area, app, "HUD", "Tab/click focus a panel · ↑↓ wheel scroll · r diffview · Ctrl+K deck · Esc close");
        self.hits.clear();
        let wide = body.width >= 100;
        let heights = |total: u16, wants: &[u16]| -> Vec<u16> {
            // Fixed heights first; the last panel takes what is left. A panel that cannot fit is given 0.
            let mut left = total;
            let mut out = Vec::new();
            for (i, want) in wants.iter().enumerate() {
                let h = if i + 1 == wants.len() { left } else { (*want).min(left) };
                out.push(h);
                left = left.saturating_sub(h);
            }
            out
        };
        let mut place: Vec<(usize, Rect)> = Vec::new();
        if wide {
            let half = body.width / 2;
            let (lw, rw) = (half, body.width - half);
            let left = heights(body.height, &[8, 5, 0]);
            let right = heights(body.height, &[7, 6, 8, 0]);
            let mut y = body.y;
            for (i, h) in left.iter().enumerate() {
                place.push((i, Rect::new(body.x, y, lw, *h)));
                y += h;
            }
            let mut y = body.y;
            for (i, h) in right.iter().enumerate() {
                place.push((3 + i, Rect::new(body.x + lw, y, rw, *h)));
                y += h;
            }
        } else {
            let all = heights(body.height, &[7, 4, 4, 6, 5, 7, 0]);
            let mut y = body.y;
            for (i, h) in all.iter().enumerate() {
                place.push((i, Rect::new(body.x, y, body.width, *h)));
                y += h;
            }
        }
        for (index, rect) in place {
            if rect.height < 3 {
                continue;
            }
            let inner = panel(buffer, rect, PANELS[index], index == self.focus);
            self.hits.push((rect, index));
            let lines = match index {
                0 => session_lines(app, inner.width as usize),
                1 => context_lines(app, inner.width as usize),
                2 => usage_lines(app),
                3 => task_lines(app, inner.width as usize),
                4 => agent_lines(app, inner.width as usize),
                5 => run_lines(app, inner.width as usize),
                _ => activity_lines(app, inner.width as usize),
            };
            // A list that is longer than its panel scrolls; one that fits does not.
            let rows = inner.height as usize;
            let max = lines.len().saturating_sub(rows);
            self.scroll[index] = self.scroll[index].min(max);
            for (offset, spans) in lines.into_iter().skip(self.scroll[index]).take(rows).enumerate() {
                put(buffer, inner.x, inner.y + offset as u16, spans, inner.width);
            }
            if max > 0 {
                let mark = if self.scroll[index] < max { "▾" } else { "▴" };
                buffer.set_string(rect.x + rect.width - 2, rect.y, mark, Style::new().fg(Color::Cyan));
            }
        }
    }
}

type Row = Vec<Span<'static>>;

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text:<9}"), dim())
}

fn session_lines(app: &App, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut mode = vec![label("mode")];
    mode.extend(hud::pill(&app.mode.to_uppercase(), mode_color(&app.mode)));
    let adaptive = app.settings.get("modes.adaptive").and_then(serde_json::Value::as_bool).unwrap_or(true);
    mode.push(Span::styled(if adaptive { "  adaptive · plans first when asked, never loosens" } else { "  adaptive off" }, dim()));
    rows.push(mode);
    if let Some(pending) = app.pending_mode.as_deref().filter(|p| *p != app.mode) {
        rows.push(vec![label(""), Span::styled(format!("→ {pending}… waiting for the backend"), Style::new().fg(mode_color(pending)))]);
    }
    let model: Vec<String> = [app.config("model"), app.config("temperature")].into_iter().flatten().map(|o| o.current_name().to_owned()).collect();
    rows.push(vec![label("model"), Span::styled(truncate(&model.join(" · "), width.saturating_sub(9)), Style::new().fg(Color::Magenta))]);
    let cwd = app.session.as_ref().map(|s| short_path(&s.cwd)).unwrap_or_default();
    let mut place = vec![label("where"), Span::styled(truncate(&cwd, width.saturating_sub(9 + 14)), Style::new().fg(Color::Gray))];
    if let Some(branch) = &app.branch {
        place.push(Span::styled(format!("  ⎇ {branch}"), Style::new().fg(Color::Gray)));
    }
    rows.push(place);
    let (state, color) = match (&app.conn, app.turn_active, app.pending.is_empty()) {
        (ConnState::Closed { .. }, _, _) => ("disconnected", Color::Red),
        (ConnState::Connecting, _, _) => ("connecting…", Color::Yellow),
        (_, _, false) => ("waiting for your approval", Color::Yellow),
        (_, true, _) => ("working", Color::Cyan),
        _ => ("idle", Color::Green),
    };
    let mut status = vec![label("status"), Span::styled(state, Style::new().fg(color))];
    if app.turn_active {
        status.push(Span::raw("  "));
        status.extend(hud::scanner(app.spinner, 12));
    }
    rows.push(status);
    if let Some(goal) = &app.goal {
        rows.push(vec![label("goal"), Span::styled(truncate(&format!("◎ {}/{} {}", goal.turn, goal.max, goal.condition), width.saturating_sub(9)), Style::new().fg(Color::Magenta))]);
    }
    rows
}

fn context_lines(app: &App, width: usize) -> Vec<Row> {
    let Some(gauge) = &app.context else {
        return vec![vec![Span::styled("no reading yet — it appears after the first turn", dim())]];
    };
    let approx = if gauge.estimated { "~" } else { "" };
    let bar = (width.saturating_sub(10)).clamp(8, 40);
    match gauge.pct {
        Some(pct) => {
            let color = if pct >= 0.85 { Color::Red } else if pct >= 0.6 { Color::Yellow } else { Color::Cyan };
            let window = gauge.window.map(|w| format!(" of {}", short_count(w))).unwrap_or_default();
            vec![
                vec![Span::styled(hud::segbar(pct, bar), Style::new().fg(color)), Span::styled(format!(" {approx}{:.0}%", pct * 100.0), Style::new().fg(color).add_modifier(Modifier::BOLD))],
                vec![Span::styled(format!("{approx}{} tokens{window}", short_count(gauge.used)), dim()), Span::styled(if pct >= 0.7 { "   /compact frees room" } else { "" }, Style::new().fg(Color::Yellow))],
            ]
        }
        None => vec![vec![Span::styled(format!("{approx}{} tokens — this model's window is unknown", short_count(gauge.used)), dim())]],
    }
}

fn usage_lines(app: &App) -> Vec<Row> {
    let (tokens_in, tokens_out) = app.session_tokens;
    vec![
        vec![label("tokens"), Span::styled(format!("{} in · {} out", short_count(tokens_in), short_count(tokens_out)), Style::new().fg(Color::White))],
        vec![label("turns"), Span::styled(app.turns_done.to_string(), Style::new().fg(Color::White))],
        vec![label("tools"), Span::styled(format!("{} this turn", app.turn_tools), dim())],
    ]
}

fn task_lines(app: &App, width: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = app
        .checklist
        .iter()
        .map(|item| {
            let (glyph, color) = match item.status {
                TodoStatus::Pending => ("☐", Color::Gray),
                TodoStatus::InProgress => ("◐", Color::Cyan),
                TodoStatus::Completed => ("☑", Color::Green),
            };
            vec![Span::styled(format!("{glyph} "), Style::new().fg(color)), Span::styled(truncate(&item.content, width.saturating_sub(2)), Style::new().fg(if item.status == TodoStatus::Completed { Color::DarkGray } else { Color::White }))]
        })
        .collect();
    for task in &app.tasks {
        let done = task.info.total.map_or(String::new(), |t| format!(" {:.0}/{:.0}", task.info.done, t));
        rows.push(vec![Span::styled("◌ ", Style::new().fg(Color::Magenta)), Span::styled(truncate(&format!("{}{done}", task.info.title), width.saturating_sub(2)), dim())]);
    }
    if rows.is_empty() {
        rows.push(vec![Span::styled("no task list — the agent keeps one for multi-step work", dim())]);
    }
    rows
}

fn agent_lines(app: &App, width: usize) -> Vec<Row> {
    let pods = &app.lab.state.pods;
    if pods.is_empty() {
        return vec![vec![Span::styled("no agents reported yet — F2 opens the Lab", dim())]];
    }
    pods.iter()
        .map(|pod| {
            let (glyph, color) = match pod.status {
                PodStatus::Idle => ("○", Color::DarkGray),
                PodStatus::Synthesizing => ("◉", Color::Magenta),
                PodStatus::Executing => ("◉", Color::Cyan),
                PodStatus::Contained => ("◈", Color::Yellow),
                PodStatus::Isolated => ("⊘", Color::Red),
            };
            let model = match (&pod.model, &pod.effort) {
                (Some(m), Some(e)) => format!("{m} · {e}"),
                (Some(m), None) => m.clone(),
                _ => String::new(),
            };
            vec![
                Span::styled(format!("{glyph} "), Style::new().fg(color)),
                Span::styled(format!("{:<14}", truncate(&pod.name, 14)), Style::new().fg(Color::White)),
                Span::styled(truncate(&model, width.saturating_sub(18)), dim()),
            ]
        })
        .collect()
}

fn run_lines(app: &App, width: usize) -> Vec<Row> {
    let Some(run) = &app.run else {
        return vec![vec![Span::styled("no orchestrated run — the agent starts one for multi-step work", dim())]];
    };
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
    let mut rows: Vec<Row> = vec![vec![
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(truncate(&format!("run {} · {}{progress}", run.run_id, run.state), width.saturating_sub(2)), Style::new().fg(color).add_modifier(Modifier::BOLD)),
    ]];
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
            Span::styled(format!("{:<6}", truncate(&step.id, 6)), Style::new().fg(Color::White)),
            Span::styled(truncate(&step.worker, width.saturating_sub(18 + display_width(&step.status))), Style::new().fg(Color::Magenta)),
            Span::styled(format!(" {}", step.status), dim()),
        ];
        if let Some(cost) = step.cost_usd.filter(|&c| c > 0.0) {
            spans.push(Span::styled(format!(" · ${cost:.4}"), Style::new().fg(Color::Yellow)));
        } else if let Some((tin, tout)) = step.tokens_in.zip(step.tokens_out).filter(|&(i, o)| i > 0 || o > 0) {
            spans.push(Span::styled(format!(" · {}/{} tok", short_count(tin), short_count(tout)), dim()));
        }
        rows.push(spans);
    }
    if let Some(handoff) = &run.handoff {
        rows.push(vec![Span::styled("⇄ ", Style::new().fg(Color::Yellow)), Span::styled(truncate(handoff, width.saturating_sub(2)), dim())]);
    }
    rows
}

fn activity_lines(app: &App, width: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for message in app.messages.iter().rev() {
        let Message::Tool(card) = message else { continue };
        let (glyph, color) = match card.status {
            ToolStatus::Pending => ("◌", Color::Gray),
            ToolStatus::InProgress => ("◐", Color::Cyan),
            ToolStatus::Completed => ("✓", Color::Green),
            ToolStatus::Failed => ("✕", Color::Red),
        };
        let input = card.raw_input.as_ref().map(summarize_input).unwrap_or_default();
        let name = format!("{:<16}", truncate(&card.name, 16));
        let room = width.saturating_sub(3 + display_width(&name) + 1);
        rows.push(vec![Span::styled(format!("{glyph} "), Style::new().fg(color)), Span::styled(name, Style::new().fg(Color::White)), Span::styled(format!(" {}", truncate(&input, room)), dim())]);
        if rows.len() >= 60 {
            break;
        }
    }
    if rows.is_empty() {
        rows.push(vec![Span::styled("no tool calls yet", dim())]);
    }
    rows
}

/// The one line that says what a call was about: the command, the path or the pattern.
fn summarize_input(input: &serde_json::Value) -> String {
    for key in ["command", "path", "pattern", "query", "url", "name", "task"] {
        if let Some(text) = input.get(key).and_then(serde_json::Value::as_str) {
            return text.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    if input.get("patch").is_some() {
        return "patch".into();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use acp_core::{AppEvent, ToolCallInfo, ToolKind};

    use super::super::testing::*;
    use super::*;
    use crate::overlay::Overlay;

    fn busy_app() -> App {
        let mut app = app();
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t1".into(),
            name: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"command": "cargo test"})),
            content: "ok".into(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t2".into(),
            name: "apply_patch".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Failed,
            raw_input: Some(serde_json::json!({"patch": "--- a\n+++ b"})),
            content: String::new(),
            untrusted_source: false,
            untrusted: false,
            diffs: vec![],
        }));
        app.session_tokens = (12_400, 3_100);
        app.turns_done = 4;
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Context(serde_json::json!({"used": 41000, "window": 100000, "pct": 0.41}))));
        app.on_event(AppEvent::Harness(acp_core::HarnessEvent::TaskList(serde_json::json!({
            "items": [{"content": "add models.py", "status": "completed"}, {"content": "update the loader", "status": "in_progress"}, {"content": "run the tests", "status": "pending"}],
            "done": 1, "total": 3
        }))));
        app
    }

    #[test]
    fn every_panel_shows_what_the_app_knows() {
        let app = busy_app();
        let shown = text(&Overlay::Hud(Dashboard::new()).render(&app, 120, 40));
        for needle in [
            "HUD", "╴// SESSION", "BUILD", "kimi-k2.7-code", "adaptive", "╴// CONTEXT", "41%", "41k tokens of 100k", "╴// USAGE", "12k in · 3.1k out", "╴// TASKS", "☑ add models.py", "◐ update the loader",
            "☐ run the tests", "╴// AGENTS", "╴// RUN", "no orchestrated run", "╴// ACTIVITY", "✓ shell", "cargo test", "✕ apply_patch",
        ] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
    }

    #[test]
    fn a_run_is_drawn_as_a_board_of_steps_with_their_workers() {
        let mut app = busy_app();
        for event in [
            serde_json::json!({"kind": "state", "run_id": "r7", "state": "executing", "steps": 3}),
            serde_json::json!({"kind": "step_dispatch", "run_id": "r7", "step_id": "s1", "worker": "ollama:kimi-k2.7-code"}),
            serde_json::json!({"kind": "step_result", "run_id": "r7", "step_id": "s1", "status": "success"}),
            serde_json::json!({"kind": "step_dispatch", "run_id": "r7", "step_id": "s2", "worker": "claude_code:sonnet"}),
            serde_json::json!({"kind": "handoff", "run_id": "r7", "from_worker": "claude_code:sonnet", "to_worker": "antigravity:gemini", "reason": "quota"}),
        ] {
            app.on_event(AppEvent::Harness(acp_core::HarnessEvent::Orchestrator(event)));
        }
        let shown = text(&Overlay::Hud(Dashboard::new()).render(&app, 120, 44));
        for needle in ["run r7 · executing 1/3", "✓ s1", "ollama:kimi-k2.7-code", "◐ s2", "claude_code:sonnet", "⇄ claude_code:sonnet → antigravity:gemini (quota)"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
    }

    #[test]
    fn a_narrow_terminal_stacks_the_panels_and_still_fits() {
        let app = busy_app();
        let shown = text(&Overlay::Hud(Dashboard::new()).render(&app, 60, 40));
        assert!(shown.contains("╴// SESSION") && shown.contains("╴// ACTIVITY"), "{shown}");
        assert!(shown.lines().all(|line| line.chars().count() <= 60));
    }

    #[test]
    fn tab_and_clicks_focus_panels_and_the_wheel_scrolls_the_focused_one() {
        let mut app = busy_app();
        for n in 0..40 {
            app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
                id: format!("x{n}"),
                name: format!("tool{n}"),
                kind: ToolKind::Read,
                status: ToolStatus::Completed,
                raw_input: Some(serde_json::json!({"path": format!("file{n}.rs")})),
                content: String::new(),
                untrusted_source: false,
                untrusted: false,
                diffs: vec![],
            }));
        }
        let mut overlay = Overlay::Hud(Dashboard::new());
        let first = text(&overlay.render(&app, 120, 30));
        assert!(first.contains("tool39") && !first.contains("tool0 "), "newest first:\n{first}");
        overlay.on_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 100, 25));
        let scrolled = text(&overlay.render(&app, 120, 30));
        assert!(!scrolled.contains("tool39"), "the wheel over ACTIVITY scrolls it:\n{scrolled}");
        assert!(scrolled.contains("▴") || scrolled.contains("▾"), "a scrolling panel says so");

        let buffer = overlay.render(&app, 120, 30);
        let (column, row) = find(&buffer, "╴// SESSION").unwrap();
        overlay.on_mouse(&mut app, click(column + 2, row));
        assert!(matches!(&overlay, Overlay::Hud(h) if h.focus == 0), "a click focuses the panel");
        overlay.on_key(&mut app, key(KeyCode::Tab));
        assert!(matches!(&overlay, Overlay::Hud(h) if h.focus == 1), "Tab moves on");
    }

    #[test]
    fn escape_closes_and_the_other_views_are_one_key_away() {
        let mut app = busy_app();
        let mut hud = Dashboard::new();
        assert!(hud.on_key(&mut app, key(KeyCode::Esc)).close);
        assert_eq!(hud.on_key(&mut app, key(KeyCode::Char('r'))).open, Some(Kind::Review));
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(hud.on_key(&mut app, ctrl_k).open, Some(Kind::Palette));
    }
}
