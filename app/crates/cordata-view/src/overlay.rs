//! Full-screen overlays (the command deck and the diff review) as the shared state sees them.
//!
//! The state that decides *when* an overlay opens, which keys it takes and what it asks for afterwards is shared; how
//! one looks and behaves inside is the front end's. The terminal implements [`View`] for its ratatui overlays
//! (`tui/crates/acp-tui/src/overlay/`) and installs a [`Factory`] on the [`App`]; a front end that draws these places
//! its own way installs none, and [`App::open_overlay`] then only reports the request (the review's diff fetch).

use std::any::Any;

use crate::app::{App, Effect};
use crate::key::{KeyEvent, MouseEvent};

/// Which overlay to open (what a palette entry or a key asks for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Palette,
    Review,
}

/// What an overlay asks the app to do after an input.
#[derive(Default)]
pub struct Outcome {
    pub close: bool,
    pub effect: Option<Effect>,
    /// Close this overlay and open another.
    pub open: Option<Kind>,
}

impl Outcome {
    pub fn stay() -> Self {
        Self::default()
    }

    pub fn stay_with(effect: Option<Effect>) -> Self {
        Self { close: false, effect, open: None }
    }

    pub fn close(effect: Option<Effect>) -> Self {
        Self { close: true, effect, open: None }
    }

    pub fn switch(kind: Kind) -> Self {
        Self { close: true, effect: None, open: Some(kind) }
    }
}

/// An open overlay, as the front end implements it.
pub trait View: Any {
    fn kind(&self) -> Kind;
    fn on_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome;
    fn on_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Outcome;
    /// The working-tree diff the review asked for has arrived (`LocalEvent::ReviewDone`).
    fn review_loaded(&mut self, _outcome: &crate::shell::Outcome) {}
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub type Overlay = Box<dyn View>;

/// Builds the overlay for a kind; `None` when this front end has none of that kind.
pub type Factory = fn(Kind, &App) -> Option<Overlay>;

/// The factory of a front end with no overlays of its own.
pub fn none(_kind: Kind, _app: &App) -> Option<Overlay> {
    None
}
