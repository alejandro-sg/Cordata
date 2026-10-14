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
