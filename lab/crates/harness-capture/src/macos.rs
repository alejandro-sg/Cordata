//! The real reads, on macOS.
//!
//! Three permissions are at work here and they are not interchangeable, so the code is arranged by
//! which one each read costs:
//!
//! * **The frontmost application costs nothing.** No TCC prompt, no Accessibility, always available —
//!   which is why a refusal can always name the app, and why "we could not tell which app" is
//!   reported as an app switch in flight rather than as a denial.
//! * **The window title and the selection cost Accessibility.** Both come from the app's own
//!   accessibility tree. `kAXErrorAPIDisabled` is the OS saying this process is not trusted;
//!   `kAXErrorCannotComplete` is the app saying it is busy or will not answer. Both become
//!   [`Refusal::NotPermitted`], because from the person's side they are the same instruction: grant
//!   Accessibility to this app, and try again.
//! * **Secure input costs nothing and overrides everything.** `IsSecureEventInputEnabled` is the OS
//!   reporting that a password field has focus *anywhere* on the machine. [`MacCaptureSource`]
//!   reports it, and [`crate::capture`] then reads no text at all — the ordering is in the policy, so
//!   the answer does not depend on this file getting it right.
//!
//! The FFI constants keep Apple's own spelling (`kAXErrorSuccess`, not `KAXERRORSUCCESS`) — they are
//! what a person searching Apple's headers types — so the lint is off for this module.
//! The tree reads are raw FFI (`accessibility-sys`, i.e. `ApplicationServices`) rather than a wrapper
//! crate, because the *error code* is part of the product: it is what turns "no selected text" into
//! "Accessibility is not granted, here is the switch to flip".

#![allow(non_upper_case_globals)]

use accessibility_sys::{
    AXIsProcessTrusted, AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementRef, kAXErrorAPIDisabled, kAXErrorCannotComplete, kAXErrorNoValue,
    kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXFocusedWindowAttribute, kAXSelectedTextAttribute, kAXSubroleAttribute, kAXTitleAttribute, kAXSecureTextFieldSubrole,
};
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::string::{
    CFStringCreateWithCString, CFStringGetCString, CFStringGetLength, CFStringGetTypeID, kCFStringEncodingUTF8,
};
use objc2_app_kit::NSWorkspace;

use crate::source::{CaptureSource, Frontmost, Refusal};

// Is secure event input on? The OS's own answer about a focused password field, anywhere. Linked
// from Carbon rather than reimplemented: this is the only way to ask, and it needs no permission.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

/// The real source. Cheap to construct and holds nothing: every read is a fresh call, so a capture
/// can never be answering from an earlier look at another app.
#[derive(Debug, Default)]
pub struct MacCaptureSource;

impl MacCaptureSource {
    pub fn new() -> Self {
        Self
    }
}

impl CaptureSource for MacCaptureSource {
    fn frontmost(&self) -> Option<Frontmost> {
        // `frontmostApplication` hands back an autoreleased object that `Retained` holds for as long
        // as this scope; objc2 marks the call safe, and it reads AppKit state only.
        let application = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let name = application.localizedName().map(|name| name.to_string()).unwrap_or_default();
        let bundle_id = application.bundleIdentifier().map(|id| id.to_string()).unwrap_or_default();
        Some(Frontmost { name, bundle_id, pid: application.processIdentifier() })
    }

    fn secure_input(&self) -> bool {
        // SAFETY: a parameterless query of a global OS flag.
        unsafe { IsSecureEventInputEnabled() != 0 }
    }

    fn window_title(&self, pid: i32) -> Result<String, Refusal> {
        let application = AppElement::new(pid)?;
        let window = application.element(kAXFocusedWindowAttribute)?;
        window.string(kAXTitleAttribute)
    }

    fn selection(&self, pid: i32) -> Result<String, Refusal> {
        let application = AppElement::new(pid)?;
        let focused = application.element(kAXFocusedUIElementAttribute)?;
        // A secure field says so about itself, even when global secure input is off (a field that is
        // not focused reports the subrole without the global flag). Belt and braces on purpose: this
        // is the one read where being wrong is a password in a transcript.
        if focused.string(kAXSubroleAttribute).as_deref() == Ok(kAXSecureTextFieldSubrole) {
            return Err(Refusal::Secure);
        }
        focused.string(kAXSelectedTextAttribute)
    }
}

/// The application element and the `AXUIElement` lifetime rules, in one place.
///
/// `AXUIElementCreateApplication` follows the CF "create" rule — the caller owns the reference — and
/// every value copied out of the tree follows it too. Both are released here, so no read can leak,
/// and no reference outlives the element it came from.
struct AppElement(AXUIElementRef);

impl AppElement {
    fn new(pid: i32) -> Result<Self, Refusal> {
        // A process with no accessibility tree (a background daemon, a process that has quit between
        // the frontmost read and this call) answers `CannotComplete`, not a crash.
        let element = unsafe { AXUIElementCreateApplication(pid) };
        if element.is_null() {
            return Err(Refusal::NotPermitted);
        }
        Ok(Self(element))
    }

    /// One element-valued attribute: the focused window, the focused field.
    fn element(&self, attribute: &str) -> Result<Element, Refusal> {
        let raw = self.copy(attribute)?;
        // SAFETY: the attribute is one of the element-valued ones, so the copied reference is an
        // `AXUIElement`; ownership transfers to `Element`, which releases it once.
        Ok(Element(raw as AXUIElementRef))
    }

    fn copy(&self, attribute: &str) -> Result<CFTypeRef, Refusal> {
        let name = cf_string(attribute).ok_or(Refusal::NotPermitted)?;
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: `self.0` is a live application element for the length of this call, `name` is a
        // CFString, and `value` is a valid out-pointer. Any copied value is released by the caller.
        let status = unsafe { AXUIElementCopyAttributeValue(self.0, name, &mut value) };
        // The attribute name's own reference is ours (the "create" rule), so it goes back now.
        unsafe { CFRelease(name as CFTypeRef) };
        match status {
            kAXErrorSuccess if !value.is_null() => Ok(value),
            kAXErrorAPIDisabled | kAXErrorCannotComplete => Err(Refusal::NotPermitted),
            // No value, an unsupported attribute, or an element that went away between two calls:
            // the app has nothing here, which is an answer rather than a failure.
            _ => Err(Refusal::Nothing),
        }
    }
}

impl Drop for AppElement {
    fn drop(&mut self) {
        // SAFETY: created by `AXUIElementCreateApplication` and owned here, released once.
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// An element read out of the tree, released when it goes out of scope.
struct Element(AXUIElementRef);

impl Element {
    /// A string-valued attribute, as Rust text. `Err(Nothing)` covers both "empty" and "not a
    /// string", because for a title or a selection those are the same thing to a person.
    fn string(&self, attribute: &str) -> Result<String, Refusal> {
        let name = cf_string(attribute).ok_or(Refusal::NotPermitted)?;
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: as above — a live element, a CFString name, a valid out-pointer, and the copied
        // value released below.
        let status = unsafe { AXUIElementCopyAttributeValue(self.0, name, &mut value) };
        unsafe { CFRelease(name as CFTypeRef) };
        let value = match status {
            kAXErrorSuccess if !value.is_null() => value,
            kAXErrorAPIDisabled | kAXErrorCannotComplete => return Err(Refusal::NotPermitted),
            kAXErrorNoValue => return Err(Refusal::Nothing),
            _ => return Err(Refusal::Nothing),
        };
        // A focused field's selected text can be an AXValue (a range) rather than a string in some
        // apps. Anything that is not a string is `Nothing`: the fallback of reading the whole field
        // value is refused on purpose — that is the field's *contents*, which is not a selection.
        // SAFETY: a type query on a live CF object.
        let is_string = unsafe { CFGetTypeID(value) == CFStringGetTypeID() };
        if !is_string {
            unsafe { CFRelease(value) };
            return Err(Refusal::Nothing);
        }
        // SAFETY: the object is a CFString, read into a buffer that is sized from its own length.
        let text = unsafe { string_of(value) };
        unsafe { CFRelease(value) };
        match text {
            Some(text) if !text.trim().is_empty() => Ok(text),
            _ => Err(Refusal::Nothing),
        }
    }
}

impl Drop for Element {
    fn drop(&mut self) {
        // SAFETY: a copied reference owned here, released once.
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// Is this process trusted for Accessibility at all? Not used to decide anything — the per-attribute
/// error is what decides — but it is what turns "grant this in System Settings" into a switch the deck
/// can offer.
pub fn accessibility_granted() -> bool {
    // SAFETY: a parameterless query.
    unsafe { AXIsProcessTrusted() }
}

/// Ask the OS to show its own "allow Accessibility" prompt for this process, and say whether it is
/// already trusted. Called only from the deck's Grant button — never at launch, never on a tick — so
/// the prompt appears because a person asked for it, once, rather than nagging.
pub fn request_accessibility() -> bool {
    use accessibility_sys::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
    use core_foundation_sys::dictionary::{CFDictionaryCreate, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks};
    use core_foundation_sys::number::kCFBooleanTrue;
    // SAFETY: a one-entry dictionary of two immortal CF constants, created with the CF type callbacks
    // (so it retains what it holds) and released once after the query.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt as *const std::ffi::c_void];
        let values = [kCFBooleanTrue as *const std::ffi::c_void];
        let options = CFDictionaryCreate(kCFAllocatorDefault, keys.as_ptr(), values.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        if options.is_null() {
            return AXIsProcessTrusted();
        }
        let trusted = AXIsProcessTrustedWithOptions(options);
        CFRelease(options as CFTypeRef);
        trusted
    }
}

/// A CFString from a `&str`, owned by the caller, or `None` if the string has an interior NUL.
fn cf_string(text: &str) -> Option<core_foundation_sys::string::CFStringRef> {
    std::ffi::CString::new(text).ok().map(|text| {
        // SAFETY: a NUL-terminated C string with a known length and the UTF-8 encoding, allocated by
        // the default allocator; ownership is handed to the caller.
        unsafe { CFStringCreateWithCString(kCFAllocatorDefault, text.as_ptr(), kCFStringEncodingUTF8) }
    })
}

/// A CFString's contents as Rust text.
///
/// # Safety
/// `value` must be a live `CFString`.
unsafe fn string_of(value: CFTypeRef) -> Option<String> {
    let string = value as core_foundation_sys::string::CFStringRef;
    // SAFETY: a length query on a live CFString.
    let length = unsafe { CFStringGetLength(string) } as usize;
    if length == 0 {
        return Some(String::new());
    }
    // Room for the text plus a NUL, in the UTF-8 encoding the copy below uses.
    let mut buffer = vec![0u8; length * 4 + 1];
    // SAFETY: `buffer` is at least as large as the request, and `string` is live.
    let ok = unsafe {
        CFStringGetCString(string, buffer.as_mut_ptr() as *mut std::ffi::c_char, buffer.len() as isize, kCFStringEncodingUTF8)
    };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec()).ok()
}
