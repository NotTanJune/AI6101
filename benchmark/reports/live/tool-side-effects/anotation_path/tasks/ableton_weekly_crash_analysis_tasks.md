# Tasks - Ableton Weekly Crash Analysis Tasks

## In Progress

- [ ] P1 — Inventory Ableton log formats and define analysis requirements

## To Do

- [ ] P1 — Inventory available Ableton logs and analysis requirements: inspect recent log inputs, existing parsing/aggregation code, and crash-analysis notes; document formats, date coverage, fields, week boundaries, desired metrics, and ambiguities. Acceptance: input inventory and requirements identify gaps and prevent unsupported causal claims.
- [ ] P2 — Define resilient parsing and normalization: specify timestamp/timezone normalization, event fields, signature creation, format variants, and malformed-line accounting while retaining source references. Depends on P1. Acceptance: parsing rules cover observed formats and no lines are silently discarded.
- [ ] P3 — Define weekly crash aggregation and metrics: distinguish crashes from general errors/warnings; define seven-day filtering, duplicate counting, grouping by signature/component/time and any logged system context. Depends on P1 and P2. Acceptance: metrics and caveats clearly define totals, recurrence, and data-quality limits.
- [ ] P4 — Run and validate the past-week analysis: apply parsing and aggregation to available logs, cross-check outputs against source records, and produce an evidence-based summary of recurring signatures and patterns. Depends on P2 and P3. If logs do not span seven days, report actual coverage rather than implying full-week coverage. Acceptance: reproducible report includes findings, counts, coverage, and limitations.
- [ ] P2 — Specify resilient parsing and normalized event schema
- [ ] P3 — Implement and test log ingestion and normalization
- [ ] P4 — Define crash classification and recurring-signature aggregation
- [ ] P5 — Produce weekly trend report with coverage and evidence
- [ ] P6 — Validate weekly findings and document operational limitations
- [ ] P1 — Specify rolling seven-day input discovery and timestamp normalization
- [ ] P1 — Implement tolerant parser for semi-structured and multiline crash records
- [ ] P2 — Normalize crash signatures and extract system/context fields
- [ ] P2 — Aggregate crash frequencies and identify temporal/system patterns
- [ ] P2 — Validate against recent samples and malformed-log cases
- [ ] P3 — Produce a concise, reproducible weekly crash-pattern report
- [ ] Priority 1 — Inspect weekly Ableton inputs and requirements: review supplied log files and crash-analysis notes; establish the exact seven-day window, timestamps/time zone, file coverage, and expected outputs. Record unsupported assumptions and representative log examples.
- [ ] Priority 1 — Define semi-structured log parsing and normalization: identify record boundaries, timestamp formats, crash/error indicators and fields; specify normalized event schema, malformed/missing-field handling, duplicate-event strategy, and diagnostics for skipped lines.
- [ ] Priority 1 — Implement ingestion and parser validation: read all in-scope logs, parse into normalized crash events, preserve source file/line provenance, and report parse coverage and malformed records without silently dropping data.
- [ ] Priority 2 — Build weekly crash aggregation: filter to the defined week and calculate crash counts and trends by day/time, error signature/message, component or module, and other reliably available dimensions; avoid inferring fields absent from logs.
- [ ] Priority 2 — Generate a findings report: summarize dominant patterns, notable changes or clusters, data-quality limitations, and evidence with timestamps/signatures; include concise tables or charts where supported.
- [ ] Priority 2 — Verify results against source evidence: test representative valid, malformed, incomplete, duplicate, and out-of-window entries; reconcile aggregates to parsed events and inspect sampled findings against original logs.
- [ ] Priority 3 — Document repeatable weekly workflow: specify input file placement, date-window configuration, command/process, outputs, and known limitations so subsequent weekly analyses can be reproduced.
- [ ] P1 — Build robust seven-day log ingestion and parsing, including timestamp normalization and malformed-record diagnostics.
- [ ] P1 — Classify and aggregate recurring crash signatures, components, and day-by-day frequency with evidence links.
- [ ] P1 — Generate and validate the weekly crash-pattern report, reconciling totals and stating coverage limitations.
- [ ] P1 — Validate weekly log coverage and data quality: confirm the available Ableton logs span the requested past-week date window; record gaps, duplicates, timestamp issues, and unreadable sections. Depends on inventory and date-window definition. Acceptance: documented earliest/latest timestamps, day-level coverage, and explicit limitations.
- [ ] P1 — Specify normalized event schema and crash classification: define normalized fields, multiline/unknown-format handling, provenance, and conservative crash-versus-error/warning/ambiguous rules. Depends on inventory. Acceptance: every classified event can be traced to raw source lines and uncertain classifications are retained as uncertain.
- [ ] P1 — Implement and test robust semi-structured log parsing: parse variable timestamps, multiline records, optional fields, and malformed entries into the agreed schema while retaining diagnostics. Depends on schema. Acceptance: tests cover representative formats and bad records do not abort batch parsing.
- [ ] P1 — Analyze crash signatures and weekly patterns: deduplicate under documented rules and aggregate by signature, time, component, and available system context. Depends on coverage validation and parser. Acceptance: aggregates reconcile with classified events and include traceable source evidence; partial coverage is labeled.
- [ ] P1 — Produce evidence-backed weekly crash findings: report coverage, method, counts, recurring signatures, time patterns, uncertainty, and next investigative steps without unsupported causal claims. Depends on analysis. Acceptance: counts and claims are reproducible from source records with clear limitations.

## Backlog


## Done
