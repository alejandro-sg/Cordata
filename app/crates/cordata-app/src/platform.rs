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

/// A global hotkey: a Carbon virtual key code and modifier mask, parsed from the prefs' `ctrl-alt-space` form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    pub key_code: u32,
    pub modifiers: u32,
    /// `⌃⌥Space`: what the settings and the shortcut list show.
    pub label: String,
}

/// What the settings menu offers; any other combination `Combo::parse` accepts works from prefs.toml.
pub const COMBO_PRESETS: [&str; 6] = ["ctrl-alt-space", "alt-space", "ctrl-space", "cmd-shift-space", "ctrl-alt-k", "f5"];

const CMD_KEY: u32 = 0x0100;
const SHIFT_KEY: u32 = 0x0200;
const OPTION_KEY: u32 = 0x0800;
const CONTROL_KEY: u32 = 0x1000;

/// Carbon `kVK_*` codes: the ANSI layout's letters, digits and punctuation, space, return, tab, backquote and F1–F12.
fn key_code(name: &str) -> Option<(u32, String)> {
    const LETTERS: [(char, u32); 26] = [
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9), ('b', 11), ('q', 12), ('w', 13),
        ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('o', 31), ('u', 32), ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('k', 40), ('n', 45), ('m', 46),
    ];
    const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const F_KEYS: [u32; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    let named = match name {
        "space" => Some((49, "Space")),
        "return" | "enter" => Some((36, "↵")),
        "tab" => Some((48, "⇥")),
        "backquote" | "`" => Some((50, "`")),
        "-" => Some((27, "-")),
        "=" => Some((24, "=")),
        "[" => Some((33, "[")),
        "]" => Some((30, "]")),
        ";" => Some((41, ";")),
        "'" => Some((39, "'")),
        "," => Some((43, ",")),
        "." => Some((47, ".")),
        "/" => Some((44, "/")),
        "\\" => Some((42, "\\")),
        _ => None,
    };
    if let Some((code, label)) = named {
        return Some((code, label.to_owned()));
    }
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if let Some(&(_, code)) = LETTERS.iter().find(|(l, _)| *l == ch) {
            return Some((code, ch.to_ascii_uppercase().to_string()));
        }
        if let Some(digit) = ch.to_digit(10) {
            return Some((DIGITS[digit as usize], ch.to_string()));
        }
    }
    let n: usize = name.strip_prefix('f')?.parse().ok()?;
    (1..=12).contains(&n).then(|| (F_KEYS[n - 1], format!("F{n}")))
}

impl Combo {
    /// `ctrl-alt-space`, `cmd-shift-k`, `f5`: modifiers (`ctrl`, `alt`/`opt`, `shift`, `cmd`) then one key. A key other
    /// than F1–F12 needs ⌃, ⌥ or ⌘ — a bare letter or ⇧-letter would swallow typing in every app.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lower = text.trim().to_ascii_lowercase();
        let parts: Vec<&str> = lower.split('-').filter(|p| !p.is_empty()).collect();
        // `-` itself is a key: `ctrl--` splits into ["ctrl"] and loses it.
        let (mods, key) = if lower.ends_with("--") || lower == "-" {
            (parts.as_slice(), "-")
        } else {
            match parts.split_last() {
                Some((key, mods)) => (mods, *key),
                None => return Err("no key given".into()),
            }
        };
        let mut modifiers = 0;
        for m in mods {
            modifiers |= match *m {
                "ctrl" | "control" => CONTROL_KEY,
                "alt" | "opt" | "option" => OPTION_KEY,
                "shift" => SHIFT_KEY,
                "cmd" | "command" => CMD_KEY,
                other => return Err(format!("“{other}” is not a modifier (ctrl, alt, shift, cmd)")),
            };
        }
        let (key_code, key_label) = key_code(key).ok_or_else(|| format!("“{key}” is not a key Cordata can bind"))?;
        let is_f_key = key_label.starts_with('F') && key_label.len() > 1;
        if !is_f_key && modifiers & (CONTROL_KEY | OPTION_KEY | CMD_KEY) == 0 {
            return Err(format!("{key_label} needs ⌃, ⌥ or ⌘ with it"));
        }
        if modifiers == CMD_KEY && key_code == 49 {
            return Err("⌘Space is Spotlight".into());
        }
        let mut label = String::new();
        for (bit, glyph) in [(CONTROL_KEY, "⌃"), (OPTION_KEY, "⌥"), (SHIFT_KEY, "⇧"), (CMD_KEY, "⌘")] {
            if modifiers & bit != 0 {
                label.push_str(glyph);
            }
        }
        label.push_str(&key_label);
        Ok(Self { key_code, modifiers, label })
    }
}

/// The global quick-ask hotkey: Carbon's `RegisterEventHotKey`, which needs no Accessibility permission. A press only
/// sets a flag; the window polls it (`take_hotkey`) and comes forward with the composer focused.
#[cfg(target_os = "macos")]
mod hotkey {
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::Combo;

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

    pub static PRESSED: AtomicBool = AtomicBool::new(false);
    static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
    /// The registered hot key (an opaque Carbon reference, kept as an address) and what it was registered for.
    static REGISTERED: Mutex<(usize, u32, u32)> = Mutex::new((0, 0, 0));

    extern "C" fn on_hotkey(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
        PRESSED.store(true, Ordering::SeqCst);
        0
    }

    pub fn set(combo: Option<&Combo>) -> bool {
        let mut registered = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: Carbon calls on the main thread at launch or from a settings change; the handler is a plain function
        // that only stores a flag; the hot key reference is only ever passed back to Carbon.
        unsafe {
            let Some(combo) = combo else {
                if registered.0 != 0 {
                    UnregisterEventHotKey(registered.0 as *mut c_void);
                    *registered = (0, 0, 0);
                }
                return true;
            };
            if registered.0 != 0 && (registered.1, registered.2) == (combo.key_code, combo.modifiers) {
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
            // The new one first: when another app holds it, the old binding keeps working.
            let mut out: *mut c_void = std::ptr::null_mut();
            let id = EventHotKeyId { signature: u32::from_be_bytes(*b"CDTA"), id: 1 };
            if RegisterEventHotKey(combo.key_code, combo.modifiers, id, target, 0, &mut out) != 0 {
                return false;
            }
            if registered.0 != 0 {
                UnregisterEventHotKey(registered.0 as *mut c_void);
            }
            *registered = (out as usize, combo.key_code, combo.modifiers);
        }
        true
    }
}

/// Bind the quick-ask hotkey to `combo`, or unbind it with `None`. False when it could not be registered (another app
/// holds the combination); a binding already in place is kept then.
#[cfg(target_os = "macos")]
pub fn set_hotkey(combo: Option<&Combo>) -> bool {
    hotkey::set(combo)
}

#[cfg(not(target_os = "macos"))]
pub fn set_hotkey(_combo: Option<&Combo>) -> bool {
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

#[cfg(target_os = "macos")]
#[link(name = "PDFKit", kind = "framework")]
unsafe extern "C" {}

/// Extract `(page_count, text)` from a PDF on macOS via Apple's native `PDFKit` (`PDFDocument`).
#[cfg(target_os = "macos")]
pub fn extract_pdf_text(path: &std::path::Path) -> Option<(usize, String)> {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    let path_str = path.to_str()?;
    autoreleasepool(|_| unsafe {
        let url_cls = AnyClass::get(c"NSURL")?;
        let pdf_cls = AnyClass::get(c"PDFDocument")?;
        let ns_path = NSString::from_str(path_str);
        let url: *mut AnyObject = msg_send![url_cls, fileURLWithPath: &*ns_path];
        if url.is_null() {
            return None;
        }
        let alloc: *mut AnyObject = msg_send![pdf_cls, alloc];
        if alloc.is_null() {
            return None;
        }
        let doc: *mut AnyObject = msg_send![alloc, initWithURL: url];
        if doc.is_null() {
            return None;
        }
        let pages: usize = msg_send![doc, pageCount];
        let raw_text: *const NSString = msg_send![doc, string];
        let text = if raw_text.is_null() { String::new() } else { (*raw_text).to_string() };
        let _: () = msg_send![doc, release];
        Some((pages, text))
    })
}

#[cfg(not(target_os = "macos"))]
pub fn extract_pdf_text(_path: &std::path::Path) -> Option<(usize, String)> {
    None
}

/// Convert in-memory raster image bytes (TIFF, BMP, HEIC, ICO, etc.) to PNG via AppKit `NSBitmapImageRep`.
#[cfg(target_os = "macos")]
pub fn image_bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};

    if bytes.is_empty() {
        return None;
    }
    /// `NSBitmapImageFileTypePNG` in AppKit.
    const FILE_TYPE_PNG: usize = 4;
    autoreleasepool(|_| unsafe {
        let data_cls = AnyClass::get(c"NSData")?;
        let rep_cls = AnyClass::get(c"NSBitmapImageRep")?;
        let dict_cls = AnyClass::get(c"NSDictionary")?;
        let ns_data: *mut AnyObject = msg_send![
            data_cls,
            dataWithBytes: bytes.as_ptr().cast::<c_void>(),
            length: bytes.len()
        ];
        if ns_data.is_null() {
            return None;
        }
        let rep: *mut AnyObject = msg_send![rep_cls, imageRepWithData: ns_data];
        if rep.is_null() {
            return None;
        }
        let empty_props: *mut AnyObject = msg_send![dict_cls, dictionary];
        let png_data: *mut AnyObject = msg_send![
            rep,
            representationUsingType: FILE_TYPE_PNG,
            properties: empty_props
        ];
        if png_data.is_null() {
            return None;
        }
        let len: usize = msg_send![png_data, length];
        let ptr: *const u8 = msg_send![png_data, bytes];
        if ptr.is_null() || len == 0 {
            return None;
        }
        Some(std::slice::from_raw_parts(ptr, len).to_vec())
    })
}

#[cfg(not(target_os = "macos"))]
pub fn image_bytes_to_png(_bytes: &[u8]) -> Option<Vec<u8>> {
    None
}

/// Fallback clipboard image reader for macOS screenshots / Preview / browser image copies that put
/// `public.tiff` or `public.heic` on `NSPasteboard` without plain text.
#[cfg(target_os = "macos")]
pub fn pasteboard_image_png() -> Option<Vec<u8>> {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    autoreleasepool(|_| unsafe {
        let pb_cls = AnyClass::get(c"NSPasteboard")?;
        let pb: *mut AnyObject = msg_send![pb_cls, generalPasteboard];
        if pb.is_null() {
            return None;
        }
        // Never intercept plain text pastes.
        let utf8_ut = NSString::from_str("public.utf8-plain-text");
        let existing_str: *mut AnyObject = msg_send![pb, dataForType: &*utf8_ut];
        if !existing_str.is_null() {
            return None;
        }
        let png_ut = NSString::from_str("public.png");
        let png_data: *mut AnyObject = msg_send![pb, dataForType: &*png_ut];
        if !png_data.is_null() {
            let len: usize = msg_send![png_data, length];
            let ptr: *const u8 = msg_send![png_data, bytes];
            if !ptr.is_null() && len > 0 {
                return Some(std::slice::from_raw_parts(ptr, len).to_vec());
            }
        }
        for uti in ["public.tiff", "public.heic", "public.jpeg"] {
            let ns_uti = NSString::from_str(uti);
            let raw: *mut AnyObject = msg_send![pb, dataForType: &*ns_uti];
            if !raw.is_null() {
                let len: usize = msg_send![raw, length];
                let ptr: *const u8 = msg_send![raw, bytes];
                if !ptr.is_null() && len > 0 {
                    let bytes = std::slice::from_raw_parts(ptr, len);
                    if let Some(png) = image_bytes_to_png(bytes) {
                        return Some(png);
                    }
                }
            }
        }
        None
    })
}

#[cfg(not(target_os = "macos"))]
pub fn pasteboard_image_png() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_parse_to_carbon_codes_and_labels() {
        let c = Combo::parse("ctrl-alt-space").unwrap();
        assert_eq!((c.key_code, c.modifiers, c.label.as_str()), (49, CONTROL_KEY | OPTION_KEY, "⌃⌥Space"));
        assert_eq!(Combo::parse("Cmd-Shift-K").unwrap().label, "⇧⌘K");
        assert_eq!(Combo::parse("cmd-shift-k").unwrap().key_code, 40);
        assert_eq!(Combo::parse("f5").unwrap(), Combo { key_code: 96, modifiers: 0, label: "F5".into() });
        assert_eq!(Combo::parse("ctrl-1").unwrap().key_code, 18);
        assert_eq!(Combo::parse("ctrl--").unwrap().label, "⌃-");
        for preset in COMBO_PRESETS {
            assert!(Combo::parse(preset).is_ok(), "{preset}");
        }
    }

    #[test]
    fn combos_that_would_eat_typing_or_clash_are_refused() {
        assert!(Combo::parse("k").unwrap_err().contains("needs"));
        assert!(Combo::parse("shift-space").unwrap_err().contains("needs"));
        assert!(Combo::parse("cmd-space").unwrap_err().contains("Spotlight"));
        assert!(Combo::parse("hyper-space").unwrap_err().contains("not a modifier"));
        assert!(Combo::parse("ctrl-f13").unwrap_err().contains("not a key"));
        assert!(Combo::parse("").is_err());
    }
}
