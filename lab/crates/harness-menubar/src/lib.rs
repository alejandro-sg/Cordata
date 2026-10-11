//! The menu bar front end: state at a glance, and the deck on a hotkey.
//!
//! * [`app`] — the app-scoped model and client: alive with no window, and the only writer of it
//! * [`tray`] — the status item: the three-state glyph, and clicks turned into plain values
//! * [`hotkey`] — the one keystroke that calls the deck, and what it says when it cannot
//! * [`deck_window`] — where the deck lands, and how it dismisses

pub mod app;
pub mod settings;
pub mod deck;
pub mod deck_window;
pub mod hotkey;
pub mod tray;
