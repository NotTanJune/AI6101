//! A yes/no question.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::popup;
use crate::app::{App, ConfirmView};

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App, view: &ConfirmView) {
    popup::show(
        frame,
        app,
        area,
        &popup::Spec {
            width: 72,
            height: 11,
            title: "Confirm",
            accent: app.theme.warning,
        },
        |frame, inner| {
            let theme = &app.theme;
            let lines = vec![
                Line::from(Span::styled(view.prompt.clone(), theme.fg(theme.text))),
                Line::from(""),
                Line::from(vec![
                    Span::styled("[y]", theme.fg(theme.success).add_modifier(Modifier::BOLD)),
                    Span::styled(" yes   ", theme.fg(theme.text)),
                    Span::styled(
                        "[n]/Esc",
                        theme.fg(theme.error).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" no", theme.fg(theme.text)),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        },
    );
}
