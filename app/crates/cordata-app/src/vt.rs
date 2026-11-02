//! The dock terminal's engine: a real PTY running your login shell, parsed by `alacritty_terminal` (the emulator Zed's
//! GPUI terminal uses), read back as rows of styled runs to draw, and keys encoded the way a terminal sends them. Full
//! screen programs (`claude`, `vim`, `htop`, `less`) run here as they do in any terminal.
//!
//! This is your shell, with your rights, like Terminal.app: the agent's sandbox is not involved and the agent never
//! types into it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Shell};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use tokio::sync::mpsc;

/// Hands the emulator's events to the window.
#[derive(Clone)]
pub struct Proxy(mpsc::UnboundedSender<Event>);

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// What to start: the person's login shell in the project for the dock, a fixed program in tests.
pub struct Launch<'a> {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: &'a Path,
    pub cols: u16,
    pub lines: u16,
}

impl<'a> Launch<'a> {
    /// `$SHELL -l` (zsh when unset) in `cwd`.
    pub fn login_shell(cwd: &'a Path) -> Self {
        let program = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/zsh".into());
        Self { program, args: vec!["-l".into()], cwd, cols: 80, lines: 24 }
    }
}

struct Inner {
    term: Arc<FairMutex<Term<Proxy>>>,
    sender: EventLoopSender,
    size: FairMutex<(u16, u16)>,
    /// A second handle on the PTY's controlling side, only to ask which process group is in the foreground.
    controller: std::fs::File,
}

/// One running terminal. Cloning shares it; `shutdown` ends the shell.
#[derive(Clone)]
pub struct Session(Arc<Inner>);

fn window_size(cols: u16, lines: u16, cell: (u16, u16)) -> WindowSize {
    WindowSize { num_lines: lines.max(1), num_cols: cols.max(2), cell_width: cell.0.max(1), cell_height: cell.1.max(1) }
}

impl Session {
    pub fn spawn(launch: Launch<'_>) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let proxy = Proxy(tx);
        let env: HashMap<String, String> = [
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("TERM_PROGRAM", "Cordata"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let options = tty::Options {
            shell: Some(Shell::new(launch.program, launch.args)),
            working_directory: Some(launch.cwd.to_path_buf()),
            drain_on_exit: true,
            env,
        };
        let size = window_size(launch.cols, launch.lines, (8, 16));
        let pty = tty::new(&options, size, 0)?;
        let controller = pty.file().try_clone()?;
        let dims = TermSize::new(size.num_cols as usize, size.num_lines as usize);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &dims, proxy.clone())));
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        let inner = Inner { term, sender, size: FairMutex::new((size.num_cols, size.num_lines)), controller };
        Ok((Self(Arc::new(inner)), rx))
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            // Typing goes to the live screen, not the history being read.
            self.0.term.lock().scroll_display(Scroll::Bottom);
            let _ = self.0.sender.send(Msg::Input(bytes));
        }
    }

    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.write(paste_bytes(text, bracketed));
    }

    pub fn mode(&self) -> TermMode {
        *self.0.term.lock().mode()
    }

    pub fn size(&self) -> (u16, u16) {
        *self.0.size.lock()
    }

    /// Fits the terminal to `cols` × `lines`; a no-op when that is its size already.
    pub fn resize(&self, cols: u16, lines: u16, cell: (u16, u16)) {
        let next = window_size(cols, lines, cell);
        {
            let mut size = self.0.size.lock();
            if *size == (next.num_cols, next.num_lines) {
                return;
            }
            *size = (next.num_cols, next.num_lines);
        }
        self.0.term.lock().resize(TermSize::new(next.num_cols as usize, next.num_lines as usize));
        let _ = self.0.sender.send(Msg::Resize(next));
    }

    /// Scrolls the history by `lines` (positive: back in time).
    pub fn scroll(&self, lines: i32) {
        self.0.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// The command line of the program in the terminal's foreground (the shell itself at its prompt).
    pub fn foreground(&self) -> Option<Vec<String>> {
        use std::os::fd::AsRawFd;
        // SAFETY: tcgetpgrp only reads the terminal's foreground group for a descriptor this session owns.
        let group = unsafe { libc::tcgetpgrp(self.0.controller.as_raw_fd()) };
        (group > 0).then(|| process_argv(group)).flatten()
    }

    /// The program's title (OSC 0/2) is kept by the window; the screen's rows as plain text, for detection and `read`.
    pub fn text(&self) -> Vec<String> {
        self.screen(&Palette::new(0, 0, 0)).rows.into_iter().map(|r| r.text).collect()
    }

    pub fn shutdown(&self) {
        let _ = self.0.sender.send(Msg::Shutdown);
    }

    pub fn screen(&self, palette: &Palette) -> Screen {
        screen(&self.0.term.lock(), palette)
    }
}

/// A process's argv: macOS keeps it behind `KERN_PROCARGS2` (argc, the executable path, padding, then the arguments).
#[cfg(target_os = "macos")]
pub fn process_argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query (null buffer), then a read into a buffer of that size; both only write what they are given.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(size);
        parse_procargs(&buf)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn process_argv(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    (!args.is_empty()).then_some(args)
}

/// `KERN_PROCARGS2`'s layout: a native-endian argc, the executable path, NUL padding, then argc NUL-ended arguments.
pub fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let rest = &buf[4..];
    let after_path = rest.iter().position(|b| *b == 0)?;
    let start = after_path + rest[after_path..].iter().position(|b| *b != 0)?;
    let args: Vec<String> = rest[start..]
        .split(|b| *b == 0)
        .take(usize::try_from(argc).ok()?)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

/// The colours a screen is drawn in: the theme's ground and ink, and the 16 ANSI colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub ansi: [u32; 16],
}

impl Palette {
    pub fn new(foreground: u32, background: u32, cursor: u32) -> Self {
        Self {
            foreground,
            background,
            cursor,
            ansi: [
                0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6, //
                0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
            ],
        }
    }

    /// The 16 ANSI colours darkened for a light ground, so yellow and cyan text stay readable on paper.
    pub fn for_light(mut self) -> Self {
        self.ansi = [
            0x2C2F33, 0xB3261E, 0x2E7D32, 0x8A6100, 0x2B5FA8, 0x8E3FA0, 0x0E7C86, 0x6B7178, //
            0x5A6068, 0xC9372C, 0x3B8F3F, 0x9C6D00, 0x3A6FC0, 0xA14DB5, 0x168F99, 0x2C2F33,
        ];
        self
    }

    /// 0–15 the palette, 16–231 the 6×6×6 cube, 232–255 the grey ramp (xterm's).
    pub fn indexed(&self, index: u8) -> u32 {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
                (level(i / 36) << 16) | (level((i / 6) % 6) << 8) | level(i % 6)
            }
            232..=255 => {
                let g = 8 + 10 * (index - 232) as u32;
                (g << 16) | (g << 8) | g
            }
        }
    }

    fn named(&self, color: NamedColor) -> u32 {
        match color {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::DimForeground => blend(self.foreground, self.background),
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            other => {
                let n = other as usize;
                if n < 16 {
                    self.ansi[n]
                } else {
                    // The dim ANSI colours (DimBlack … DimWhite) follow the cursor entries.
                    let base = n.saturating_sub(NamedColor::DimBlack as usize).min(7);
                    blend(self.ansi[base], self.background)
                }
            }
        }
    }

    /// A colour the program set (OSC 4/10/11) wins over the palette.
    pub fn resolve(&self, color: Color, set: &Colors) -> u32 {
        let rgb = |c: Rgb| ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32;
        match color {
            Color::Spec(c) => rgb(c),
            Color::Indexed(i) => set[i as usize].map(rgb).unwrap_or_else(|| self.indexed(i)),
            Color::Named(n) => set[n].map(rgb).unwrap_or_else(|| self.named(n)),
        }
    }
}

/// Halfway between two colours: dim text.
fn blend(a: u32, b: u32) -> u32 {
    let ch = |shift: u32| ((((a >> shift) & 0xff) + ((b >> shift) & 0xff)) / 2) << shift;
    ch(16) | ch(8) | ch(0)
}

/// A stretch of one row in one style. `range` is in bytes of the row's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub range: Range<usize>,
    pub fg: u32,
    /// `None`: the terminal's own background.
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub runs: Vec<Run>,
}

/// What is on screen right now, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub rows: Vec<Row>,
    /// (row, column) of the cursor when it is shown and in view.
    pub cursor: Option<(usize, usize)>,
    /// Lines scrolled back into the history (0: the live screen).
    pub scrolled: usize,
    pub mode: TermMode,
}

pub fn screen<T: EventListener>(term: &Term<T>, palette: &Palette) -> Screen {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let lines = term.screen_lines();
    let mut cells: Vec<Vec<Shown>> = vec![Vec::new(); lines];
    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        let Some(cells) = usize::try_from(row).ok().and_then(|r| cells.get_mut(r)) else { continue };
        let cell = indexed.cell;
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let (mut fg, mut bg) = (palette.resolve(cell.fg, content.colors), palette.resolve(cell.bg, content.colors));
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::DIM) {
            fg = blend(fg, bg);
        }
        let c = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
        let bg = (bg != palette.background).then_some(bg);
        cells.push((c, fg, bg, cell.flags));
    }
    let shown = content.mode.contains(TermMode::SHOW_CURSOR) && offset == 0;
    let point = content.cursor.point;
    let cursor = (shown && point.line.0 >= 0).then_some((point.line.0 as usize, point.column.0)).filter(|(r, _)| *r < lines);
    let rows = cells.into_iter().map(row_of).collect();
    Screen { rows, cursor, scrolled: content.display_offset, mode: content.mode }
}

/// One cell as drawn: its character, ink, background (`None`: the terminal's) and attributes.
type Shown = (char, u32, Option<u32>, Flags);

/// Merges neighbouring cells of one style into runs; trailing blanks with no background are dropped.
fn row_of(cells: Vec<Shown>) -> Row {
    let keep = cells.iter().rposition(|(c, _, bg, _)| *c != ' ' || bg.is_some()).map_or(0, |i| i + 1);
    let mut row = Row::default();
    for (c, fg, bg, flags) in cells.into_iter().take(keep) {
        let start = row.text.len();
        row.text.push(c);
        let style = Run {
            range: start..row.text.len(),
            fg,
            bg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strike: flags.contains(Flags::STRIKEOUT),
        };
        match row.runs.last_mut() {
            Some(last) if Run { range: last.range.clone(), ..style.clone() } == *last => last.range.end = row.text.len(),
            _ => row.runs.push(style),
        }
    }
    row
}

/// The row with a block cursor over column `col`: the text padded to reach it, and the run under it split so the
/// cursor cell draws in `ink` on `block`.
pub fn with_cursor(row: &Row, col: usize, ink: u32, block: u32, default_fg: u32) -> Row {
    let mut out = row.clone();
    let have = out.text.chars().count();
    if have <= col {
        out.text.extend(std::iter::repeat_n(' ', col + 1 - have));
    }
    let Some((start, c)) = out.text.char_indices().nth(col) else { return out };
    let end = start + c.len_utf8();
    let plain = |range: Range<usize>| Run { range, fg: default_fg, bg: None, bold: false, italic: false, underline: false, strike: false };
    let mut runs = Vec::new();
    for run in out.runs.iter().cloned().chain(std::iter::once(plain(row.text.len()..out.text.len())).filter(|r| !r.range.is_empty())) {
        if run.range.end <= start || run.range.start >= end {
            runs.push(run);
            continue;
        }
        if run.range.start < start {
            runs.push(Run { range: run.range.start..start, ..run.clone() });
        }
        runs.push(Run { range: start..end, fg: ink, bg: Some(block), ..run.clone() });
        if run.range.end > end {
            runs.push(Run { range: end..run.range.end, ..run });
        }
    }
    out.runs = runs;
    out
}

pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    // A pasted ESC could end bracketed paste early and run the rest as typed keys.
    let clean = text.replace('\x1b', "").replace("\r\n", "\r").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{clean}\x1b[201~").into_bytes() } else { clean.into_bytes() }
}

/// Which modifiers were held, as xterm numbers them in `CSI 1 ; m X` (1 + shift + 2·alt + 4·ctrl).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

/// The bytes a key sends. `key` is GPUI's key name (`enter`, `up`, `a`), `typed` the character it produced, if any.
/// `None`: the key sends nothing (a lone modifier, a command-key shortcut the window handles).
pub fn encode_key(key: &str, typed: Option<&str>, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let csi = |end: &str| -> Vec<u8> {
        if mods.any() { format!("\x1b[1;{}{end}", mods.param()).into_bytes() } else { format!("\x1b[{end}").into_bytes() }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods.any() { format!("\x1b[{n};{}~", mods.param()).into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    let arrow = |end: &str| if app_cursor && !mods.any() { format!("\x1bO{end}").into_bytes() } else { csi(end) };
    let alt = |bytes: Vec<u8>| if mods.alt { [b"\x1b".as_slice(), &bytes].concat() } else { bytes };
    Some(match key {
        "enter" => alt(b"\r".to_vec()),
        "backspace" => alt(if mods.ctrl { b"\x08".to_vec() } else { b"\x7f".to_vec() }),
        "tab" if mods.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => arrow("A"),
        "down" => arrow("B"),
        "right" => arrow("C"),
        "left" => arrow("D"),
        "home" => arrow("H"),
        "end" => arrow("F"),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => csi_ss3(mods, "P"),
        "f2" => csi_ss3(mods, "Q"),
        "f3" => csi_ss3(mods, "R"),
        "f4" => csi_ss3(mods, "S"),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if mods.ctrl => b"\0".to_vec(),
        _ if mods.ctrl => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            alt(vec![byte])
        }
        // Option as Meta: the key's own character after ESC (macOS would otherwise type `ß` for ⌥S).
        _ if mods.alt => {
            let base = if mods.shift { key.to_uppercase() } else { key.to_owned() };
            if base.chars().count() != 1 {
                return None;
            }
            [b"\x1b".as_slice(), base.as_bytes()].concat()
        }
        "space" => b" ".to_vec(),
        _ => typed.filter(|t| !t.is_empty())?.as_bytes().to_vec(),
    })
}

/// F1–F4: `SS3 P` alone, `CSI 1 ; m P` with modifiers.
fn csi_ss3(mods: Mods, end: &str) -> Vec<u8> {
    if mods.any() { format!("\x1b[1;{}{end}", mods.param()).into_bytes() } else { format!("\x1bO{end}").into_bytes() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> TermMode {
        TermMode::empty()
    }

    #[test]
    fn keys_send_what_a_terminal_sends() {
        let m = Mods::default();
        assert_eq!(encode_key("a", Some("a"), m, none()), Some(b"a".to_vec()));
        assert_eq!(encode_key("enter", None, m, none()), Some(b"\r".to_vec()));
        assert_eq!(encode_key("backspace", None, m, none()), Some(b"\x7f".to_vec()));
        assert_eq!(encode_key("up", None, m, none()), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key("up", None, m, TermMode::APP_CURSOR), Some(b"\x1bOA".to_vec()), "vim and less ask for SS3 arrows");
        let ctrl = Mods { ctrl: true, ..m };
        assert_eq!(encode_key("c", None, ctrl, none()), Some(vec![3]));
        assert_eq!(encode_key("d", None, ctrl, none()), Some(vec![4]));
        assert_eq!(encode_key("[", None, ctrl, none()), Some(vec![0x1b]));
        assert_eq!(encode_key("right", None, ctrl, none()), Some(b"\x1b[1;5C".to_vec()), "ctrl-arrow moves by word");
        let alt = Mods { alt: true, ..m };
        assert_eq!(encode_key("b", Some("∫"), alt, none()), Some(b"\x1bb".to_vec()), "option is meta");
        assert_eq!(encode_key("tab", None, Mods { shift: true, ..m }, none()), Some(b"\x1b[Z".to_vec()));
        assert_eq!(encode_key("delete", None, m, none()), Some(b"\x1b[3~".to_vec()));
        assert_eq!(encode_key("f1", None, m, none()), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key("f5", None, m, none()), Some(b"\x1b[15~".to_vec()));
        assert_eq!(encode_key("shift", None, m, none()), None, "a lone modifier sends nothing");
        assert_eq!(encode_key("é", Some("é"), m, none()), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn a_paste_is_bracketed_when_asked_and_cannot_end_the_bracket() {
        assert_eq!(paste_bytes("a\nb", false), b"a\rb");
        assert_eq!(paste_bytes("x\x1b[201~rm -rf ~\n", true), b"\x1b[200~x[201~rm -rf ~\r\x1b[201~");
    }

    #[test]
    fn the_palette_follows_xterm() {
        let p = Palette::new(0xeeeeee, 0x101010, 0xffaa00);
        assert_eq!(p.indexed(1), p.ansi[1]);
        assert_eq!(p.indexed(16), 0x000000);
        assert_eq!(p.indexed(196), 0xff0000);
        assert_eq!(p.indexed(231), 0xffffff);
        assert_eq!(p.indexed(232), 0x080808);
        assert_eq!(p.resolve(Color::Named(NamedColor::Foreground), &Colors::default()), 0xeeeeee);
        assert_eq!(p.resolve(Color::Spec(Rgb { r: 1, g: 2, b: 3 }), &Colors::default()), 0x010203);
    }

    #[test]
    fn an_offline_screen_is_rows_of_styled_runs() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::vte::ansi::Processor;
        let mut term = Term::new(Config::default(), &TermSize::new(20, 3), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[31mred\x1b[0m ok\r\n\x1b[1;44mbold\x1b[0m");
        let palette = Palette::new(0xeeeeee, 0x101010, 0xffaa00);
        let screen = screen(&term, &palette);
        assert_eq!(screen.rows[0].text, "red ok");
        assert_eq!(screen.rows[0].runs[0], Run { range: 0..3, fg: palette.ansi[1], bg: None, bold: false, italic: false, underline: false, strike: false });
        assert_eq!(screen.rows[0].runs[1].fg, 0xeeeeee);
        let bold = &screen.rows[1].runs[0];
        assert!(bold.bold && bold.bg == Some(palette.ansi[4]) && bold.range == (0..4));
        assert_eq!(screen.rows[2].text, "");
        assert_eq!(screen.cursor, Some((1, 4)));
    }

    #[test]
    fn the_cursor_splits_the_run_under_it_or_pads_the_row_to_reach_it() {
        let run = |range: Range<usize>, fg: u32| Run { range, fg, bg: None, bold: false, italic: false, underline: false, strike: false };
        let row = Row { text: "héllo".into(), runs: vec![run(0..6, 1)] };
        let mid = with_cursor(&row, 1, 9, 8, 7);
        assert_eq!(mid.text, "héllo");
        assert_eq!(mid.runs.iter().map(|r| (r.range.clone(), r.fg, r.bg)).collect::<Vec<_>>(), [(0..1, 1, None), (1..3, 9, Some(8)), (3..6, 1, None)]);
        let past = with_cursor(&row, 7, 9, 8, 7);
        assert_eq!(past.text, "héllo   ");
        assert_eq!(past.runs.iter().map(|r| (r.range.clone(), r.fg, r.bg)).collect::<Vec<_>>(), [(0..6, 1, None), (6..8, 7, None), (8..9, 9, Some(8))]);
        let empty = with_cursor(&Row::default(), 0, 9, 8, 7);
        assert_eq!((empty.text.as_str(), empty.runs.len()), (" ", 1));
    }

    #[test]
    fn process_arguments_come_out_of_the_kernel_layout() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0\0node\0/opt/bin/gemini\0PATH=/usr/bin\0");
        assert_eq!(parse_procargs(&buf), Some(vec!["node".to_owned(), "/opt/bin/gemini".to_owned()]), "the environment is not argv");
        assert_eq!(parse_procargs(&[1, 0]), None);
        let mine = process_argv(std::process::id() as i32).expect("this process's own arguments");
        assert!(!mine.is_empty());
    }

    #[tokio::test]
    async fn the_foreground_program_is_named() {
        let dir = std::env::temp_dir();
        let launch = Launch { program: "/bin/sh".into(), args: vec!["-c".into(), "exec /bin/sleep 5".into()], cwd: &dir, cols: 40, lines: 5 };
        let (session, _events) = Session::spawn(launch).unwrap();
        let mut found = None;
        for _ in 0..50 {
            found = session.foreground().filter(|argv| argv.first().is_some_and(|a| a.ends_with("sleep")));
            if found.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(found.map(|a| a[1..].to_vec()), Some(vec!["5".to_owned()]));
        session.shutdown();
    }

    #[tokio::test]
    async fn a_real_pty_runs_a_program_and_reports_its_exit() {
        let dir = std::env::temp_dir();
        let launch = Launch { program: "/bin/sh".into(), args: vec!["-c".into(), "printf 'hi from %s' \"$TERM\"; exit 3".into()], cwd: &dir, cols: 40, lines: 5 };
        let (session, mut events) = Session::spawn(launch).unwrap();
        let palette = Palette::new(0xeeeeee, 0x101010, 0xffaa00);
        let mut exit = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while exit.is_none() && tokio::time::Instant::now() < deadline {
            if let Ok(Some(Event::ChildExit(status))) = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await {
                exit = Some(status);
            }
        }
        assert_eq!(exit.and_then(|s| s.code()), Some(3));
        assert_eq!(session.screen(&palette).rows[0].text, "hi from xterm-256color");
        session.resize(30, 4, (8, 16));
        assert_eq!(session.size(), (30, 4));
        session.shutdown();
    }
}
