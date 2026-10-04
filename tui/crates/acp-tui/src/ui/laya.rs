//! The live Laya triage line: what the fast local classifier decided the moment
//! a prompt was sent, and — once the turn ends — whether the LLM then did what
//! Laya expected. Two dim lines under the prompt:
//!
//! ```text
//! ◆ laya  files_and_code 98% ▸ hint sent · complexity 1.3 · 64 ms
//!    ↳ LLM used files_and_code (file_read, shell) ✓ agreed
//! ```

use acp_core::{LayaDecision, LayaOutcome};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn pct(value: f64) -> String {
    format!("{}%", (value * 100.0).round() as i64)
}

fn decision_line(decision: &LayaDecision, width: usize) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![Span::styled("◆ laya  ", Style::new().fg(Color::Magenta))];

    if let Some(error) = &decision.error {
        spans.push(Span::styled(format!("no decision — {error}"), dim));
        return fit(spans, width);
    }

    let route = decision.route.clone().unwrap_or_else(|| "?".to_owned());
    spans.push(Span::styled(
        format!("{route} {}", pct(decision.route_confidence)),
        Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
    ));

    // Router v2 (laya_router.py): say what the policy did with the decision.
    if let Some(action) = decision.action.as_deref() {
        if let Some(calibrated) = decision.calibrated
            && (calibrated - decision.route_confidence).abs() >= 0.005
        {
            spans.push(Span::styled(
                format!(" (calibrated {})", pct(calibrated)),
                dim,
            ));
        }
        let strong = |text: String, color: Color| {
            Span::styled(text, Style::new().fg(color).add_modifier(Modifier::BOLD))
        };
        spans.push(match (decision.abstained, action) {
            (true, "escalate") => strong(
                format!(
                    " ⛔ abstained → escalated to {}",
                    decision
                        .escalate_model
                        .as_deref()
                        .unwrap_or("the frontier tier")
                ),
                Color::Yellow,
            ),
            (true, _) => strong(
                " ⛔ abstained → the LLM decides alone".to_owned(),
                Color::Yellow,
            ),
            (false, "dispatch") => strong(
                format!(" ▸ dispatched to the {route} specialist"),
                Color::Cyan,
            ),
            (false, "scope") => strong(
                format!(" ▸ tools scoped to {}", route.trim_start_matches("plugin_")),
                Color::Cyan,
            ),
            (false, "hint") => strong(" ▸ hint sent".to_owned(), Color::Cyan),
            _ => Span::styled(" · confident, nothing to change".to_owned(), dim),
        });
        if decision.armed {
            spans.push(strong(
                " ⚠ destructive: approvals armed".to_owned(),
                Color::Red,
            ));
        } else if let Some(risk) = decision.risk.as_deref().filter(|r| *r != "read_only") {
            spans.push(Span::styled(
                format!(" · risk {}", risk.replace('_', " ")),
                dim,
            ));
        }
        if let Some(ms) = decision.latency_ms {
            spans.push(Span::styled(format!(" · {ms:.0} ms"), dim));
        }
        if decision.abstained
            && let Some(reason) = decision.reason.as_deref()
        {
            let why = reason.split(" — ").next().unwrap_or(reason);
            spans.push(Span::styled(format!(" · {why}"), dim));
        }
        return fit(spans, width);
    }

    let note = if decision.hint.is_some() {
        Span::styled(
            " ▸ hint sent",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else if decision.mode == "shadow" {
        Span::styled(" · shadow mode (log only)", dim)
    } else if decision.route_confidence < decision.gate {
        Span::styled(
            format!(" · below the {} gate, no hint", pct(decision.gate)),
            dim,
        )
    } else {
        Span::styled(" · no hint for this route", dim)
    };
    spans.push(note);

    spans.push(Span::styled(
        format!(" · complexity {:.1}", decision.complexity),
        dim,
    ));
    // The suggested sub-agent tier is deliberately not shown here: it matters only
    // if a team is spawned, and the outcome line reports the tiers actually used.
    if let Some(ms) = decision.latency_ms {
        spans.push(Span::styled(format!(" · {ms:.0} ms"), dim));
    }
    if decision.late {
        spans.push(Span::styled(" (late: after the turn had started)", dim));
    }
    fit(spans, width)
}

fn outcome_line(outcome: &LayaOutcome, width: usize) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled("   ↳ ", dim),
        Span::styled(format!("LLM used {}", outcome.actual_route), dim),
    ];
    if !outcome.tools.is_empty() {
        spans.push(Span::styled(
            format!(" ({})", outcome.tools.join(", ")),
            dim,
        ));
    }
    match outcome.agreed {
        Some(true) => spans.push(Span::styled(" ✓ agreed", Style::new().fg(Color::Green))),
        Some(false) => spans.push(Span::styled(" ✗ disagreed", Style::new().fg(Color::Yellow))),
        None => {}
    }
    match outcome.clean {
        Some(false) => spans.push(Span::styled(
            " · unresolved failure",
            Style::new().fg(Color::Red),
        )),
        Some(true) => spans.push(Span::styled(" · clean", dim)),
        None => {}
    }
    if let Some(loss) = outcome.log_loss {
        spans.push(Span::styled(format!(" · log-loss {loss:.2}"), dim));
    }
    let tiers: Vec<String> = outcome
        .tiers
        .iter()
        .map(|(node, tier)| format!("{node}={}", tier.as_deref().unwrap_or("default")))
        .collect();
    if !tiers.is_empty() {
        spans.push(Span::styled(format!(" · tiers: {}", tiers.join(", ")), dim));
    }
    fit(spans, width)
}

fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    super::text::fit_spans(spans, width)
}

pub fn decision_lines(decision: &LayaDecision, width: u16) -> Vec<Line<'static>> {
    vec![decision_line(decision, width as usize)]
}

pub fn outcome_lines(outcome: &LayaOutcome, width: u16) -> Vec<Line<'static>> {
    vec![outcome_line(outcome, width as usize)]
}

#[cfg(test)]
mod tests {
    use super::super::text::display_width;
    use super::*;

    fn decision(route: &str, confidence: f64, hint: Option<&str>) -> LayaDecision {
        LayaDecision {
            mode: "route".into(),
            route: Some(route.into()),
            route_confidence: confidence,
            gate: 0.85,
            needs_team: 0.1,
            complexity: 1.29,
            complexity_confidence: 0.6,
            latency_ms: Some(64.2),
            waited_ms: Some(64.2),
            hint: hint.map(Into::into),
            tier: None,
            late: false,
            error: None,
            ..Default::default()
        }
    }

    fn v2(route: &str, confidence: f64, action: &str, abstained: bool) -> LayaDecision {
        LayaDecision {
            action: Some(action.into()),
            abstained,
            calibrated: Some(confidence),
            reason: Some(if abstained {
                "Laya abstained (answer_confidence 0.43 < min_confidence 0.50) — escalated to big"
                    .into()
            } else {
                "dispatched".into()
            }),
            escalate_model: abstained.then(|| "big".into()),
            ..decision(route, confidence, None)
        }
    }

    #[test]
    fn router_v2_lines_say_what_the_policy_did() {
        let dispatch = plain(&render(v2("debugger", 0.97, "dispatch", false), None, 120));
        assert_eq!(
            dispatch[0],
            "◆ laya  debugger 97% ▸ dispatched to the debugger specialist · 64 ms"
        );
        let abstain = plain(&render(v2("code", 0.43, "escalate", true), None, 160));
        assert!(
            abstain[0].contains("⛔ abstained → escalated to big"),
            "{abstain:?}"
        );
        assert!(
            abstain[0].contains("Laya abstained (answer_confidence 0.43"),
            "{abstain:?}"
        );
        let scope = plain(&render(
            v2("plugin_pihole", 0.95, "scope", false),
            None,
            120,
        ));
        assert!(scope[0].contains("▸ tools scoped to pihole"));
        let mut armed = v2("code", 0.9, "hint", false);
        armed.armed = true;
        assert!(plain(&render(armed, None, 160))[0].contains("approvals armed"));
        let mut calibrated = v2("code", 0.9, "hint", false);
        calibrated.calibrated = Some(0.81);
        assert!(plain(&render(calibrated, None, 160))[0].contains("(calibrated 81%)"));
    }

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The decision line, then the outcome line if there is one — as they now
    /// appear one after the other in the transcript.
    fn render(
        decision: LayaDecision,
        outcome: Option<LayaOutcome>,
        width: u16,
    ) -> Vec<Line<'static>> {
        let mut lines = decision_lines(&decision, width);
        if let Some(outcome) = outcome {
            lines.extend(outcome_lines(&outcome, width));
        }
        lines
    }

    #[test]
    fn a_sent_hint_is_stated() {
        let lines = plain(&render(
            decision("files_and_code", 0.98, Some("h")),
            None,
            100,
        ));
        assert_eq!(
            lines,
            ["◆ laya  files_and_code 98% ▸ hint sent · complexity 1.3 · 64 ms"]
        );
    }

    #[test]
    fn below_the_gate_and_no_hint_routes_say_why_nothing_was_sent() {
        let low = plain(&render(decision("team", 0.26, None), None, 100));
        assert!(
            low[0].contains("team 26% · below the 85% gate, no hint"),
            "{low:?}"
        );
        let chat = plain(&render(decision("chat", 0.88, None), None, 100));
        assert!(
            chat[0].contains("chat 88% · no hint for this route"),
            "{chat:?}"
        );
        let mut shadow = decision("chat", 0.9, None);
        shadow.mode = "shadow".into();
        assert!(plain(&render(shadow, None, 100))[0].contains("shadow mode (log only)"));
    }

    #[test]
    fn timeouts_errors_and_late_answers_are_labelled() {
        let mut timed_out = decision("x", 0.0, None);
        timed_out.route = None;
        timed_out.error = Some("timed out after 2s — the LLM decides alone".into());
        assert_eq!(
            plain(&render(timed_out, None, 100))[0],
            "◆ laya  no decision — timed out after 2s — the LLM decides alone"
        );
        let mut late = decision("web_research", 0.9, None);
        late.late = true;
        assert!(plain(&render(late, None, 120))[0].contains("(late: after the turn had started)"));
    }

    #[test]
    fn the_outcome_line_shows_what_the_llm_did_and_whether_it_agreed() {
        let outcome = |agreed| LayaOutcome {
            actual_route: "files_and_code".into(),
            agreed,
            tools: vec!["file_read".into(), "shell".into()],
            tiers: vec![("cat".into(), Some("strong".into())), ("dog".into(), None)],
            stop_reason: Some("end_turn".into()),
            turn_ms: Some(900.0),
            ..Default::default()
        };
        let yes = plain(&render(
            decision("files_and_code", 0.98, Some("h")),
            Some(outcome(Some(true))),
            120,
        ));
        assert_eq!(
            yes[1],
            "   ↳ LLM used files_and_code (file_read, shell) ✓ agreed · tiers: cat=strong, dog=default"
        );
        let no = plain(&render(
            decision("chat", 0.9, None),
            Some(outcome(Some(false))),
            120,
        ));
        assert!(no[1].contains("✗ disagreed"));
        let none = plain(&render(
            decision("chat", 0.9, None),
            Some(outcome(None)),
            120,
        ));
        assert!(!none[1].contains("agreed"));
    }

    #[test]
    fn lines_never_exceed_the_pane_width() {
        let lines = render(decision("files_and_code", 0.98, Some("h")), None, 30);
        assert!(
            lines.iter().all(|l| l
                .spans
                .iter()
                .map(|s| display_width(&s.content))
                .sum::<usize>()
                <= 30),
            "{:?}",
            plain(&lines)
        );
    }
}
