//! Reusable test doubles for the agent — `CP5d`.
//!
//! Everything in here is `pub` so tests in downstream crates (the CLI
//! in `CP6`, the end-to-end harness in `CP8`) can drive the agent loop
//! without each crate rewriting its own mock. Nothing here is suitable
//! for production use.
//!
//! The surface is intentionally small:
//!  - [`MockLlmBackend`] — an [`LlmBackend`] that hands out canned
//!    [`CompletionResponse`]s (or errors) in FIFO order.
//!  - [`MockResponse`] builder for the canonical response shapes the
//!    agent loop exercises.
//!  - [`EchoTool`] — deterministic test tool that echoes its input.
//!  - [`build_test_runner`] — assembles an [`AgentRunner`] with an
//!    in-memory store + the given backend + a minimal tool registry
//!    (echo + finalize). Callers own the repo-cache directory so they
//!    can pick a tempdir lifetime that matches the test.
//!
//! [`LlmBackend`]: basilisk_llm::LlmBackend
//! [`AgentRunner`]: crate::AgentRunner

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::{
    CompletionRequest, CompletionResponse, CompletionStream, ContentBlock, LlmBackend, LlmError,
    StopReason, TokenUsage,
};

use crate::runner::AgentRunner;
use crate::session::SessionStore;
use crate::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use crate::tools::{Confidence, FinalizeReport, FINALIZE_REPORT_NAME};
use crate::Budget;

/// FIFO-scripted [`LlmBackend`]. Each `complete` call pops the next
/// queued item; calling beyond the queue returns an [`LlmError`] so
/// tests fail loudly rather than hang.
///
/// Overrides `complete` directly rather than rebuilding a streaming
/// sequence — `CP5c`'s loop uses `complete`, and a stream-backed mock
/// would just hand-roll the same events back for the default `complete`
/// to fold them. When the loop switches to raw `stream` in `CP6`, we'll
/// extend this with a stream-aware variant.
pub struct MockLlmBackend {
    id: String,
    queue: Mutex<VecDeque<Result<CompletionResponse, LlmError>>>,
}

impl MockLlmBackend {
    /// Empty queue. Push with [`Self::push`] / [`Self::push_err`].
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: model.into(),
            queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Shortcut: seed with every response at once.
    #[must_use]
    pub fn with_responses(
        model: impl Into<String>,
        responses: impl IntoIterator<Item = CompletionResponse>,
    ) -> Self {
        let backend = Self::new(model);
        for r in responses {
            backend.push(r);
        }
        backend
    }

    /// Append one successful response to the back of the queue.
    pub fn push(&self, response: CompletionResponse) -> &Self {
        self.queue.lock().unwrap().push_back(Ok(response));
        self
    }

    /// Append one error to the back of the queue.
    pub fn push_err(&self, err: LlmError) -> &Self {
        self.queue.lock().unwrap().push_back(Err(err));
        self
    }

    /// Number of queued items still waiting to be served.
    pub fn remaining(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

#[async_trait]
impl LlmBackend for MockLlmBackend {
    fn identifier(&self) -> &str {
        &self.id
    }

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Other("mock backend exhausted".into())))
    }

    async fn stream(&self, _request: CompletionRequest) -> Result<CompletionStream, LlmError> {
        Err(LlmError::Other(
            "MockLlmBackend does not stream; call complete()".into(),
        ))
    }
}

/// Build one [`CompletionResponse`] fluently.
///
/// Defaults: `stop_reason = ToolUse` when any `tool_use` is present,
/// otherwise `EndTurn`; zero token usage; `model = "mock"`. Override
/// with the setters if a test needs something specific.
#[derive(Debug, Clone)]
pub struct MockResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<StopReason>,
    usage: TokenUsage,
    model: String,
}

impl MockResponse {
    #[must_use]
    pub fn new() -> Self {
        Self {
            content: Vec::new(),
            stop_reason: None,
            usage: TokenUsage::default(),
            model: "mock".into(),
        }
    }

    /// Add a text block. Multiple calls append multiple blocks.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.content.push(ContentBlock::text(text));
        self
    }

    /// Add a tool-use block.
    #[must_use]
    pub fn tool_use(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        self.content.push(ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        });
        self
    }

    /// Shorthand for a single `finalize_report` tool-use block.
    #[must_use]
    pub fn finalize(
        self,
        id: impl Into<String>,
        markdown: impl Into<String>,
        confidence: Confidence,
        notes: Option<String>,
    ) -> Self {
        let mut input = serde_json::json!({
            "markdown": markdown.into(),
            "confidence": confidence_str(confidence),
        });
        if let Some(n) = notes {
            input["notes"] = serde_json::Value::String(n);
        }
        self.tool_use(id, FINALIZE_REPORT_NAME, input)
    }

    /// Override token usage (default is zero).
    #[must_use]
    pub fn usage(mut self, input_tokens: u32, output_tokens: u32) -> Self {
        self.usage.input_tokens = input_tokens;
        self.usage.output_tokens = output_tokens;
        self
    }

    /// Override the stop reason. If unset, it's inferred from content.
    #[must_use]
    pub fn stop_reason(mut self, sr: StopReason) -> Self {
        self.stop_reason = Some(sr);
        self
    }

    /// Override the model identifier carried on the response (defaults
    /// to `"mock"`; doesn't have to match the backend's identifier).
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn build(self) -> CompletionResponse {
        let has_tool_use = self
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        let stop_reason = self.stop_reason.unwrap_or(if has_tool_use {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        CompletionResponse {
            content: self.content,
            stop_reason,
            usage: self.usage,
            model: self.model,
        }
    }
}

impl Default for MockResponse {
    fn default() -> Self {
        Self::new()
    }
}

impl From<MockResponse> for CompletionResponse {
    fn from(r: MockResponse) -> Self {
        r.build()
    }
}

/// Deterministic tool that echoes its input back as `{"echo": <input>}`.
///
/// Use this for loop-level tests where what the tool *does* is
/// irrelevant — only the agent's decision to call it matters. Every
/// input is accepted.
pub struct EchoTool;

impl EchoTool {
    pub const NAME: &'static str = "echo";
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &'static str {
        "Test tool. Returns the input you pass in under an `echo` key. \
         Not available in production."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok(serde_json::json!({ "echo": input }))
    }
}

/// Registry with only [`EchoTool`] + [`FinalizeReport`]. Enough for
/// loop-level tests that don't want to drag real filesystem / RPC
/// tools into the assertion surface.
#[must_use]
pub fn echo_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(EchoTool));
    r.register(Arc::new(FinalizeReport));
    r
}

/// Assemble an [`AgentRunner`] backed by `backend` + an in-memory
/// session store + the [`echo_registry`].
///
/// `repo_cache_root` should be a writable directory the caller owns
/// (typically a `tempfile::TempDir`). The runner holds an `Arc` into
/// that directory for the rest of its life, so keep the tempdir alive
/// at least as long as the runner.
#[must_use]
pub fn build_test_runner(
    backend: Arc<dyn LlmBackend>,
    repo_cache_root: PathBuf,
    budget: Budget,
) -> AgentRunner {
    let store = Arc::new(SessionStore::open_in_memory().expect("in-memory store opens"));
    let config = Arc::new(Config::default());
    let github = Arc::new(GithubClient::new(None).expect("tokenless github client"));
    let repo_cache = Arc::new(RepoCache::open_at(repo_cache_root).expect("repo cache opens"));
    AgentRunner::new(
        backend,
        echo_registry(),
        store,
        config,
        github,
        repo_cache,
        "you are a helpful test auditor",
        budget,
    )
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_backend_drains_queue_in_order() {
        let backend = MockLlmBackend::new("m");
        backend.push(MockResponse::new().text("one").usage(1, 1).build());
        backend.push(MockResponse::new().text("two").usage(2, 2).build());
        assert_eq!(backend.remaining(), 2);

        let req = CompletionRequest {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1,
            temperature: None,
            tool_choice: basilisk_llm::ToolChoice::default(),
            stop_sequences: Vec::new(),
            cache_system_prompt: false,
        };
        let r1 = backend.complete(req.clone()).await.unwrap();
        let r2 = backend.complete(req.clone()).await.unwrap();
        assert_eq!(r1.usage.input_tokens, 1);
        assert_eq!(r2.usage.input_tokens, 2);

        let err = backend.complete(req).await.unwrap_err();
        assert!(err.to_string().contains("exhausted"));
    }

    #[test]
    fn mock_response_infers_tool_use_stop_reason_when_tool_use_present() {
        let r = MockResponse::new()
            .tool_use("tu_1", "echo", serde_json::json!({}))
            .build();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn mock_response_infers_end_turn_for_text_only() {
        let r = MockResponse::new().text("done").build();
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn mock_response_finalize_shortcut_builds_expected_input() {
        let r = MockResponse::new()
            .finalize(
                "tu_f",
                "# brief",
                Confidence::High,
                Some("notes here".into()),
            )
            .build();
        assert_eq!(r.content.len(), 1);
        match &r.content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, FINALIZE_REPORT_NAME);
                assert_eq!(input["markdown"], "# brief");
                assert_eq!(input["confidence"], "high");
                assert_eq!(input["notes"], "notes here");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn echo_tool_echoes_its_input() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        let result = EchoTool
            .execute(serde_json::json!({"a": 1, "b": "two"}), &ctx)
            .await;
        match result {
            ToolResult::Ok(v) => {
                assert_eq!(v["echo"]["a"], 1);
                assert_eq!(v["echo"]["b"], "two");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn echo_registry_has_echo_and_finalize_only() {
        let reg = echo_registry();
        assert_eq!(reg.len(), 2);
        let names = reg.names();
        assert!(names.contains(&EchoTool::NAME.to_string()));
        assert!(names.contains(&FINALIZE_REPORT_NAME.to_string()));
    }
}
