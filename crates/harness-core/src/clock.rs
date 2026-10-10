//! Time source abstraction so event output can be deterministic in tests and smoke runs.

use chrono::{SecondsFormat, Utc};

/// Source of RFC 3339 timestamps.
pub trait Clock: Send + Sync {
    /// Current time as an RFC 3339 string (for example `2026-01-01T00:00:00Z`).
    fn now_rfc3339(&self) -> String;
}

/// Wall-clock UTC time with millisecond precision.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

/// Clock that always returns the same timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedClock {
    ts: String,
}

impl FixedClock {
    /// Default timestamp used by [`FixedClock::default`].
    pub const DEFAULT_TS: &'static str = "2026-01-01T00:00:00Z";

    /// Create a clock frozen at `ts` (an RFC 3339 string).
    pub fn new(ts: impl Into<String>) -> Self {
        Self { ts: ts.into() }
    }
}

impl Default for FixedClock {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TS)
    }
}

impl Clock for FixedClock {
    fn now_rfc3339(&self) -> String {
        self.ts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_is_constant() {
        let clock = FixedClock::new("2026-02-03T04:05:06Z");
        assert_eq!(clock.now_rfc3339(), "2026-02-03T04:05:06Z");
        assert_eq!(clock.now_rfc3339(), clock.now_rfc3339());
    }

    #[test]
    fn system_clock_is_rfc3339_utc() {
        let ts = SystemClock.now_rfc3339();
        assert!(ts.ends_with('Z'), "{ts}");
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "{ts}");
    }
}
