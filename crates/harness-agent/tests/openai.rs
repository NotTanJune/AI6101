//! OpenAI Responses client behaviour against a local stub server.

mod common;

use std::time::Duration;

use common::{DUMMY_KEY, StubReply, StubServer, drain, request, spec, stream_channel, tool_schema};
use harness_agent::model::common::RetryPolicy;
use harness_agent::model::openai::OpenAiClient;
use harness_agent::model::{AssistantTurn, Item, ModelClient, ModelError, StreamEvent, ToolCall};
use harness_agent::registry::Effort;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn client(server: &StubServer) -> OpenAiClient {
    OpenAiClient::new(DUMMY_KEY.to_owned(), Some(&server.base_url))
        .expect("client")
        .with_retry(RetryPolicy {
            max_attempts: 4,
            backoff: vec![Duration::from_millis(1); 3],
        })
}

fn completed(output: Value, usage: Value) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed", "output": output, "usage": usage}})
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_owned(),
    }
}

fn text_message(text: &str) -> Value {
    json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
           "content": [{"type": "output_text", "text": text}]})
}

#[tokio::test]
async fn openai_text_only_reply_streams_deltas_and_maps_usage() {
    let server = StubServer::start(vec![StubReply::sse(&[
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_text.delta", "delta": "Hel"}),
        json!({"type": "response.output_text.delta", "delta": "lo"}),
        completed(
            json!([text_message("Hello")]),
            json!({"input_tokens": 100, "input_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 10},
                   "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 30}, "total_tokens": 150}),
        ),
    ])])
    .await;
    let sol = spec("sol");
    let history = [user("hi")];
    let (tx, rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&sol, Effort::Xhigh, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");

    assert_eq!(response.turn.text, "Hello");
    assert!(response.turn.tool_calls.is_empty());
    assert_eq!(response.turn.model_alias, "sol");
    assert_eq!(response.turn.raw, vec![text_message("Hello")]);
    assert_eq!(response.request_id, "resp_1");
    assert_eq!(response.usage.input, Some(100));
    assert_eq!(response.usage.cached_input, Some(40));
    assert_eq!(response.usage.cache_write, Some(10));
    assert_eq!(response.usage.output_including_reasoning, Some(50));
    assert_eq!(response.usage.reasoning, Some(30));
    assert_eq!(
        drain(rx),
        vec![
            StreamEvent::TextDelta("Hel".into()),
            StreamEvent::TextDelta("lo".into())
        ]
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.path, "/responses");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {DUMMY_KEY}").as_str())
    );
    assert_eq!(sent.body["model"], "gpt-6.1-sol");
    assert_eq!(sent.body["instructions"], "be helpful");
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(sent.body["reasoning"], json!({"effort": "xhigh"}));
    assert_eq!(sent.body["max_output_tokens"], 4096);
    assert_eq!(
        sent.body["input"],
        json!([{"role": "user", "content": "hi"}])
    );
    assert!(
        sent.body.get("tools").is_none(),
        "no tools key when none offered"
    );
}

#[tokio::test]
async fn openai_two_function_calls_are_parsed_and_announced() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC", "summary": []});
    let call_a = json!({"type": "function_call", "id": "fc_1", "call_id": "call_a",
                        "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"});
    let call_b = json!({"type": "function_call", "id": "fc_2", "call_id": "call_b",
                        "name": "grep", "arguments": "not json"});
    let server = StubServer::start(vec![StubReply::sse(&[
        json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
        json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}}),
        json!({"type": "response.function_call_arguments.delta", "delta": "{\"pa"}),
        json!({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_b", "name": "grep"}}),
        completed(
            json!([reasoning.clone(), call_a.clone(), call_b.clone()]),
            json!({"input_tokens": 10, "output_tokens": 5}),
        ),
    ])])
    .await;
    let sol = spec("sol");
    let history = [user("go")];
    let tools = [tool_schema("read_file"), tool_schema("grep")];
    let (tx, rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&sol, Effort::High, &history, &tools),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");

    assert_eq!(response.turn.text, "");
    assert_eq!(response.turn.tool_calls.len(), 2);
    assert_eq!(response.turn.tool_calls[0].call_id, "call_a");
    assert_eq!(response.turn.tool_calls[0].name, "read_file");
    assert_eq!(
        response.turn.tool_calls[0].arguments,
        json!({"path": "a.txt"})
    );
    assert_eq!(
        response.turn.tool_calls[0].raw_arguments,
        "{\"path\":\"a.txt\"}"
    );
    // Invalid JSON stays a string; the tool layer reports it.
    assert_eq!(response.turn.tool_calls[1].arguments, json!("not json"));
    assert_eq!(response.turn.raw, vec![reasoning, call_a, call_b]);
    assert_eq!(
        drain(rx),
        vec![
            StreamEvent::ToolCallStarted {
                call_id: "call_a".into(),
                name: "read_file".into()
            },
            StreamEvent::ToolCallStarted {
                call_id: "call_b".into(),
                name: "grep".into()
            },
        ]
    );
}

#[tokio::test]
async fn openai_every_tool_is_sent_with_strict_false() {
    let server = StubServer::start(vec![StubReply::sse(&[completed(
        json!([text_message("ok")]),
        json!({}),
    )])])
    .await;
    let sol = spec("sol");
    let history = [user("go")];
    let tools = [tool_schema("a"), tool_schema("b"), tool_schema("c")];
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(
            request(&sol, Effort::High, &history, &tools),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    let sent = &server.requests()[0].body;
    let sent_tools = sent["tools"].as_array().expect("tools");
    assert_eq!(sent_tools.len(), 3);
    for tool in sent_tools {
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["strict"], json!(false), "{tool}");
        assert_eq!(tool["parameters"]["type"], "object");
    }
    assert_eq!(sent_tools[0]["name"], "a");
}

#[tokio::test]
async fn openai_reasoning_item_is_replayed_verbatim_with_tool_output() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC", "summary": []});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_a",
                      "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"});
    let turn = AssistantTurn {
        model_alias: "sol".into(),
        text: String::new(),
        tool_calls: vec![ToolCall {
            call_id: "call_a".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.txt"}),
            raw_arguments: "{\"path\":\"a.txt\"}".into(),
        }],
        raw: vec![reasoning.clone(), call.clone()],
    };
    let history = [
        user("read it"),
        Item::Assistant(turn),
        Item::ToolResult {
            call_id: "call_a".into(),
            name: "read_file".into(),
            output: "contents".into(),
            is_error: false,
        },
    ];
    let server = StubServer::start(vec![StubReply::sse(&[completed(
        json!([text_message("done")]),
        json!({}),
    )])])
    .await;
    let sol = spec("sol");
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(
            request(&sol, Effort::High, &history, &[tool_schema("read_file")]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    assert_eq!(
        server.requests()[0].body["input"],
        json!([
            {"role": "user", "content": "read it"},
            reasoning,
            call,
            {"type": "function_call_output", "call_id": "call_a", "output": "contents"},
        ])
    );
}

#[tokio::test]
async fn openai_foreign_turn_is_rendered_from_text_and_tool_calls() {
    let foreign = AssistantTurn {
        model_alias: "flash".into(),
        text: "checking".into(),
        tool_calls: vec![ToolCall {
            call_id: "g1".into(),
            name: "grep".into(),
            arguments: json!({"pattern": "x"}),
            raw_arguments: "{\"pattern\":\"x\"}".into(),
        }],
        // Gemini steps must never reach OpenAI.
        raw: vec![json!({"type": "thought", "thought_signature": "SIG"})],
    };
    let silent = AssistantTurn {
        model_alias: "flash".into(),
        text: String::new(),
        tool_calls: vec![],
        raw: vec![],
    };
    let history = [
        user("q"),
        Item::Assistant(foreign),
        Item::ToolResult {
            call_id: "g1".into(),
            name: "grep".into(),
            output: "hit".into(),
            is_error: false,
        },
        Item::Assistant(silent),
    ];
    let server = StubServer::start(vec![StubReply::sse(&[completed(
        json!([text_message("ok")]),
        json!({}),
    )])])
    .await;
    let sol = spec("sol");
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(
            request(&sol, Effort::Low, &history, &[tool_schema("grep")]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    let input = &server.requests()[0].body["input"];
    assert_eq!(
        *input,
        json!([
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "checking"},
            {"type": "function_call", "call_id": "g1", "name": "grep", "arguments": "{\"pattern\":\"x\"}"},
            {"type": "function_call_output", "call_id": "g1", "output": "hit"},
        ])
    );
    assert!(!input.to_string().contains("SIG"));
}

#[tokio::test]
async fn openai_astra_request_has_no_sampling_parameters() {
    let server = StubServer::start(vec![StubReply::sse(&[completed(
        json!([text_message("ok")]),
        json!({}),
    )])])
    .await;
    let astra = spec("astra");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(
            request(&astra, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    let body = &server.requests()[0].body;
    assert_eq!(body["model"], "gpt-6-astra");
    for key in ["temperature", "top_p", "seed", "logprobs"] {
        assert!(body.get(key).is_none(), "{key} must not be sent");
    }
}

#[tokio::test]
async fn openai_rate_limit_with_retry_after_zero_is_retried() {
    let server = StubServer::start(vec![
        StubReply::status(
            429,
            &[("retry-after", "0")],
            "{\"error\":{\"code\":\"slow_down\",\"message\":\"slow down\"}}",
        ),
        StubReply::sse(&[completed(json!([text_message("after retry")]), json!({}))]),
    ])
    .await;
    let sol = spec("sol");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("retried");
    assert_eq!(response.turn.text, "after retry");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn openai_retries_stop_after_four_attempts() {
    let busy = || {
        StubReply::status(
            503,
            &[("retry-after", "0")],
            "{\"error\":{\"message\":\"server_is_overloaded\"}}",
        )
    };
    let server = StubServer::start(vec![busy(), busy(), busy(), busy(), busy()]).await;
    let sol = spec("sol");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("gives up");
    assert!(matches!(err, ModelError::Http { status: 503, .. }), "{err}");
    assert!(err.billing_uncertain());
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn openai_unsupported_effort_fails_before_any_request() {
    let server = StubServer::start(vec![]).await;
    let sol = spec("sol");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::None, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("sol rejects none");
    assert!(matches!(err, ModelError::UnsupportedEffort { .. }), "{err}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn openai_error_event_and_failed_status_map_to_invalid_response_without_the_key() {
    let leaked = format!("bad key {DUMMY_KEY} rejected");
    let server = StubServer::start(vec![StubReply::sse(&[
        json!({"type": "error", "message": leaked}),
    ])])
    .await;
    let sol = spec("sol");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("error event");
    assert!(matches!(err, ModelError::InvalidResponse(_)), "{err}");
    assert!(err.billing_uncertain());
    assert!(!err.to_string().contains(DUMMY_KEY), "{err}");

    let server = StubServer::start(vec![StubReply::sse(&[json!({
        "type": "response.failed",
        "response": {"error": {"message": "model exploded"}}
    })])])
    .await;
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("failed status");
    assert!(err.to_string().contains("model exploded"), "{err}");

    let server = StubServer::start(vec![StubReply::sse(&[
        json!({"type": "response.output_text.delta", "delta": "x"}),
    ])])
    .await;
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("truncated stream");
    assert!(
        err.to_string().contains("before response.completed"),
        "{err}"
    );
}

#[tokio::test]
async fn openai_http_error_text_is_scrubbed_of_the_key() {
    let body = json!({"error": {"message": format!("Incorrect API key provided: {DUMMY_KEY}")}})
        .to_string();
    let server = StubServer::start(vec![StubReply::status(401, &[], &body)]).await;
    let sol = spec("sol");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&sol, Effort::High, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("401");
    assert!(matches!(err, ModelError::Http { status: 401, .. }), "{err}");
    assert!(!err.billing_uncertain());
    assert!(!format!("{err} {err:?}").contains(DUMMY_KEY));
    assert!(err.to_string().contains("[REDACTED]"), "{err}");
}

#[tokio::test]
async fn openai_json_schema_uses_text_format() {
    let server = StubServer::start(vec![StubReply::sse(&[completed(
        json!([text_message("{}")]),
        json!({}),
    )])])
    .await;
    let sol = spec("sol");
    let history = [user("go")];
    let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
    let mut req = request(&sol, Effort::High, &history, &[]);
    req.json_schema = Some(&schema);
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(req, tx, CancellationToken::new())
        .await
        .expect("ok");
    let format = &server.requests()[0].body["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["schema"], schema);
    assert!(format["name"].is_string());
}

#[tokio::test]
async fn openai_cancelled_token_returns_cancelled() {
    let server = StubServer::start(vec![]).await;
    let sol = spec("sol");
    let history = [user("go")];
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(request(&sol, Effort::High, &history, &[]), tx, cancel)
        .await
        .expect_err("cancelled");
    assert!(matches!(err, ModelError::Cancelled), "{err}");
}

#[test]
fn openai_base_url_must_be_https_unless_loopback() {
    let err = OpenAiClient::new(DUMMY_KEY.to_owned(), Some("http://example.com/v1"))
        .expect_err("plain http to a remote host");
    assert!(matches!(err, ModelError::Config(_)), "{err}");
    assert!(!err.to_string().contains(DUMMY_KEY));
    assert!(OpenAiClient::new(DUMMY_KEY.to_owned(), Some("https://example.com/v1")).is_ok());
    assert!(OpenAiClient::new(DUMMY_KEY.to_owned(), Some("http://127.0.0.1:9/v1")).is_ok());
    assert!(OpenAiClient::new(DUMMY_KEY.to_owned(), Some("http://localhost:9")).is_ok());
    assert!(OpenAiClient::new(DUMMY_KEY.to_owned(), Some("http://[::1]:9")).is_ok());
    assert!(OpenAiClient::new(String::new(), None).is_err());
    let client = OpenAiClient::new(DUMMY_KEY.to_owned(), None).expect("default base");
    assert_eq!(client.endpoint(), "https://api.openai.com/v1/responses");
    assert!(!format!("{client:?}").contains(DUMMY_KEY));
}
