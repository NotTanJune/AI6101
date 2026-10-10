//! Shared popup frame for every overlay.
//!
//! Root cause of the old overlap: popups were a bordered `Paragraph` drawn straight over the
//! transcript with no inner padding and nothing that separated them from the text below. A
//! popup now (1) dims what is already on screen, (2) clears and fills its own rectangle with the
//! raised surface colour, (3) draws a rounded border and (4) hands the caller an inner rectangle
//! with one column and one row of padding on every side. While it opens, its colours fade in
//! from the app's open-animation progress.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding};

use crate::anim;
use crate::app::App;
use crate::theme::{Rgb, Theme, color_to_rgb, lerp};

/// Smallest popup that is still drawn.
const MIN_WIDTH: u16 = 8;
const MIN_HEIGHT: u16 = 5;

/// Fraction of the text colour that is kept under a popup (the rest blends into the background).
const BACKDROP_KEEP: f32 = 0.45;

/// Largest size a popup may take inside `area`: a clear margin on every side (three columns and
/// two rows) when the terminal is big enough, one cell when it is small.
fn max_size(area: Rect) -> (u16, u16) {
    let max_w = if area.width >= 40 {
        area.width - 6
    } else {
        area.width.saturating_sub(2).max(area.width.min(MIN_WIDTH))
    };
    let max_h = if area.height >= 16 {
        area.height - 4
    } else {
        area.height
            .saturating_sub(2)
            .max(area.height.min(MIN_HEIGHT))
    };
    (max_w, max_h)
}

/// The rectangle of a popup of at most `want_w` x `want_h`, centred in `area` and kept away from
/// the terminal edge so the dimmed screen frames it.
pub(crate) fn rect(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let (max_w, max_h) = max_size(area);
    let width = want_w.min(max_w).min(area.width);
    let height = want_h.min(max_h).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Inner (content) width of a popup of `want_w` columns in `area`.
pub(crate) fn inner_width(area: Rect, want_w: u16) -> u16 {
    inner(rect(area, want_w, MIN_HEIGHT)).width
}

/// The content area inside a popup rectangle: border plus one cell of padding on every side.
pub(crate) fn inner(popup: Rect) -> Rect {
    Rect {
        x: popup.x.saturating_add(2),
        y: popup.y.saturating_add(2),
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(4),
    }
}

/// Blend the colours of every cell in `region` toward the theme background by `amount`
/// (`0.0` leaves them alone, `1.0` removes the text). Without colours, cells get `DIM`.
pub(crate) fn dim_region(buf: &mut Buffer, region: Rect, theme: &Theme, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let region = region.intersection(buf.area);
    for y in region.top()..region.bottom() {
        for x in region.left()..region.right() {
            let cell = &mut buf[(x, y)];
            if theme.no_color {
                cell.modifier.insert(Modifier::DIM);
                continue;
            }
            let fg = color_to_rgb(cell.fg).unwrap_or(theme.text);
            let bg = color_to_rgb(cell.bg).unwrap_or(theme.bg);
            cell.set_fg(theme.resolve(lerp(fg, theme.bg, amount)));
            cell.set_bg(theme.resolve(lerp(bg, theme.bg, amount * 0.6)));
        }
    }
}

/// Blend the foreground of every cell in `region` toward `toward` by `amount`.
fn fade_region(buf: &mut Buffer, region: Rect, theme: &Theme, toward: Rgb, amount: f32) {
    if amount <= 0.0 || theme.no_color {
        return;
    }
    let region = region.intersection(buf.area);
    for y in region.top()..region.bottom() {
        for x in region.left()..region.right() {
            let cell = &mut buf[(x, y)];
            let fg = color_to_rgb(cell.fg).unwrap_or(theme.text);
            cell.set_fg(theme.resolve(lerp(fg, toward, amount)));
        }
    }
}

/// Size, title and accent colour of a popup.
pub(crate) struct Spec<'a> {
    /// Wanted outer width (clamped to the terminal).
    pub width: u16,
    /// Wanted outer height (clamped to the terminal).
    pub height: u16,
    /// Title shown in the top border.
    pub title: &'a str,
    /// Colour of the border and the title.
    pub accent: Rgb,
}

/// Draw a popup over the whole `area`, then let `body` fill the padded inner rectangle.
pub(crate) fn show<F>(frame: &mut Frame, app: &App, area: Rect, spec: &Spec<'_>, body: F)
where
    F: FnOnce(&mut Frame, Rect),
{
    let (want_w, want_h, title, accent) = (spec.width, spec.height, spec.title, spec.accent);
    let theme = &app.theme;
    let open = anim::open_progress(app.frame, app.overlay_opened_at);
    dim_region(
        frame.buffer_mut(),
        area,
        theme,
        (1.0 - BACKDROP_KEEP) * open,
    );

    let popup = rect(area, want_w, want_h);
    if popup.width < MIN_WIDTH || popup.height < MIN_HEIGHT {
        return;
    }
    frame.render_widget(Clear, popup);
    let border = lerp(theme.raised, accent, open);
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            title.to_owned(),
            theme.on(accent, theme.raised).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.on(border, theme.raised))
        .style(theme.on(theme.text, theme.raised))
        .title(title_line)
        .padding(Padding::uniform(1));
    let content = block.inner(popup);
    frame.render_widget(block, popup);
    body(frame, content);
    if open < 1.0 {
        fade_region(frame.buffer_mut(), popup, theme, theme.raised, 1.0 - open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_is_clamped_to_the_area_and_centred() {
        let area = Rect::new(0, 0, 100, 30);
        let r = rect(area, 60, 10);
        assert_eq!((r.width, r.height), (60, 10));
        assert_eq!((r.x, r.y), (20, 10));
        let big = rect(area, 500, 500);
        assert_eq!(
            (big.width, big.height),
            (94, 26),
            "three columns and two rows of margin on a big terminal"
        );
        let tiny = rect(Rect::new(0, 0, 6, 4), 50, 50);
        assert!(tiny.width <= 6 && tiny.height <= 4);
    }

    #[test]
    fn inner_is_padded_by_two_cells_on_every_side() {
        let popup = Rect::new(10, 5, 40, 12);
        assert_eq!(inner(popup), Rect::new(12, 7, 36, 8));
        assert_eq!(inner(Rect::new(0, 0, 3, 3)), Rect::new(2, 2, 0, 0));
    }
}
