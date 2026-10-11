//! A capture source a test can hold by the hand.
//!
//! Every rule in this crate is about what happens when the OS says no — no Accessibility, no
//! selection, secure input, an app switch in flight — and none of that can be arranged on a real
//! machine from a test. So the fake answers each question the way a test tells it to.

use std::cell::RefCell;

use crate::source::{CaptureSource, Frontmost, Refusal};

/// Answers, one field at a time. `None` for the frontmost app means an app switch in flight.
pub struct FakeCaptureSource {
    frontmost: Option<Frontmost>,
    secure: bool,
    title: RefCell<Result<String, Refusal>>,
    selection: RefCell<Result<String, Refusal>>,
}

impl FakeCaptureSource {
    /// A readable app: a title, no selection. The ordinary case, and the one every test starts from.
    pub fn new(name: &str, bundle_id: &str) -> Self {
        Self {
            frontmost: Some(Frontmost { name: name.into(), bundle_id: bundle_id.into(), pid: 4242 }),
            secure: false,
            title: RefCell::new(Ok("Untitled".into())),
            selection: RefCell::new(Err(Refusal::Nothing)),
        }
    }

    /// No app could be read at all.
    pub fn nothing_frontmost() -> Self {
        Self { frontmost: None, secure: false, title: RefCell::new(Err(Refusal::Nothing)), selection: RefCell::new(Err(Refusal::Nothing)) }
    }

    pub fn with_title(self, title: &str) -> Self {
        *self.title.borrow_mut() = Ok(title.into());
        self
    }

    pub fn with_selection(self, selection: &str) -> Self {
        *self.selection.borrow_mut() = Ok(selection.into());
        self
    }

    pub fn refusing_title(self, refusal: Refusal) -> Self {
        *self.title.borrow_mut() = Err(refusal);
        self
    }

    pub fn refusing_selection(self, refusal: Refusal) -> Self {
        *self.selection.borrow_mut() = Err(refusal);
        self
    }

    pub fn with_secure_input(self, secure: bool) -> Self {
        Self { secure, ..self }
    }

    pub fn with_pid(self, pid: i32) -> Self {
        let frontmost = self.frontmost.map(|frontmost| Frontmost { pid, ..frontmost });
        Self { frontmost, ..self }
    }
}

impl CaptureSource for FakeCaptureSource {
    fn frontmost(&self) -> Option<Frontmost> {
        self.frontmost.clone()
    }

    fn secure_input(&self) -> bool {
        self.secure
    }

    fn window_title(&self, _pid: i32) -> Result<String, Refusal> {
        self.title.borrow().clone()
    }

    fn selection(&self, _pid: i32) -> Result<String, Refusal> {
        self.selection.borrow().clone()
    }
}
