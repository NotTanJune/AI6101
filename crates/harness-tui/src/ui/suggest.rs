//! Slash-command suggestions: a popup directly above the input box, and ghost text after the
//! cursor. No backdrop: the transcript stays readable and the popup never reaches the status
//! bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::{padded, truncate};
use crate::app::App;
use crate::completion::{self, MAX_ROWS};

/// Widest the popup gets.
const MAX_WIDTH: u16 = 80;

/// The popup rectangle for `rows` suggestion rows above `input_area`, inside `chat_area`.
/// `None` when there is no room for even one row.
pub(crate) fn popup_rect(chat_area: Rect, input_area: Rect, rows: usize) -> Option<Rect> {
    // Border on both sides of the rows.
    let room = usize::from(chat_area.height).saturating_sub(2);
    let rows = rows.min(MAX_ROWS).min(room);
    if rows == 0 || input_area.width < 8 {
        return None;
    }
    let height = u16::try_from(rows + 2).ok()?;
    let width = input_area.width.saturating_sub(2).min(MAX_WIDTH);
    Some(Rect {
        x: input_area.x + 1,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    })
}

pub(crate) fn render(frame: &mut Frame, app: &App, chat_area: Rect, input_area: Rect) {
    let Some(completion) = app.suggestions() else {
        return;
    };
    let Some(rect) = popup_rect(chat_area, input_area, completion.items.len()) else {
        return;
    };
    let theme = &app.theme;
    let selected = app.selected_suggestion(&completion);
    let rows = usize::from(rect.height) - 2;
    let first = if selected >= rows {
        selected + 1 - rows
    } else {
        0
    };

    let count = if completion.items.len() > rows {
        format!(" {}/{} ", selected + 1, completion.items.len())
    } else {
        String::new()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.on(theme.accent_alt, theme.raised))
        .style(theme.on(theme.text, theme.raised))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                completion.title,
                theme
                    .on(theme.accent_alt, theme.raised)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
        .title(
            Line::from(Span::styled(count, theme.on(theme.muted, theme.raised))).right_aligned(),
        );
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    if inner.width < 4 {
        return;
    }
    let width = usize::from(inner.width);
    let label_width = completion
        .items
        .iter()
        .map(|i| UnicodeWidthStr::width(i.insert.as_str()))
        .max()
        .unwrap_or(0)
        + 3;
    let prefix_len = completion.prefix.chars().count();
    let lines: Vec<Line> = completion
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, item)| {
            let is_selected = i == selected;
            let (base, typed, detail) = if is_selected {
                let base = theme.selected_style();
                (
                    base,
                    base.add_modifier(Modifier::UNDERLINED),
                    base.remove_modifier(Modifier::BOLD),
                )
            } else {
                (
                    theme.on(theme.text, theme.raised),
                    theme
                        .on(theme.accent, theme.raised)
                        .add_modifier(Modifier::BOLD),
                    theme.on(theme.muted, theme.raised),
                )
            };
            let split = item
                .insert
                .char_indices()
                .nth(prefix_len)
                .map_or(item.insert.len(), |(at, _)| at);
            let (head, tail) = item.insert.split_at(split);
            let marker = if is_selected { "› " } else { "  " };
            let gap = label_width.saturating_sub(UnicodeWidthStr::width(item.insert.as_str()));
            let detail_room = width.saturating_sub(2 + label_width);
            let spans = vec![
                Span::styled(marker, base),
                Span::styled(head.to_owned(), typed),
                Span::styled(tail.to_owned(), base),
                Span::styled(" ".repeat(gap), base),
                Span::styled(truncate(&item.detail, detail_room), detail),
            ];
            if is_selected {
                padded(spans, width, base)
            } else {
                Line::from(spans)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Dim ghost text after the cursor: the rest of the selected suggestion.
pub(crate) fn ghost(frame: &mut Frame, app: &App, text_rect: Rect, left: usize) {
    let Some(completion) = app.suggestions() else {
        return;
    };
    let selected = app.selected_suggestion(&completion);
    let Some(rest) = completion::ghost(&completion, selected) else {
        return;
    };
    let typed = UnicodeWidthStr::width(app.input.text());
    let Some(offset) = typed.checked_sub(left) else {
        return;
    };
    let room = usize::from(text_rect.width).saturating_sub(offset);
    if room == 0 {
        return;
    }
    let area = Rect {
        x: text_rect.x + u16::try_from(offset).unwrap_or(0),
        y: text_rect.y,
        width: u16::try_from(room).unwrap_or(0),
        height: 1.min(text_rect.height),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&rest, room),
            app.theme.on(app.theme.muted, app.theme.surface),
        ))),
        area,
    );
}
