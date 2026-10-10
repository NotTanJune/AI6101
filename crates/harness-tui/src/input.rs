//! Multi-line input buffer with a cursor.
//!
//! The buffer holds plain text; `\n` separates lines. The cursor is a byte offset that always
//! sits on a character boundary. Rendering and key handling live elsewhere.

/// Editable multi-line text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
    /// Empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The whole text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the whole text; the cursor moves to the end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    /// Whether the cursor is after the last character.
    pub fn cursor_at_end(&self) -> bool {
        self.cursor == self.text.len()
    }

    /// Whether nothing is typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Lines of the text. An empty buffer has one empty line, and a trailing newline yields a
    /// trailing empty line, so the cursor always has a row.
    pub fn lines(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    /// Cursor position as (row, column) in characters.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        (row, before[line_start..].chars().count())
    }

    /// Insert one character at the cursor. `\r` is ignored; a line break is inserted with
    /// [`InputBuffer::newline`].
    pub fn insert_char(&mut self, c: char) {
        if c == '\r' {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert pasted text. Line endings are normalised to `\n`.
    pub fn insert_str(&mut self, pasted: &str) {
        let normalised = pasted.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &normalised);
        self.cursor += normalised.len();
    }

    /// Insert a line break at the cursor.
    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    /// Move one character left.
    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    /// Move one character right.
    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    /// Move to the start of the current line.
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    /// Move to the end of the current line.
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    /// Move up one line, keeping the column when the line is long enough.
    pub fn up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row > 0 {
            self.set_row_col(row - 1, col);
        }
    }

    /// Move down one line, keeping the column when the line is long enough.
    pub fn down(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row + 1 < self.lines().len() {
            self.set_row_col(row + 1, col);
        }
    }

    /// Remove and return the whole text, leaving the buffer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Empty the buffer.
    pub fn clear(&mut self) {
        self.take();
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    fn set_row_col(&mut self, row: usize, col: usize) {
        let mut offset = 0;
        for (i, line) in self.text.split('\n').enumerate() {
            if i == row {
                let within: usize = line.chars().take(col).map(char::len_utf8).sum();
                self.cursor = offset + within;
                return;
            }
            offset += line.len() + 1;
        }
    }
}
