//! LLM-driven agent loop, tool definitions, session management.
//!
//! Phase 3 of Basilisk: an LLM orchestrates the ingestion tools shipped
//! in Phases 1–2. This crate is the tool surface the agent calls; it
//! does NOT contain decision logic. Decisions live in the model's
//! prompts + the tool descriptions — our job is to expose capability,
//! not to steer. See the checkpoint trail:
//!
//!  - **`CP3`** (this commit): [`Tool`] trait + [`ToolRegistry`] + the
//!    eleven standard recon tools. No loop yet; tools are callable in
//!    isolation so they're testable without an LLM.
//!  - **`CP4`**: `SQLite`-backed session persistence.
//!  - **`CP5`**: the tool-use loop itself + budget enforcement.
//!  - **CP6+**: CLI wiring, system prompts, live tests.

pub mod prompts;
pub mod runner;
pub mod session;
pub mod testing;
pub mod tool;
pub mod tools;

pub use prompts::{RECON_DEFAULT_PROMPT, RECON_V1_PROMPT, RECON_V2_PROMPT};
pub use runner::{
    AgentError, AgentObserver, AgentOutcome, AgentRunner, AgentStats, AgentStopReason, Budget,
    NoopObserver, NudgeEvent, NudgeKind,
};
pub use session::{
    default_db_path, LoadedSession, SessionError, SessionRecord, SessionStatus, SessionStore,
    SessionSummary, ToolCallRecord, TurnRecord, TurnRole,
};
pub use tool::{SessionId, Tool, ToolContext, ToolRegistry, ToolResult};
pub use tools::{
    knowledge_enhanced_registry, standard_registry, vuln_registry, AnalyzeProject,
    BuildAndRunFoundryTestTool, ClassifyTarget, Confidence, FetchGithubRepo, FinalReport,
    FinalizeReport, FinalizeSelfCritique, FindCallersOfTool, GetStorageSlot, GrepProject,
    ListDirectory, ReadFile, RecordFinding, RecordLimitation, RecordSuspicion,
    ResolveOnchainContract, ResolveOnchainSystem, SearchKnowledgeBase, SearchProtocolDocs,
    SearchSimilarCode, SimulateCallChainTool, StaticCall, TraceStateDependenciesTool,
    BUILD_AND_RUN_FOUNDRY_TEST_NAME, FINALIZE_REPORT_NAME, FINALIZE_SELF_CRITIQUE_NAME,
    FIND_CALLERS_OF_NAME, RECORD_LIMITATION_NAME, RECORD_SUSPICION_NAME, SIMULATE_CALL_CHAIN_NAME,
    TRACE_STATE_DEPENDENCIES_NAME, VULN_V1_PROMPT, VULN_V2_PROMPT, VULN_V3_PROMPT,
};
