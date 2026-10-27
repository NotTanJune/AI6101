# Tasks - Log Analysis

## In Progress


## To Do

- [ ] P1 — Inventory Ableton log sources and weekly coverage: Review the crash-analysis briefs and available logs; document formats, timestamps/timezone evidence, crash-related fields, and coverage gaps for the latest seven days. Do not assert week-long trends if the sources do not cover the period. Verify with a source/date coverage inventory.
- [ ] P1 — Specify resilient parsing and normalized event schema: Define extraction of timestamp, severity, component, message, crash indicators, raw line, source path, and line number. Specify malformed-line retention, timestamp normalization, and a documented deduplication policy. Depends on: Inventory Ableton log sources and weekly coverage.
- [ ] P2 — Classify crash signatures and group incidents: Create auditable rules separating confirmed/probable crashes from warnings and unrelated errors; group linked records conservatively and preserve confidence plus evidence. Keep ambiguous events for review. Depends on: Specify resilient parsing and normalized event schema.
- [ ] P2 — Aggregate latest-week crash patterns and assess coverage: Summarize daily incident counts and recurring signatures/components for the defined date range and timezone; identify missing days and reconcile totals to classified incidents. Depends on: Classify crash signatures and group incidents.
- [ ] P3 — Produce findings and prioritized follow-up recommendations: Report recurring patterns with representative log references, confidence, coverage limitations, and evidence-grounded next steps; distinguish observations from hypotheses. Depends on: Aggregate latest-week crash patterns and assess coverage.

## Backlog


## Done
