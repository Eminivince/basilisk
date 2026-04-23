//! Request / response types shared by every `LlmBackend`.
//!
//! The shape deliberately mirrors Anthropic's Messages API since that's
//! the first backend. Other providers (`OpenAI`, local) get shimmed at
//! their backend's boundary — this crate keeps one canonical vocabulary.

use serde::{Deserialize, Serialize};

/// One full completion request.
///
/// `system` is the system prompt; `messages` is the turn-by-turn history
/// the model should see; `tools` are the tool definitions it may call;
/// `max_tokens` is the upper bound on the assistant's reply length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// When `true`, mark the system prompt with `cache_control: ephemeral`
    /// so Anthropic caches it across turns. Saves real money on long
    /// tool-use loops where the prompt is identical every turn.
    #[serde(default)]
    pub cache_system_prompt: bool,
}

/// One message in the conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

/// Who said it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

/// A single block inside a message's content array.
///
/// Anthropic's API models messages as sequences of heterogeneous blocks
/// — text, tool-use calls the model emitted, tool-result entries the
/// caller sends back. We mirror that directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text. The assistant's natural-language output or a user-
    /// authored prompt.
    Text { text: String },

    /// The assistant decided to call a tool. `input` is the raw JSON it
    /// produced, validated by the caller against the tool's schema.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// The caller's response to a prior `ToolUse`. `content` is the
    /// serialized tool output (typically JSON, but plain text is fine).
    /// `is_error` flips when the tool failed — the assistant sees the
    /// error and decides how to respond.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentBlock {
    /// Convenience for the most common case.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

/// Tool metadata the model sees when deciding whether to call a tool.
///
/// `input_schema` is a JSON Schema describing the `input` object the
/// model should produce. Descriptions matter a lot — the model reads
/// them to pick between tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// How the model should decide whether to call a tool this turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Default. Model decides whether to call a tool.
    #[default]
    Auto,
    /// Model *must* call some tool.
    Any,
    /// Model must call this specific tool.
    Tool { name: String },
    /// Model must not call any tool this turn.
    None,
}

/// Everything a completion returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    pub model: String,
}

/// Why the model stopped producing tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model decided it was done.
    EndTurn,
    /// Hit `max_tokens`. Response may be truncated.
    MaxTokens,
    /// Model emitted tool-use blocks and paused for the caller to respond.
    ToolUse,
    /// Caller-provided stop sequence was produced.
    StopSequence { sequence: String },
}

/// Token accounting for one response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens read from Anthropic's prompt cache (cheaper than fresh input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Tokens written to the cache (charged extra the first time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
}

impl TokenUsage {
    /// Add another turn's usage into this one.
    pub fn accumulate(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens =
            sum_opt(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.cache_creation_input_tokens = sum_opt(
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
    }

    /// Total tokens across all categories. Useful for a coarse budget check.
    pub fn total(&self) -> u64 {
        u64::from(self.input_tokens)
            + u64::from(self.output_tokens)
            + u64::from(self.cache_read_input_tokens.unwrap_or(0))
            + u64::from(self.cache_creation_input_tokens.unwrap_or(0))
    }
}

fn sum_opt(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_text_helper() {
        let block = ContentBlock::text("hi");
        assert_eq!(block, ContentBlock::Text { text: "hi".into() });
    }

    #[test]
    fn tool_choice_default_is_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn token_usage_accumulates() {
        let mut a = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: Some(30),
            cache_creation_input_tokens: None,
        };
        let b = TokenUsage {
            input_tokens: 25,
            output_tokens: 75,
            cache_read_input_tokens: Some(10),
            cache_creation_input_tokens: Some(5),
        };
        a.accumulate(&b);
        assert_eq!(a.input_tokens, 125);
        assert_eq!(a.output_tokens, 125);
        assert_eq!(a.cache_read_input_tokens, Some(40));
        assert_eq!(a.cache_creation_input_tokens, Some(5));
    }

    #[test]
    fn token_usage_total_includes_cache_tokens() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: Some(30),
            cache_creation_input_tokens: Some(20),
        };
        assert_eq!(u.total(), 200);
    }

    #[test]
    fn content_block_serializes_tool_use_with_type_tag() {
        let b = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "ping".into(),
            input: serde_json::json!({"host": "example.com"}),
        };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j["type"], "tool_use");
        assert_eq!(j["name"], "ping");
        assert_eq!(j["id"], "tu_1");
    }

    #[test]
    fn stop_reason_round_trips() {
        for reason in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::ToolUse,
            StopReason::StopSequence {
                sequence: "STOP".into(),
            },
        ] {
            let j = serde_json::to_string(&reason).unwrap();
            let back: StopReason = serde_json::from_str(&j).unwrap();
            assert_eq!(back, reason);
        }
    }
}
