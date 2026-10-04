//! The self-healing loop in the transcript: one block per phase that matters to
//! a reader (the in-between "diagnosing" / "verifying" phases are progress bars,
//! not scrollback).
//!
//! ```text
//! ⟲ self-heal · `cargo test -p calc` failed — Debugger engaged (up to 3 attempts)
//! ⟲ attempt 1/3 · root cause: add() subtracts instead of adding
//!   --- a/src/lib.rs
//!   +++ b/src/lib.rs
//!   @@ -1,2 +1,2 @@
//!   -    a - b
//!   +    a + b
//! ✓ verified in a sandbox copy: `cargo test -p calc` passed — applying through the approval gate
//! ✓ self-heal · `cargo test -p calc` passes now (14.2s)
//! ```

use acp_core::HealInfo;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::diff;
use super::text::{fit_spans, truncate, wrap};

const PATCH_ROWS: usize = 24;

fn code(text: &str) -> String {
    format!("`{}`", crate::app::sanitize(text).replace('\n', " "))
}

fn attempt(info: &HealInfo) -> String {
    match (info.attempt, info.max) {
        (Some(a), Some(m)) => format!("attempt {a}/{m}"),
        (Some(a), None) => format!("attempt {a}"),
        _ => "attempt".to_owned(),
    }
}

/// `None` for phases that show as progress bars instead.
pub fn lines(info: &HealInfo, width: u16, expanded: bool) -> Vec<Line<'static>> {
    let w = width as usize;
    let accent = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::Gray);
    let good = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let bad = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
    let command = info
        .command
        .as_deref()
        .map(code)
        .unwrap_or_else(|| "the command".into());
    let head = |glyph: &str, style: Style, text: String| {
        fit_spans(
            vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(text, dim),
            ],
            w,
        )
    };
    let mut out = Vec::new();
    match info.phase.as_str() {
        "detected" => {
            let max = info.max.map(|m| format!(" (up to {m} attempts)")).unwrap_or_default();
            out.push(fit_spans(
                vec![
                    Span::styled("⟲ self-heal ", accent),
                    Span::styled(format!("· {command} failed — Debugger engaged{max}"), dim),
                ],
                w,
            ));
            if let Some(tail) = &info.output_tail {
                // The shell tool's own labels ("Error:", "Output:") carry nothing on their own.
                let last: Vec<&str> = tail
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !matches!(l.trim(), "Error:" | "Output:" | "Status: error"))
                    .collect();
                for line in &last[last.len().saturating_sub(2)..] {
                    out.push(Line::styled(
                        format!("  │ {}", truncate(&crate::app::sanitize(line), w.saturating_sub(4))),
                        Style::new().fg(Color::Red),
                    ));
                }
            }
        }
        "proposed" => {
            let cause = info.root_cause.as_deref().unwrap_or("unknown");
            let rows = wrap(&format!("{} · root cause: {}", attempt(info), crate::app::sanitize(cause)), w.saturating_sub(2));
            for (i, row) in rows.into_iter().enumerate() {
                let lead = if i == 0 { Span::styled("⟲ ", accent) } else { Span::raw("  ") };
                out.push(Line::from(vec![lead, Span::styled(row, dim)]));
            }
            if let Some(patch) = &info.patch {
                let cap = (!expanded).then_some(PATCH_ROWS);
                for row in diff::patch_rows(patch, w.saturating_sub(2), cap) {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(row);
                    out.push(Line::from(spans));
                }
            }
        }
        "verified" => out.push(head(
            "✓",
            good,
            format!(
                "verified in a sandbox copy: {} passed — applying through the approval gate",
                info.verify.as_deref().map(code).unwrap_or(command)
            ),
        )),
        "verify_failed" => {
            out.push(head(
                "✗",
                bad,
                format!(
                    "{} · in the sandbox {} still fails{} — asking the Debugger again",
                    attempt(info),
                    info.command.as_deref().map(code).unwrap_or_default(),
                    info.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default()
                ),
            ));
        }
        "no_patch" => {
            let text = format!(
                "{} · root cause: {} — no applicable code fix",
                attempt(info),
                crate::app::sanitize(info.root_cause.as_deref().unwrap_or("unknown"))
            );
            for (i, row) in wrap(&text, w.saturating_sub(2)).into_iter().enumerate() {
                let lead = if i == 0 { Span::styled("⟲ ", accent) } else { Span::raw("  ") };
                out.push(Line::from(vec![lead, Span::styled(row, dim)]));
            }
        }
        "command_missing" => out.push(head(
            "⟲",
            accent,
            format!("{command} is not a command here (exit 127) — told the agent to call its tool directly"),
        )),
        "healed" => out.push(fit_spans(
            vec![
                Span::styled("✓ self-heal ", good),
                Span::styled(format!("· {command} passes now"), dim),
            ],
            w,
        )),
        "escalated" => out.push(fit_spans(
            vec![
                Span::styled("⚠ self-heal ", bad),
                Span::styled(format!("· gave up on {command} after {} attempt(s) — over to you", info.attempt.unwrap_or(0)), dim),
            ],
            w,
        )),
        "continuing" => out.push(head("⟲", accent, "the agent stopped before verifying — continuing autonomously".into())),
        "state_mismatch" => out.push(head(
            "⟲",
            accent,
            format!("{} didn't apply (file changed) — told the agent to re-read it", info.tool.as_deref().unwrap_or("the edit")),
        )),
        "auto_approved" => out.push(head(
            "✓",
            good,
            format!("{} auto-approved by selfHeal.autoApply (the verified fix / the failing command)", info.tool.as_deref().unwrap_or("call")),
        )),
        _ => {}
    }
    out
}

/// Phases that are worth a transcript block (others are progress bars).
pub fn is_shown(phase: &str) -> bool {
    !matches!(phase, "diagnosing" | "verifying")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn info(phase: &str) -> HealInfo {
        HealInfo {
            phase: phase.into(),
            attempt: Some(1),
            max: Some(3),
            command: Some("cargo test".into()),
            root_cause: Some("add() subtracts".into()),
            patch: Some("--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a - b\n+a + b\n".into()),
            verify: Some("cargo test".into()),
            output_tail: Some("running 1 test\nassertion failed: left == right\n".into()),
            ..Default::default()
        }
    }

    #[test]
    fn each_phase_reads_as_one_story() {
        let detected = plain(&lines(&info("detected"), 90, false));
        assert!(
            detected[0].contains("`cargo test` failed") && detected[0].contains("up to 3 attempts")
        );
        assert!(detected.iter().any(|l| l.contains("assertion failed")));
        let proposed = plain(&lines(&info("proposed"), 90, false));
        assert!(proposed[0].contains("attempt 1/3") && proposed[0].contains("add() subtracts"));
        assert!(proposed.iter().any(|l| l.contains("+a + b")));
        assert!(plain(&lines(&info("verified"), 120, false))[0].contains("sandbox copy"));
        assert!(plain(&lines(&info("healed"), 90, false))[0].contains("passes now"));
        assert!(plain(&lines(&info("escalated"), 90, false))[0].contains("over to you"));
        assert!(!is_shown("diagnosing") && is_shown("verified"));
    }

    #[test]
    fn heal_rows_fit_the_width() {
        for phase in [
            "detected",
            "proposed",
            "verified",
            "verify_failed",
            "healed",
            "escalated",
            "no_patch",
            "continuing",
        ] {
            for row in plain(&lines(&info(phase), 40, false)) {
                assert!(
                    super::super::text::display_width(&row) <= 40,
                    "{phase}: {row}"
                );
            }
        }
    }
}
