//! The transcript: user messages, streamed model text, tool cards and notices.

use harness_agent::approval::DENIED_TEXT;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::{diff_style, looks_like_diff, padded, truncate, wrap};
use crate::anim;
use crate::app::{App, Entry, ToolCard};
use crate::theme::{Theme, lerp};

/// Output lines shown in an expanded card before `… N more lines`.
pub const OUTPUT_PREVIEW_LINES: usize = 40;

/// Block character that marks a streaming reply.
pub const CURSOR: &str = "█";

/// Left bar of a user message.
pub const USER_BAR: &str = "▎";

/// Left gutter of an assistant message.
pub const ASSISTANT_GUTTER: &str = "▏";

/// Columns kept free on each side of the transcript when the terminal is wide enough.
const SIDE_MARGIN: u16 = 1;

/// The transcript area inside the side margins.
pub(crate) fn content_area(area: Rect) -> Rect {
    if area.width >= 20 {
        Rect {
            x: area.x + SIDE_MARGIN,
            width: area.width - 2 * SIDE_MARGIN,
            ..area
        }
    } else {
        area
    }
}

pub(crate) fn render(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if app.transcript.is_empty() {
        super::welcome::render(frame, app, area);
        return;
    }
    let area = content_area(area);
    let lines = build_lines(app, usize::from(area.width));
    let height = usize::from(area.height);
    let max_offset = lines.len().saturating_sub(height);
    let offset = app.scroll.shown.min(max_offset);
    let end = lines.len() - offset;
    let start = end.saturating_sub(height);
    frame.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
}

/// Every transcript line, already wrapped to `width`. An empty transcript has no lines (the
/// welcome banner is drawn instead).
pub(crate) fn build_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, entry) in app.transcript.iter().enumerate() {
        match entry {
            Entry::User(text) => {
                let bar = theme.fg(theme.user).add_modifier(Modifier::BOLD);
                for part in wrap(text, width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{USER_BAR} "), bar),
                        Span::styled(part, theme.fg(theme.user)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            Entry::Assistant { text, streaming } => {
                let gutter = theme.fg(lerp(theme.assistant, theme.bg, 0.45));
                let mut parts = wrap(text, width.saturating_sub(2));
                if *streaming {
                    let room = parts
                        .last()
                        .is_some_and(|last| UnicodeWidthStr::width(last.as_str()) + 2 < width);
                    if !room {
                        parts.push(String::new());
                    }
                }
                let last_index = parts.len().saturating_sub(1);
                for (i, part) in parts.into_iter().enumerate() {
                    let mut spans = vec![
                        Span::styled(format!("{ASSISTANT_GUTTER} "), gutter),
                        Span::styled(part, theme.fg(theme.text)),
                    ];
                    if *streaming && i == last_index {
                        spans.push(Span::styled(CURSOR, cursor_style(theme, app.frame)));
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            Entry::ToolCard(card) => card_lines(
                theme,
                app.frame,
                card,
                app.focus == Some(index),
                width,
                &mut lines,
            ),
            Entry::Notice(text) => {
                for (i, part) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                    let prefix = if i == 0 { "· " } else { "  " };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{part}"),
                        theme.muted_style(),
                    )));
                }
            }
            Entry::Error(text) => {
                for (i, part) in wrap(text, width.saturating_sub(7)).into_iter().enumerate() {
                    let prefix = if i == 0 { "error: " } else { "       " };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{part}"),
                        theme.fg(theme.error).add_modifier(Modifier::BOLD),
                    )));
                }
            }
        }
    }
    lines
}

/// Colour of the streaming cursor: it pulses between the muted and the accent colour.
fn cursor_style(theme: &Theme, frame: u64) -> Style {
    theme.fg(lerp(theme.muted, theme.accent, anim::cursor_pulse(frame)))
}

/// The spinner glyph with a colour that cycles along the accent gradient.
pub(crate) fn spinner_span(
    theme: &Theme,
    frame: u64,
    bg: Option<crate::theme::Rgb>,
) -> Span<'static> {
    let color = lerp(theme.accent, theme.accent_alt, anim::gradient_t(frame));
    let style = match bg {
        Some(bg) => theme.on(color, bg),
        None => theme.fg(color),
    };
    Span::styled(anim::spinner_glyph(frame), style)
}

fn status_span(
    theme: &Theme,
    frame: u64,
    card: &ToolCard,
    bg: crate::theme::Rgb,
) -> Vec<Span<'static>> {
    match (&card.output, card.is_error) {
        (None, _) => vec![
            spinner_span(theme, frame, Some(bg)),
            Span::styled(" running", theme.on(theme.muted, bg)),
        ],
        (Some(output), true) if output == DENIED_TEXT => {
            vec![Span::styled("✗ denied", theme.on(theme.error, bg))]
        }
        (Some(_), true) => vec![Span::styled(
            format!("✗ {}", duration(card)),
            theme.on(theme.error, bg),
        )],
        (Some(_), false) => vec![Span::styled(
            format!("✓ {}", duration(card)),
            theme.on(theme.success, bg),
        )],
    }
}

fn duration(card: &ToolCard) -> String {
    card.duration_ms
        .map_or_else(String::new, |ms| format!("{ms}ms"))
}

fn card_lines(
    theme: &Theme,
    frame: u64,
    card: &ToolCard,
    focused: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let bg = if focused {
        theme.highlight
    } else {
        theme.raised
    };
    let fill = theme.bg_style(bg);
    let rail_color = if focused { theme.accent } else { theme.border };
    let rail = |glyph: &str| Span::styled(format!("{glyph} "), theme.on(rail_color, bg));

    let status = status_span(theme, frame, card, bg);
    let suffix_width = 2 + super::spans_width(&status);
    let arrow = if card.expanded { "▾ " } else { "▸ " };
    let arrow_style = if focused {
        theme.on(theme.accent, bg).add_modifier(Modifier::BOLD)
    } else {
        theme.on(theme.warning, bg)
    };
    let available = width.saturating_sub(4 + suffix_width);
    let name_style = theme.on(theme.accent, bg).add_modifier(Modifier::BOLD);
    let mut head = vec![
        rail(if card.expanded { "╭" } else { "╶" }),
        Span::styled(arrow, arrow_style),
    ];
    if card.expanded {
        head.push(Span::styled(truncate(&card.name, available), name_style));
    } else {
        let full = truncate(&format!("{}({})", card.name, card.args), available);
        let name_chars = card.name.chars().count();
        let name: String = full.chars().take(name_chars).collect();
        let rest: String = full.chars().skip(name_chars).collect();
        head.push(Span::styled(name, name_style));
        if !rest.is_empty() {
            head.push(Span::styled(rest, theme.on(theme.muted, bg)));
        }
    }
    head.push(Span::styled("  ", fill));
    head.extend(status);
    out.push(padded(head, width, fill));
    if !card.expanded {
        return;
    }
    let inner = width.saturating_sub(4).max(1);
    let label = theme.on(theme.muted, bg).add_modifier(Modifier::BOLD);
    let body = theme.on(theme.text, bg);
    let row = |text: String, style: Style| {
        padded(vec![rail("│"), Span::styled(text, style)], width, fill)
    };

    out.push(row("args:".to_owned(), label));
    let pretty = match serde_json::from_str::<Value>(&card.args) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| card.args.clone()),
        Err(_) => card.args.clone(),
    };
    for part in wrap(&pretty, inner) {
        out.push(row(format!("  {part}"), body));
    }
    out.push(row("output:".to_owned(), label));
    match &card.output {
        None => out.push(row("  (running)".to_owned(), theme.on(theme.muted, bg))),
        Some(output) => {
            let diffy = looks_like_diff(output);
            let source: Vec<&str> = output.lines().collect();
            for line in source.iter().take(OUTPUT_PREVIEW_LINES) {
                let style = if diffy {
                    let base = diff_style(theme, line);
                    if base.bg.is_some() {
                        base
                    } else {
                        base.patch(theme.on(theme.text, bg))
                    }
                } else if card.is_error {
                    theme.on(theme.error, bg)
                } else {
                    body
                };
                for part in wrap(line, inner) {
                    out.push(row(format!("  {part}"), style));
                }
            }
            if source.len() > OUTPUT_PREVIEW_LINES {
                out.push(row(
                    format!("  … {} more lines", source.len() - OUTPUT_PREVIEW_LINES),
                    theme.on(theme.muted, bg),
                ));
            }
        }
    }
    out.push(padded(vec![rail("╰")], width, fill));
}
