//! Cost ledger behaviour.

use harness_core::{
    CostError, CostLedger, CostRecord, CostStatus, EventBody, Phase, ProviderKind, RateCard,
    RateEntry, ReserveRequest, Usage, read_events,
};

fn card() -> RateCard {
    RateCard {
        id: "rc-test".into(),
        date: "2026-09-01".into(),
        entries: vec![RateEntry {
            provider: "openai".into(),
            model: "gpt-x".into(),
            input_usd_per_mtok: 2.0,
            output_usd_per_mtok: 8.0,
            cached_input_usd_per_mtok: None,
            cache_write_usd_per_mtok: None,
            long_context_threshold_tokens: None,
            long_input_usd_per_mtok: None,
            long_cached_input_usd_per_mtok: None,
            long_cache_write_usd_per_mtok: None,
            long_output_usd_per_mtok: None,
            price_date: None,
        }],
    }
}

fn paid(input: u64, max_out: u32, phase: Phase) -> ReserveRequest {
    ReserveRequest {
        provider: "openai".into(),
        model: "gpt-x".into(),
        phase,
        input_tokens_estimate: input,
        max_output_tokens: max_out,
        kind: ProviderKind::Paid,
    }
}

fn usage(id: &str, input: Option<u64>, output: Option<u64>) -> Usage {
    Usage {
        request_id: id.into(),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        latency_ms: 100,
    }
}

fn assert_close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn reserve_settle_prices_from_actual_usage() {
    let mut ledger = CostLedger::new(1.0, card());
    // Reserve: (1_000_000 * 2 + 100_000 * 8) / 1e6 = 2.8
    let mut big = CostLedger::new(10.0, card());
    let r = big
        .reserve(paid(1_000_000, 100_000, Phase::Adaptation))
        .unwrap();
    assert_close(r.reserved_usd(), 2.8);
    assert_close(big.remaining_usd(), 7.2);
    big.release(r);

    let r = ledger
        .reserve(paid(100_000, 10_000, Phase::Adaptation))
        .unwrap();
    assert_close(r.reserved_usd(), 0.28);
    assert_close(ledger.remaining_usd(), 0.72);
    assert_close(ledger.outstanding_reserved_usd(), 0.28);

    // Actual: (50_000 * 2 + 5_000 * 8) / 1e6 = 0.14
    let record = ledger.settle(r, usage("req-1", Some(50_000), Some(5_000)));
    assert_eq!(record.cost_status, CostStatus::Priced);
    assert_close(record.estimated_usd.unwrap(), 0.14);
    assert_eq!(record.rate_card_id.as_deref(), Some("rc-test"));
    assert_eq!(record.rate_card_date.as_deref(), Some("2026-09-01"));
    assert_close(ledger.spent_usd(), 0.14);
    assert_close(ledger.outstanding_reserved_usd(), 0.0);
    assert_close(ledger.unreconciled_usd(), 0.0);
    assert_close(ledger.remaining_usd(), 0.86);
    assert_eq!(ledger.records().len(), 1);
}

#[test]
fn release_frees_the_reservation_and_records_nothing() {
    let mut ledger = CostLedger::new(1.0, card());
    let r = ledger
        .reserve(paid(100_000, 10_000, Phase::Canary))
        .unwrap();
    assert!(ledger.remaining_usd() < 1.0);
    ledger.release(r);
    assert_close(ledger.remaining_usd(), 1.0);
    assert!(ledger.records().is_empty());
    assert!(ledger.totals_by_phase().is_empty());
}

#[test]
fn refuses_dispatch_that_would_exceed_the_cap() {
    let mut ledger = CostLedger::new(0.5, card());
    let first = ledger
        .reserve(paid(100_000, 10_000, Phase::Adaptation))
        .unwrap(); // 0.28
    // 0.28 outstanding + 0.28 requested > 0.5
    let err = ledger
        .reserve(paid(100_000, 10_000, Phase::Adaptation))
        .expect_err("cap");
    match err {
        CostError::CapExceeded {
            requested_usd,
            remaining_usd,
            cap_usd,
        } => {
            assert_close(requested_usd, 0.28);
            assert_close(remaining_usd, 0.22);
            assert_close(cap_usd, 0.5);
        }
        other => panic!("unexpected {other:?}"),
    }
    // Releasing the first frees room again.
    ledger.release(first);
    assert!(
        ledger
            .reserve(paid(100_000, 10_000, Phase::Adaptation))
            .is_ok()
    );
}

#[test]
fn reservation_exactly_at_the_cap_is_allowed() {
    let mut ledger = CostLedger::new(0.28, card());
    assert!(
        ledger
            .reserve(paid(100_000, 10_000, Phase::Adaptation))
            .is_ok()
    );
    assert!(ledger.reserve(paid(1, 1, Phase::Adaptation)).is_err());
}

#[test]
fn paid_provider_without_rate_is_blocked() {
    let mut ledger = CostLedger::new(100.0, card());
    let mut req = paid(10, 10, Phase::Adaptation);
    req.model = "gpt-unpriced".into();
    assert_eq!(
        ledger.reserve(req).expect_err("no rate"),
        CostError::NoRate {
            provider: "openai".into(),
            model: "gpt-unpriced".into()
        }
    );
    let mut empty = CostLedger::new(100.0, RateCard::empty("none", "2026-09-01"));
    assert!(matches!(
        empty.reserve(paid(10, 10, Phase::Adaptation)),
        Err(CostError::NoRate { .. })
    ));
}

#[test]
fn missing_usage_is_unknown_and_stays_reserved_against_the_cap() {
    let mut ledger = CostLedger::new(0.5, card());
    let r = ledger
        .reserve(paid(100_000, 10_000, Phase::Verifier))
        .unwrap(); // 0.28
    let record = ledger.settle(r, usage("req-x", None, None));
    assert_eq!(record.cost_status, CostStatus::Unknown);
    assert_eq!(record.estimated_usd, None, "unknown is never zero");
    assert_close(ledger.spent_usd(), 0.0);
    assert_close(ledger.unreconciled_usd(), 0.28);
    assert_close(ledger.remaining_usd(), 0.22);
    // The unreconciled amount keeps blocking dispatch.
    assert!(matches!(
        ledger.reserve(paid(100_000, 10_000, Phase::Verifier)),
        Err(CostError::CapExceeded { .. })
    ));

    // Partial usage (output missing) is unknown as well.
    let r = ledger.reserve(paid(1, 1, Phase::Verifier)).unwrap();
    let partial = ledger.settle(r, usage("req-y", Some(1), None));
    assert_eq!(partial.cost_status, CostStatus::Unknown);
    assert_eq!(partial.estimated_usd, None);
    assert_eq!(partial.input_tokens, Some(1));
}

#[test]
fn mock_and_local_are_never_usd_and_never_zero_valued() {
    let mut ledger = CostLedger::new(1.0, card());
    for (kind, status) in [
        (ProviderKind::Mock, CostStatus::Mock),
        (ProviderKind::Local, CostStatus::Local),
    ] {
        let r = ledger
            .reserve(ReserveRequest {
                provider: "mock-or-local".into(),
                model: "m".into(),
                phase: Phase::Agent,
                input_tokens_estimate: 1_000_000,
                max_output_tokens: 1_000_000,
                kind,
            })
            .unwrap();
        assert_eq!(r.reserved_usd(), 0.0);
        let record = ledger.settle(r, usage("id", Some(10), Some(20)));
        assert_eq!(record.cost_status, status);
        assert_eq!(record.estimated_usd, None);
        assert_eq!(record.rate_card_id, None);
    }
    assert_eq!(ledger.spent_usd(), 0.0);
    assert_eq!(ledger.remaining_usd(), 1.0);

    let totals = &ledger.totals_by_phase()[&Phase::Agent];
    assert_eq!(totals.calls, 2);
    assert_eq!(totals.mock_calls, 1);
    assert_eq!(
        totals.local_latency_ms, 100,
        "only the local call's latency"
    );
    assert_eq!(totals.usd, 0.0);
    assert_eq!(totals.input_tokens, 20);
}

#[test]
fn totals_by_phase_aggregate_priced_unknown_and_tokens() {
    let mut ledger = CostLedger::new(100.0, card());
    let r = ledger.reserve(paid(1000, 100, Phase::Adaptation)).unwrap();
    ledger.settle(r, usage("a", Some(1_000_000), Some(0)));
    let r = ledger.reserve(paid(1000, 100, Phase::Adaptation)).unwrap();
    ledger.settle(r, usage("b", Some(0), Some(1_000_000)));
    let r = ledger.reserve(paid(1000, 100, Phase::Canary)).unwrap();
    ledger.settle(r, usage("c", None, None));

    let totals = ledger.totals_by_phase();
    let adaptation = &totals[&Phase::Adaptation];
    assert_eq!(adaptation.calls, 2);
    assert_close(adaptation.usd, 2.0 + 8.0);
    assert_eq!(adaptation.input_tokens, 1_000_000);
    assert_eq!(adaptation.output_tokens, 1_000_000);
    let canary = &totals[&Phase::Canary];
    assert_eq!(canary.unknown_calls, 1);
    assert_eq!(canary.usd, 0.0);

    let overall = ledger.totals_overall();
    assert_eq!(overall.calls, 3);
    assert_eq!(overall.unknown_calls, 1);
    assert_close(overall.usd, 10.0);
}

fn fixture_cost_records() -> Vec<CostRecord> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/contracts/events.valid.jsonl");
    read_events(path)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ModelCall(m) => Some((e.source, m)),
            _ => None,
        })
        .map(|(_, m)| m)
        .collect()
}

#[test]
fn ingest_external_aggregates_fixture_records_including_python_source() {
    let records = fixture_cost_records();
    assert_eq!(
        records.len(),
        4,
        "priced, unknown (python), mock, local (python)"
    );
    let mut ledger = CostLedger::new(100.0, card());
    for record in &records {
        ledger.ingest_external(record).unwrap();
    }
    assert_eq!(ledger.records().len(), 4);
    // Only the single priced call counts as USD.
    assert_close(ledger.spent_usd(), 0.000456);
    assert_close(ledger.remaining_usd(), 100.0 - 0.000456);

    let totals = ledger.totals_by_phase();
    let adaptation = &totals[&Phase::Adaptation];
    assert_eq!(adaptation.calls, 2, "rust priced + rust mock");
    assert_eq!(adaptation.mock_calls, 1);
    assert_eq!(adaptation.input_tokens, 1200 + 900);
    let verifier = &totals[&Phase::Verifier];
    assert_eq!(
        verifier.unknown_calls, 1,
        "python unknown usage stays unknown"
    );
    assert_eq!(verifier.input_tokens, 0);
    assert_eq!(verifier.usd, 0.0);
    let agent = &totals[&Phase::Agent];
    assert_eq!(agent.local_latency_ms, 5300);
    assert_eq!(agent.output_tokens, 120);
}

#[test]
fn ingest_external_rejects_inconsistent_records() {
    let mut ledger = CostLedger::new(100.0, card());
    let base = fixture_cost_records()
        .into_iter()
        .find(|r| r.cost_status == CostStatus::Priced)
        .unwrap();

    let mut mock_with_usd = base.clone();
    mock_with_usd.cost_status = CostStatus::Mock;
    assert!(matches!(
        ledger.ingest_external(&mock_with_usd),
        Err(CostError::InvalidRecord(_))
    ));

    let mut unknown_with_usd = base.clone();
    unknown_with_usd.cost_status = CostStatus::Unknown;
    assert!(ledger.ingest_external(&unknown_with_usd).is_err());

    let mut priced_without_usd = base.clone();
    priced_without_usd.estimated_usd = None;
    assert!(ledger.ingest_external(&priced_without_usd).is_err());

    let mut negative = base.clone();
    negative.estimated_usd = Some(-1.0);
    assert!(ledger.ingest_external(&negative).is_err());

    let mut nan = base.clone();
    nan.estimated_usd = Some(f64::NAN);
    assert!(ledger.ingest_external(&nan).is_err());

    let mut no_id = base;
    no_id.request_id.clear();
    assert!(ledger.ingest_external(&no_id).is_err());

    assert!(
        ledger.records().is_empty(),
        "rejected records are not aggregated"
    );
    assert_eq!(ledger.spent_usd(), 0.0);
}

fn tiered_card() -> RateCard {
    let mut entry = card().entries.remove(0);
    entry.cached_input_usd_per_mtok = Some(0.2);
    entry.cache_write_usd_per_mtok = Some(2.5);
    entry.long_context_threshold_tokens = Some(200_000);
    entry.long_input_usd_per_mtok = Some(4.0);
    entry.long_cached_input_usd_per_mtok = Some(0.4);
    entry.long_output_usd_per_mtok = Some(12.0);
    RateCard {
        id: "rc-tiered".into(),
        date: "2026-10-04".into(),
        entries: vec![entry],
    }
}

fn settle_with(
    rates: RateCard,
    input: u64,
    cached: Option<u64>,
    writes: Option<u64>,
    output: u64,
) -> f64 {
    let mut ledger = CostLedger::new(1000.0, rates);
    let r = ledger.reserve(paid(10, 10, Phase::Agent)).unwrap();
    let mut u = usage("req-x", Some(input), Some(output));
    u.cached_input_tokens = cached;
    u.cache_write_tokens = writes;
    ledger.settle(r, u).estimated_usd.unwrap()
}

#[test]
fn old_format_rate_entry_toml_without_new_fields_still_parses() {
    let text = r#"
provider = "openai"
model = "gpt-x"
input_usd_per_mtok = 2.0
output_usd_per_mtok = 8.0
"#;
    let entry: RateEntry = toml::from_str(text).expect("old format parses");
    assert_eq!(entry.input_usd_per_mtok, 2.0);
    assert_eq!(entry.cached_input_usd_per_mtok, None);
    assert_eq!(entry.long_context_threshold_tokens, None);
    assert_eq!(entry.price_date, None);
    // Optional fields stay out of the serialised form.
    let back = toml::to_string(&entry).unwrap();
    assert!(!back.contains("cached"), "{back}");
}

#[test]
fn cached_tokens_are_billed_at_the_cached_rate() {
    // 100_000 input of which 60_000 cached: 40_000 * 2.0 + 60_000 * 0.2 + 1_000 * 8.0 = 100_000.
    let usd = settle_with(tiered_card(), 100_000, Some(60_000), None, 1_000);
    assert_close(usd, 0.1);
    // Cache writes at their own rate: 10_000 * 2.0 + 20_000 * 2.5 + 0 = 70_000.
    let usd = settle_with(tiered_card(), 30_000, None, Some(20_000), 0);
    assert_close(usd, 0.07);
}

#[test]
fn request_above_threshold_uses_long_rates_for_the_whole_request() {
    // 300_000 input (200_000 cached) and 10_000 output, all at long rates:
    // 100_000 * 4.0 + 200_000 * 0.4 + 10_000 * 12.0 = 600_000.
    let usd = settle_with(tiered_card(), 300_000, Some(200_000), None, 10_000);
    assert_close(usd, 0.6);
    // Exactly at the threshold stays on the short rates: 200_000 * 2.0 + 1_000 * 8.0.
    let usd = settle_with(tiered_card(), 200_000, None, None, 1_000);
    assert_close(usd, 0.408);
}

#[test]
fn missing_cached_rate_falls_back_to_the_input_rate() {
    // `card()` has no cached rate: cached tokens bill like ordinary input.
    let usd = settle_with(card(), 100_000, Some(60_000), None, 1_000);
    assert_close(usd, 100_000.0 * 2.0 / 1e6 + 1_000.0 * 8.0 / 1e6);
}

#[test]
fn reservation_uses_long_rates_when_the_estimate_exceeds_the_threshold() {
    let mut ledger = CostLedger::new(1000.0, tiered_card());
    let r = ledger.reserve(paid(300_000, 10_000, Phase::Agent)).unwrap();
    // 300_000 * 4.0 + 10_000 * 12.0 = 1_320_000 micro-USD.
    assert_close(r.reserved_usd(), 1.32);
    ledger.release(r);
}
