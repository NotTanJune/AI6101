//! The empty-session banner.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::truncate;
use crate::anim;
use crate::app::App;
use crate::theme::{Theme, lerp};

/// Title of the banner.
pub const TITLE: &str = "mcpe";

fn hint_line(theme: &Theme, keys: &str, what: &str, fade: f32) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{keys:<12}"),
            theme.fg(lerp(theme.bg, theme.accent, fade)),
        ),
        Span::styled(what.to_owned(), theme.fg(lerp(theme.bg, theme.muted, fade))),
    ])
}

/// Title spans: a gradient from the accent to the second accent colour, with a highlight that
/// sweeps across once while the banner is new.
fn title_spans(theme: &Theme, frame: u64) -> Vec<Span<'static>> {
    let fade = anim::banner_fade(frame);
    let len = TITLE.chars().count();
    let center = anim::shimmer_center(frame, len);
    TITLE
        .chars()
        .enumerate()
        .map(|(i, c)| {
            #[allow(clippy::cast_precision_loss)]
            let t = if len > 1 {
                i as f32 / (len - 1) as f32
            } else {
                0.0
            };
            let mut color = lerp(theme.accent, theme.accent_alt, t);
            if let Some(center) = center {
                let distance = (i64::try_from(i).unwrap_or(0) - center).abs();
                if distance <= 2 {
                    #[allow(clippy::cast_precision_loss)]
                    let glow = 1.0 - distance as f32 / 3.0;
                    color = lerp(color, theme.text, glow * 0.85);
                }
            }
            Span::styled(
                c.to_string(),
                theme
                    .fg(lerp(theme.bg, color, fade))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

pub(crate) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let fade = anim::banner_fade(app.frame);
    let width = usize::from(area.width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(title_spans(theme, app.frame)));
    lines.push(Line::from(Span::styled(
        format!(
            "coding agent · {} · effort {}",
            app.status.model_alias, app.status.effort
        ),
        theme.fg(lerp(theme.bg, theme.text, fade)),
    )));
    lines.push(Line::from(Span::styled(
        truncate(&app.workspace, width.saturating_sub(2)),
        theme.fg(lerp(theme.bg, theme.muted, fade)),
    )));
    lines.push(Line::from(""));
    lines.push(hint_line(theme, "/help", "commands and keys", fade));
    lines.push(hint_line(theme, "/model", "switch model", fade));
    lines.push(hint_line(theme, "Alt+Enter", "new line", fade));

    // Left-align the block and centre it as a whole; the title and subtitle share the left edge
    // of the hints so the block reads as one unit.
    let block_width = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.to_string().as_str()))
        .max()
        .unwrap_or(0)
        .min(width);
    let x_pad = u16::try_from(width.saturating_sub(block_width) / 2).unwrap_or(0);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    // A little above the vertical centre.
    let y_pad = (area.height.saturating_sub(height)) / 3;
    let rect = Rect {
        x: area.x + x_pad,
        y: area.y + y_pad,
        width: area.width - x_pad,
        height,
    };
    frame.render_widget(Paragraph::new(lines), rect);
}
