//! Full-screen overlays: the interactive half of the hybrid UI.
//!
//! The conversation stays inline, in the terminal's own scrollback (see `term.rs`). Anything that is *a place to
//! look around in* — the command deck (Ctrl+K) and the diff review (F4) — takes over the screen
//! while it is open, with the mouse on: hover highlights, click chooses, the wheel scrolls. Closing it puts the
//! terminal back exactly as it was, scrollback and live region intact (`main.rs` switches the alternate screen and
//! mouse capture around it).
//!
//! An overlay is plain state plus three pure-ish functions — `render` (into a full-screen `Buffer`), `on_key` and
//! `on_mouse` (each returns an [`Outcome`]) — so every one is tested without a terminal.

mod palette;
mod review;

use cordata_view::key::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub use palette::Palette;
pub use review::Review;

use crate::app::App;
use crate::ui::hud;
use crate::ui::text::{display_width, truncate};

pub use cordata_view::overlay::{Kind, Outcome};

pub enum Overlay {
    Palette(Palette),
    Review(Review),
}

impl Overlay {
    pub fn new(kind: Kind, app: &App) -> Self {
        match kind {
            Kind::Palette => Overlay::Palette(Palette::new(app)),
            Kind::Review => Overlay::Review(Review::loading()),
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Overlay::Palette(_) => Kind::Palette,
            Overlay::Review(_) => Kind::Review,
        }
    }

    /// The whole screen, `width`×`height`.
    pub fn render(&mut self, app: &App, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width.max(20), height.max(6));
        let mut buffer = Buffer::empty(area);
        match self {
            Overlay::Palette(view) => view.render(app, &mut buffer, area),
            Overlay::Review(view) => view.render(app, &mut buffer, area),
        }
        buffer
    }

    pub fn on_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome {
        match self {
            Overlay::Palette(view) => view.on_key(app, key),
            Overlay::Review(view) => view.on_key(app, key),
        }
    }

    pub fn on_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Outcome {
        match self {
            Overlay::Palette(view) => view.on_mouse(app, mouse),
            Overlay::Review(view) => view.on_mouse(app, mouse),
        }
    }
}

// ------------------------------------------------------------------------------------------------- shared drawing

pub(crate) fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub(crate) fn put(buffer: &mut Buffer, x: u16, y: u16, spans: Vec<Span<'static>>, width: u16) {
    buffer.set_line(x, y, &Line::from(spans), width);
}

/// A full-width horizontal rule of `─`.
pub(crate) fn rule(buffer: &mut Buffer, y: u16, area: Rect) {
    buffer.set_string(area.x, y, "─".repeat(area.width as usize), dim());
}

/// The frame every overlay shares: a title row (`⌜ C O R D A T A ⌝ // TITLE` on the left, the mode and model on the
/// right), a rule, and at the bottom a rule and the key hints. Returns the body rectangle between them.
pub(crate) fn chrome(buffer: &mut Buffer, area: Rect, app: &App, title: &str, hints: &str) -> Rect {
    let width = area.width as usize;
    let mut left = vec![Span::raw(" ")];
    left.extend(hud::bracketed(&hud::tracked("cordata"), Color::White));
    left.push(Span::styled("  // ", dim()));
    left.push(Span::styled(title.to_owned(), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)));

    let mut right: Vec<Span<'static>> = Vec::new();
    let model = app.session.as_ref().map(|session| session.model.clone()).unwrap_or_default();
    if !model.is_empty() {
        right.push(Span::styled(model, Style::new().fg(Color::Magenta)));
        right.push(Span::raw("  "));
    }
    right.push(Span::raw(" "));

    let used: usize = left.iter().map(|s| display_width(&s.content)).sum();
    let right_width: usize = right.iter().map(|s| display_width(&s.content)).sum();
    let mut row = left;
    if used + right_width < width {
        row.push(Span::raw(" ".repeat(width - used - right_width)));
        row.extend(right);
    }
    put(buffer, area.x, area.y, row, area.width);
    rule(buffer, area.y + 1, area);

    let bottom = area.y + area.height - 1;
    rule(buffer, bottom - 1, area);
    buffer.set_string(area.x + 1, bottom, truncate(hints, width.saturating_sub(2)), dim());
    Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(4))
}

/// An angular frame — `┌╴// TITLE ───┐` over `└───┘` — for a HUD panel; returns the inside. `focused` brightens the
/// frame so a click or Tab target is visible.
pub(crate) fn panel(buffer: &mut Buffer, area: Rect, title: &str, focused: bool) -> Rect {
    if area.width < 6 || area.height < 3 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let edge = if focused { Style::new().fg(Color::Cyan) } else { dim() };
    let head = format!("╴// {title} ");
    let fill = (area.width as usize).saturating_sub(2 + display_width(&head));
    let top = Line::from(vec![
        Span::styled("┌", edge),
        Span::styled(head, Style::new().fg(if focused { Color::Cyan } else { Color::Gray }).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(fill), edge),
        Span::styled("┐", edge),
    ]);
    buffer.set_line(area.x, area.y, &top, area.width);
    for y in area.y + 1..area.y + area.height - 1 {
        buffer.set_string(area.x, y, "│", edge);
        buffer.set_string(area.x + area.width - 1, y, "│", edge);
    }
    let base = area.y + area.height - 1;
    buffer.set_string(area.x, base, format!("└{}┘", "─".repeat(area.width as usize - 2)), edge);
    Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2)
}

pub(crate) fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// The terminal's overlays, for the shared state to open (`App::overlay_factory`).
pub fn factory(kind: Kind, app: &App) -> Option<cordata_view::overlay::Overlay> {
    Some(Box::new(Overlay::new(kind, app)))
}

/// The terminal's `App`: the shared state with this front end's overlays installed.
pub fn new_app() -> App {
    let mut app = App::new();
    app.overlay_factory = factory;
    app
}

impl cordata_view::overlay::View for Overlay {
    fn kind(&self) -> Kind {
        Overlay::kind(self)
    }

    fn on_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome {
        Overlay::on_key(self, app, key)
    }

    fn on_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Outcome {
        Overlay::on_mouse(self, app, mouse)
    }

    fn review_loaded(&mut self, outcome: &crate::shell::Outcome) {
        if let Overlay::Review(review) = self {
            review.load(outcome);
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The open overlay as the terminal's own type (every overlay the terminal opens is one).
pub fn concrete(overlay: &mut cordata_view::overlay::Overlay) -> Option<&mut Overlay> {
    overlay.as_any_mut().downcast_mut::<Overlay>()
}

/// `CORDATA_DUMP_DIR=/some/dir cargo test dump_screens -- --ignored` writes each screen as ANSI text, for looking at the
/// real thing (or turning into a picture) without driving a terminal.
#[cfg(test)]
pub(crate) mod testing {
    use acp_core::{AppEvent, SessionInfo};
    use cordata_view::key::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;

    use crate::app::App;

    pub fn app() -> App {
        let mut app = crate::overlay::new_app();
        app.on_event(AppEvent::Connected(SessionInfo {
            session_id: "abcdef12-3456".into(),
            cwd: "/proj".into(),
            model: "kimi-k2.7-code".into(),
            transport: "native · kimi-k2.7-code".into(),
        }));
        app
    }

    pub fn text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.top()..area.bottom())
            .map(|y| (area.left()..area.right()).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    pub fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    pub fn click(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    /// The (column, row) of the first occurrence of `needle` on screen.
    pub fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let area = buffer.area;
        (area.top()..area.bottom()).find_map(|y| {
            let cells: Vec<String> = (area.left()..area.right()).map(|x| buffer[(x, y)].symbol().to_owned()).collect();
            (0..cells.len()).find(|&x| cells[x..].concat().starts_with(needle)).map(|x| (area.left() + x as u16, y))
        })
    }
}
