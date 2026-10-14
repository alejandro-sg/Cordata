//! The few macOS things GPUI does not reach: the system's accessibility display flags, sounds and notifications.
//! (docs/app.md §4b). Everything else is GPUI's.

/// The system's display preferences, read once at launch (System Settings → Accessibility → Display).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Display {
    pub increase_contrast: bool,
    pub reduce_transparency: bool,
    pub reduce_motion: bool,
    pub differentiate_without_color: bool,
}

impl Display {
    #[cfg(target_os = "macos")]
    pub fn read() -> Self {
        use objc2_app_kit::NSWorkspace;
        // SAFETY: plain reads of NSWorkspace's shared instance, on the main thread at launch.
        let workspace = NSWorkspace::sharedWorkspace();
        Self {
            increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
            reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
            reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
            differentiate_without_color: workspace.accessibilityDisplayShouldDifferentiateWithoutColor(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn read() -> Self {
        Self::default()
    }

    /// Glass and the drifting haze are decoration: off when the person asked for less transparency or motion.
    pub fn atmosphere(self) -> bool {
        !self.reduce_transparency && !self.reduce_motion
    }
}

impl gpui::Global for Display {}

/// A system sound by name (`Glass`, `Ping`…), for a finished turn. Silent where there is none.
#[cfg(target_os = "macos")]
pub fn play_sound(name: &str) {
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSString;
    if let Some(sound) = NSSound::soundNamed(&NSString::from_str(name)) {
        sound.play();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn play_sound(_name: &str) {}

/// A system notification. Only fixed text from the app is ever passed (never model or tool output), and it goes
/// through argv to AppleScript's `on run`, never spliced into the script, so nothing in it is code.
#[cfg(target_os = "macos")]
pub fn notify(title: &'static str, body: &'static str) {
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run", title, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "macos"))]
pub fn notify(_title: &'static str, _body: &'static str) {}

/// System dictation into the focused text box (Edit → Start Dictation): the composer must have focus first.
#[cfg(target_os = "macos")]
pub fn start_dictation() {
    use objc2_app_kit::NSApplication;
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: a standard responder-chain action with no arguments, sent on the main thread.
    unsafe {
        app.sendAction_to_from(objc2::sel!(startDictation:), None, None);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn start_dictation() {}

/// The global quick-ask hotkey (⌃⌥Space): Carbon's `RegisterEventHotKey`, which needs no Accessibility permission. A
/// press only sets a flag; the window polls it (`take_hotkey`) and comes forward with the composer focused.
#[cfg(target_os = "macos")]
mod hotkey {
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[repr(C)]
    struct EventTypeSpec {
        event_class: u32,
        event_kind: u32,
    }

    #[repr(C)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }

    type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetApplicationEventTarget() -> *mut c_void;
        fn InstallEventHandler(target: *mut c_void, handler: Handler, count: u32, types: *const EventTypeSpec, user: *mut c_void, out: *mut *mut c_void) -> i32;
        fn RegisterEventHotKey(key: u32, modifiers: u32, id: EventHotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
        fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    }

    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
    const KVK_SPACE: u32 = 49;
    const OPTION_KEY: u32 = 0x0800;
    const CONTROL_KEY: u32 = 0x1000;

    pub static PRESSED: AtomicBool = AtomicBool::new(false);
    static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
    /// The registered hot key (an opaque Carbon reference, kept as an address).
    static REGISTERED: Mutex<usize> = Mutex::new(0);

    extern "C" fn on_hotkey(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
        PRESSED.store(true, Ordering::SeqCst);
        0
    }

    pub fn set(enabled: bool) -> bool {
        let mut registered = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: Carbon calls on the main thread at launch or from a settings change; the handler is a plain function
        // that only stores a flag; the hot key reference is only ever passed back to Carbon.
        unsafe {
            if !enabled {
                if *registered != 0 {
                    UnregisterEventHotKey(*registered as *mut c_void);
                    *registered = 0;
                }
                return true;
            }
            if *registered != 0 {
                return true;
            }
            let target = GetApplicationEventTarget();
            if !HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
                let spec = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
                if InstallEventHandler(target, on_hotkey, 1, &spec, std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                    HANDLER_INSTALLED.store(false, Ordering::SeqCst);
                    return false;
                }
            }
            let mut out: *mut c_void = std::ptr::null_mut();
            let id = EventHotKeyId { signature: u32::from_be_bytes(*b"CDTA"), id: 1 };
            if RegisterEventHotKey(KVK_SPACE, OPTION_KEY | CONTROL_KEY, id, target, 0, &mut out) != 0 {
                return false; // another app holds the combination
            }
            *registered = out as usize;
        }
        true
    }
}

/// Turn the quick-ask hotkey on or off. False when it could not be registered (another app holds ⌃⌥Space).
#[cfg(target_os = "macos")]
pub fn set_hotkey(enabled: bool) -> bool {
    hotkey::set(enabled)
}

#[cfg(not(target_os = "macos"))]
pub fn set_hotkey(_enabled: bool) -> bool {
    false
}

/// Whether the hotkey was pressed since the last call.
pub fn take_hotkey() -> bool {
    #[cfg(target_os = "macos")]
    {
        hotkey::PRESSED.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
