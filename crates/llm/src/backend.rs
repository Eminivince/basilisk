//! The `LlmBackend` trait and supporting abstractions.
//!
//! Exposes both the buffered `complete` entry point and the streaming
//! `stream` variant. The agent loop consumes `stream` so the terminal
//! UI can print tokens as they arrive; callers that just want the full
//! response use `complete` (which the default implementation wires on
//! top of `stream`).

use async_trait::async_trait;
use futures::StreamExt;

use crate::{
    error::LlmError,
    types::{
        BlockType, CompletionRequest, CompletionResponse, CompletionStream, ContentBlock, Delta,
        StopReason, StreamEvent, TokenUsage,
    },
};

/// A model-agnostic LLM provider.
///
/// Implementations wrap one provider's HTTP surface. Callers (the agent
/// loop, tests) hold a `&dyn LlmBackend` or `Arc<dyn LlmBackend>` and
/// don't care which backend they're talking to.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// A stable, human-readable identifier for this backend + model —
    /// e.g. `"anthropic/claude-opus-4-7"`. The agent records this on
    /// the session so resumes can detect model drift.
    fn identifier(&self) -> &str;

    /// Send a completion request and wait for the full response.
    ///
    /// Default implementation folds [`Self::stream`]; backends that
    /// want to issue a non-streaming HTTP call for whatever reason can
    /// override, but the default is correct for every Anthropic-shaped
    /// provider.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let stream = self.stream(request).await?;
        collect_stream(stream).await
    }

    /// Send a completion request and receive events as they arrive.
    ///
    /// Implementations are expected to be cancel-safe: if the future
    /// holding the stream is dropped mid-flight, no partial state
    /// leaks out to subsequent calls.
    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, LlmError>;
}

/// Fold a streaming response into a buffered [`CompletionResponse`].
///
/// Used by the default [`LlmBackend::complete`] implementation; exposed
/// publicly so tests and ad-hoc callers can reuse the same logic.
pub async fn collect_stream(mut stream: CompletionStream) -> Result<CompletionResponse, LlmError> {
    let mut model = String::new();
    let mut blocks: Vec<PartialBlock> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = TokenUsage::default();

    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::MessageStart { model: m } => {
                model = m;
            }
            StreamEvent::ContentBlockStart { index, block } => {
                let idx = index as usize;
                if blocks.len() <= idx {
                    blocks.resize_with(idx + 1, PartialBlock::default);
                }
                blocks[idx] = PartialBlock::from_start(block);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let idx = index as usize;
                if idx < blocks.len() {
                    blocks[idx].push(delta);
                }
            }
            StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta {
                stop_reason: sr,
                usage: u,
            } => {
                if let Some(sr) = sr {
                    stop_reason = sr;
                }
                if let Some(u) = u {
                    // Output-token count arrives in the final MessageDelta.
                    // Input-token counts come from MessageStart (captured
                    // there if the backend surfaces it); we take the max of
                    // what we've seen so streaming + non-streaming match.
                    usage.input_tokens = usage.input_tokens.max(u.input_tokens);
                    usage.output_tokens = usage.output_tokens.max(u.output_tokens);
                    if u.cache_read_input_tokens.is_some() {
                        usage.cache_read_input_tokens = u.cache_read_input_tokens;
                    }
                    if u.cache_creation_input_tokens.is_some() {
                        usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                    }
                }
            }
            StreamEvent::MessageStop => break,
        }
    }

    let content: Result<Vec<ContentBlock>, LlmError> =
        blocks.into_iter().map(PartialBlock::finish).collect();
    Ok(CompletionResponse {
        content: content?,
        stop_reason,
        usage,
        model,
    })
}

/// Accumulator for one content block while the stream is in flight.
#[derive(Default)]
struct PartialBlock {
    kind: Option<BlockType>,
    text: String,
    input_json: String,
}

impl PartialBlock {
    fn from_start(block: BlockType) -> Self {
        Self {
            kind: Some(block),
            text: String::new(),
            input_json: String::new(),
        }
    }

    fn push(&mut self, delta: Delta) {
        match delta {
            Delta::TextDelta(s) => self.text.push_str(&s),
            Delta::InputJsonDelta(s) => self.input_json.push_str(&s),
        }
    }

    fn finish(self) -> Result<ContentBlock, LlmError> {
        match self.kind {
            Some(BlockType::Text) => Ok(ContentBlock::Text { text: self.text }),
            Some(BlockType::ToolUse { id, name }) => {
                // Anthropic sends `{}` when the model produced no input.
                let raw = if self.input_json.is_empty() {
                    "{}".to_string()
                } else {
                    self.input_json
                };
                let input: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|e| LlmError::ParseError(format!("tool_use input: {e}")))?;
                Ok(ContentBlock::ToolUse { id, name, input })
            }
            None => Err(LlmError::ParseError(
                "content block delta without start".into(),
            )),
        }
    }
}
