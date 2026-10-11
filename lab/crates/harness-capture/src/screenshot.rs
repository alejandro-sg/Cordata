//! The window screenshot: the most sensitive thing the deck can read, so the narrowest one.
//!
//! * **One window, never the screen.** The target app's frontmost normal window, found by window id,
//!   captured on its own (`SCContentFilter(desktopIndependentWindow:)`) — so a window *behind* it, a
//!   notification banner, or another app's panel never enters the pixels.
//! * **Screen Recording is its own permission.** [`screen_recording_granted`] asks without prompting;
//!   [`request_screen_recording`] shows the OS prompt and is only ever called from the deck's Grant
//!   button. A capture without the grant is refused here rather than handed to ScreenCaptureKit,
//!   which would prompt on its own schedule.
//! * **Bounded.** Longest side at most [`MAX_SIDE`] pixels, JPEG, at most [`MAX_BYTES`] — the quality
//!   steps down until it fits, and a window that cannot fit is refused rather than truncated.
//!
//! ScreenCaptureKit rather than `CGWindowListCreateImage`, which is obsoleted from macOS 15 on. Its
//! calls complete on a queue of their own, so [`capture_window`] blocks its caller until they do — it
//! must be called off the main thread (the deck runs it on a background task).
//!
//! Pixels cannot be masked. The defences for an image are the policy ones, and they sit in front of
//! this module: never a never-read app, never an app the person did not allow, never while secure input
//! is on, and never to a cloud model unless the person opted in (`capture.screenshotCloud`).

#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFRelease, CFRetain, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::data::{CFDataCreateMutable, CFDataGetBytePtr, CFDataGetLength, CFMutableDataRef};
use core_foundation_sys::dictionary::{CFDictionaryCreate, CFDictionaryGetValue, CFDictionaryRef, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks};
use core_foundation_sys::number::{CFNumberCreate, CFNumberGetValue, CFNumberRef, kCFNumberDoubleType, kCFNumberSInt64Type};
use core_foundation_sys::string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8};
use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2_core_graphics::CGImage;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow};

pub use crate::preview::Shot;
use crate::source::Refusal;

/// The longest side a screenshot may have, in pixels. Enough to read code in an editor window; a
/// 5K window is scaled down by ScreenCaptureKit itself rather than by this process.
pub const MAX_SIDE: usize = 2048;
/// The most a screenshot may weigh: well inside the 16 MiB message limit on both ends of the bridge.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// How long one ScreenCaptureKit call may take before the capture is abandoned.
const WAIT: Duration = Duration::from_secs(3);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    static kCGWindowNumber: CFStringRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowLayer: CFStringRef;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithData(data: CFMutableDataRef, kind: CFStringRef, count: usize, options: CFDictionaryRef) -> *mut c_void;
    fn CGImageDestinationAddImage(destination: *mut c_void, image: *const c_void, properties: CFDictionaryRef);
    fn CGImageDestinationFinalize(destination: *mut c_void) -> bool;
    static kCGImageDestinationLossyCompressionQuality: CFStringRef;
}

const kCGWindowListOptionOnScreenOnly: u32 = 1 << 0;
const kCGWindowListExcludeDesktopElements: u32 = 1 << 4;

/// Has this process been granted Screen Recording? Never prompts.
pub fn screen_recording_granted() -> bool {
    // SAFETY: a parameterless query.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Show the OS's own Screen Recording prompt (or open the setting, once it has been declined) and say
/// whether access is granted. Only from the deck's Grant button.
pub fn request_screen_recording() -> bool {
    // SAFETY: a parameterless request; the OS owns the prompt.
    unsafe { CGRequestScreenCaptureAccess() }
}

/// The pixel size a window of `width`×`height` points is captured at: its backing scale, then scaled
/// down so the longest side is at most [`MAX_SIDE`]. Pure, so the arithmetic is tested.
pub fn fit(width: f64, height: f64, scale: f64) -> (usize, usize) {
    let (w, h) = ((width * scale).max(1.0), (height * scale).max(1.0));
    let longest = w.max(h);
    let factor = if longest > MAX_SIDE as f64 { MAX_SIDE as f64 / longest } else { 1.0 };
    (((w * factor).round() as usize).max(1), ((h * factor).round() as usize).max(1))
}

/// Capture `pid`'s frontmost normal window. Blocks until ScreenCaptureKit answers (at most a few
/// seconds); call it off the main thread.
pub fn capture_window(pid: i32) -> Result<Shot, Refusal> {
    if !screen_recording_granted() {
        return Err(Refusal::ScreenRecording);
    }
    if objc2::runtime::AnyClass::get(c"SCScreenshotManager").is_none() {
        // macOS 13: ScreenCaptureKit exists but cannot take a single screenshot.
        return Err(Refusal::Unsupported);
    }
    let window_id = frontmost_window_id(pid).ok_or(Refusal::Nothing)?;
    let content = shareable_content().ok_or(Refusal::Failed)?;
    // SAFETY: `windows` returns a retained array of `SCWindow`s owned by `content`, read here only.
    let windows = unsafe { content.0.windows() };
    let window: Retained<SCWindow> = windows.iter().find(|window| unsafe { window.windowID() } == window_id).ok_or(Refusal::Nothing)?;

    // SAFETY: a filter over one window the system just listed; the frame is a plain struct read.
    let filter = unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) };
    let frame = unsafe { window.frame() };
    let scale = unsafe { filter.pointPixelScale() } as f64;
    let (width, height) = fit(frame.size.width, frame.size.height, if scale > 0.0 { scale } else { 2.0 });
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setShowsCursor(false);
        config.setIgnoreShadowsSingleWindow(true);
    }

    let image = screenshot(&filter, &config).ok_or(Refusal::Failed)?;
    let encoded = encode_jpeg(image.0 as *const c_void);
    // SAFETY: `image` holds the one reference retained in the completion handler.
    unsafe { CFRelease(image.0 as CFTypeRef) };
    let jpeg = encoded.ok_or(Refusal::Failed)?;
    Ok(Shot { jpeg, width: width as u32, height: height as u32 })
}

/// Encode a `CGImage` as JPEG, stepping the quality down until it fits in [`MAX_BYTES`]. `None` when
/// even the lowest step does not fit, or ImageIO refuses.
///
/// `image` must be a live `CGImage`.
pub fn encode_jpeg(image: *const c_void) -> Option<Vec<u8>> {
    for quality in [0.8, 0.65, 0.5, 0.35] {
        let bytes = encode_at(image, quality)?;
        if bytes.len() <= MAX_BYTES {
            return Some(bytes);
        }
    }
    None
}

fn encode_at(image: *const c_void, quality: f64) -> Option<Vec<u8>> {
    // SAFETY: every CF object created here is released before returning; `image` is borrowed.
    unsafe {
        let data = CFDataCreateMutable(kCFAllocatorDefault, 0);
        if data.is_null() {
            return None;
        }
        let kind = CFStringCreateWithCString(kCFAllocatorDefault, c"public.jpeg".as_ptr(), kCFStringEncodingUTF8);
        let destination = CGImageDestinationCreateWithData(data, kind, 1, std::ptr::null());
        CFRelease(kind as CFTypeRef);
        if destination.is_null() {
            CFRelease(data as CFTypeRef);
            return None;
        }
        let number = CFNumberCreate(kCFAllocatorDefault, kCFNumberDoubleType, &quality as *const f64 as *const c_void);
        let keys = [kCGImageDestinationLossyCompressionQuality as *const c_void];
        let values = [number as *const c_void];
        let properties = CFDictionaryCreate(kCFAllocatorDefault, keys.as_ptr(), values.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFRelease(number as CFTypeRef);
        CGImageDestinationAddImage(destination, image, properties);
        CFRelease(properties as CFTypeRef);
        let finished = CGImageDestinationFinalize(destination);
        CFRelease(destination as CFTypeRef);
        let bytes = finished.then(|| std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize).to_vec());
        CFRelease(data as CFTypeRef);
        bytes
    }
}

/// The id of `pid`'s frontmost normal (layer 0) on-screen window. The window server's list is in
/// front-to-back order, so the first match is the one on top. No permission is needed for ids, pids
/// and layers — only window *names* are withheld without Screen Recording.
fn frontmost_window_id(pid: i32) -> Option<u32> {
    // SAFETY: the copied array is released before returning; values are borrowed from it.
    unsafe {
        let list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, 0);
        if list.is_null() {
            return None;
        }
        let mut found = None;
        for index in 0..CFArrayGetCount(list) {
            let info = CFArrayGetValueAtIndex(list, index) as CFDictionaryRef;
            let owner = number_of(info, kCGWindowOwnerPID);
            let layer = number_of(info, kCGWindowLayer);
            if owner == Some(pid as i64) && layer == Some(0) {
                found = number_of(info, kCGWindowNumber).map(|id| id as u32);
                break;
            }
        }
        CFRelease(list as CFTypeRef);
        found
    }
}

/// One integer out of a window-info dictionary.
///
/// # Safety
/// `info` must be a live `CFDictionary` and `key` a live `CFString`.
unsafe fn number_of(info: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    let value = unsafe { CFDictionaryGetValue(info, key as *const c_void) } as CFNumberRef;
    if value.is_null() {
        return None;
    }
    let mut out: i64 = 0;
    let ok = unsafe { CFNumberGetValue(value, kCFNumberSInt64Type, &mut out as *mut i64 as *mut c_void) };
    ok.then_some(out)
}

/// A value handed from ScreenCaptureKit's completion queue back to the waiting thread. The objects are
/// only touched by one thread at a time — the completion handler stores, the caller reads after `recv`.
struct Handoff<T>(T);
// SAFETY: see above; the channel is the only path between the two threads.
unsafe impl<T> Send for Handoff<T> {}

fn shareable_content() -> Option<Handoff<Retained<SCShareableContent>>> {
    let (send, receive) = mpsc::sync_channel::<Option<Handoff<Retained<SCShareableContent>>>>(1);
    let handler = RcBlock::new(move |content: *mut SCShareableContent, _error: *mut NSError| {
        // SAFETY: a non-null content object is retained for the waiting thread; the handler's own
        // reference is ScreenCaptureKit's to release.
        let content = unsafe { Retained::retain(content) }.map(Handoff);
        let _ = send.try_send(content);
    });
    // SAFETY: the handler outlives the call (ScreenCaptureKit copies the block).
    unsafe { SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, true, &handler) };
    receive.recv_timeout(WAIT).ok().flatten()
}

fn screenshot(filter: &SCContentFilter, config: &SCStreamConfiguration) -> Option<Handoff<*mut CGImage>> {
    let (send, receive) = mpsc::sync_channel::<Option<Handoff<*mut CGImage>>>(1);
    let handler = RcBlock::new(move |image: *mut CGImage, _error: *mut NSError| {
        let image = (!image.is_null()).then(|| {
            // SAFETY: the image is only valid for the handler's duration unless retained; the waiting
            // thread releases this reference once it has encoded it.
            unsafe { CFRetain(image as CFTypeRef) };
            Handoff(image)
        });
        let _ = send.try_send(image);
    });
    // SAFETY: filter and configuration are live for the call; the block is copied by the callee.
    unsafe { SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(filter, config, Some(&handler)) };
    // Timed out: if the handler fires later it retains an image nobody releases — a leak of one frame,
    // accepted rather than a use-after-free.
    receive.recv_timeout(WAIT).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGBitmapContextCreate(data: *mut c_void, width: usize, height: usize, bits: usize, row: usize, space: *mut c_void, info: u32) -> *mut c_void;
        fn CGBitmapContextCreateImage(context: *mut c_void) -> *mut c_void;
        fn CGContextSetRGBFillColor(context: *mut c_void, r: f64, g: f64, b: f64, a: f64);
        fn CGContextFillRect(context: *mut c_void, rect: Rect);
    }

    /// `CGRect`'s layout: origin then size, four doubles, passed by value.
    #[repr(C)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    /// A synthetic image, made without the window server — what the encoder is tested against.
    fn image(width: usize, height: usize) -> *mut c_void {
        unsafe {
            let space = CGColorSpaceCreateDeviceRGB();
            // kCGImageAlphaPremultipliedLast
            let context = CGBitmapContextCreate(std::ptr::null_mut(), width, height, 8, 0, space, 1);
            CGContextSetRGBFillColor(context, 0.2, 0.5, 0.8, 1.0);
            CGContextFillRect(context, Rect { x: 0.0, y: 0.0, width: width as f64 / 2.0, height: height as f64 });
            let image = CGBitmapContextCreateImage(context);
            CFRelease(context as CFTypeRef);
            CFRelease(space as CFTypeRef);
            image
        }
    }

    #[test]
    fn a_retina_window_is_captured_at_its_pixels_and_a_huge_one_is_scaled_down() {
        assert_eq!(fit(800.0, 600.0, 2.0), (1600, 1200), "within the cap: the window's own pixels");
        let (w, h) = fit(2560.0, 1440.0, 2.0);
        assert_eq!(w, MAX_SIDE, "the longest side is held to the cap");
        assert!((h as f64 - 2048.0 * 1440.0 / 2560.0).abs() <= 1.0, "and the aspect is kept: {w}x{h}");
        let (w, h) = fit(300.0, 3000.0, 2.0);
        assert_eq!(h, MAX_SIDE, "a tall window is capped on its height: {w}x{h}");
        assert!(fit(0.0, 0.0, 2.0).0 >= 1, "a degenerate frame never asks for a zero-pixel image");
    }

    #[test]
    fn an_image_encodes_to_a_bounded_jpeg() {
        let image = image(640, 400);
        let jpeg = encode_jpeg(image as *const c_void).expect("ImageIO encodes a plain bitmap");
        unsafe { CFRelease(image as CFTypeRef) };
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "a JPEG, by its own magic");
        assert!(jpeg.len() < MAX_BYTES && jpeg.len() > 100, "{} bytes", jpeg.len());
    }
}
