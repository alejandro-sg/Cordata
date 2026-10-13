//! Keys and pointer events as the shared state sees them, independent of any front end.
//!
//! The names and shapes deliberately match crossterm's (`KeyEvent { code, modifiers, kind }`, `KeyCode::Char`,
//! `KeyModifiers::CONTROL`, `MouseEventKind::ScrollUp`, …): the reducers were written against crossterm, and keeping
//! its vocabulary means moving them here changed an import line, not their logic. The terminal converts its events at
//! the edge (`From<crossterm::event::KeyEvent>`, feature `crossterm`); the native app builds them from its own
//! keystrokes.

use std::ops::{BitAnd, BitOr, BitOrAssign};

/// A key, as pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    /// F1–F24.
    F(u8),
    Char(char),
    Null,
    Esc,
}

/// Modifier keys held with a key or a click. A small bit set, with crossterm's constant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    /// ⌘ on macOS.
    pub const SUPER: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self::NONE
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every modifier in `other` is held.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Any modifier in `other` is held.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for KeyModifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for KeyModifiers {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// Press, auto-repeat, or release (only terminals with the kitty protocol report releases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

impl KeyEvent {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers, kind: KeyEventKind::Press }
    }

    pub const fn new_with_kind(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Self {
        Self { code, modifiers, kind }
    }
}

impl From<KeyCode> for KeyEvent {
    fn from(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    Moved,
    ScrollDown,
    ScrollUp,
    ScrollLeft,
    ScrollRight,
}

/// A pointer event in cells (the terminal) or in whatever unit the front end lays the overlay out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub column: u16,
    pub row: u16,
    pub modifiers: KeyModifiers,
}

#[cfg(feature = "crossterm")]
mod from_crossterm {
    use super::*;
    use crossterm::event as ct;

    impl From<ct::KeyCode> for KeyCode {
        fn from(code: ct::KeyCode) -> Self {
            match code {
                ct::KeyCode::Backspace => KeyCode::Backspace,
                ct::KeyCode::Enter => KeyCode::Enter,
                ct::KeyCode::Left => KeyCode::Left,
                ct::KeyCode::Right => KeyCode::Right,
                ct::KeyCode::Up => KeyCode::Up,
                ct::KeyCode::Down => KeyCode::Down,
                ct::KeyCode::Home => KeyCode::Home,
                ct::KeyCode::End => KeyCode::End,
                ct::KeyCode::PageUp => KeyCode::PageUp,
                ct::KeyCode::PageDown => KeyCode::PageDown,
                ct::KeyCode::Tab => KeyCode::Tab,
                ct::KeyCode::BackTab => KeyCode::BackTab,
                ct::KeyCode::Delete => KeyCode::Delete,
                ct::KeyCode::Insert => KeyCode::Insert,
                ct::KeyCode::F(n) => KeyCode::F(n),
                ct::KeyCode::Char(c) => KeyCode::Char(c),
                ct::KeyCode::Esc => KeyCode::Esc,
                // Caps/Num/Scroll lock, media keys, bare modifiers, keypad begin: nothing here acts on them.
                _ => KeyCode::Null,
            }
        }
    }

    impl From<ct::KeyModifiers> for KeyModifiers {
        fn from(mods: ct::KeyModifiers) -> Self {
            let mut out = KeyModifiers::NONE;
            if mods.contains(ct::KeyModifiers::SHIFT) {
                out |= KeyModifiers::SHIFT;
            }
            if mods.contains(ct::KeyModifiers::CONTROL) {
                out |= KeyModifiers::CONTROL;
            }
            if mods.contains(ct::KeyModifiers::ALT) {
                out |= KeyModifiers::ALT;
            }
            if mods.contains(ct::KeyModifiers::SUPER) {
                out |= KeyModifiers::SUPER;
            }
            out
        }
    }

    impl From<ct::KeyEventKind> for KeyEventKind {
        fn from(kind: ct::KeyEventKind) -> Self {
            match kind {
                ct::KeyEventKind::Press => KeyEventKind::Press,
                ct::KeyEventKind::Repeat => KeyEventKind::Repeat,
                ct::KeyEventKind::Release => KeyEventKind::Release,
            }
        }
    }

    impl From<ct::KeyEvent> for KeyEvent {
        fn from(key: ct::KeyEvent) -> Self {
            KeyEvent { code: key.code.into(), modifiers: key.modifiers.into(), kind: key.kind.into() }
        }
    }

    impl From<ct::MouseButton> for MouseButton {
        fn from(button: ct::MouseButton) -> Self {
            match button {
                ct::MouseButton::Left => MouseButton::Left,
                ct::MouseButton::Right => MouseButton::Right,
                ct::MouseButton::Middle => MouseButton::Middle,
            }
        }
    }

    impl From<ct::MouseEventKind> for MouseEventKind {
        fn from(kind: ct::MouseEventKind) -> Self {
            match kind {
                ct::MouseEventKind::Down(b) => MouseEventKind::Down(b.into()),
                ct::MouseEventKind::Up(b) => MouseEventKind::Up(b.into()),
                ct::MouseEventKind::Drag(b) => MouseEventKind::Drag(b.into()),
                ct::MouseEventKind::Moved => MouseEventKind::Moved,
                ct::MouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
                ct::MouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
                ct::MouseEventKind::ScrollLeft => MouseEventKind::ScrollLeft,
                ct::MouseEventKind::ScrollRight => MouseEventKind::ScrollRight,
            }
        }
    }

    impl From<ct::MouseEvent> for MouseEvent {
        fn from(mouse: ct::MouseEvent) -> Self {
            MouseEvent { kind: mouse.kind.into(), column: mouse.column, row: mouse.row, modifiers: mouse.modifiers.into() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_combine_and_test_like_crossterm_s() {
        let both = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert!(both.contains(KeyModifiers::CONTROL) && both.contains(KeyModifiers::SHIFT));
        assert!(!both.contains(KeyModifiers::ALT) && both.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert!(KeyModifiers::NONE.is_empty() && KeyModifiers::NONE.contains(KeyModifiers::NONE));
        assert_eq!(KeyEvent::from(KeyCode::Enter), KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[cfg(feature = "crossterm")]
    #[test]
    fn crossterm_events_convert_exactly() {
        use crossterm::event as ct;
        let key = ct::KeyEvent::new(ct::KeyCode::Char('k'), ct::KeyModifiers::CONTROL | ct::KeyModifiers::ALT);
        let ours = KeyEvent::from(key);
        assert_eq!(ours.code, KeyCode::Char('k'));
        assert_eq!(ours.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(ours.kind, KeyEventKind::Press);
        let mouse = ct::MouseEvent { kind: ct::MouseEventKind::Down(ct::MouseButton::Left), column: 3, row: 7, modifiers: ct::KeyModifiers::SHIFT };
        assert_eq!(MouseEvent::from(mouse), MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 7, modifiers: KeyModifiers::SHIFT });
        assert_eq!(KeyCode::from(ct::KeyCode::F(4)), KeyCode::F(4));
        assert_eq!(KeyCode::from(ct::KeyCode::CapsLock), KeyCode::Null);
    }
}
