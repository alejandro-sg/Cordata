//! The field: the deck's third view. Every open thread is a node standing in its checkout; zooming adds detail
//! instead of opening a window.
//!
//! - Under 60%: mark and name. Who is working, who is stuck.
//! - To 140%: what each is doing now, the components it stands on, and its gate if it is waiting on you.
//! - Past 140%: its last actions too.
//!
//! Geometry is `cordata_view::field` (a node keeps its place whatever it is doing). Everything shown is read from
//! tool calls, approvals and edited files. Scroll pans, ⌘-scroll zooms, dragging the ground pans.

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{Message, TabState};
use cordata_view::desk::Touch;
use cordata_view::field::{self, Altitude};
use cordata_view::flight::{self, Zone};
use cordata_view::runs::{self, Run};
use cordata_view::shown;
use cordata_view::work::TriageIn;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, canvas, div, px,
};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Workspace;
use super::sheet::DeskMode;
use crate::theme::{c, ca, line, mono, tk};
use crate::ui::{self, Kind, MarkState};

/// One wheel notch of zoom, and the step of the − / + buttons.
const WHEEL_ZOOM: f32 = 0.004;
const BUTTON_ZOOM: f32 = 1.25;
/// World offset of everything on the field, so nothing sits against the window's edge.
const PAD_X: f32 = 28.;
const PAD_Y: f32 = 16.;
/// The attention rail's width, and the window width under which it shows only when it has something to say.
const RAIL_W: f32 = 284.;
const RAIL_FROM: f32 = 1000.;

fn touch_word(touch: Touch) -> &'static str {
    match touch {
        Touch::Waiting => "waiting",
        Touch::Failed => "failed",
        Touch::Working => "now",
        Touch::Edited => "edited",
        Touch::Read => "read",
    }
}

impl Workspace {
    /// Zoom from the buttons: about the middle of the field as last drawn.
    pub(super) fn field_zoom_step(&mut self, factor: f32, cx: &mut Context<Self>) {
        let size = self.field_bounds.get().size;
        self.field_cam.zoom_at(factor, f32::from(size.width) / 2.0, f32::from(size.height) / 2.0);
        cx.notify();
    }

    pub(super) fn field_reset(&mut self, cx: &mut Context<Self>) {
        self.field_cam = field::Camera::default();
        cx.notify();
    }

    /// Show the whole field: every thread and run in view at once.
    pub(super) fn field_fit(&mut self, cx: &mut Context<Self>) {
        let inputs = self.lane_inputs(cx);
        let laid = field::layout(&inputs, &[]).with_runs(&self.field_runs);
        let size = self.field_bounds.get().size;
        // Chips and gates hang under nodes and the tape covers the bottom edge: leave them room.
        self.field_cam = field::Camera::fit(laid.w + 2.0 * PAD_X, laid.h + 2.0 * PAD_Y + 120.0, f32::from(size.width), f32::from(size.height) - 40.0);
        cx.notify();
    }

    /// The − 100% + control for the deck's header: zoom without a wheel.
    pub(super) fn field_zoom_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        div().flex().items_center().gap(px(4.))
            .child(ui::button("field-zoom-out", "−", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| this.field_zoom_step(1.0 / BUTTON_ZOOM, cx))))
            .child(div().id("field-zoom-reset").role(gpui::Role::Button).cursor_pointer().w(px(44.)).text_center().font_family(mono(cx)).text_size(px(t::TEXT_META))
                .text_color(c(tk.text_muted)).hover(|s| s.text_color(c(tk.text_primary)))
                .tooltip(ui::tip("Back to the starting view"))
                .on_click(cx.listener(|this, _, _, cx| this.field_reset(cx)))
                .child(SharedString::from(format!("{:.0}%", self.field_cam.zoom * 100.0))))
            .child(ui::button("field-zoom-in", "+", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| this.field_zoom_step(BUTTON_ZOOM, cx))))
            .child(ui::button("field-fit", "Fit", Kind::Ghost, cx).tooltip(ui::tip("Show every thread and run at once")).on_click(cx.listener(|this, _, _, cx| this.field_fit(cx))))
            .into_any_element()
    }

    pub(super) fn field_view(&self, inputs: &[TriageIn], zones: &[Zone], cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let cam = self.field_cam;
        let z = cam.zoom;
        let alt = field::altitude(z);
        let sel = self.rack_selected_slot;
        // In the Canvas / Diagram mode, all open agent threads are living nodes in the network constellation.
        let laid = field::layout(inputs, zones).with_runs(&self.field_runs);
        let runs_els = self.run_elements(&laid, alt, cx);
        let wide = laid.w * z + cam.x > f32::from(self.field_bounds.get().size.width);
        let tape = if self.selected_step_tape.is_some() { self.step_tape_panel(cx) } else { self.run_tape(cx) };
        // World length to screen length; text follows the zoom but never drops under a readable size.
        let s = |v: f32| px(v * z);
        let text = |base: f32| px((base * z).clamp(11., base * 1.5));

        let (pad_x, pad_y) = (PAD_X, PAD_Y);

        // Blueprint constellation grid dots
        let mut grid_dots: Vec<AnyElement> = Vec::new();
        for gx in (0..=10).map(|i| i as f32 * 160.0) {
            for gy in (0..=8).map(|i| i as f32 * 140.0) {
                let (sx, sy) = cam.to_screen(gx, gy);
                grid_dots.push(
                    div().absolute().left(px(sx)).top(px(sy)).size(px(2.5)).rounded_full().bg(ca(tk.amber, 0.12)).into_any_element()
                );
            }
        }

        let territories = laid.territories.iter().enumerate().map(|(ti, tr)| {
            let (x, y) = cam.to_screen(tr.x + pad_x, tr.y + pad_y);
            div().id(("field-place", ti)).absolute().left(px(x)).top(px(y)).w(s(tr.w)).h(s(tr.h))
                .rounded(px(6.))
                .border_1()
                .border_color(ca(tk.amber, 0.25))
                .bg(ca(tk.bg_panel, 0.55))
                // Territory header & coordinate marker
                .child(
                    // Zoomed out, the name keeps its readable size, so it moves up to stay clear of the first node.
                    div().absolute().top(px((field::HEAD * z - 15.).min(8. * z).max(1.))).left(s(12.)).right(s(12.)).flex().items_center().justify_between().gap(px(6.))
                        .child(div().truncate().font_family(mono(cx)).text_size(text(t::TEXT_META)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(tr.place.to_string())))
                        .child(div().flex_shrink_0().font_family(mono(cx)).text_size(text(t::TEXT_META)).text_color(c(tk.text_muted)).child("folder"))
                )
                // Guide rope running down the gutter
                .child(
                    div().absolute().left(s(field::GUTTER / 2. + field::PAD)).top(s(36.)).bottom(s(14.)).w(px(2.0)).bg(ca(tk.amber, 0.35))
                )
                .into_any_element()
        });

        let links = laid.links.iter().enumerate().map(|(li, l)| {
            let (x, y0) = cam.to_screen(l.x + pad_x, l.y0 + pad_y);
            let (_, y1) = cam.to_screen(l.x + pad_x, l.y1 + pad_y);
            let len = (y1 - y0).abs().max(1.0);
            div().id(("field-link", li)).absolute().left(px(x - 1.0)).top(px(y0)).w(px(2.5)).h(px(len)).bg(c(tk.attention))
                .tooltip(ui::tip(format!("#{} and #{} both write {}", l.a, l.b, l.zone)))
                // Two still beads on the line: where the two threads meet
                .child(
                    div().absolute().left(px(-2.5)).top(px(len * 0.35)).size(px(7.)).rounded_full().bg(c(tk.amber))
                )
                .child(
                    div().absolute().left(px(-2.5)).top(px(len * 0.7)).size(px(5.)).rounded_full().bg(c(tk.attention))
                )
                // Center shared zone badge
                .child(
                    div().absolute().left(px(8.)).top(px(len / 2.0 - 10.)).px(px(6.)).py(px(2.)).rounded(px(2.))
                        .bg(ca(tk.bg_panel, 0.95)).border_1().border_color(c(tk.attention))
                        .font_family(mono(cx)).text_size(px(t::TEXT_MIN)).text_color(c(tk.attention))
                        .child(SharedString::from(format!("shared: {}", l.zone)))
                )
                .into_any_element()
        });

        let nodes = laid.nodes.iter().flat_map(|n| {
            let Some(item) = inputs.get(n.index) else { return Vec::new() };
            let slot = n.slot;
            let needs_you = item.gate_title.is_some();
            let working = item.state == TabState::Working;
            let mark = match item.state {
                _ if needs_you => MarkState::NeedsYou,
                TabState::Working => MarkState::Working,
                TabState::Done => MarkState::Done,
                TabState::Closed => MarkState::Error,
                _ => MarkState::Idle,
            };
            let (edge, card_bg) = if needs_you {
                (c(tk.attention), ca(tk.attention, 0.08))
            } else if working {
                (c(tk.amber), ca(tk.amber, 0.08))
            } else if sel == Some(slot) {
                (c(tk.amber), ca(tk.bg_panel, 0.98))
            } else {
                (line(cx, t::LINE_DEFAULT), ca(tk.bg_panel, 0.94))
            };
            let (x, y) = cam.to_screen(n.x + pad_x, n.y + pad_y);

            // Knot tying node left edge to territory guide rope
            let (kx, ky) = cam.to_screen(n.x + pad_x - 10.0, n.y + pad_y + field::NODE_H / 2.0);
            let knot = div().id(("field-knot", slot)).absolute().left(px(kx)).top(px(ky - 1.0)).w(px(10.0 * z)).h(px(2.0)).bg(ca(tk.amber, 0.45))
                .child(div().absolute().left(px(-3.0)).top(px(-3.0)).size(px(7.)).rounded_full().bg(c(tk.amber)).border_1().border_color(c(tk.bg_panel)));

            let beat = self.beat_of(item.index, cx);
            let status_badge = if needs_you {
                ("needs you", c(tk.attention))
            } else if working {
                ("working", c(tk.amber))
            } else if item.state == TabState::Done {
                ("done", c(tk.teal))
            } else {
                ("idle", c(tk.text_muted))
            };
            let card = div().id(("field-node", slot)).role(gpui::Role::Button).cursor_pointer().absolute().left(px(x)).top(px(y)).w(s(field::NODE_W)).h(s(field::NODE_H))
                .px(s(12.)).py(s(10.)).rounded(s(6.)).border_1().border_color(edge).bg(card_bg).overflow_hidden()
                .flex().items_start().gap(s(10.))
                .tooltip(ui::tip(format!("#{slot} {} · {}", item.title, item.now)))
                .on_click(cx.listener(move |this, _, _, cx| this.field_click(slot, cx)))
                .child(div().flex_shrink_0().self_start().pt(s(3.)).child(ui::mark_live(("field-mark", slot), mark, 12., beat, cx)))
                .child(
                    div().flex_1().min_w_0().flex().flex_col().gap(s(3.))
                        // The title wraps to two lines; it is never cut to make room for a status.
                        .child(div().line_clamp(2).text_ellipsis().font_family(crate::theme::voice(cx)).text_size(text(14.)).line_height(gpui::relative(1.25)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                        .child(div().flex().items_baseline().gap(s(6.)).font_family(mono(cx)).text_size(text(t::TEXT_META))
                            .child(div().flex_shrink_0().text_color(c(tk.text_muted)).child(SharedString::from(format!("#{slot}"))))
                            .child(div().flex_shrink_0().text_color(status_badge.1).child(status_badge.0)))
                        // What it is doing now; left out when that only repeats the status word above it.
                        .when(alt != Altitude::Fleet && !item.now.is_empty() && item.now != status_badge.0, |d| {
                            d.child(
                                div().line_clamp(2).text_ellipsis().font_family(mono(cx)).text_size(text(t::TEXT_META))
                                    .text_color(if needs_you { c(tk.attention) } else if working { c(tk.amber) } else { c(tk.text_muted) })
                                    .child(SharedString::from(item.now.clone()))
                            )
                        })
                );
            if alt == Altitude::Fleet {
                return vec![knot.into_any_element(), card.into_any_element()];
            }

            let chips = n.chips.iter().map(|chip| {
                div().flex().items_center().justify_between().gap(s(8.)).px(s(8.)).h(s(22.)).rounded(px(2.)).border_1()
                    .border_color(if chip.contested { c(tk.attention) } else { line(cx, t::LINE_SUBTLE) })
                    .bg(if chip.contested { ca(tk.attention, 0.08) } else { line(cx, t::FILL_FAINT) })
                    .font_family(mono(cx)).text_size(text(t::TEXT_TAG))
                    .child(div().truncate().text_color(c(tk.text_secondary)).child(SharedString::from(chip.name.clone())))
                    .child(div().flex_shrink_0().text_color(if chip.contested { c(tk.attention) } else { c(tk.text_muted) })
                        .child(if chip.contested { "contested" } else { touch_word(chip.touch) }))
            });
            let (sig_allow, sig_deny) = (item.gate_signature.clone().unwrap_or_default(), item.gate_signature.clone().unwrap_or_default());
            let gate = item.gate_title.clone().map(|title| {
                div().flex().flex_col().gap(s(6.)).p(s(10.)).rounded(px(4.)).border_1().border_color(c(tk.attention))
                    .bg(ca(tk.attention, 0.08))
                    .child(
                        div().flex().items_center().gap(s(6.))
                            .child(div().font_family(mono(cx)).text_size(text(10.)).text_color(c(tk.attention)).child("needs your decision"))
                    )
                    .child(div().truncate().font_family(mono(cx)).text_size(text(t::TEXT_META)).text_color(c(tk.text_primary)).child(SharedString::from(shown(&title))))
                    .child(div().flex().items_center().gap(px(6.))
                        .when(!item.gate_high_risk, |r| r.child(ui::button(("field-allow", slot), "Allow ↵", Kind::Primary, cx)
                            .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.answer_batch_gate(&sig_allow, ApprovalDecision::Allow, cx) }))))
                        .child(ui::button(("field-deny", slot), "Deny esc", Kind::Ghost, cx)
                            .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.answer_batch_gate(&sig_deny, ApprovalDecision::Reject, cx) })))
                        .when(item.gate_high_risk, |r| r.child(ui::meta("high risk: open thread to allow", cx))))
            });
            let actions: Vec<(&'static str, String)> = if alt == Altitude::Agent {
                self.sessions.get(item.index).map(|e| {
                    e.read(cx).view.messages.iter().rev().filter_map(|m| match m {
                        Message::Tool(card) => Some((flight::beat_of(card).label(), flight::target_of(card).map(|p| shown(&p)).unwrap_or_else(|| card.name.clone()))),
                        _ => None,
                    }).take(4).collect()
                }).unwrap_or_default()
            } else {
                Vec::new()
            };
            let (ux, uy) = cam.to_screen(n.x + pad_x, n.y + pad_y + field::NODE_H + 8.0);
            let under = div().id(("field-under", slot)).absolute().left(px(ux)).top(px(uy)).w(s(field::NODE_W)).flex().flex_col().gap(s(4.))
                .when_some(gate, |d, g| d.child(g))
                .children(chips)
                .when(n.more > 0, |d| d.child(div().font_family(mono(cx)).text_size(text(t::TEXT_TAG)).text_color(c(tk.text_faint)).child(SharedString::from(format!("+{} more", n.more)))))
                .children(actions.into_iter().map(|(beat, target)| div().flex().items_center().gap(s(8.)).font_family(mono(cx)).text_size(text(t::TEXT_TAG))
                    .child(div().flex_shrink_0().w(s(36.)).text_color(c(tk.text_muted)).child(beat))
                    .child(div().flex_1().min_w_0().truncate().text_color(c(tk.text_secondary)).child(SharedString::from(target)))));
            vec![knot.into_any_element(), card.into_any_element(), under.into_any_element()]
        });

        let bounds = self.field_bounds.clone();
        let children: Vec<AnyElement> = grid_dots
            .into_iter()
            .chain(territories)
            .chain(links)
            .chain(nodes)
            .chain(runs_els)
            .collect();
        let empty = self.field_runs.is_empty() && inputs.is_empty();
        div().id("field").relative().size_full().overflow_hidden()
            .child(canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full())
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, window, cx| {
                let d = ev.delta.pixel_delta(window.line_height());
                if ev.modifiers.platform || ev.modifiers.control {
                    let o = this.field_bounds.get().origin;
                    let factor = (1.0 + f32::from(d.y) * WHEEL_ZOOM).clamp(0.8, 1.25);
                    this.field_cam.zoom_at(factor, f32::from(ev.position.x - o.x), f32::from(ev.position.y - o.y));
                } else {
                    this.field_cam.pan(f32::from(d.x), f32::from(d.y));
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &gpui::MouseDownEvent, _, _| this.field_drag = Some(ev.position)))
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _, cx| {
                let Some(last) = this.field_drag else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.field_drag = None;
                    return;
                }
                let next_x = (this.field_cam.x + f32::from(ev.position.x - last.x)).clamp(-2500.0, 300.0);
                let next_y = (this.field_cam.y + f32::from(ev.position.y - last.y)).clamp(-2500.0, 300.0);
                this.field_cam.x = next_x;
                this.field_cam.y = next_y;
                this.field_drag = Some(ev.position);
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.field_drag = None))
            .children(children)
            .child(div().absolute().right(px(16.)).bottom(px(56.)).child(self.field_zoom_control(cx)))
            .when_some(tape, |d, t| d.child(t))
            .when(empty, |d| d.child(div().p(px(24.)).text_color(c(tk.text_muted)).text_size(px(t::TEXT_SMALL)).child("Nothing needs you and nothing is moving.")))
            // Something stands past the right edge: say so, and offer the way to see it.
            .when(wide && !empty, |d| d.child(div().id("field-more").role(gpui::Role::Button).cursor_pointer().absolute().right(px(16.)).bottom(px(92.)).px(px(8.)).py(px(3.)).rounded(px(t::RADIUS_CONTROL))
                .border_1().border_color(line(cx, t::LINE_DEFAULT)).bg(c(tk.bg_panel)).font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary))
                .hover(|s| s.text_color(c(tk.text_primary)))
                .on_click(cx.listener(|this, _, _, cx| this.field_fit(cx))).child("more to the right · show all")))
            .into_any_element()
    }
}

fn cordata_home() -> PathBuf {
    std::env::var_os("CORDATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cordata"))
}

fn ago(ts: f64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let secs = (now - ts).max(0.0) as u64;
    if secs < 90 { format!("{secs}s ago") } else if secs < 5400 { format!("{}m ago", secs / 60) } else { format!("{}h ago", secs / 3600) }
}

/// This project's newest runs and their handoffs, from `cordata-run`; an empty list with a reason when it cannot.
fn read_runs(cwd: &Path) -> (Vec<Run>, Option<String>) {
    let Some(bin) = super::agents::cordata_run() else { return (Vec::new(), Some("runs need cordata-run: make tui-build".into())) };
    let call = |args: &[&str]| -> Result<serde_json::Value, String> {
        let out = std::process::Command::new(&bin).args(args).current_dir(cwd).stdin(std::process::Stdio::null()).output().map_err(|e| format!("could not run cordata-run: {e}"))?;
        if !out.status.success() {
            return Err(format!("cordata-run {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("could not read cordata-run {}: {e}", args[0]))
    };
    let project = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let status = match call(&["status", "--json"]) {
        Ok(v) => v,
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut found = Vec::new();
    for id in runs::pick(&status, &project) {
        match call(&["show", &id, "--json"]) {
            Ok(show) => found.extend(runs::parse(&show, &status)),
            Err(e) => return (found, Some(e)),
        }
    }
    (found, None)
}

impl Workspace {
    /// Read the runs again, off the UI thread. One read at a time.
    pub(super) fn refresh_runs(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = cx.try_global::<crate::session::Runtime>().map(|r| r.0.clone()) else { return };
        if self.field_runs_busy {
            return;
        }
        self.field_runs_busy = true;
        let cwd = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let (found, note) = runtime.spawn_blocking(move || read_runs(&cwd)).await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.field_runs_busy = false;
                // A read that failed saw nothing; only a good one may count as "nothing changed" or "this changed".
                if note.is_none() {
                    this.field_pulse.observe(&found, Instant::now());
                }
                if this.field_runs != found || this.field_runs_note != note {
                    // Runs showing up where there were none, in a view you have not moved: bring them into it.
                    let arrived = this.field_runs.is_empty() && !found.is_empty() && this.field_cam == field::Camera::default();
                    this.field_runs = found;
                    if arrived && f32::from(this.field_bounds.get().size.width) > 0.0 {
                        this.field_fit(cx);
                    }
                    // A failed read keeps what the last good one said; its reason shows instead of the hold note.
                    this.field_runs_note = note.or_else(|| this.field_runs_note.take().filter(|n| n.starts_with("hold") || n.starts_with("resum")));
                    cx.notify();
                }
                if this.selected_step_tape.is_some() {
                    this.refresh_step_tape(cx);
                }
            });
        })
        .detach();
    }

    /// While the field is open, look at the runs every few seconds. Started once; stops with the window.
    pub(super) fn ensure_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.field_polling {
            return;
        }
        self.field_polling = true;
        self.refresh_runs(cx);
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let alive = this.update(cx, |this, cx| {
                if this.desk_mode == DeskMode::Rack && this.rack_view_mode == flight::RackViewMode::Field {
                    this.refresh_runs(cx);
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
    }

    /// Ask a run to pause at its next safe point. Nothing stops mid-call.
    pub(super) fn hold_run(&mut self, run_id: &str, cx: &mut Context<Self>) {
        self.field_runs_note = Some(match runs::request_hold(&cordata_home(), run_id, "window") {
            Ok(()) => format!("hold sent: {run_id} stops at its next safe point"),
            Err(e) => format!("could not send the hold: {e}"),
        });
        cx.notify();
    }

    /// Start `cordata-run resume`, optionally on one worker family and with the person's words for the next step.
    pub(super) fn resume_run(&mut self, run_id: &str, family: Option<&'static str>, direction: Option<String>, cx: &mut Context<Self>) {
        self.resume_run_with(run_id, family, direction, None, cx);
    }

    /// [`Self::resume_run`], running `redo_step` again (a completed step too) with the direction on it.
    pub(super) fn resume_run_with(&mut self, run_id: &str, family: Option<&'static str>, direction: Option<String>, redo_step: Option<String>, cx: &mut Context<Self>) {
        let Some(bin) = super::agents::cordata_run() else {
            self.field_runs_note = Some("resuming needs cordata-run: make tui-build".into());
            cx.notify();
            return;
        };
        let mut cmd = std::process::Command::new(bin);
        cmd.args(["resume", run_id]);
        if let Some(f) = family {
            cmd.args(["--workers", f]);
        }
        if let Some(text) = direction.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            cmd.args(["--direction", text]);
        }
        if let Some(step) = redo_step.as_deref() {
            cmd.args(["--redo-step", step]);
        }
        cmd.current_dir(&self.cwd).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        self.field_runs_note = Some(match cmd.spawn() {
            Ok(_) => format!("resuming {run_id}{}{}", family.map(|f| format!(" on {f}")).unwrap_or_default(), if direction.is_some() { " with your direction" } else { "" }),
            Err(e) => format!("could not resume: {e}"),
        });
        cx.notify();
    }

    /// Select or deselect a step tape for viewing.
    pub(super) fn select_step_tape(&mut self, run_id: &str, step_id: &str, cx: &mut Context<Self>) {
        if self.selected_step_tape.as_ref().map(|(r, s)| (r.as_str(), s.as_str())) == Some((run_id, step_id)) {
            self.selected_step_tape = None;
            self.step_tape_lines.clear();
        } else {
            self.selected_step_tape = Some((run_id.to_string(), step_id.to_string()));
            self.refresh_step_tape(cx);
        }
        cx.notify();
    }

    /// Reload the newest lines for the currently selected step tape.
    pub(super) fn refresh_step_tape(&mut self, cx: &mut Context<Self>) {
        let Some((run_id, step_id)) = self.selected_step_tape.as_ref() else { return };
        let steps_dir = cordata_home().join("orchestrator").join(run_id).join("steps");
        let prefix = format!("{step_id}-");
        let mut tapes = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&steps_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && name.ends_with(".ndjson") {
                    tapes.push(entry.path());
                }
            }
        }
        tapes.sort();
        if let Some(latest) = tapes.last() {
            if let Ok(content) = std::fs::read_to_string(latest) {
                self.step_tape_lines = content
                    .lines()
                    .rev()
                    .take(200)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(String::from)
                    .collect();
            } else {
                self.step_tape_lines = vec!["could not read step tape file".to_string()];
            }
        } else {
            self.step_tape_lines = vec!["no tape recorded yet for this step".to_string()];
        }
        cx.notify();
    }

    fn run_elements(&self, laid: &field::Field, alt: Altitude, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let cam = self.field_cam;
        let z = cam.zoom;
        let s = |v: f32| px(v * z);
        let text = |base: f32| px((base * z).clamp(11., base * 1.5));
        let bar_text = self.composer.read(cx).value().trim().to_string();
        let mut out: Vec<AnyElement> = Vec::new();

        let at = |wx: f32, wy: f32| cam.to_screen(wx + PAD_X, wy + PAD_Y);
        let (now, moving) = (Instant::now(), crate::motion::on(cx));

        for rt in &laid.runs {
            let Some(run) = self.field_runs.get(rt.run) else { continue };
            let rid = rt.run;
            let (x, y) = at(rt.x, rt.y);
            let mark = match run.state.as_str() {
                "done" => MarkState::Done,
                "failed" | "cancelled" | "budget_exhausted" => MarkState::Error,
                "paused" => MarkState::Idle,
                _ => MarkState::Working,
            };
            let state_word = match run.state.as_str() {
                "paused" if run.awaits_approval => "plan awaits you".to_owned(),
                "paused" => "held".to_owned(),
                "budget_exhausted" => "out of budget".to_owned(),
                "research" => "reading the project".to_owned(),
                "plan" => "planning".to_owned(),
                "execute" => "working".to_owned(),
                "harden" => "hardening".to_owned(),
                "audit" => "checking the result".to_owned(),
                other => other.to_owned(),
            };
            let done = run.steps.iter().filter(|s| s.status == "completed").count();
            let buttons = (alt != Altitude::Fleet).then(|| {
                let id = run.run_id.clone();
                let mut row = div().flex().flex_wrap().items_center().gap(px(6.));
                if run.can_hold() {
                    let id = id.clone();
                    row = row.child(ui::button(("run-hold", rid), "Hold at next safe point", Kind::Tint, cx).on_click(cx.listener(move |this, _, _, cx| this.hold_run(&id, cx))));
                }
                if run.can_resume() {
                    let id1 = id.clone();
                    row = row.child(ui::button(("run-resume", rid), if run.awaits_approval { "Approve and build" } else { "Resume" }, Kind::Primary, cx).on_click(cx.listener(move |this, _, _, cx| this.resume_run(&id1, None, None, cx))));
                    for (fi, fam) in runs::FAMILIES.iter().enumerate() {
                        let id2 = id.clone();
                        row = row.child(ui::button(("run-resume-on", rid * 8 + fi), SharedString::from(format!("on {fam}")), Kind::Ghost, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.resume_run(&id2, Some(fam), None, cx))));
                    }
                    if !bar_text.is_empty() {
                        let id3 = id.clone();
                        row = row.child(ui::button(("run-direct", rid), "Resume with the bar's text as direction", Kind::Tint, cx).on_click(cx.listener(move |this, _, window, cx| {
                            let words = this.composer.read(cx).value().to_string();
                            this.composer.update(cx, |st, cx| st.set_value("", window, cx));
                            this.resume_run(&id3, None, Some(words), cx);
                        })));
                    }
                }
                row
            });
            out.push(
                div().id(("field-run", rid)).absolute().left(px(x)).top(px(y)).w(s(rt.w)).h(s(rt.h)).rounded(px(6.)).border_1()
                    .border_color(if run.can_hold() { ca(tk.amber, 0.5) } else { line(cx, t::LINE_DEFAULT) }).bg(ca(tk.bg_panel, 0.55))
                    .child(div().absolute().left(s(12.)).top(s(8.)).w(s(rt.w - 24.)).flex().flex_col().gap(s(6.))
                        .child(div().flex().items_center().gap(px(8.))
                            .child(ui::mark(("run-mark", rid), mark, 12., cx))
                            .child(div().flex_1().min_w_0().truncate().font_family(mono(cx)).text_size(text(t::TEXT_META)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary))
                                .child(SharedString::from(format!("run · {state_word}"))))
                            .child(div().flex_shrink_0().font_family(mono(cx)).text_size(text(t::TEXT_META)).text_color(c(tk.text_muted))
                                .child(SharedString::from(format!("{done} of {} steps done", run.steps.len())))))
                        .child(div().id(("run-goal", rid)).line_clamp(3).text_ellipsis().text_size(text(14.)).line_height(gpui::relative(1.25)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                            .tooltip(ui::tip(format!("{} · {}", run.run_id, run.goal))).child(SharedString::from(run.goal.clone())))
                        .when_some(buttons, |d, b| d.child(b)))
                    .into_any_element(),
            );
        }

        // The cords between steps: which step waits on which. Drawn under the steps, so a cord that passes a step
        // it does not stop at goes behind it.
        let mut knots: Vec<AnyElement> = Vec::new();
        for e in &laid.edges {
            let Some(run) = self.field_runs.get(e.run) else { continue };
            let (Some(from), Some(to)) = (run.steps.get(e.from), run.steps.get(e.to)) else { continue };
            // Work has passed along this cord once the step above finished and this one started.
            let lit = from.status == "completed" && matches!(to.status.as_str(), "running" | "completed");
            let (ink, thick) = if lit { (ca(tk.amber, 0.75), 2.0) } else { (ca(tk.text_muted, 0.55), 1.0) };
            // Zoomed out, handoffs are not drawn, so the cord leaves from the step itself.
            let leave = if alt == Altitude::Fleet { laid.steps.iter().find(|n| n.run == e.run && n.step == e.from).map_or(e.y0, |n| n.y + field::STEP_H) } else { e.y0 };
            let ((x0, y0), (x1, y1), (_, ym)) = (at(e.x0, leave), at(e.x1, e.y1), at(e.x0, e.mid));
            let (la, lb, lc) = ((ym - y0).max(0.0), (x1 - x0).abs(), (y1 - ym).max(0.0));
            let total = la + lb + lc;
            let id = e.run * 1_000_000 + e.from * 1000 + e.to;
            let down = div().absolute().left(px(x0 - thick / 2.0)).top(px(y0)).w(px(thick)).bg(ink);
            let across = div().absolute().top(px(ym - thick / 2.0)).h(px(thick)).bg(ink);
            let drop = div().absolute().left(px(x1 - thick / 2.0)).top(px(ym)).w(px(thick)).bg(ink);
            // It draws itself only when a poll has just seen the step start; any other time it is simply there.
            let started = (moving && lit && to.status == "running").then(|| self.field_pulse.started(&run.run_id, &to.id, now)).flatten();
            match started {
                Some(seq) => {
                    let rightward = x1 >= x0;
                    out.push(crate::motion::flow(down, ("edge-a", id), seq, move |el, p| el.h(px(la * crate::motion::piece(p, 0.0, la, total)))));
                    out.push(crate::motion::flow(across, ("edge-b", id), seq, move |el, p| {
                        let w = lb * crate::motion::piece(p, la, lb, total);
                        el.w(px(w)).left(px(if rightward { x0 } else { x0 - w }))
                    }));
                    out.push(crate::motion::flow(drop, ("edge-c", id), seq, move |el, p| el.h(px(lc * crate::motion::piece(p, la + lb, lc, total)))));
                }
                None => {
                    out.push(down.h(px(la)).into_any_element());
                    if lb > 0.0 {
                        out.push(across.left(px(x0.min(x1))).w(px(lb)).into_any_element());
                    }
                    out.push(drop.h(px(lc)).into_any_element());
                }
            }
            // The knot where the cord ties on to the step.
            knots.push(div().absolute().left(px(x1 - 3.0)).top(px(y1 - 3.0)).size(px(6.)).rounded_full().bg(if lit { c(tk.amber) } else { c(tk.text_muted) }).into_any_element());
        }

        for sn in &laid.steps {
            let Some(run) = self.field_runs.get(sn.run) else { continue };
            let Some(step) = run.steps.get(sn.step) else { continue };
            let key = sn.run * 1000 + sn.step;
            let running = step.status == "running";
            // A running step's node lights each time a poll sees it change, and rests between: the same beat a
            // thread has, at the pace the run can be read.
            let mark = match step.status.as_str() {
                "running" => ui::mark_live(("step-mark", key), MarkState::Working, 12., self.field_pulse.count(&run.run_id, &step.id), cx),
                "completed" => ui::mark(("step-mark", key), MarkState::Done, 12., cx),
                "failed" => ui::mark(("step-mark", key), MarkState::Error, 12., cx),
                "blocked" => ui::mark(("step-mark", key), MarkState::NeedsYou, 12., cx),
                _ => ui::mark(("step-mark", key), MarkState::Idle, 12., cx),
            };
            let (mut edge, ground) = match step.status.as_str() {
                "running" => (c(tk.amber), ca(tk.amber, 0.08)),
                "failed" => (c(tk.danger), c(tk.bg_panel)),
                "blocked" => (c(tk.attention), ca(tk.attention, 0.08)),
                _ => (line(cx, t::LINE_DEFAULT), c(tk.bg_panel)),
            };
            let is_selected_tape = self.selected_step_tape.as_ref().map(|(r, s)| (r.as_str(), s.as_str())) == Some((&run.run_id, &step.id));
            if is_selected_tape {
                edge = c(tk.amber);
            }
            let said = runs::step_line(run, step);
            let (x, y) = at(sn.x, sn.y);
            let rid = run.run_id.clone();
            let sid = step.id.clone();
            out.push(
                div().id(("field-step", key)).cursor_pointer().absolute().left(px(x)).top(px(y)).w(s(field::NODE_W)).h(s(field::STEP_H))
                    .px(s(12.)).rounded(s(6.)).border_1().border_color(edge).bg(c(tk.bg_main)).overflow_hidden()
                    .child(div().absolute().size_full().top_0().left_0().bg(ground))
                    .flex().items_center().gap(s(10.))
                    .tooltip(ui::tip(format!("{} {} · {said}{}{}", step.id, step.name, if step.scope.is_empty() { String::new() } else { format!(" · may edit {}", step.scope.join(", ")) },
                        if step.behavior.is_empty() { String::new() } else { format!(" · expects: {}", step.behavior) })))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_step_tape(&rid, &sid, cx);
                    }))
                    .child(div().relative().flex_shrink_0().child(mark))
                    .child(div().relative().flex_1().min_w_0().flex().flex_col().gap(s(3.))
                        .child(div().line_clamp(2).text_ellipsis().text_size(text(14.)).line_height(gpui::relative(1.25)).font_weight(FontWeight::MEDIUM)
                            .text_color(if step.status == "pending" { c(tk.text_secondary) } else { c(tk.text_primary) }).child(SharedString::from(format!("{} {}", step.id, step.name))))
                        .when(alt != Altitude::Fleet, |d| d.child(div().truncate().font_family(mono(cx)).text_size(text(t::TEXT_META))
                            .text_color(if running { c(tk.amber) } else if step.status == "failed" { c(tk.danger) } else if step.status == "blocked" { c(tk.attention) } else { c(tk.text_muted) }).child(SharedString::from(said)))))
                    .into_any_element(),
            );
            if alt == Altitude::Fleet {
                continue;
            }
            for (hi, h_ix) in sn.handoffs.iter().enumerate() {
                let Some(h) = run.handoffs.get(*h_ix) else { continue };
                let (hx, hy) = at(sn.x, sn.y + field::STEP_H + 6.0 + hi as f32 * field::HANDOFF_H);
                let you = h.reason == "user_pause";
                out.push(
                    div().id(("field-handoff", sn.run * 10_000 + sn.step * 100 + hi)).absolute().left(px(hx)).top(px(hy)).w(s(field::NODE_W)).h(s(field::HANDOFF_H - 4.))
                        .px(s(8.)).rounded(px(2.)).border_1().border_color(if you { c(tk.attention) } else { line(cx, t::LINE_SUBTLE) }).bg(c(tk.bg_main)).flex().items_center()
                        .font_family(mono(cx)).text_size(text(t::TEXT_TAG)).text_color(if you { c(tk.attention) } else { c(tk.text_secondary) })
                        .tooltip(ui::tip(format!("{} → {} · {} · step {}", h.from, h.to.clone().unwrap_or_else(|| "next worker".into()), runs::reason_words(&h.reason), h.step_status)))
                        .child(div().truncate().child(SharedString::from(format!("{} → {} · {}", h.from, h.to.clone().unwrap_or_else(|| "…".into()), runs::reason_words(&h.reason)))))
                        .into_any_element(),
                );
            }
        }
        out.extend(knots);
        out
    }

    /// The newest run's transitions along the bottom, as they happened. Read-only: it records, it does not rewind.
    fn run_tape(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = tk(cx);
        let run = self.field_runs.first()?;
        let stops: Vec<(f64, String)> = run.history.iter().rev().take(12).rev().cloned().collect();
        Some(
            div().id("field-tape").absolute().left(px(0.)).right(px(0.)).bottom(px(0.)).px(px(24.)).py(px(8.)).border_t_1().border_color(line(cx, t::LINE_SUBTLE)).bg(c(tk.bg_panel))
                .overflow_x_scroll().flex().items_center().gap(px(10.)).font_family(mono(cx)).text_size(px(t::TEXT_META))
                .child(div().id("tape-run").flex_shrink_0().text_color(c(tk.text_muted)).tooltip(ui::tip(format!("{} · every stage this run has been through", run.run_id))).child("newest run"))
                .children(stops.into_iter().enumerate().map(|(i, (ts, state))| {
                    let last = i + 1 == run.history.len().min(12);
                    div().flex_shrink_0().flex().items_center().gap(px(10.))
                        .child(div().w(px(18.)).h(px(1.)).bg(line(cx, t::LINE_SUBTLE)))
                        .child(div().id(("tape-stop", i)).text_color(if last { c(tk.text_primary) } else { c(tk.text_secondary) }).tooltip(ui::tip(ago(ts))).child(SharedString::from(state)))
                }))
                .when_some(self.field_runs_note.clone(), |d, n| d.child(div().flex_shrink_0().ml_auto().pl(px(16.)).text_color(c(tk.text_muted)).child(SharedString::from(n))))
                .into_any_element(),
        )
    }

    /// The step tape viewer along the bottom, displaying live/durable per-step events.
    fn step_tape_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (run_id, step_id) = self.selected_step_tape.as_ref()?;
        let tk = tk(cx);
        let rid = run_id.clone();
        let sid = step_id.clone();
        let title = format!("step tape · {rid} · {sid}");
        let count = format!("{} events", self.step_tape_lines.len());

        Some(
            div().id("field-step-tape").absolute().left(px(0.)).right(px(0.)).bottom(px(0.)).h(px(240.))
                .border_t_1().border_color(ca(tk.amber, 0.4)).bg(ca(tk.bg_panel, 0.98))
                .flex().flex_col()
                .child(
                    div().flex().items_center().justify_between().px(px(16.)).py(px(6.))
                        .border_b_1().border_color(line(cx, t::LINE_SUBTLE))
                        .child(
                            div().flex().items_center().gap(px(10.))
                                .child(div().font_family(mono(cx)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.amber)).child(title))
                                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(count))
                        )
                        .child(
                            div().flex().items_center().gap(px(6.))
                                .child(ui::button("tape-refresh", "Refresh", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| this.refresh_step_tape(cx))))
                                .child(ui::button("tape-close", "Close ✕", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| {
                                    this.selected_step_tape = None;
                                    this.step_tape_lines.clear();
                                    cx.notify();
                                })))
                        )
                )
                .child(
                    div().id("tape-body").flex_1().min_h_0().overflow_y_scroll().p(px(12.)).font_family(mono(cx)).text_size(px(11.))
                        .flex().flex_col().gap(px(2.))
                        .children(self.step_tape_lines.iter().enumerate().map(|(i, line)| {
                            let text = line.clone();
                            div().id(("tape-line", i)).truncate().text_color(c(tk.text_secondary))
                                .hover(|s| s.text_color(c(tk.text_primary)))
                                .child(SharedString::from(text))
                        }))
                )
                .into_any_element(),
        )
    }
}

impl Workspace {
    /// First click picks a thread (and shows it); clicking the picked one opens its transcript.
    fn field_click(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.rack_selected_slot == Some(slot) && self.shown_slot() == slot {
            self.desk_mode = DeskMode::Transcript;
            cx.notify();
        } else {
            self.pick_lane(slot, cx);
        }
    }

    /// The field as the whole window: a goal bar on top, the field, an attention rail on the right, the tape below.
    /// No thread list and no transcript; those are one key away (`⌘1`).
    pub(super) fn field_window(&self, composer: AnyElement, menu: Option<AnyElement>, window_w: f32, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let inputs = self.lane_inputs(cx);
        let per_thread: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, e)| {
                let s = e.read(cx);
                (ix + 1, s.cwd.clone(), cordata_view::desk::places(&s.view.messages, &s.cwd, s.view.pending.front().map(|p| p.info.tool_call_id.as_str())))
            })
            .collect();
        let zones = flight::territory(&per_thread);
        let working = inputs.iter().filter(|i| i.state == TabState::Working).count();
        let waiting = inputs.iter().filter(|i| i.gate_title.is_some()).count();
        let blocked_steps: Vec<(&Run, &cordata_view::runs::Step)> = self.field_runs.iter()
            .flat_map(|r| r.steps.iter().filter(|s| s.status == "blocked").map(move |s| (r, s)))
            .collect();
        let blocked_count = blocked_steps.len();
        let quiet = inputs.len().saturating_sub(working + waiting);
        let live_runs = self.field_runs.iter().filter(|r| !r.finished()).count();

        let count = |n: usize, word: &'static str, col: gpui::Hsla| div().flex().items_baseline().gap(px(5.))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(if n > 0 { col } else { c(tk.text_faint) }).child(SharedString::from(n.to_string())))
            .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(word));
        let top = div().flex().flex_wrap().items_center().justify_between().gap_x(px(20.)).gap_y(px(6.)).pl(px(84.)).pr(px(20.)).py(px(8.)).border_b_1().border_color(line(cx, t::LINE_DEFAULT))
            .child(
                div().flex().items_center().gap(px(10.))
                    .child(ui::cordata_logo(18., cx))
                    .child(div().flex().flex_col().gap(px(1.))
                        .child(div().flex().items_center().gap(px(6.))
                            .child(div().font_family(crate::theme::voice(cx)).text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child(SharedString::from(self.project_name())))
                            .child(div().font_family(mono(cx)).text_size(px(11.)).px(px(6.)).py(px(1.5)).rounded(px(2.)).bg(ca(tk.amber, 0.15)).text_color(c(tk.amber)).child(SharedString::from(self.git.branch.clone().unwrap_or_else(|| "main".into())))))
                        .child(ui::meta(SharedString::from(format!("{} thread{}", inputs.len(), if inputs.len() == 1 { "" } else { "s" })), cx)))
            )
            .child(div().flex_1().min_w(px(380.)).flex().justify_center()
                .child(div().w_full().min_w(px(380.)).max_w(px(640.)).flex().flex_col().children(menu).child(composer)))
            .child(div().flex().items_center().gap(px(16.))
                .child(count(working + live_runs, "working", c(tk.amber)))
                .child(count(quiet, "quiet", c(tk.text_secondary)))
                .child(count(waiting + blocked_count, "need you", c(tk.attention)))
                .child(div().flex().items_center().gap(px(2.))
                    .child(ui::button("fw-lanes", "Lanes", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| { this.rack_view_mode = flight::RackViewMode::Lanes; cx.notify(); })))
                    .child(ui::button("fw-ground", "Ground", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| { this.rack_view_mode = flight::RackViewMode::Ground; cx.notify(); })))
                    .child(ui::button("fw-threads", "Transcript ⌘1", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| { this.desk_mode = DeskMode::Transcript; cx.notify(); })))));

        let head = |text: &'static str, col: gpui::Hsla| div().font_family(mono(cx)).text_size(px(t::TEXT_META)).font_weight(FontWeight::MEDIUM).text_color(col).child(text);
        let mut rail = div().id("field-rail").w(px(RAIL_W)).flex_none().h_full().overflow_y_scroll().border_l_1().border_color(line(cx, t::LINE_DEFAULT)).p(px(16.)).flex().flex_col().gap(px(12.));

        let waiting_items: Vec<_> = inputs.iter().filter(|i| i.gate_title.is_some()).collect();
        if !waiting_items.is_empty() || !blocked_steps.is_empty() {
            rail = rail.child(head("Needs you", c(tk.attention)));
            for item in waiting_items {
                let slot = item.slot;
                let sig = item.gate_signature.clone().unwrap_or_default();
                let sig2 = sig.clone();
                rail = rail.child(div().flex().flex_col().gap(px(8.)).p(px(12.)).rounded(px(4.)).border_1().border_color(c(tk.attention))
                    .bg(ca(tk.attention, 0.08))
                    .child(div().id(("rail-open", slot)).cursor_pointer().truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                        .on_click(cx.listener(move |this, _, _, cx| this.pick_lane(slot, cx))).child(SharedString::from(format!("#{slot} {}", item.title))))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.attention)).child(SharedString::from(shown(item.gate_title.as_deref().unwrap_or("")))))
                    .child(div().flex().gap(px(6.))
                        .when(!item.gate_high_risk, |r| r.child(ui::button(("rail-allow", slot), "Allow ↵", Kind::Primary, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig, ApprovalDecision::Allow, cx)))))
                        .child(ui::button(("rail-deny", slot), "Deny", Kind::Ghost, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.answer_batch_gate(&sig2, ApprovalDecision::Reject, cx)))))
                    .when(item.gate_high_risk, |d| d.child(ui::meta("high risk: open the thread to allow", cx))));
            }
            for (run, step) in blocked_steps {
                let run_id = run.run_id.clone();
                let step_id = step.id.clone();
                let step_name = step.name.clone();
                let reason = step.blocked_reason.clone().unwrap_or_else(|| "action needs permission".to_string());
                let rid_allow = run_id.clone();
                let sid_allow = step_id.clone();
                let rid_deny = run_id.clone();
                let sid_deny = step_id.clone();
                rail = rail.child(
                    div().flex().flex_col().gap(px(8.)).p(px(12.)).rounded(px(4.)).border_1().border_color(c(tk.attention))
                        .bg(ca(tk.attention, 0.08))
                        .child(div().font_family(mono(cx)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.attention))
                            .child(SharedString::from(format!("run · {run_id}"))))
                        .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary))
                            .child(SharedString::from(format!("{step_id} {step_name}"))))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_secondary))
                            .child(SharedString::from(reason)))
                        .child(div().flex().gap(px(6.))
                            .child(ui::button(SharedString::from(format!("rail-allow-{run_id}-{step_id}")), "Allow ↵", Kind::Primary, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = runs::request_permission(&cordata_home(), &rid_allow, &sid_allow, true);
                                    this.refresh_runs(cx);
                                })))
                            .child(ui::button(SharedString::from(format!("rail-deny-{run_id}-{step_id}")), "Deny", Kind::Ghost, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = runs::request_permission(&cordata_home(), &rid_deny, &sid_deny, false);
                                    this.refresh_runs(cx);
                                }))))
                );
            }
        } else if let Some(slot) = self.rack_selected_slot {
            if let Some(item) = inputs.iter().find(|i| i.slot == slot) {
                rail = rail.child(head("Selected thread", c(tk.text_muted)));
                rail = rail.child(
                    div().flex().flex_col().gap(px(8.)).p(px(12.)).rounded(px(4.))
                        .border_1().border_color(line(cx, t::LINE_DEFAULT))
                        .bg(ca(tk.bg_panel, 0.95))
                        .child(
                            div().flex().items_center().justify_between()
                                .child(div().font_family(mono(cx)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_secondary)).child(format!("#{slot}")))
                                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_MIN)).text_color(if item.state == TabState::Working { c(tk.amber) } else { c(tk.text_muted) })
                                    .child(match item.state { TabState::Working => "working", TabState::Done => "done", TabState::Closed => "stopped", _ => "idle" }))
                        )
                        .child(div().font_family(crate::theme::voice(cx)).text_size(px(14.)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(SharedString::from(item.title.clone())))
                        .when(!item.now.is_empty() && item.now != "idle", |d| d.child(div().font_family(mono(cx)).text_size(px(11.)).text_color(c(tk.text_muted)).child(SharedString::from(item.now.clone()))))
                        .child(div().font_family(mono(cx)).text_size(px(t::TEXT_MIN)).text_color(c(tk.text_faint)).child(format!("{} files · +{} −{}", item.files, item.added, item.removed)))
                        .child(
                            div().flex().items_center().gap(px(6.)).pt(px(4.))
                                .child(ui::button(("rail-open-transcript", slot), "Open thread ⌘1", Kind::Primary, cx).on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_lane(slot, cx);
                                    this.desk_mode = DeskMode::Transcript;
                                    cx.notify();
                                })))
                        )
                );
            }
        } else {
            rail = rail
                // Said plainly, from the same count the header shows: no status the app has not measured.
                .child(div().text_size(px(t::TEXT_UI)).text_color(c(tk.text_muted)).child("Nothing is waiting on you."))
                .child(
                    div().flex().flex_col().items_start().gap(px(4.)).ml(px(-10.))
                        .child(ui::button("rail-diffs", "Review what changed  ⌘D", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| this.toggle_change_sheet(cx))))
                        .child(ui::button("rail-recenter", "Show every thread and run", Kind::Ghost, cx).on_click(cx.listener(|this, _, _, cx| this.field_fit(cx))))
                );
        }

        // In a narrow window the rail would take the field's room to say nothing: it shows only when it has something.
        let mut rail_has = waiting > 0 || blocked_count > 0 || self.rack_selected_slot.is_some_and(|slot| inputs.iter().any(|i| i.slot == slot));
        let ready: Vec<&TriageIn> = inputs.iter().filter(|i| flight::ready(i)).collect();
        rail_has |= !ready.is_empty() || zones.iter().any(|z| z.contested);
        if !ready.is_empty() {
            rail = rail.child(div().pt(px(8.)).child(head("Ready to review", c(tk.text_muted))));
            for item in ready {
                let slot = item.slot;
                let check = match &item.check {
                    Some((_, cordata_view::desk::CheckState::Passed)) => "check passed",
                    Some((_, cordata_view::desk::CheckState::Running)) => "check running",
                    _ => "no check run",
                };
                rail = rail.child(div().id(("rail-ready", slot)).cursor_pointer().flex().flex_col().gap(px(2.)).text_size(px(13.))
                    .on_click(cx.listener(move |this, _, _, cx| { this.pick_lane(slot, cx); this.load_review(cx); this.desk_mode = DeskMode::ChangeSheet; cx.notify(); }))
                    .child(div().truncate().text_color(c(tk.text_primary)).child(SharedString::from(format!("#{slot} {}", item.title))))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_muted)).child(SharedString::from(format!("{} files  +{} −{} · {check} · open changes", item.files, item.added, item.removed)))));
            }
        }

        let contested: Vec<&Zone> = zones.iter().filter(|z| z.contested).collect();
        if !contested.is_empty() {
            rail = rail.child(div().pt(px(8.)).child(head("Two threads, one place", c(tk.attention))));
            for z in contested {
                let who = z.occupants.iter().filter(|o| matches!(o.touch, Touch::Edited | Touch::Working | Touch::Waiting)).map(|o| format!("#{}", o.slot)).collect::<Vec<_>>().join(" and ");
                rail = rail.child(div().flex().flex_col().gap(px(2.)).text_size(px(13.))
                    .child(div().text_color(c(tk.text_primary)).child(SharedString::from(z.name.clone())))
                    .child(div().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.attention)).child(SharedString::from(format!("{who} both write here")))));
            }
        }

        div().flex().flex_col().flex_1().min_h_0().min_w_0()
            .child(top)
            .child(div().flex().flex_1().min_h_0()
                .child(div().flex_1().min_w_0().h_full().child(self.field_view(&inputs, &zones, cx)))
                .when(window_w >= RAIL_FROM || (rail_has && window_w >= RAIL_W * 2.5), |d| d.child(rail)))
            .into_any_element()
    }
}
