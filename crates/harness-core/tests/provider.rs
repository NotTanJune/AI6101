//! Providers: request builders, response parsers, mock, metering and secret hygiene.
//! No test touches the network: HTTP providers run against a fake transport.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use harness_core::config::ProviderSection;
use harness_core::provider::{
    CompletionRequest, Decoding, GeminiProvider, HttpReply, HttpTransport, LocalProvider,
    MeteredError, MeteredProvider, MockProvider, OpenAiProvider, Provider, ProviderError,
    TokenLimitParam, UsageReport, build_gemini_request, build_openai_request, build_provider,
    build_provider_with_mock_dir, parse_gemini_response, parse_openai_response,
};
use harness_core::{
    ConfigProviderKind, CostError, CostLedger, CostStatus, EventBody, EventEmitter, EventSource,
    FixedClock, Phase, ProviderKind, RateCard, RateEntry, Redactor, Strategy, VecSink,
};
use serde_json::{Value, json};

const SECRET: &str = "sk-test-0123456789abcdefSECRETVALUE";

fn request() -> CompletionRequest {
    CompletionRequest {
        purpose: "full_rewrite".into(),
        system: "You are terse.".into(),
        user: "Rewrite the KB.".into(),
        max_output_tokens: 500,
        temperature: Some(0.0),
        top_p: Some(0.9),
        seed: Some(7),
        json_schema: Some(json!({ "type": "object" })),
    }
}

fn emitter() -> (EventEmitter, VecSink) {
    let sink = VecSink::new();
    let emitter = EventEmitter::new(
        "run-1",
        EventSource::Rust,
        Arc::new(FixedClock::default()),
        Redactor::default(),
        Box::new(sink.clone()),
    );
    (emitter, sink)
}

fn rate_card(provider: &str, model: &str) -> RateCard {
    RateCard {
        id: "card-1".into(),
        date: "2026-01-01".into(),
        entries: vec![RateEntry {
            provider: provider.into(),
            model: model.into(),
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 2.0,
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

fn model_calls(sink: &VecSink) -> Vec<harness_core::ModelCallEvent> {
    sink.events()
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ModelCall(m) => Some(m),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Pure builders / parsers
// ---------------------------------------------------------------------------------------------

#[test]
fn openai_request_body() {
    let body = build_openai_request("gpt-x", &request(), TokenLimitParam::MaxCompletionTokens);
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(
        body["messages"][0],
        json!({ "role": "system", "content": "You are terse." })
    );
    assert_eq!(
        body["messages"][1],
        json!({ "role": "user", "content": "Rewrite the KB." })
    );
    assert_eq!(body["max_completion_tokens"], 500);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["temperature"], 0.0);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(
        body["response_format"]["json_schema"]["schema"],
        json!({ "type": "object" })
    );
}

#[test]
fn openai_request_local_style_and_minimal() {
    let mut req = request();
    req.system.clear();
    req.temperature = None;
    req.top_p = None;
    req.seed = None;
    req.json_schema = None;
    let body = build_openai_request("m", &req, TokenLimitParam::MaxTokens);
    assert_eq!(body["max_tokens"], 500);
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    for key in ["temperature", "top_p", "seed", "response_format"] {
        assert!(body.get(key).is_none(), "{key}");
    }
}

#[test]
fn openai_response_parsing() {
    let body = json!({
        "id": "chatcmpl-1",
        "choices": [{ "message": { "role": "assistant", "content": "{\"ops\":[]}" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150 }
    });
    let parsed = parse_openai_response(&body).unwrap();
    assert_eq!(parsed.text, "{\"ops\":[]}");
    assert_eq!(parsed.request_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(
        parsed.usage,
        Some(UsageReport {
            input_tokens: Some(120),
            output_tokens: Some(30)
        })
    );
}

#[test]
fn openai_missing_usage_is_none_never_zero() {
    let no_usage = json!({ "choices": [{ "message": { "content": "hi" } }] });
    assert_eq!(parse_openai_response(&no_usage).unwrap().usage, None);
    let empty_usage = json!({ "choices": [{ "message": { "content": "hi" } }], "usage": {} });
    assert_eq!(parse_openai_response(&empty_usage).unwrap().usage, None);
    let partial =
        json!({ "choices": [{ "message": { "content": "hi" } }], "usage": { "prompt_tokens": 5 } });
    assert_eq!(
        parse_openai_response(&partial).unwrap().usage,
        Some(UsageReport {
            input_tokens: Some(5),
            output_tokens: None
        })
    );
}

#[test]
fn openai_unusable_responses_are_errors() {
    let bodies = [
        json!({}),
        json!({ "choices": [] }),
        json!({ "choices": [{ "message": { "content": null, "refusal": "no" } }] }),
        json!({ "choices": [{ "message": {} }] }),
    ];
    for body in bodies {
        assert!(
            matches!(
                parse_openai_response(&body),
                Err(ProviderError::InvalidResponse(_))
            ),
            "{body}"
        );
    }
}

#[test]
fn gemini_request_body() {
    let body = build_gemini_request(&request());
    assert_eq!(body["contents"][0]["parts"][0]["text"], "Rewrite the KB.");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "You are terse."
    );
    let gen_cfg = &body["generationConfig"];
    assert_eq!(gen_cfg["maxOutputTokens"], 500);
    assert_eq!(gen_cfg["temperature"], 0.0);
    assert_eq!(gen_cfg["topP"], 0.9);
    assert_eq!(gen_cfg["seed"], 7);
    assert_eq!(gen_cfg["responseMimeType"], "application/json");
    assert_eq!(gen_cfg["responseJsonSchema"], json!({ "type": "object" }));

    let mut plain = request();
    plain.system.clear();
    plain.json_schema = None;
    let body = build_gemini_request(&plain);
    assert!(body.get("systemInstruction").is_none());
    assert!(body["generationConfig"].get("responseMimeType").is_none());
}

#[test]
fn gemini_response_parsing_counts_thought_tokens_as_output() {
    let body = json!({
        "responseId": "resp-9",
        "candidates": [{
            "content": { "parts": [
                { "text": "thinking...", "thought": true },
                { "text": "{\"a\":" },
                { "text": "1}" }
            ]},
            "finishReason": "STOP"
        }],
        "usageMetadata": { "promptTokenCount": 40, "candidatesTokenCount": 10, "thoughtsTokenCount": 25 }
    });
    let parsed = parse_gemini_response(&body).unwrap();
    assert_eq!(parsed.text, "{\"a\":1}");
    assert_eq!(parsed.request_id.as_deref(), Some("resp-9"));
    assert_eq!(
        parsed.usage,
        Some(UsageReport {
            input_tokens: Some(40),
            output_tokens: Some(35)
        })
    );
}

#[test]
fn gemini_missing_candidate_tokens_leave_output_unknown() {
    let body = json!({
        "candidates": [{ "content": { "parts": [{ "text": "x" }] } }],
        "usageMetadata": { "promptTokenCount": 3, "thoughtsTokenCount": 9 }
    });
    let parsed = parse_gemini_response(&body).unwrap();
    assert_eq!(
        parsed.usage,
        Some(UsageReport {
            input_tokens: Some(3),
            output_tokens: None
        })
    );
    assert_eq!(
        parse_gemini_response(
            &json!({ "candidates": [{ "content": { "parts": [{ "text": "x" }] } }] })
        )
        .unwrap()
        .usage,
        None
    );
}

#[test]
fn gemini_unusable_responses_are_errors() {
    let blocked = json!({ "promptFeedback": { "blockReason": "SAFETY" } });
    match parse_gemini_response(&blocked) {
        Err(ProviderError::InvalidResponse(m)) => assert!(m.contains("SAFETY"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(parse_gemini_response(&json!({})).is_err());
    let empty = json!({ "candidates": [{ "finishReason": "SAFETY", "content": { "parts": [] } }] });
    assert!(parse_gemini_response(&empty).is_err());
}

// ---------------------------------------------------------------------------------------------
// MockProvider
// ---------------------------------------------------------------------------------------------

fn provider_fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/provider")
}

#[test]
fn mock_scripted_returns_in_order_then_errors() {
    let mut mock = MockProvider::scripted(vec!["one".into(), "two".into()]);
    assert_eq!(mock.kind(), ProviderKind::Mock);
    assert_eq!(mock.id(), "mock");
    let a = mock.complete(&request()).unwrap();
    let b = mock.complete(&request()).unwrap();
    assert_eq!((a.text.as_str(), b.text.as_str()), ("one", "two"));
    assert_ne!(a.request_id, b.request_id);
    let usage = a.usage.unwrap();
    assert!(usage.input_tokens.unwrap() > 0 && usage.output_tokens.unwrap() > 0);
    assert!(matches!(
        mock.complete(&request()),
        Err(ProviderError::Mock(_))
    ));
    assert_eq!(mock.call_count(), 3);
    assert_eq!(mock.requests().len(), 3);
}

#[test]
fn mock_is_deterministic() {
    let run = || {
        let mut mock = MockProvider::scripted(vec!["same".into()]);
        mock.complete(&request()).unwrap()
    };
    assert_eq!(run(), run());
}

#[test]
fn mock_fixture_dir_selects_by_purpose() {
    let mut mock = MockProvider::from_fixture_dir(provider_fixtures()).unwrap();
    let mut req = request();
    req.purpose = "selective_patch".into();
    let patch = mock.complete(&req).unwrap();
    assert!(
        serde_json::from_str::<Value>(&patch.text)
            .unwrap()
            .get("ops")
            .is_some()
    );
    req.purpose = "full_rewrite".into();
    let kb = mock.complete(&req).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&kb.text).unwrap()["schema_version"],
        "tool-kb/1"
    );
    req.purpose = "unknown".into();
    assert!(matches!(mock.complete(&req), Err(ProviderError::Mock(_))));
}

#[test]
fn mock_fixture_dir_errors() {
    assert!(MockProvider::from_fixture_dir("/definitely/not/here").is_err());
    let dir = tempfile::tempdir().unwrap();
    assert!(MockProvider::from_fixture_dir(dir.path()).is_err());
}

// ---------------------------------------------------------------------------------------------
// MeteredProvider
// ---------------------------------------------------------------------------------------------

#[test]
fn metered_mock_call_emits_mock_model_call_event() {
    let mut mock = MockProvider::scripted(vec!["hello world".into()]);
    let mut ledger = CostLedger::new(1.0, RateCard::empty("none", "2026-01-01"));
    let (mut em, sink) = emitter();
    {
        let mut metered =
            MeteredProvider::new(&mut mock, &mut ledger, &mut em, Some(Strategy::FullRewrite));
        let resp = metered.call(Phase::Adaptation, &request()).unwrap();
        assert_eq!(resp.text, "hello world");
    }
    let calls = model_calls(&sink);
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.cost_status, CostStatus::Mock);
    assert_eq!(call.phase, Phase::Adaptation);
    assert_eq!(call.provider, "mock");
    assert_eq!(call.estimated_usd, None);
    assert!(call.input_tokens.is_some() && call.output_tokens.is_some());
    assert_eq!(sink.events()[0].strategy, Some(Strategy::FullRewrite));
    assert_eq!(ledger.spent_usd(), 0.0);
    assert_eq!(ledger.totals_by_phase()[&Phase::Adaptation].mock_calls, 1);
}

#[test]
fn metered_paid_call_is_priced_from_usage() {
    let mut mock = MockProvider::scripted(vec!["x".repeat(400)])
        .with_identity("openai", "m1")
        .with_kind(ProviderKind::Paid);
    let mut ledger = CostLedger::new(1.0, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    let req = request();
    let expected_in = req.estimated_input_tokens();
    MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &req)
        .unwrap();
    let call = &model_calls(&sink)[0];
    assert_eq!(call.cost_status, CostStatus::Priced);
    assert_eq!(call.rate_card_id.as_deref(), Some("card-1"));
    let expected = (expected_in as f64 * 1.0 + 100.0 * 2.0) / 1e6;
    assert!((call.estimated_usd.unwrap() - expected).abs() < 1e-12);
    assert!((ledger.spent_usd() - expected).abs() < 1e-12);
}

#[test]
fn metered_paid_call_without_usage_is_unknown_not_zero() {
    let mut mock = MockProvider::scripted(vec!["x".into()])
        .with_identity("openai", "m1")
        .with_kind(ProviderKind::Paid)
        .without_usage();
    let mut ledger = CostLedger::new(1.0, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap();
    let call = &model_calls(&sink)[0];
    assert_eq!(call.cost_status, CostStatus::Unknown);
    assert_eq!(call.estimated_usd, None);
    assert_eq!(call.input_tokens, None);
    assert!(
        ledger.unreconciled_usd() > 0.0,
        "reservation must keep counting against the cap"
    );
}

#[test]
fn metered_local_call_is_local_status_without_usd() {
    let mut mock = MockProvider::scripted(vec!["x".into()])
        .with_identity("local", "llama")
        .with_kind(ProviderKind::Local);
    let mut ledger = CostLedger::new(1.0, RateCard::empty("none", "2026-01-01"));
    let (mut em, sink) = emitter();
    MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap();
    let call = &model_calls(&sink)[0];
    assert_eq!(call.cost_status, CostStatus::Local);
    assert_eq!(call.estimated_usd, None);
}

#[test]
fn budget_refusal_makes_zero_provider_calls_and_emits_nothing() {
    let mut mock = MockProvider::scripted(vec!["x".into()])
        .with_identity("openai", "m1")
        .with_kind(ProviderKind::Paid);
    // Cap far below the reservation for 500 output tokens at $2/Mtok plus input.
    let mut ledger = CostLedger::new(0.000_000_1, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    let err = {
        let mut metered = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None);
        metered.call(Phase::Adaptation, &request()).unwrap_err()
    };
    assert!(err.is_refusal(), "{err:?}");
    assert!(matches!(
        err,
        MeteredError::Refused(CostError::CapExceeded { .. })
    ));
    assert_eq!(
        mock.call_count(),
        0,
        "provider must not be called when refused"
    );
    assert!(sink.is_empty());
    assert!(ledger.records().is_empty());
    assert_eq!(ledger.outstanding_reserved_usd(), 0.0);
}

#[test]
fn missing_rate_blocks_paid_call_without_calling_provider() {
    let mut mock = MockProvider::scripted(vec!["x".into()])
        .with_identity("openai", "unpriced-model")
        .with_kind(ProviderKind::Paid);
    let mut ledger = CostLedger::new(100.0, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    let err = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap_err();
    assert!(matches!(
        err,
        MeteredError::Refused(CostError::NoRate { .. })
    ));
    assert_eq!(mock.call_count(), 0);
    assert!(sink.is_empty());
}

#[test]
fn released_reservation_leaves_room_for_the_next_call() {
    let mut mock = MockProvider::scripted(vec!["x".into(), "y".into()])
        .with_identity("openai", "m1")
        .with_kind(ProviderKind::Paid);
    let req = request();
    // Room for exactly one worst-case reservation.
    let worst = (req.estimated_input_tokens() as f64 * 1.0 + 500.0 * 2.0) / 1e6;
    let mut ledger = CostLedger::new(worst * 1.5, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    {
        let mut metered = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None);
        metered.call(Phase::Adaptation, &req).unwrap();
        // Actual cost was far below the reservation, so it is released and a second call still fits.
        metered.call(Phase::Adaptation, &req).unwrap();
    }
    assert_eq!(mock.call_count(), 2);
    assert_eq!(model_calls(&sink).len(), 2);
}

#[test]
fn definite_failure_releases_reservation_and_emits_no_event() {
    let mut mock = MockProvider::scripted_results(vec![Err(ProviderError::Http {
        status: 401,
        message: "bad key".into(),
    })])
    .with_identity("openai", "m1")
    .with_kind(ProviderKind::Paid);
    let mut ledger = CostLedger::new(1.0, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    let err = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap_err();
    assert!(matches!(
        err,
        MeteredError::Provider(ProviderError::Http { status: 401, .. })
    ));
    assert!(sink.is_empty());
    assert_eq!(ledger.outstanding_reserved_usd(), 0.0);
    assert_eq!(ledger.unreconciled_usd(), 0.0);
    assert_eq!(mock.call_count(), 1);
}

#[test]
fn uncertain_paid_failure_is_recorded_unknown_and_keeps_counting() {
    for error in [
        ProviderError::Timeout,
        ProviderError::Http {
            status: 503,
            message: "x".into(),
        },
        ProviderError::InvalidResponse("garbled".into()),
    ] {
        let mut mock = MockProvider::scripted_results(vec![Err(error)])
            .with_identity("openai", "m1")
            .with_kind(ProviderKind::Paid);
        let mut ledger = CostLedger::new(1.0, rate_card("openai", "m1"));
        let (mut em, sink) = emitter();
        let err = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
            .call(Phase::Adaptation, &request())
            .unwrap_err();
        assert!(matches!(err, MeteredError::Provider(_)));
        let calls = model_calls(&sink);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].cost_status, CostStatus::Unknown);
        assert_eq!(calls[0].estimated_usd, None);
        assert!(ledger.unreconciled_usd() > 0.0);
    }
}

#[test]
fn local_failure_records_nothing() {
    let mut mock = MockProvider::scripted_results(vec![Err(ProviderError::Timeout)])
        .with_identity("local", "m")
        .with_kind(ProviderKind::Local);
    let mut ledger = CostLedger::new(1.0, RateCard::empty("none", "2026-01-01"));
    let (mut em, sink) = emitter();
    let _ = MeteredProvider::new(&mut mock, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap_err();
    assert!(sink.is_empty());
}

#[test]
fn metered_exposes_fixed_decoding() {
    let mut mock = MockProvider::scripted(vec![]);
    let mut ledger = CostLedger::new(1.0, RateCard::empty("none", "2026-01-01"));
    let (mut em, _sink) = emitter();
    let decoding = Decoding {
        max_output_tokens: 123,
        temperature: Some(0.2),
        top_p: None,
        seed: Some(3),
    };
    let metered =
        MeteredProvider::new(&mut mock, &mut ledger, &mut em, None).with_decoding(decoding.clone());
    assert_eq!(metered.decoding(), &decoding);
    let req = metered
        .decoding()
        .request("p", "s".into(), "u".into(), None);
    assert_eq!(
        (req.max_output_tokens, req.temperature, req.seed),
        (123, Some(0.2), Some(3))
    );
}

// ---------------------------------------------------------------------------------------------
// HTTP providers against a fake transport
// ---------------------------------------------------------------------------------------------

type Captured = Arc<Mutex<Vec<(String, Vec<(String, String)>, Value)>>>;

struct FakeTransport {
    captured: Captured,
    reply: Result<HttpReply, String>,
}

impl HttpTransport for FakeTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpReply, ProviderError> {
        self.captured
            .lock()
            .unwrap()
            .push((url.to_owned(), headers.to_vec(), body.clone()));
        self.reply.clone().map_err(ProviderError::Transport)
    }
}

fn fake(reply: Result<HttpReply, String>) -> (Box<dyn HttpTransport>, Captured) {
    let captured: Captured = Arc::default();
    (
        Box::new(FakeTransport {
            captured: Arc::clone(&captured),
            reply,
        }),
        captured,
    )
}

fn ok_reply(body: Value) -> Result<HttpReply, String> {
    Ok(HttpReply {
        status: 200,
        body: body.to_string(),
        request_id_header: Some("hdr-1".into()),
    })
}

#[test]
fn openai_provider_sends_bearer_key_to_chat_completions() {
    let (transport, captured) = fake(ok_reply(json!({
        "id": "chatcmpl-7",
        "choices": [{ "message": { "content": "ok" } }],
        "usage": { "prompt_tokens": 11, "completion_tokens": 2 }
    })));
    let mut p = OpenAiProvider::new("gpt-x", SECRET.into(), None)
        .unwrap()
        .with_transport(transport);
    assert_eq!(p.endpoint(), "https://api.openai.com/v1/chat/completions");
    assert_eq!(
        (p.kind(), p.id(), p.model()),
        (ProviderKind::Paid, "openai", "gpt-x")
    );
    let resp = p.complete(&request()).unwrap();
    assert_eq!(resp.text, "ok");
    assert_eq!(resp.request_id, "chatcmpl-7");
    assert_eq!(
        resp.usage,
        Some(UsageReport {
            input_tokens: Some(11),
            output_tokens: Some(2)
        })
    );
    let calls = captured.lock().unwrap();
    let (url, headers, body) = &calls[0];
    assert!(url.starts_with("https://"));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == &format!("Bearer {SECRET}"))
    );
    assert!(!url.contains(SECRET), "key must never be in the URL");
    assert!(
        !body.to_string().contains(SECRET),
        "key must never be in the body"
    );
    assert_eq!(body["max_completion_tokens"], 500);
}

#[test]
fn request_id_falls_back_to_header_then_placeholder() {
    let (t, _) = fake(ok_reply(
        json!({ "choices": [{ "message": { "content": "ok" } }] }),
    ));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    assert_eq!(p.complete(&request()).unwrap().request_id, "hdr-1");
    let (t, _) = fake(Ok(HttpReply {
        status: 200,
        body: json!({ "choices": [{ "message": { "content": "ok" } }] }).to_string(),
        request_id_header: None,
    }));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    assert_eq!(p.complete(&request()).unwrap().request_id, "unavailable");
}

#[test]
fn gemini_provider_puts_key_in_header_not_url() {
    let (transport, captured) = fake(ok_reply(json!({
        "responseId": "g-1",
        "candidates": [{ "content": { "parts": [{ "text": "ok" }] } }],
        "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 1 }
    })));
    let key = "AIzaSyTESTKEY0123456789abcdefghijklmnop";
    let mut p = GeminiProvider::new("models/gemini-x", key.into(), None)
        .unwrap()
        .with_transport(transport);
    assert_eq!(
        p.endpoint(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent"
    );
    assert_eq!(p.model(), "gemini-x");
    assert_eq!(p.kind(), ProviderKind::Paid);
    p.complete(&request()).unwrap();
    let calls = captured.lock().unwrap();
    let (url, headers, _) = &calls[0];
    assert!(!url.contains(key));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-goog-api-key" && v == key)
    );
}

#[test]
fn local_provider_is_local_kind_and_keyless_by_default() {
    let (transport, captured) = fake(ok_reply(
        json!({ "choices": [{ "message": { "content": "ok" } }] }),
    ));
    let mut p = LocalProvider::new("llama", "http://localhost:8080/v1/", None)
        .unwrap()
        .with_transport(transport);
    assert_eq!(p.endpoint(), "http://localhost:8080/v1/chat/completions");
    assert_eq!((p.kind(), p.id()), (ProviderKind::Local, "local"));
    let resp = p.complete(&request()).unwrap();
    assert_eq!(resp.usage, None, "absent usage stays absent");
    let calls = captured.lock().unwrap();
    assert!(calls[0].1.iter().all(|(k, _)| k != "authorization"));
    assert_eq!(calls[0].2["max_tokens"], 500);
}

#[test]
fn keys_are_never_sent_over_plain_http_to_remote_hosts() {
    assert!(matches!(
        OpenAiProvider::new("m", SECRET.into(), Some("http://api.example.com/v1")),
        Err(ProviderError::Config(_))
    ));
    assert!(matches!(
        GeminiProvider::new("m", SECRET.into(), Some("http://example.com")),
        Err(ProviderError::Config(_))
    ));
    assert!(matches!(
        LocalProvider::new("m", "http://10.1.2.3:8080/v1", Some(SECRET.into())),
        Err(ProviderError::Config(_))
    ));
    assert!(LocalProvider::new("m", "http://127.0.0.1:8080/v1", Some(SECRET.into())).is_ok());
    assert!(LocalProvider::new("m", "https://gpu.example.com/v1", Some(SECRET.into())).is_ok());
    assert!(LocalProvider::new("m", "ftp://x", None).is_err());
    assert!(OpenAiProvider::new("m", "  ".into(), None).is_err());
}

#[test]
fn http_errors_are_mapped_and_scrubbed() {
    let (t, _) = fake(Ok(HttpReply {
        status: 401,
        body: json!({ "error": { "message": format!("Incorrect API key provided: {SECRET}") } })
            .to_string(),
        request_id_header: None,
    }));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    let err = p.complete(&request()).unwrap_err();
    let text = format!("{err} / {err:?}");
    assert!(matches!(err, ProviderError::Http { status: 401, .. }));
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("[REDACTED]"));
    assert!(!err.billing_uncertain());

    let (t, _) = fake(Ok(HttpReply {
        status: 503,
        body: "overloaded".into(),
        request_id_header: None,
    }));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    assert!(p.complete(&request()).unwrap_err().billing_uncertain());
}

#[test]
fn transport_errors_and_non_json_bodies_are_scrubbed_and_typed() {
    let (t, _) = fake(Err(format!("connection reset while sending {SECRET}")));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    let err = p.complete(&request()).unwrap_err();
    assert!(matches!(err, ProviderError::Transport(_)));
    assert!(!format!("{err}").contains(SECRET));

    let (t, _) = fake(Ok(HttpReply {
        status: 200,
        body: "<html>".into(),
        request_id_header: None,
    }));
    let mut p = OpenAiProvider::new("m", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    let err = p.complete(&request()).unwrap_err();
    assert!(matches!(err, ProviderError::InvalidResponse(_)));
    assert!(err.billing_uncertain());
}

#[test]
fn provider_debug_output_never_contains_the_key() {
    let openai = OpenAiProvider::new("m", SECRET.into(), None).unwrap();
    let gemini = GeminiProvider::new("m", SECRET.into(), None).unwrap();
    let local = LocalProvider::new("m", "http://localhost:1/v1", Some(SECRET.into())).unwrap();
    for text in [
        format!("{openai:?}"),
        format!("{gemini:?}"),
        format!("{local:?}"),
    ] {
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("SECRETVALUE"), "{text}");
    }
    let err = ProviderError::MissingApiKey {
        provider: "openai".into(),
        env_name: "OPENAI_API_KEY".into(),
    };
    assert!(format!("{err}").contains("OPENAI_API_KEY"));
}

#[test]
fn model_call_events_from_http_providers_carry_no_secret() {
    let (t, _) = fake(ok_reply(json!({
        "id": "chatcmpl-1",
        "choices": [{ "message": { "content": "ok" } }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    })));
    let mut p = OpenAiProvider::new("m1", SECRET.into(), None)
        .unwrap()
        .with_transport(t);
    let mut ledger = CostLedger::new(1.0, rate_card("openai", "m1"));
    let (mut em, sink) = emitter();
    MeteredProvider::new(&mut p, &mut ledger, &mut em, None)
        .call(Phase::Adaptation, &request())
        .unwrap();
    let lines: Vec<String> = sink
        .events()
        .iter()
        .map(|e| e.to_json_line().unwrap())
        .collect();
    assert!(lines.iter().all(|l| !l.contains(SECRET)));
    assert_eq!(model_calls(&sink)[0].cost_status, CostStatus::Priced);
}

// ---------------------------------------------------------------------------------------------
// build_provider
// ---------------------------------------------------------------------------------------------

fn section(kind: ConfigProviderKind) -> ProviderSection {
    ProviderSection {
        kind,
        model: "model-1".into(),
        base_url: None,
        api_key_env: None,
        temperature: Some(0.0),
        top_p: None,
        max_output_tokens: Some(1000),
        seed: Some(1),
        mock_responses_dir: None,
    }
}

#[test]
fn build_provider_paid_requires_named_env_var_with_a_value() {
    let mut s = section(ConfigProviderKind::OpenAi);
    let none = |_: &str| None;
    assert!(matches!(
        build_provider(&s, &none),
        Err(ProviderError::Config(_))
    ));

    s.api_key_env = Some("MY_KEY".into());
    match build_provider(&s, &none) {
        Err(ProviderError::MissingApiKey { env_name, .. }) => assert_eq!(env_name, "MY_KEY"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    let blank = |_: &str| Some("  ".to_owned());
    assert!(matches!(
        build_provider(&s, &blank),
        Err(ProviderError::MissingApiKey { .. })
    ));

    let env = |n: &str| (n == "MY_KEY").then(|| SECRET.to_owned());
    let p = build_provider(&s, &env).unwrap();
    assert_eq!(
        (p.kind(), p.id(), p.model()),
        (ProviderKind::Paid, "openai", "model-1")
    );
    assert!(!format!("{:?}", section(ConfigProviderKind::OpenAi)).contains(SECRET));

    let mut g = section(ConfigProviderKind::Gemini);
    g.api_key_env = Some("MY_KEY".into());
    assert_eq!(build_provider(&g, &env).unwrap().id(), "gemini");
}

#[test]
fn build_provider_local_and_mock() {
    let mut l = section(ConfigProviderKind::Local);
    let none = |_: &str| None;
    assert!(matches!(
        build_provider(&l, &none),
        Err(ProviderError::Config(_))
    ));
    l.base_url = Some("http://localhost:8080/v1".into());
    let p = build_provider(&l, &none).unwrap();
    assert_eq!((p.kind(), p.id()), (ProviderKind::Local, "local"));
    l.api_key_env = Some("LOCAL_KEY".into());
    assert!(matches!(
        build_provider(&l, &none),
        Err(ProviderError::MissingApiKey { .. })
    ));

    let m = section(ConfigProviderKind::Mock);
    let mut bare = build_provider(&m, &none).unwrap();
    assert_eq!(
        (bare.kind(), bare.id(), bare.model()),
        (ProviderKind::Mock, "mock", "model-1")
    );
    assert!(
        bare.complete(&request()).is_err(),
        "no canned responses without a fixture dir"
    );
    let dir = provider_fixtures();
    let mut loaded = build_provider_with_mock_dir(&m, &none, Some(&dir)).unwrap();
    assert!(loaded.complete(&request()).is_ok());
}

#[test]
fn decoding_from_section() {
    let d = Decoding::from_section(&section(ConfigProviderKind::Mock));
    assert_eq!(
        (d.max_output_tokens, d.temperature, d.seed),
        (1000, Some(0.0), Some(1))
    );
    let mut s = section(ConfigProviderKind::Mock);
    s.max_output_tokens = None;
    assert_eq!(
        Decoding::from_section(&s).max_output_tokens,
        harness_core::provider::DEFAULT_MAX_OUTPUT_TOKENS
    );
}
