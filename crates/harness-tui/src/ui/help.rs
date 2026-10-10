//! Key and command help: a scrollable popup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::popup;
use crate::app::App;
use crate::commands::COMMAND_HELP;
use crate::theme::Theme;

const WIDTH: u16 = 96;

fn key_row(theme: &Theme, keys: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {keys:<20}"), theme.fg(theme.accent)),
        Span::styled(what.to_owned(), theme.fg(theme.text)),
    ])
}

fn heading(theme: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        theme.fg(theme.accent_alt).add_modifier(Modifier::BOLD),
    ))
}

/// Every help line.
pub(crate) fn lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(theme, "Keys"),
        key_row(
            theme,
            "Enter",
            "send the message (ignored while a turn runs)",
        ),
        key_row(
            theme,
            "Alt+Enter, Ctrl+J",
            "new line (Shift+Enter where the terminal reports it)",
        ),
        key_row(
            theme,
            "Ctrl-C",
            "cancel the running turn; when idle press twice to quit",
        ),
        key_row(theme, "PgUp / PgDn", "scroll the transcript"),
        key_row(
            theme,
            "Tab / Shift+Tab",
            "expand or collapse the focused tool card / move focus",
        ),
        key_row(
            theme,
            "/",
            "start a command: Up/Down choose, Tab completes, Esc dismisses",
        ),
        key_row(theme, "?", "this help (when the input is empty)"),
        key_row(theme, "Esc", "close an overlay"),
        Line::from(""),
        heading(theme, "Approvals"),
        Line::from(vec![
            Span::styled("  y", theme.fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("  allow once     "),
            Span::styled("a", theme.fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::raw("  allow this class for the session     "),
            Span::styled(
                "n or Esc",
                theme.fg(theme.error).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  deny"),
        ]),
        Line::from(""),
        heading(theme, "Commands"),
    ];
    for (usage, what) in COMMAND_HELP {
        lines.push(Line::from(vec![
            Span::styled(format!("  {usage:<24}"), theme.fg(theme.accent)),
            Span::styled((*what).to_owned(), theme.fg(theme.text)),
        ]));
    }
    lines
}

/// Number of help lines (without the footer).
pub(crate) fn line_count() -> usize {
    lines(&Theme::default()).len()
}

fn popup_rect(area: Rect) -> Rect {
    let height = u16::try_from(line_count() + 5).unwrap_or(u16::MAX);
    popup::rect(area, WIDTH, height)
}

/// How many help lines fit in the popup inside the chat region `area`.
pub(crate) fn visible_rows(area: Rect) -> usize {
    let inner = popup::inner(popup_rect(area));
    usize::from(inner.height.saturating_sub(1))
}

/// Largest useful scroll offset inside the chat region `area`.
pub(crate) fn max_scroll(area: Rect) -> usize {
    line_count().saturating_sub(visible_rows(area))
}

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let want_h = u16::try_from(line_count() + 5).unwrap_or(u16::MAX);
    popup::show(
        frame,
        app,
        area,
        &popup::Spec {
            width: WIDTH,
            height: want_h,
            title: "Help",
            accent: app.theme.accent,
        },
        |frame, inner| {
            if inner.width == 0 || inner.height < 2 {
                return;
            }
            let theme = &app.theme;
            let [body, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let all = lines(theme);
            let visible = usize::from(body.height);
            let max_offset = all.len().saturating_sub(visible);
            let offset = app.help_scroll.min(max_offset);
            let shown: Vec<Line> = all.iter().skip(offset).take(visible).cloned().collect();
            frame.render_widget(Paragraph::new(shown), body);
            let below = all.len().saturating_sub(offset + visible);
            let text = if max_offset == 0 {
                "Esc, Enter or ? closes this help.".to_owned()
            } else if below > 0 && offset > 0 {
                format!(
                    "↑↓ {below} more below, {offset} above · Up/Down PgUp/PgDn scroll · Esc closes"
                )
            } else if below > 0 {
                format!("↓ {below} more · Up/Down PgUp/PgDn scroll · Esc closes")
            } else {
                "↑ scroll up for the rest · Esc closes".to_owned()
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(text, theme.muted_style()))),
                footer,
            );
        },
    );
}
