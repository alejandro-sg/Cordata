//! The team board: one row per agent, arranged as the delegation tree.
//!
//! A pod is drawn by [`crate::ui::canvas::AgentNode`] — the unit a new agent type plugs into. What makes one
//! agent type look different from another is data, not code: [`role_style`] is the single table naming
//! roles (a glyph, a hue, a caption), and a role it has never heard of draws as the generic row instead
//! of going unrendered.
//!
//! A row says four things and nothing else: **who** (name, role, a status light and word), **what it is
//! doing right now** (a sentence, from `harness_client::words`), **how its recent work went** (a strip of
//! ticks, one per call) and **how risky Laya has found its calls** (a small meter). Everything else —
//! model, counters, knobs — is one click away in the inspector.

use gpui::{App, ClickEvent, Window, div, prelude::*, px, relative};

use harness_proto::v1::{Pod, TraceRow};

use super::kit::mono;
use crate::theme::{self, Theme};

/// How a role is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleStyle {
    pub glyph: &'static str,
    pub color: u32,
    pub caption: &'static str,
}

/// The only place roles are named.
pub fn role_style(role: &str) -> RoleStyle {
    let (glyph, color, caption) = match role {
        "orchestrator" => ("◆", 0x22d3ee, "orchestrator"),
        "architect" => ("▲", 0xa78bfa, "architect"),
        "debugger" => ("●", 0xf87171, "debugger"),
        "claude_code" | "claude_code_engineer" => ("✳", 0xfbbf24, "claude code"),
        "observer" => ("◉", 0x38bdf8, "observer"),
        "digest" | "context_digest" => ("▤", 0x34d399, "context digest"),
        "laya" => ("◈", 0xfb923c, "laya"),
        "" => ("·", 0x8b949e, "agent"),
        _ => ("◇", 0x8b949e, "agent"),
    };
    RoleStyle { glyph, color, caption }
}

/// What a click on a row does; built by the window with `cx.listener`.
pub type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

// ------------------------------------------------------------------------------ the tree

/// One row of the flattened delegation tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// Index into the pod list.
    pub index: usize,
    /// For each level from the root's children down to this row: was the node at that level (this row,
    /// for the last entry) the last child of its parent? It decides which connector lines continue.
    pub trail: Vec<bool>,
}

const MAX_DEPTH: usize = 6;

/// Pods in tree order: each root, then its children, depth first. A pod whose parent is not in the
/// list is a root; a cycle or a runaway depth cannot hang the window or hide a pod.
pub fn flatten_tree(pods: &[Pod]) -> Vec<TreeRow> {
    let ids: Vec<&str> = pods.iter().map(|p| p.id.as_str()).collect();
    let is_root = |p: &Pod| p.parent.is_empty() || !ids.contains(&p.parent.as_str()) || p.parent == p.id;
    let mut out = Vec::with_capacity(pods.len());
    let mut seen = vec![false; pods.len()];

    fn walk(pods: &[Pod], at: usize, trail: Vec<bool>, out: &mut Vec<TreeRow>, seen: &mut [bool]) {
        if seen[at] {
            return;
        }
        seen[at] = true;
        out.push(TreeRow { index: at, trail: trail.clone() });
        if trail.len() >= MAX_DEPTH {
            return;
        }
        let kids: Vec<usize> = pods.iter().enumerate().filter(|(i, p)| p.parent == pods[at].id && *i != at && !seen[*i]).map(|(i, _)| i).collect();
        let last = kids.len().saturating_sub(1);
        for (n, kid) in kids.into_iter().enumerate() {
            let mut next = trail.clone();
            next.push(n == last);
            walk(pods, kid, next, out, seen);
        }
    }

    for (i, pod) in pods.iter().enumerate() {
        if is_root(pod) {
            walk(pods, i, Vec::new(), &mut out, &mut seen);
        }
    }
    // Anything a cycle or the depth cap left unvisited still appears, as a root.
    for i in 0..pods.len() {
        if !seen[i] {
            walk(pods, i, Vec::new(), &mut out, &mut seen);
        }
    }
    out
}

// ------------------------------------------------------------------------------ the row

/// One tick per recent call: green completed, red failed or stopped, cyan running, violet a directive.
pub(crate) fn ticks(trace: &[TraceRow], theme: &Theme) -> gpui::Div {
    let mut strip = div().flex().flex_row().items_end().gap(px(2.));
    for row in trace.iter().rev().take(14).rev() {
        let (color, height) = match (row.status.as_str(), row.tool.as_str()) {
            (_, "ISOLATE") | ("failed", _) => (theme.danger, 12.0),
            ("running", _) => (theme.accent, 12.0),
            (_, "GUIDANCE") => (theme.violet, 9.0),
            _ => (Theme::tint(theme.ok, 0.75), 9.0),
        };
        strip = strip.child(div().flex_none().w(px(4.)).h(px(height)).rounded(px(1.)).bg(color));
    }
    strip
}

/// Laya's last reading of this agent's calls, as a small meter and a number.
pub(crate) fn risk_meter(last: f64, theme: &Theme) -> gpui::Div {
    let color = theme::threat_color(Some(last), theme);
    div()
        .debug_selector(|| "risk-meter".to_owned())
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(div().flex_none().w(px(34.)).h(px(4.)).rounded_full().bg(theme.card).child(div().h_full().w(relative(last.clamp(0.0, 1.0) as f32)).rounded_full().bg(color)))
        .child(mono(format!("{last:.2}"), color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(id: &str, parent: &str) -> Pod {
        Pod { id: id.into(), name: id.into(), parent: parent.into(), ..Default::default() }
    }

    #[test]
    fn every_known_role_has_its_own_glyph_and_an_unknown_one_still_draws() {
        let known = ["orchestrator", "architect", "debugger", "claude_code", "observer", "digest"];
        let mut glyphs: Vec<&str> = known.iter().map(|r| role_style(r).glyph).collect();
        let count = glyphs.len();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), count, "two roles share a glyph: {glyphs:?}");
        assert_eq!(role_style("").glyph, "·");
        assert_eq!(role_style("a_role_from_the_future").glyph, "◇");
        assert_eq!(role_style("claude_code_engineer"), role_style("claude_code"), "the tool's pod and the role share a look");
    }

    #[test]
    fn the_team_is_a_tree_in_delegation_order() {
        let pods = vec![pod("orchestrator", ""), pod("architect", "orchestrator"), pod("debugger", "orchestrator"), pod("claude", "orchestrator"), pod("sub", "claude")];
        let rows = flatten_tree(&pods);
        let order: Vec<&str> = rows.iter().map(|r| pods[r.index].id.as_str()).collect();
        assert_eq!(order, ["orchestrator", "architect", "debugger", "claude", "sub"], "a child follows its parent, depth first");
        assert_eq!(rows[0].trail, Vec::<bool>::new(), "the root has no connector");
        assert_eq!(rows[1].trail, [false], "architect has siblings after it");
        assert_eq!(rows[3].trail, [true], "claude is the last child");
        assert_eq!(rows[4].trail, [true, true], "sub is the last child of the last child");
    }

    #[test]
    fn a_missing_parent_a_cycle_and_a_runaway_chain_cannot_hang_or_hide_a_pod() {
        let orphan = vec![pod("a", "ghost"), pod("b", "a")];
        assert_eq!(flatten_tree(&orphan).len(), 2, "a pod whose parent is unknown is a root");
        let cycle = vec![pod("a", "b"), pod("b", "a")];
        assert_eq!(flatten_tree(&cycle).len(), 2, "a cycle still shows both pods exactly once");
        let chain: Vec<Pod> = (0..40).map(|i| pod(&format!("p{i}"), &if i == 0 { String::new() } else { format!("p{}", i - 1) })).collect();
        let rows = flatten_tree(&chain);
        assert_eq!(rows.len(), 40, "nothing is dropped from a very deep chain");
        assert!(rows.iter().all(|r| r.trail.len() <= MAX_DEPTH));
        assert!(flatten_tree(&[]).is_empty());
    }
}
