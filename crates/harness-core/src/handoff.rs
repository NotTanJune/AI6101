//! Offline validation of the two Rust/Python handoff artifacts.
//!
//! This checks file contracts and log consistency. It does not execute tools, judge tasks,
//! require a completed run, or prove that a log was generated using the supplied Tool KB.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::error::HarnessError;
use crate::events::{EventBody, EventError, EventSource, Metric, read_events};
use crate::model::ToolKb;

/// Counts observed in valid handoff files, without tool arguments, answers or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandoffSummary {
    /// Schema version read from the Tool KB.
    pub tool_kb_schema_version: String,
    /// Schema version read from the event log.
    pub run_event_schema_version: String,
    /// Tool cards present, including cards for different servers.
    pub tool_cards: usize,
    /// Total JSONL events.
    pub events: usize,
    /// Distinct run IDs observed.
    pub runs: usize,
    /// Distinct `(run_id, source)` sequence streams.
    pub streams: usize,
    /// Number of events from each producer present in the file.
    pub source_event_counts: BTreeMap<EventSource, usize>,
    /// Number of score events for each metric present in the file, not aggregate scores.
    pub metric_event_counts: BTreeMap<Metric, usize>,
}

/// Failure to validate a handoff. Messages intentionally omit untrusted file contents.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// The Tool KB could not be loaded or did not satisfy its contract.
    #[error("Tool KB: {reason}")]
    ToolKb {
        /// A fixed diagnostic that does not echo the input payload.
        reason: &'static str,
    },
    /// The event log could not be opened.
    #[error("event log: cannot read file")]
    ReadEvents,
    /// One event failed the existing run-event parser.
    #[error("event log line {line}: {reason}")]
    InvalidEvent {
        /// One-based line number.
        line: usize,
        /// A fixed diagnostic that does not echo the input payload.
        reason: &'static str,
    },
    /// An empty log cannot demonstrate the event contract.
    #[error("event log: no events")]
    EmptyEvents,
    /// Sequence numbers repeated or moved backwards within one producer's run.
    #[error(
        "event log line {line}: sequence {seq} must be greater than previous sequence {previous} in the same run/source"
    )]
    Sequence {
        /// One-based line number.
        line: usize,
        /// Invalid sequence number.
        seq: u64,
        /// Last sequence number in the same `(run_id, source)` stream.
        previous: u64,
    },
    /// A canary refers to evidence absent from the earlier part of its stream.
    #[error(
        "event log line {line}: event reference {reference} does not identify an earlier event in the same run/source"
    )]
    MissingReference {
        /// One-based line number.
        line: usize,
        /// Missing or forward sequence reference.
        reference: u64,
    },
}

fn kb_error(error: HarnessError) -> HandoffError {
    let reason = match error {
        HarnessError::Io { .. } => "cannot read file",
        HarnessError::Json(_) => "invalid JSON",
        HarnessError::Schema { .. } => "schema validation failed (expected tool-kb/1)",
        _ => "validation failed",
    };
    HandoffError::ToolKb { reason }
}

fn event_error(error: EventError) -> HandoffError {
    if matches!(error, EventError::Io { .. }) {
        return HandoffError::ReadEvents;
    }
    let line = error.line().unwrap_or(0);
    let reason = match error {
        EventError::BlankLine { .. } => "blank line",
        EventError::BadJson { .. } => "invalid JSON",
        EventError::UnsupportedSchemaVersion { .. } => {
            "unsupported schema version (expected run-event/1)"
        }
        EventError::Schema { .. } => "schema validation failed",
        EventError::Decode { .. } => "cannot decode event",
        _ => "validation failed",
    };
    HandoffError::InvalidEvent { line, reason }
}

/// Read and validate a Tool KB and a nonempty run-event JSONL file.
///
/// Sequences must increase strictly within each `(run_id, source)` stream. The first sequence
/// need not be zero, and gaps are allowed. Each canary `event_refs` entry must identify an
/// earlier event in that same stream; the wire format has no field for another source.
/// Related-event types are not narrowed beyond the existing schema. No completion verdict,
/// metric, specific producer, or terminal event is required.
pub fn check_handoff(
    kb_path: impl AsRef<Path>,
    events_path: impl AsRef<Path>,
) -> Result<HandoffSummary, HandoffError> {
    let kb = ToolKb::read(kb_path).map_err(kb_error)?;
    let events = read_events(events_path).map_err(event_error)?;
    let Some(first) = events.first() else {
        return Err(HandoffError::EmptyEvents);
    };
    let mut summary = HandoffSummary {
        tool_kb_schema_version: kb.schema_version,
        run_event_schema_version: first.schema_version.clone(),
        tool_cards: kb.cards.len(),
        events: events.len(),
        runs: 0,
        streams: 0,
        source_event_counts: BTreeMap::new(),
        metric_event_counts: BTreeMap::new(),
    };
    let mut runs = BTreeSet::new();
    let mut seen: BTreeMap<(&str, EventSource), BTreeSet<u64>> = BTreeMap::new();
    for (index, event) in events.iter().enumerate() {
        let line = index + 1;
        let stream = seen.entry((&event.run_id, event.source)).or_default();
        if let Some(&previous) = stream.last()
            && event.seq <= previous
        {
            return Err(HandoffError::Sequence {
                line,
                seq: event.seq,
                previous,
            });
        }
        if let EventBody::CanaryResult(canary) = &event.body {
            for &reference in &canary.event_refs {
                if !stream.contains(&reference) {
                    return Err(HandoffError::MissingReference { line, reference });
                }
            }
        }
        stream.insert(event.seq);
        runs.insert(&event.run_id);
        *summary.source_event_counts.entry(event.source).or_default() += 1;
        if let EventBody::MetricScore(score) = &event.body {
            *summary.metric_event_counts.entry(score.metric).or_default() += 1;
        }
    }
    summary.runs = runs.len();
    summary.streams = seen.len();
    Ok(summary)
}
