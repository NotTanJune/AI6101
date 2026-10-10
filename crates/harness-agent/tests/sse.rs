//! SSE parser behaviour.

use harness_agent::model::sse::{SseEvent, SseItem, SseParser};

fn events(items: Vec<SseItem>) -> Vec<(Option<String>, String)> {
    items
        .into_iter()
        .filter_map(|i| match i {
            SseItem::Event(SseEvent { event, data }) => Some((event, data)),
            SseItem::Done => None,
        })
        .collect()
}

#[test]
fn sse_parses_events_regardless_of_chunk_boundaries() {
    let text = "event: a\ndata: {\"x\":1}\n\n: comment\nevent: b\ndata: line1\ndata: line2\n\ndata: [DONE]\n\n";
    // Feed one byte at a time and in two halves: the result must be identical.
    for step in [1usize, 3, 7, text.len()] {
        let mut parser = SseParser::new();
        let mut items = Vec::new();
        for chunk in text.as_bytes().chunks(step) {
            items.extend(parser.push(chunk));
        }
        assert!(parser.is_done(), "step {step}");
        assert_eq!(items.last(), Some(&SseItem::Done), "step {step}");
        assert_eq!(
            events(items),
            vec![
                (Some("a".to_owned()), "{\"x\":1}".to_owned()),
                (Some("b".to_owned()), "line1\nline2".to_owned()),
            ],
            "step {step}"
        );
    }
}

#[test]
fn sse_handles_crlf_split_across_chunks_and_multibyte_split() {
    let mut parser = SseParser::new();
    // "data: h" + first byte of e-acute | second byte + "llo" + CR | LF CR LF
    let mut items = parser.push(b"data: h\xc3");
    items.extend(parser.push(b"\xa9llo\r"));
    items.extend(parser.push(b"\n\r\n"));
    assert_eq!(events(items), vec![(None, "h\u{e9}llo".to_owned())]);
}

#[test]
fn sse_field_without_space_and_unknown_fields_and_finish_flush() {
    let mut parser = SseParser::new();
    let mut items = parser.push(b"id: 5\nretry: 10\ndata:nospace\n");
    assert!(
        items.is_empty(),
        "event is not complete without a blank line"
    );
    items.extend(parser.finish());
    assert_eq!(events(items), vec![(None, "nospace".to_owned())]);
}

#[test]
fn sse_input_after_done_is_ignored() {
    let mut parser = SseParser::new();
    let items = parser.push(b"data: [DONE]\n\ndata: late\n\n");
    assert_eq!(items, vec![SseItem::Done]);
    assert!(parser.push(b"data: later\n\n").is_empty());
}
