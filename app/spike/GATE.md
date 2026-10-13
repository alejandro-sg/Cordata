# D1 gate report — GPUI dependency (2026-10-12)

Question (docs/app.md §3 D1): can Cordata.app build on **option A** — `gpui-pre =0.3.9` (a re-publish of Zed's gpui at
zed@93d7456) plus `gpui-component =0.7.3` — instead of hand-rolling editor, selection and accessibility on the stale
official `gpui 0.2.2` (option B)?

**Verdict: GO on option A**, with the controls below. Two criteria are measured by hand, not by a script (VoiceOver,
a real IME), and one is a documented fallback (HTML artifacts).

Machine: M4 Pro, macOS 26.6.2, rustc 1.97.1, Command Line Tools only (no Xcode, no `metal` CLI).

| Criterion | Result | Evidence |
|---|---|---|
| Builds with CLT only | **pass** | `gpui-pre-platform` feature `runtime_shaders` compiles Metal shaders at run time; clean build ≈1 min, incremental <1 s |
| Opens a window | **pass** | `cargo run --release`; Metal renderer, 120 Hz ProMotion |
| 10k-block markdown transcript scrolls at refresh | **pass** | `--selftest`: variable-height `list(ListState)`, 10,000 selectable `TextView::markdown` blocks (headings, lists, tables, fenced code, quotes); 2,356 frames: p50 8.33 ms, p95 9.30 ms, p99 9.33 ms, 0 frames > 16 ms; first frame 325 ms (includes the 50k-line editor's first layout) |
| Code editor: 50k-line file, tree-sitter, diagnostics | **pass** (LSP wiring pending) | `EditorState::language("rust")` on a generated 50,000-line file; highlighting and line numbers render; diagnostics fed through `diagnostics_mut()` show as squiggles. No language server is installed on this machine, so rust-analyzer → diagnostics is P3 work, not a gate risk (the editor's `Lsp`/`DiagnosticSet` side exists) |
| IME + selection + copy across blocks | **structural pass, manual check owed** | composer is `Input` over gpui-base's `EntityInputHandler` (the `NSTextInputClient` path); `simulate_input` round-trips in the headless test. Cross-block selection: `TextView::selectable(true)` opts blocks into window-level selection (gpui-base `window_selection`, upstream example `text_selection`). A Japanese/Chinese IME session and a drag-select-copy across three blocks still need a person's two minutes |
| Headless UI tests in CI | **pass** | `cargo test` (debug): `#[gpui::test]` + `TestAppContext` builds the whole workspace, types into the composer, finds regions by `debug_selector` (debug builds only) |
| VoiceOver via AccessKit | **unmeasured, mechanism present** | gpui-pre links `accesskit` + `accesskit_macos`; elements take `.role()`, `.aria_label()`, `.aria_description()`; gpui-component sets roles/labels/actions on its controls. The window tree is built only when an assistive technology activates it — the test platform never does, and this shell has no Accessibility permission to query AX from outside. To check: run the spike, turn VoiceOver on (⌘F5), VO-arrow through Transcript / Editor / Composer |
| FPS/frame-time HUD from day one | **pass** | `gpui-fps =0.7.3` overlay (FPS, frame time, p95, drops, CPU, GPU, memory) |
| HTML artifact in a webview (JS off, no network) | **fallback** | `gpui-webview` exists only in the gpui-kit repo, not on crates.io at a matching version. Artifacts render as Markdown / images / code until it is published; revisit in P4 |
| Idle cost | note | ≈190 MB RSS with 10k blocks + 50k-line editor loaded; frames stop entirely when nothing changes and when the window is occluded (macOS pauses the display link) |

## Supply-chain controls (all in place before P0c)

- **Exact pins**: `gpui-pre =0.3.9`, `gpui-pre-platform =0.3.9`, `gpui-component =0.7.3`, `gpui-fps =0.7.3`; `Cargo.lock` committed.
- **Snapshot proof**: `app/scripts/verify-gpui-snapshot.sh` fetches zed@93d745668805 (sparse, blobless) and compares every
  file the 14 `gpui-pre-*` crates ship. Result: 12 crates byte-identical; `gpui` and `gpui-pre-macros` differ only in
  three files that carry the "Modified for gpui-pre" marker — a proc-macro path facade so the crates can be renamed
  (`gpui::Action` → `$crate::Action`, and a token rewriter for `gpui::`/`gpui_platform::` paths). Nothing else.
- **No network stack** in the macOS dependency graph (436 crates; `gpui-pre-reqwest` is not built on macOS).
- Licenses: gpui-pre crates and gpui-component are Apache-2.0. `cargo-deny` (licenses Apache-2.0/MIT/BSD/ISC/Zlib/
  Unicode only; bans; advisories) is wired in P0c.

## Findings that change the plan

1. **gpui-component 0.7.3 already has most of the chat surface**: `message_scroller`, `bubble`, `attachment`,
   `questionnaire` (≈ ask card), `diff` (≈ per-hunk review), `dock` (serializable right dock), `command` (⌘K deck),
   `speech` (macOS speech recognition behind a feature — the local-first voice tier of P6), `notification`, `sheet`,
   `sidebar`, `table`, `tree`. P1/P3 shrink accordingly.
2. **`App::reduce_motion`** is in gpui-pre; `with_animation` respects it — the Reduce Motion rule in §4b is free.
3. The display-link pause on occlusion means a self-test or soak must enforce its own time limit off the frame loop
   (the spike's watchdog).
4. The test macro is `gpui::test`; a test module must not glob-import `gpui::*` (it shadows `#[test]` and recurses).

## Reproduce

    cd app/spike
    cargo run --release                         # interactive: transcript, editor, composer, HUD
    cargo run --release -- --selftest           # SPIKE_SECONDS=20 SPIKE_STEP=60 to tune
    cargo test                                  # headless UI test (debug build: debug_selector)
    ../scripts/verify-gpui-snapshot.sh          # supply-chain proof (network once)
