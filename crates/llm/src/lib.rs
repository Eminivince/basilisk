//! Model-agnostic LLM backend for Basilisk.
//!
//! The crate provides:
//!  * [`LlmBackend`] — a provider-agnostic trait for completion requests.
//!  * [`AnthropicBackend`] — first implementation, using the Anthropic
//!    Messages API.
//!  * Shared request / response / error / pricing types.
//!
//! Streaming support lands in set-6 CP2; the CP1 surface is the
//! non-streaming `complete` path, which is enough to drive the agent
//! loop's first iteration.
//!
//! Public vocabulary ([`types`]) mirrors Anthropic's Messages shape
//! because that's where we've validated first. `OpenAI` / local
//! implementations shim their provider types at the backend boundary;
//! downstream agent code never sees provider-specific structures.

pub mod anthropic;
pub mod backend;
pub mod error;
pub mod openai_compat;
pub mod pricing;
pub(crate) mod sse;
pub mod types;

pub use anthropic::{AnthropicBackend, DEFAULT_MODEL, DEFAULT_VULN_MODEL};
pub use backend::{collect_stream, LlmBackend};
pub use error::LlmError;
pub use openai_compat::{OpenAICompatibleBackend, Provider};
pub use pricing::{ModelPricing, ModelPricingSource, PricingTable};
pub use types::{
    BlockType, CompletionRequest, CompletionResponse, CompletionStream, ContentBlock, Delta,
    Message, MessageRole, StopReason, StreamEvent, TokenUsage, ToolChoice, ToolDefinition,
};
