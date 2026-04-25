//! The `Tool` trait, execution context, and registry.
//!
//! A tool is any unit of capability the agent can invoke. Each tool
//! declares a name, a description (which the model reads to decide
//! when to call), and a JSON Schema describing its input. `execute`
//! takes that input (already JSON-validated upstream against the
//! schema) and a [`ToolContext`] carrying everything chain-agnostic
//! (config, GitHub client, repo cache, session id), and returns a
//! [`ToolResult`] — either a JSON payload the agent will see on its
//! next turn, or a typed error that lets the agent decide whether to
//! retry / work around / give up.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use basilisk_core::Config;
use basilisk_git::RepoCache;
use basilisk_github::GithubClient;
use basilisk_llm::ToolDefinition;
use basilisk_scratchpad::{Scratchpad, ScratchpadStore};
use serde::{Deserialize, Serialize};

/// A stable session identifier.
///
/// Wraps a UUID-v4 string in production. Arbitrary strings are still
/// accepted via [`Self::new`] so tests can use stable, readable ids
/// like `"test-session"` — no validation, no surprises.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Wrap an existing identifier. Used by `load_session` / resume paths
    /// and by tests that want a known id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Mint a fresh UUID-v4-backed identifier. `CP4b`'s `create_session`
    /// calls this whenever the CLI starts a new run.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything a tool needs that isn't chain-specific.
///
/// Chain-scoped clients (RPC, explorers, ingester) are built
/// per-invocation by the tools that need them, keyed on the `chain`
/// field of their JSON input. That way a single agent session can
/// span multiple chains without rebuilding the context.
#[derive(Clone)]
pub struct ToolContext {
    pub config: Arc<Config>,
    pub github: Arc<GithubClient>,
    pub repo_cache: Arc<RepoCache>,
    pub session_id: SessionId,
    /// Optional knowledge-base handle. `None` when the session was
    /// spawned without knowledge wiring (recon, tests). Knowledge
    /// tools (`search_knowledge_base`, `record_finding`, …) return
    /// a structured error when dispatched against `None` — the
    /// agent can self-correct instead of crashing.
    pub knowledge: Option<Arc<basilisk_knowledge::KnowledgeBase>>,
    /// Optional engagement id, used by `search_protocol_docs` to
    /// filter the `protocols` collection. `None` disables
    /// engagement-scoped filtering.
    pub engagement_id: Option<String>,
    /// Live in-memory scratchpad shared with the runner. The three
    /// scratchpad tools mutate through this handle; the runner
    /// re-renders the compact form into the system prompt each
    /// turn and persists via [`Self::scratchpad_store`].
    ///
    /// `None` means the session was spawned without working-memory
    /// wiring (tests, offline smoke-checks); scratchpad tools
    /// return a typed error rather than crashing.
    pub scratchpad: Option<Arc<Mutex<Scratchpad>>>,
    /// Persistence handle for the scratchpad. Independent of
    /// [`Self::scratchpad`] so tools can mutate in-memory and the
    /// runner flushes to disk at turn boundaries.
    pub scratchpad_store: Option<Arc<ScratchpadStore>>,
}

impl ToolContext {
    /// Quick constructor for tests: empty config, tokenless GitHub
    /// client, cache in `$TMP`.
    #[cfg(test)]
    pub fn test(repo_cache_root: std::path::PathBuf) -> Self {
        let config = Arc::new(Config::default());
        let github =
            Arc::new(GithubClient::new(None).expect("tokenless github client always succeeds"));
        let repo_cache =
            Arc::new(RepoCache::open_at(repo_cache_root).expect("opening test repo cache"));
        Self {
            config,
            github,
            repo_cache,
            knowledge: None,
            engagement_id: None,
            session_id: SessionId::new("test-session"),
            scratchpad: None,
            scratchpad_store: None,
        }
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .finish()
    }
}

/// Outcome of one tool invocation.
///
/// `Err.retryable` distinguishes transient failures (network blip,
/// rate limit) from ones where a retry with the same input won't help
/// (malformed address, unknown chain). The agent sees both — it's not
/// our job to retry; we surface the signal, the agent decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResult {
    Ok(serde_json::Value),
    Err { message: String, retryable: bool },
}

impl ToolResult {
    pub fn ok(v: impl Serialize) -> Self {
        Self::Ok(serde_json::to_value(v).expect("tool output serialises"))
    }

    pub fn err(message: impl Into<String>, retryable: bool) -> Self {
        Self::Err {
            message: message.into(),
            retryable,
        }
    }

    /// `true` on `Ok`.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Render as the agent will see it in its next turn: a string the
    /// `ToolResult` content block will carry. `is_error` pairs with this.
    pub fn to_content_pair(&self) -> (String, bool) {
        match self {
            Self::Ok(v) => (
                serde_json::to_string(v).unwrap_or_else(|e| format!("\"<serialize failed: {e}>\"")),
                false,
            ),
            Self::Err { message, .. } => {
                (serde_json::json!({ "error": message }).to_string(), true)
            }
        }
    }
}

/// One capability the agent can call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name as the model sees it in `tool_use.name`. Must be a
    /// stable identifier — pick one and don't rename.
    fn name(&self) -> &'static str;

    /// Description the model reads to decide when to call this tool.
    /// Write it as you would brief a new team member.
    fn description(&self) -> &'static str;

    /// JSON Schema for the tool's input. Field descriptions matter —
    /// they're the model's second layer of guidance after the top-level
    /// description.
    fn input_schema(&self) -> serde_json::Value;

    async fn execute(&self, input: serde_json::Value, context: &ToolContext) -> ToolResult;

    /// Convert the tool's declarations into the shape the LLM backend
    /// expects. Default implementation assembles from the trait methods.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Collection of tools keyed by name. Constructed once per session;
/// handed to the agent loop, which reads `definitions()` for the LLM
/// request and calls `dispatch()` on each `tool_use` response.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Empty registry. Use [`Self::register`] to populate; consumers
    /// typically use [`crate::tools::standard_registry`] instead.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool. Panics if a tool with the same name is
    /// already registered — a name collision is a programming bug.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.name().to_string();
        assert!(
            !self.tools.contains_key(&name),
            "tool {name:?} already registered",
        );
        self.tools.insert(name, tool);
        self
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// `true` when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// List every registered tool's name. Sorted for stable output.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.tools.keys().cloned().collect();
        out.sort();
        out
    }

    /// Tool definitions in the LLM-backend shape. Sorted by name so the
    /// prompt cache hit rate stays high across turns.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut tools: Vec<_> = self.tools.values().collect();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        tools.into_iter().map(|t| t.definition()).collect()
    }

    /// Fetch a tool by name. Returns `None` if unknown — callers must
    /// surface that as an error the agent can self-correct from.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Invoke a tool by name. Unknown names return a retryable-false
    /// error so the agent sees "no such tool, try another" rather than
    /// the loop crashing.
    pub async fn dispatch(
        &self,
        name: &str,
        input: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute(input, context).await,
            None => ToolResult::err(format!("unknown tool: {name:?}"), false),
        }
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("names", &self.names())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    struct StubTool;

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn description(&self) -> &'static str {
            "a stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: serde_json::Value, _context: &ToolContext) -> ToolResult {
            ToolResult::ok(serde_json::json!({ "echo": input }))
        }
    }

    fn test_ctx() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        // Keep the tempdir alive by leaking it — tests are short-lived.
        std::mem::forget(dir);
        ctx
    }

    #[test]
    fn session_id_round_trips_through_display() {
        let s = SessionId::new("abc-123");
        assert_eq!(s.to_string(), "abc-123");
        assert_eq!(s.as_str(), "abc-123");
    }

    #[test]
    fn tool_result_to_content_pair_ok() {
        let r = ToolResult::ok(serde_json::json!({"k": 1}));
        let (s, is_err) = r.to_content_pair();
        assert!(!is_err);
        assert!(s.contains("\"k\":1"));
    }

    #[test]
    fn tool_result_to_content_pair_err() {
        let r = ToolResult::err("boom", true);
        let (s, is_err) = r.to_content_pair();
        assert!(is_err);
        assert!(s.contains("boom"));
    }

    #[test]
    fn registry_dispatch_unknown_is_retryable_false_error() {
        let ctx = test_ctx();
        let reg = ToolRegistry::new();
        let res = futures_lite_block_on(reg.dispatch("nope", serde_json::json!({}), &ctx));
        match res {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("unknown tool"));
                assert!(!retryable);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn registry_register_and_dispatch_round_trips() {
        let ctx = test_ctx();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.names(), vec!["stub".to_string()]);

        let res = futures_lite_block_on(reg.dispatch("stub", serde_json::json!({"a": 1}), &ctx));
        match res {
            ToolResult::Ok(v) => assert_eq!(v["echo"]["a"], 1),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn registry_definitions_are_sorted_by_name() {
        struct A;
        struct B;
        #[async_trait]
        impl Tool for A {
            fn name(&self) -> &'static str {
                "zzz"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> ToolResult {
                ToolResult::ok(())
            }
        }
        #[async_trait]
        impl Tool for B {
            fn name(&self) -> &'static str {
                "aaa"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> ToolResult {
                ToolResult::ok(())
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(A));
        reg.register(Arc::new(B));
        let defs = reg.definitions();
        assert_eq!(defs[0].name, "aaa");
        assert_eq!(defs[1].name, "zzz");
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn registry_rejects_duplicate_names() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool));
        reg.register(Arc::new(StubTool));
    }

    /// Minimal `block_on` helper for sync tests of async code.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(fut)
    }
}
