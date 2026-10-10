//! Mock model client behaviour and the history replay helper.

mod common;

use common::{request, spec, stream_channel, tool_schema};
use harness_agent::model::mock::{DEFAULT_USAGE, MockModelClient, Scripted};
use harness_agent::model::{
    AssistantTurn, Item, ModelClient, ModelError, Replayed, StreamEvent, TokenUsage, ToolCall,
    classify_history,
};
use harness_agent::registry::{Effort, ProviderId};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn user(text: &str) -> Item {
    Item::User { text: text.into() }
}

#[tokio::test]
async fn mock_returns_script_in_order_and_records_requests() {
    let mock = MockModelClient::new(
        ProviderId::OpenAi,
        vec![
            Scripted::tool_calls("thinking", &[("read_file", json!({"path": "a"}))]),
            Scripted::text("done").with_usage(TokenUsage {
                input: Some(7),
                output_including_reasoning: Some(3),
                ..TokenUsage::default()
            }),
        ],
    );
    let sol = spec("sol");
    let tools = [tool_schema("read_file")];
    let history = [user("go")];

    let (tx, mut rx) = stream_channel();
    let first = mock
        .send(
            request(&sol, Effort::High, &history, &tools),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("first");
    assert_eq!(first.turn.text, "thinking");
    assert_eq!(first.turn.model_alias, "sol");
    assert_eq!(first.turn.tool_calls[0].call_id, "mock_call_0");
    assert_eq!(first.turn.tool_calls[0].arguments, json!({"path": "a"}));
    assert_eq!(first.usage, DEFAULT_USAGE);
    assert_eq!(
        rx.try_recv().expect("text delta"),
        StreamEvent::TextDelta("thinking".into())
    );
    assert_eq!(
        rx.try_recv().expect("tool call"),
        StreamEvent::ToolCallStarted {
            call_id: "mock_call_0".into(),
            name: "read_file".into()
        }
    );

    let history2 = [user("go"), Item::Assistant(first.turn.clone())];
    let (tx, _rx) = stream_channel();
    let second = mock
        .send(
            request(&sol, Effort::Low, &history2, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("second");
    assert_eq!(second.turn.text, "done");
    assert_eq!(second.usage.input, Some(7));

    let (tx, _rx) = stream_channel();
    let err = mock
        .send(
            request(&sol, Effort::Low, &history2, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("exhausted");
    assert!(matches!(err, ModelError::Mock(_)), "{err}");

    let recorded = mock.requests();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[0].tool_names, vec!["read_file".to_owned()]);
    assert_eq!(recorded[0].history, history.to_vec());
    assert_eq!(recorded[0].effort, Effort::High);
    assert_eq!(recorded[1].history.len(), 2);
    assert_eq!(recorded[1].instructions, "be helpful");
    assert_eq!(mock.call_count(), 3);
}

#[tokio::test]
async fn mock_errors_and_pending_cancellation() {
    let mock = MockModelClient::new(
        ProviderId::Gemini,
        vec![Scripted::error(ModelError::Timeout), Scripted::Pending],
    );
    let flash = spec("flash");
    let history = [user("go")];
    let (tx, _rx) = stream_channel();
    let err = mock
        .send(
            request(&flash, Effort::Low, &history, &[]),
            tx,
            CancellationToken::new(),
        )
        .await
        .expect_err("scripted error");
    assert!(matches!(err, ModelError::Timeout));

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });
    let (tx, _rx) = stream_channel();
    let err = mock
        .send(request(&flash, Effort::Low, &history, &[]), tx, cancel)
        .await
        .expect_err("cancelled while pending");
    assert!(matches!(err, ModelError::Cancelled));
}

#[tokio::test]
async fn mock_loads_fixture_directory_in_file_name_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("02.json"),
        r#"{"text": "second", "usage": {"input": 5, "output_including_reasoning": 6}}"#,
    )
    .expect("write");
    std::fs::write(
        dir.path().join("01.json"),
        r#"{"tool_calls": [{"name": "grep", "call_id": "c1", "arguments": {"pattern": "x"}}]}"#,
    )
    .expect("write");
    std::fs::write(dir.path().join("03.json"), r#"{"error": "boom"}"#).expect("write");
    std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write");

    let mock = MockModelClient::from_fixture_dir(ProviderId::OpenAi, dir.path()).expect("load");
    assert_eq!(mock.remaining(), 3);
    let sol = spec("sol");
    let history = [user("go")];
    let mut turns = Vec::new();
    for _ in 0..3 {
        let (tx, _rx) = stream_channel();
        turns.push(
            mock.send(
                request(&sol, Effort::Low, &history, &[]),
                tx,
                CancellationToken::new(),
            )
            .await,
        );
    }
    let first = turns.remove(0).expect("first");
    assert_eq!(first.turn.tool_calls[0].call_id, "c1");
    assert_eq!(first.turn.tool_calls[0].name, "grep");
    let second = turns.remove(0).expect("second");
    assert_eq!(second.turn.text, "second");
    assert_eq!(second.usage.input, Some(5));
    let third = turns.remove(0).expect_err("third");
    assert!(third.to_string().contains("boom"), "{third}");

    let empty = tempfile::tempdir().expect("tempdir");
    assert!(MockModelClient::from_fixture_dir(ProviderId::OpenAi, empty.path()).is_err());
    std::fs::write(empty.path().join("bad.json"), r#"{"nope": 1}"#).expect("write");
    assert!(MockModelClient::from_fixture_dir(ProviderId::OpenAi, empty.path()).is_err());
}

#[test]
fn mock_replay_classifies_own_and_foreign_turns_and_their_tool_results() {
    let call = |id: &str| ToolCall {
        call_id: id.into(),
        name: "grep".into(),
        arguments: json!({}),
        raw_arguments: "{}".into(),
    };
    let own = AssistantTurn {
        model_alias: "sol".into(),
        text: "own".into(),
        tool_calls: vec![call("c-own")],
        raw: vec![json!({"type": "reasoning"})],
    };
    let foreign = AssistantTurn {
        model_alias: "astra".into(),
        text: "foreign".into(),
        tool_calls: vec![call("c-foreign")],
        raw: vec![json!({"type": "reasoning"})],
    };
    let result = |id: &str| Item::ToolResult {
        call_id: id.into(),
        name: "grep".into(),
        output: format!("out-{id}"),
        is_error: false,
    };
    let history = vec![
        user("q"),
        Item::Assistant(own.clone()),
        result("c-own"),
        Item::Assistant(foreign.clone()),
        result("c-foreign"),
    ];
    let classified = classify_history(&history, "sol");
    assert_eq!(classified.len(), 5);
    assert_eq!(classified[0], Replayed::User("q"));
    assert_eq!(classified[1], Replayed::Own(&own));
    assert!(matches!(
        classified[2],
        Replayed::ToolResult { foreign: false, result } if result.call_id == "c-own"
    ));
    assert_eq!(classified[3], Replayed::Foreign(&foreign));
    assert!(matches!(
        classified[4],
        Replayed::ToolResult { foreign: true, result } if result.output == "out-c-foreign"
    ));

    // From the other model's point of view the roles swap.
    let other = classify_history(&history, "astra");
    assert!(matches!(other[1], Replayed::Foreign(_)));
    assert!(matches!(other[3], Replayed::Own(_)));
}
