//! The proactive workspace observer.
//!
//! Watches a directory for saves, parses each saved file locally with tree-sitter, and reports
//! only the functions, methods, classes and types whose text changed — not the file. That is the
//! whole trade: the GUI thread's side of "gather context" costs a parse and a hash, and what
//! leaves the process is a few small snippets, so the daemon's memory graph stays current without
//! anyone paying tokens to re-read files that did not change.
//!
//! * [`extract`] — tree-sitter → [`extract::Symbol`]s, and the diff between two versions of a file.
//! * [`watch`]   — `notify` events, debounced, filtered (no `target/`, no `.git`, never `.env`),
//!   baselined against `git HEAD` the first time a file is seen.

pub mod extract;
pub mod watch;

pub use extract::{Change, Lang, Symbol};
pub use watch::{Observer, ObserverConfig, ObserverError, Snippet, Stats};
