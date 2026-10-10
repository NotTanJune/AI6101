//! Animation helpers. Every function here is a pure function of a frame counter or of the
//! current and target values: no clocks, no state. The application advances `App::frame` once
//! per tick, so animations are deterministic and tests step frames instead of waiting.
//!
//! Every animation ends in an exact final state: progress reaches exactly `1.0`, eased numbers
//! reach exactly their target and settled frames are stable.

/// Length of the popup open animation in frames (about 165 ms at the 33 ms tick).
pub const OPEN_FRAMES: u64 = 5;

/// Frames before the welcome banner starts to appear.
pub const BANNER_DELAY_FRAMES: u64 = 2;

/// Length of the welcome banner fade-in in frames.
pub const BANNER_FADE_FRAMES: u64 = 14;

/// Frame at which the banner shimmer starts.
pub const BANNER_SHIMMER_START: u64 = 16;

/// Frames the banner shimmer sweep takes.
pub const BANNER_SHIMMER_FRAMES: u64 = 30;

/// Braille spinner frames.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Clamp to `0.0..=1.0`; NaN becomes `0.0`.
pub fn clamp01(t: f32) -> f32 {
    if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) }
}

/// Cubic ease-out: fast start, gentle stop. `0.0` maps to `0.0` and `1.0` to exactly `1.0`.
pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - clamp01(t);
    1.0 - u * u * u
}

/// Linear progress of an animation that started at frame `start` and lasts `duration` frames.
/// Before the start it is `0.0`; at and after the end it is exactly `1.0`.
#[allow(clippy::cast_precision_loss)]
pub fn progress(frame: u64, start: u64, duration: u64) -> f32 {
    if duration == 0 {
        return 1.0;
    }
    let elapsed = frame.saturating_sub(start);
    if elapsed >= duration {
        1.0
    } else {
        elapsed as f32 / duration as f32
    }
}

/// Eased progress of a popup that opened at frame `opened_at`.
pub fn open_progress(frame: u64, opened_at: u64) -> f32 {
    ease_out_cubic(progress(frame, opened_at, OPEN_FRAMES))
}

/// The braille glyph for `frame` (advances every two frames).
pub fn spinner_glyph(frame: u64) -> &'static str {
    let index = usize::try_from((frame / 2) % SPINNER.len() as u64).unwrap_or(0);
    SPINNER[index]
}

/// A triangle wave over `period` frames: `0.0` at frame 0, `1.0` at half the period, back to
/// `0.0`. A `period` below 2 gives `0.0`.
#[allow(clippy::cast_precision_loss)]
pub fn triangle(frame: u64, period: u64) -> f32 {
    if period < 2 {
        return 0.0;
    }
    let phase = frame % period;
    let half = period as f32 / 2.0;
    let p = phase as f32;
    if p <= half {
        p / half
    } else {
        (period as f32 - p) / half
    }
}

/// Position in the accent gradient used by the spinner (`0.0..=1.0`, a slow wave).
pub fn gradient_t(frame: u64) -> f32 {
    triangle(frame, 40)
}

/// Brightness of the streaming cursor (`0.0..=1.0`).
pub fn cursor_pulse(frame: u64) -> f32 {
    triangle(frame, 20)
}

/// Brightness of the MCP activity dot (`0.0..=1.0`).
pub fn dot_pulse(frame: u64) -> f32 {
    triangle(frame, 30)
}

/// Move `current` toward `target` by about 40 percent of the distance, at least one step.
/// Monotonic, and it reaches `target` exactly.
pub fn ease_usize(current: usize, target: usize) -> usize {
    if current == target {
        return current;
    }
    let distance = current.abs_diff(target);
    let step = (distance * 2 / 5).max(1);
    if current < target {
        current + step
    } else {
        current - step
    }
}

/// [`ease_usize`] for token counts.
pub fn ease_u64(current: u64, target: u64) -> u64 {
    if current == target {
        return current;
    }
    let distance = current.abs_diff(target);
    let step = (distance * 2 / 5).max(1);
    if current < target {
        current + step
    } else {
        current - step
    }
}

/// Move `current` toward `target` by 40 percent of the distance and snap to `target` once
/// within `epsilon`. Monotonic, and it reaches `target` exactly.
pub fn ease_f64(current: f64, target: f64, epsilon: f64) -> f64 {
    let distance = target - current;
    if !distance.is_finite() || distance.abs() <= epsilon {
        return target;
    }
    current + distance * 0.4
}

/// Brightness of the welcome banner (`0.0..=1.0`): fades in after a short delay.
pub fn banner_fade(frame: u64) -> f32 {
    ease_out_cubic(progress(frame, BANNER_DELAY_FRAMES, BANNER_FADE_FRAMES))
}

/// Column of the shimmer highlight over a title of `len` columns, or `None` while no shimmer
/// is running (before it starts and after it has finished). The sweep starts left of the title
/// and leaves on the right.
pub fn shimmer_center(frame: u64, len: usize) -> Option<i64> {
    if !(BANNER_SHIMMER_START..BANNER_SHIMMER_START + BANNER_SHIMMER_FRAMES).contains(&frame) {
        return None;
    }
    let t = progress(frame, BANNER_SHIMMER_START, BANNER_SHIMMER_FRAMES);
    let span = i64::try_from(len).unwrap_or(0) + 6;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let at = (t * span as f32).round() as i64;
    Some(at - 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_is_monotonic_and_exact_at_the_ends() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        let mut last = 0.0;
        for i in 0..=100 {
            #[allow(clippy::cast_precision_loss)]
            let value = ease_out_cubic(i as f32 / 100.0);
            assert!(value >= last);
            last = value;
        }
        assert!(ease_out_cubic(0.5) > 0.5, "ease-out starts fast");
    }

    #[test]
    fn open_progress_rises_monotonically_and_finishes_at_exactly_one() {
        let mut last = -1.0;
        for frame in 10..(10 + OPEN_FRAMES + 3) {
            let value = open_progress(frame, 10);
            assert!(value >= last, "frame {frame}: {value} < {last}");
            last = value;
        }
        assert_eq!(open_progress(10, 10), 0.0);
        assert_eq!(open_progress(10 + OPEN_FRAMES, 10), 1.0);
        assert_eq!(open_progress(10_000, 10), 1.0);
        assert_eq!(open_progress(0, 10), 0.0, "not started yet");
    }

    #[test]
    fn progress_handles_zero_duration() {
        assert_eq!(progress(0, 0, 0), 1.0);
    }

    #[test]
    fn the_spinner_cycles_through_all_ten_glyphs() {
        let glyphs: std::collections::BTreeSet<&str> = (0..20).map(spinner_glyph).collect();
        assert_eq!(glyphs.len(), SPINNER.len());
        assert_eq!(spinner_glyph(0), spinner_glyph(1));
        assert_ne!(spinner_glyph(0), spinner_glyph(2));
        assert_eq!(spinner_glyph(0), spinner_glyph(20));
    }

    #[test]
    fn triangle_waves_between_zero_and_one() {
        assert_eq!(triangle(0, 20), 0.0);
        assert_eq!(triangle(10, 20), 1.0);
        assert_eq!(triangle(20, 20), 0.0);
        assert_eq!(triangle(5, 1), 0.0);
        for frame in 0..100 {
            let v = triangle(frame, 20);
            assert!((0.0..=1.0).contains(&v));
            assert!((0.0..=1.0).contains(&gradient_t(frame)));
            assert!((0.0..=1.0).contains(&cursor_pulse(frame)));
            assert!((0.0..=1.0).contains(&dot_pulse(frame)));
        }
    }

    #[test]
    fn eased_integers_are_monotonic_and_land_exactly() {
        for (start, target) in [(0usize, 1000usize), (1000, 0), (5, 6), (6, 5), (7, 7)] {
            let mut value = start;
            let mut steps = 0;
            let mut last_distance = value.abs_diff(target);
            while value != target {
                value = ease_usize(value, target);
                let distance = value.abs_diff(target);
                assert!(distance < last_distance, "must get closer every step");
                last_distance = distance;
                steps += 1;
                assert!(steps < 100, "must finish");
            }
            assert_eq!(value, target);
        }
        let mut tokens = 0u64;
        while tokens != 123_456 {
            let next = ease_u64(tokens, 123_456);
            assert!(next > tokens && next <= 123_456);
            tokens = next;
        }
    }

    #[test]
    fn eased_floats_are_monotonic_and_land_exactly() {
        let mut value = 0.0;
        let mut steps = 0;
        while value != 1.2345 {
            let next = ease_f64(value, 1.2345, 0.00005);
            assert!(next > value && next <= 1.2345);
            value = next;
            steps += 1;
            assert!(steps < 60, "must finish within a few dozen frames");
        }
        let mut down = 3.0;
        while down != 0.5 {
            let next = ease_f64(down, 0.5, 0.00005);
            assert!(next < down && next >= 0.5);
            down = next;
        }
        assert!(ease_f64(1.0, f64::NAN, 0.1).is_nan());
    }

    #[test]
    fn the_banner_fades_in_and_the_shimmer_ends() {
        assert_eq!(banner_fade(0), 0.0);
        let mut last = 0.0;
        for frame in 0..(BANNER_DELAY_FRAMES + BANNER_FADE_FRAMES + 2) {
            let v = banner_fade(frame);
            assert!(v >= last);
            last = v;
        }
        assert_eq!(banner_fade(BANNER_DELAY_FRAMES + BANNER_FADE_FRAMES), 1.0);
        assert_eq!(banner_fade(100_000), 1.0);
        assert_eq!(shimmer_center(0, 10), None);
        assert!(shimmer_center(BANNER_SHIMMER_START, 10).is_some());
        let mut last = i64::MIN;
        for frame in BANNER_SHIMMER_START..(BANNER_SHIMMER_START + BANNER_SHIMMER_FRAMES) {
            let at = shimmer_center(frame, 10).expect("running");
            assert!(at >= last, "the highlight only moves right");
            last = at;
        }
        assert_eq!(
            shimmer_center(BANNER_SHIMMER_START + BANNER_SHIMMER_FRAMES, 10),
            None
        );
        assert_eq!(shimmer_center(100_000, 10), None);
    }
}
