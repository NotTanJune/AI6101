//! A list picker (used by `/model`).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::{padded, popup, truncate};
use crate::app::{App, PickerView};

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App, view: &PickerView) {
    let want_h = u16::try_from(view.items.len() + 7).unwrap_or(u16::MAX);
    popup::show(
        frame,
        app,
        area,
        &popup::Spec {
            width: 72,
            height: want_h,
            title: &view.title,
            accent: app.theme.accent,
        },
        |frame, inner| {
            if inner.width == 0 || inner.height < 2 {
                return;
            }
            let theme = &app.theme;
            let [list, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let rows = usize::from(list.height);
            let width = usize::from(list.width);
            let selected = view.selected.min(view.items.len().saturating_sub(1));
            let first = if selected >= rows {
                selected + 1 - rows
            } else {
                0
            };
            let lines: Vec<Line> = view
                .items
                .iter()
                .enumerate()
                .skip(first)
                .take(rows)
                .map(|(i, item)| {
                    let is_selected = i == selected;
                    let marker = if is_selected { "› " } else { "  " };
                    let label = format!("{marker}{:<8}", item.label);
                    let detail_room = width.saturating_sub(UnicodeWidthStr::width(label.as_str()));
                    let detail = truncate(&format!(" {}", item.detail), detail_room);
                    if is_selected {
                        let style = theme.selected_style();
                        padded(
                            vec![
                                Span::styled(label, style),
                                Span::styled(detail, style.remove_modifier(Modifier::BOLD)),
                            ],
                            width,
                            style,
                        )
                    } else {
                        Line::from(vec![
                            Span::styled(label, theme.fg(theme.text)),
                            Span::styled(detail, theme.muted_style()),
                        ])
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), list);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Up/Down select · Enter choose · Esc cancel",
                    theme.muted_style(),
                ))),
                footer,
            );
        },
    );
}
