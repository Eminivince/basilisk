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

pub mod session;
pub mod tool;
pub mod tools;

pub use session::{
    SessionError, SessionRecord, SessionStatus, SessionStore, SessionSummary, ToolCallRecord,
    TurnRecord, TurnRole,
};
pub use tool::{SessionId, Tool, ToolContext, ToolRegistry, ToolResult};
pub use tools::{
    standard_registry, AnalyzeProject, ClassifyTarget, Confidence, FetchGithubRepo, FinalReport,
    FinalizeReport, GetStorageSlot, GrepProject, ListDirectory, ReadFile, ResolveOnchainContract,
    ResolveOnchainSystem, StaticCall, FINALIZE_REPORT_NAME,
};
