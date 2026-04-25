//! Basilisk benchmark harness.
//!
//! Five real post-exploit targets with pinned chain blocks and
//! expected-findings metadata. Runs are first-class sessions — each
//! bench run's findings flow through the normal `AgentRunner` plus a
//! wrapping that scores the output against the target's expectations.
//!
//! Entry points:
//!
//!   - [`targets::all()`] — every benchmark target shipped with Set 9.
//!   - [`BenchmarkTarget`] — one target's definition.
//!   - [`BenchmarkRun`] — the record produced by running against a
//!     target.
//!   - [`score`] — heuristic matcher that produces a
//!     [`BenchmarkScore`] from a run.
//!   - [`BenchStore`] — SQLite-backed history of runs (uses the
//!     existing sessions.db plus a new `bench_runs` table).

pub mod error;
pub mod score;
pub mod store;
pub mod targets;
pub mod types;

pub use error::BenchError;
pub use score::{score, BenchmarkScore, FindingMatch};
pub use store::{BenchStore, ReviewVerdict};
pub use targets::all_targets;
pub use types::{AgentFindingSummary, BenchmarkRun, BenchmarkTarget, ExpectedFinding, Severity};
