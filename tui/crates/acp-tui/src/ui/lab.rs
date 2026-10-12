//! The Experimental Lab canvas: containment pods, A2A links, the Laya threat
//! gauge, the memory radar, the resource monitor, the inspector and the HITL
//! station — drawn in the live region like every other panel (the UI is inline,
//! there is no screen to take over), so the transcript above it stays in the
//! terminal's scrollback untouched.
//!
//! Everything here is a pure function of `App`: the Lab's own state comes from
//! `app.lab`, the staged intervention from `app.pending` (the ACP permission the
//! backend is parked on — the HITL station is that request, drawn bigger).

use acp_core::{LabPod, LabThreat, PodStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::LiveFrame;
use super::footer;
use super::panels;
use super::text::{display_width, truncate, wrap};
use crate::app::{App, sanitize};

const CARD_WIDTH: usize = 27;
const CARD_HEIGHT: usize = 5;
const TRACE_ROWS: usize = 8;
const STREAM_ROWS: usize = 5;
const PROMPT_ROWS: usize = 5;

const CYAN: Color = Color::Cyan;
const GREEN: Color = Color::Green;
const AMBER: Color = Color::Yellow;
const RED: Color = Color::Red;
const MAGENTA: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;

/// The badge a pod wears. The text is the contract (tests and docs quote it).
pub fn badge(status: PodStatus) -> &'static str {
    match status {
        PodStatus::Idle => "[IDLE 💤]",
        PodStatus::Synthesizing => "[SYNTHESIZING 🧪]",
        PodStatus::Executing => "[EXECUTING ⚡]",
        PodStatus::Contained => "[CONTAINED ⚠️]",
        PodStatus::Isolated => "[ISOLATED 🛑]",
    }
}

fn status_color(status: PodStatus) -> Color {
    match status {
        PodStatus::Idle => DIM,
        PodStatus::Synthesizing => CYAN,
        PodStatus::Executing => GREEN,
        PodStatus::Contained => AMBER,
        PodStatus::Isolated => RED,
    }
}

/// `value` in 0.0..=1.0 as a bar of `width` cells.
pub fn meter(value: f64, width: usize) -> String {
    let filled = ((value.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width - filled.min(width))
    )
}

fn band_color(score: f64) -> Color {
    if score > 0.8 {
        RED
    } else if score < 0.2 {
        GREEN
    } else {
        AMBER
    }
}

fn label(text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
    )
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(DIM))
}

fn effort_color(effort: &str) -> Color {
    match effort {
        "max" => RED,
        "high" => AMBER,
        "medium" => CYAN,
        _ => GREEN,
    }
}

// ------------------------------------------------------------------- pods

/// One pod as `CARD_HEIGHT` rows of exactly `CARD_WIDTH` columns.
fn card(pod: &LabPod, selected: bool) -> Vec<Line<'static>> {
    let color = status_color(pod.status);
    let border = if selected {
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(color)
    };
    let inner = CARD_WIDTH - 2;
    let title = truncate(&format!(" {} ", pod.name), inner.saturating_sub(2));
    let top = format!(
        "╭{}{}╮",
        title,
        "─".repeat(inner.saturating_sub(display_width(&title)))
    );
    let side = |content: Vec<Span<'static>>| {
        let used: usize = content
            .iter()
            .map(|span| display_width(&span.content))
            .sum();
        let mut spans = vec![Span::styled("│", border)];
        spans.extend(content);
        spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        spans.push(Span::styled("│", border));
        Line::from(spans)
    };
    let badge_style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    let model = pod.model.clone().unwrap_or_else(|| "—".into());
    let effort = pod.effort.clone().unwrap_or_else(|| "—".into());
    let dial_style = if (pod.dial - pod.dial_default).abs() > 1e-9 {
        Style::new().fg(AMBER).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(DIM)
    };
    vec![
        Line::from(vec![
            Span::styled(if selected { "▶" } else { "╭" }, border),
            Span::styled(top.chars().skip(1).collect::<String>(), border),
        ]),
        side(vec![
            Span::raw(" "),
            Span::styled(badge(pod.status), badge_style),
        ]),
        side(vec![
            Span::raw(" "),
            Span::styled(
                truncate(&model, inner.saturating_sub(10)),
                Style::new().fg(Color::White),
            ),
            dim(" · "),
            Span::styled(effort.clone(), Style::new().fg(effort_color(&effort))),
        ]),
        side(vec![
            Span::raw(" "),
            Span::styled(format!("dial {:.2}", pod.dial), dial_style),
            dim(format!(" · {} calls", pod.calls)),
        ]),
        Line::from(vec![Span::styled(
            format!("╰{}╯", "─".repeat(inner)),
            border,
        )]),
    ]
}

/// Cards left to right, wrapping when the terminal is narrow.
fn pod_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let lab = &app.lab;
    let pods = lab.pods();
    if pods.is_empty() {
        return vec![Line::from(vec![
            Span::raw("  "),
            dim(if lab.seen {
                "no agents yet — the pods appear when the session starts"
            } else {
                "waiting for the backend's first Lab snapshot…"
            }),
        ])];
    }
    let selected = lab.selected_index().unwrap_or(0);
    let per_row = ((width.saturating_sub(1)) / (CARD_WIDTH + 1)).max(1);
    let mut lines = Vec::new();
    for (row, chunk) in pods.chunks(per_row).enumerate() {
        let cards: Vec<Vec<Line<'static>>> = chunk
            .iter()
            .enumerate()
            .map(|(i, pod)| card(pod, row * per_row + i == selected))
            .collect();
        for line in 0..CARD_HEIGHT {
            let mut spans = vec![Span::raw(" ")];
            for card in &cards {
                spans.extend(card[line].spans.iter().cloned());
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// One line per pod, for a terminal too short for cards.
fn compact_pod_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let lab = &app.lab;
    let selected = lab.selected_index().unwrap_or(0);
    lab.pods()
        .iter()
        .enumerate()
        .map(|(i, pod)| {
            let model = pod.model.as_deref().unwrap_or("—");
            let effort = pod.effort.as_deref().unwrap_or("—");
            let text = format!(
                " {} {:<13} {:<22} {} · {} · dial {:.2}",
                if i == selected { "▶" } else { " " },
                pod.name,
                badge(pod.status),
                model,
                effort,
                pod.dial
            );
            Line::styled(
                truncate(&text, width),
                Style::new().fg(status_color(pod.status)),
            )
        })
        .collect()
}

// -------------------------------------------------------------- telemetry

fn link_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let links = &app.lab.state.links;
    let mut lines = vec![];
    let mut sorted: Vec<_> = links.iter().collect();
    // Live vectors first, then the most used.
    sorted.sort_by(|a, b| b.active.cmp(&a.active).then(b.count.cmp(&a.count)));
    for link in sorted.into_iter().take(3) {
        const PULSE: [&str; 4] = ["◐", "◓", "◑", "◒"];
        let (arrow, color, mark) = if link.active {
            ("━━━▶", GREEN, PULSE[app.spinner % PULSE.len()].to_owned())
        } else {
            ("───▶", DIM, "○".to_owned())
        };
        let note = if link.label.is_empty() {
            String::new()
        } else {
            format!("  “{}”", sanitize(&link.label))
        };
        let text = format!(
            "{} {} {}  {} ×{}{}",
            link.from, arrow, link.to, link.kind, link.count, note
        );
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(mark, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(
                truncate(&text, width.saturating_sub(4)),
                Style::new().fg(color),
            ),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![Span::raw("   "), dim("no delegation yet")]));
    }
    lines
}

fn threat_line(threat: Option<&LabThreat>, width: usize) -> Line<'static> {
    let head = label(" THREAT ");
    let Some(threat) = threat else {
        return Line::from(vec![head, dim(" no pending tool call")]);
    };
    let Some(score) = threat.score else {
        return Line::from(vec![
            head,
            dim(format!(
                " {} @ {} — Laya has not rated this turn",
                threat.tool, threat.pod
            )),
        ]);
    };
    let confidence = threat.confidence.unwrap_or(0.0);
    let color = band_color(score);
    let band = threat.band.clone().unwrap_or_default().to_uppercase();
    let bar = if width >= 90 { 14 } else { 8 };
    let mut spans = vec![
        head,
        dim("▕"),
        Span::styled(meter(score, bar), Style::new().fg(color)),
        dim("▏ "),
        Span::styled(
            format!("{score:.2} {band}"),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        dim("  confidence ▕"),
        Span::styled(meter(confidence, bar), Style::new().fg(CYAN)),
        dim("▏ "),
        Span::styled(format!("{confidence:.2}"), Style::new().fg(CYAN)),
    ];
    spans.push(dim(format!(
        "  {} @ {}{}",
        threat.tool,
        threat.pod,
        if threat.pending { " ▸ pending" } else { "" }
    )));
    Line::from(spans)
}

fn radar_lines(pod: Option<&LabPod>, width: usize) -> Vec<Line<'static>> {
    let head = |name: &str| vec![label(" RADAR "), dim(format!("▸ {name}  "))];
    let Some(pod) = pod else {
        let mut spans = head("—");
        spans.push(dim("no pod selected"));
        return vec![Line::from(spans)];
    };
    if pod.memory.is_empty() {
        let mut spans = head(&pod.name);
        spans.push(dim(
            "no memory reads yet (LanceDB / knowledge graph / Qdrant)",
        ));
        return vec![Line::from(spans)];
    }
    pod.memory
        .iter()
        .rev()
        .take(3)
        .enumerate()
        .map(|(i, read)| {
            let mut spans = if i == 0 {
                head(&pod.name)
            } else {
                vec![Span::raw(" ".repeat(8 + display_width(&pod.name) + 4))]
            };
            let noun = if read.source == "kg" { "nodes" } else { "hits" };
            let hits: Vec<String> = read
                .hits
                .iter()
                .take(3)
                .map(|(label, _)| sanitize(label))
                .collect();
            let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
            let body = format!(
                "{} ◉ {} {} · “{}”{}{}",
                read.source,
                read.count,
                noun,
                sanitize(&read.query),
                if hits.is_empty() { "" } else { " → " },
                hits.join(", ")
            );
            spans.push(Span::styled(
                truncate(&body, width.saturating_sub(used)),
                Style::new().fg(MAGENTA),
            ));
            Line::from(spans)
        })
        .collect()
}

fn monitor_line(app: &App, pod: Option<&LabPod>, width: usize) -> Line<'static> {
    let resources = &app.lab.state.resources;
    let mut spans = vec![label(" MONITOR ")];
    if let Some(pod) = pod {
        let effort = pod.effort.clone().unwrap_or_else(|| "—".into());
        spans.push(dim(format!("▸ {}  ", pod.name)));
        spans.push(Span::styled(
            pod.model.clone().unwrap_or_else(|| "no model yet".into()),
            Style::new().fg(Color::White),
        ));
        spans.push(dim(" · effort "));
        spans.push(Span::styled(
            effort.clone(),
            Style::new()
                .fg(effort_color(&effort))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(dim(format!(" · busy {:.1}s", pod.busy_ms as f64 / 1000.0)));
    }
    spans.push(dim(format!(
        " · cpu {:.0}% · rss {:.0} MB (process)",
        resources.cpu_percent, resources.rss_mb
    )));
    spans.push(dim(if resources.mcp.is_empty() {
        " · mcp none".to_owned()
    } else {
        format!(" · mcp {}", resources.mcp.join(", "))
    }));
    super::text::fit_spans(spans, width)
}

// -------------------------------------------------------------- inspector

fn inspector_lines(pod: &LabPod, width: usize, budget: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(10);
    let mut lines = vec![Line::from(vec![
        label(" INSPECT "),
        Span::styled(
            format!("▸ {}", pod.name),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        dim(format!(
            "  {} messages · {} calls · role {}{}",
            pod.messages,
            pod.calls,
            pod.role,
            pod.parent
                .as_deref()
                .map(|parent| format!(" · under {parent}"))
                .unwrap_or_default()
        )),
    ])];
    let section = |title: &str| Line::styled(format!("  {title}"), Style::new().fg(CYAN));

    lines.push(section("system prompt"));
    match &pod.system_prompt {
        Some(prompt) => {
            let rows = wrap(&sanitize(prompt), inner);
            let shown = rows.len().min(PROMPT_ROWS);
            for row in rows.iter().take(shown) {
                lines.push(Line::styled(
                    format!("    {row}"),
                    Style::new().fg(Color::Gray),
                ));
            }
            if rows.len() > shown {
                lines.push(Line::from(vec![dim(format!(
                    "    … {} more lines",
                    rows.len() - shown
                ))]));
            }
        }
        None => lines.push(Line::from(vec![dim("    (loading…)")])),
    }

    lines.push(section("tool trace"));
    if pod.trace.is_empty() {
        lines.push(Line::from(vec![dim("    nothing yet")]));
    }
    for row in pod
        .trace
        .iter()
        .rev()
        .take(TRACE_ROWS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let operator = matches!(row.tool.as_str(), "GUIDANCE" | "ISOLATE");
        let (glyph, color) = match (operator, row.status.as_str()) {
            (true, _) => ("◆", MAGENTA),
            (_, "running") => ("●", AMBER),
            (_, "failed") => ("✗", RED),
            _ => ("✓", GREEN),
        };
        let ms = if row.ms > 0 {
            format!(" {}ms", row.ms)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(glyph, Style::new().fg(color)),
            Span::styled(
                format!(" {}", sanitize(&row.tool)),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            dim(truncate(
                &format!(" {}{}", sanitize(&row.input), ms),
                inner.saturating_sub(display_width(&row.tool) + 6),
            )),
        ]));
    }

    lines.push(section("stream"));
    if pod.stream.trim().is_empty() {
        lines.push(Line::from(vec![dim("    (nothing streamed)")]));
    } else {
        let rows = wrap(&sanitize(&pod.stream), inner);
        for row in rows.iter().skip(rows.len().saturating_sub(STREAM_ROWS)) {
            lines.push(Line::raw(format!("    {row}")));
        }
    }
    if !pod.guidance.is_empty() || pod.guidance_pending > 0 {
        lines.push(section("guidance queued"));
        for text in &pod.guidance {
            lines.push(Line::styled(
                format!("    ◆ {}", truncate(&sanitize(text), inner)),
                Style::new().fg(MAGENTA),
            ));
        }
        if pod.guidance.is_empty() {
            lines.push(Line::styled(
                format!(
                    "    ◆ {} directive(s) waiting for its next model call",
                    pod.guidance_pending
                ),
                Style::new().fg(MAGENTA),
            ));
        }
    }
    if lines.len() > budget && budget > 2 {
        // Keep the header and the newest rows of the last sections.
        let drop = lines.len() - budget + 1;
        lines.drain(2..2 + drop);
        lines.insert(
            2,
            Line::from(vec![dim(format!(
                "  … {drop} rows hidden — enlarge the terminal"
            ))]),
        );
    }
    lines
}

fn keys_line(app: &App, width: usize) -> Line<'static> {
    let key = |text: &'static str, color: Color| {
        Span::styled(
            text,
            Style::new()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    };
    let spans = vec![
        Span::raw(" "),
        key(" ←→ ", CYAN),
        dim(" pod "),
        key(" ⏎ ", CYAN),
        dim(if app.lab.inspecting {
            " collapse "
        } else {
            " inspect "
        }),
        key(" K ", RED),
        dim(" E-stop "),
        key(" R ", GREEN),
        dim(" release "),
        key(" [ ] ", AMBER),
        dim(" laya dial "),
        key(" I ", MAGENTA),
        dim(" guide "),
        key(" Y/N ", AMBER),
        dim(" HITL "),
        key(" Esc ", DIM),
    ];
    super::text::fit_spans(spans, width)
}

fn header_line(app: &App, width: usize) -> Line<'static> {
    let lab = &app.lab;
    let session = lab
        .state
        .session_id
        .as_deref()
        .map(|id| id.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "—".into());
    let pending = app.pending.len();
    let mut spans = vec![
        Span::styled(
            " ⚗ EXPERIMENTAL LAB ",
            Style::new()
                .fg(Color::Black)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        dim(format!(
            "  session {session} · {} pods · {} busy · seq {}",
            lab.pods().len(),
            lab.busy(),
            lab.state.seq
        )),
    ];
    if pending > 0 {
        spans.push(Span::styled(
            format!("  ⚠ {pending} intervention(s) awaiting you"),
            Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        ));
    }
    super::text::fit_spans(spans, width)
}

/// The Lab as the live region. `max_rows` is the terminal's height; the region
/// takes what it needs up to that.
pub fn render_live(app: &App, width: u16, max_rows: u16) -> LiveFrame {
    let width = width.max(40);
    let w = width as usize;
    let max_rows = max_rows.max(8) as usize;
    let lab = &app.lab;
    let selected = lab.selected_pod();
    let compact = max_rows < 24;

    // The bottom: a staged intervention (HITL station) beats the guidance box,
    // which beats the key hints.
    let station = panels::current(app, width).filter(|_| !app.pending.is_empty());
    let guidance_rows = if lab.guidance.is_some() { 3 } else { 0 };
    let station_rows = station
        .as_ref()
        .map(|panel| panels::height(panel, (max_rows as u16).saturating_sub(10)) as usize)
        .unwrap_or(0);
    let notice = lab.notice.as_ref().map(|(text, error)| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(text, w.saturating_sub(2)),
                Style::new()
                    .fg(if *error { RED } else { GREEN })
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    });

    let mut top: Vec<Line<'static>> = vec![header_line(app, w)];
    top.extend(if compact {
        compact_pod_lines(app, w)
    } else {
        pod_lines(app, w)
    });
    top.extend(link_lines(app, w));
    top.push(threat_line(lab.state.threat.as_ref(), w));
    top.extend(radar_lines(selected, w));
    top.push(monitor_line(app, selected, w));

    let mut bottom_rows = station_rows.max(guidance_rows) + usize::from(notice.is_some());
    if station_rows == 0 && guidance_rows == 0 {
        bottom_rows += 1; // the key hints
    }
    let footer_rows = 1;
    let fixed = top.len() + bottom_rows + footer_rows;
    let mut inspector = Vec::new();
    if lab.inspecting
        && let Some(pod) = selected
    {
        let budget = max_rows.saturating_sub(fixed);
        if budget >= 4 {
            inspector = inspector_lines(pod, w, budget);
        }
    }
    let mut content: Vec<Line<'static>> = top;
    content.extend(inspector);
    let room = max_rows.saturating_sub(bottom_rows + footer_rows);
    if content.len() > room {
        // Never lose the header; the telemetry rows go before the pods do.
        let cut = content.len() - room;
        let keep_head = 1 + if compact { lab.pods().len().max(1) } else { 0 };
        let start = keep_head.min(content.len());
        let end = (start + cut).min(content.len());
        content.drain(start..end);
    }

    let total = (content.len() + bottom_rows + footer_rows) as u16;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, total));
    for (y, line) in content.iter().enumerate() {
        buffer.set_line(0, y as u16, line, width);
    }
    let mut y = content.len() as u16;
    let mut cursor = None;
    if let Some(notice) = notice {
        buffer.set_line(0, y, &notice, width);
        y += 1;
    }
    if let Some(panel) = station {
        let area = Rect::new(0, y, width, station_rows as u16);
        panels::render(&mut buffer, area, panel);
    } else if let Some(input) = &lab.guidance {
        let area = Rect::new(0, y, width, 3);
        cursor = Some(render_guidance(&mut buffer, area, app, input));
    } else {
        buffer.set_line(0, y, &keys_line(app, w), width);
    }
    buffer.set_line(0, total - 1, &footer::line(app, width), width);
    LiveFrame { buffer, cursor }
}

fn render_guidance(
    buffer: &mut Buffer,
    area: Rect,
    app: &App,
    input: &crate::input::Input,
) -> (u16, u16) {
    use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
    let name = app.lab.selected_pod().map_or("?", |pod| pod.name.as_str());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(
            " Inject guidance → {name} · Enter sends · Esc cancels "
        ))
        .border_style(Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    let (rows, (row, col)) = super::input::layout(
        input.text(),
        input.cursor(),
        inner.width.saturating_sub(2) as usize,
    );
    let first = (row + 1).saturating_sub(inner.height as usize);
    let lines: Vec<Line> = rows
        .iter()
        .skip(first)
        .take(inner.height as usize)
        .enumerate()
        .map(|(i, text)| {
            Line::from(vec![
                Span::styled(
                    if first + i == 0 { "◆ " } else { "  " },
                    Style::new().fg(MAGENTA),
                ),
                Span::raw(text.clone()),
            ])
        })
        .collect();
    Paragraph::new(lines).block(block).render(area, buffer);
    (inner.x + 2 + col as u16, inner.y + (row - first) as u16)
}

#[cfg(test)]
mod tests {
    use acp_core::{LabHitl, LabLink, LabMemory, LabResources, LabState, LabTrace};

    use super::*;

    /// The frame as text, the way a terminal shows it: a wide glyph (an emoji)
    /// occupies its cell plus a continuation cell that must not be read twice.
    fn text_of(frame: &LiveFrame) -> String {
        let area = frame.buffer.area;
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < area.width {
                    let symbol = frame.buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += display_width(symbol).max(1) as u16;
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pod(name: &str, status: PodStatus, model: &str, effort: &str) -> LabPod {
        LabPod {
            id: name.into(),
            name: name.into(),
            role: name.into(),
            parent: (name != "orchestrator").then(|| "orchestrator".to_owned()),
            model: Some(model.into()),
            effort: Some(effort.into()),
            status,
            dial: 0.8,
            dial_default: 0.8,
            calls: 3,
            ..LabPod::default()
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.lab.on_state(LabState {
            seq: 12,
            session_id: Some("abcdef0123".into()),
            pods: vec![
                pod("orchestrator", PodStatus::Executing, "glm-5.3-flash", "low"),
                {
                    let mut p = pod("architect", PodStatus::Idle, "glm-5.3-flash", "high");
                    p.dial = 0.5;
                    p
                },
                {
                    let mut p = pod("debugger", PodStatus::Contained, "kimi-k2.7-code", "max");
                    p.memory = vec![LabMemory {
                        source: "lancedb".into(),
                        query: "cargo test failed".into(),
                        count: 3,
                        hits: vec![("cargo test".into(), Some(0.1))],
                    }];
                    p.trace = vec![LabTrace {
                        tool: "shell".into(),
                        status: "running".into(),
                        input: "cargo test".into(),
                        ms: 0,
                    }];
                    p.system_prompt = Some("You are the Debugger specialist".into());
                    p.stream = "reading the failure".into();
                    p
                },
            ],
            links: vec![LabLink {
                from: "orchestrator".into(),
                to: "debugger".into(),
                kind: "delegation".into(),
                active: true,
                count: 2,
                label: "fix the build".into(),
            }],
            threat: Some(LabThreat {
                pod: "debugger".into(),
                tool: "shell".into(),
                score: Some(0.92),
                confidence: Some(0.88),
                band: Some("destructive".into()),
                pending: true,
            }),
            hitl: vec![LabHitl::default()],
            resources: LabResources {
                cpu_percent: 12.4,
                rss_mb: 480.0,
                mcp: vec!["pihole".into()],
            },
            ..LabState::default()
        });
        app.lab.open = true;
        app
    }

    #[test]
    fn every_status_has_the_specified_badge() {
        assert_eq!(badge(PodStatus::Idle), "[IDLE 💤]");
        assert_eq!(badge(PodStatus::Synthesizing), "[SYNTHESIZING 🧪]");
        assert_eq!(badge(PodStatus::Executing), "[EXECUTING ⚡]");
        assert_eq!(badge(PodStatus::Contained), "[CONTAINED ⚠️]");
        assert_eq!(badge(PodStatus::Isolated), "[ISOLATED 🛑]");
    }

    #[test]
    fn the_canvas_shows_pods_links_threat_radar_and_monitor() {
        let app = app();
        let frame = render_live(&app, 110, 40);
        let text = text_of(&frame);
        for needle in [
            "EXPERIMENTAL LAB",
            "3 pods",
            "orchestrator",
            "architect",
            "debugger",
            "[EXECUTING ⚡]",
            "[IDLE 💤]",
            "[CONTAINED ⚠️]",
            "glm-5.3-flash",
            "kimi-k2.7-code",
            "dial 0.50",
            "orchestrator ━━━▶ debugger",
            "delegation ×2",
            "fix the build",
            "THREAT",
            "0.92 DESTRUCTIVE",
            "confidence",
            "0.88",
            "shell @ debugger",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        // the radar and monitor follow the selected pod (the first one, here)
        assert!(text.contains("RADAR") && text.contains("MONITOR"), "{text}");
        assert!(
            text.contains("cpu 12% · rss 480 MB (process) · mcp pihole"),
            "{text}"
        );
    }

    #[test]
    fn the_radar_and_monitor_follow_the_selected_pod() {
        let mut app = app();
        app.lab.selected = Some("debugger".into());
        let text = text_of(&render_live(&app, 110, 40));
        assert!(
            text.contains("lancedb ◉ 3 hits · “cargo test failed” → cargo test"),
            "{text}"
        );
        assert!(
            text.contains("▸ debugger") && text.contains("effort max"),
            "{text}"
        );
    }

    #[test]
    fn meters_scale_and_clamp() {
        assert_eq!(meter(0.0, 4), "░░░░");
        assert_eq!(meter(0.5, 4), "██░░");
        assert_eq!(meter(1.0, 4), "████");
        assert_eq!(meter(9.0, 4), "████");
        assert_eq!(meter(-1.0, 4), "░░░░");
    }

    #[test]
    fn the_inspector_expands_the_selected_pod() {
        let mut app = app();
        app.lab.selected = Some("debugger".into());
        app.lab.inspecting = true;
        let text = text_of(&render_live(&app, 110, 50));
        for needle in [
            "INSPECT ▸ debugger",
            "system prompt",
            "You are the Debugger specialist",
            "tool trace",
            "● shell",
            "cargo test",
            "stream",
            "reading the failure",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn a_short_terminal_gets_one_line_per_pod_and_keeps_the_header() {
        let app = app();
        let frame = render_live(&app, 100, 14);
        let text = text_of(&frame);
        assert!(
            frame.buffer.area.height <= 14,
            "never taller than the terminal"
        );
        assert!(
            text.contains("EXPERIMENTAL LAB") && text.contains("[CONTAINED ⚠️]"),
            "{text}"
        );
        assert!(
            !text.contains('╭'),
            "no cards when there is no room:\n{text}"
        );
    }

    #[test]
    fn a_narrow_terminal_wraps_the_cards() {
        let app = app();
        let frame = render_live(&app, 60, 50);
        let text = text_of(&frame);
        assert!(
            text.contains("orchestrator") && text.contains("debugger"),
            "{text}"
        );
        let widest = (0..frame.buffer.area.height)
            .map(|y| text.lines().nth(y as usize).map_or(0, display_width))
            .max()
            .unwrap();
        assert!(widest <= 60, "no row is wider than the terminal ({widest})");
    }

    #[test]
    fn an_empty_lab_says_what_it_is_waiting_for() {
        let mut app = App::new();
        app.lab.open = true;
        let text = text_of(&render_live(&app, 100, 30));
        assert!(
            text.contains("waiting for the backend's first Lab snapshot"),
            "{text}"
        );
        app.lab.seen = true;
        let text = text_of(&render_live(&app, 100, 30));
        assert!(text.contains("no agents yet"), "{text}");
        assert!(
            text.contains("no pending tool call") && text.contains("no delegation yet"),
            "{text}"
        );
    }

    #[test]
    fn the_guidance_box_takes_the_bottom_and_places_the_cursor() {
        let mut app = app();
        app.lab.selected = Some("debugger".into());
        app.lab.guidance = Some(crate::input::Input::default());
        app.lab.guidance.as_mut().unwrap().set_text("use pytest -x");
        let frame = render_live(&app, 100, 40);
        let text = text_of(&frame);
        assert!(
            text.contains("Inject guidance → debugger") && text.contains("◆ use pytest -x"),
            "{text}"
        );
        assert!(frame.cursor.is_some(), "the text cursor is in the box");
        assert!(
            !text.contains("E-stop"),
            "the key hints give way to the box"
        );
    }

    #[test]
    fn a_pending_approval_becomes_the_hitl_intervention_station() {
        let mut app = app();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.on_event(acp_core::AppEvent::PermissionRequested {
            info: acp_core::PermissionInfo {
                tool_call_id: "t1".into(),
                tool_name: "apply_patch".into(),
                title: "Run apply_patch".into(),
                kind: acp_core::ToolKind::Edit,
                raw_input: Some(serde_json::json!({"patch": "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old line\n+new line\n"})),
                untrusted_source: false,
                always_label: None,
                options: vec![],
            },
            decision: tx,
        });
        app.lab.state.hitl = vec![LabHitl {
            id: "i1".into(),
            pod: "debugger".into(),
            tool: "apply_patch".into(),
            status: "pending".into(),
            risk_score: Some(0.92),
            ..LabHitl::default()
        }];
        let text = text_of(&render_live(&app, 110, 50));
        for needle in [
            "HITL INTERVENTION STATION",
            "pod   debugger",
            "Laya destructiveness 0.92",
            "apply_patch",
            "old line",
            "new line",
            "Y  Approve & Execute",
            "N  Reject & Discard",
            "awaiting you",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(
            text.contains("EXPERIMENTAL LAB"),
            "the station sits under the Lab, not instead of it"
        );
        // closed Lab: the ordinary prompt, unchanged wording
        app.lab.open = false;
        let plain = text_of(&super::super::render_live(&app, 110, 50));
        assert!(
            plain.contains("Permission required") && !plain.contains("HITL INTERVENTION"),
            "{plain}"
        );
    }

    #[test]
    fn a_notice_is_shown_above_the_keys() {
        let mut app = app();
        app.lab.notice = Some((
            "orchestrator ISOLATED — every model and tool call is refused".into(),
            false,
        ));
        let text = text_of(&render_live(&app, 120, 40));
        assert!(
            text.contains("orchestrator ISOLATED") && text.contains("E-stop"),
            "{text}"
        );
    }
}
