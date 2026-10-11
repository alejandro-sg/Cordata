//! The one keystroke that calls the deck, from anywhere.
//!
//! `global-hotkey` is Carbon `RegisterEventHotKey` on macOS, and that is the decisive fact for this
//! module: it needs **neither Accessibility nor Input Monitoring**, because macOS routes a
//! registered hotkey to the app without granting the app the right to watch the keyboard. A
//! "global event monitor" would have cost Accessibility — the same permission the capture needs —
//! for nothing.
//!
//! Two rules shape the rest:
//!
//! * **A failure is visible.** A hotkey that silently never fires is worse than no hotkey: the
//!   person keeps pressing it and concludes the app is broken. A failed registration becomes a
//!   notice in the model and a value the deck renders as a chip, and the code then tries an ordered
//!   fallback list, recording which binding actually won.
//! * **Clicking the tray always works**, so a hotkey failure degrades the app rather than
//!   disabling it. That is why nothing here can fail the launch.
//!
//! The registrar is a trait so the whole of this is testable without a keyboard: the real one
//! registers with Carbon, the fake one refuses whatever a test says it should.

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// The bindings tried in order. The first is the documented default; the others exist so a
/// conflict with another app (Spaces, Spotlight, a keyboard remapper) degrades to a working
/// hotkey rather than to nothing.
pub fn bindings() -> Vec<(&'static str, HotKey)> {
    vec![
        ("cmd+shift+space", HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Space)),
        ("ctrl+alt+space", HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)),
        ("ctrl+shift+space", HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space)),
        ("cmd+shift+k", HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyK)),
    ]
}

/// Something that can claim a keystroke for the app. Implemented for real by [`CarbonRegistrar`],
/// and by a fake in tests.
pub trait Registrar: Send + Sync {
    fn register(&self, hotkey: HotKey) -> Result<(), String>;
}

/// The real registrar: the process-wide Carbon hotkey manager.
pub struct CarbonRegistrar {
    manager: GlobalHotKeyManager,
}

impl CarbonRegistrar {
    /// Create the manager. Must be called on the main thread, with the event loop running:
    /// `RegisterEventHotKey` is pumped by the app's own run loop.
    pub fn new() -> Result<Self, String> {
        GlobalHotKeyManager::new().map(|manager| Self { manager }).map_err(|error| format!("the hotkey manager could not be created: {error}"))
    }
}

impl Registrar for CarbonRegistrar {
    fn register(&self, hotkey: HotKey) -> Result<(), String> {
        self.manager.register(hotkey).map_err(|error| format!("{error}"))
    }
}

/// The hotkey the app is listening for, or why it is not listening for one.
pub struct Hotkey {
    /// Which binding won, in the spellings [`bindings`] uses — shown to the person, so it has to
    /// say what to press rather than what the library calls it.
    pub binding: Option<String>,
    /// The id to match events against, when a binding was registered.
    id: Option<u32>,
    /// Why no binding could be claimed, in a sentence a person can act on.
    pub error: Option<String>,
}

impl Hotkey {
    /// Claim the first binding that the registrar accepts. Never fails: a hotkey is a convenience,
    /// and the tray is the thing that must always work.
    pub fn install(registrar: &dyn Registrar) -> Self {
        let mut failures = Vec::new();
        for (name, hotkey) in bindings() {
            match registrar.register(hotkey) {
                Ok(()) => return Self { binding: Some(name.to_owned()), id: Some(hotkey.id), error: None },
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        Self { binding: None, id: None, error: Some(format!("no summon hotkey is available — click the menu bar instead. Tried: {}", failures.join("; "))) }
    }

    /// No registrar could even be created — different from every binding being taken, and worth
    /// distinguishing in what is said, because the fix is different.
    pub fn unavailable(error: String) -> Self {
        Self { binding: None, id: None, error: Some(format!("no summon hotkey is available ({error}) — click the menu bar instead")) }
    }

    /// A keystroke press since the last look. Polled, not callbacked, for the same reason the tray
    /// polls: the callback API is built around a winit event loop this app does not have.
    ///
    /// Only `Pressed` counts — a hotkey that acted on both press and release would open the deck and
    /// immediately close it.
    pub fn pressed(&self) -> bool {
        let Some(id) = self.id else { return false };
        let receiver = GlobalHotKeyEvent::receiver();
        let mut pressed = false;
        while let Ok(event) = receiver.try_recv() {
            if event.id == id && event.state == HotKeyState::Pressed {
                pressed = true;
            }
        }
        pressed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Refuses whatever it is told to, so the fallback list and the failure message can be tested
    /// without a keyboard, a compositor, or another app holding a binding.
    struct Fake {
        refuse: Vec<Code>,
        tried: Mutex<Vec<Code>>,
    }

    impl Fake {
        fn refusing(codes: Vec<Code>) -> Self {
            Self { refuse: codes, tried: Mutex::new(Vec::new()) }
        }
    }

    impl Registrar for Fake {
        fn register(&self, hotkey: HotKey) -> Result<(), String> {
            self.tried.lock().unwrap().push(hotkey.key);
            if self.refuse.contains(&hotkey.key) {
                Err("this keystroke is taken by another app".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn the_first_binding_wins_when_it_is_free() {
        let fake = Fake::refusing(vec![]);
        let hotkey = Hotkey::install(&fake);
        assert_eq!(hotkey.binding.as_deref(), Some("cmd+shift+space"), "the documented default");
        assert!(hotkey.error.is_none());
        assert_eq!(fake.tried.lock().unwrap().len(), 1, "a binding that worked means no fallback is tried");
    }

    #[test]
    fn a_taken_binding_falls_through_in_order_and_says_which_won() {
        // Both space bindings are taken by other apps; the fourth is free.
        let fake = Fake::refusing(vec![Code::Space, Code::Space, Code::Space]);
        let hotkey = Hotkey::install(&fake);
        assert_eq!(hotkey.binding.as_deref(), Some("cmd+shift+k"), "the first free binding, not a random one");
        assert!(hotkey.error.is_none(), "a fallback that works is not a failure to report");
        let tried = fake.tried.lock().unwrap();
        assert_eq!(*tried, vec![Code::Space, Code::Space, Code::Space, Code::KeyK], "tried in the order listed, once each");
    }

    #[test]
    fn when_every_binding_is_taken_the_reason_is_said_and_the_tray_still_works() {
        let fake = Fake::refusing(vec![Code::Space, Code::KeyK]);
        let hotkey = Hotkey::install(&fake);
        assert!(hotkey.binding.is_none());
        let error = hotkey.error.expect("a total failure must be explained");
        assert!(error.contains("cmd+shift+space") && error.contains("taken by another app"), "the message names what was tried and why: {error}");
        assert!(error.contains("click the menu bar"), "…and what to do instead: {error}");
    }

    #[test]
    fn without_a_binding_nothing_ever_reads_as_pressed() {
        let hotkey = Hotkey { binding: None, id: None, error: Some("none".into()) };
        assert!(!hotkey.pressed(), "an unregistered hotkey must never fire");
    }

    #[test]
    fn the_bindings_are_distinct_and_the_first_is_the_documented_one() {
        let list = bindings();
        let mut seen = Vec::new();
        for (name, hotkey) in &list {
            assert!(!seen.contains(&hotkey.id), "{name} is the same keystroke as an earlier binding");
            seen.push(hotkey.id);
        }
        assert_eq!(list[0].0, "cmd+shift+space");
        assert!(list[0].1.mods.contains(Modifiers::SUPER) && list[0].1.mods.contains(Modifiers::SHIFT));
    }
}
