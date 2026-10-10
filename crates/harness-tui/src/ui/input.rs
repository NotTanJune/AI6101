//! The bordered input box.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::chat::spinner_span;
use crate::anim;
use crate::app::App;
use crate::theme::lerp;

/// Most text lines the box shows before it scrolls.
pub const MAX_INPUT_LINES: usize = 6;

/// Prompt glyph at the start of the first line.
pub const PROMPT: &str = "❯ ";

/// Width of the prompt column.
const PROMPT_WIDTH: usize = 2;

/// Shown while the input is empty.
pub const PLACEHOLDER: &str = "Ask anything, or type / for commands";

/// Key hint shown on the right of the top border when there is room.
const HINT: &str = "Enter send · Alt+Enter newline · ? help";

/// Height of the box including its border.
pub(crate) fn height(app: &App) -> u16 {
    let lines = app.input.lines().len().clamp(1, MAX_INPUT_LINES);
    u16::try_from(lines + 2).unwrap_or(8)
}

pub(crate) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let border_rgb = if app.busy {
        // Warm and moving while a turn runs.
        lerp(theme.warning, theme.accent_alt, anim::gradient_t(app.frame))
    } else {
        theme.accent
    };
    let bg = theme.surface;
    let border = theme.on(border_rgb, bg);
    let mut title_spans: Vec<Span> = vec![Span::styled(" ", theme.on(theme.text, bg))];
    if app.busy {
        title_spans.push(spinner_span(theme, app.frame, Some(bg)));
        title_spans.push(Span::styled(
            " working… ",
            theme.on(theme.warning, bg).add_modifier(Modifier::BOLD),
        ));
    } else {
        title_spans.push(Span::styled(
            "message ",
            theme.on(theme.accent, bg).add_modifier(Modifier::BOLD),
        ));
    }
    let title_width: usize = super::spans_width(&title_spans);
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .style(theme.on(theme.text, bg))
        .title(Line::from(title_spans));
    if usize::from(area.width) >= title_width + HINT.chars().count() + 6 {
        block = block.title(
            Line::from(Span::styled(format!(" {HINT} "), theme.on(theme.muted, bg)))
                .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = app.input.lines();
    let (row, col) = app.input.cursor_row_col();
    let visible = usize::from(inner.height);
    let top = if row >= visible { row + 1 - visible } else { 0 };
    let cursor_text: String = lines
        .get(row)
        .map_or_else(String::new, |l| l.chars().take(col).collect());
    let cursor_width = UnicodeWidthStr::width(cursor_text.as_str());
    let text_width = usize::from(inner.width).saturating_sub(PROMPT_WIDTH).max(1);
    let left = if cursor_width >= text_width {
        cursor_width + 1 - text_width
    } else {
        0
    };
    let prompt_rect = Rect {
        width: inner.width.min(PROMPT_WIDTH as u16),
        ..inner
    };
    let text_rect = Rect {
        x: inner.x.saturating_add(PROMPT_WIDTH as u16),
        width: inner.width.saturating_sub(PROMPT_WIDTH as u16),
        ..inner
    };
    let prompt_style = if app.busy {
        theme.on(theme.muted, bg)
    } else {
        theme.on(theme.accent, bg).add_modifier(Modifier::BOLD)
    };
    let mut prompt_lines = vec![Line::from(Span::styled(PROMPT, prompt_style))];
    if top > 0 {
        prompt_lines.clear();
        prompt_lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(prompt_lines), prompt_rect);
    if text_rect.width == 0 {
        return;
    }
    if app.input.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                PLACEHOLDER,
                theme.on(theme.muted, bg),
            ))),
            text_rect,
        );
    } else {
        let shown: Vec<Line> = lines
            .iter()
            .skip(top)
            .take(visible)
            .map(|l| Line::from(Span::styled((*l).to_owned(), theme.on(theme.text, bg))))
            .collect();
        frame.render_widget(
            Paragraph::new(shown).scroll((0, u16::try_from(left).unwrap_or(0))),
            text_rect,
        );
        super::suggest::ghost(frame, app, text_rect, left);
    }
    if app.overlay.is_none() {
        let x = text_rect.x + u16::try_from(cursor_width - left).unwrap_or(0);
        let y = inner.y + u16::try_from(row - top).unwrap_or(0);
        frame.set_cursor_position((x, y));
    }
}
