//! What the person allowed the harness to see.
//!
//! This crate is the *client's* half of OS context, and the requirement it exists to satisfy is
//! stated by the person, not by a design document: **"I should be able to dictate what the agent can
//! see. I can't have it view my secrets."** Everything here is shaped by that sentence:
//!
//! * **Deny by default.** An empty allowlist captures nothing from any app. A capture is a thing a
//!   person turns on for an app they name, never a thing that is on until someone turns it off.
//! * **Some apps are never read, whatever the allowlist says.** A password manager's window is not
//!   context, and the list is not editable — it is a constant, mirrored in the daemon
//!   ([`policy::ALWAYS_DENY_BUNDLE_IDS`]) so a scripted gRPC client cannot route around it.
//! * **Masked before it leaves the process.** Titles and selections go through the same scrubber the
//!   payload is checked against, so the *preview* shows what would actually be sent — a person
//!   approving a masked title, not a title that will be masked later.
//! * **Partial success is the normal case.** Every field is independently fallible — Accessibility
//!   may be off, an app may not expose a selection, a title may be empty — so a capture is a list of
//!   what was and was not read ([`Preview`]) rather than an all-or-nothing payload.
//! * **Never in the background.** [`capture`] is called when a person asks; nothing here watches,
//!   polls, or runs on a tick.
//!
//! The macOS reads live behind [`source::CaptureSource`], so the policy, the masking and the
//! checklist are all tested in CI without a GUI session, a TCC prompt, or a second application.

#[cfg(target_os = "macos")]
pub mod macos;
pub mod fake;
pub mod policy;
pub mod preview;
#[cfg(target_os = "macos")]
pub mod screenshot;
pub mod source;

#[cfg(target_os = "macos")]
pub use macos::MacCaptureSource;
pub use policy::{ALWAYS_DENY_BUNDLE_IDS, Allowlist, Verdict};
pub use preview::{Capture, Field, FieldState, Preview, Shot};
pub use source::{CaptureSource, Frontmost, Refusal};

/// Read the OS context for one app, if the policy allows it at all.
///
/// The order is the design, and it is the order the platform makes possible:
///
/// 1. **Is the person even allowed to have this app read?** An app whose bundle id is denied, or one
///    that is not on the allowlist, is refused here — before a title is read, so a refusal costs
///    nothing and reveals nothing.
/// 2. **Is the OS telling us the field is not readable?** Secure event input — a password field
///    focused anywhere — means the app is named and nothing else is read, which is what makes the
///    difference between "we asked and it was empty" and "we did not look".
/// 3. Title, then selection, each independently: one failing does not take the other with it.
/// 4. **Mask**, and count the spans, so the preview and the audit row agree on what was hidden.
pub fn capture(source: &dyn CaptureSource, allowlist: &Allowlist) -> Capture {
    let Some(frontmost) = source.frontmost() else {
        // An app switch in flight: the frontmost application is briefly unknown. Not a refusal —
        // there was nothing to read — so it is reported as an omission rather than as a denial.
        return Capture::refused("the frontmost app could not be read (an app switch in flight)");
    };
    capture_app(source, &frontmost, allowlist)
}

/// [`capture`] for a named app rather than whichever one is frontmost now.
///
/// This is the call the deck makes. By the time a person clicks "Capture", the deck itself has the
/// keyboard — the app they were looking at is the one that was frontmost when they *summoned* it,
/// which the deck records then (the app's identity costs no permission). Accessibility answers for an
/// app that is not frontmost: its focused window and its focused element keep their state, selection
/// included, while another app has the keyboard.
pub fn capture_app(source: &dyn CaptureSource, frontmost: &Frontmost, allowlist: &Allowlist) -> Capture {
    if let Verdict::Refuse(reason) = policy::verdict(&frontmost.bundle_id, allowlist) {
        // Named, so the deck can say *which* app and offer to allow it when that is a person's call.
        return Capture::refused_app(reason, frontmost);
    }

    let mut preview = Preview::new(frontmost);

    if source.secure_input() {
        // The app is named; nothing else is read. A person looking at the deck sees why, instead of
        // an empty deck that looks like a bug.
        preview.omit("window title", "secure input is on — a password field has focus");
        preview.omit("selected text", "secure input is on — a password field has focus");
        return Capture::captured(preview);
    }

    match source.window_title(frontmost.pid) {
        Ok(title) => preview.title(&title),
        Err(refusal) => preview.omit("window title", refusal.reason()),
    }
    match source.selection(frontmost.pid) {
        Ok(selection) => preview.selection(&selection),
        Err(refusal) => preview.omit("selected text", refusal.reason()),
    }
    Capture::captured(preview)
}

#[cfg(test)]
mod tests {
    use crate::fake::FakeCaptureSource;
    use crate::preview::FieldState;
    use crate::source::Refusal;
    use crate::source::CaptureSource;
    use crate::{Allowlist, Capture, capture, capture_app};

    /// Safari, allowed, with a title and no selection — the ordinary capture.
    fn safari() -> FakeCaptureSource {
        FakeCaptureSource::new("Safari", "com.apple.Safari").with_title("Inbox — 3 unread")
    }

    fn allowed() -> Allowlist {
        Allowlist::of(["com.apple.Safari"])
    }

    fn preview_of(capture: &Capture) -> &crate::Preview {
        capture.preview().expect("a capture that was allowed")
    }

    #[test]
    fn an_app_that_is_not_allowed_is_refused_before_a_single_read() {
        let source = safari();
        let capture = capture(&source, &Allowlist::empty());
        assert!(capture.reason().is_some_and(|reason| reason.contains("nothing is allowed")), "{:?}", capture.reason());
        assert!(capture.preview().is_none(), "nothing was read, so there is nothing to send");
    }

    #[test]
    fn a_refusal_names_the_app_and_only_a_preference_can_be_lifted() {
        // Not on the list: the deck says which app, and offers the one-click "always allow".
        let refused = capture(&safari(), &Allowlist::empty());
        assert_eq!(refused.allowable(), Some("com.apple.Safari"), "{refused:?}");
        // On the never-read list: named, but there is no button that could lift a rule.
        let source = FakeCaptureSource::new("1Password", "com.1password.1password");
        let refused = capture(&source, &Allowlist::empty());
        assert_eq!(refused.allowable(), None, "{refused:?}");
        assert!(matches!(&refused, Capture::Refused { app: Some(app), .. } if app == "1Password"));
        // An app switch in flight has nothing to allow at all.
        assert_eq!(capture(&FakeCaptureSource::nothing_frontmost(), &allowed()).allowable(), None);
    }

    #[test]
    fn the_deck_reads_the_app_it_was_summoned_over_not_itself() {
        // The deck is frontmost when the person clicks Capture; the read is for the app recorded at
        // summon time, by pid, and the deck's own identity never enters it.
        let deck = FakeCaptureSource::new("harness-menubar", "dev.sierralab.menubar").with_selection("not this");
        let target = crate::Frontmost { name: "Safari".into(), bundle_id: "com.apple.Safari".into(), pid: deck.frontmost().map(|f| f.pid).unwrap_or(1) };
        let capture = capture_app(&deck, &target, &allowed());
        let preview = preview_of(&capture);
        assert_eq!(preview.app_name, "Safari");
        assert_eq!(preview.bundle_id, "com.apple.Safari");
    }

    #[test]
    fn a_never_read_app_is_refused_however_the_settings_look() {
        let source = FakeCaptureSource::new("1Password", "com.1password.1password").with_title("Vault — 200 items");
        let capture = capture(&source, &Allowlist::of(["com.1password.1password"]));
        assert!(capture.reason().is_some_and(|reason| reason.contains("never read")));
        assert!(capture.preview().is_none(), "its title was never read into a preview");
    }

    #[test]
    fn secure_input_names_the_app_and_reads_nothing_else() {
        // A password field has focus somewhere. The person is told which app, and the title and the
        // selection are both omitted with the reason — not read and then dropped.
        let source = safari().with_secure_input(true).with_selection("hunter2");
        let capture = capture(&source, &allowed());
        let preview = preview_of(&capture);
        assert_eq!(preview.app_name, "Safari", "the app is still named");
        assert!(preview.fields.iter().all(|field| matches!(field.state, FieldState::Omitted(_))), "and nothing else was read: {:?}", preview.fields);
        assert_eq!(preview.fields.len(), 2, "both fields are accounted for");
        assert!(preview.omissions().iter().all(|omission| omission.contains("secure input")), "{:?}", preview.omissions());
        let context = preview.to_context(0);
        assert!(context.window_title.is_empty() && context.selected_text.is_empty());
        assert!(!context.omitted.is_empty(), "the daemon is told what was skipped, so it can say so");
    }

    #[test]
    fn one_field_failing_does_not_take_the_other_with_it() {
        // The ordinary case: Accessibility is refused for the selection but the title came through —
        // or the reverse. A capture that gave up on the first failure would send nothing at all.
        let source = safari().refusing_selection(Refusal::NotPermitted);
        let capture = capture(&source, &allowed());
        let preview = preview_of(&capture);
        assert_eq!(preview.fields.len(), 2);
        assert_eq!(preview.fields[0].value, "Inbox — 3 unread", "the title was read");
        assert_eq!(preview.fields[0].state, FieldState::Captured);
        assert!(matches!(preview.fields[1].state, FieldState::Omitted(_)), "and the selection says why not");
        assert!(preview.fields[1].value.is_empty());
    }

    #[test]
    fn a_title_read_refuses_nothing_but_a_selection_read_still_happens() {
        let source = safari().refusing_title(Refusal::NotPermitted).with_selection("a line I highlighted");
        let capture = capture(&source, &allowed());
        let preview = preview_of(&capture);
        assert!(matches!(preview.fields[0].state, FieldState::Omitted(_)));
        assert_eq!(preview.fields[1].value, "a line I highlighted", "the second read is not skipped because the first failed");
    }

    #[test]
    fn an_app_switch_in_flight_is_an_omission_not_a_refusal() {
        // Nothing was denied and nothing was read; saying "refused" would be a lie about the person's
        // settings, and a person would go looking for a setting that is not the problem.
        let capture = capture(&FakeCaptureSource::nothing_frontmost(), &allowed());
        assert!(capture.reason().is_some_and(|reason| reason.contains("app switch")), "{:?}", capture.reason());
        assert!(capture.preview().is_none());
    }

    #[test]
    fn a_secret_shaped_title_reaches_the_preview_already_masked() {
        let source = safari().with_title("Terminal — export OPENAI_API_KEY=sk-proj-AAAAAAAAAAAAAAAAAAAA");
        let capture = capture(&source, &allowed());
        let preview = preview_of(&capture);
        assert!(!preview.fields[0].value.contains("sk-proj-AAAAAAAAAAAAAAAAAAAA"), "{}", preview.fields[0].value);
        assert!(preview.redactions() >= 1);
        let context = preview.to_context(0);
        assert_eq!(context.client_redactions, preview.redactions() as u32, "the count the daemon audits is the count the preview showed");
        assert!(!context.window_title.contains("sk-proj-AAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn the_capture_is_one_call_and_reads_nothing_it_was_not_asked_for() {
        // `capture` takes no session, no state and no callback: it reads what it is asked for, once.
        // There is no path here that watches, polls or remembers, which is what "never in the
        // background" means in code.
        let capture = capture(&safari(), &allowed());
        assert_eq!(preview_of(&capture).fields.len(), 2, "exactly the two text fields, no more");
        assert_eq!(preview_of(&capture).to_context(0).screenshot.len(), 0, "and no image unless one was added");
    }

    #[test]
    fn a_screenshot_is_a_checklist_line_and_rides_the_payload_with_its_cloud_answer() {
        let mut preview = preview_of(&capture(&safari(), &allowed())).clone();
        preview.screenshot(Err(Refusal::ScreenRecording));
        assert!(preview.omissions().iter().any(|o| o.starts_with("screenshot: Screen Recording")), "{:?}", preview.omissions());
        assert!(preview.to_context(0).screenshot.is_empty());

        preview.screenshot(Ok(crate::Shot { jpeg: vec![0xFF, 0xD8, 0xFF, 1, 2], width: 1600, height: 1000 }));
        assert_eq!(preview.fields.iter().filter(|field| field.label == "screenshot").count(), 1, "retaking replaces the line");
        assert!(!preview.omissions().iter().any(|o| o.starts_with("screenshot")), "and the old refusal with it");
        let context = preview.to_context(0);
        assert_eq!((context.screenshot.len(), context.screenshot_mime.as_str(), context.screenshot_width), (5, "image/jpeg", 1600));
        assert!(!context.screenshot_cloud_ok, "local only unless the person said otherwise");
        preview.cloud_ok = true;
        assert!(preview.to_context(0).screenshot_cloud_ok);
        assert_eq!(format!("{:?}", preview.shot.as_ref().unwrap()), "Shot(1600x1000, 5 bytes)", "a debug print never carries the pixels");
    }
}
