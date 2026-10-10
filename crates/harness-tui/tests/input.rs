//! The multi-line input buffer.

use harness_tui::InputBuffer;

fn typed(text: &str) -> InputBuffer {
    let mut buffer = InputBuffer::new();
    for c in text.chars() {
        if c == '\n' {
            buffer.newline();
        } else {
            buffer.insert_char(c);
        }
    }
    buffer
}

#[test]
fn typing_editing_and_cursor_position() {
    let mut b = typed("héllo");
    assert_eq!(b.text(), "héllo");
    assert_eq!(b.cursor_row_col(), (0, 5));
    b.backspace();
    assert_eq!(b.text(), "héll");
    b.left();
    b.left();
    b.insert_char('X');
    assert_eq!(b.text(), "héXll");
    b.home();
    b.delete();
    assert_eq!(b.text(), "éXll");
    b.end();
    assert_eq!(b.cursor_row_col(), (0, 4));
}

#[test]
fn multiple_lines_and_vertical_movement() {
    let mut b = typed("one\ntwo three\nx");
    assert_eq!(b.lines(), vec!["one", "two three", "x"]);
    assert_eq!(b.cursor_row_col(), (2, 1));
    b.up();
    assert_eq!(b.cursor_row_col(), (1, 1));
    b.end();
    b.up();
    assert_eq!(
        b.cursor_row_col(),
        (0, 3),
        "column is clamped to the shorter line"
    );
    b.down();
    b.down();
    assert_eq!(b.cursor_row_col().0, 2);
    b.down();
    assert_eq!(b.cursor_row_col().0, 2, "no line below");
}

#[test]
fn paste_normalises_line_endings_and_take_empties_the_buffer() {
    let mut b = InputBuffer::new();
    b.insert_str("a\r\nb\rc");
    assert_eq!(b.text(), "a\nb\nc");
    assert_eq!(b.lines().len(), 3);
    assert_eq!(b.take(), "a\nb\nc");
    assert!(b.is_empty());
    assert_eq!(b.lines(), vec![""], "an empty buffer still has one row");
}

#[test]
fn backspace_at_a_line_start_joins_lines_and_edges_do_nothing() {
    let mut b = typed("ab\ncd");
    b.home();
    b.backspace();
    assert_eq!(b.text(), "abcd");
    let mut empty = InputBuffer::new();
    empty.backspace();
    empty.delete();
    empty.left();
    empty.right();
    assert!(empty.is_empty());
}
