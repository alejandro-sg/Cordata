//! D1 gate spike (docs/app.md §3): does gpui-pre 0.3.9 + gpui-component 0.7.3 carry the Cordata workspace?
//!
//! One window: a 10,000-block markdown transcript (variable-height `list`, selectable across blocks), a code editor
//! on a generated 50,000-line Rust file (tree-sitter + diagnostics), a composer (`Input`, IME through
//! `EntityInputHandler`), and the `gpui-fps` HUD.
//!
//!     cargo run --release                 interactive
//!     cargo run --release -- --selftest   scroll the whole transcript, print frame-time stats, exit
//!
//! Throwaway: nothing here is the app's code.

use std::time::{Duration, Instant};

use gpui::*;
use gpui_component::{
    highlighter::{Diagnostic, DiagnosticSeverity},
    input::{Editor, EditorState, Input, InputState, Position},
    text::TextView,
    ActiveTheme as _, Root,
};

const BLOCKS: usize = 10_000;
const EDITOR_LINES: usize = 50_000;

fn block_source(ix: usize) -> String {
    match ix % 5 {
        0 => format!("**you** · turn {ix}\n\nRefactor `orchestrator/machine.py` so cancel is terminal, and keep the work."),
        1 => format!(
            "### Plan {ix}\n\n1. Read `machine.py` and the executor.\n2. Wrap `run()` in a cancel handler.\n3. Add a regression test.\n\n> The record used to stay at **execute** forever."
        ),
        2 => format!("```python\n# block {ix}\nasync def run(self) -> RunRecord:\n    try:\n        return await self._loop()\n    except asyncio.CancelledError:\n        self._transition(\"cancelled\")\n        raise\n```"),
        3 => format!("| pod | worker | tokens | state |\n|---|---|---|---|\n| {ix} | claude_code | 12,400 | done |\n| {} | ollama:kimi | 3,100 | running |", ix + 1),
        _ => format!("Tool `shell` ran `pytest -q` — **{} passed**, 0 failed, in 4.{}s. Long line to wrap: {}", 80 + ix % 20, ix % 10, "lorem ipsum dolor sit amet ".repeat(6)),
    }
}

fn editor_source() -> String {
    let mut out = String::with_capacity(EDITOR_LINES * 48);
    let mut line = 0;
    while line < EDITOR_LINES {
        out.push_str(&format!("/// Function number {line}.\npub fn f{line}(x: u64) -> u64 {{\n    let y = x.wrapping_mul({line});\n    y ^ (y >> 7)\n}}\n\n"));
        line += 6;
    }
    out
}

struct Workspace {
    transcript: ListState,
    sources: Vec<SharedString>,
    editor: Entity<EditorState>,
    composer: Entity<InputState>,
    selftest: Option<SelfTest>,
}

struct SelfTest {
    started: Instant,
    last: Instant,
    frames: Vec<Duration>,
    built_ms: u128,
}

impl Workspace {
    fn new(selftest: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let t0 = Instant::now();
        let sources: Vec<SharedString> = (0..BLOCKS).map(|ix| block_source(ix).into()).collect();
        let transcript = ListState::new(BLOCKS, ListAlignment::Bottom, px(800.));
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("rust")
                .line_number(true)
                .searchable(true)
                .default_value(editor_source())
        });
        // A language server is not installed on this machine (docs/app.md §15.6): diagnostics are fed the way an LSP
        // client would feed them, to prove the editor's side of the contract.
        editor.update(cx, |state, cx| {
            if let Some(set) = state.diagnostics_mut() {
                set.push(Diagnostic::new(Position::new(1, 7)..Position::new(1, 9), "spike: unused function `f0`").with_severity(DiagnosticSeverity::Warning));
                set.push(Diagnostic::new(Position::new(25_001, 4)..Position::new(25_001, 10), "spike: an error far down the file").with_severity(DiagnosticSeverity::Error));
            }
            cx.notify();
        });
        let composer = cx.new(|cx| InputState::new(window, cx).placeholder("Ask Cordata…  (/ commands · @ files · ! shell)"));
        let built_ms = t0.elapsed().as_millis();
        eprintln!("spike: built {BLOCKS} blocks + {EDITOR_LINES}-line editor in {built_ms} ms");
        let selftest = selftest.then(|| SelfTest { started: Instant::now(), last: Instant::now(), frames: Vec::new(), built_ms });
        if selftest.is_some() {
            // The frame loop stops while the window is occluded (macOS pauses the display link), so the time limit is
            // also enforced from a timer, not only from render.
            let limit = std::env::var("SPIKE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(90u64);
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_secs(limit + 15)).await;
                eprintln!("SELFTEST watchdog: no frames for a while (window occluded?) — quitting");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
            transcript.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
        }
        Self { transcript, sources, editor, composer, selftest }
    }

    fn step_selftest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(test) = self.selftest.as_mut() else { return };
        let now = Instant::now();
        test.frames.push(now - test.last);
        test.last = now;
        let top = self.transcript.logical_scroll_top();
        if test.frames.len() < 5 {
            // Let the first layouts measure the blocks around the viewport: a scroll over items never laid out has no
            // heights to walk and lands at the end.
            window.request_animation_frame();
            return;
        }
        let limit = std::env::var("SPIKE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(90);
        let done = top.item_ix + 30 >= BLOCKS || test.started.elapsed() > Duration::from_secs(limit);
        if done {
            let mut frames: Vec<f64> = test.frames.iter().skip(5).map(|d| d.as_secs_f64() * 1000.0).collect();
            frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pick = |q: f64| frames.get(((frames.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
            let over = frames.iter().filter(|ms| **ms > 16.7).count();
            println!(
                "SELFTEST blocks={BLOCKS} reached_item={} frames={} build_ms={} p50_ms={:.2} p95_ms={:.2} p99_ms={:.2} max_ms={:.2} over_16ms={} elapsed_s={:.1}",
                top.item_ix,
                frames.len(),
                test.built_ms,
                pick(0.5),
                pick(0.95),
                pick(0.99),
                frames.last().copied().unwrap_or(0.0),
                over,
                test.started.elapsed().as_secs_f64()
            );
            let mut worst: Vec<(usize, f64)> = test.frames.iter().enumerate().map(|(ix, d)| (ix, d.as_secs_f64() * 1000.0)).collect();
            worst.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            println!("WORST {:?}", worst.iter().take(8).map(|(ix, ms)| format!("#{ix}={ms:.1}ms")).collect::<Vec<_>>());
            cx.quit();
            return;
        }
        // ~6 screens of transcript a second at 120 Hz: a fast fling, every frame a fresh range of blocks laid out.
        let step = std::env::var("SPIKE_STEP").ok().and_then(|v| v.parse().ok()).unwrap_or(60.0);
        self.transcript.scroll_by(px(step));
        window.request_animation_frame();
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.step_selftest(window, cx);
        let sources = self.sources.clone();
        let theme = cx.theme().clone();
        let transcript = list(self.transcript.clone(), move |ix, _window, _cx| {
            div()
                .px_4()
                .py_2()
                .child(TextView::markdown(("block", ix), sources[ix].clone()).selectable(true))
                .into_any_element()
        })
        .size_full();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x07090c))
            .text_color(rgb(0xeef1f5))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(div().id("transcript").role(Role::Log).aria_label("Transcript").debug_selector(|| "transcript".into()).w(relative(0.55)).h_full().child(transcript))
                    .child(
                        div()
                            .id("editor")
                            .role(Role::Group)
                            .aria_label("Editor")
                            .flex_1()
                            .h_full()
                            .border_l_1()
                            .border_color(rgb(0x1b2129))
                            .child(Editor::new(&self.editor).bordered(false).h(relative(1.)).font_family(theme.mono_font_family.clone())),
                    ),
            )
            .child(div().id("composer").role(Role::Group).aria_label("Composer").debug_selector(|| "composer".into()).p_2().border_t_1().border_color(rgb(0xff9e3d)).child(Input::new(&self.composer)))
            .child(div().absolute().top_2().right_2().child(gpui_fps::fps_monitor(window, cx)))
    }
}

fn main() {
    let selftest = std::env::args().any(|a| a == "--selftest");
    gpui_platform::application().run(move |cx: &mut App| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Cordata spike".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Workspace::new(selftest, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::Workspace;
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    fn the_workspace_renders_takes_typing_and_exposes_an_a11y_tree(cx: &mut TestAppContext) {
        cx.update(|cx| gpui_component::init(cx));
        let (view, cx) = cx.add_window_view(|window, cx| Workspace::new(false, window, cx));
        cx.run_until_parked();

        let composer = view.read_with(cx, |this, _| this.composer.clone());
        cx.update(|window, cx| composer.update(cx, |state, cx| state.focus(window, cx)));
        cx.simulate_input("refactor the machine");
        cx.run_until_parked();
        let typed = composer.read_with(cx, |state, _| state.value().to_string());
        assert_eq!(typed, "refactor the machine", "the composer takes typed text");

        let editor_len = view.read_with(cx, |this, cx| this.editor.read(cx).value().len());
        assert!(editor_len > 900_000, "the 50k-line file is loaded ({editor_len} bytes)");
        assert!(cx.debug_bounds("transcript").is_some(), "the transcript is laid out");

        // The window's AccessKit tree is only built once an assistive technology activates it (the platform window's
        // `a11y_init` callback), which the test platform never does: node-level checks are how gpui-base tests roles
        // and labels (`write_a11y_info`), and VoiceOver itself is checked by hand (docs/app.md, D1 gate report).
        match cx.update(|window, _| window.debug_a11y_tree_json()) {
            Some(tree) => {
                for label in ["Transcript", "Editor", "Composer"] {
                    assert!(tree.contains(label), "the a11y tree names the {label}");
                }
            }
            None => eprintln!("a11y: tree inactive in TestAppContext (no assistive technology attached)"),
        }
    }
}
