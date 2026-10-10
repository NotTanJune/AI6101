//! The one-line status bar: styled segments on a raised background.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim;
use crate::app::{App, Entry};
use crate::theme::{Rgb, Theme, lerp};

/// Token count in a compact form (`950`, `12.3k`, `1.2M`).
pub(crate) fn tokens(count: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = count as f64;
    if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", value / 1_000.0)
    } else {
        format!("{:.1}M", value / 1_000_000.0)
    }
}

/// Colour of the spend figure: calm while there is plenty of budget left, amber past half the
/// cap and red as the cap is reached.
pub(crate) fn cost_color(theme: &Theme, spent: f64, cap: f64) -> Rgb {
    let ratio = if cap > 0.0 && spent.is_finite() {
        (spent / cap).clamp(0.0, 1.0)
    } else {
        0.0
    };
    #[allow(clippy::cast_possible_truncation)]
    let ratio = ratio as f32;
    if ratio < 0.5 {
        theme.text
    } else if ratio < 0.8 {
        lerp(theme.text, theme.warning, (ratio - 0.5) / 0.3)
    } else {
        lerp(theme.warning, theme.error, (ratio - 0.8) / 0.2)
    }
}

fn tool_in_flight(app: &App) -> bool {
    app.transcript
        .iter()
        .rev()
        .take(8)
        .any(|e| matches!(e, Entry::ToolCard(card) if card.output.is_none()))
}

/// The status bar as `(text, style)` segments.
fn segments(app: &App) -> Vec<(String, Style)> {
    let theme = &app.theme;
    let s = &app.status;
    let d = &app.display;
    let bar = theme.bg_style(theme.raised);
    let text = theme.on(theme.text, theme.raised);
    let muted = if theme.no_color {
        text.add_modifier(Modifier::DIM)
    } else {
        theme.on(theme.muted, theme.raised)
    };
    let mut out: Vec<(String, Style)> = Vec::new();
    // Model and effort chip.
    let chip = if theme.no_color {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        theme
            .on(theme.bg, theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    out.push((format!(" {}/{} ", s.model_alias, s.effort), chip));
    out.push(("  ".to_owned(), bar));
    out.push(("tokens ".to_owned(), muted));
    out.push((
        format!("in {} out {}", tokens(d.tokens_in), tokens(d.tokens_out)),
        text,
    ));
    out.push(("  ".to_owned(), bar));
    out.push((
        format!("${:.4}", d.spent_usd),
        theme
            .on(cost_color(theme, d.spent_usd, s.cap_usd), theme.raised)
            .add_modifier(Modifier::BOLD),
    ));
    out.push((format!(" / ${:.2}", s.cap_usd), muted));
    out.push(("  ".to_owned(), bar));
    // MCP: a coloured dot that pulses while a tool call is in flight.
    let dot_color = if s.mcp_total == 0 {
        theme.muted
    } else if s.mcp_up == s.mcp_total {
        theme.success
    } else if s.mcp_up > 0 {
        theme.warning
    } else {
        theme.error
    };
    let dot_color = if s.mcp_up > 0 && tool_in_flight(app) {
        lerp(dot_color, theme.raised, 0.6 * anim::dot_pulse(app.frame))
    } else {
        dot_color
    };
    out.push(("●".to_owned(), theme.on(dot_color, theme.raised)));
    out.push((format!(" MCP {}/{}", s.mcp_up, s.mcp_total), text));
    out.push(("  ".to_owned(), bar));
    if s.drift_count > 0 {
        let drift = if theme.no_color {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme
                .on(theme.bg, theme.warning)
                .add_modifier(Modifier::BOLD)
        };
        out.push((format!(" drift {} ", s.drift_count), drift));
    } else {
        out.push(("drift 0".to_owned(), muted));
    }
    out
}

pub(crate) fn render(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = usize::from(area.width);
    let bar = app.theme.bg_style(app.theme.raised);
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0;
    for (text, style) in segments(app) {
        if used >= width {
            break;
        }
        let room = width - used;
        let text = clip(&text, room);
        used += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

/// Cut `text` to at most `max` display columns.
fn clip(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > max {
            break;
        }
        out.push(c);
        used += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_colour_shifts_toward_warning_then_error_as_spend_nears_the_cap() {
        let theme = Theme::default();
        assert_eq!(cost_color(&theme, 0.0, 5.0), theme.text);
        assert_eq!(cost_color(&theme, 2.4, 5.0), theme.text);
        assert_eq!(cost_color(&theme, 4.0, 5.0), theme.warning);
        assert_eq!(cost_color(&theme, 5.0, 5.0), theme.error);
        assert_eq!(cost_color(&theme, 9.0, 5.0), theme.error);
        let mid = cost_color(&theme, 3.25, 5.0);
        assert_ne!(mid, theme.text);
        assert_ne!(mid, theme.warning);
        assert_eq!(cost_color(&theme, 1.0, 0.0), theme.text);
    }

    #[test]
    fn tokens_are_compact() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_300), "12.3k");
        assert_eq!(tokens(1_200_000), "1.2M");
    }
}
