//! Minimal multi-line text input. The cursor is a byte index kept on char
//! boundaries.

#[derive(Debug, Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Replace the whole text and put the cursor at the end (history recall).
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    /// Replace the bytes in `range` with `with` and put the cursor just after it (completion).
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, with: &str) {
        let start = range.start.min(self.text.len());
        let end = range.end.clamp(start, self.text.len());
        self.text.replace_range(start..end, with);
        self.cursor = start + with.len();
    }

    /// No newline before the cursor: Up should walk history, not move a line.
    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    /// No newline after the cursor: Down should walk history.
    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(prev) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.text[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_respect_char_boundaries() {
        let mut input = Input::default();
        for c in "héllo".chars() {
            input.insert(c);
        }
        input.left();
        input.left();
        input.backspace();
        assert_eq!(input.text(), "hélo");
        input.home();
        input.delete();
        assert_eq!(input.text(), "élo");
        input.end();
        input.insert('!');
        assert_eq!(input.text(), "élo!");
    }

    #[test]
    fn completion_replaces_a_range_and_leaves_the_cursor_after_it() {
        let mut input = Input::default();
        input.set_text("fix @sr and more");
        input.replace_range(4..7, "@src/app.rs ");
        assert_eq!(input.text(), "fix @src/app.rs  and more");
        assert_eq!(input.cursor(), 4 + "@src/app.rs ".len());
    }

    #[test]
    fn home_end_work_per_line() {
        let mut input = Input::default();
        for c in "ab\ncd".chars() {
            input.insert(c);
        }
        input.home();
        assert_eq!(input.cursor(), 3);
        input.left();
        input.home();
        assert_eq!(input.cursor(), 0);
        input.end();
        assert_eq!(input.cursor(), 2);
    }
}
