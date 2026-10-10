//! Rendering. Pure functions of [`App`] state: no input handling, no I/O.

mod approval;
mod chat;
mod confirm;
mod drift;
pub(crate) mod help;
mod input;
mod picker;
pub(crate) mod popup;
mod status;
mod suggest;
mod welcome;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Overlay};
use crate::theme::Theme;

/// Draw the whole application: transcript, input box, status bar and the open overlay.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(app.theme.base()), area);
    let input_height = input::height(app).min(area.height.saturating_sub(2).max(1));
    let [chat_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);

    chat::render(frame, app, chat_area);
    input::render(frame, app, input_area);
    status::render(frame, app, status_area);
    if app.overlay.is_none() {
        suggest::render(frame, app, chat_area, input_area);
    }

    // Overlays live in the chat region only: the input box and status bar stay visible.
    match &app.overlay {
        Some(Overlay::Approval(view)) => approval::render(frame, chat_area, app, view),
        Some(Overlay::Drift(view)) => drift::render(frame, chat_area, app, view),
        Some(Overlay::Help) => help::render(frame, chat_area, app),
        Some(Overlay::Picker(view)) => picker::render(frame, chat_area, app, view),
        Some(Overlay::Confirm(view)) => confirm::render(frame, chat_area, app, view),
        None => {}
    }
}

/// The transcript region of a frame of `area`: what is left above the input box and the status
/// bar. Overlays are confined to it.
pub(crate) fn chat_rect(area: ratatui::layout::Rect, app: &App) -> ratatui::layout::Rect {
    let input_height = input::height(app).min(area.height.saturating_sub(2).max(1));
    let [chat_area, _, _] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);
    chat_area
}

/// Style of one unified-diff line: tinted background, coloured text.
pub(crate) fn diff_style(theme: &Theme, line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        theme.fg(theme.text).add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        theme.on(theme.success, theme.diff_add_bg)
    } else if line.starts_with('-') {
        theme.on(theme.error, theme.diff_del_bg)
    } else if line.starts_with("@@") {
        theme.fg(theme.accent)
    } else {
        Style::new()
    }
}

/// Whether `text` looks like a unified diff (a hunk header, or a `---`/`+++` file header pair).
pub(crate) fn looks_like_diff(text: &str) -> bool {
    let mut previous_minus = false;
    for line in text.lines() {
        if line.starts_with("@@ ") {
            return true;
        }
        if previous_minus && line.starts_with("+++ ") {
            return true;
        }
        previous_minus = line.starts_with("--- ");
    }
    false
}

/// Display width of the spans of a line.
pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// A line made of `spans`, padded with `fill` to `width` columns so a background reaches the
/// right edge.
pub(crate) fn padded(mut spans: Vec<Span<'static>>, width: usize, fill: Style) -> Line<'static> {
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans)
}

/// Wrap `text` to `width` display columns. Lines break at spaces; a word longer than a line is
/// split. Embedded newlines start new lines and empty lines are kept.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.replace('\t', "    ");
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for token in raw.split_inclusive(' ') {
            let token_width = UnicodeWidthStr::width(token);
            let visible_width = UnicodeWidthStr::width(token.trim_end_matches(' '));
            if line_width + visible_width <= width {
                line.push_str(token);
                line_width += token_width;
                continue;
            }
            if !line.trim().is_empty() {
                out.push(line.trim_end().to_owned());
            }
            line = String::new();
            line_width = 0;
            if visible_width <= width {
                line.push_str(token);
                line_width = token_width;
                continue;
            }
            for c in token.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if line_width + cw > width && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                line.push(c);
                line_width += cw;
            }
        }
        out.push(line.trim_end().to_owned());
    }
    out
}

/// Cut `text` to at most `max` display columns, ending with `…` when something was removed.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw + 1 > max {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}
