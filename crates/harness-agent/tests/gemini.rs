//! Gemini Interactions client behaviour against a local stub server.

mod common;

use std::time::Duration;

use common::{DUMMY_KEY, StubReply, StubServer, drain, request, spec, stream_channel, tool_schema};
use harness_agent::model::common::RetryPolicy;
use harness_agent::model::gemini::GeminiClient;
use harness_agent::model::{AssistantTurn, Item, ModelClient, ModelError, StreamEvent, ToolCall};
use harness_agent::registry::Effort;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn client(server: &StubServer) -> GeminiClient {
    GeminiClient::new(DUMMY_KEY.to_owned(), Some(&server.base_url))
        .expect("client")
        .with_retry(RetryPolicy {
            max_attempts: 4,
            backoff: vec![Duration::from_millis(1); 3],
        })
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_owned(),
    }
}

fn user_step(text: &str) -> Value {
    json!({"type": "user_input", "content": [{"type": "text", "text": text}]})
}

fn thought() -> Value {
    json!({"type": "thought", "signature": "SIG-1", "summary": []})
}

fn call_step() -> Value {
    json!({"type": "function_call", "id": "fc-1", "name": "read_file", "arguments": {"path": "a.txt"}})
}

fn output_step(text: &str) -> Value {
    json!({"type": "model_output", "content": [{"type": "text", "text": text}]})
}

fn interaction(status: &str, steps: Vec<Value>) -> Value {
    json!({"id": "int-1", "status": status, "steps": steps, "usage": {
        "total_input_tokens": 20, "total_cached_tokens": 5,
        "total_output_tokens": 10, "total_thought_tokens": 30, "total_tokens": 60}})
}

#[tokio::test]
async fn gemini_text_reply_maps_text_usage_and_request() {
    let server = StubServer::start(vec![StubReply::json(&interaction(
        "completed",
        vec![thought(), output_step("Hello "), output_step("world")],
    ))])
    .await;
    let flash = spec("flash");
    let history = [user("hi")];
    let (tx, rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&flash, Effort::Medium, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");

    assert_eq!(response.turn.text, "Hello world");
    assert_eq!(response.turn.model_alias, "flash");
    assert!(response.turn.tool_calls.is_empty());
    assert_eq!(response.request_id, "int-1");
    assert_eq!(response.usage.input, Some(20));
    assert_eq!(response.usage.cached_input, Some(5));
    // Thought tokens are not part of total_output_tokens: 10 + 30.
    assert_eq!(response.usage.output_including_reasoning, Some(40));
    assert_eq!(response.usage.reasoning, Some(30));
    assert_eq!(
        drain(rx),
        vec![StreamEvent::TextDelta("Hello world".into())]
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/interactions");
    assert_eq!(sent.header("x-goog-api-key"), Some(DUMMY_KEY));
    assert!(sent.header("authorization").is_none());
    assert_eq!(sent.body["model"], "gemini-3.8-flash");
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["stream"], false);
    assert_eq!(sent.body["system_instruction"], "be helpful");
    assert_eq!(sent.body["generation_config"]["thinking_level"], "medium");
    assert_eq!(sent.body["generation_config"]["max_output_tokens"], 4096);
    assert_eq!(sent.body["input"], json!([user_step("hi")]));
    assert!(sent.body.get("tools").is_none());
    assert!(sent.body["generation_config"].get("temperature").is_none());
}

#[tokio::test]
async fn gemini_function_call_with_requires_action() {
    let server = StubServer::start(vec![StubReply::json(&interaction(
        "requires_action",
        vec![thought(), call_step()],
    ))])
    .await;
    let pro = spec("pro");
    let history = [user("read it")];
    let tools = [tool_schema("read_file")];
    let (tx, rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&pro, Effort::High, &history, &tools),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");

    assert_eq!(response.turn.tool_calls.len(), 1);
    let call = &response.turn.tool_calls[0];
    assert_eq!(call.call_id, "fc-1");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments, json!({"path": "a.txt"}));
    assert_eq!(call.raw_arguments, "{\"path\":\"a.txt\"}");
    assert_eq!(response.turn.raw, vec![thought(), call_step()]);
    assert!(drain(rx).is_empty(), "no text, so no text delta");

    let sent_tools = server.requests()[0].body["tools"].clone();
    assert_eq!(sent_tools.as_array().expect("tools").len(), 1);
    assert_eq!(sent_tools[0]["type"], "function");
    assert_eq!(sent_tools[0]["name"], "read_file");
    assert!(
        sent_tools[0].get("strict").is_none(),
        "Interactions function tools have no strict flag"
    );
    assert_eq!(
        server.requests()[0].body["generation_config"]["thinking_level"],
        "high"
    );
}

#[tokio::test]
async fn gemini_next_request_replays_thought_and_call_steps_verbatim() {
    let turn = AssistantTurn {
        model_alias: "pro".into(),
        text: String::new(),
        tool_calls: vec![ToolCall {
            call_id: "fc-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.txt"}),
            raw_arguments: "{\"path\":\"a.txt\"}".into(),
        }],
        raw: vec![thought(), call_step()],
    };
    let history = [
        user("read it"),
        Item::Assistant(turn),
        Item::ToolResult {
            call_id: "fc-1".into(),
            name: "read_file".into(),
            output: "contents".into(),
            is_error: false,
        },
    ];
    let server = StubServer::start(vec![StubReply::json(&interaction(
        "completed",
        vec![output_step("done")],
    ))])
    .await;
    let pro = spec("pro");
    let (tx, _rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&pro, Effort::High, &history, &[tool_schema("read_file")]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    assert_eq!(response.turn.text, "done");
    assert_eq!(
        server.requests()[0].body["input"],
        json!([
            user_step("read it"),
            thought(),
            call_step(),
            {"type": "function_result", "name": "read_file", "call_id": "fc-1",
             "result": [{"type": "text", "text": "contents"}]},
        ])
    );
}

#[tokio::test]
async fn gemini_foreign_turns_become_a_plain_transcript_step() {
    let foreign = AssistantTurn {
        model_alias: "sol".into(),
        text: "looking".into(),
        tool_calls: vec![ToolCall {
            call_id: "call_x".into(),
            name: "grep".into(),
            arguments: json!({"pattern": "x"}),
            raw_arguments: "{\"pattern\":\"x\"}".into(),
        }],
        raw: vec![json!({"type": "reasoning", "encrypted_content": "ENC"})],
    };
    let history = [
        user("q"),
        Item::Assistant(foreign),
        Item::ToolResult {
            call_id: "call_x".into(),
            name: "grep".into(),
            output: "hit".into(),
            is_error: false,
        },
    ];
    let server = StubServer::start(vec![StubReply::json(&interaction(
        "completed",
        vec![output_step("ok")],
    ))])
    .await;
    let flash = spec("flash");
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(
            request(&flash, Effort::Low, &history, &[tool_schema("grep")]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");
    let input = &server.requests()[0].body["input"];
    assert_eq!(
        *input,
        json!([
            user_step("q"),
            user_step(
                "Assistant said: looking\nAssistant called grep with {\"pattern\":\"x\"}\nTool grep returned: hit"
            ),
        ])
    );
    assert!(!input.to_string().contains("ENC"));
}

#[tokio::test]
async fn gemini_minimal_effort_is_rejected_before_any_http_call() {
    let server = StubServer::start(vec![]).await;
    let flash = spec("flash");
    let history = [user("go")];
    for effort in [Effort::Minimal, Effort::None, Effort::Max] {
        let (tx, _rx) = stream_channel();
        let err = client(&server)
            .send(
                request(&flash, effort, &history, &[]),
                tx,
                CancellationToken::new(),
            )
            .await
            .expect_err("unsupported");
        assert!(matches!(err, ModelError::UnsupportedEffort { .. }), "{err}");
    }
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn gemini_rate_limit_is_retried_and_errors_are_scrubbed() {
    let server = StubServer::start(vec![
        StubReply::status(
            429,
            &[("retry-after", "0")],
            "{\"error\":{\"message\":\"slow\"}}",
        ),
        StubReply::json(&interaction("completed", vec![output_step("ok")])),
    ])
    .await;
    let flash = spec("flash");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let response = client(&server)
        .send(
            request(&flash, Effort::Low, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("retried");
    assert_eq!(response.turn.text, "ok");
    assert_eq!(server.requests().len(), 2);

    let body = json!({"error": {"message": format!("API key {DUMMY_KEY} not valid")}}).to_string();
    let server = StubServer::start(vec![StubReply::status(400, &[], &body)]).await;
    let (tx, _rx) = stream_channel();
    let err = client(&server)
        .send(
            request(&flash, Effort::Low, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("400");
    assert!(matches!(err, ModelError::Http { status: 400, .. }), "{err}");
    assert!(!format!("{err} {err:?}").contains(DUMMY_KEY));
}

#[tokio::test]
async fn gemini_failed_status_and_missing_steps_are_invalid_responses() {
    let flash = spec("flash");
    let history = [user("go")];
    for body in [
        json!({"id": "i", "status": "failed", "error": {"message": "boom"}}),
        json!({"id": "i", "status": "completed"}),
    ] {
        let server = StubServer::start(vec![StubReply::json(&body)]).await;
        let (tx, _rx) = stream_channel();
        let err = client(&server)
            .send(
                request(&flash, Effort::Low, &history, &[]),
                tx,
                CancellationToken::new(),
            )
            .await
            .expect_err("invalid");
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err}");
    }
}

#[tokio::test]
async fn gemini_json_schema_uses_response_format() {
    let server = StubServer::start(vec![StubReply::json(&interaction(
        "completed",
        vec![output_step("{}")],
    ))])
    .await;
    let flash = spec("flash");
    let history = [user("go")];
    let schema = json!({"type": "object"});
    let mut req = request(&flash, Effort::Low, &history, &[]);
    req.json_schema = Some(&schema);
    let (tx, _rx) = stream_channel();
    client(&server)
        .send(req, tx, CancellationToken::new())
        .await
        .expect("ok");
    assert_eq!(
        server.requests()[0].body["response_format"],
        json!({"type": "text", "mime_type": "application/json", "schema": schema})
    );
}

#[test]
fn gemini_base_url_rules_and_default_endpoint() {
    assert!(GeminiClient::new(DUMMY_KEY.to_owned(), Some("http://example.com")).is_err());
    assert!(GeminiClient::new(DUMMY_KEY.to_owned(), Some("http://127.0.0.1:1")).is_ok());
    let client = GeminiClient::new(DUMMY_KEY.to_owned(), None).expect("default");
    assert_eq!(
        client.endpoint(),
        "https://generativelanguage.googleapis.com/v1beta/interactions"
    );
    assert!(!format!("{client:?}").contains(DUMMY_KEY));
}
