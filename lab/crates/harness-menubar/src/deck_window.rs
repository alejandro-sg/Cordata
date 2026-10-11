//! The deck: the compact window that opens under the menu bar, and where it lands.
//!
//! Placement is a pure function ([`anchor`]) rather than something the window does, because it is
//! the part with real logic — a menu bar that is auto-hidden, a second display, a notch, an icon
//! near the right edge all move the icon, and the deck has to stay on screen and stay attached to
//! it. A test asserts the arithmetic; the window merely does what the arithmetic says.
//!
//! Two facts from the platform shape what follows, and both are verified rather than assumed:
//!
//! * **gpui's `WindowKind::PopUp` is not an `NSPanel`.** gpui 0.2.2 has no panel path, so the deck
//!   *will* take focus when it opens. That is accepted and documented rather than fought: a deck
//!   the person has to click into is worse than one that took the keyboard, and Esc always closes.
//! * **There is no public window-deactivation hook.** `activation_observers` is private, so
//!   dismissing on an outside click has to come from AppKit directly
//!   (`NSWindowDidResignKeyNotification`) — and a *global* event monitor is refused, because that
//!   would cost Accessibility, which is the permission the capture needs to spend only on screen
//!   content.

use gpui::{AnyWindowHandle, App, Bounds, Global, Point, Size, Window, point, px, size};

use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSGlassEffectView, NSGlassEffectViewStyle, NSScreen, NSView, NSWindowButton,
    NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// A rectangle in logical points, in the coordinate space of the display it is on. Kept as plain
/// data so the placement can be tested without a window server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Frame {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn center_x(&self) -> f32 {
        self.x + self.width / 2.0
    }
}

/// The gap between the status item and the deck, and the margin kept from any screen edge. Six
/// points reads as "attached to the icon"; eight points is the smallest margin that does not look
/// like the window is falling off the screen.
pub const GAP: f32 = 6.0;
pub const MARGIN: f32 = 8.0;

/// Where the deck's top-left corner goes, in the same logical space as `display`.
///
/// Under the icon and centred on it, then pulled back inside the display if that would hang it off
/// an edge. The y is clamped too: on a display whose menu bar is at the bottom, or a deck taller
/// than the space under the icon, the deck lands on screen rather than under the floor.
pub fn anchor(icon: Frame, display: Frame, deck: Size<f32>) -> Point<f32> {
    let centered = icon.center_x() - deck.width / 2.0;
    let x = centered.clamp(display.x + MARGIN, (display.right() - deck.width - MARGIN).max(display.x + MARGIN));
    let below = icon.bottom() + GAP;
    let y = below.clamp(display.y + MARGIN, (display.bottom() - deck.height - MARGIN).max(display.y + MARGIN));
    point(x, y)
}

/// Convert a rectangle AppKit reports in physical pixels into logical points. `scale` is the
/// display's backing scale factor (2.0 on a retina display), and the icon's y is measured from the
/// top of the display its menu bar is on.
pub fn physical_to_logical(rect: Frame, scale: f32) -> Frame {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    Frame { x: rect.x / scale, y: rect.y / scale, width: rect.width / scale, height: rect.height / scale }
}

/// The backing scale factor of the screen the tray is on, from AppKit rather than from a window:
/// the deck is placed before it exists, and gpui's display type carries no scale of its own.
fn screen_scale() -> f32 {
    let Some(mtm) = objc2::MainThreadMarker::new() else { return 1.0 };
    NSScreen::mainScreen(mtm).map(|screen| screen.backingScaleFactor() as f32).unwrap_or(1.0)
}

/// The deck's own window, as much as anything outside it needs to know: the handle to close it by,
/// and the address of its `NSView` to ask AppKit whether it is still the key window.
///
/// The pointer is an address rather than a reference because it is stored in a `Global` that
/// outlives the borrow it came from, and because it is only ever used while `handle` is `Some` —
/// which is exactly while the window, and therefore the view, is alive.
#[derive(Default)]
pub struct DeckSession {
    pub handle: Option<AnyWindowHandle>,
    view: Option<usize>,
    /// Whether this deck has been the key window at least once. Activation is asynchronous, so a deck
    /// that has not *yet* become key has not lost anything — and one opened by an interrupting request
    /// deliberately does not take the keyboard until the person clicks it.
    seen_key: bool,
}

impl Global for DeckSession {}

impl DeckSession {
    /// Remember a freshly opened deck.
    pub fn opened(&mut self, handle: AnyWindowHandle, view: Option<usize>) {
        self.handle = Some(handle);
        self.view = view;
        self.seen_key = false;
    }

    pub fn is_open(&self) -> bool {
        self.handle.is_some()
    }

    /// Is the deck still the key window? Polled from the app's tick, never observed: gpui keeps its
    /// deactivation observers private, and a *global* event monitor would cost Accessibility — the
    /// same permission the capture needs, spent on something Esc already does. A pointer whose
    /// window has been closed is never asked: `handle` goes with it.
    pub fn still_key(&self) -> bool {
        let Some(view) = self.view.filter(|_| self.handle.is_some()) else { return false };
        ViewPtr(view).is_key_window()
    }

    /// Has the deck had the keyboard and then lost it — the outside click? `false` until it has been
    /// key once, so a deck is never dismissed in the moment between opening and activating.
    pub fn lost_key(&mut self) -> bool {
        if self.handle.is_none() {
            return false;
        }
        let key = self.still_key();
        self.seen_key |= key;
        self.seen_key && !key
    }

    /// The deck window's chrome as AppKit reports it — traffic lights visible or not, and the
    /// appearance — for `--deck-selftest`, since none of it is visible to a headless test.
    pub fn chrome(&self) -> Option<String> {
        let view = self.view.filter(|_| self.handle.is_some())?;
        // SAFETY: as `is_key_window` — a live window's view, read only.
        unsafe {
            let view = &*(view as *const NSView);
            let window = view.window()?;
            let shown = [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
                .into_iter()
                .filter(|button| window.standardWindowButton(*button).is_some_and(|button| !button.isHidden()))
                .count();
            let appearance = window.appearance().map(|appearance| appearance.name().to_string()).unwrap_or_else(|| "system".into());
            Some(format!("traffic lights shown={shown} appearance={appearance}"))
        }
    }

    pub fn forget(&mut self) {
        self.handle = None;
        self.view = None;
        self.seen_key = false;
    }

    /// Close the deck, wherever it is being closed from: the view removing its own window on Esc, or
    /// the app closing it because something else took the keyboard.
    pub fn close(&mut self, cx: &mut App) {
        if let Some(handle) = self.handle.take() {
            self.view = None;
            self.seen_key = false;
            // A window already closing by its own hand fails here; that is not an error worth saying.
            let _ = handle.update(cx, |_view, window, _cx| window.remove_window());
        }
    }
}

/// The address of a window's `NSView`, for [`DeckSession::opened`]. `None` when the platform gives
/// something that is not AppKit, which on this app's only platform means the window simply cannot be
/// asked whether it is still key — dismissal then waits for Esc or the hotkey.
pub fn view_ptr_of(window: &Window) -> Option<usize> {
    ViewPtr::from_window(window).map(|ptr| ptr.0)
}

/// An `NSView` address. Only used to ask whether its window is key, and only while the window is up.
#[derive(Clone, Copy)]
struct ViewPtr(usize);

impl ViewPtr {
    fn from_window(window: &Window) -> Option<Self> {
        // Fully qualified: gpui's `Window` has an inherent `window_handle` of its own that would
        // shadow the trait method and give back an `AnyWindowHandle` instead of the raw one.
        let handle = <Window as HasWindowHandle>::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::AppKit(handle) => Some(Self(handle.ns_view.as_ptr() as usize)),
            _ => None,
        }
    }

    fn is_key_window(self) -> bool {
        // SAFETY: the address came from a live window's `NSView` and `DeckSession::close` clears it
        // in the same tick it stops being valid. `window()` and `isKeyWindow` are read-only reads of
        // AppKit state; the object is borrowed, never retained or released.
        unsafe {
            let view = &*(self.0 as *const NSView);
            view.window().is_some_and(|window| window.isKeyWindow())
        }
    }
}

/// What the deck is drawn over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// macOS 26's Liquid Glass: an `NSGlassEffectView` behind gpui's view, in a transparent window.
    Glass,
    /// gpui's own blur, where there is no glass: the window is `Blurred` and the palette more opaque.
    Blur,
}

impl Backdrop {
    /// Which backdrop this macOS can draw. The class is looked up at run time *before* any typed call:
    /// the bindings compile on every macOS, and messaging a class that does not exist is undefined
    /// behaviour, not an error.
    pub fn detect() -> Self {
        Self::for_class(objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_some())
    }

    /// The decision, with the lookup taken out — so both paths are tested on whichever macOS runs CI.
    pub fn for_class(present: bool) -> Self {
        if present { Backdrop::Glass } else { Backdrop::Blur }
    }

    /// `Transparent` over glass — gpui's blur would sit behind the glass and double it — and gpui's
    /// `Blurred` where there is none.
    pub fn appearance(self) -> gpui::WindowBackgroundAppearance {
        match self {
            Backdrop::Glass => gpui::WindowBackgroundAppearance::Transparent,
            Backdrop::Blur => gpui::WindowBackgroundAppearance::Blurred,
        }
    }

    /// The palette for this backdrop: one layout that reads as glass either way.
    pub fn theme(self, base: harness_lab::theme::Theme) -> harness_lab::theme::Theme {
        match self {
            Backdrop::Glass => base.glass(),
            Backdrop::Blur => base.solid(),
        }
    }
}

/// The deck's corner radius, shared by the glass and the panel drawn on it so their edges agree.
pub const RADIUS: f32 = 14.0;

/// The deck's window options: a non-resizable popover that takes focus, over whichever backdrop this
/// macOS has.
pub fn options(bounds: Bounds<gpui::Pixels>, backdrop: Backdrop) -> gpui::WindowOptions {
    gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        kind: gpui::WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        is_movable: false,
        window_background: backdrop.appearance(),
        // No title bar: gpui's default draws an opaque one. Without it the content runs edge to edge
        // under a transparent, hidden title — the traffic lights it still leaves are hidden by `dress`.
        titlebar: None,
        focus: true,
        show: true,
        ..Default::default()
    }
}

/// Make the deck's window read as a popover rather than a window: hide the three traffic lights
/// (gpui keeps a titled window's buttons, and a popover has nothing to close, minimise or zoom), and
/// pin it to the dark appearance — the deck's palette is dark, and both Liquid Glass and gpui's blur
/// follow the window's appearance, so in light mode they would draw a pale material under dark text.
pub fn dress(window: &Window) {
    let Some(view) = ViewPtr::from_window(window) else { return };
    // SAFETY: the window's live `NSView`, borrowed for these calls only; the buttons and the
    // appearance are AppKit's own objects, read and set, never released here.
    unsafe {
        let view = &*(view.0 as *const NSView);
        let Some(ns_window) = view.window() else { return };
        for button in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
            if let Some(button) = ns_window.standardWindowButton(button) {
                button.setHidden(true);
            }
        }
        if let Some(dark) = NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) {
            ns_window.setAppearance(Some(&dark));
        }
    }
}

/// Put Liquid Glass behind the deck: an `NSGlassEffectView` added as a *sibling* of gpui's view, below
/// it, in the same content view — the place gpui already uses for its own blur (`window.rs`).
/// Never by reparenting gpui's view into the glass's `contentView`: gpui manages that view's frame and
/// its blur sibling, and Apple promises no z-order for arbitrary subviews there. gpui's view is only
/// borrowed — never retained or released here. Returns whether the glass went in.
pub fn install_glass(window: &Window) -> bool {
    if Backdrop::detect() != Backdrop::Glass {
        return false;
    }
    let (Some(view), Some(mtm)) = (ViewPtr::from_window(window), objc2::MainThreadMarker::new()) else { return false };
    // SAFETY: the address is the live `NSView` of the window being built; it is borrowed for these
    // calls only. The class was checked above, so the typed glass calls message a real class.
    unsafe {
        let view = &*(view.0 as *const NSView);
        let Some(container) = view.superview() else { return false };
        let glass = NSGlassEffectView::initWithFrame(<NSGlassEffectView as objc2::MainThreadOnly>::alloc(mtm), container.bounds());
        glass.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        glass.setCornerRadius(RADIUS as f64);
        glass.setStyle(NSGlassEffectViewStyle::Regular);
        container.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, Some(view));
    }
    true
}

/// Where the deck goes, from the icon the tray reported (physical pixels, screen coordinates) and
/// the display it is on. Falls back to the display's top-right when no icon rect is known — a
/// hotkey press before the status item has ever been clicked.
pub fn bounds_for(cx: &App, icon: Option<Frame>) -> Bounds<gpui::Pixels> {
    let display = cx.primary_display().or_else(|| cx.displays().first().cloned());
    let bounds = display.map(|display| display.bounds()).unwrap_or(Bounds { origin: point(px(0.), px(0.)), size: gpui::size(px(1440.), px(900.)) });
    let scale = screen_scale();
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let origin_x: f32 = bounds.origin.x.into();
    let origin_y: f32 = bounds.origin.y.into();
    let local_icon = icon
        .map(|icon| physical_to_logical(icon, scale))
        .map(|icon| Frame { x: icon.x - origin_x, y: icon.y - origin_y, ..icon })
        // No known icon: the status item's usual corner, so the deck still appears where a person
        // would look for it rather than centred on the screen like a dialog.
        .unwrap_or(Frame { x: width - 40.0, y: 0.0, width: 22.0, height: 22.0 });
    let corner = anchor(local_icon, Frame { x: 0.0, y: 0.0, width, height }, size(crate::deck::DECK.0, crate::deck::DECK.1));
    Bounds {
        origin: point(bounds.origin.x + px(corner.x), bounds.origin.y + px(corner.y)),
        size: size(px(crate::deck::DECK.0), px(crate::deck::DECK.1)),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_backdrop_follows_the_class_and_each_has_its_window_and_palette() {
        // Both branches, whatever macOS runs this: the lookup is the only thing taken out.
        assert_eq!(Backdrop::for_class(true), Backdrop::Glass);
        assert_eq!(Backdrop::for_class(false), Backdrop::Blur);
        assert_eq!(Backdrop::Glass.appearance(), gpui::WindowBackgroundAppearance::Transparent, "no gpui blur under the glass");
        assert_eq!(Backdrop::Blur.appearance(), gpui::WindowBackgroundAppearance::Blurred);
        let base = harness_lab::theme::Theme::dark();
        assert!(Backdrop::Glass.theme(base).panel.a < Backdrop::Blur.theme(base).panel.a, "the glass palette lets the material through");
        let bounds = Bounds { origin: point(px(0.), px(0.)), size: size(px(420.), px(520.)) };
        assert_eq!(options(bounds, Backdrop::Blur).window_background, gpui::WindowBackgroundAppearance::Blurred);
        assert!(options(bounds, Backdrop::Glass).titlebar.is_none(), "a popover has no title bar");
    }

    use super::*;
    use gpui::size;

    /// A 1440×900 display with its menu bar on top — the ordinary case.
    fn display() -> Frame {
        Frame { x: 0.0, y: 0.0, width: 1440.0, height: 900.0 }
    }

    /// An icon of the usual size, sitting in the menu bar at the given centre.
    fn icon(center: f32) -> Frame {
        Frame { x: center - 11.0, y: 0.0, width: 22.0, height: 22.0 }
    }

    fn deck() -> Size<f32> {
        size(420.0, 520.0)
    }

    #[test]
    fn the_deck_hangs_under_the_icon_centred_on_it() {
        let placed = anchor(icon(1200.0), display(), deck());
        assert_eq!(placed.x, 1200.0 - 210.0, "centred on the icon");
        assert_eq!(placed.y, 22.0 + GAP, "attached below it, not overlapping");
    }

    #[test]
    fn an_icon_near_the_right_edge_keeps_the_deck_on_the_display() {
        // The rightmost icon on a 1440-wide display: centred, the deck would run 108 points off.
        let placed = anchor(icon(1429.0), display(), deck());
        assert_eq!(placed.x, 1440.0 - 420.0 - MARGIN, "pulled back inside the right edge");
        assert!(placed.x + 420.0 <= 1440.0, "and it is still on the display");
    }

    #[test]
    fn an_icon_near_the_left_edge_keeps_the_deck_on_the_display() {
        let placed = anchor(icon(4.0), display(), deck());
        assert_eq!(placed.x, MARGIN);
        assert!(placed.x >= 0.0);
    }

    #[test]
    fn a_display_to_the_right_of_the_first_is_respected() {
        // A second display's own origin is not 0: x is in that display's space, or the deck lands
        // on the other screen — which is the bug this test exists for.
        let second = Frame { x: 1440.0, y: 0.0, width: 1920.0, height: 1080.0 };
        let placed = anchor(icon(1700.0), second, deck());
        assert_eq!(placed.x, 1700.0 - 210.0, "centred on the icon in its own display's space");
        assert!(placed.x >= 1440.0 + MARGIN, "the deck must not be pulled onto the first display");
        let near_left = anchor(icon(1444.0), second, deck());
        assert_eq!(near_left.x, 1440.0 + MARGIN, "clamped to the display it belongs to");
    }

    #[test]
    fn a_deck_taller_than_the_space_left_lands_on_screen_anyway() {
        // A short display, or a menu bar at the bottom: below-the-icon would put it off the bottom.
        let short = Frame { x: 0.0, y: 0.0, width: 800.0, height: 400.0 };
        let placed = anchor(icon(400.0), short, deck());
        assert_eq!(placed.y, MARGIN, "clamped to the top margin so as much of the deck as possible is visible");
    }

    #[test]
    fn a_deck_larger_than_the_display_is_not_pushed_off_the_top() {
        // Degenerate but reachable (a small external display): clamp must not invert.
        let tiny = Frame { x: 0.0, y: 0.0, width: 300.0, height: 300.0 };
        let placed = anchor(icon(150.0), tiny, deck());
        assert_eq!(placed.x, MARGIN, "the top-left is the answer when nothing fits");
        assert_eq!(placed.y, MARGIN);
    }

    #[test]
    fn a_retina_icon_rect_is_halved_into_points() {
        // AppKit reports the icon at 2×: 44 physical pixels is a 22-point glyph.
        let physical = Frame { x: 2400.0, y: 0.0, width: 44.0, height: 44.0 };
        let logical = physical_to_logical(physical, 2.0);
        assert_eq!(logical, Frame { x: 1200.0, y: 0.0, width: 22.0, height: 22.0 });
        // A nonsense scale is treated as 1 rather than producing infinities.
        assert_eq!(physical_to_logical(physical, 0.0), physical);
    }
}
