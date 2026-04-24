//! `OpenAICompatibleBackend` — POSTs to `/v1/chat/completions` and
//! translates the `OpenAI` chat-completions shape into our
//! provider-neutral [`CompletionRequest`] / [`CompletionResponse`]
//! vocabulary.
//!
//! This one backend covers every provider that speaks the `OpenAI` API:
//!
//!  - `OpenRouter` (`https://openrouter.ai/api/v1`, Bearer key)
//!  - `OpenAI` itself (`https://api.openai.com/v1`, Bearer key)
//!  - `Ollama` (`http://localhost:11434/v1`, optional/no key)
//!  - `llama.cpp` server (`http://localhost:8080/v1`)
//!  - `LM Studio` (`http://localhost:1234/v1`)
//!  - `LocalAI`, `vLLM`, any other `OpenAI`-compatible server
//!
//! Why one backend instead of separate `OpenRouterBackend` /
//! `OllamaBackend`: the wire shape is identical, so provider
//! differences collapse to (base URL, optional API key, model id).
//! Making three classes would be busywork.
//!
//! Translation notes — where the two APIs diverge:
//!
//!  - **Tool-use framing.** `Anthropic` models an assistant message as
//!    a single array of heterogeneous content blocks (`text` +
//!    `tool_use`). `OpenAI` splits that: `choices[0].message.content`
//!    is a string (or null), and `tool_calls` is a sidecar array on
//!    the same message. We flatten back to content blocks on parse.
//!  - **Tool results.** `Anthropic` sends tool results inside a
//!    `user`-role message as `tool_result` content blocks. `OpenAI`
//!    requires a `role: "tool"` message per result, carrying only
//!    `tool_call_id` + `content`. One of our user messages with N
//!    `ToolResult` blocks fans out into N `OpenAI` messages.
//!  - **System prompt.** `Anthropic` has a top-level `system` string.
//!    `OpenAI` prepends a `role: "system"` message to the `messages`
//!    array.
//!  - **Streaming framing.** `Anthropic` emits explicit
//!    `content_block_start`/`stop` events around each block. `OpenAI`
//!    streams naked `delta.content` / `delta.tool_calls[i]` chunks and
//!    leaves block boundaries implicit. The stream adapter in this
//!    module synthesizes start/stop events so downstream code (which
//!    was written against our neutral vocabulary) doesn't care.
//!  - **Usage.** `OpenAI` only returns a `usage` block on streaming if
//!    `stream_options.include_usage = true` — we set that by default.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    backend::LlmBackend,
    error::LlmError,
    sse::{SseDecoder, SseFrame},
    types::{
        BlockType, CompletionRequest, CompletionResponse, CompletionStream, ContentBlock, Delta,
        Message, MessageRole, StopReason, StreamEvent, TokenUsage, ToolChoice, ToolDefinition,
    },
};

/// Presets for well-known OpenAI-compatible providers.
///
/// Consumed by the CLI so `--provider openrouter` picks the right base
/// URL + default model without the operator typing the URL themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// <https://openrouter.ai/api/v1>
    OpenRouter,
    /// <https://api.openai.com/v1>
    OpenAi,
    /// <http://localhost:11434/v1> — Ollama's default.
    Ollama,
    /// Custom base URL. The CLI supplies it verbatim.
    Custom,
}

impl Provider {
    /// Base URL for this provider. For `Custom`, callers supply their
    /// own via [`OpenAICompatibleBackend::with_base_model_and_provider`].
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::Ollama => Some("http://localhost:11434/v1"),
            Self::Custom => None,
        }
    }

    /// Identifier prefix — used to tag the backend's
    /// [`LlmBackend::identifier`] so session records stay
    /// provider-attributable.
    pub fn identifier_prefix(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
            Self::Custom => "openai-compat",
        }
    }

    /// Whether an API key is required.
    ///
    /// Local providers (Ollama) accept requests without a key. Remote
    /// ones do not. The backend lets callers pass an empty key either
    /// way — this is just advisory for the CLI's config loading.
    pub fn requires_api_key(self) -> bool {
        matches!(self, Self::OpenRouter | Self::OpenAi)
    }
}

/// Anthropic-neutral LLM backend targeting OpenAI-compatible APIs.
#[derive(Clone)]
pub struct OpenAICompatibleBackend {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    base: String,
    api_key: Option<Redacted>,
    model: String,
    identifier: String,
    provider: Provider,
}

/// Token that prints as `***` so nobody accidentally logs it.
struct Redacted(String);

impl Redacted {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redacted(***)")
    }
}

impl OpenAICompatibleBackend {
    /// Build a backend against a provider preset. Pass an empty
    /// `api_key` only for local providers that don't need one.
    pub fn with_provider_and_model(
        provider: Provider,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let base = provider.default_base_url().ok_or_else(|| {
            LlmError::Other(
                "Provider::Custom has no default base URL — call \
                 with_base_model_and_provider instead"
                    .into(),
            )
        })?;
        Self::with_base_model_and_provider(base, api_key, model, provider)
    }

    /// Full-control constructor. Used by the CLI's `--llm-base-url`
    /// path and by wiremock tests.
    ///
    /// `api_key` may be empty — local providers typically don't require
    /// one. When empty, no `Authorization` header is sent.
    pub fn with_base_model_and_provider(
        base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        provider: Provider,
    ) -> Result<Self, LlmError> {
        let api_key = api_key.into();
        let trimmed = api_key.trim().to_string();
        if trimmed.is_empty() && provider.requires_api_key() {
            return Err(LlmError::AuthError(format!(
                "{} API key is empty",
                provider.identifier_prefix()
            )));
        }
        let client = Client::builder()
            // Connect fast, then give the server real time to stream
            // a tool-use turn. See AnthropicBackend for the rationale
            // — OpenRouter-routed Claude turns against fat tool
            // results easily cross 2–4 minutes.
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| LlmError::Other(format!("building http client: {e}")))?;
        let model = model.into();
        let identifier = format!("{}/{}", provider.identifier_prefix(), model);
        let api_key = if trimmed.is_empty() {
            None
        } else {
            Some(Redacted(trimmed))
        };
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                base: base.into().trim_end_matches('/').to_string(),
                api_key,
                model,
                identifier,
                provider,
            }),
        })
    }

    /// Model id sent on the wire (no provider prefix).
    pub fn model(&self) -> &str {
        &self.inner.model
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.inner.api_key {
            Some(key) => req.header("authorization", format!("Bearer {}", key.as_ref())),
            None => req,
        }
    }
}

#[async_trait]
impl LlmBackend for OpenAICompatibleBackend {
    fn identifier(&self) -> &str {
        &self.inner.identifier
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let body = build_request_body(&self.inner.model, &request, false);
        let url = format!("{}/chat/completions", self.inner.base);
        let mut builder = self.inner.client.post(&url).header("content-type", "application/json");
        builder = self.apply_auth(builder);
        let response = builder
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

    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, LlmError> {
        let body = build_request_body(&self.inner.model, &request, true);
        let url = format!("{}/chat/completions", self.inner.base);
        let mut builder = self
            .inner
            .client
            .post(&url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json");
        builder = self.apply_auth(builder);
        let response = builder
            .json(&body)
            .send()
            .await
            .map_err(classify_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(map_http_error(status, response).await);
        }

        Ok(Box::pin(stream_events(response)))
    }
}

// ---- request serialisation ----------------------------------------------

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    /// Ask `OpenAI` to include `usage` in the final streamed chunk.
    /// Without this, streaming responses report zero tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    /// Text content; required for `user`/`system`/`tool`, optional for
    /// `assistant` (may be null when the message only carries
    /// `tool_calls`).
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// Assistant-only: tool calls the model emitted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    /// Tool-only: id of the call this message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str, // always "function"
    function: WireFunction,
}

#[derive(Serialize)]
struct WireFunction {
    name: String,
    /// JSON-encoded argument object, serialised as a string per
    /// `OpenAI`'s schema.
    arguments: String,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str, // "function"
    function: WireToolFn<'a>,
}

#[derive(Serialize)]
struct WireToolFn<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

fn build_request_body(model: &str, req: &CompletionRequest, stream: bool) -> serde_json::Value {
    let mut messages: Vec<WireMessage> = Vec::new();
    if !req.system.is_empty() {
        messages.push(WireMessage {
            role: "system",
            content: Some(req.system.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for m in &req.messages {
        append_message_to_wire(m, &mut messages);
    }

    let tools: Vec<WireTool<'_>> = req
        .tools
        .iter()
        .map(|t: &ToolDefinition| WireTool {
            kind: "function",
            function: WireToolFn {
                name: &t.name,
                description: &t.description,
                parameters: &t.input_schema,
            },
        })
        .collect();

    let stream_options = if stream {
        Some(serde_json::json!({ "include_usage": true }))
    } else {
        None
    };

    let wire = WireRequest {
        model,
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        tools,
        tool_choice: tool_choice_to_wire(&req.tool_choice),
        stop: req.stop_sequences.clone(),
        stream,
        stream_options,
    };
    serde_json::to_value(&wire).expect("WireRequest serialises")
}

/// Translate one of our [`Message`]s to one-or-more `OpenAI` wire
/// messages. A single user message with multiple `ToolResult` blocks
/// becomes one `OpenAI` `role: "tool"` message per block.
fn append_message_to_wire(m: &Message, out: &mut Vec<WireMessage>) {
    match m.role {
        MessageRole::User => {
            // Split: tool-results go to their own `tool` messages; any
            // remaining text/tool_use blocks land in a user message.
            let mut user_text: Vec<String> = Vec::new();
            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => user_text.push(text.clone()),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(WireMessage {
                            role: "tool",
                            content: Some(content.clone()),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                    // A ToolUse in a user message shouldn't happen in
                    // practice; drop silently rather than error.
                    ContentBlock::ToolUse { .. } => {}
                }
            }
            if !user_text.is_empty() {
                out.push(WireMessage {
                    role: "user",
                    content: Some(user_text.join("\n")),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
        }
        MessageRole::Assistant => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<WireToolCall> = Vec::new();
            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    ContentBlock::ToolUse { id, name, input } => {
                        let arguments = serde_json::to_string(input)
                            .unwrap_or_else(|_| "{}".to_string());
                        tool_calls.push(WireToolCall {
                            id: id.clone(),
                            kind: "function",
                            function: WireFunction {
                                name: name.clone(),
                                arguments,
                            },
                        });
                    }
                    // Assistant wouldn't produce a ToolResult; drop.
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            let content = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join(""))
            };
            out.push(WireMessage {
                role: "assistant",
                content,
                tool_calls,
                tool_call_id: None,
            });
        }
    }
}

fn tool_choice_to_wire(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => Some(serde_json::Value::String("auto".into())),
        ToolChoice::Any => Some(serde_json::Value::String("required".into())),
        ToolChoice::Tool { name } => Some(serde_json::json!({
            "type": "function",
            "function": { "name": name }
        })),
        ToolChoice::None => Some(serde_json::Value::String("none".into())),
    }
}

// ---- response parsing ---------------------------------------------------

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallOut>,
}

#[derive(Deserialize)]
struct WireToolCallOut {
    id: String,
    #[allow(dead_code)]
    #[serde(rename = "type", default)]
    kind: Option<String>,
    function: WireFunctionOut,
}

#[derive(Deserialize)]
struct WireFunctionOut {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

fn parse_response(wire: WireResponse) -> Result<CompletionResponse, LlmError> {
    let model = wire.model.unwrap_or_default();
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::ParseError("response had no choices".into()))?;

    let mut content: Vec<ContentBlock> = Vec::new();
    if let Some(text) = choice.message.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
    }
    for tc in choice.message.tool_calls {
        let input: serde_json::Value = if tc.function.arguments.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&tc.function.arguments).map_err(|e| {
                LlmError::ParseError(format!("tool_call arguments JSON: {e}"))
            })?
        };
        content.push(ContentBlock::ToolUse {
            id: tc.id,
            name: tc.function.name,
            input,
        });
    }

    let stop_reason = match choice.finish_reason.as_deref() {
        // `content_filter` is a soft stop; surface it as an ordinary
        // end-of-turn rather than an error since the agent will still
        // have any partial output it produced.
        Some("stop" | "content_filter") | None => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some(other) => {
            return Err(LlmError::ParseError(format!(
                "unknown finish_reason: {other}"
            )));
        }
    };

    Ok(CompletionResponse {
        content,
        stop_reason,
        usage: TokenUsage {
            input_tokens: wire.usage.prompt_tokens,
            output_tokens: wire.usage.completion_tokens,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
        model,
    })
}

// ---- streaming ----------------------------------------------------------

/// Per-block state the SSE adapter carries. `OpenAI` leaves block
/// framing implicit — we synthesize `ContentBlockStart`/`ContentBlockStop`
/// events by tracking which indices we've already opened.
struct StreamAccumulator {
    text_block_open: Option<u32>,
    tool_blocks: std::collections::BTreeMap<u32, ToolBlockState>,
    next_index: u32,
    usage: TokenUsage,
    stop_reason: StopReason,
    model: String,
    /// Whether we've already emitted `MessageStart`.
    announced_start: bool,
}

impl Default for StreamAccumulator {
    fn default() -> Self {
        Self {
            text_block_open: None,
            tool_blocks: std::collections::BTreeMap::new(),
            next_index: 0,
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
            model: String::new(),
            announced_start: false,
        }
    }
}

#[derive(Default)]
struct ToolBlockState {
    /// Our block-index for this tool call (assigned on first sight).
    block_index: u32,
    id: Option<String>,
    name: Option<String>,
    started: bool,
}

fn stream_events(
    response: reqwest::Response,
) -> impl futures::Stream<Item = Result<StreamEvent, LlmError>> + Send + 'static {
    let bytes: futures::stream::BoxStream<'static, _> = Box::pin(response.bytes_stream());
    let state = StreamState {
        bytes,
        decoder: SseDecoder::new(),
        pending: std::collections::VecDeque::new(),
        acc: StreamAccumulator::default(),
        // `flushed` flips to true once we've emitted MessageDelta +
        // MessageStop — either because we saw `[DONE]` or because the
        // upstream bytes stream hung up without one.
        flushed: false,
        done: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(next) = s.pending.pop_front() {
                if next.is_err() {
                    s.done = true;
                }
                return Some((next, s));
            }
            if s.done {
                return None;
            }
            if s.flushed {
                s.done = true;
                continue;
            }
            match s.bytes.next().await {
                None => {
                    // Stream closed without `[DONE]`. Flush anyway —
                    // real providers sometimes just close the socket.
                    flush_terminal(&mut s.acc, &mut s.pending);
                    s.flushed = true;
                }
                Some(Err(e)) => {
                    s.pending.push_back(Err(classify_reqwest_error(e)));
                }
                Some(Ok(chunk)) => {
                    let frames = match s.decoder.push_bytes(&chunk) {
                        Ok(f) => f,
                        Err(e) => {
                            s.pending.push_back(Err(e));
                            continue;
                        }
                    };
                    for frame in frames {
                        match handle_frame(&frame, &mut s.acc, &mut s.pending) {
                            Ok(FrameOutcome::Continue) => {}
                            Ok(FrameOutcome::Done) => {
                                flush_terminal(&mut s.acc, &mut s.pending);
                                s.flushed = true;
                                break;
                            }
                            Err(e) => {
                                s.pending.push_back(Err(e));
                                break;
                            }
                        }
                    }
                }
            }
        }
    })
}

struct StreamState {
    bytes: futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    decoder: SseDecoder,
    pending: std::collections::VecDeque<Result<StreamEvent, LlmError>>,
    acc: StreamAccumulator,
    flushed: bool,
    done: bool,
}

/// Outcome of processing one SSE frame. `Done` means we saw the
/// `[DONE]` sentinel and the caller should flush final events.
enum FrameOutcome {
    Continue,
    Done,
}

/// Emit `ContentBlockStop` for every open block, then `MessageDelta` +
/// `MessageStop` carrying accumulated usage and `stop_reason`.
fn flush_terminal(
    acc: &mut StreamAccumulator,
    pending: &mut std::collections::VecDeque<Result<StreamEvent, LlmError>>,
) {
    close_open_blocks(acc, pending);
    pending.push_back(Ok(StreamEvent::MessageDelta {
        stop_reason: Some(acc.stop_reason.clone()),
        usage: Some(TokenUsage {
            input_tokens: acc.usage.input_tokens,
            output_tokens: acc.usage.output_tokens,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }),
    }));
    pending.push_back(Ok(StreamEvent::MessageStop));
}

/// Handle one decoded SSE frame. `OpenAI`'s streaming schema is:
///   `data: {"choices":[{"delta":{...},"finish_reason":null|"stop"|...}]}`
/// with a sentinel `data: [DONE]` at the end.
fn handle_frame(
    frame: &SseFrame,
    acc: &mut StreamAccumulator,
    pending: &mut std::collections::VecDeque<Result<StreamEvent, LlmError>>,
) -> Result<FrameOutcome, LlmError> {
    let data = frame.data.trim();
    if data == "[DONE]" {
        return Ok(FrameOutcome::Done);
    }
    if data.is_empty() {
        return Ok(FrameOutcome::Continue);
    }
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| LlmError::ParseError(format!("SSE data JSON: {e}")))?;

    if !acc.announced_start {
        acc.model = value
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        pending.push_back(Ok(StreamEvent::MessageStart {
            model: acc.model.clone(),
        }));
        acc.announced_start = true;
    }

    if let Some(usage) = value.get("usage") {
        if let Some(prompt) = usage.get("prompt_tokens").and_then(serde_json::Value::as_u64) {
            acc.usage.input_tokens = u32::try_from(prompt).unwrap_or(u32::MAX);
        }
        if let Some(comp) = usage
            .get("completion_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            acc.usage.output_tokens = u32::try_from(comp).unwrap_or(u32::MAX);
        }
    }

    let choices = value.get("choices").and_then(serde_json::Value::as_array);
    if let Some(choices) = choices {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                handle_delta(delta, acc, pending);
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
            {
                acc.stop_reason = match reason {
                    "length" => StopReason::MaxTokens,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                };
            }
        }
    }
    Ok(FrameOutcome::Continue)
}

fn handle_delta(
    delta: &serde_json::Value,
    acc: &mut StreamAccumulator,
    pending: &mut std::collections::VecDeque<Result<StreamEvent, LlmError>>,
) {
    // Text content deltas.
    if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            let idx = if let Some(i) = acc.text_block_open {
                i
            } else {
                let i = acc.next_index;
                acc.next_index += 1;
                acc.text_block_open = Some(i);
                pending.push_back(Ok(StreamEvent::ContentBlockStart {
                    index: i,
                    block: BlockType::Text,
                }));
                i
            };
            pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                index: idx,
                delta: Delta::TextDelta(text.to_string()),
            }));
        }
    }

    // Tool call deltas. Each element carries an `index` (0-based across
    // OpenAI's `tool_calls` array), optionally id/type/function.name
    // on first appearance, and function.arguments streamed as string
    // fragments.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(serde_json::Value::as_array) {
        for tc in tool_calls {
            let Some(tool_idx) = tc.get("index").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let tool_idx = u32::try_from(tool_idx).unwrap_or(u32::MAX);

            if !acc.tool_blocks.contains_key(&tool_idx) {
                let assigned = acc.next_index;
                acc.next_index = acc.next_index.saturating_add(1);
                acc.tool_blocks.insert(
                    tool_idx,
                    ToolBlockState {
                        block_index: assigned,
                        ..ToolBlockState::default()
                    },
                );
            }
            let state = acc
                .tool_blocks
                .get_mut(&tool_idx)
                .expect("just inserted");

            if let Some(id) = tc.get("id").and_then(serde_json::Value::as_str) {
                state.id.get_or_insert_with(|| id.to_string());
            }
            if let Some(name) = tc
                .pointer("/function/name")
                .and_then(serde_json::Value::as_str)
            {
                state.name.get_or_insert_with(|| name.to_string());
            }
            // Emit ContentBlockStart once we know id+name.
            if !state.started {
                if let (Some(id), Some(name)) = (&state.id, &state.name) {
                    pending.push_back(Ok(StreamEvent::ContentBlockStart {
                        index: state.block_index,
                        block: BlockType::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                        },
                    }));
                    state.started = true;
                }
            }
            if let Some(args) = tc
                .pointer("/function/arguments")
                .and_then(serde_json::Value::as_str)
            {
                if state.started && !args.is_empty() {
                    pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                        index: state.block_index,
                        delta: Delta::InputJsonDelta(args.to_string()),
                    }));
                }
            }
        }
    }
}

fn close_open_blocks(
    acc: &mut StreamAccumulator,
    pending: &mut std::collections::VecDeque<Result<StreamEvent, LlmError>>,
) {
    if let Some(idx) = acc.text_block_open.take() {
        pending.push_back(Ok(StreamEvent::ContentBlockStop { index: idx }));
    }
    for state in acc.tool_blocks.values() {
        if state.started {
            pending.push_back(Ok(StreamEvent::ContentBlockStop {
                index: state.block_index,
            }));
        }
    }
    acc.tool_blocks.clear();
}

// ---- error mapping ------------------------------------------------------

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

impl std::fmt::Debug for OpenAICompatibleBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatibleBackend")
            .field("provider", &self.inner.provider)
            .field("base", &self.inner.base)
            .field("model", &self.inner.model)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> CompletionRequest {
        CompletionRequest {
            system: "you are a tester".into(),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::text("ping")],
            }],
            tools: vec![ToolDefinition {
                name: "classify_target".into(),
                description: "Classify a target input.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input": { "type": "string" }
                    },
                    "required": ["input"],
                }),
            }],
            max_tokens: 512,
            temperature: Some(0.3),
            tool_choice: ToolChoice::Auto,
            stop_sequences: vec![],
            cache_system_prompt: false,
        }
    }

    #[test]
    fn remote_providers_reject_empty_api_key() {
        let err =
            OpenAICompatibleBackend::with_provider_and_model(Provider::OpenRouter, "", "gpt-4o")
                .unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)));

        let err = OpenAICompatibleBackend::with_provider_and_model(Provider::OpenAi, "", "gpt-4o")
            .unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)));
    }

    #[test]
    fn ollama_allows_empty_api_key_for_local_use() {
        let b = OpenAICompatibleBackend::with_provider_and_model(
            Provider::Ollama,
            "",
            "llama3.1",
        )
        .expect("ollama backend builds without a key");
        assert_eq!(b.identifier(), "ollama/llama3.1");
    }

    #[test]
    fn custom_provider_requires_explicit_base_url() {
        let err = OpenAICompatibleBackend::with_provider_and_model(
            Provider::Custom,
            "key",
            "model",
        )
        .unwrap_err();
        assert!(matches!(err, LlmError::Other(_)));
    }

    #[test]
    fn identifier_tags_each_provider_distinctly() {
        let o = OpenAICompatibleBackend::with_provider_and_model(
            Provider::OpenRouter,
            "sk-or-x",
            "anthropic/claude-opus-4-7",
        )
        .unwrap();
        assert_eq!(o.identifier(), "openrouter/anthropic/claude-opus-4-7");

        let l = OpenAICompatibleBackend::with_provider_and_model(
            Provider::Ollama,
            "",
            "llama3.1:70b",
        )
        .unwrap();
        assert_eq!(l.identifier(), "ollama/llama3.1:70b");
    }

    #[test]
    fn request_body_prepends_system_as_first_message() {
        let body = build_request_body("gpt-4o", &sample_request(), false);
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "you are a tester");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "ping");
    }

    #[test]
    fn request_body_passes_stream_options_only_when_streaming() {
        let buffered = build_request_body("gpt-4o", &sample_request(), false);
        assert!(buffered.get("stream_options").is_none());

        let streamed = build_request_body("gpt-4o", &sample_request(), true);
        assert_eq!(streamed["stream"], true);
        assert_eq!(streamed["stream_options"]["include_usage"], true);
    }

    #[test]
    fn request_body_translates_tools_to_function_shape() {
        let body = build_request_body("gpt-4o", &sample_request(), false);
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "classify_target");
        assert_eq!(
            tools[0]["function"]["parameters"]["type"],
            "object",
            "parameters carries the JSON Schema verbatim"
        );
    }

    #[test]
    fn request_body_translates_tool_choice_variants() {
        fn body_for(tc: ToolChoice) -> serde_json::Value {
            let mut r = sample_request();
            r.tool_choice = tc;
            build_request_body("gpt-4o", &r, false)
        }
        assert_eq!(body_for(ToolChoice::Auto)["tool_choice"], "auto");
        assert_eq!(body_for(ToolChoice::Any)["tool_choice"], "required");
        assert_eq!(body_for(ToolChoice::None)["tool_choice"], "none");
        let specific = body_for(ToolChoice::Tool {
            name: "finalize_report".into(),
        });
        assert_eq!(specific["tool_choice"]["type"], "function");
        assert_eq!(specific["tool_choice"]["function"]["name"], "finalize_report");
    }

    #[test]
    fn assistant_message_with_tool_use_splits_into_content_and_tool_calls() {
        let msg = Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::text("calling classify_target"),
                ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "classify_target".into(),
                    input: serde_json::json!({ "input": "0xdeadbeef" }),
                },
            ],
        };
        let mut out = Vec::new();
        append_message_to_wire(&msg, &mut out);
        assert_eq!(out.len(), 1);
        let wire = serde_json::to_value(&out[0]).unwrap();
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["content"], "calling classify_target");
        assert_eq!(wire["tool_calls"][0]["id"], "tu_1");
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "classify_target");
        let args: serde_json::Value =
            serde_json::from_str(wire["tool_calls"][0]["function"]["arguments"].as_str().unwrap())
                .unwrap();
        assert_eq!(args["input"], "0xdeadbeef");
    }

    #[test]
    fn user_message_with_tool_result_fans_out_to_tool_role_messages() {
        let msg = Message {
            role: MessageRole::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "{\"kind\":\"OnChain\"}".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tu_2".into(),
                    content: "oops".into(),
                    is_error: true,
                },
            ],
        };
        let mut out = Vec::new();
        append_message_to_wire(&msg, &mut out);
        assert_eq!(out.len(), 2, "one tool message per ToolResult block");
        let m0 = serde_json::to_value(&out[0]).unwrap();
        assert_eq!(m0["role"], "tool");
        assert_eq!(m0["tool_call_id"], "tu_1");
        let m1 = serde_json::to_value(&out[1]).unwrap();
        assert_eq!(m1["role"], "tool");
        assert_eq!(m1["tool_call_id"], "tu_2");
    }

    #[test]
    fn parse_response_handles_text_only_turn() {
        let raw = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": { "content": "hello world", "tool_calls": [] },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 3 }
        });
        let wire: WireResponse = serde_json::from_value(raw).unwrap();
        let parsed = parse_response(wire).unwrap();
        assert_eq!(parsed.content.len(), 1);
        assert!(matches!(parsed.content[0], ContentBlock::Text { .. }));
        assert!(matches!(parsed.stop_reason, StopReason::EndTurn));
        assert_eq!(parsed.usage.input_tokens, 12);
        assert_eq!(parsed.usage.output_tokens, 3);
    }

    #[test]
    fn parse_response_handles_tool_calls_turn() {
        let raw = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "classify_target",
                            "arguments": "{\"input\":\"0xdead\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
        });
        let wire: WireResponse = serde_json::from_value(raw).unwrap();
        let parsed = parse_response(wire).unwrap();
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "classify_target");
                assert_eq!(input["input"], "0xdead");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(matches!(parsed.stop_reason, StopReason::ToolUse));
    }

    #[test]
    fn parse_response_handles_mixed_text_and_tool_calls_turn() {
        let raw = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "let me classify",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "classify_target", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let wire: WireResponse = serde_json::from_value(raw).unwrap();
        let parsed = parse_response(wire).unwrap();
        assert_eq!(parsed.content.len(), 2);
        assert!(matches!(parsed.content[0], ContentBlock::Text { .. }));
        assert!(matches!(parsed.content[1], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn parse_response_rejects_unknown_finish_reason() {
        let raw = serde_json::json!({
            "choices": [{
                "message": { "content": "x" },
                "finish_reason": "weird_new_thing"
            }]
        });
        let wire: WireResponse = serde_json::from_value(raw).unwrap();
        assert!(matches!(parse_response(wire), Err(LlmError::ParseError(_))));
    }

    #[test]
    fn parse_response_rejects_empty_choices_array() {
        let raw = serde_json::json!({ "choices": [] });
        let wire: WireResponse = serde_json::from_value(raw).unwrap();
        assert!(matches!(parse_response(wire), Err(LlmError::ParseError(_))));
    }

    #[test]
    fn provider_presets_have_sensible_defaults() {
        assert!(Provider::OpenRouter
            .default_base_url()
            .unwrap()
            .starts_with("https://openrouter.ai"));
        assert!(Provider::Ollama
            .default_base_url()
            .unwrap()
            .contains("localhost"));
        assert!(Provider::Custom.default_base_url().is_none());
        assert!(Provider::OpenRouter.requires_api_key());
        assert!(!Provider::Ollama.requires_api_key());
    }
}
