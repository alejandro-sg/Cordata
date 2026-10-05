//! The spatial canvas: the team as containment pods on a 2D surface, joined by conduits.
//!
//! The board used to be a vertical tree of rows. It is now a canvas, because what a person needs to
//! see is *shape*: which agent is working, which is waiting on them, and what is flowing between
//! them. Painted back to front:
//!
//! 1. **Conduits** — a stroked bezier per delegation/dispatch link (the daemon's own `LabState.links`
//!    when it sent any, else one edge per parent), with a packet running along an active one.
//! 2. **The pods** — one card per agent, placed by [`layout_pods`], its border glowing by state
//!    ([`crate::theme::glow`]).
//! 3. **Memory orbitals** — a packet flying into a pod when it reads a store (`model.memory_hits`).
//!
//! The layout is a pure function of the pods: no physics, no drag, no size to be told, so it is
//! deterministic and unit-tested. It *derives* the surface it needs — wide enough that siblings
//! cannot overlap, tall enough for the whole chain — and the canvas scrolls when that is bigger than
//! the window. That is why a card never overlaps another and never sits half off the edge, however
//! deep or wide the team gets. Selection and the keys are unchanged: a card is still `pod-{id}` and
//! still sets the same `selected`.

use std::time::Duration;

use gpui::{Animation, AnimationExt, Bounds, IntoElement, PathBuilder, Pixels, Point, RenderOnce, Rgba, SharedString, Size, Window, canvas, div, point, prelude::*, px, pulsating_between, size};
use harness_client::MemoryHitNotice;
use harness_proto::v1::{Link, Pod};

use super::kit::{self, chip, led, mono};
use super::pods::{ClickHandler, flatten_tree, risk_meter, role_style, ticks};
use crate::app::LabApp;
use crate::theme::{self, Theme};

/// The card every pod is drawn as. Fixed, so the layout can be computed before anything is drawn.
pub const CARD: Size<Pixels> = Size { width: px(216.), height: px(134.) };
/// The breathing rate of a pod that is thinking or waiting — slow enough not to nag.
const PULSE: Duration = Duration::from_millis(1600);
/// How long a memory packet takes to fly in from outside the team into its pod.
pub const FLIGHT: Duration = Duration::from_millis(1400);
/// How long a conduit's packet takes to run from one pod to the next.
const FLOW: Duration = Duration::from_millis(1200);
/// The canvas is never narrower than this, so a one-agent team is not a sliver.
const MIN_WIDTH: f32 = 720.0;
const MARGIN: f32 = 28.0;
const COL_GAP: f32 = 26.0;
const ROW_GAP: f32 = 44.0;

/// The daemon's wall clock and ours are the same machine's.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ------------------------------------------------------------------------ layout

/// Where one pod is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePlacement {
    /// Index into the pod list.
    pub index: usize,
    /// Depth in the delegation tree: 0 for a root (the orchestrator), 1 for its children, …
    pub depth: usize,
    /// The card's top-left corner, inside the surface.
    pub top_left: Point<Pixels>,
}

/// The surface the team needs, and where each pod goes on it.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasLayout {
    pub size: Size<Pixels>,
    pub nodes: Vec<NodePlacement>,
}

/// Place every pod: roots on the top row, each tier below, siblings spread evenly across the row.
///
/// The surface is sized to the tree: the widest tier decides the width, the deepest chain the
/// height, so rows are always `ROW_GAP` apart and columns always `COL_GAP` apart — a card cannot
/// overlap another. Deterministic: the same pods always give the same canvas. Fed
/// `flatten_tree`, so a missing parent, a cycle and a runaway chain all behave.
pub fn layout_pods(pods: &[Pod], card: Size<Pixels>) -> CanvasLayout {
    let rows = flatten_tree(pods);
    let max_depth = rows.iter().map(|r| r.trail.len()).max().unwrap_or(0);
    let (card_w, card_h) = (f32::from(card.width), f32::from(card.height));
    let widest = (0..=max_depth).map(|d| rows.iter().filter(|r| r.trail.len() == d).count()).max().unwrap_or(0) as f32;

    // `count + 1` gaps is what an even spread over the width needs for `count` cards plus a gap on
    // each side — anything less and the outermost two would touch.
    let width = (2.0 * MARGIN + (widest + 1.0) * (card_w + COL_GAP)).max(MIN_WIDTH);
    let height = 2.0 * MARGIN + card_h + max_depth as f32 * (card_h + ROW_GAP);
    let centre = |depth: usize, n: usize, count: usize| {
        let x = width * (n as f32 + 1.0) / (count as f32 + 1.0);
        point(px(x - card_w / 2.0), px(MARGIN + depth as f32 * (card_h + ROW_GAP)))
    };

    let mut nodes = Vec::with_capacity(rows.len());
    for depth in 0..=max_depth {
        let tier: Vec<&super::pods::TreeRow> = rows.iter().filter(|r| r.trail.len() == depth).collect();
        for (n, row) in tier.iter().enumerate() {
            nodes.push(NodePlacement { index: row.index, depth, top_left: centre(depth, n, tier.len()) });
        }
    }
    CanvasLayout { size: size(px(width), px(height)), nodes }
}

/// The centre of a card, which is where a conduit and a memory packet aim.
pub fn card_centre(placement: &NodePlacement, card: Size<Pixels>) -> Point<Pixels> {
    point(placement.top_left.x + card.width / 2.0, placement.top_left.y + card.height / 2.0)
}

// ------------------------------------------------------------------------ conduits

/// The control points of the S-curve between two pods: out of the bottom of one, into the top of
/// the next. Shared by the painted line and the travelling packet so they cannot disagree.
pub fn conduit_controls(from: Point<Pixels>, to: Point<Pixels>) -> (Point<Pixels>, Point<Pixels>) {
    let mid_y = from.y + (to.y - from.y) / 2.0;
    (point(from.x, mid_y), point(to.x, mid_y))
}

/// The point on the conduit at `t` (0 at `from`, 1 at `to`) — a cubic bezier, evaluated directly.
pub fn point_on_conduit(from: Point<Pixels>, to: Point<Pixels>, t: f32) -> Point<Pixels> {
    let (c1, c2) = conduit_controls(from, to);
    let t = t.clamp(0.0, 1.0);
    let u = 1.0 - t;
    let (u2, t2) = (u * u, t * t);
    let at = |a: Pixels, b: Pixels, c: Pixels, d: Pixels| u2 * u * a + 3.0 * u2 * t * b + 3.0 * u * t2 * c + t2 * t * d;
    point(at(from.x, c1.x, c2.x, to.x), at(from.y, c1.y, c2.y, to.y))
}

/// The hue of a link, by what kind of handoff it is. A quiet link is the same hue, dimmed.
pub fn link_color(kind: &str, active: bool, theme: &Theme) -> Rgba {
    let hue = match kind {
        "dispatch" => theme.violet,
        "team" | "pipeline" => theme.warn,
        _ => theme.accent,
    };
    if active { hue } else { Theme::tint(hue, 0.34) }
}

/// One conduit, resolved to two on-canvas points.
#[derive(Debug, Clone, PartialEq)]
pub struct Conduit {
    pub from: String,
    pub to: String,
    pub a: Point<Pixels>,
    pub b: Point<Pixels>,
    pub color: Rgba,
    pub active: bool,
}

/// The conduits to draw: the daemon's own links when it sent any that land on two pods, else one
/// edge per delegation from the pods' own `parent` fields — so the canvas is never blank just
/// because a child predates links.
pub fn conduits(pods: &[Pod], links: &[Link]) -> Vec<(String, String, String, bool)> {
    let known: Vec<&str> = pods.iter().map(|p| p.id.as_str()).collect();
    let real: Vec<(String, String, String, bool)> =
        links.iter().filter(|l| known.contains(&l.from.as_str()) && known.contains(&l.to.as_str())).map(|l| (l.from.clone(), l.to.clone(), l.kind.clone(), l.active)).collect();
    if !real.is_empty() {
        return real;
    }
    pods.iter()
        .filter(|p| !p.parent.is_empty() && p.parent != p.id && known.contains(&p.parent.as_str()))
        .map(|p| (p.parent.clone(), p.id.clone(), "delegation".to_owned(), false))
        .collect()
}

/// Resolve the conduits onto the canvas: one line per link that has both ends placed.
pub fn resolved_conduits(pods: &[Pod], placements: &[NodePlacement], links: &[Link], theme: &Theme) -> Vec<Conduit> {
    let centre = |id: &str| placements.iter().find(|p| pods.get(p.index).is_some_and(|pod| pod.id == id)).map(|p| card_centre(p, CARD));
    conduits(pods, links)
        .into_iter()
        .filter_map(|(from, to, kind, active)| {
            let (a, b) = (centre(&from)?, centre(&to)?);
            Some(Conduit { from, to, a, b, color: link_color(&kind, active, theme), active })
        })
        .collect()
}

/// The conduit layer: the lines painted under the pods, and a packet running along each active one.
/// The packet is an animated element because a canvas paint closure gets no per-frame delta.
pub fn conduit_layer(lines: Vec<Conduit>) -> impl IntoElement {
    let painted: Vec<(Point<Pixels>, Point<Pixels>, Rgba, f32)> = lines.iter().map(|l| (l.a, l.b, l.color, if l.active { 1.8 } else { 1.0 })).collect();
    div()
        .debug_selector(|| "lab-conduits".to_owned())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            canvas(
                |_, _, _| (),
                move |_: Bounds<Pixels>, _: (), window: &mut Window, _: &mut gpui::App| {
                    for (a, b, color, width) in &painted {
                        let (c1, c2) = conduit_controls(*a, *b);
                        let mut path = PathBuilder::stroke(px(*width));
                        path.move_to(*a);
                        path.cubic_bezier_to(*b, c1, c2);
                        if let Ok(path) = path.build() {
                            window.paint_path(path, *color);
                        }
                    }
                },
            )
            .size_full(),
        )
        .children(lines.into_iter().filter(|l| l.active).enumerate().map(|(i, line)| {
            let (a, b) = (line.a, line.b);
            div().absolute().top_0().left_0().size(px(0.)).child(
                div().absolute().size(px(7.)).rounded_full().bg(line.color).with_animation(
                    SharedString::from(format!("flow-{}-{}-{i}", line.from, line.to)),
                    Animation::new(FLOW).repeat(),
                    move |el, delta| {
                        let at = point_on_conduit(a, b, delta);
                        el.left(at.x - px(3.5)).top(at.y - px(3.5))
                    },
                ),
            )
        }))
}

// ------------------------------------------------------------------------ orbitals

/// Where a memory packet is at `t`: in from the top-right corner of the canvas — reads come from
/// the stores, outside the team — and into the pod that made the read.
pub fn memory_packet_position(surface: Size<Pixels>, node: Point<Pixels>, t: f32) -> Point<Pixels> {
    let origin = point(surface.width - px(10.), px(10.));
    let t = t.clamp(0.0, 1.0);
    point(origin.x + (node.x - origin.x) * t, origin.y + (node.y - origin.y) * t)
}

/// The colour of a store read, by which store was read.
pub fn source_color(source: &str, theme: &Theme) -> Rgba {
    match source {
        "qdrant" => theme.violet,
        "kg" => theme.ok,
        "lancedb" => theme.accent,
        _ => theme.dim,
    }
}

/// One memory packet, animated into its pod. Rendered only while the read is fresh; the animation
/// is one-shot, so it flies once and fades rather than looping.
pub fn memory_packet(notice: &MemoryHitNotice, node: Point<Pixels>, surface: Size<Pixels>, theme: Theme) -> impl IntoElement {
    let color = source_color(&notice.hit.source, &theme);
    let id = SharedString::from(format!("mem-{}-{}", notice.hit.pod, notice.hit.ts_ms));
    div().absolute().top_0().left_0().size(px(0.)).child(
        div().absolute().size(px(10.)).rounded_full().border_2().border_color(color).bg(Theme::tint(color, 0.35)).with_animation(id, Animation::new(FLIGHT), move |el, delta| {
            let at = memory_packet_position(surface, node, delta);
            el.left(at.x - px(5.)).top(at.y - px(5.)).opacity(1.0 - delta * delta)
        }),
    )
}

// ------------------------------------------------------------------------ the pod

#[derive(IntoElement)]
pub struct AgentNode {
    pub pod: Pod,
    pub placement: NodePlacement,
    pub selected: bool,
    /// The agent the session is currently driving.
    pub focused: bool,
    pub now_ms: i64,
    pub theme: Theme,
    pub on_click: ClickHandler,
}

impl RenderOnce for AgentNode {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let Self { pod, placement, selected, focused, now_ms, theme, on_click } = self;
        let role = role_style(&pod.role);
        // The border is the state, as the directive asks: a pod that is thinking or waiting breathes,
        // one that is stopped or idle is still. Selection overrides it, so a click is always visible.
        let (glow, pulse) = theme::glow(&pod, &theme);
        let (word, _tone) = harness_client::words::status_word(&pod);
        let (line, line_tone) = harness_client::words::now_line(&pod);
        let border = if selected { Theme::tint(theme.accent, 0.9) } else { glow };
        let selector = format!("pod-{}", pod.id);
        let pulse_id = SharedString::from(format!("glow-{}", pod.id));

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(led(SharedString::from(format!("led-{}", pod.id)), glow, pulse))
            .child(div().text_color(theme::hex(role.color)).child(role.glyph))
            .child(div().min_w_0().truncate().font_weight(gpui::FontWeight::SEMIBOLD).text_sm().child(pod.name.clone()))
            .child(chip(word, glow));
        if focused {
            header = header.child(kit::caption("driving", &theme));
        }
        header = header.child(div().flex_1());
        if let Some(for_how_long) = harness_client::words::in_state_for(&pod, now_ms) {
            header = header.child(mono(for_how_long, theme.faint));
        }

        let mut meta = div().flex().flex_row().items_center().gap_2().min_w_0().child(ticks(&pod.trace, &theme));
        if pod.guidance_pending > 0 {
            meta = meta.child(chip(format!("{} directive{}", pod.guidance_pending, kit::plural(pod.guidance_pending as usize)), theme.violet));
        }
        meta = meta.child(div().flex_1().min_w_0());
        if let Some(last) = pod.risk_last {
            meta = meta.child(risk_meter(last, &theme));
        }
        meta = meta.child(div().max_w(px(120.)).truncate().text_size(px(10.)).text_color(theme.faint).child(pod.model.clone()));

        let card = div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .absolute()
            .left(placement.top_left.x)
            .top(placement.top_left.y)
            .w(CARD.width)
            .h(CARD.height)
            .flex()
            .flex_col()
            .gap(px(5.))
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(if selected { theme.card } else { theme.panel })
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .hover(move |this| this.border_color(theme.line_strong).bg(theme.card))
            .on_click(on_click)
            .child(header)
            .child(div().text_sm().text_color(kit::tone_color(line_tone, &theme)).line_clamp(2).child(line))
            .child(div().flex_1())
            .child(meta);

        // A pod that is actually running something carries a strip and how long it has been at it.
        // There is no fraction to show — telemetry reports elapsed time, not a total — so the strip
        // is a pulse rather than a progress bar that would be inventing a denominator.
        let working = theme::working(&pod);
        let card = if working && pod.busy_ms > 0 {
            card.child(
                div()
                    .debug_selector({
                        let id = pod.id.clone();
                        move || format!("busy-{id}")
                    })
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().h(px(3.)).rounded_full().bg(Theme::tint(glow, 0.35)).with_animation(SharedString::from(format!("busy-{}", pod.id)), Animation::new(PULSE).repeat().with_easing(pulsating_between(0.35, 1.0)), move |el, d| el.opacity(d)))
                    .child(mono(theme::human_ms(pod.busy_ms), theme.faint)),
            )
        } else {
            card
        };

        // The breathing border: an overlay that pulses in the pod's own glow colour. It is drawn
        // inside the card and ignores the mouse, so it can never eat a click meant for the card.
        let halo = if pulse {
            let color = border;
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded_lg()
                .border_2()
                .border_color(color)
                .with_animation(pulse_id, Animation::new(PULSE).repeat().with_easing(pulsating_between(0.18, 0.9)), move |el, d| el.opacity(d))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        card.child(halo)
    }
}

// ------------------------------------------------------------------------ the board

impl LabApp {
    /// The team as a canvas: the conduits, the pods, the memory packets flying in.
    pub(super) fn canvas_board(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let now = now_ms();
        let selected = self.selected_pod().map(|p| p.id.clone());
        let shell = div().id("canvas").debug_selector(|| "lab-canvas".to_owned()).relative().flex_1().min_w_0().h_full().overflow_scroll();

        let lab = match self.model.lab() {
            Some(lab) if !lab.pods.is_empty() => lab,
            _ => return shell.child(self.empty_team()),
        };
        let pods = lab.pods.clone();
        let layout = layout_pods(&pods, CARD);
        let lines = resolved_conduits(&pods, &layout.nodes, &lab.links, &theme);
        let surface = layout.size;

        let mut layer = div().debug_selector(|| "lab-canvas-layer".to_owned()).relative().w(surface.width).h(surface.height).child(conduit_layer(lines));
        for placement in layout.nodes.clone() {
            let pod = pods[placement.index].clone();
            let id = pod.id.clone();
            layer = layer.child(AgentNode {
                selected: selected.as_deref() == Some(pod.id.as_str()),
                focused: lab.focus == pod.id,
                placement,
                now_ms: now,
                theme,
                on_click: Box::new(cx.listener(move |this, _e, _w, cx| {
                    this.selected = Some(id.clone());
                    cx.notify();
                })),
                pod,
            });
        }
        // Memory orbitals, above the pods: a read the client has just recorded, on its way in.
        for notice in self.model.memory_hits.iter() {
            if notice.at.elapsed() >= FLIGHT {
                continue;
            }
            let Some(placement) = layout.nodes.iter().find(|p| pods.get(p.index).is_some_and(|pod| pod.id == notice.hit.pod)) else { continue };
            layer = layer.child(memory_packet(notice, card_centre(placement, CARD), surface, theme));
        }
        shell.child(layer)
    }

    fn empty_team(&self) -> gpui::Div {
        let theme = self.theme;
        let online = self.model.connection.is_online();
        div()
            .debug_selector(|| "empty-team".to_owned())
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .m_4()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child(if online { "No agents yet" } else { "Waiting for the daemon" }))
            .child(kit::text(if online { "Agents appear here as soon as a session starts working. Start one in a terminal:" } else { "The window keeps looking for it and connects by itself. Start the daemon, or watch a scripted team:" }, theme.dim))
            .child(mono(if online { "make tui" } else { "make daemon" }, theme.accent))
            .child(kit::text("or watch a scripted team, no model or key needed:", theme.dim))
            .child(mono("make lab-demo", theme.accent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(id: &str, parent: &str) -> Pod {
        Pod { id: id.into(), name: id.into(), parent: parent.into(), ..Default::default() }
    }

    fn team() -> Vec<Pod> {
        vec![pod("orchestrator", ""), pod("architect", "orchestrator"), pod("debugger", "orchestrator"), pod("claude", "orchestrator")]
    }

    fn rect(placement: &NodePlacement) -> (f32, f32, f32, f32) {
        let (x, y) = (f32::from(placement.top_left.x), f32::from(placement.top_left.y));
        (x, y, x + f32::from(CARD.width), y + f32::from(CARD.height))
    }

    #[test]
    fn every_pod_is_placed_inside_the_surface_the_layout_asked_for() {
        let layout = layout_pods(&team(), CARD);
        assert_eq!(layout.nodes.len(), 4, "every pod is placed");
        for placement in &layout.nodes {
            let (x0, y0, x1, y1) = rect(placement);
            assert!(x0 >= 0.0 && y0 >= 0.0 && x1 <= f32::from(layout.size.width) + 0.5 && y1 <= f32::from(layout.size.height) + 0.5, "{placement:?} is off the surface");
        }
        let empty = layout_pods(&[], CARD);
        assert!(empty.nodes.is_empty(), "no pods, no placements");
        assert_eq!(f32::from(empty.size.width), MIN_WIDTH, "and it is still a usable canvas");
    }

    #[test]
    fn no_two_cards_overlap_however_many_siblings_there_are() {
        let many: Vec<Pod> = std::iter::once(pod("orchestrator", "")).chain((0..7).map(|i| pod(&format!("child{i}"), "orchestrator"))).collect();
        for pods in [team(), many] {
            let layout = layout_pods(&pods, CARD);
            for (i, a) in layout.nodes.iter().enumerate() {
                for b in layout.nodes.iter().skip(i + 1) {
                    let ((ax0, ay0, ax1, ay1), (bx0, by0, bx1, by1)) = (rect(a), rect(b));
                    let disjoint = ax1 <= bx0 || bx1 <= ax0 || ay1 <= by0 || by1 <= ay0;
                    assert!(disjoint, "{} overlaps {}", pods[a.index].name, pods[b.index].name);
                }
            }
        }
    }

    #[test]
    fn the_root_is_on_top_and_children_are_a_tier_below() {
        let pods = team();
        let layout = layout_pods(&pods, CARD);
        let by = |id: &str| layout.nodes.iter().find(|p| pods[p.index].id == id).unwrap();
        assert_eq!(by("orchestrator").depth, 0);
        assert_eq!(by("architect").depth, 1);
        assert!(by("orchestrator").top_left.y < by("architect").top_left.y, "the root sits above its children");
        assert_ne!(by("architect").top_left.x, by("debugger").top_left.x, "siblings are spread across the row");
        assert!(by("orchestrator").top_left.x + CARD.width / 2.0 == layout.size.width / 2.0, "the single root is centred");
    }

    #[test]
    fn a_missing_parent_a_cycle_and_a_deep_chain_still_place_every_pod() {
        let orphan = vec![pod("a", "ghost"), pod("b", "a")];
        assert_eq!(layout_pods(&orphan, CARD).nodes.len(), 2, "an unknown parent just means a root");
        let cycle = vec![pod("a", "b"), pod("b", "a")];
        let layout = layout_pods(&cycle, CARD);
        assert_eq!(layout.nodes.len(), 2, "a cycle shows both pods exactly once");
        let chain: Vec<Pod> = (0..40).map(|i| pod(&format!("p{i}"), &if i == 0 { String::new() } else { format!("p{}", i - 1) })).collect();
        let layout = layout_pods(&chain, CARD);
        assert_eq!(layout.nodes.len(), 40, "nothing is dropped");
        let deepest = layout.nodes.iter().max_by_key(|p| p.depth).unwrap();
        assert!(f32::from(deepest.top_left.y) + f32::from(CARD.height) <= f32::from(layout.size.height) + 0.5, "a deep chain makes the canvas taller rather than clipping");
        assert!(f32::from(layout.size.height) > f32::from(layout_pods(&team(), CARD).size.height), "…and the canvas really did grow for it");
    }

    #[test]
    fn a_conduit_starts_at_one_pod_and_ends_at_the_other() {
        let (a, b) = (point(px(100.), px(50.)), point(px(400.), px(300.)));
        assert_eq!(point_on_conduit(a, b, 0.0), a);
        assert_eq!(point_on_conduit(a, b, 1.0), b);
        let mid = point_on_conduit(a, b, 0.5);
        assert!(f32::from(mid.x) > 100.0 && f32::from(mid.x) < 400.0, "…and bends between them: {mid:?}");
        assert_eq!(point_on_conduit(a, b, 5.0), b, "an out-of-range t is clamped");
    }

    #[test]
    fn a_conduit_falls_back_to_the_parent_edges_when_the_daemon_sent_no_links() {
        let pods = vec![pod("orchestrator", ""), pod("architect", "orchestrator")];
        let fallback = [("orchestrator".to_owned(), "architect".to_owned(), "delegation".to_owned(), false)];
        assert_eq!(conduits(&pods, &[]), fallback);
        // A link into a pod that is not on the canvas is dropped rather than drawn into nowhere —
        // and because that leaves no usable link, the parent fallback takes over.
        let ghost = vec![Link { from: "architect".into(), to: "ghost".into(), ..Default::default() }];
        assert_eq!(conduits(&pods, &ghost), fallback);
        let links = vec![Link { from: "architect".into(), to: "orchestrator".into(), kind: "dispatch".into(), active: true, ..Default::default() }];
        assert_eq!(conduits(&pods, &links).len(), 1, "the daemon's own links win");
    }

    #[test]
    fn conduits_are_resolved_to_the_pods_that_are_actually_placed() {
        let pods = team();
        let layout = layout_pods(&pods, CARD);
        let theme = Theme::dark();
        let lines = resolved_conduits(&pods, &layout.nodes, &[], &theme);
        assert_eq!(lines.len(), 3, "one conduit per delegated child");
        let orchestrator = card_centre(layout.nodes.iter().find(|p| pods[p.index].id == "orchestrator").unwrap(), CARD);
        assert!(lines.iter().all(|l| l.a == orchestrator), "each runs out of the orchestrator");
        assert!(lines.iter().all(|l| !l.active), "and a delegation fallback is not reported as flowing");
        let link = vec![Link { from: "orchestrator".into(), to: "architect".into(), kind: "dispatch".into(), active: true, ..Default::default() }];
        let active = resolved_conduits(&pods, &layout.nodes, &link, &theme);
        assert_eq!(active.len(), 1);
        assert!(active[0].active, "an active link is what the canvas animates");
        assert_eq!(active[0].color, theme.violet, "a dispatch is violet");
        assert_eq!(link_color("delegation", true, &theme), theme.accent);
        assert_eq!(link_color("delegation", false, &theme).a, 0.34, "a quiet link is dimmed, not hidden");
    }

    #[test]
    fn a_memory_packet_travels_from_outside_the_team_into_its_pod() {
        let surface = size(px(900.), px(600.));
        let node = point(px(200.), px(300.));
        assert_eq!(memory_packet_position(surface, node, 1.0), node, "it lands on the pod");
        let start = memory_packet_position(surface, node, 0.0);
        assert!(f32::from(start.x) > 200.0 && f32::from(start.y) < 300.0, "…having come in from outside the team: {start:?}");
        assert_eq!(source_color("qdrant", &Theme::dark()), Theme::dark().violet);
        assert_ne!(source_color("kg", &Theme::dark()), source_color("lancedb", &Theme::dark()));
    }
}
