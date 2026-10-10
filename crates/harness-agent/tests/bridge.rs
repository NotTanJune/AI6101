//! The blocking provider bridge used by core strategies.

mod common;

use std::sync::Arc;

use common::{registry, spec};
use harness_agent::bridge::BlockingProvider;
use harness_agent::drift::{detect_drift, kb_subset};
use harness_agent::model::mock::{MockModelClient, Scripted};
use harness_agent::pricing::rate_card;
use harness_agent::registry::{Effort, ProviderId};
use harness_core::provider::{CompletionRequest, MeteredProvider, Provider, ProviderError};
use harness_core::strategy::{AdaptationContext, strategy_for};
use harness_core::{
    CostLedger, CostStatus, EventEmitter, EventSource, FixedClock, Phase, Redactor, Strategy,
    VecSink,
};
use serde_json::json;

fn card_kb(description: &str) -> harness_core::ToolKb {
    common::kb(vec![common::card("calc", "add", description)])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_runs_full_rewrite_through_the_metered_provider() {
    let source = card_kb("Adds numbers.");
    let live_tools = vec![harness_agent::mcp::LiveTool {
        name: "add".into(),
        description: "Adds two integers and returns the sum.".into(),
        input_schema: common::card("calc", "add", "").input_schema,
        output_schema: None,
    }];
    let subset = kb_subset(&source, "calc");
    let (report, live) = detect_drift("calc", &source, &live_tools);
    let rewritten = serde_json::to_string(&live).expect("serialise");

    let mock = Arc::new(MockModelClient::new(
        ProviderId::OpenAi,
        vec![Scripted::text(rewritten)],
    ));
    let handle = tokio::runtime::Handle::current();
    let client = mock.clone();
    let diff = report.diff.clone();

    // Strategies are blocking: run them off the runtime workers.
    let (candidate_ok, records, spent) = tokio::task::spawn_blocking(move || {
        let sol = spec("sol");
        let mut provider = BlockingProvider::new(client, sol, Effort::High, handle);
        let models = registry();
        let mut ledger = CostLedger::new(10.0, rate_card(&models));
        let sink = VecSink::new();
        let mut emitter = EventEmitter::new(
            "bridge-test",
            EventSource::Rust,
            Arc::new(FixedClock::default()),
            Redactor::default(),
            Box::new(sink),
        );
        let result = {
            let mut metered = MeteredProvider::new(
                &mut provider,
                &mut ledger,
                &mut emitter,
                Some(Strategy::FullRewrite),
            );
            let mut ctx = AdaptationContext {
                source: &subset,
                target: &live,
                diff: &diff,
                provider: &mut metered,
            };
            strategy_for(Strategy::FullRewrite).adapt(&mut ctx)
        };
        (
            result.is_ok(),
            ledger.records().to_vec(),
            ledger.spent_usd(),
        )
    })
    .await
    .expect("blocking task");

    assert!(candidate_ok, "full rewrite accepted the mock KB");
    assert_eq!(records.len(), 1, "exactly one settled call");
    let record = &records[0];
    assert_eq!(record.cost_status, CostStatus::Priced);
    assert_eq!(record.phase, Phase::Adaptation);
    assert_eq!(record.provider, "openai");
    assert_eq!(record.model, "gpt-6.1-sol");
    assert_eq!(record.input_tokens, Some(100));
    assert_eq!(record.output_tokens, Some(20));
    // (100 * 2.00 + 20 * 10.00) / 1e6 from the registry rate card.
    assert!((record.estimated_usd.unwrap() - 0.0004).abs() < 1e-12);
    assert!((spent - 0.0004).abs() < 1e-12);

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].instructions.contains("You maintain a Tool KB"));
    assert_eq!(requests[0].history.len(), 1);
    assert!(matches!(&requests[0].history[0],
            harness_agent::model::Item::User { text } if text.contains("Target server version")));
    assert!(
        requests[0].tool_names.is_empty(),
        "no tools on the bridge path"
    );
    assert!(
        requests[0].json_schema.is_some(),
        "the KB schema is passed through"
    );
    assert_eq!(requests[0].effort, Effort::High);
    assert_eq!(requests[0].model_alias, "sol");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_maps_errors_and_identity() {
    let mock = Arc::new(MockModelClient::new(
        ProviderId::Gemini,
        vec![
            Scripted::error(harness_agent::model::ModelError::Http {
                status: 503,
                message: "overloaded".into(),
            }),
            Scripted::text("plain text"),
            Scripted::error(harness_agent::model::ModelError::Cancelled),
        ],
    ));
    let handle = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        let mut provider = BlockingProvider::new(mock, spec("pro"), Effort::High, handle);
        assert_eq!(provider.id(), "gemini");
        assert_eq!(provider.model(), "gemini-3.1-pro-preview");
        assert_eq!(provider.kind(), harness_core::ProviderKind::Paid);
        let req = CompletionRequest {
            purpose: "test".into(),
            system: "sys".into(),
            user: "usr".into(),
            max_output_tokens: 100,
            temperature: Some(0.7),
            top_p: Some(0.9),
            seed: Some(1),
            json_schema: Some(json!({"type": "object"})),
        };
        let first = provider.complete(&req);
        let second = provider.complete(&req);
        let third = provider.complete(&req);
        (first, second, third)
    })
    .await
    .expect("blocking task");
    let (first, second, third) = result;
    let err = first.expect_err("503");
    assert!(
        matches!(err, ProviderError::Http { status: 503, .. }),
        "{err}"
    );
    assert!(err.billing_uncertain());
    let ok = second.expect("second call works");
    assert_eq!(ok.text, "plain text");
    assert_eq!(ok.usage.expect("usage").input_tokens, Some(100));
    assert_eq!(ok.request_id, "mock-request");
    // A cancelled call may have been billed: it must not look free to core's ledger.
    let cancelled = third.expect_err("cancelled");
    assert!(cancelled.billing_uncertain(), "{cancelled}");
}
