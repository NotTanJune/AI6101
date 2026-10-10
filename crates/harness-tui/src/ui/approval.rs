//! The approval modal. A safety control: it shows exactly what the agent wants to do and has no
//! default answer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{diff_style, popup, truncate, wrap};
use crate::app::{App, ApprovalView};

/// Rows the diff may take before it is cut with `… N more diff lines`.
const MAX_DIFF_ROWS: usize = 18;

const WIDTH: u16 = 100;

/// Popup height that fits the request: frame (4), header (3), summary, diff, blank and legend (2).
fn wanted_height(area: Rect, view: &ApprovalView) -> u16 {
    let width = usize::from(popup::inner_width(area, WIDTH));
    let request = &view.request;
    let mut rows = 4 + 3 + wrap(&request.summary, width.saturating_sub(2)).len() + 2;
    if let Some(diff) = &request.diff {
        rows += 2 + diff.lines().count().min(MAX_DIFF_ROWS + 1);
    }
    u16::try_from(rows).unwrap_or(u16::MAX)
}

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App, view: &ApprovalView) {
    popup::show(
        frame,
        app,
        area,
        &popup::Spec {
            width: WIDTH,
            height: wanted_height(area, view),
            title: "Approval required",
            accent: app.theme.warning,
        },
        |frame, inner| {
            let theme = &app.theme;
            let [body, legend] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
            let width = usize::from(body.width);
            let request = &view.request;
            let label = theme.fg(theme.muted).add_modifier(Modifier::BOLD);

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::styled("Tool:    ", label),
                    Span::styled(
                        request.tool.clone(),
                        theme.fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Class:   ", label),
                    Span::styled(request.class.as_str(), theme.fg(theme.warning)),
                ]),
                Line::from(Span::styled("Summary:", label)),
            ];
            for part in wrap(&request.summary, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!("  {part}"),
                    theme.fg(theme.text),
                )));
            }
            if let Some(diff) = &request.diff {
                lines.push(Line::from(""));
                if diff.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "Diff: (no changes)",
                        theme.muted_style(),
                    )));
                } else {
                    lines.push(Line::from(Span::styled("Diff:", label)));
                    let budget = usize::from(body.height).saturating_sub(lines.len());
                    let diff_lines: Vec<&str> = diff.lines().collect();
                    let shown = if diff_lines.len() > budget {
                        budget.saturating_sub(1)
                    } else {
                        diff_lines.len()
                    };
                    for line in diff_lines.iter().take(shown) {
                        let text = truncate(line, width);
                        let style = diff_style(theme, line);
                        let pad = width
                            .saturating_sub(unicode_width::UnicodeWidthStr::width(text.as_str()));
                        let tinted = style.bg.is_some();
                        let mut spans = vec![Span::styled(text, style)];
                        if tinted && pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), style));
                        }
                        lines.push(Line::from(spans));
                    }
                    if shown < diff_lines.len() {
                        lines.push(Line::from(Span::styled(
                            format!("… {} more diff lines", diff_lines.len() - shown),
                            theme.muted_style(),
                        )));
                    }
                }
            }
            frame.render_widget(Paragraph::new(lines), body);

            let legend_text = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("[y]", theme.fg(theme.success).add_modifier(Modifier::BOLD)),
                    Span::styled(" allow once   ", theme.fg(theme.text)),
                    Span::styled("[a]", theme.fg(theme.warning).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(" allow {} for this session   ", request.class.as_str()),
                        theme.fg(theme.text),
                    ),
                    Span::styled(
                        "[n]/Esc",
                        theme.fg(theme.error).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" deny", theme.fg(theme.text)),
                ]),
            ];
            frame.render_widget(Paragraph::new(legend_text), legend);
        },
    );
}
