//! What the terminal around Cordata can show without anyone looking at the conversation: the window or tab title,
//! with a spinner while a session works (multiplexers such as herdr and tmux read it), a progress indicator in the
//! terminals that draw OSC 9;4, and a desktop notification when a session needs the person.
//!
//! Every sequence is written only when what it shows changes. `CORDATA_TITLE=off` keeps the title alone;
//! `CORDATA_NOTIFY=off` silences the bell and notifications.

use crate::app::TabState;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// `OSC 9 ; text` (iTerm2, Ghostty, WezTerm).
    Osc9,
    /// `OSC 777 ; notify ; title ; body` (Ghostty, foot, urxvt).
    Osc777,
    Off,
}

/// What this terminal understands, read from the environment once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub title: bool,
    pub progress: bool,
    pub notify: Notify,
    pub bell: bool,
}

impl Caps {
    pub fn detect(env: impl Fn(&str) -> Option<String>) -> Self {
        let off = |name: &str| env(name).is_some_and(|v| v == "off");
        let program = env("TERM_PROGRAM").unwrap_or_default();
        // Inside tmux or screen these would need passthrough; inside herdr, herdr notifies on its own reports.
        let wrapped = env("TMUX").is_some() || env("STY").is_some() || env("HERDR_ENV").is_some_and(|v| v == "1");
        let quiet = off("CORDATA_NOTIFY");
        let notify = match program.as_str() {
            _ if quiet || wrapped => Notify::Off,
            "iTerm.app" | "WezTerm" => Notify::Osc9,
            "ghostty" => Notify::Osc777,
            _ => Notify::Off,
        };
        Self {
            title: !off("CORDATA_TITLE"),
            progress: !wrapped && (program == "ghostty" || env("WT_SESSION").is_some()),
            notify,
            bell: !quiet,
        }
    }
}

/// The state to show for several sessions at once: an approval anywhere wins, then work, then something unseen.
pub fn overall(states: &[TabState]) -> TabState {
    [TabState::NeedsApproval, TabState::Working, TabState::Done]
        .into_iter()
        .find(|wanted| states.contains(wanted))
        .unwrap_or(TabState::Idle)
}

/// Control characters would end the sequence early or start another one.
fn clean(text: &str, max: usize) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect::<String>().trim().to_owned()
}

pub fn title_text(state: TabState, spinner: usize, thread: &str) -> String {
    let thread = clean(thread, 60);
    let glyph = match state {
        TabState::Working => Some(SPINNER[spinner % SPINNER.len()]),
        TabState::NeedsApproval => Some('⚠'),
        TabState::Done => Some('✓'),
        TabState::Connecting | TabState::Idle | TabState::Closed => None,
    };
    let name = if thread.is_empty() || thread == "New thread" { "cordata".to_owned() } else { format!("{thread} · cordata") };
    match glyph {
        Some(g) => format!("{g} {name}"),
        None => name,
    }
}

pub fn title_sequence(text: &str) -> String {
    format!("\x1b]2;{text}\x07")
}

/// Save and restore the title the terminal had before (xterm's title stack).
pub const PUSH_TITLE: &str = "\x1b[22;0t";
pub const POP_TITLE: &str = "\x1b[23;0t";

pub fn progress_sequence(state: TabState) -> &'static str {
    match state {
        TabState::Working => "\x1b]9;4;3\x07",
        // Paused/warning: the session waits on the person.
        TabState::NeedsApproval => "\x1b]9;4;4;100\x07",
        _ => "\x1b]9;4;0\x07",
    }
}

pub fn notification(kind: Notify, title: &str, body: &str) -> Option<String> {
    let (title, body) = (clean(title, 60), clean(body, 200));
    match kind {
        Notify::Osc9 => Some(format!("\x1b]9;{title}: {body}\x07")),
        Notify::Osc777 => Some(format!("\x1b]777;notify;{};{}\x07", title.replace(';', ","), body.replace(';', ","))),
        Notify::Off => None,
    }
}

/// Remembers what was last written so each change is written once.
#[derive(Debug, Default)]
pub struct Shown {
    title: Option<String>,
    progress: Option<&'static str>,
}

impl Shown {
    /// The bytes that bring the terminal up to date with `state`; empty when nothing changed.
    pub fn update(&mut self, caps: &Caps, state: TabState, spinner: usize, thread: &str) -> String {
        let mut out = String::new();
        if caps.title {
            let text = title_text(state, spinner, thread);
            if self.title.as_deref() != Some(text.as_str()) {
                out.push_str(&title_sequence(&text));
                self.title = Some(text);
            }
        }
        if caps.progress {
            let sequence = progress_sequence(state);
            if self.progress != Some(sequence) {
                out.push_str(sequence);
                self.progress = Some(sequence);
            }
        }
        out
    }
}

/// What to write on the way in: the title the terminal had, saved.
pub fn enter(caps: &Caps) -> &'static str {
    if caps.title { PUSH_TITLE } else { "" }
}

/// What to write on the way out, a crash included: the progress bar cleared and the old title back.
pub fn restore(caps: &Caps) -> String {
    let mut out = String::new();
    if caps.progress {
        out.push_str(progress_sequence(TabState::Idle));
    }
    if caps.title {
        out.push_str(POP_TITLE);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(vars: &[(&str, &str)]) -> Caps {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Caps::detect(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    #[test]
    fn capabilities_follow_the_terminal_and_the_switches() {
        let ghostty = caps(&[("TERM_PROGRAM", "ghostty")]);
        assert!(ghostty.title && ghostty.progress && ghostty.bell);
        assert_eq!(ghostty.notify, Notify::Osc777);
        assert_eq!(caps(&[("TERM_PROGRAM", "iTerm.app")]).notify, Notify::Osc9);
        assert!(!caps(&[("TERM_PROGRAM", "iTerm.app")]).progress, "iTerm2 would show OSC 9;4 as a notification");
        let tmux = caps(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/x")]);
        assert!(!tmux.progress && tmux.notify == Notify::Off && tmux.title);
        assert_eq!(caps(&[("TERM_PROGRAM", "ghostty"), ("HERDR_ENV", "1")]).notify, Notify::Off);
        let quiet = caps(&[("TERM_PROGRAM", "iTerm.app"), ("CORDATA_NOTIFY", "off"), ("CORDATA_TITLE", "off")]);
        assert!(!quiet.bell && !quiet.title && quiet.notify == Notify::Off);
        assert!(caps(&[("WT_SESSION", "x")]).progress);
    }

    #[test]
    fn the_overall_state_puts_an_approval_first() {
        use TabState::*;
        assert_eq!(overall(&[Idle, Working, NeedsApproval]), NeedsApproval);
        assert_eq!(overall(&[Done, Working]), Working);
        assert_eq!(overall(&[Idle, Done]), Done);
        assert_eq!(overall(&[Idle, Closed, Connecting]), Idle);
    }

    #[test]
    fn the_title_spins_while_working_and_says_why_otherwise() {
        assert_eq!(title_text(TabState::Working, 0, "fix the parser"), "⠋ fix the parser · cordata");
        assert_eq!(title_text(TabState::Working, 11, "x"), "⠙ x · cordata");
        assert_eq!(title_text(TabState::NeedsApproval, 0, "New thread"), "⚠ cordata");
        assert_eq!(title_text(TabState::Done, 0, ""), "✓ cordata");
        assert_eq!(title_text(TabState::Idle, 0, "a\x1b]0;evil\x07b"), "a ]0;evil b · cordata", "no escape gets through");
    }

    #[test]
    fn each_change_is_written_once() {
        let caps = caps(&[("TERM_PROGRAM", "ghostty")]);
        let mut shown = Shown::default();
        let first = shown.update(&caps, TabState::Working, 0, "t");
        assert!(first.contains("\x1b]2;⠋ t · cordata\x07") && first.contains("\x1b]9;4;3\x07"));
        assert_eq!(shown.update(&caps, TabState::Working, 0, "t"), "");
        let next = shown.update(&caps, TabState::Working, 1, "t");
        assert!(next.contains('⠙') && !next.contains("9;4"), "the spinner moves, the progress bar stays");
        assert!(shown.update(&caps, TabState::Idle, 1, "t").contains("\x1b]9;4;0\x07"));
        assert_eq!(restore(&caps), format!("\x1b]9;4;0\x07{POP_TITLE}"));
        assert_eq!(enter(&caps), PUSH_TITLE);
        let iterm = super::Caps { title: false, ..caps };
        assert_eq!(restore(&Caps { progress: false, ..iterm }), "");
    }

    #[test]
    fn notifications_are_one_clean_sequence() {
        assert_eq!(notification(Notify::Osc9, "Cordata", "session 2 needs approval").unwrap(), "\x1b]9;Cordata: session 2 needs approval\x07");
        assert_eq!(notification(Notify::Osc777, "Cor;data", "a;b\x07c").unwrap(), "\x1b]777;notify;Cor,data;a,b c\x07");
        assert_eq!(notification(Notify::Off, "a", "b"), None);
    }
}
