//! Inline terminal driver: the way Claude Code (Ink) and Codex draw.
//!
//! No alternate screen, no mouse capture. The program owns only a small *live
//! region* at the bottom of the terminal — the streaming reply, running tool
//! cards, the prompt box and the footer — which it erases and redraws in place.
//! Everything that is *finished* is written once, above the live region, as
//! ordinary terminal output. It therefore becomes real scrollback: the mouse
//! wheel, trackpad, `Shift+PageUp`, text selection, copy, search and tmux's
//! copy-mode all work on it exactly as they do on any command's output —
//! which is what a full-screen app (where a long reply could only be scrolled
//! with a key the keyboard might not have) cannot give.
//!
//! One frame is one `write`, bracketed by synchronized-update markers so a
//! terminal that supports them never shows a half-drawn region.

use std::io::{self, Write};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";

fn push_color(codes: &mut Vec<String>, color: Color, foreground: bool) {
    let (base, bright_base) = if foreground { (30, 90) } else { (40, 100) };
    let extended = if foreground { 38 } else { 48 };
    match color {
        Color::Reset => {}
        Color::Black => codes.push(base.to_string()),
        Color::Red => codes.push((base + 1).to_string()),
        Color::Green => codes.push((base + 2).to_string()),
        Color::Yellow => codes.push((base + 3).to_string()),
        Color::Blue => codes.push((base + 4).to_string()),
        Color::Magenta => codes.push((base + 5).to_string()),
        Color::Cyan => codes.push((base + 6).to_string()),
        Color::Gray => codes.push((base + 7).to_string()),
        Color::DarkGray => codes.push(bright_base.to_string()),
        Color::LightRed => codes.push((bright_base + 1).to_string()),
        Color::LightGreen => codes.push((bright_base + 2).to_string()),
        Color::LightYellow => codes.push((bright_base + 3).to_string()),
        Color::LightBlue => codes.push((bright_base + 4).to_string()),
        Color::LightMagenta => codes.push((bright_base + 5).to_string()),
        Color::LightCyan => codes.push((bright_base + 6).to_string()),
        Color::White => codes.push((bright_base + 7).to_string()),
        Color::Indexed(index) => codes.push(format!("{extended};5;{index}")),
        Color::Rgb(r, g, b) => codes.push(format!("{extended};2;{r};{g};{b}")),
    }
}

/// `ESC[0;…m` for a cell's style (always a full reset first, so styles never leak).
fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_owned()];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    push_color(&mut codes, fg, true);
    push_color(&mut codes, bg, false);
    format!("\x1b[{}m", codes.join(";"))
}

/// Each row of `buffer` as an ANSI string (no trailing newline). Trailing cells
/// that are plain blanks are dropped: a copy from the terminal should not carry
/// a screenful of trailing spaces, and the line needn't be padded to be redrawn.
pub fn buffer_to_ansi_rows(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    let mut rows = Vec::with_capacity(area.height as usize);
    for y in area.top()..area.bottom() {
        // Index of the last cell worth printing.
        let mut last = None;
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            let blank = cell.symbol() == " " && cell.bg == Color::Reset && cell.modifier.is_empty();
            if !blank {
                last = Some(x);
            }
        }
        let mut out = String::new();
        let mut current: Option<(Color, Color, Modifier)> = None;
        let mut skip = 0usize;
        if let Some(last) = last {
            for x in area.left()..=last {
                let cell = &buffer[(x, y)];
                if skip > 0 {
                    skip -= 1; // the trailing half of a wide glyph
                    continue;
                }
                let style = (cell.fg, cell.bg, cell.modifier);
                if current != Some(style) {
                    out.push_str(&sgr(style.0, style.1, style.2));
                    current = Some(style);
                }
                let symbol = cell.symbol();
                out.push_str(if symbol.is_empty() { " " } else { symbol });
                skip = UnicodeWidthStr::width(symbol).saturating_sub(1);
            }
        }
        if current.is_some() {
            out.push_str("\x1b[0m");
        }
        rows.push(out);
    }
    rows
}

/// Styled lines (already wrapped to `width`) as ANSI rows.
pub fn lines_to_ansi(lines: &[Line<'_>], width: u16) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut buffer = Buffer::empty(Rect::new(0, 0, width.max(1), lines.len() as u16));
    for (y, line) in lines.iter().enumerate() {
        buffer.set_line(0, y as u16, line, width.max(1));
    }
    buffer_to_ansi_rows(&buffer)
}

/// The inline region: erase-and-redraw bookkeeping over any `Write`.
pub struct InlineTerm<W: Write> {
    out: W,
    /// Rows the live region occupied after the last frame.
    rows: u16,
    /// Which row of the region the terminal cursor was left on.
    cursor_row: u16,
    cursor_visible: bool,
}

impl<W: Write> InlineTerm<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            rows: 0,
            cursor_row: 0,
            cursor_visible: true,
        }
    }

    /// Draw one frame: `commit` rows go above the live region (into scrollback),
    /// `live` rows replace the previous live region, and the cursor is put at
    /// `cursor` (column, row-within-live) if given, else hidden.
    pub fn draw(
        &mut self,
        commit: &[String],
        live: &[String],
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        let mut frame = String::new();
        frame.push_str(BEGIN_SYNC);
        frame.push_str("\x1b[?25l"); // never show the cursor mid-redraw

        // Back to the top-left of the previous live region and erase it (and
        // anything below it — that is all ours).
        if self.cursor_row > 0 {
            frame.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        frame.push('\r');
        frame.push_str("\x1b[J");

        // Finished output: written exactly like any program's output, so the
        // terminal scrolls it into its own history as the lines pile up.
        for row in commit {
            frame.push_str(row);
            frame.push_str("\r\n");
        }

        for (index, row) in live.iter().enumerate() {
            frame.push_str(row);
            if index + 1 < live.len() {
                frame.push_str("\r\n");
            }
        }
        self.rows = live.len() as u16;

        // Leave the cursor where the prompt is (so IME / accessibility tools see
        // it), or hide it.
        match cursor {
            Some((column, row)) if !live.is_empty() => {
                let last = self.rows.saturating_sub(1);
                let row = row.min(last);
                if last > row {
                    frame.push_str(&format!("\x1b[{}A", last - row));
                }
                frame.push_str(&format!("\x1b[{}G", column + 1));
                frame.push_str("\x1b[?25h");
                self.cursor_row = row;
                self.cursor_visible = true;
            }
            _ => {
                self.cursor_row = self.rows.saturating_sub(1);
                self.cursor_visible = false;
            }
        }
        frame.push_str(END_SYNC);
        self.out.write_all(frame.as_bytes())?;
        self.out.flush()
    }

    /// Erase the live region and put the cursor where it began, so the shell
    /// prompt comes back directly under the scrollback.
    pub fn clear_live(&mut self) -> io::Result<()> {
        let mut frame = String::new();
        if self.cursor_row > 0 {
            frame.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        frame.push_str("\r\x1b[J\x1b[?25h");
        self.out.write_all(frame.as_bytes())?;
        self.rows = 0;
        self.cursor_row = 0;
        self.out.flush()
    }

    /// Wipe the screen and its scrollback (`/clear`); the live region is gone with it.
    pub fn clear_screen(&mut self) -> io::Result<()> {
        self.out.write_all(b"\r\x1b[2J\x1b[3J\x1b[H")?;
        self.rows = 0;
        self.cursor_row = 0;
        self.out.flush()
    }
}

#[cfg(test)]
impl InlineTerm<Vec<u8>> {
    /// What has been written since the last call (for the emulator in tests).
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

/// A terminal emulator for tests: feeds our output through vt100 so assertions
/// see what a real terminal would show, scrollback included.
#[cfg(test)]
pub(crate) mod testing {
    pub fn emulator(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(rows, cols, 5000)
    }

    /// Everything the terminal has: scrollback (oldest first) then the screen.
    pub fn transcript(parser: &mut vt100::Parser) -> Vec<String> {
        let (rows, _cols) = parser.screen().size();
        parser.screen_mut().set_scrollback(usize::MAX);
        let max = parser.screen().scrollback();
        let mut lines = Vec::new();
        // Walk from the oldest page forward, one `rows`-sized window at a time.
        let mut offset = max;
        loop {
            parser.screen_mut().set_scrollback(offset);
            let page: Vec<String> = parser
                .screen()
                .contents()
                .lines()
                .map(str::to_owned)
                .collect();
            let take = if offset == 0 {
                page.len()
            } else {
                (rows as usize).min(page.len()).min(offset)
            };
            lines.extend(page.into_iter().take(take));
            if offset == 0 {
                break;
            }
            offset = offset.saturating_sub(rows as usize);
        }
        parser.screen_mut().set_scrollback(0);
        lines
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;
    use ratatui::text::Span;

    use super::testing::{emulator, transcript};
    use super::*;

    fn rows(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn feed(term: &mut InlineTerm<Vec<u8>>, parser: &mut vt100::Parser) {
        let bytes = std::mem::take(&mut term.out);
        parser.process(&bytes);
    }

    #[test]
    fn ansi_rows_carry_colors_wide_glyphs_and_trim_trailing_blanks() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_line(
            0,
            0,
            &Line::from(vec![
                Span::styled(
                    "hi",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" 日本"),
            ]),
            12,
        );
        let rows = buffer_to_ansi_rows(&buffer);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\x1b[0;1;31mhi"), "{:?}", rows[0]);
        assert!(
            rows[0].contains("日本") && !rows[0].contains("日  本"),
            "wide glyph printed once: {:?}",
            rows[0]
        );
        assert_eq!(rows[1], "", "an empty row is empty, not padded");
        assert!(
            !rows[0].ends_with(' '),
            "trailing blanks trimmed: {:?}",
            rows[0]
        );
    }

    #[test]
    fn the_live_region_is_redrawn_in_place_not_appended() {
        let mut parser = emulator(10, 40);
        let mut term = InlineTerm::new(Vec::new());
        term.draw(&[], &rows(&["live one", "prompt> "]), Some((8, 1)))
            .unwrap();
        feed(&mut term, &mut parser);
        term.draw(&[], &rows(&["live two", "prompt> a"]), Some((9, 1)))
            .unwrap();
        feed(&mut term, &mut parser);
        let screen = parser.screen().contents();
        let lines: Vec<&str> = screen.lines().collect();
        assert_eq!(
            lines,
            ["live two", "prompt> a"],
            "the old frame was erased: {screen:?}"
        );
        assert_eq!(
            parser.screen().cursor_position(),
            (1, 9),
            "cursor sits in the prompt"
        );
    }

    #[test]
    fn a_shrinking_live_region_leaves_no_ghost_rows() {
        let mut parser = emulator(10, 40);
        let mut term = InlineTerm::new(Vec::new());
        term.draw(&[], &rows(&["a", "b", "c", "d"]), None).unwrap();
        feed(&mut term, &mut parser);
        term.draw(&[], &rows(&["only"]), None).unwrap();
        feed(&mut term, &mut parser);
        assert_eq!(
            parser.screen().contents().lines().collect::<Vec<_>>(),
            ["only"]
        );
    }

    #[test]
    fn committed_output_lands_above_the_live_region_in_order() {
        let mut parser = emulator(12, 40);
        let mut term = InlineTerm::new(Vec::new());
        term.draw(&[], &rows(&["▸ prompt"]), Some((2, 0))).unwrap();
        feed(&mut term, &mut parser);
        term.draw(
            &rows(&["first message", "second message"]),
            &rows(&["▸ prompt"]),
            Some((2, 0)),
        )
        .unwrap();
        feed(&mut term, &mut parser);
        term.draw(
            &rows(&["third message"]),
            &rows(&["▸ prompt"]),
            Some((2, 0)),
        )
        .unwrap();
        feed(&mut term, &mut parser);
        assert_eq!(
            parser.screen().contents().lines().collect::<Vec<_>>(),
            [
                "first message",
                "second message",
                "third message",
                "▸ prompt"
            ]
        );
    }

    #[test]
    fn long_output_goes_into_real_scrollback_and_nothing_is_lost() {
        // THE bug being fixed: a reply far taller than the terminal. In the old
        // full-screen UI the early lines were unreachable without PageUp; here
        // they are ordinary scrollback that the terminal itself can scroll to.
        let mut parser = emulator(10, 50);
        let mut term = InlineTerm::new(Vec::new());
        term.draw(&[], &rows(&["> prompt"]), Some((2, 0))).unwrap();
        feed(&mut term, &mut parser);
        let reply: Vec<String> = (1..=100).map(|i| format!("reply line {i}")).collect();
        for chunk in reply.chunks(7) {
            term.draw(chunk, &rows(&["> prompt"]), Some((2, 0)))
                .unwrap();
            feed(&mut term, &mut parser);
        }
        let all = transcript(&mut parser);
        let reply_lines: Vec<&String> =
            all.iter().filter(|l| l.starts_with("reply line")).collect();
        assert_eq!(
            reply_lines.len(),
            100,
            "every line of the reply is in the terminal's history"
        );
        assert_eq!(reply_lines.first().unwrap().as_str(), "reply line 1");
        assert_eq!(reply_lines.last().unwrap().as_str(), "reply line 100");
        assert_eq!(
            all.iter().filter(|l| l.contains("> prompt")).count(),
            1,
            "exactly one live prompt, no stale copies"
        );
        assert_eq!(parser.screen().contents().lines().last(), Some("> prompt"));
    }

    #[test]
    fn a_tall_live_region_and_clearing_it_on_exit() {
        let mut parser = emulator(8, 30);
        let mut term = InlineTerm::new(Vec::new());
        term.draw(
            &rows(&["history"]),
            &rows(&["l1", "l2", "l3", "box"]),
            Some((1, 3)),
        )
        .unwrap();
        feed(&mut term, &mut parser);
        term.clear_live().unwrap();
        feed(&mut term, &mut parser);
        assert_eq!(
            parser.screen().contents().lines().collect::<Vec<_>>(),
            ["history"],
            "only the scrollback remains"
        );
        assert_eq!(
            parser.screen().cursor_position(),
            (1, 0),
            "the shell prompt returns right below it"
        );
    }
}
