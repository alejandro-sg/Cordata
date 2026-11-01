//! The one-line notice the settings window shows under its rows (a taken hotkey, a saved key, a reset).

/// The latest notice, and whether it is an error.
#[derive(Default)]
pub struct SettingsView {
    pub notice: Option<String>,
    pub notice_error: bool,
}

impl SettingsView {
    pub fn set_notice(&mut self, text: String, error: bool) {
        self.notice = Some(text);
        self.notice_error = error;
    }
}
