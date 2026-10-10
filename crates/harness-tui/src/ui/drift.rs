//! The drift overlay: changes between the KB and a live server, strategy actions and the
//! canary result.

use harness_core::CanaryStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{padded, popup, truncate, wrap};
use crate::app::{App, DriftPhase, DriftState, DriftView};
use crate::bridge::CanarySummary;
use crate::theme::{Rgb, Theme};

/// Lowercase label of a canary status.
pub(crate) fn status_label(status: CanaryStatus) -> &'static str {
    match status {
        CanaryStatus::Pass => "pass",
        CanaryStatus::Fail => "fail",
        CanaryStatus::Inconclusive => "inconclusive",
        CanaryStatus::NotApplicable => "not applicable",
    }
}

fn status_color(theme: &Theme, status: CanaryStatus) -> Rgb {
    match status {
        CanaryStatus::Pass => theme.success,
        CanaryStatus::Fail => theme.error,
        CanaryStatus::Inconclusive => theme.warning,
        CanaryStatus::NotApplicable => theme.muted,
    }
}

fn bold(theme: &Theme) -> Style {
    theme.fg(theme.accent_alt).add_modifier(Modifier::BOLD)
}

fn plain(theme: &Theme, text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), theme.fg(theme.text)))
}

fn change_lines(theme: &Theme, state: &DriftState, cap: usize, width: usize) -> Vec<Line<'static>> {
    let changes = &state.report.diff.changes;
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "Changes ({} KB card(s), {} live tool(s)):",
            state.report.kb_cards, state.report.live_tools
        ),
        bold(theme),
    ))];
    for change in changes.iter().take(cap) {
        let field = change.field_path.as_deref().unwrap_or("(whole tool)");
        let class = format!("{:?}", change.class).to_lowercase();
        let kind = format!("{:?}", change.kind).to_lowercase();
        lines.push(plain(
            theme,
            truncate(
                &format!("  {}  {field}  [{class}, {kind}]", change.tool_id),
                width,
            ),
        ));
    }
    if changes.len() > cap {
        lines.push(Line::from(Span::styled(
            format!("  … {} more change(s)", changes.len() - cap),
            theme.muted_style(),
        )));
    }
    lines
}

fn canary_lines(theme: &Theme, summary: &CanarySummary, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("Strategy {}  canary ", summary.strategy),
            bold(theme),
        ),
        Span::styled(
            status_label(summary.status),
            theme
                .fg(status_color(theme, summary.status))
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for probe in &summary.probes {
        let (mark, color) = match probe.status {
            CanaryStatus::Pass => ("✓", theme.success),
            CanaryStatus::Fail => ("✗", theme.error),
            _ => ("?", theme.warning),
        };
        let name = probe.probe_id.as_deref().unwrap_or("(no probe)");
        let kind = if probe.is_control { " control" } else { "" };
        let observed = probe.actual.as_deref().unwrap_or("no observation");
        lines.push(Line::from(vec![
            Span::styled(format!("  {mark} "), theme.fg(color)),
            Span::styled(
                truncate(
                    &format!("{name}{kind}: {observed}"),
                    width.saturating_sub(4),
                ),
                theme.fg(theme.text),
            ),
        ]));
    }
    for part in wrap(&format!("Decision: {}", summary.decision), width) {
        lines.push(plain(theme, part));
    }
    if summary.collateral_damage {
        lines.push(Line::from(Span::styled(
            "A control probe failed: the candidate damaged an unchanged tool.",
            theme.fg(theme.error),
        )));
    }
    lines
}

const WIDTH: u16 = 110;

/// Most change rows listed before `… N more`.
const MAX_CHANGE_ROWS: usize = 10;

/// Detail lines of the selected server: changes, then the running note or the canary report.
fn detail_lines(app: &App, state: &DriftState, width: usize, rows: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines = match &state.phase {
        DriftPhase::Ready(_) => change_lines(theme, state, 4, width),
        _ => change_lines(
            theme,
            state,
            rows.saturating_sub(1).clamp(1, MAX_CHANGE_ROWS),
            width,
        ),
    };
    if let DriftPhase::Ready(summary) = &state.phase {
        lines.push(Line::from(""));
        lines.extend(canary_lines(theme, summary, width));
    }
    if let DriftPhase::Running(strategy) = &state.phase {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", crate::anim::spinner_glyph(app.frame)),
                theme.fg(crate::theme::lerp(
                    theme.accent,
                    theme.accent_alt,
                    crate::anim::gradient_t(app.frame),
                )),
            ),
            Span::styled(
                format!("Running {strategy} and the canary…"),
                theme.fg(theme.warning),
            ),
        ]));
    }
    lines
}

/// Popup height that fits the list, the detail and the action rows.
fn wanted_height(area: Rect, app: &App, view: &DriftView) -> u16 {
    if app.drifts.is_empty() {
        return 4 + 3;
    }
    let width = usize::from(popup::inner_width(area, WIDTH));
    let selected = view.selected.min(app.drifts.len() - 1);
    let list = app.drifts.len().min(5);
    let detail = app
        .drifts
        .values()
        .nth(selected)
        .map_or(1, |state| detail_lines(app, state, width, usize::MAX).len());
    u16::try_from(4 + list + 1 + detail + 1 + 3).unwrap_or(u16::MAX)
}

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App, view: &DriftView) {
    popup::show(
        frame,
        app,
        area,
        &popup::Spec {
            width: WIDTH,
            height: wanted_height(area, app, view),
            title: "Drift",
            accent: app.theme.accent_alt,
        },
        |frame, inner| draw(frame, inner, app, view),
    );
}

fn draw(frame: &mut Frame, inner: Rect, app: &App, view: &DriftView) {
    let theme = &app.theme;
    if app.drifts.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                plain(
                    theme,
                    "No drift detected between the session KB and the connected MCP servers.",
                ),
                Line::from(""),
                Line::from(Span::styled("Esc closes this view.", theme.muted_style())),
            ])
            .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        return;
    }
    let width = usize::from(inner.width);
    let list_height = u16::try_from(app.drifts.len().min(5)).unwrap_or(5);
    let [list_area, _gap, detail_area, action_area] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(inner);

    let selected = view.selected.min(app.drifts.len() - 1);
    let rows: Vec<Line> = app
        .drifts
        .iter()
        .enumerate()
        .map(|(i, (server, state))| {
            let phase = match &state.phase {
                DriftPhase::Idle => "needs a decision",
                DriftPhase::Running(_) => "running",
                DriftPhase::Ready(_) => "candidate ready",
            };
            let marker = if i == selected { "› " } else { "  " };
            let text = truncate(
                &format!(
                    "{marker}{server}  {} change(s)  {phase}",
                    state.report.diff.changes.len()
                ),
                width,
            );
            if i == selected {
                let style = theme.selected_style();
                padded(vec![Span::styled(text, style)], width, style)
            } else {
                plain(theme, text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);

    let Some((_, state)) = app.drifts.iter().nth(selected) else {
        return;
    };
    let mut lines = detail_lines(app, state, width, usize::from(detail_area.height));
    let room = usize::from(detail_area.height);
    if lines.len() > room && room > 0 {
        let hidden = lines.len() - (room - 1);
        lines.truncate(room - 1);
        lines.push(Line::from(Span::styled(
            format!("… {hidden} more lines"),
            theme.muted_style(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), detail_area);

    let actions = match &state.phase {
        DriftPhase::Idle => vec![
            plain(
                theme,
                "[n] no update  [f] full rewrite  [s] selective patch",
            ),
            hint(theme, "Up/Down choose server, Esc close"),
        ],
        DriftPhase::Running(_) => vec![hint(theme, "Waiting for the strategy and the canary…")],
        DriftPhase::Ready(summary) if summary.can_accept => vec![
            plain(theme, "[a] accept  [r] reject"),
            hint(theme, "[n]/[f]/[s] run another strategy, Esc close"),
        ],
        DriftPhase::Ready(_) => vec![
            plain(theme, "[r] reject  (this candidate cannot be accepted)"),
            hint(theme, "[n]/[f]/[s] run another strategy, Esc close"),
        ],
    };
    frame.render_widget(Paragraph::new(actions), action_area);
}

fn hint(theme: &Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), theme.muted_style()))
}
