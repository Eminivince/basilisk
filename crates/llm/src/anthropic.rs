//! Anthropic Messages API backend.
//!
//! Covers non-streaming `complete`. Streaming lands in CP2.
//!
//! Wire format is intentionally hand-rolled (not `anthropic-sdk` or
//! similar) so we keep a thin, auditable dependency footprint — the
//! only external is `reqwest`. The public-facing types from
//! [`crate::types`] are mapped into on-wire `Wire*` shapes at the
//! boundary; the public surface doesn't leak any provider specifics.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    backend::LlmBackend,
    error::LlmError,
    types::{
        CompletionRequest, CompletionResponse, ContentBlock, Message, MessageRole, StopReason,
        TokenUsage, ToolChoice, ToolDefinition,
    },
};

/// Anthropic API default base URL.
const DEFAULT_BASE: &str = "https://api.anthropic.com";
/// API version header value we've validated against.
const API_VERSION: &str = "2023-06-01";
/// Default model — swap to a dated variant when pinning is needed.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

/// Anthropic Messages API backend.
#[derive(Clone, Debug)]
pub struct AnthropicBackend {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    client: Client,
    base: String,
    /// Redacted in `Debug` output — never logged, never serialised.
    api_key: Redacted,
    model: String,
    identifier: String,
}

/// `Debug`-safe wrapper: prints as `Redacted` instead of the secret.
#[derive(Clone)]
struct Redacted(String);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Redacted")
    }
}

impl AsRef<str> for Redacted {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AnthropicBackend {
    /// Construct with an explicit API key and the default model.
    ///
    /// Returns `LlmError::AuthError` when `api_key` is empty (after
    /// trimming) — we'd rather fail at construction than at first
    /// request.
    pub fn new(api_key: impl Into<String>) -> Result<Self, LlmError> {
        Self::with_model(api_key, DEFAULT_MODEL)
    }

    /// Construct with an explicit model identifier (e.g. for pinning to
    /// `claude-opus-4-7-20250929`).
    pub fn with_model(
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        Self::with_base_and_model(DEFAULT_BASE, api_key, model)
    }

    /// Construct against an explicit base URL (used by wiremock tests).
    pub fn with_base_and_model(
        base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let api_key = api_key.into();
        let trimmed = api_key.trim().to_string();
        if trimmed.is_empty() {
            return Err(LlmError::AuthError("ANTHROPIC_API_KEY is empty".into()));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| LlmError::Other(format!("building http client: {e}")))?;
        let model = model.into();
        let identifier = format!("anthropic/{model}");
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base: base.into().trim_end_matches('/').to_string(),
                api_key: Redacted(trimmed),
                model,
                identifier,
            }),
        })
    }

    /// Model name (without the `anthropic/` prefix).
    pub fn model(&self) -> &str {
        &self.inner.model
    }
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    fn identifier(&self) -> &str {
        &self.inner.identifier
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let body = build_request_body(&self.inner.model, &request);
        let url = format!("{}/v1/messages", self.inner.base);
        let response = self
            .inner
            .client
            .post(&url)
            .header("x-api-key", self.inner.api_key.as_ref())
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(classify_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(map_http_error(status, response).await);
        }

        let wire: WireResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ParseError(e.to_string()))?;
        parse_response(wire)
    }
}

// --- wire types ---------------------------------------------------------
//
// Kept private. The public API in `types.rs` is the contract; this is
// just the JSON-on-the-wire shape.

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<serde_json::Value>,
    messages: Vec<WireMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    content: Vec<WireContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_sequence: Option<String>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Forward-compat catch-all; ignored on read.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Default)]
// Field names mirror Anthropic's on-wire shape; the shared postfix is
// theirs, not a design choice on our side.
#[allow(clippy::struct_field_names)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

/// Build the JSON body sent to `/v1/messages`.
///
/// Split out so tests can assert the wire shape without hitting the network.
fn build_request_body(model: &str, req: &CompletionRequest) -> serde_json::Value {
    let system = if req.system.is_empty() {
        None
    } else if req.cache_system_prompt {
        // Array form with an ephemeral cache marker — Anthropic reuses
        // identical-prefix system content across requests at a reduced rate.
        Some(serde_json::json!([
            {
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" },
            }
        ]))
    } else {
        Some(serde_json::Value::String(req.system.clone()))
    };

    let messages: Vec<WireMessage> = req.messages.iter().map(message_to_wire).collect();
    let tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .map(|t: &ToolDefinition| WireTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
        })
        .collect();

    let tool_choice = tool_choice_to_wire(&req.tool_choice);

    let wire = WireRequest {
        model,
        system,
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        tools,
        tool_choice,
        stop_sequences: req.stop_sequences.clone(),
    };

    serde_json::to_value(&wire).expect("WireRequest serialises")
}

fn message_to_wire(m: &Message) -> WireMessage {
    let role = match m.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };
    let content = m.content.iter().map(block_to_wire).collect();
    WireMessage { role, content }
}

fn block_to_wire(b: &ContentBlock) -> serde_json::Value {
    match b {
        ContentBlock::Text { text } => serde_json::json!({ "type": "text", "text": text }),
        ContentBlock::ToolUse { id, name, input } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            // Anthropic accepts either a string `content` or a list of
            // content blocks. Plain string is simplest + lowest-noise.
            let mut obj = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            if *is_error {
                obj["is_error"] = serde_json::Value::Bool(true);
            }
            obj
        }
    }
}

fn tool_choice_to_wire(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => Some(serde_json::json!({ "type": "auto" })),
        ToolChoice::Any => Some(serde_json::json!({ "type": "any" })),
        ToolChoice::Tool { name } => Some(serde_json::json!({ "type": "tool", "name": name })),
        ToolChoice::None => Some(serde_json::json!({ "type": "none" })),
    }
}

fn parse_response(wire: WireResponse) -> Result<CompletionResponse, LlmError> {
    let model = wire.model.unwrap_or_default();

    let content: Vec<ContentBlock> = wire
        .content
        .into_iter()
        .filter_map(|c| match c {
            WireContent::Text { text } => Some(ContentBlock::Text { text }),
            WireContent::ToolUse { id, name, input } => {
                Some(ContentBlock::ToolUse { id, name, input })
            }
            WireContent::Unknown => None,
        })
        .collect();

    let stop_reason = match wire.stop_reason.as_deref() {
        Some("end_turn") | None => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        Some("stop_sequence") => StopReason::StopSequence {
            sequence: wire.stop_sequence.unwrap_or_default(),
        },
        Some(other) => {
            return Err(LlmError::ParseError(format!(
                "unknown stop_reason: {other}"
            )));
        }
    };

    let usage = TokenUsage {
        input_tokens: wire.usage.input_tokens,
        output_tokens: wire.usage.output_tokens,
        cache_read_input_tokens: wire.usage.cache_read_input_tokens,
        cache_creation_input_tokens: wire.usage.cache_creation_input_tokens,
    };

    Ok(CompletionResponse {
        content,
        stop_reason,
        usage,
        model,
    })
}

fn classify_reqwest_error(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout
    } else if e.is_connect() || e.is_request() {
        LlmError::NetworkError(e.to_string())
    } else {
        LlmError::Other(e.to_string())
    }
}

async fn map_http_error(status: reqwest::StatusCode, response: reqwest::Response) -> LlmError {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);

    let body = response.text().await.unwrap_or_default();

    match status.as_u16() {
        401 | 403 => LlmError::AuthError(body),
        429 => LlmError::RateLimited { retry_after },
        code @ 400..=499 => LlmError::BadRequest(format!("HTTP {code}: {body}")),
        code => LlmError::ServerError { status: code, body },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> CompletionRequest {
        CompletionRequest {
            system: "you are a test".into(),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::text("ping")],
            }],
            tools: vec![],
            max_tokens: 100,
            temperature: Some(0.2),
            tool_choice: ToolChoice::Auto,
            stop_sequences: vec![],
            cache_system_prompt: false,
        }
    }

    #[test]
    fn new_rejects_empty_api_key() {
        let err = AnthropicBackend::new("").unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)));
    }

    #[test]
    fn new_rejects_whitespace_api_key() {
        let err = AnthropicBackend::new("   \t  ").unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)));
    }

    #[test]
    fn identifier_includes_provider_and_model() {
        let b = AnthropicBackend::new("sk-ant-x").unwrap();
        assert_eq!(b.identifier(), "anthropic/claude-opus-4-7");
    }

    #[test]
    fn identifier_respects_model_override() {
        let b = AnthropicBackend::with_model("sk-ant-x", "claude-sonnet-4-6").unwrap();
        assert_eq!(b.identifier(), "anthropic/claude-sonnet-4-6");
        assert_eq!(b.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn request_body_has_required_top_level_fields() {
        let body = build_request_body("claude-opus-4-7", &sample_request());
        assert_eq!(body["model"], "claude-opus-4-7");
        assert_eq!(body["max_tokens"], 100);
        // `0.2_f32` is not exactly representable — compare as f64 with epsilon.
        let temp = body["temperature"].as_f64().expect("numeric temperature");
        assert!((temp - 0.2).abs() < 1e-6, "got {temp}");
        assert_eq!(body["system"], "you are a test");
        assert!(body["messages"].is_array());
    }

    #[test]
    fn request_body_encodes_user_text_block() {
        let body = build_request_body("claude-opus-4-7", &sample_request());
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "ping");
    }

    #[test]
    fn request_body_encodes_tool_use_and_tool_result() {
        let req = CompletionRequest {
            system: String::new(),
            messages: vec![
                Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "ping".into(),
                        input: serde_json::json!({"host": "example.com"}),
                    }],
                },
                Message {
                    role: MessageRole::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "tu_1".into(),
                        content: "{\"rtt_ms\": 42}".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 50,
            temperature: None,
            tool_choice: ToolChoice::Auto,
            stop_sequences: vec![],
            cache_system_prompt: false,
        };
        let body = build_request_body("claude-opus-4-7", &req);
        // No system in body when empty.
        assert!(body.get("system").is_none());
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        assert_eq!(assistant["content"][0]["id"], "tu_1");
        let user = &body["messages"][1];
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"][0]["type"], "tool_result");
        assert_eq!(user["content"][0]["tool_use_id"], "tu_1");
        assert!(user["content"][0].get("is_error").is_none());
    }

    #[test]
    fn request_body_sets_is_error_when_tool_result_errored() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
            }],
            tools: vec![],
            max_tokens: 10,
            temperature: None,
            tool_choice: ToolChoice::Auto,
            stop_sequences: vec![],
            cache_system_prompt: false,
        };
        let body = build_request_body("m", &req);
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
    }

    #[test]
    fn request_body_caches_system_prompt_when_flag_set() {
        let mut req = sample_request();
        req.cache_system_prompt = true;
        let body = build_request_body("claude-opus-4-7", &req);
        let system = &body["system"];
        assert!(system.is_array(), "expected array, got {system}");
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "you are a test");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn request_body_includes_tools_with_schema() {
        let mut req = sample_request();
        req.tools.push(ToolDefinition {
            name: "add".into(),
            description: "add two numbers".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "a": {"type": "number"}, "b": {"type": "number"} },
            }),
        });
        let body = build_request_body("m", &req);
        assert_eq!(body["tools"][0]["name"], "add");
        assert_eq!(body["tools"][0]["description"], "add two numbers");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn request_body_tool_choice_encodings() {
        let cases = [
            (ToolChoice::Auto, serde_json::json!({"type": "auto"})),
            (ToolChoice::Any, serde_json::json!({"type": "any"})),
            (
                ToolChoice::Tool { name: "add".into() },
                serde_json::json!({"type": "tool", "name": "add"}),
            ),
            (ToolChoice::None, serde_json::json!({"type": "none"})),
        ];
        for (choice, expected) in cases {
            let mut req = sample_request();
            req.tool_choice = choice;
            let body = build_request_body("m", &req);
            assert_eq!(body["tool_choice"], expected);
        }
    }

    #[test]
    fn request_body_empty_system_is_omitted() {
        let mut req = sample_request();
        req.system = String::new();
        let body = build_request_body("m", &req);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn parse_response_extracts_text_and_tool_use() {
        let wire = serde_json::json!({
            "model": "claude-opus-4-7-20250929",
            "content": [
                { "type": "text", "text": "here we go" },
                {
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "resolve_onchain_system",
                    "input": { "address": "0xabc", "chain": "ethereum" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 120,
                "output_tokens": 45,
                "cache_read_input_tokens": 30
            }
        });
        let wr: WireResponse = serde_json::from_value(wire).unwrap();
        let resp = parse_response(wr).unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.usage.input_tokens, 120);
        assert_eq!(resp.usage.cache_read_input_tokens, Some(30));
        assert_eq!(resp.model, "claude-opus-4-7-20250929");
    }

    #[test]
    fn parse_response_handles_stop_sequence() {
        let wire = serde_json::json!({
            "model": "m",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "stop_sequence",
            "stop_sequence": "</done>",
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        });
        let wr: WireResponse = serde_json::from_value(wire).unwrap();
        let resp = parse_response(wr).unwrap();
        assert_eq!(
            resp.stop_reason,
            StopReason::StopSequence {
                sequence: "</done>".into()
            },
        );
    }

    #[test]
    fn parse_response_ignores_unknown_content_blocks() {
        // Forward-compat: future Anthropic block types shouldn't fail the parse.
        let wire = serde_json::json!({
            "model": "m",
            "content": [
                { "type": "text", "text": "before" },
                { "type": "future_thing", "payload": "???" },
                { "type": "text", "text": "after" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 0, "output_tokens": 0 }
        });
        let wr: WireResponse = serde_json::from_value(wire).unwrap();
        let resp = parse_response(wr).unwrap();
        assert_eq!(resp.content.len(), 2);
    }

    #[test]
    fn parse_response_rejects_unknown_stop_reason() {
        let wire = serde_json::json!({
            "model": "m",
            "content": [],
            "stop_reason": "cosmic_ray",
            "usage": {}
        });
        let wr: WireResponse = serde_json::from_value(wire).unwrap();
        let err = parse_response(wr).unwrap_err();
        assert!(matches!(err, LlmError::ParseError(_)));
    }
}
