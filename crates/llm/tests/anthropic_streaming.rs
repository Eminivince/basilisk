//! Streaming (`/v1/messages` with SSE body) tests via wiremock.
//!
//! We drive `stream()` + `collect_stream` end-to-end by serving the
//! same canned SSE chunks Anthropic's API emits.

use basilisk_llm::{
    collect_stream, AnthropicBackend, BlockType, CompletionRequest, ContentBlock, Delta,
    LlmBackend, Message, MessageRole, StopReason, StreamEvent, ToolChoice,
};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> CompletionRequest {
    CompletionRequest {
        system: "you are a test".into(),
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::text("hi")],
        }],
        tools: vec![],
        max_tokens: 200,
        temperature: None,
        tool_choice: ToolChoice::Auto,
        stop_sequences: vec![],
        cache_system_prompt: false,
    }
}

fn backend(server: &MockServer) -> AnthropicBackend {
    AnthropicBackend::with_base_and_model(server.uri(), "sk-ant-fake", "claude-opus-4-7").unwrap()
}

/// Canned SSE body for a simple "hello world" two-delta text response.
const HELLO_SSE: &[u8] = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7-20250929\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: ping\n\
data: {\"type\":\"ping\"}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

/// Canned SSE body for a tool-use response. `input` is streamed as two
/// `input_json_delta` fragments that must be concatenated.
const TOOL_USE_SSE: &[u8] = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"resolve_onchain_system\",\"input\":{}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"address\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"0xabc\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

#[tokio::test]
async fn stream_emits_full_event_sequence_for_text_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({ "stream": true })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(HELLO_SSE, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut events = backend(&server).stream(req()).await.unwrap();
    let mut seen: Vec<StreamEvent> = Vec::new();
    while let Some(ev) = events.next().await {
        seen.push(ev.unwrap());
    }

    // Heartbeat filtered out; message_stop is the terminal event.
    assert!(
        matches!(seen.first(), Some(StreamEvent::MessageStart { model }) if model == "claude-opus-4-7-20250929"),
        "first event was {:?}",
        seen.first(),
    );
    assert!(matches!(seen.last(), Some(StreamEvent::MessageStop)));
    let deltas: Vec<&StreamEvent> = seen
        .iter()
        .filter(|e| matches!(e, StreamEvent::ContentBlockDelta { .. }))
        .collect();
    assert_eq!(deltas.len(), 2);
    assert!(!seen
        .iter()
        .any(|e| matches!(e, StreamEvent::ContentBlockStart { block, .. } if !matches!(block, BlockType::Text))));
}

#[tokio::test]
async fn collect_stream_folds_deltas_into_final_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(HELLO_SSE, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let stream = backend(&server).stream(req()).await.unwrap();
    let resp = collect_stream(stream).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.content.len(), 1);
    match &resp.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "hello world"),
        other => panic!("got {other:?}"),
    }
    assert_eq!(resp.usage.output_tokens, 7);
}

#[tokio::test]
async fn stream_reconstructs_tool_use_input_from_json_deltas() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(TOOL_USE_SSE, "text/event-stream"),
        )
        .mount(&server)
        .await;

    // Collect individual events to assert fragment delta types, then
    // re-stream to build the final response.
    let mut events = backend(&server).stream(req()).await.unwrap();
    let mut fragments: Vec<String> = Vec::new();
    while let Some(ev) = events.next().await {
        if let StreamEvent::ContentBlockDelta {
            delta: Delta::InputJsonDelta(s),
            ..
        } = ev.unwrap()
        {
            fragments.push(s);
        }
    }
    assert_eq!(fragments.join(""), r#"{"address":"0xabc"}"#);
}

#[tokio::test]
async fn default_complete_delegates_to_stream_when_override_present() {
    // AnthropicBackend overrides complete to issue a non-streaming POST;
    // prove that the streaming path also produces a coherent response so
    // callers which fold the stream get the same data.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(TOOL_USE_SSE, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let stream = backend(&server).stream(req()).await.unwrap();
    let resp = collect_stream(stream).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    match &resp.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tu_1");
            assert_eq!(name, "resolve_onchain_system");
            assert_eq!(input["address"], "0xabc");
        }
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn stream_surfaces_http_errors_before_body_parse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;
    let res = backend(&server).stream(req()).await;
    match res {
        Ok(_) => panic!("expected auth error, got ok"),
        Err(err) => assert!(
            matches!(err, basilisk_llm::LlmError::AuthError(_)),
            "got {err:?}"
        ),
    }
}
