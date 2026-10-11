//! What was captured, and what the model will be told about it.
//!
//! The checklist is the product here. A capture is *usually* partial — Accessibility off, no
//! selection, an app that has no title — and the alternative to showing a person a checklist is
//! showing them nothing while the model is given a half-empty context. So every field carries its own
//! state, and a person approving a capture is approving exactly what the list says.
//!
//! Masking happens here, in the client, before a preview is shown — not later, on the way out. That
//! is deliberate: a person who is asked to approve a window title should be looking at the same
//! string the agent will receive. A preview that shows the raw title and a payload that carries the
//! masked one would make the approval ceremony a lie.

use harness_client::DeckContext;

use crate::source::Frontmost;

/// Caps, mirroring `backend/os_context.py`: a window title is one line, a selection is at most a
/// paragraph, and an app name is a name. Both sides cap, because either side may be the only one that
/// looked.
pub const MAX_TITLE: usize = 400;
pub const MAX_SELECTION: usize = 8_000;
pub const MAX_APP_NAME: usize = 200;

/// One field's state, in the words a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldState {
    /// Read, and nothing in it looked like a secret.
    Captured,
    /// Read, masked, and the count of spans hidden — so "3 things were hidden here" is visible rather
    /// than left to a person to notice.
    Masked(usize),
    /// Not read, with the reason. Never silently absent.
    Omitted(String),
}

impl FieldState {
    /// The one-word state, as the preview draws it.
    pub fn label(&self) -> &'static str {
        match self {
            FieldState::Captured => "read",
            FieldState::Masked(_) => "masked",
            FieldState::Omitted(_) => "not read",
        }
    }
}

/// One line of the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    /// What will be sent, already masked. Empty when omitted — a state that is not read carries no
    /// value at all rather than a value with a caveat.
    pub value: String,
    pub state: FieldState,
}

/// A captured window, encoded and ready for the payload (`screenshot::capture_window` makes one).
#[derive(Clone, PartialEq, Eq)]
pub struct Shot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Shot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the bytes: a debug print of a capture must not become a copy of the screen in a log.
        write!(f, "Shot({}x{}, {} bytes)", self.width, self.height, self.jpeg.len())
    }
}

/// Everything the person is deciding about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub app_name: String,
    pub bundle_id: String,
    pub pid: i32,
    pub fields: Vec<Field>,
    /// The window's image, when the person added one. Previewed by its size; never masked, because
    /// pixels cannot be — the policy in front of the capture is the only defence an image has.
    pub shot: Option<Shot>,
    /// Whether this capture's image may go to a cloud model (`capture.screenshotCloud`). Off means the
    /// daemon may only hand it to a local one.
    pub cloud_ok: bool,
}

impl Preview {
    pub fn new(frontmost: &Frontmost) -> Self {
        Self {
            app_name: harness_client::redact::scrub(&frontmost.name).0.chars().take(MAX_APP_NAME).collect(),
            bundle_id: frontmost.bundle_id.clone(),
            pid: frontmost.pid,
            fields: Vec::new(),
            shot: None,
            cloud_ok: false,
        }
    }

    /// Record a screenshot, or why there is none. Taking another replaces the first: the checklist
    /// has one screenshot line, and the payload carries one image.
    pub fn screenshot(&mut self, result: Result<Shot, crate::source::Refusal>) {
        self.fields.retain(|field| field.label != "screenshot");
        match result {
            Ok(shot) => {
                let value = format!("{}×{} · {} KB", shot.width, shot.height, shot.jpeg.len().div_ceil(1024));
                self.fields.push(Field { label: "screenshot", value, state: FieldState::Captured });
                self.shot = Some(shot);
            }
            Err(refusal) => {
                self.shot = None;
                self.omit("screenshot", refusal.reason());
            }
        }
    }

    /// Record a title, masked and capped.
    pub fn title(&mut self, title: &str) {
        let (masked, redactions) = harness_client::redact::scrub(title);
        let value: String = masked.chars().take(MAX_TITLE).collect();
        self.fields.push(Field { label: "window title", value, state: state_of(redactions) });
    }

    /// Record a selection, masked and capped.
    pub fn selection(&mut self, selection: &str) {
        let (masked, redactions) = harness_client::redact::scrub(selection);
        let value: String = masked.chars().take(MAX_SELECTION).collect();
        self.fields.push(Field { label: "selected text", value, state: state_of(redactions) });
    }

    /// Record a field that was not read, and why.
    pub fn omit(&mut self, label: &'static str, reason: impl Into<String>) {
        self.fields.push(Field { label, value: String::new(), state: FieldState::Omitted(reason.into()) });
    }

    /// How many spans were masked across the whole capture.
    pub fn redactions(&self) -> usize {
        self.fields
            .iter()
            .map(|field| match field.state {
                FieldState::Masked(count) => count,
                _ => 0,
            })
            .sum()
    }

    /// What the agent would be told, in the words the checklist used — so the two cannot disagree.
    pub fn omissions(&self) -> Vec<String> {
        self.fields
            .iter()
            .filter_map(|field| match &field.state {
                FieldState::Omitted(reason) => Some(format!("{}: {reason}", field.label)),
                _ => None,
            })
            .collect()
    }

    /// The value of one field, for the wire.
    fn value_of(&self, label: &str) -> String {
        self.fields.iter().find(|field| field.label == label).map(|field| field.value.clone()).unwrap_or_default()
    }

    /// The payload for `SubmitTurn`. Empty fields stay empty: the daemon renders a block only for what
    /// is there, and an empty string would be indistinguishable from "read, and it was blank".
    pub fn to_context(&self, captured_ms: i64) -> DeckContext {
        DeckContext {
            app_name: self.app_name.clone(),
            app_bundle_id: self.bundle_id.clone(),
            app_pid: self.pid,
            window_title: self.value_of("window title"),
            selected_text: self.value_of("selected text"),
            client_redactions: self.redactions() as u32,
            omitted: self.omissions(),
            captured_ms,
            screenshot: self.shot.as_ref().map(|shot| shot.jpeg.clone()).unwrap_or_default(),
            screenshot_mime: if self.shot.is_some() { "image/jpeg".into() } else { String::new() },
            screenshot_width: self.shot.as_ref().map_or(0, |shot| shot.width),
            screenshot_height: self.shot.as_ref().map_or(0, |shot| shot.height),
            screenshot_cloud_ok: self.shot.is_some() && self.cloud_ok,
        }
    }
}

fn state_of(redactions: usize) -> FieldState {
    if redactions > 0 { FieldState::Masked(redactions) } else { FieldState::Captured }
}

/// The outcome of one capture: either a refusal with its reason, or a preview to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture {
    /// Allowed: what was read, ready to be previewed and sent.
    Captured(Preview),
    /// Refused, with the sentence a person reads. No fields at all, because there is nothing that was
    /// read — a refused capture must not carry a partial payload a caller could still send.
    Refused {
        reason: String,
        /// The app's name, when it was known before the refusal. A person who is told "1Password is
        /// never read" is being told something useful; one who is told "capture refused" is not.
        app: Option<String>,
        bundle_id: String,
    },
}

impl Capture {
    pub fn captured(preview: Preview) -> Self {
        Capture::Captured(preview)
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Capture::Refused { reason: reason.into(), app: None, bundle_id: String::new() }
    }

    /// A refusal that can name the app, used by callers that read the frontmost app first.
    pub fn refused_app(reason: impl Into<String>, frontmost: &Frontmost) -> Self {
        Capture::Refused { reason: reason.into(), app: Some(frontmost.name.clone()), bundle_id: frontmost.bundle_id.clone() }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Capture::Refused { reason, .. } => Some(reason),
            Capture::Captured(_) => None,
        }
    }

    /// The bundle id a person could allow from the deck, when this refusal is theirs to lift: a named
    /// app with a bundle-id-shaped id that is not on the never-read list. `None` for every refusal that
    /// is a rule rather than a preference — there is no button that can lift a rule.
    pub fn allowable(&self) -> Option<&str> {
        match self {
            Capture::Refused { bundle_id, app: Some(_), .. } if crate::policy::acceptable(bundle_id) && !crate::policy::is_never_read(bundle_id) => Some(bundle_id),
            _ => None,
        }
    }

    pub fn preview(&self) -> Option<&Preview> {
        match self {
            Capture::Captured(preview) => Some(preview),
            Capture::Refused { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontmost() -> Frontmost {
        Frontmost { name: "Safari".into(), bundle_id: "com.apple.Safari".into(), pid: 4321 }
    }

    #[test]
    fn a_title_that_looks_like_a_secret_is_masked_in_the_preview_not_later() {
        let mut preview = Preview::new(&frontmost());
        preview.title("deploy — AWS_KEY=AKIAIOSFODNN7EXAMPLE");
        let field = &preview.fields[0];
        assert!(!field.value.contains("AKIAIOSFODNN7EXAMPLE"), "the preview must show what will be sent: {}", field.value);
        assert!(matches!(field.state, FieldState::Masked(count) if count >= 1), "and that something was hidden: {:?}", field.state);
        assert_eq!(preview.redactions(), 1);
    }

    #[test]
    fn what_the_model_is_told_agrees_with_what_the_checklist_showed() {
        let mut preview = Preview::new(&frontmost());
        preview.title("Inbox — 3 unread");
        preview.selection("meeting at 4");
        preview.omit("selected text", "Accessibility is not granted, so this cannot be read");
        let context = preview.to_context(1_700_000_000_000);
        assert_eq!(context.app_name, "Safari");
        assert_eq!(context.app_bundle_id, "com.apple.Safari");
        assert_eq!(context.app_pid, 4321);
        assert_eq!(context.window_title, "Inbox — 3 unread");
        assert_eq!(context.selected_text, "meeting at 4", "the later read replaces the earlier omission");
        assert_eq!(context.client_redactions, 0);
        assert_eq!(context.captured_ms, 1_700_000_000_000);
    }

    #[test]
    fn a_field_that_was_not_read_carries_no_value_at_all() {
        let mut preview = Preview::new(&frontmost());
        preview.omit("window title", "Accessibility is not granted, so this cannot be read");
        assert_eq!(preview.fields[0].value, "", "nothing to send for a field that was not read");
        assert_eq!(preview.fields[0].state.label(), "not read");
        assert_eq!(preview.omissions(), vec!["window title: Accessibility is not granted, so this cannot be read".to_owned()]);
        let context = preview.to_context(0);
        assert!(context.window_title.is_empty(), "and the payload agrees");
        assert_eq!(context.omitted.len(), 1, "while still saying that it was not read");
    }

    #[test]
    fn an_empty_capture_is_not_the_same_as_a_refused_one() {
        // The distinction matters on the wire and in the UI: an allowed app that had nothing to say
        // still sends its name.
        let capture = Capture::captured(Preview::new(&frontmost()));
        assert!(capture.reason().is_none());
        assert_eq!(capture.preview().map(|preview| preview.app_name.as_str()), Some("Safari"));
        let refused = Capture::refused_app("never read", &frontmost());
        assert!(refused.reason().is_some());
        assert!(refused.preview().is_none(), "a refusal must not carry a payload a caller could still send");
    }

    #[test]
    fn long_fields_are_capped_and_the_cap_is_counted_in_characters_not_bytes() {
        // A cap counted in bytes would split a multi-byte character and produce invalid text.
        let mut preview = Preview::new(&frontmost());
        preview.title(&"é".repeat(MAX_TITLE + 50));
        assert_eq!(preview.fields[0].value.chars().count(), MAX_TITLE);
        preview.selection(&"x".repeat(MAX_SELECTION + 50));
        assert_eq!(preview.fields[1].value.chars().count(), MAX_SELECTION);
    }

    #[test]
    fn an_app_name_that_looks_like_a_secret_is_masked_too() {
        let mut frontmost = frontmost();
        frontmost.name = "token sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA".into();
        let preview = Preview::new(&frontmost);
        assert!(!preview.app_name.contains("sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA"), "the app name rides on the same payload: {}", preview.app_name);
    }
}
