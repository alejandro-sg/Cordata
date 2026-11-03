//! Cordata's front-end state, shared by the terminal (`tui/crates/acp-tui`) and the native app (`app/crates/cordata-app`).
//!
//! Everything here is plain state and pure-ish functions over it: the conversation reducer ([`app::App::on_event`] turns
//! acp-core's events into messages, tool cards, pending approvals, gauges), the slash commands, the composer's input
//! model, menus, `@` mentions, `!` shell, custom commands, the settings schema and its editor and the run
//! board. Nothing draws: no ratatui, no GPUI, no terminal. Keys and pointer events arrive as [`key`]'s neutral types;
//! the full-screen overlays are the front end's, behind [`overlay::View`].
//!
//! Moved here from acp-tui unchanged in behaviour (docs/app.md §3 D3); acp-tui re-exports each module under its old
//! path, so its renderers and tests did not change.

pub mod app;
pub mod assistants;
pub mod commands;
pub mod config_ui;
pub mod custom;
pub mod extras;
pub mod input;
pub mod key;
pub mod mentions;
pub mod menu;
pub mod models;
pub mod overlay;
pub mod redact;
pub mod review;
pub mod settings;
pub mod shell;
pub mod telemetry;
pub mod tree;
pub mod view;

/// Text as a front end may draw it: terminal escapes and control characters stripped, credential-shaped spans masked.
/// The backend masks before it sends and the daemon masks again; this runs last, on everything shown, so the rule
/// "a key is never drawn" does not depend on the other two (THREAT_MODEL.md, invariant "scrub before draw").
pub fn shown(text: &str) -> String {
    redact::scrub(&app::sanitize(text)).0
}
