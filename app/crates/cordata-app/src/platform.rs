//! The few macOS things GPUI does not reach: the system's accessibility display flags, and Liquid Glass behind the
//! window (docs/app.md §4b). Everything else is GPUI's.

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

/// Put Liquid Glass behind the window's content: an `NSGlassEffectView` added as a *sibling* below GPUI's view (the
/// place GPUI itself uses for its blur), never by reparenting GPUI's view. The window must be created `Transparent`;
/// whatever GPUI paints opaque covers the glass, so only the rail — painted translucent — shows it. Returns whether
/// glass went in (macOS 26+, and not with Reduce Transparency).
#[cfg(target_os = "macos")]
pub fn install_glass(window: &gpui::Window) -> bool {
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSGlassEffectView, NSGlassEffectViewStyle, NSView, NSWindowOrderingMode};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // The class is looked up before any typed call: messaging a class this macOS lacks is undefined behaviour.
    if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_none() {
        return false;
    }
    let Some(mtm) = objc2::MainThreadMarker::new() else { return false };
    let Ok(handle) = <gpui::Window as HasWindowHandle>::window_handle(window) else { return false };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return false };
    // SAFETY: the live NSView of the window being built, borrowed for these calls only (never retained or released
    // here); the glass class was checked above.
    unsafe {
        let view = &*(handle.ns_view.as_ptr() as *const NSView);
        let Some(container) = view.superview() else { return false };
        let glass = NSGlassEffectView::initWithFrame(<NSGlassEffectView as objc2::MainThreadOnly>::alloc(mtm), container.bounds());
        glass.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        glass.setStyle(NSGlassEffectViewStyle::Regular);
        container.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, Some(view));
    }
    true
}

#[cfg(not(target_os = "macos"))]
pub fn install_glass(_window: &gpui::Window) -> bool {
    false
}
