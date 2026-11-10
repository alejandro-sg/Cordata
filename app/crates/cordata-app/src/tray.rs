//! The menu bar item: Cordata where you already are. `cordata_view::tray` decides what it says; this turns that into a
//! native status item and menu, and a click back into an action id. Nothing here decides anything.
//!
//! The app keeps running when its window is closed (⌘Q quits), so the item is there when something needs you.

use std::sync::atomic::AtomicBool;

/// ⌘Q or the menu's Quit was chosen: the window may close for real now.
pub static QUITTING: AtomicBool = AtomicBool::new(false);
/// The item is up, so the window's close button hides the app instead of ending it.
pub static IN_MENU_BAR: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
pub use mac::Native;

#[cfg(not(target_os = "macos"))]
pub struct Native;

#[cfg(not(target_os = "macos"))]
impl Native {
    pub fn new() -> Option<Self> {
        None
    }
    pub fn set(&self, _: &cordata_view::tray::Tray) {}
    pub fn drain(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use cordata_view::tray::{Item, Tray};
    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
    use objc2_foundation::{NSObjectProtocol, NSString};

    struct Ivars {
        tx: Sender<String>,
        /// Action id of each clickable item, by the tag it carries.
        ids: RefCell<Vec<String>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "CordataTrayTarget"]
        #[ivars = Ivars]
        struct Target;

        unsafe impl NSObjectProtocol for Target {}

        impl Target {
            #[unsafe(method(fire:))]
            fn fire(&self, sender: &NSMenuItem) {
                let id = usize::try_from(sender.tag()).ok().and_then(|i| self.ivars().ids.borrow().get(i).cloned());
                if let Some(id) = id {
                    let _ = self.ivars().tx.send(id);
                }
            }
        }
    );

    pub struct Native {
        mtm: MainThreadMarker,
        item: Retained<NSStatusItem>,
        target: Retained<Target>,
        rx: Receiver<String>,
        last: RefCell<Option<Tray>>,
    }

    impl Native {
        /// On the main thread only; `None` anywhere else.
        pub fn new() -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let (tx, rx) = channel();
            let target = mtm.alloc::<Target>().set_ivars(Ivars { tx, ids: RefCell::new(Vec::new()) });
            // SAFETY: a plain NSObject subclass with no custom init.
            let target: Retained<Target> = unsafe { msg_send![super(target), init] };
            let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            super::IN_MENU_BAR.store(true, Ordering::Relaxed);
            Some(Self { mtm, item, target, rx, last: RefCell::new(None) })
        }

        /// Show `tray`. A tray that is the same as the last one is not rebuilt (an open menu would flicker).
        pub fn set(&self, tray: &Tray) {
            if self.last.borrow().as_ref() == Some(tray) {
                return;
            }
            if let Some(button) = self.item.button(self.mtm) {
                button.setTitle(&NSString::from_str(&tray.title));
                button.setToolTip(Some(&NSString::from_str(&tray.tooltip)));
            }
            let menu = NSMenu::new(self.mtm);
            menu.setAutoenablesItems(false);
            let mut ids = Vec::new();
            for item in &tray.items {
                let entry = match item {
                    Item::Sep => NSMenuItem::separatorItem(self.mtm),
                    Item::Header(text) | Item::Note(text) => {
                        let entry = NSMenuItem::new(self.mtm);
                        entry.setTitle(&NSString::from_str(text));
                        entry.setEnabled(false);
                        if matches!(item, Item::Note(_)) {
                            entry.setIndentationLevel(1);
                        }
                        entry
                    }
                    Item::Action { id, title } => {
                        // SAFETY: `fire:` is defined on Target with the signature an action needs.
                        let entry = unsafe {
                            NSMenuItem::initWithTitle_action_keyEquivalent(self.mtm.alloc(), &NSString::from_str(title), Some(sel!(fire:)), &NSString::from_str(""))
                        };
                        // SAFETY: the target outlives the menu: this struct owns both.
                        unsafe { entry.setTarget(Some(&self.target)) };
                        entry.setTag(ids.len() as isize);
                        ids.push(id.clone());
                        entry
                    }
                };
                menu.addItem(&entry);
            }
            *self.target.ivars().ids.borrow_mut() = ids;
            self.item.setMenu(Some(&menu));
            *self.last.borrow_mut() = Some(tray.clone());
        }

        /// The action ids clicked since the last call.
        pub fn drain(&self) -> Vec<String> {
            self.rx.try_iter().collect()
        }
    }

    impl Drop for Native {
        fn drop(&mut self) {
            super::IN_MENU_BAR.store(false, Ordering::Relaxed);
            NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        }
    }
}
