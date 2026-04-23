//! End-to-end HTTP tests against a wiremock-hosted fake Anthropic.
//!
//! Covers the happy path, tool-use, and every error path the backend
//! classifies. Requires no API keys and no network.

use basilisk_llm::{
    AnthropicBackend, CompletionRequest, ContentBlock, LlmBackend, LlmError, Message, MessageRole,
    StopReason, ToolChoice, ToolDefinition,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> CompletionRequest {
    CompletionRequest {
        system: "you are a test".into(),
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::text("ping")],
        }],
        tools: vec![],
        max_tokens: 100,
        temperature: None,
        tool_choice: ToolChoice::Auto,
        stop_sequences: vec![],
        cache_system_prompt: false,
    }
}

fn backend(server: &MockServer) -> AnthropicBackend {
    AnthropicBackend::with_base_and_model(server.uri(), "sk-ant-fake", "claude-opus-4-7").unwrap()
}

#[tokio::test]
async fn complete_happy_path_returns_text() {
    let server = MockServer::start().await;
    let resp = serde_json::json!({
        "model": "claude-opus-4-7-20250929",
        "content": [{ "type": "text", "text": "pong" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 9, "output_tokens": 1 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-fake"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .mount(&server)
        .await;

    let out = backend(&server).complete(req()).await.unwrap();
    assert_eq!(out.stop_reason, StopReason::EndTurn);
    assert_eq!(out.model, "claude-opus-4-7-20250929");
    match &out.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "pong"),
        other => panic!("unexpected block: {other:?}"),
    }
    assert_eq!(out.usage.input_tokens, 9);
    assert_eq!(out.usage.output_tokens, 1);
}

#[tokio::test]
async fn complete_surfaces_tool_use_blocks() {
    let server = MockServer::start().await;
    let resp = serde_json::json!({
        "model": "claude-opus-4-7",
        "content": [
            { "type": "text", "text": "calling tool" },
            {
                "type": "tool_use",
                "id": "tu_abc",
                "name": "classify_target",
                "input": { "input": "0xdead" }
            }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 50, "output_tokens": 20 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .mount(&server)
        .await;

    let mut r = req();
    r.tools.push(ToolDefinition {
        name: "classify_target".into(),
        description: "classify a target".into(),
        input_schema: serde_json::json!({"type": "object"}),
    });
    let out = backend(&server).complete(r).await.unwrap();
    assert_eq!(out.stop_reason, StopReason::ToolUse);
    assert_eq!(out.content.len(), 2);
    match &out.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tu_abc");
            assert_eq!(name, "classify_target");
            assert_eq!(input["input"], "0xdead");
        }
        other => panic!("unexpected block: {other:?}"),
    }
}

#[tokio::test]
async fn complete_401_is_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
        .mount(&server)
        .await;

    let err = backend(&server).complete(req()).await.unwrap_err();
    assert!(matches!(err, LlmError::AuthError(_)), "got {err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn complete_429_with_retry_after_is_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;

    let err = backend(&server).complete(req()).await.unwrap_err();
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(30)));
        }
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn complete_400_is_bad_request_not_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("messages required"))
        .mount(&server)
        .await;

    let err = backend(&server).complete(req()).await.unwrap_err();
    assert!(matches!(err, LlmError::BadRequest(_)), "got {err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn complete_500_is_server_error_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let err = backend(&server).complete(req()).await.unwrap_err();
    match &err {
        LlmError::ServerError { status, body } => {
            assert_eq!(*status, 503);
            assert!(body.contains("overloaded"));
        }
        other => panic!("got {other:?}"),
    }
    assert!(err.is_retryable());
}

#[tokio::test]
async fn complete_malformed_body_is_parse_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .mount(&server)
        .await;
    let err = backend(&server).complete(req()).await.unwrap_err();
    assert!(matches!(err, LlmError::ParseError(_)), "got {err:?}");
}

#[tokio::test]
async fn complete_round_trips_cached_system_prompt_marker() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    // Assert the on-wire body contains the ephemeral cache marker the
    // backend emits when `cache_system_prompt: true`.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({
            "system": [{
                "type": "text",
                "cache_control": { "type": "ephemeral" }
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "claude-opus-4-7",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 100 }
        })))
        .mount(&server)
        .await;

    let mut r = req();
    r.cache_system_prompt = true;
    let out = backend(&server).complete(r).await.unwrap();
    assert_eq!(out.usage.cache_read_input_tokens, Some(100));
}
