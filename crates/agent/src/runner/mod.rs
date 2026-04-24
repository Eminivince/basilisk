//! LLM-driven agent runner.
//!
//! `CP5` lands in slices so each commit stays small enough to review:
//!
//!  - **`CP5a`**: type vocabulary in [`types`] — [`Budget`],
//!    [`AgentStats`], [`AgentStopReason`], [`AgentOutcome`],
//!    [`AgentError`].
//!  - **`CP5b`** (this commit): [`AgentRunner`] skeleton in [`agent`]
//!    — constructor, dependencies, budget-enforcement helpers, cost
//!    estimator. No `run()` body yet.
//!  - **`CP5c`**: the loop body itself (build request → stream → fold
//!    → record turn → dispatch tool calls → repeat).
//!  - **`CP5d`**: a stub `LlmBackend` for tests + end-to-end loop
//!    tests.

pub mod agent;
pub mod observer;
pub mod types;

pub use agent::AgentRunner;
pub use observer::{AgentObserver, NoopObserver};
pub use types::{AgentError, AgentOutcome, AgentStats, AgentStopReason, Budget};
