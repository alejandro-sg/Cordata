//! The macOS reads, behind a trait so the policy can be tested without one.
//!
//! The trait exists for two reasons, and the second is the important one:
//!
//! * **CI has no GUI session.** Every rule in this crate — deny by default, the never-read list,
//!   masking, the partial-success checklist — is exercised against [`crate::fake::FakeCaptureSource`].
//! * **Each datum here costs a different permission.** The frontmost app is free; the window title
//!   and the selected text are Accessibility; a screenshot is Screen Recording and lives in its own
//!   phase. `CaptureSource` keeps the free reads from being taken down by a permissioned one: an app
//!   whose title cannot be read is still named, because naming it is a read that needs nothing.
//!
//! `active-win-pos-rs` is not used, and the reason is the permissions: on macOS it reads the window
//! title through `kCGWindowName`, which is Screen Recording. Accessibility gives the same string for
//! a permission that is *also* what the selection needs — so one permission covers both text fields
//! instead of two covering a field each.

/// The frontmost application. Free to read: no permission, and therefore the one thing that is
/// always available, which is why a refusal can always say *which app* was refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmost {
    pub name: String,
    pub bundle_id: String,
    pub pid: i32,
}

/// Why a field could not be read. Distinct from an empty string, which is a real answer: "the app has
/// no title" and "we were not allowed to look" are different sentences on the deck, and only one of
/// them is a permission a person can grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Accessibility is not granted for this process, or the app cannot be asked at all
    /// (`kAXErrorAPIDisabled` / `kAXErrorCannotComplete`).
    NotPermitted,
    /// The focused element is a secure text field. The app is named; nothing else is read, and the
    /// OS is the one saying so.
    Secure,
    /// The app is readable but exposes nothing for this question — some apps have no selection, and
    /// that is an answer rather than a failure.
    Nothing,
    /// Screen Recording is not granted, so no screenshot is taken. A permission of its own, separate
    /// from Accessibility, and the deck offers its own switch for it.
    ScreenRecording,
    /// This macOS cannot take a single-window screenshot (ScreenCaptureKit's screenshot API is 14+).
    Unsupported,
    /// The OS was asked and did not deliver — a timeout or a refused frame. Said, never retried behind
    /// the person's back.
    Failed,
}

impl Refusal {
    /// The reason as a person reads it, for the checklist and the audit row.
    pub fn reason(&self) -> &'static str {
        match self {
            Refusal::NotPermitted => "Accessibility is not granted, so this cannot be read",
            Refusal::Secure => "a secure field has focus, so this is never read",
            Refusal::Nothing => "this app has nothing here to read",
            Refusal::ScreenRecording => "Screen Recording is not granted, so no screenshot was taken",
            Refusal::Unsupported => "this macOS cannot capture a single window (needs macOS 14)",
            Refusal::Failed => "the screenshot did not arrive — try again",
        }
    }
}

/// The OS reads the capture needs, and nothing else.
///
/// No screenshot method: an image costs Screen Recording, which is a permission of its own, and it
/// belongs to the phase that can preview and audit it properly rather than to the trait that must
/// stay easy to fake.
pub trait CaptureSource {
    /// The frontmost application, or `None` during an app switch.
    fn frontmost(&self) -> Option<Frontmost>;

    /// Is secure event input on? True means a password field has focus *somewhere*, so no text is
    /// read from anywhere.
    fn secure_input(&self) -> bool;

    /// The focused window's title.
    fn window_title(&self, pid: i32) -> Result<String, Refusal>;

    /// The focused element's selected text.
    fn selection(&self, pid: i32) -> Result<String, Refusal>;
}
