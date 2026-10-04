//! Background tasks as progress bars in the live region, just above the prompt:
//! plugin start-up, language servers indexing, MCP tools reporting progress, the
//! self-heal loop diagnosing and verifying, Laya loading its weights.
//!
//! ```text
//! ⠹ lsp · rust: indexing  ██████████▍          52%  crates/acp-core
//! ⠹ self-heal: verifying in sandbox (1/3)  12s
//! ✓ plugin echo  2 tools
//! ```
//!
//! Running tasks stay until they finish; finished ones linger briefly so a fast
//! task is still seen, then disappear (they never enter scrollback).

use std::time::{Duration, Instant};

use acp_core::{TaskInfo, TaskState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, fit_spans, truncate};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_CELLS: usize = 20;
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
pub const LINGER: Duration = Duration::from_millis(2500);
pub const MAX_ROWS: usize = 5;

#[derive(Debug, Clone)]
pub struct TaskView {
    pub info: TaskInfo,
    pub started: Instant,
    pub finished: Option<Instant>,
}

impl TaskView {
    pub fn visible(&self, now: Instant) -> bool {
        self.finished
            .is_none_or(|at| now.duration_since(at) < LINGER)
    }
}

/// A bar of `cells` cells, smooth to an eighth of a cell.
pub fn bar(fraction: f64, cells: usize) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * cells as f64 * 8.0).round() as usize;
    let (full, part) = (filled / 8, filled % 8);
    let mut out = "█".repeat(full.min(cells));
    if full < cells {
        out.push_str(EIGHTHS[part]);
        let used = full + usize::from(part > 0);
        out.push_str(&" ".repeat(cells - used));
    }
    out
}

fn row(task: &TaskView, width: usize, spinner: usize, now: Instant) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    let (glyph, color) = match task.info.state {
        TaskState::Running => (SPINNER[spinner % SPINNER.len()], Color::Yellow),
        TaskState::Completed => ("✓", Color::Green),
        TaskState::Failed => ("✗", Color::Red),
    };
    let title = crate::app::sanitize(&task.info.title);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(
            truncate(&title, 38),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    match (task.info.state, task.info.fraction()) {
        (TaskState::Running, Some(fraction)) => {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                bar(fraction, BAR_CELLS),
                Style::new().fg(Color::Cyan),
            ));
            spans.push(Span::styled(
                format!(" {:>3}%", (fraction * 100.0).round() as i64),
                Style::new().fg(Color::Cyan),
            ));
        }
        (TaskState::Running, None) => {
            let secs = now.duration_since(task.started).as_secs();
            if secs > 0 {
                spans.push(Span::styled(format!("  {secs}s"), dim));
            }
        }
        _ => {}
    }
    let message = crate::app::sanitize(&task.info.message).replace('\n', " ");
    if !message.is_empty() {
        let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        let room = width.saturating_sub(used + 2);
        if room > 6 {
            let style = if task.info.state == TaskState::Failed {
                Style::new().fg(Color::Red)
            } else {
                dim
            };
            spans.push(Span::styled(
                format!("  {}", truncate(&message, room)),
                style,
            ));
        }
    }
    fit_spans(spans, width)
}

/// The rows to draw now (running first, newest finished after), capped.
pub fn lines(tasks: &[TaskView], width: u16, spinner: usize) -> Vec<Line<'static>> {
    let now = Instant::now();
    let mut visible: Vec<&TaskView> = tasks.iter().filter(|t| t.visible(now)).collect();
    visible.sort_by_key(|t| (t.info.state != TaskState::Running, t.started));
    let width = width as usize;
    let mut out: Vec<Line<'static>> = visible
        .iter()
        .take(MAX_ROWS)
        .map(|task| row(task, width, spinner, now))
        .collect();
    if visible.len() > MAX_ROWS {
        out.push(Line::styled(
            format!("  +{} more background tasks", visible.len() - MAX_ROWS),
            Style::new().fg(Color::DarkGray),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: TaskState, done: f64, total: Option<f64>) -> TaskView {
        TaskView {
            info: TaskInfo {
                id: "t".into(),
                title: "lsp: indexing".into(),
                done,
                total,
                message: "crates/acp-core".into(),
                state,
                source: "lsp".into(),
            },
            started: Instant::now(),
            finished: (state != TaskState::Running).then(Instant::now),
        }
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn bars_fill_smoothly_and_keep_their_width() {
        assert_eq!(bar(0.0, 4), "    ");
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 4), "██  ");
        assert_eq!(display_width(&bar(0.33, 20)), 20);
        assert_eq!(bar(0.0625, 2), "▏ ");
    }

    #[test]
    fn determinate_running_tasks_show_a_bar_and_percent() {
        let text = plain(&lines(&[task(TaskState::Running, 52.0, Some(100.0))], 100, 0)[0]);
        assert!(
            text.contains("lsp: indexing") && text.contains("52%") && text.contains('█'),
            "{text}"
        );
        assert!(text.contains("crates/acp-core"));
    }

    #[test]
    fn finished_tasks_linger_then_vanish_and_rows_are_capped() {
        let mut done = task(TaskState::Completed, 1.0, Some(1.0));
        assert_eq!(lines(std::slice::from_ref(&done), 80, 0).len(), 1);
        done.finished = Some(Instant::now() - LINGER - Duration::from_millis(1));
        assert!(lines(&[done], 80, 0).is_empty());
        let many: Vec<TaskView> = (0..8)
            .map(|_| task(TaskState::Running, 0.0, None))
            .collect();
        let rows = lines(&many, 80, 0);
        assert_eq!(rows.len(), MAX_ROWS + 1);
        assert!(plain(rows.last().unwrap()).contains("+3 more"));
    }

    #[test]
    fn rows_never_exceed_the_width() {
        let mut long = task(TaskState::Running, 3.0, Some(10.0));
        long.info.message = "x".repeat(300);
        for width in [30u16, 60, 120] {
            let text = plain(&lines(&[long.clone()], width, 0)[0]);
            assert!(display_width(&text) <= width as usize, "{width}: {text}");
        }
    }
}
